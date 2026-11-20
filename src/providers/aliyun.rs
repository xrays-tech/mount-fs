//! Aliyun Drive provider.

use crate::config::{Profile, ProviderKind};
use crate::core::creds;
use crate::engines::aliyundrive_fuse::AliyundriveFuseEngine;
use crate::engines::aliyundrive_webdav::AliyundriveWebdavEngine;
use crate::engines::qrypt::QryptEngine;
use crate::engines::Engine;
use crate::error::{AppError, Result};
use crate::providers::Provider;

const SUPPORTED_ENGINES: &[&str] = &["aliyundrive-fuse", "qrypt", "aliyundrive-webdav"];

/// Aliyun Drive provider.
#[derive(Debug, Default)]
pub struct AliyunProvider;

impl AliyunProvider {
    /// Create the provider.
    pub fn new() -> Self {
        AliyunProvider
    }
}

impl Provider for AliyunProvider {
    fn id(&self) -> &'static str {
        "aliyun"
    }

    fn engines(&self) -> Vec<Box<dyn Engine>> {
        vec![
            Box::new(AliyundriveFuseEngine::new()),
            Box::new(QryptEngine::new()),
            Box::new(AliyundriveWebdavEngine::new()),
        ]
    }

    fn validate(&self, profile: &Profile) -> Result<()> {
        if profile.provider != ProviderKind::Aliyun {
            return Err(AppError::Unsupported(format!(
                "profile uses provider `{}`, expected `aliyun`",
                profile.provider
            )));
        }

        let Some(engine_id) = profile.engine.as_deref() else {
            // Engine selection happens later; variant-specific checks below.
            return Ok(());
        };

        match engine_id {
            "aliyundrive-fuse" => {
                if creds::has_aliyun_token(profile) {
                    Ok(())
                } else {
                    Err(AppError::Credential(
                        "aliyundrive-fuse requires a legacy web refresh token; set \
                         `credentials_ref` or ALIYUN_REFRESH_TOKEN"
                            .to_string(),
                    ))
                }
            }
            "qrypt" => {
                // `drive_id` is validated at plan time so an interactive TTY
                // prompt can still supply it.
                if creds::has_aliyun_token(profile) {
                    Ok(())
                } else {
                    Err(AppError::Credential(
                        "qrypt requires an OpenAPI refresh token; set `credentials_ref` or \
                         ALIYUN_REFRESH_TOKEN"
                            .to_string(),
                    ))
                }
            }
            "aliyundrive-webdav" => {
                if creds::has_aliyun_token(profile) {
                    Ok(())
                } else {
                    Err(AppError::Credential(
                        "aliyundrive-webdav requires an OpenAPI refresh token".to_string(),
                    ))
                }
            }
            other => Err(AppError::Unsupported(format!(
                "unknown Aliyun engine `{other}`; supported: {}",
                SUPPORTED_ENGINES.join(", ")
            ))),
        }
    }

    fn select_engine(&self, profile: &Profile) -> Result<Box<dyn Engine>> {
        let engines = self.engines();
        if let Some(id) = profile.engine.as_deref() {
            return engines
                .into_iter()
                .find(|engine| engine.id() == id)
                .ok_or_else(|| {
                    AppError::NotFound(format!(
                        "engine `{id}` is not supported for Aliyun; supported: {}",
                        SUPPORTED_ENGINES.join(", ")
                    ))
                });
        }

        for engine in engines {
            if engine.detect().found {
                return Ok(engine);
            }
        }

        Err(AppError::NotFound(
            "no Aliyun engine is installed; run: mount-fs install aliyundrive-fuse".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn profile(engine: Option<&str>, drive_id: Option<&str>) -> Profile {
        Profile {
            provider: ProviderKind::Aliyun,
            engine: engine.map(str::to_string),
            mountpoint: PathBuf::from("/mnt/pan"),
            allow_other: false,
            read_only: false,
            credentials_ref: None,
            options: crate::config::MountOptions::default(),
            bucket: None,
            endpoint: None,
            region: None,
            path_style: None,
            drive_id: drive_id.map(str::to_string),
        }
    }

    #[test]
    fn qrypt_drive_id_is_not_required_at_validate_time() {
        let provider = AliyunProvider::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let token_file = temp.path().join("token");
        std::fs::write(&token_file, "token-value\n").expect("write");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");

        let mut with_token = profile(Some("qrypt"), None);
        with_token.credentials_ref = Some(token_file);
        // No drive_id yet: allowed (prompted/validated at plan time).
        assert!(provider.validate(&with_token).is_ok());
    }

    #[test]
    fn rejects_unknown_engine_with_list() {
        let provider = AliyunProvider::new();
        let err = provider
            .validate(&profile(Some("nope"), None))
            .expect_err("unknown engine");
        let message = err.to_string();
        assert!(message.contains("aliyundrive-fuse"));
        assert!(message.contains("qrypt"));
    }

    #[test]
    fn explicit_engine_selection_is_honored() {
        let provider = AliyunProvider::new();
        let engine = provider
            .select_engine(&profile(Some("qrypt"), Some("drive-1")))
            .expect("select");
        assert_eq!(engine.id(), "qrypt");
    }

    #[test]
    fn missing_engine_is_not_found() {
        let provider = AliyunProvider::new();
        assert!(provider
            .select_engine(&profile(Some("nope"), None))
            .is_err());
    }
}
