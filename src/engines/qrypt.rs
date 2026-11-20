//! `qrypt` engine (OpenAPI refresh token + drive id, foreground FUSE3).

use std::path::{Path, PathBuf};

use semver::VersionReq;
use serde::Serialize;

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

const PROGRAM: &str = "qrypt";
const VERSION: &str = "0.7.0";
const MIN_VERSION: &str = ">=0.7.0";

/// `qrypt` mount engine.
#[derive(Debug, Default)]
pub struct QryptEngine;

impl QryptEngine {
    /// Create the engine.
    pub fn new() -> Self {
        QryptEngine
    }

    /// The pinned release for the current architecture, if supported.
    pub fn release_asset() -> Option<ReleaseAsset> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => return None,
        };
        let file = format!("qrypt_{VERSION}_linux_{arch}.tar.gz");
        Some(ReleaseAsset {
            version: VERSION.to_string(),
            url: format!(
                "https://github.com/yinzhenyu-su/qrypt/releases/download/v{VERSION}/{file}"
            ),
            kind: ArchiveKind::TarGz,
            sha256: crate::system::checksums::lookup(PROGRAM, VERSION, std::env::consts::ARCH),
            binary: PROGRAM.to_string(),
        })
    }

    /// Render the qrypt TOML configuration (pure/testable).
    pub fn render_config(
        profile_name: &str,
        refresh_token: &str,
        drive_id: &str,
        mountpoint: &Path,
        root_path: Option<&str>,
    ) -> Result<String> {
        let mount_point = mountpoint.to_string_lossy();
        let config = QryptConfig {
            mounts: vec![QryptMount {
                name: profile_name,
                kind: "aliyundrive",
                params: QryptParams {
                    refresh_token,
                    drive_id,
                    root_path,
                },
            }],
            mount: QryptMountSection {
                mount_point: mount_point.as_ref(),
            },
        };
        toml::to_string_pretty(&config)
            .map_err(|err| AppError::Config(format!("cannot render qrypt config: {err}")))
    }

    fn config_path(effective: &EffectiveMount) -> PathBuf {
        effective
            .runtime_dir
            .join(creds::qrypt_config_name(&effective.profile_name))
    }
}

#[derive(Serialize)]
struct QryptConfig<'a> {
    mounts: Vec<QryptMount<'a>>,
    mount: QryptMountSection<'a>,
}

#[derive(Serialize)]
struct QryptMount<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    params: QryptParams<'a>,
}

#[derive(Serialize)]
struct QryptParams<'a> {
    refresh_token: &'a str,
    drive_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_path: Option<&'a str>,
}

#[derive(Serialize)]
struct QryptMountSection<'a> {
    mount_point: &'a str,
}

impl Engine for QryptEngine {
    fn id(&self) -> &'static str {
        "qrypt"
    }

    fn detect(&self) -> DetectResult {
        crate::engines::probe_binary(PROGRAM)
    }

    fn min_version(&self) -> Option<VersionReq> {
        VersionReq::parse(MIN_VERSION).ok()
    }

    fn install_plan(&self, _distro: &Distro, _pkg: PackageManager) -> InstallPlan {
        let mut notes = vec![
            "Read/write Aliyun Drive mount using OpenAPI credentials.".to_string(),
            "Requires a refresh token and a drive id (no interactive login in qrypt).".to_string(),
            "Runs in the foreground; mount-fs supervises it in the background.".to_string(),
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
        effective: &EffectiveMount,
        secrets: &MountSecrets,
    ) -> Result<MountPlan> {
        let token = secrets
            .refresh_token
            .as_deref()
            .ok_or_else(|| AppError::Credential("missing Aliyun refresh token".to_string()))?;
        let drive_id = secrets
            .drive_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AppError::Config("qrypt requires a non-empty `drive_id`".to_string()))?;

        let config_path = Self::config_path(effective);
        let config = Self::render_config(
            &effective.profile_name,
            token,
            drive_id,
            &effective.mountpoint,
            None,
        )?;
        creds::write_private_file(&config_path, config.as_bytes())?;

        let program = which::which(PROGRAM).unwrap_or_else(|_| PathBuf::from(PROGRAM));

        Ok(MountPlan {
            program,
            argv: vec![
                "mount".to_string(),
                "--config".to_string(),
                config_path.display().to_string(),
                "--mount-point".to_string(),
                effective.mountpoint.display().to_string(),
            ],
            env: Vec::new(),
            mountpoint: effective.mountpoint.clone(),
            unmount: UnmountStrategy::FuseMount3,
            credential_path: Some(config_path),
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
    fn renders_expected_toml() {
        let config = QryptEngine::render_config(
            "pan",
            "REFRESH_SECRET",
            "drive-123",
            Path::new("/home/u/mnt/pan"),
            None,
        )
        .expect("render");
        assert!(config.contains("[[mounts]]"), "config: {config}");
        assert!(
            config.contains("type = \"aliyundrive\""),
            "config: {config}"
        );
        assert!(
            config.contains("refresh_token = \"REFRESH_SECRET\""),
            "config: {config}"
        );
        assert!(
            config.contains("drive_id = \"drive-123\""),
            "config: {config}"
        );
        assert!(config.contains("[mount]"), "config: {config}");
        assert!(config.contains("/home/u/mnt/pan"), "config: {config}");
    }

    #[test]
    fn plan_mount_uses_config_and_never_puts_secret_in_argv() {
        let engine = QryptEngine::new();
        let secrets = MountSecrets {
            refresh_token: Some("REFRESH_SECRET".to_string()),
            drive_id: Some("drive-123".to_string()),
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
        assert_eq!(plan.argv[0], "mount");
        assert!(plan.argv.contains(&"--config".to_string()));
        assert!(plan.argv.contains(&"--mount-point".to_string()));
        let credential_path = plan.credential_path.expect("config path");
        let config = std::fs::read_to_string(&credential_path).expect("read config");
        assert!(config.contains("REFRESH_SECRET"));
        assert!(config.contains("drive-123"));
        let _ = std::fs::remove_file(credential_path);
    }

    #[test]
    fn plan_mount_requires_drive_id() {
        let engine = QryptEngine::new();
        let secrets = MountSecrets {
            refresh_token: Some("token".to_string()),
            ..MountSecrets::default()
        };
        assert!(engine
            .plan_mount(
                &crate::config::Profile::default_for_test(),
                &effective(),
                &secrets
            )
            .is_err());
    }
}
