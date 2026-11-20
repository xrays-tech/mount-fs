//! S3-compatible object storage provider.

use crate::config::{Profile, ProviderKind};
use crate::engines::{s3fs::S3fsEngine, Engine};
use crate::error::{AppError, Result};
use crate::providers::Provider;

/// S3-compatible object storage provider.
#[derive(Debug, Default)]
pub struct S3Provider;

impl S3Provider {
    /// Create the provider.
    pub fn new() -> Self {
        S3Provider
    }
}

impl Provider for S3Provider {
    fn id(&self) -> &'static str {
        "s3"
    }

    fn engines(&self) -> Vec<Box<dyn Engine>> {
        vec![Box::new(S3fsEngine::new())]
    }

    fn validate(&self, profile: &Profile) -> Result<()> {
        if profile.provider != ProviderKind::S3 {
            return Err(AppError::Unsupported(format!(
                "profile uses provider `{}`, expected `s3`",
                profile.provider
            )));
        }
        match profile.bucket.as_deref() {
            Some(bucket) if !bucket.is_empty() => {}
            _ => {
                return Err(AppError::Config(
                    "S3 profile requires a non-empty `bucket`".to_string(),
                ))
            }
        }

        // Non-AWS endpoints require sigv4, which requires a region.
        if let Some(endpoint) = &profile.endpoint {
            let is_aws = endpoint.contains("amazonaws.com");
            let region_missing = profile.region.as_deref().map(str::is_empty).unwrap_or(true);
            if !is_aws && region_missing {
                return Err(AppError::Config(format!(
                    "S3 endpoint `{endpoint}` requires a non-empty `region` for sigv4"
                )));
            }
        }

        Ok(())
    }

    fn select_engine(&self, profile: &Profile) -> Result<Box<dyn Engine>> {
        let engines = self.engines();
        match profile.engine.as_deref() {
            Some(id) => engines
                .into_iter()
                .find(|engine| engine.id() == id)
                .ok_or_else(|| {
                    AppError::NotFound(format!("engine `{id}` is not available for provider s3"))
                }),
            None => engines
                .into_iter()
                .next()
                .ok_or_else(|| AppError::NotFound("provider s3 has no engines".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MountOptions;
    use std::path::PathBuf;

    fn profile(engine: Option<&str>, bucket: Option<&str>) -> Profile {
        Profile {
            provider: ProviderKind::S3,
            engine: engine.map(str::to_string),
            mountpoint: PathBuf::from("/mnt/bucket"),
            allow_other: false,
            read_only: false,
            credentials_ref: None,
            options: MountOptions::default(),
            bucket: bucket.map(str::to_string),
            endpoint: None,
            region: None,
            path_style: None,
            drive_id: None,
        }
    }

    #[test]
    fn validates_complete_profile() {
        let provider = S3Provider::new();
        assert!(provider.validate(&profile(None, Some("mybucket"))).is_ok());
    }

    #[test]
    fn rejects_missing_bucket() {
        let provider = S3Provider::new();
        assert!(provider.validate(&profile(None, None)).is_err());
        assert!(provider.validate(&profile(None, Some(""))).is_err());
    }

    #[test]
    fn rejects_wrong_provider() {
        let provider = S3Provider::new();
        let mut p = profile(None, Some("mybucket"));
        p.provider = ProviderKind::Aliyun;
        assert!(provider.validate(&p).is_err());
    }

    #[test]
    fn non_aws_endpoint_requires_region() {
        let provider = S3Provider::new();
        let mut p = profile(None, Some("mybucket"));
        p.endpoint = Some("https://minio.example.com".to_string());
        p.region = None;
        assert!(provider.validate(&p).is_err());

        p.region = Some(String::new());
        assert!(provider.validate(&p).is_err());

        p.region = Some("us-east-1".to_string());
        assert!(provider.validate(&p).is_ok());
    }

    #[test]
    fn aws_endpoint_does_not_require_region() {
        let provider = S3Provider::new();
        let mut p = profile(None, Some("mybucket"));
        p.endpoint = Some("https://s3.amazonaws.com".to_string());
        p.region = None;
        assert!(provider.validate(&p).is_ok());
    }

    #[test]
    fn selects_default_engine() {
        let provider = S3Provider::new();
        let engine = provider
            .select_engine(&profile(None, Some("b")))
            .expect("engine");
        assert_eq!(engine.id(), "s3fs-fuse");
    }

    #[test]
    fn rejects_unknown_engine() {
        let provider = S3Provider::new();
        assert!(provider
            .select_engine(&profile(Some("qrypt"), Some("b")))
            .is_err());
    }
}
