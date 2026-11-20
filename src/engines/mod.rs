//! Mount engine abstractions.
//!
//! An engine wraps one external FUSE tool (`s3fs-fuse`, `aliyundrive-fuse`,
//! `qrypt`, ...). The [`Engine`] trait is the extension point for backends.

pub mod aliyundrive_fuse;
pub mod aliyundrive_webdav;
pub mod qrypt;
pub mod s3fs;

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

use semver::{Version, VersionReq};

use crate::config::Profile;
use crate::system::distro::Distro;
use crate::system::download::ArchiveKind;
use crate::system::pkg::PackageManager;

/// Result of probing an engine binary.
#[derive(Debug, Clone, Default)]
pub struct DetectResult {
    /// Whether the binary was found.
    pub found: bool,
    /// Resolved path, if found.
    pub path: Option<PathBuf>,
    /// Parsed semantic version, if available.
    pub version: Option<Version>,
    /// Raw version string as printed by the tool.
    pub version_raw: Option<String>,
}

/// How an engine can be installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstallMethod {
    /// Install through the system package manager.
    PackageManager,
    /// Build from source (manual / explicit only for now).
    SourceBuild,
    /// Download a pinned release archive and install the binary.
    ReleaseArchive,
    /// No automatic method is available.
    #[default]
    Unavailable,
}

/// A pinned release archive to download and install.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    /// Upstream version string.
    pub version: String,
    /// Download URL.
    pub url: String,
    /// Archive format.
    pub kind: ArchiveKind,
    /// Pinned sha256, if known.
    pub sha256: Option<&'static str>,
    /// Binary name inside the archive.
    pub binary: String,
}

/// A data-only installation plan.
#[derive(Debug, Clone, Default)]
pub struct InstallPlan {
    /// Preferred installation method.
    pub method: InstallMethod,
    /// Packages to install via the package manager.
    pub packages: Vec<String>,
    /// Whether installation requires root privileges.
    pub requires_root: bool,
    /// Human-readable notes and caveats.
    pub notes: Vec<String>,
    /// Manual source-build commands (never executed implicitly).
    pub source_commands: Vec<Vec<String>>,
    /// Release archive details for [`InstallMethod::ReleaseArchive`].
    pub release: Option<ReleaseAsset>,
}

/// Engine feature support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Supports read/write mounts.
    pub read_write: bool,
    /// Supports `allow_other`.
    pub allow_other: bool,
    /// Can run in the foreground.
    pub foreground: bool,
}

/// Which token system an engine consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Legacy Aliyun Drive web token (`aliyundrive-fuse`).
    LegacyWeb,
    /// Aliyun OpenAPI refresh token + drive id (`qrypt`, webdav).
    OpenApi,
    /// S3 access key / secret key (`s3fs-fuse`).
    AwsKey,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenKind::LegacyWeb => "legacy-web",
            TokenKind::OpenApi => "openapi",
            TokenKind::AwsKey => "aws-key",
        })
    }
}

/// Whether an engine's credential file is transient or persistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRetention {
    /// Removed on unmount/rollback (runtime artifacts).
    Temp,
    /// Intended to survive (e.g. a user-managed credential file).
    Persistent,
}

impl fmt::Display for CredentialRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CredentialRetention::Temp => "temp",
            CredentialRetention::Persistent => "persistent",
        })
    }
}

/// How to unmount a filesystem produced by an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmountStrategy {
    /// `fusermount3 -u <mountpoint>`.
    FuseMount3,
    /// `fusermount3 -uz <mountpoint>` (lazy).
    LazyFuseMount3,
    /// `umount <mountpoint>` (root).
    Umount,
}

/// Readiness condition before starting the next process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Process can be considered started immediately.
    Immediate,
    /// Wait for a TCP listener on `127.0.0.1:<port>`.
    TcpPort(u16),
}

/// An additional process that must run alongside the main mount process.
#[derive(Clone)]
pub struct PlannedProcess {
    /// Program to execute.
    pub program: PathBuf,
    /// Arguments (never containing credentials).
    pub argv: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Readiness condition before the main process starts.
    pub readiness: Readiness,
}

impl fmt::Debug for PlannedProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: Vec<String> = self
            .env
            .iter()
            .map(|(key, _)| format!("{key}=<redacted>"))
            .collect();
        f.debug_struct("PlannedProcess")
            .field("program", &self.program)
            .field("argv", &self.argv)
            .field("env", &env)
            .field("readiness", &self.readiness)
            .finish()
    }
}

/// Secrets resolved for a mount (never serialized or logged).
#[derive(Clone, Default)]
pub struct MountSecrets {
    /// S3 access key id.
    pub access_key: Option<String>,
    /// S3 secret access key.
    pub secret_key: Option<String>,
    /// Aliyun refresh token.
    pub refresh_token: Option<String>,
    /// Aliyun OpenAPI drive id.
    pub drive_id: Option<String>,
}

/// A fully resolved mount command (no secrets in `argv`).
#[derive(Clone)]
pub struct MountPlan {
    /// Engine binary to execute.
    pub program: PathBuf,
    /// Arguments, never containing credentials.
    pub argv: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Mountpoint.
    pub mountpoint: PathBuf,
    /// Unmount strategy.
    pub unmount: UnmountStrategy,
    /// Temporary credential artifact to keep while mounted and unlink later.
    pub credential_path: Option<PathBuf>,
    /// Additional processes (e.g. a WebDAV server) started before the main one.
    pub sidecars: Vec<PlannedProcess>,
}

impl fmt::Debug for MountSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn redacted(value: &Option<String>) -> Option<&'static str> {
            value.as_ref().map(|_| "<redacted>")
        }
        f.debug_struct("MountSecrets")
            .field("access_key", &redacted(&self.access_key))
            .field("secret_key", &redacted(&self.secret_key))
            .field("refresh_token", &redacted(&self.refresh_token))
            .field("drive_id", &redacted(&self.drive_id))
            .finish()
    }
}

impl fmt::Debug for MountPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: Vec<String> = self
            .env
            .iter()
            .map(|(key, _)| format!("{key}=<redacted>"))
            .collect();
        f.debug_struct("MountPlan")
            .field("program", &self.program)
            .field("argv", &self.argv)
            .field("env", &env)
            .field("mountpoint", &self.mountpoint)
            .field("unmount", &self.unmount)
            .field("credential_path", &self.credential_path)
            .field("sidecars", &self.sidecars)
            .finish()
    }
}

/// Runtime-resolved mount settings (CLI flags override profile values).
#[derive(Debug, Clone, Default)]
pub struct EffectiveMount {
    /// Effective mountpoint.
    pub mountpoint: PathBuf,
    /// Effective `allow_other` setting.
    pub allow_other: bool,
    /// Effective read-only setting.
    pub read_only: bool,
    /// Profile name (used for artifact filenames).
    pub profile_name: String,
    /// Runtime directory (`$XDG_RUNTIME_DIR/mount-fs`).
    pub runtime_dir: PathBuf,
}

/// A mount engine backed by an external executable.
pub trait Engine: Send + Sync {
    /// Stable engine identifier (e.g. `s3fs-fuse`).
    fn id(&self) -> &'static str;

    /// Probe whether the engine is installed.
    fn detect(&self) -> DetectResult;

    /// Minimum supported version, if any.
    fn min_version(&self) -> Option<VersionReq>;

    /// Build an installation plan for the detected host.
    fn install_plan(&self, distro: &Distro, pkg: PackageManager) -> InstallPlan;

    /// Engine capabilities.
    fn capabilities(&self) -> Capabilities;

    /// Token system consumed by this engine.
    fn token_kind(&self) -> TokenKind;

    /// Whether the engine's credential artifact is transient or persistent.
    fn credential_retention(&self) -> CredentialRetention {
        CredentialRetention::Temp
    }

    /// Build the mount command, writing any engine-specific credential artifact.
    fn plan_mount(
        &self,
        profile: &Profile,
        effective: &EffectiveMount,
        secrets: &MountSecrets,
    ) -> crate::error::Result<MountPlan>;
}

/// Probe `<program> --version` and parse a version from its output.
pub(crate) fn probe_binary(program: &str) -> DetectResult {
    let path = which::which(program).ok();
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
            (first_version(&text), raw)
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

/// Extract the first `\d+\.\d+(\.\d+)?` version from arbitrary text.
pub(crate) fn first_version(text: &str) -> Option<Version> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                index += 1;
            }
            let candidate = text[start..index].trim_end_matches('.');
            if let Ok(version) = Version::parse(candidate) {
                return Some(version);
            }
            let parts: Vec<&str> = candidate.split('.').collect();
            if let [major, minor] = parts.as_slice() {
                if let Ok(version) = Version::parse(&format!("{major}.{minor}.0")) {
                    return Some(version);
                }
            }
        } else {
            index += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let secrets = MountSecrets {
            access_key: Some("ACCESS_SECRET".to_string()),
            secret_key: Some("AWS_SECRET".to_string()),
            refresh_token: Some("REFRESH_SECRET".to_string()),
            drive_id: Some("DRIVE_SECRET".to_string()),
        };
        let rendered = format!("{secrets:?}");
        assert!(!rendered.contains("ACCESS_SECRET"), "{rendered}");
        assert!(!rendered.contains("AWS_SECRET"), "{rendered}");
        assert!(!rendered.contains("REFRESH_SECRET"), "{rendered}");
        assert!(!rendered.contains("DRIVE_SECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn mount_plan_debug_redacts_env_values() {
        let plan = MountPlan {
            program: PathBuf::from("/bin/engine"),
            argv: vec!["--workdir".to_string(), "/tmp".to_string()],
            env: vec![("REFRESH_TOKEN".to_string(), "TOKEN_SECRET".to_string())],
            mountpoint: PathBuf::from("/mnt/x"),
            unmount: UnmountStrategy::FuseMount3,
            credential_path: None,
            sidecars: Vec::new(),
        };
        let rendered = format!("{plan:?}");
        assert!(!rendered.contains("TOKEN_SECRET"), "{rendered}");
        assert!(rendered.contains("REFRESH_TOKEN=<redacted>"), "{rendered}");
    }
}
