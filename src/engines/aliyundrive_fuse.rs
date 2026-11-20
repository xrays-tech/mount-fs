//! `aliyundrive-fuse` engine (archived upstream; legacy web token, read-only).

use std::path::PathBuf;

use semver::VersionReq;

use crate::config::Profile;
use crate::core::creds;
use crate::engines::{
    Capabilities, CredentialRetention, DetectResult, EffectiveMount, Engine, InstallMethod,
    InstallPlan, MountPlan, MountSecrets, ReleaseAsset, TokenKind, UnmountStrategy,
};
use crate::error::{AppError, Result};
use crate::system::distro::Distro;
use crate::system::download::ArchiveKind;
use crate::system::pkg::PackageManager;

const PROGRAM: &str = "aliyundrive-fuse";
const VERSION: &str = "0.1.14";
const MIN_VERSION: &str = ">=0.1.14";

/// `aliyundrive-fuse` mount engine.
#[derive(Debug, Default)]
pub struct AliyundriveFuseEngine;

impl AliyundriveFuseEngine {
    /// Create the engine.
    pub fn new() -> Self {
        AliyundriveFuseEngine
    }

    /// The pinned musl release for the current architecture, if supported.
    pub fn release_asset() -> Option<ReleaseAsset> {
        let arch = std::env::consts::ARCH;
        if arch != "x86_64" && arch != "aarch64" {
            return None;
        }
        let file = format!("aliyundrive-fuse-v{VERSION}.{arch}-unknown-linux-musl.tar.gz");
        Some(ReleaseAsset {
            version: VERSION.to_string(),
            url: format!(
                "https://github.com/messense/aliyundrive-fuse/releases/download/v{VERSION}/{file}"
            ),
            kind: ArchiveKind::TarGz,
            sha256: crate::system::checksums::lookup(PROGRAM, VERSION, arch),
            binary: PROGRAM.to_string(),
        })
    }

    /// Per-profile work directory that holds the `refresh_token` file.
    fn workdir(effective: &EffectiveMount) -> PathBuf {
        effective
            .runtime_dir
            .join(creds::aliyundrive_workdir_name(&effective.profile_name))
    }
}

impl Engine for AliyundriveFuseEngine {
    fn id(&self) -> &'static str {
        "aliyundrive-fuse"
    }

    fn detect(&self) -> DetectResult {
        crate::engines::probe_binary(PROGRAM)
    }

    fn min_version(&self) -> Option<VersionReq> {
        VersionReq::parse(MIN_VERSION).ok()
    }

    fn install_plan(&self, _distro: &Distro, _pkg: PackageManager) -> InstallPlan {
        let mut notes = vec![
            "Read-only mount with the legacy Aliyun web token.".to_string(),
            "Requires FUSE 3 (fusermount3).".to_string(),
            "Experimental alternative: `cargo install --locked aliyundrive-fuse`.".to_string(),
            "Upstream is archived (last release 2022); the OpenAPI token is not compatible."
                .to_string(),
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
            None => {
                notes.push(format!(
                    "No pinned release for arch `{}`.",
                    std::env::consts::ARCH
                ));
                InstallPlan {
                    method: InstallMethod::Unavailable,
                    requires_root: false,
                    notes,
                    ..InstallPlan::default()
                }
            }
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_write: false,
            allow_other: true,
            foreground: true,
        }
    }

    fn token_kind(&self) -> TokenKind {
        TokenKind::LegacyWeb
    }

    fn credential_retention(&self) -> CredentialRetention {
        CredentialRetention::Temp
    }

    fn plan_mount(
        &self,
        _profile: &Profile,
        effective: &EffectiveMount,
        secrets: &MountSecrets,
    ) -> Result<MountPlan> {
        let token = secrets
            .refresh_token
            .as_deref()
            .ok_or_else(|| AppError::Credential("missing Aliyun refresh token".to_string()))?;

        let workdir = Self::workdir(effective);
        let token_path = workdir.join("refresh_token");
        // The token is passed via the `REFRESH_TOKEN` env var and a pre-written
        // 0600 workdir file; never via `-r/--refresh-token` (argv).
        creds::create_private_dir(&workdir)?;
        creds::write_private_file(&token_path, token.as_bytes())?;

        let mut argv = vec![
            effective.mountpoint.display().to_string(),
            "--workdir".to_string(),
            workdir.display().to_string(),
        ];
        if effective.allow_other {
            argv.push("--allow-other".to_string());
        }

        let program = which::which(PROGRAM).unwrap_or_else(|_| PathBuf::from(PROGRAM));

        Ok(MountPlan {
            program,
            argv,
            env: vec![("REFRESH_TOKEN".to_string(), token.to_string())],
            mountpoint: effective.mountpoint.clone(),
            unmount: UnmountStrategy::FuseMount3,
            credential_path: Some(token_path),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::MountSecrets;

    fn effective() -> EffectiveMount {
        EffectiveMount {
            mountpoint: PathBuf::from("/home/u/mnt/pan"),
            allow_other: false,
            read_only: false,
            profile_name: "pan".to_string(),
            runtime_dir: std::env::temp_dir(),
        }
    }

    #[test]
    fn argv_never_contains_the_token() {
        let engine = AliyundriveFuseEngine::new();
        let secrets = MountSecrets {
            refresh_token: Some("REFRESH_SECRET".to_string()),
            ..MountSecrets::default()
        };
        let plan = engine
            .plan_mount(
                &crate::config::Profile::default_for_test(),
                &effective(),
                &secrets,
            )
            .expect("plan");

        assert!(!plan.argv.iter().any(|arg| arg.contains("REFRESH_SECRET")));
        assert!(!plan.argv.iter().any(|arg| arg == "-r"));
        assert!(!plan.argv.iter().any(|arg| arg == "--refresh-token"));
        assert_eq!(
            plan.env,
            vec![("REFRESH_TOKEN".to_string(), "REFRESH_SECRET".to_string())]
        );
        assert!(plan.argv.contains(&"--workdir".to_string()));
        assert!(plan.argv.contains(&"/home/u/mnt/pan".to_string()));
    }

    #[test]
    fn require_token() {
        let engine = AliyundriveFuseEngine::new();
        assert!(engine
            .plan_mount(
                &crate::config::Profile::default_for_test(),
                &effective(),
                &MountSecrets::default()
            )
            .is_err());
    }

    #[test]
    fn capabilities_and_token() {
        let engine = AliyundriveFuseEngine::new();
        assert!(!engine.capabilities().read_write);
        assert_eq!(engine.token_kind(), TokenKind::LegacyWeb);
        assert_eq!(engine.credential_retention(), CredentialRetention::Temp);
    }
}
