//! Engine installation.
//!
//! Supports package-manager installs, pinned release archives (with embedded
//! checksum verification), and printed source-build recipes. Downloads are only
//! performed when explicitly authorized (`--yes` and not `--dry-run`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::engines::{Engine, InstallMethod, InstallPlan, ReleaseAsset};
use crate::error::{AppError, Result};
use crate::system::checksums;
use crate::system::deps::Dependency;
use crate::system::distro::Distro;
use crate::system::download::{self, default_bin_dir, install_binary_as};
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
    let binary = locate_binary(&files, &asset.binary).ok_or_else(|| {
        let mut names: Vec<String> = files
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .collect();
        names.sort();
        names.dedup();
        let found = if names.is_empty() {
            "<none>".to_string()
        } else {
            names.join(", ")
        };
        AppError::Archive(format!(
            "archive {} does not contain `{}` (candidates: {found})",
            asset.url, asset.binary
        ))
    })?;

    // Install under the expected name even when the archive member is versioned
    // (e.g. `qrypt-linux-amd64` -> `qrypt`).
    install_binary_as(&binary, &default_bin_dir(), &asset.binary)
}

/// Choose the binary to install from extracted archive entries.
///
/// Rules, in order:
/// 1. exact basename match to `expected`;
/// 2. a single executable regular file whose basename is `expected` or starts
///    with `expected-` / `expected_` (nested paths are fine);
/// 3. a single executable regular file overall;
/// 4. otherwise `None` (ambiguous or missing).
///
/// Directories and symlinks are never selected.
pub fn locate_binary(extracted: &[PathBuf], expected: &str) -> Option<PathBuf> {
    let regular: Vec<&PathBuf> = extracted
        .iter()
        .filter(|path| is_regular_file(path))
        .collect();

    if let Some(exact) = regular.iter().find(|path| basename(path) == expected) {
        return Some((*exact).clone());
    }

    let dash = format!("{expected}-");
    let underscore = format!("{expected}_");
    let prefixed: Vec<&PathBuf> = regular
        .iter()
        .filter(|path| {
            let name = basename(path);
            name == expected || name.starts_with(&dash) || name.starts_with(&underscore)
        })
        .filter(|path| is_executable(path))
        .copied()
        .collect();

    match prefixed.len() {
        1 => return Some(prefixed[0].clone()),
        n if n > 1 => return None,
        _ => {}
    }

    let executables: Vec<&PathBuf> = regular
        .iter()
        .filter(|path| is_executable(path))
        .copied()
        .collect();
    if executables.len() == 1 {
        return Some(executables[0].clone());
    }

    None
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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

/// Missing dependencies from `deps`.
pub fn missing_dependencies(deps: &[Dependency]) -> Vec<Dependency> {
    deps.iter()
        .copied()
        .filter(|dependency| !dependency.present())
        .collect()
}

/// Deduplicated package names for a set of dependencies.
pub fn dependency_packages(deps: &[Dependency]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut packages = Vec::new();
    for dependency in deps {
        for package in dependency.packages {
            if seen.insert(*package) {
                packages.push((*package).to_string());
            }
        }
    }
    packages
}

/// Build the package-manager argv for a set of dependencies.
pub fn dependency_install_argv(
    deps: &[Dependency],
    manager: PackageManager,
) -> Option<Vec<String>> {
    let packages = dependency_packages(deps);
    if packages.is_empty() {
        return None;
    }
    let refs: Vec<&str> = packages.iter().map(String::as_str).collect();
    manager.install_argv(&refs)
}

/// Install missing runtime dependencies through the package manager.
pub fn install_dependencies(
    deps: &[Dependency],
    distro: &Distro,
    pkg: &PackageManager,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    let missing = missing_dependencies(deps);
    if missing.is_empty() {
        println!("All runtime dependencies are present.");
        return Ok(());
    }

    let manager = effective_manager(distro, *pkg);
    let argv = dependency_install_argv(&missing, manager).ok_or_else(|| {
        let names: Vec<&str> = missing.iter().map(|dependency| dependency.id).collect();
        AppError::Unsupported(format!(
            "missing dependencies ({}) cannot be installed automatically; \
             enable the fuse kernel module and install fuse3 manually",
            names.join(", ")
        ))
    })?;

    let escalation = choose_escalation()?;
    let escalated = escalated_argv(escalation, Path::new(&argv[0]), &argv[1..]);
    println!("Dependency install command: {}", render_command(&escalated));

    if dry_run {
        println!("Dry run: not executing dependency installation.");
        return Ok(());
    }
    // Package installs are already explicit (via `install`/`deps install`).
    let _ = assume_yes;
    run_command(&escalated)
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

    fn make_file(path: &Path, executable: bool) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, b"binary").expect("write");
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
        path.to_path_buf()
    }

    #[test]
    fn locates_versioned_root_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = make_file(&temp.path().join("qrypt-linux-amd64"), true);
        assert_eq!(
            locate_binary(std::slice::from_ref(&file), "qrypt"),
            Some(file)
        );
    }

    #[test]
    fn locates_nested_versioned_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = make_file(
            &temp.path().join("qrypt_0.7.0").join("qrypt-linux-arm64"),
            true,
        );
        assert_eq!(
            locate_binary(std::slice::from_ref(&file), "qrypt"),
            Some(file)
        );
    }

    #[test]
    fn locates_exact_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = make_file(&temp.path().join("s3fs"), false);
        assert_eq!(
            locate_binary(std::slice::from_ref(&file), "s3fs"),
            Some(file)
        );
    }

    #[test]
    fn exact_match_wins_over_ambiguity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exact = make_file(&temp.path().join("qrypt"), false);
        let other = make_file(&temp.path().join("qrypt-other"), true);
        let versioned = make_file(&temp.path().join("qrypt-linux-amd64"), true);
        let found = locate_binary(&[other, versioned, exact.clone()], "qrypt");
        assert_eq!(found, Some(exact));
    }

    #[test]
    fn rejects_ambiguous_nonexact_candidates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = make_file(&temp.path().join("qrypt-other"), true);
        let second = make_file(&temp.path().join("qrypt-linux-amd64"), true);
        assert_eq!(locate_binary(&[first, second], "qrypt"), None);
    }

    #[test]
    fn ignores_non_executable_prefixed_candidate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = make_file(&temp.path().join("qrypt-linux-amd64"), false);
        assert_eq!(locate_binary(&[file], "qrypt"), None);
    }

    #[test]
    fn ignores_directories_and_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("qrypt");
        fs::create_dir(&dir).expect("mkdir");
        assert_eq!(locate_binary(&[dir], "qrypt"), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let real = make_file(&temp.path().join("real-binary"), true);
        let link = temp.path().join("qrypt");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(locate_binary(&[link], "qrypt"), None);
    }

    #[test]
    fn falls_back_to_single_executable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = make_file(&temp.path().join("some-random-binary"), true);
        assert_eq!(
            locate_binary(std::slice::from_ref(&file), "qrypt"),
            Some(file)
        );
    }

    fn dependency(binary: &'static str, packages: &'static [&'static str]) -> Dependency {
        Dependency {
            id: binary,
            description: binary,
            check: crate::system::deps::DepCheck::Binary(binary),
            packages,
        }
    }

    #[test]
    fn dependency_packages_are_deduped() {
        let deps = vec![
            dependency("missing-a", &["fuse3"]),
            dependency("missing-b", &["rclone"]),
            dependency("missing-c", &["fuse3"]),
        ];
        assert_eq!(dependency_packages(&deps), vec!["fuse3", "rclone"]);
        let argv = dependency_install_argv(&deps, PackageManager::Dnf).expect("argv");
        assert_eq!(argv, vec!["dnf", "install", "-y", "fuse3", "rclone"]);
    }

    #[test]
    fn missing_dependencies_filters_present() {
        let deps = vec![
            dependency("sh", &["x"]),
            dependency("mount-fs-definitely-not-a-real-binary", &["y"]),
        ];
        let missing = missing_dependencies(&deps);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "mount-fs-definitely-not-a-real-binary");
    }

    #[test]
    fn device_only_dependency_cannot_be_installed() {
        let deps = vec![Dependency {
            id: "fuse-device",
            description: "FUSE device",
            check: crate::system::deps::DepCheck::Device("/no/such/mount-fs-device"),
            packages: &[],
        }];
        let distro = crate::system::distro::parse("ID=fedora\n");
        let err = install_dependencies(&deps, &distro, &PackageManager::Dnf, true, false)
            .expect_err("cannot install a device");
        assert!(err.to_string().contains("cannot be installed"), "{err}");
    }
}
