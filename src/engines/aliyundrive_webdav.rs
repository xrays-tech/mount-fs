//! `aliyundrive-webdav` engine (optional; needs an rclone WebDAV bridge).
//!
//! The upstream tool is a WebDAV *server*, not a FUSE filesystem. Mounting
//! therefore requires two processes (the server and `rclone mount`) sharing a
//! loopback port. Full live orchestration is deferred to M5; this engine
//! constructs both commands and, until the bridge is wired, `plan_mount`
//! returns a clear error.

use semver::VersionReq;

use crate::config::Profile;
use crate::engines::{
    Capabilities, CredentialRetention, DetectResult, EffectiveMount, Engine, InstallMethod,
    InstallPlan, MountPlan, MountSecrets, ReleaseAsset, TokenKind,
};
use crate::error::{AppError, Result};
use crate::system::distro::Distro;
use crate::system::download::ArchiveKind;
use crate::system::pkg::PackageManager;
use crate::system::process::CommandSpec;

const PROGRAM: &str = "aliyundrive-webdav";
const VERSION: &str = "2.3.3";
const MIN_VERSION: &str = ">=2.3.3";

/// `aliyundrive-webdav` + rclone bridge engine.
#[derive(Debug, Default)]
pub struct AliyundriveWebdavEngine;

impl AliyundriveWebdavEngine {
    /// Create the engine.
    pub fn new() -> Self {
        AliyundriveWebdavEngine
    }

    /// Pinned release for `x86_64` musl, when available.
    pub fn release_asset() -> Option<ReleaseAsset> {
        let arch = std::env::consts::ARCH;
        if arch != "x86_64" && arch != "aarch64" {
            return None;
        }
        let file = format!("aliyundrive-webdav-v{VERSION}.{arch}-unknown-linux-musl.tar.gz");
        Some(ReleaseAsset {
            version: VERSION.to_string(),
            url: format!(
                "https://github.com/messense/aliyundrive-webdav/releases/download/v{VERSION}/{file}"
            ),
            kind: ArchiveKind::TarGz,
            sha256: crate::system::checksums::lookup(PROGRAM, VERSION, arch),
            binary: PROGRAM.to_string(),
        })
    }

    /// The WebDAV server command (`aliyundrive-webdav`).
    pub fn server_command(effective: &EffectiveMount, read_only: bool) -> CommandSpec {
        let mut spec = CommandSpec::new(PROGRAM)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--workdir")
            .arg(effective.runtime_dir.display().to_string());
        if read_only {
            spec = spec.arg("--read-only");
        }
        spec
    }

    /// The `rclone mount` command that bridges the WebDAV server.
    pub fn rclone_command(effective: &EffectiveMount, endpoint: &str) -> CommandSpec {
        CommandSpec::new("rclone")
            .arg("mount")
            .arg(format!(":webdav:{endpoint}"))
            .arg(effective.mountpoint.display().to_string())
            .arg("--vfs-cache-mode")
            .arg("writes")
    }
}

impl Engine for AliyundriveWebdavEngine {
    fn id(&self) -> &'static str {
        "aliyundrive-webdav"
    }

    fn detect(&self) -> DetectResult {
        crate::engines::probe_binary(PROGRAM)
    }

    fn min_version(&self) -> Option<VersionReq> {
        VersionReq::parse(MIN_VERSION).ok()
    }

    fn install_plan(&self, _distro: &Distro, _pkg: PackageManager) -> InstallPlan {
        let notes = vec![
            "Optional engine: WebDAV server bridged to FUSE via rclone (2 processes, 1 port)."
                .to_string(),
            "Requires the external `rclone` binary.".to_string(),
            "Live orchestration of the bridge lands in M5.".to_string(),
        ];

        match Self::release_asset() {
            Some(release) => InstallPlan {
                method: InstallMethod::ReleaseArchive,
                packages: Vec::new(),
                requires_root: false,
                notes,
                source_commands: Vec::new(),
                release: Some(release),
            },
            None => InstallPlan {
                method: InstallMethod::Unavailable,
                requires_root: false,
                notes,
                ..InstallPlan::default()
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_write: true,
            allow_other: true,
            foreground: true,
        }
    }

    fn token_kind(&self) -> TokenKind {
        TokenKind::OpenApi
    }

    fn credential_retention(&self) -> CredentialRetention {
        CredentialRetention::Temp
    }

    fn plan_mount(
        &self,
        _profile: &Profile,
        _effective: &EffectiveMount,
        _secrets: &MountSecrets,
    ) -> Result<MountPlan> {
        Err(AppError::Unsupported(
            "aliyundrive-webdav needs the rclone WebDAV bridge; live orchestration lands in M5 \
             (use `aliyundrive-fuse` or `qrypt` for now)"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn effective() -> EffectiveMount {
        EffectiveMount {
            mountpoint: PathBuf::from("/home/u/mnt/pan"),
            allow_other: false,
            read_only: false,
            profile_name: "pan".to_string(),
            runtime_dir: PathBuf::from("/run/user/1000/mount-fs"),
        }
    }

    #[test]
    fn constructs_server_and_bridge_commands() {
        let server = AliyundriveWebdavEngine::server_command(&effective(), true);
        assert_eq!(server.program, PathBuf::from("aliyundrive-webdav"));
        assert!(server.args.contains(&"--read-only".to_string()));
        assert!(server.args.contains(&"--workdir".to_string()));

        let bridge = AliyundriveWebdavEngine::rclone_command(&effective(), "http://127.0.0.1:8080");
        assert_eq!(bridge.program, PathBuf::from("rclone"));
        assert_eq!(bridge.args[0], "mount");
    }

    #[test]
    fn plan_mount_is_explicitly_unsupported_for_now() {
        let engine = AliyundriveWebdavEngine::new();
        assert!(engine
            .plan_mount(
                &crate::config::Profile::default_for_test(),
                &effective(),
                &MountSecrets::default()
            )
            .is_err());
        assert_eq!(engine.token_kind(), TokenKind::OpenApi);
    }
}
