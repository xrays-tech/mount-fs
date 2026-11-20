//! `s3fs-fuse` engine implementation.

use std::path::PathBuf;
use std::process::Command;

use semver::{Version, VersionReq};

use crate::config::Profile;
use crate::engines::{
    Capabilities, DetectResult, EffectiveMount, Engine, InstallMethod, InstallPlan, MountPlan,
    MountSecrets, UnmountStrategy,
};
use crate::error::{AppError, Result};
use crate::system::distro::{Distro, DistroFamily};
use crate::system::pkg::PackageManager;

/// The `s3fs-fuse` binary name.
const PROGRAM: &str = "s3fs";

/// Minimum version supporting FUSE 3 and the `-o endpoint` → `-o region`
/// rename (landed in s3fs-fuse 1.96).
const MIN_VERSION: &str = ">=1.96";

/// `s3fs-fuse` mount engine.
#[derive(Debug, Default)]
pub struct S3fsEngine;

impl S3fsEngine {
    /// Create the engine.
    pub fn new() -> Self {
        S3fsEngine
    }
}

impl Engine for S3fsEngine {
    fn id(&self) -> &'static str {
        "s3fs-fuse"
    }

    fn detect(&self) -> DetectResult {
        let path = which::which(PROGRAM).ok();
        let Some(found_path) = path.as_ref() else {
            return DetectResult::default();
        };

        let output = Command::new(found_path).arg("--version").output().ok();
        let (version, version_raw) = match output {
            Some(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let raw = text
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string);
                (extract_version(&text), raw)
            }
            None => (None, None),
        };

        DetectResult {
            found: true,
            path,
            version,
            version_raw,
        }
    }

    fn min_version(&self) -> Option<VersionReq> {
        VersionReq::parse(MIN_VERSION).ok()
    }

    fn install_plan(&self, distro: &Distro, pkg: PackageManager) -> InstallPlan {
        let manager = if pkg == PackageManager::Unknown {
            PackageManager::from_distro_family(distro.family)
        } else {
            pkg
        };

        let (method, packages) = if manager == PackageManager::Unknown {
            (InstallMethod::Unavailable, Vec::new())
        } else {
            (InstallMethod::PackageManager, vec!["s3fs-fuse".to_string()])
        };

        InstallPlan {
            method,
            packages,
            requires_root: true,
            notes: vec![
                "Package installation requires root privileges.".to_string(),
                "Source build: git clone https://github.com/s3fs-fuse/s3fs-fuse.git && \
                 cd s3fs-fuse && ./autogen.sh && ./configure --with-openssl && make && \
                 sudo make install"
                    .to_string(),
                "Automated source builds land in M3; run the recipe manually for now.".to_string(),
                "s3fs-fuse >= 1.96 requires FUSE 3 and supports `-o region`.".to_string(),
            ],
            source_commands: self.source_commands(distro.family),
            release: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_write: true,
            allow_other: true,
            foreground: true,
        }
    }

    fn token_kind(&self) -> crate::engines::TokenKind {
        crate::engines::TokenKind::AwsKey
    }

    fn plan_mount(
        &self,
        profile: &Profile,
        effective: &EffectiveMount,
        secrets: &MountSecrets,
    ) -> Result<MountPlan> {
        let bucket = profile.bucket.as_deref().ok_or_else(|| {
            AppError::Config("S3 profile requires a `bucket` for mounting".to_string())
        })?;

        let access_key = secrets
            .access_key
            .as_deref()
            .ok_or_else(|| AppError::Credential("missing S3 access key id".to_string()))?;
        let secret_key = secrets
            .secret_key
            .as_deref()
            .ok_or_else(|| AppError::Credential("missing S3 secret access key".to_string()))?;

        let dirs = crate::core::creds::RuntimeDirs {
            runtime: effective.runtime_dir.clone(),
        };
        let credential = crate::core::creds::materialize_s3(
            &effective.profile_name,
            &dirs,
            access_key,
            secret_key,
        )?;
        let credential_path = credential.persist();

        let mut argv = vec![
            bucket.to_string(),
            effective.mountpoint.display().to_string(),
            "-f".to_string(),
            "-o".to_string(),
            format!("passwd_file={}", credential_path.display()),
        ];

        if let Some(endpoint) = &profile.endpoint {
            argv.push("-o".to_string());
            argv.push(format!("url={endpoint}"));
        }
        if profile.path_style == Some(true) {
            argv.push("-o".to_string());
            argv.push("use_path_request_style".to_string());
        }
        if let Some(region) = &profile.region {
            argv.push("-o".to_string());
            argv.push(format!("region={region}"));
        }
        argv.push("-o".to_string());
        argv.push("sigv4".to_string());

        if effective.read_only {
            argv.push("-o".to_string());
            argv.push("ro".to_string());
        }
        if effective.allow_other {
            argv.push("-o".to_string());
            argv.push("allow_other".to_string());
        }
        if let Some(uid) = profile.options.uid {
            argv.push("-o".to_string());
            argv.push(format!("uid={uid}"));
        }
        if let Some(gid) = profile.options.gid {
            argv.push("-o".to_string());
            argv.push(format!("gid={gid}"));
        }
        if let Some(umask) = &profile.options.umask {
            argv.push("-o".to_string());
            argv.push(format!("umask={umask}"));
        }
        if let Some(cache) = &profile.options.cache {
            argv.push("-o".to_string());
            argv.push(format!("use_cache={}", cache.display()));
        }

        let program = which::which(PROGRAM).unwrap_or_else(|_| PathBuf::from(PROGRAM));

        Ok(MountPlan {
            program,
            argv,
            env: Vec::new(),
            mountpoint: effective.mountpoint.clone(),
            unmount: UnmountStrategy::FuseMount3,
            credential_path: Some(credential_path),
            sidecars: Vec::new(),
        })
    }
}

impl S3fsEngine {
    /// Manual source-build command sequence for the family.
    fn source_commands(&self, family: DistroFamily) -> Vec<Vec<String>> {
        let mut commands = Vec::new();
        let prereqs = source_prerequisites(family);
        if !prereqs.is_empty() {
            commands.push(prereqs);
        }
        commands.push(argv(&[
            "git",
            "clone",
            "--depth",
            "1",
            "https://github.com/s3fs-fuse/s3fs-fuse.git",
        ]));
        commands.push(argv(&[
            "sh",
            "-c",
            "cd s3fs-fuse && ./autogen.sh && ./configure --with-openssl && make && sudo make install",
        ]));
        commands
    }
}

/// Build a `Vec<String>` from string literals.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

/// Distro-specific build prerequisites for `s3fs-fuse`.
fn source_prerequisites(family: DistroFamily) -> Vec<String> {
    match family {
        DistroFamily::Fedora | DistroFamily::Rhel => argv(&[
            "dnf",
            "install",
            "-y",
            "fuse3-devel",
            "libcurl-devel",
            "libxml2-devel",
            "openssl-devel",
            "automake",
            "gcc-c++",
            "mailcap",
        ]),
        DistroFamily::Debian => argv(&[
            "apt",
            "install",
            "-y",
            "libfuse3-dev",
            "libcurl4-openssl-dev",
            "libxml2-dev",
            "libssl-dev",
            "automake",
            "g++",
            "mime-support",
        ]),
        DistroFamily::Arch => argv(&[
            "pacman",
            "-S",
            "--noconfirm",
            "fuse3",
            "curl",
            "libxml2",
            "openssl",
            "automake",
            "gcc",
        ]),
        DistroFamily::Suse => argv(&[
            "zypper",
            "-n",
            "install",
            "fuse3-devel",
            "libcurl-devel",
            "libxml2-devel",
            "openssl-devel",
            "automake",
            "gcc-c++",
            "mime-types",
        ]),
        DistroFamily::Alpine => argv(&[
            "apk",
            "add",
            "fuse3-dev",
            "curl-dev",
            "libxml2-dev",
            "openssl-dev",
            "automake",
            "g++",
            "mailcap",
        ]),
        DistroFamily::Unknown => Vec::new(),
    }
}

/// Extract the first `\d+\.\d+(\.\d+)?` version from arbitrary text.
fn extract_version(text: &str) -> Option<Version> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                index += 1;
            }
            if let Some(version) = normalize_version(&text[start..index]) {
                return Some(version);
            }
        } else {
            index += 1;
        }
    }
    None
}

/// Parse a version, padding a missing patch component with zero.
fn normalize_version(candidate: &str) -> Option<Version> {
    let candidate = candidate.trim_end_matches('.');
    if let Ok(version) = Version::parse(candidate) {
        return Some(version);
    }
    let parts: Vec<&str> = candidate.split('.').collect();
    match parts.as_slice() {
        [major, minor] => Version::parse(&format!("{major}.{minor}.0")).ok(),
        _ => None,
    }
}

/// Construct a test profile.
#[cfg(test)]
fn test_profile() -> Profile {
    Profile {
        provider: crate::config::ProviderKind::S3,
        engine: Some("s3fs-fuse".to_string()),
        mountpoint: PathBuf::from("/home/u/mnt/bucket"),
        allow_other: false,
        read_only: false,
        credentials_ref: None,
        options: crate::config::MountOptions {
            uid: Some(1000),
            gid: Some(1000),
            umask: Some("077".to_string()),
            cache: Some(PathBuf::from("/home/u/.cache/mount-fs/bucket")),
        },
        bucket: Some("mybucket".to_string()),
        endpoint: Some("https://minio.example.com".to_string()),
        region: Some("us-east-1".to_string()),
        path_style: Some(true),
        drive_id: None,
        drive_type: None,
        client_id: None,
        client_secret: None,
        root_path: None,
    }
}

#[cfg(test)]
fn test_effective(read_only: bool, allow_other: bool) -> EffectiveMount {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    EffectiveMount {
        mountpoint: PathBuf::from("/home/u/mnt/bucket"),
        allow_other,
        read_only,
        profile_name: "bucket".to_string(),
        runtime_dir: std::env::temp_dir()
            .join(format!("mount-fs-s3fs-test-{}-{id}", std::process::id())),
    }
}

#[cfg(test)]
fn test_secrets() -> MountSecrets {
    MountSecrets {
        access_key: Some("AKIAEXAMPLE".to_string()),
        secret_key: Some("SECRETVALUE".to_string()),
        ..MountSecrets::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s3fs_version() {
        assert_eq!(
            extract_version("s3fs V1.97 (something)"),
            Some(Version::new(1, 97, 0))
        );
    }

    #[test]
    fn parses_aws_style_version() {
        assert_eq!(
            extract_version("Amazon Simple Storage Service File System V1.97"),
            Some(Version::new(1, 97, 0))
        );
    }

    #[test]
    fn parses_full_semver() {
        assert_eq!(
            extract_version("s3fs V1.97.2"),
            Some(Version::new(1, 97, 2))
        );
    }

    #[test]
    fn detects_engine_id_and_min_version() {
        let engine = S3fsEngine::new();
        assert_eq!(engine.id(), "s3fs-fuse");
        let requirement = engine.min_version().expect("min version");
        assert!(requirement.matches(&Version::new(1, 97, 0)));
        assert!(!requirement.matches(&Version::new(1, 90, 0)));
    }

    #[test]
    fn plan_mount_contains_expected_options() {
        let engine = S3fsEngine::new();
        let profile = test_profile();
        let effective = test_effective(false, false);
        let plan = engine
            .plan_mount(&profile, &effective, &test_secrets())
            .expect("plan");

        assert_eq!(plan.argv[0], "mybucket");
        assert_eq!(plan.argv[1], "/home/u/mnt/bucket");
        assert!(plan.argv.windows(2).any(|w| {
            w[0] == "-o" && w[1].starts_with("passwd_file=") && w[1].contains("creds-bucket-")
        }));
        assert!(plan
            .credential_path
            .as_deref()
            .map(|path| path.exists())
            .unwrap_or(false));
        assert!(plan
            .argv
            .iter()
            .any(|a| a == "url=https://minio.example.com"));
        assert!(plan.argv.iter().any(|a| a == "use_path_request_style"));
        assert!(plan.argv.iter().any(|a| a == "region=us-east-1"));
        assert!(plan.argv.iter().any(|a| a == "sigv4"));
        assert!(plan.argv.iter().any(|a| a == "uid=1000"));
        assert!(plan.argv.iter().any(|a| a == "gid=1000"));
        assert!(plan.argv.iter().any(|a| a == "umask=077"));
        assert!(plan
            .argv
            .iter()
            .any(|a| a == "use_cache=/home/u/.cache/mount-fs/bucket"));
        assert_eq!(plan.unmount, UnmountStrategy::FuseMount3);
        assert!(plan.env.is_empty());
    }

    #[test]
    fn plan_mount_adds_read_only_and_allow_other() {
        let engine = S3fsEngine::new();
        let plan = engine
            .plan_mount(
                &test_profile(),
                &test_effective(true, true),
                &test_secrets(),
            )
            .expect("plan");
        assert!(plan.argv.windows(2).any(|w| w == ["-o", "ro"]));
        assert!(plan.argv.windows(2).any(|w| w == ["-o", "allow_other"]));
    }

    #[test]
    fn plan_mount_never_contains_secrets() {
        let engine = S3fsEngine::new();
        let secret = "SUPER_SECRET_ACCESS_KEY";
        let plan = engine
            .plan_mount(
                &test_profile(),
                &test_effective(false, false),
                &test_secrets(),
            )
            .expect("plan");
        assert!(!plan.argv.iter().any(|arg| arg.contains(secret)));
        assert!(!plan.argv.iter().any(|arg| arg.contains("AKIA")));
        assert!(plan.env.is_empty());
    }

    #[test]
    fn install_plan_uses_package_manager() {
        let engine = S3fsEngine::new();
        let distro = crate::system::distro::parse("ID=fedora\n");
        let plan = engine.install_plan(&distro, PackageManager::Dnf);
        assert_eq!(plan.method, InstallMethod::PackageManager);
        assert_eq!(plan.packages, vec!["s3fs-fuse"]);
        assert!(plan.requires_root);
        assert!(!plan.source_commands.is_empty());
    }

    #[test]
    fn install_plan_falls_back_to_family() {
        let engine = S3fsEngine::new();
        let distro = crate::system::distro::parse("ID=ubuntu\nID_LIKE=debian\n");
        let plan = engine.install_plan(&distro, PackageManager::Unknown);
        assert_eq!(plan.method, InstallMethod::PackageManager);
        assert_eq!(plan.packages, vec!["s3fs-fuse"]);
    }

    #[test]
    fn install_plan_unavailable_when_unknown() {
        let engine = S3fsEngine::new();
        let distro = crate::system::distro::parse("");
        let plan = engine.install_plan(&distro, PackageManager::Unknown);
        assert_eq!(plan.method, InstallMethod::Unavailable);
        assert!(plan.packages.is_empty());
    }

    #[test]
    fn plan_mount_requires_bucket() {
        let engine = S3fsEngine::new();
        let mut profile = test_profile();
        profile.bucket = None;
        assert!(engine
            .plan_mount(&profile, &test_effective(false, false), &test_secrets())
            .is_err());
    }

    #[test]
    fn source_commands_present() {
        let engine = S3fsEngine::new();
        let commands = engine.source_commands(DistroFamily::Fedora);
        assert!(commands
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("git")));
        assert!(commands
            .iter()
            .filter_map(|c| c.last())
            .any(|arg| arg.contains("make install")));
    }

    #[test]
    fn path_style_false_omits_option() {
        let engine = S3fsEngine::new();
        let mut profile = test_profile();
        profile.path_style = Some(false);
        let plan = engine
            .plan_mount(&profile, &test_effective(false, false), &test_secrets())
            .expect("plan");
        assert!(!plan.argv.iter().any(|a| a == "use_path_request_style"));
    }

    #[test]
    fn program_is_populated() {
        let engine = S3fsEngine::new();
        let plan = engine
            .plan_mount(
                &test_profile(),
                &test_effective(false, false),
                &test_secrets(),
            )
            .expect("plan");
        assert!(!plan.program.as_os_str().is_empty());
    }
}
