//! Cloud storage provider integrations.
//!
//! A [`Provider`] describes where remote data lives and which engines can
//! mount it. The S3 and Aliyun Drive providers are implemented; additional
//! providers are added by implementing [`Provider`] and registering them in
//! [`registry`].

pub mod aliyun;
pub mod s3;

use crate::config::Profile;
use crate::engines::Engine;
use crate::error::Result;

pub use self::aliyun::AliyunProvider;
pub use self::s3::S3Provider;

/// A cloud storage provider.
pub trait Provider: Send + Sync {
    /// Stable provider identifier (`s3`, `aliyun`).
    fn id(&self) -> &'static str;

    /// Engines that can mount this provider, in preference order.
    fn engines(&self) -> Vec<Box<dyn Engine>>;

    /// Validate that a profile is complete for this provider.
    fn validate(&self, profile: &Profile) -> Result<()>;

    /// Select the engine for a profile.
    fn select_engine(&self, profile: &Profile) -> Result<Box<dyn Engine>>;
}

/// All known providers.
pub fn registry() -> Vec<Box<dyn Provider>> {
    vec![Box::new(S3Provider::new()), Box::new(AliyunProvider::new())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_s3_and_aliyun() {
        let providers = registry();
        assert!(providers.iter().any(|provider| provider.id() == "s3"));
        assert!(providers.iter().any(|provider| provider.id() == "aliyun"));
    }
}
