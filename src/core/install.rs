//! Engine installation.
//!
//! Supports package-manager installs, pinned release archives (with embedded
//! checksum verification), and printed source-build recipes. Downloads are only
//! performed when explicitly authorized (`--yes` and not `--dry-run`).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::engines::{Engine, InstallMethod, InstallPlan, ReleaseAsset};
use crate::error::{AppError, Result};
use crate::system::checksums;
use crate::system::distro::Distro;
use crate::system::download::{self, default_bin_dir, install_binary};
use crate::system::pkg::PackageManager;
use crate::system::privilege::{choose_escalation, escalated_argv};

/// What installation method the user requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallRequest {
    /// Prefer the engine's declared method.
    Auto,
    /// Require a package-manager installation.
    Pkg,
    /// Require a pinned release archive download.
    Release,
    /// Print (and optionally run) the source-build recipe.
    Source,
}

/// Install `engine` according to `request`.
///
/// `dry_run` prints the planned action without executing it. `assume_yes`
/// authorizes network downloads and source-build execution.
pub fn install_engine(
    engine: &dyn Engine,
    distro: &Distro,
    pkg: PackageManager,
    request: InstallRequest,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    let plan = engine.install_plan(distro, pkg);
    match request {
        InstallRequest::Source => install_source(engine, &plan, dry_run, assume_yes),
        InstallRequest::Pkg => install_package(engine, distro, pkg, &plan, dry_run),
        InstallRequest::Release => install_release(engine, &plan, dry_run, assume_yes),
        InstallRequest::Auto => match plan.method {
            InstallMethod::PackageManager => install_package(engine, distro, pkg, &plan, dry_run),
            InstallMethod::ReleaseArchive => install_release(engine, &plan, dry_run, assume_yes),
            InstallMethod::SourceBuild | InstallMethod::Unavailable => {
                Err(AppError::Unsupported(format!(
                    "{} cannot be installed automatically: {}",
                    engine.id(),
                    plan.notes.join(" ")
                )))
            }
        },
    }
}

fn install_package(
    engine: &dyn Engine,
    distro: &Distro,
    pkg: PackageManager,
    plan: &InstallPlan,
    dry_run: bool,
) -> Result<()> {
    if plan.method != InstallMethod::PackageManager {
        return Err(AppError::Unsupported(format!(
            "{} has no package-manager install (try --method release): {}",
            engine.id(),
            plan.notes.join(" ")
        )));
    }

    let manager = effective_manager(distro, pkg);
    let packages: Vec<&str> = plan.packages.iter().map(String::as_str).collect();
    let argv = manager.install_argv(&packages).ok_or_else(|| {
        AppError::Unsupported(format!(
            "no package manager available to install {}",
            engine.id()
        ))
    })?;

    let escalation = choose_escalation()?;
    let program = Path::new(&argv[0]);
    let escalated = escalated_argv(escalation, program, &argv[1..]);

    println!("Install command: {}", render_command(&escalated));
    if dry_run {
        println!("Dry run: not executing.");
        return Ok(());
    }

    run_command(&escalated)?;
    verify_installed(engine)
}

fn install_release(
    engine: &dyn Engine,
    plan: &InstallPlan,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    let Some(asset) = plan.release.as_ref() else {
        return Err(AppError::Unsupported(format!(
            "{} has no release archive: {}",
            engine.id(),
            plan.notes.join(" ")
        )));
    };

    let Some(sha256) = asset.sha256 else {
        return Err(AppError::Integrity(format!(
            "{} {} has no pinned checksum; refusing to install an unverified artifact",
            engine.id(),
            asset.version
        )));
    };

    println!("Release archive: {}", asset.url);
    println!("Version: {}  Binary: {}", asset.version, asset.binary);
    println!("sha256: {sha256}");
    println!(
        "Install to: {}",
        default_bin_dir().join(&asset.binary).display()
    );

    if dry_run {
        println!("Dry run: not downloading.");
        return Ok(());
    }
    if !assume_yes {
        return Err(AppError::Unsupported(
            "refusing to download a release archive without --yes".to_string(),
        ));
    }

    let work = std::env::temp_dir().join(format!(
        "mount-fs-install-{}-{}",
        std::process::id(),
        engine.id()
    ));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;

    let result = download_and_install(asset, &work);
    let _ = fs::remove_dir_all(&work);
    let installed = result?;

    verify_installed(engine)?;
    println!(
        "Installed {} ({}) to {}",
        engine.id(),
        asset.version,
        installed.display()
    );
    Ok(())
}

fn download_and_install(asset: &ReleaseAsset, work: &Path) -> Result<PathBuf> {
    let name = asset.url.rsplit('/').next().unwrap_or("archive");
    let archive = work.join(name);

    download::download(&asset.url, &archive)?;
    verify_download(&archive, asset)?;

    let files = download::extract(&archive, asset.kind, &work.join("extract"))?;
    let binary = files
        .iter()
        .find(|path| {
            path.file_name()
                .map(|name| name == OsStr::new(&asset.binary))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            AppError::Archive(format!(
                "archive {} does not contain `{}`",
                asset.url, asset.binary
            ))
        })?;

    install_binary(binary, &default_bin_dir())
}

/// Verify a downloaded artifact against its pinned checksum.
///
/// A missing pinned checksum is a hard error (never a silent pass).
fn verify_download(path: &Path, asset: &ReleaseAsset) -> Result<()> {
    let Some(expected) = asset.sha256 else {
        return Err(AppError::Integrity(format!(
            "no pinned checksum for `{}`; refusing to install",
            asset.binary
        )));
    };
    checksums::verify(path, expected)
}

fn verify_installed(engine: &dyn Engine) -> Result<()> {
    let detected = engine.detect();
    if !detected.found {
        return Err(AppError::Unsupported(format!(
            "{} was not found on PATH after installation",
            engine.id()
        )));
    }
    println!(
        "Installed {} at {}",
        engine.id(),
        detected
            .path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "an unknown path".to_string())
    );
    Ok(())
}

fn install_source(
    engine: &dyn Engine,
    plan: &InstallPlan,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    println!("Source build recipe for {}:", engine.id());
    for command in &plan.source_commands {
        println!("  {}", render_command(command));
    }

    if dry_run {
        println!("Dry run: not executing.");
        return Ok(());
    }
    if !assume_yes {
        println!("Not executing the source build without --yes.");
        return Ok(());
    }
    if plan.source_commands.is_empty() {
        return Err(AppError::Unsupported(format!(
            "no source-build recipe is available for {}",
            engine.id()
        )));
    }

    for command in &plan.source_commands {
        run_command(command)?;
    }
    verify_installed(engine)
}

fn effective_manager(distro: &Distro, pkg: PackageManager) -> PackageManager {
    if pkg == PackageManager::Unknown {
        PackageManager::from_distro_family(distro.family)
    } else {
        pkg
    }
}

fn run_command(argv: &[String]) -> Result<()> {
    let program = argv
        .first()
        .ok_or_else(|| AppError::Config("refusing to run an empty command".to_string()))?;
    let status = Command::new(program).args(&argv[1..]).status()?;
    if !status.success() {
        return Err(AppError::Unsupported(format!(
            "command failed ({status}): {}",
            render_command(argv)
        )));
    }
    Ok(())
}

/// Render an argv for display, quoting arguments that contain spaces.
pub fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.contains([' ', '\t']) {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::{
        Capabilities, DetectResult, EffectiveMount, InstallPlan, MountPlan, MountSecrets, TokenKind,
    };
    use crate::system::download::ArchiveKind;
    use crate::system::privilege::Escalation;
    use std::path::Path;

    #[test]
    fn renders_simple_command() {
        let argv = vec![
            "dnf".to_string(),
            "install".to_string(),
            "-y".to_string(),
            "s3fs-fuse".to_string(),
        ];
        assert_eq!(render_command(&argv), "dnf install -y s3fs-fuse");
    }

    #[test]
    fn renders_sudo_command() {
        let args = vec![
            "install".to_string(),
            "-y".to_string(),
            "s3fs-fuse".to_string(),
        ];
        let argv = escalated_argv(Escalation::Sudo, Path::new("dnf"), &args);
        assert_eq!(render_command(&argv), "sudo -- dnf install -y s3fs-fuse");
    }

    #[test]
    fn renders_root_command() {
        let args = vec!["install".to_string(), "-y".to_string()];
        let argv = escalated_argv(Escalation::None, Path::new("/usr/bin/dnf"), &args);
        assert_eq!(render_command(&argv), "/usr/bin/dnf install -y");
    }

    #[test]
    fn quotes_arguments_with_spaces() {
        let argv = vec!["echo".to_string(), "hello world".to_string()];
        assert_eq!(render_command(&argv), "echo \"hello world\"");
    }

    #[test]
    fn rejects_archive_with_wrong_embedded_checksum() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("qrypt_0.7.0_linux_amd64.tar.gz");
        fs::write(&archive, b"pretend archive bytes").expect("write");

        let asset = ReleaseAsset {
            version: "0.7.0".to_string(),
            url: "https://example.invalid/qrypt_0.7.0_linux_amd64.tar.gz".to_string(),
            kind: ArchiveKind::TarGz,
            sha256: Some("1111111111111111111111111111111111111111111111111111111111111111"),
            binary: "qrypt".to_string(),
        };
        let err = verify_download(&archive, &asset).expect_err("must reject");
        assert!(matches!(err, AppError::Integrity(_)));
    }

    #[test]
    fn accepts_archive_with_correct_checksum() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("file");
        fs::write(&archive, b"hello").expect("write");
        let asset = ReleaseAsset {
            version: "0".to_string(),
            url: "https://example.invalid/file".to_string(),
            kind: ArchiveKind::TarGz,
            // sha256("hello")
            sha256: Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
            binary: "tool".to_string(),
        };
        verify_download(&archive, &asset).expect("accept");
    }

    struct FakeReleaseEngine {
        sha256: Option<&'static str>,
    }

    impl Engine for FakeReleaseEngine {
        fn id(&self) -> &'static str {
            "fake-release"
        }

        fn detect(&self) -> DetectResult {
            DetectResult {
                found: true,
                ..DetectResult::default()
            }
        }

        fn min_version(&self) -> Option<semver::VersionReq> {
            None
        }

        fn install_plan(&self, _distro: &Distro, _pkg: PackageManager) -> InstallPlan {
            InstallPlan {
                method: InstallMethod::ReleaseArchive,
                release: Some(ReleaseAsset {
                    version: "1.0.0".to_string(),
                    url: "https://example.invalid/fake-1.0.0.tar.gz".to_string(),
                    kind: ArchiveKind::TarGz,
                    sha256: self.sha256,
                    binary: "fake".to_string(),
                }),
                ..InstallPlan::default()
            }
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                read_write: true,
                allow_other: false,
                foreground: true,
            }
        }

        fn token_kind(&self) -> TokenKind {
            TokenKind::AwsKey
        }

        fn plan_mount(
            &self,
            _profile: &crate::config::Profile,
            _effective: &EffectiveMount,
            _secrets: &MountSecrets,
        ) -> Result<MountPlan> {
            unreachable!("not used in install tests")
        }
    }

    #[test]
    fn rejects_missing_embedded_checksum() {
        let asset = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://example.invalid/fake.tar.gz".to_string(),
            kind: ArchiveKind::TarGz,
            sha256: None,
            binary: "fake".to_string(),
        };
        let err = verify_download(Path::new("/nonexistent"), &asset).expect_err("must error");
        assert!(matches!(err, AppError::Integrity(_)));
    }

    #[test]
    fn release_install_requires_yes_before_downloading() {
        let engine = FakeReleaseEngine {
            sha256: Some("0000000000000000000000000000000000000000000000000000000000000000"),
        };
        let distro = crate::system::distro::parse("ID=fedora\n");
        let err = install_engine(
            &engine,
            &distro,
            PackageManager::Unknown,
            InstallRequest::Release,
            false,
            false,
        )
        .expect_err("needs --yes");
        assert!(err.to_string().contains("--yes"), "{err}");
    }

    #[test]
    fn release_install_without_pin_errors_even_on_dry_run() {
        let engine = FakeReleaseEngine { sha256: None };
        let distro = crate::system::distro::parse("ID=fedora\n");
        let err = install_engine(
            &engine,
            &distro,
            PackageManager::Unknown,
            InstallRequest::Release,
            true,
            true,
        )
        .expect_err("missing pin");
        assert!(matches!(err, AppError::Integrity(_)));
    }
}
