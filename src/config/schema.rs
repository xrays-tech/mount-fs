//! Configuration schema and persistence.
//!
//! Configuration is TOML stored under `$XDG_CONFIG_HOME/mount-fs/config.toml`
//! (or the platform equivalent). Paths beginning with `~/` are expanded when
//! the configuration is loaded.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use etcetera::{choose_base_strategy, BaseStrategy};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// Directory name used for configuration files.
pub const CONFIG_DIR_NAME: &str = "mount-fs";

/// File name used for the configuration file.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Starter configuration written by `config init`.
pub const CONFIG_TEMPLATE: &str = r#"# mount-fs configuration
#
# Profiles are declared under [profiles.<name>]. Paths may start with "~/".
#
# S3 example:
# [profiles.bucket]
# provider = "s3"
# engine = "s3fs-fuse"
# mountpoint = "~/mnt/bucket"
# bucket = "mybucket"
# endpoint = "https://minio.example.com"
# region = "us-east-1"
# path_style = true
# read_only = false
# allow_other = false
# credentials_ref = "~/.config/mount-fs/credentials/bucket.s3fs"
# options = { uid = 1000, gid = 1000, umask = "077" }

[general]
mount_root = "~/mnt"
allow_other = false
engine_auto_fallback = true
# auto_install = false
# log_level = "info"

# Aliyun example (qrypt requires a drive id and an OpenAPI refresh token):
# [profiles.pan]
# provider = "aliyun"
# engine = "qrypt"
# mountpoint = "~/mnt/pan"
# drive_id = "your-drive-id"
# drive_type = "resource"   # aliyundrive-webdav only: resource | backup | default
# credentials_ref = "~/.config/mount-fs/credentials/pan.token"

[profiles]
"#;

/// Root configuration document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Global settings.
    #[serde(default)]
    pub general: General,
    /// Named mount profiles.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// Global settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Root directory under which profile mountpoints are created.
    pub mount_root: PathBuf,
    /// Whether `allow_other` is permitted by default.
    pub allow_other: bool,
    /// Whether to fall back to an alternative engine automatically.
    pub engine_auto_fallback: bool,
    /// Whether `mount` may install missing engines/dependencies automatically.
    pub auto_install: bool,
    /// Optional log level override.
    pub log_level: Option<String>,
}

impl Default for General {
    fn default() -> Self {
        General {
            mount_root: PathBuf::from("~/mnt"),
            allow_other: false,
            engine_auto_fallback: true,
            auto_install: false,
            log_level: None,
        }
    }
}

/// Cloud provider backing a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// S3-compatible object storage.
    S3,
    /// Aliyun Drive.
    Aliyun,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProviderKind::S3 => "s3",
            ProviderKind::Aliyun => "aliyun",
        })
    }
}

/// A named mount profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Provider backing this profile.
    pub provider: ProviderKind,
    /// Preferred engine identifier.
    #[serde(default)]
    pub engine: Option<String>,
    /// Local mountpoint.
    pub mountpoint: PathBuf,
    /// Whether multi-user access is requested.
    #[serde(default)]
    pub allow_other: bool,
    /// Whether the mount should be read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Path to a credentials file, if any.
    #[serde(default)]
    pub credentials_ref: Option<PathBuf>,
    /// Extra mount options.
    #[serde(default)]
    pub options: MountOptions,
    /// S3 bucket name.
    #[serde(default)]
    pub bucket: Option<String>,
    /// S3 endpoint URL.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// S3 region.
    #[serde(default)]
    pub region: Option<String>,
    /// Whether to use path-style requests.
    #[serde(default)]
    pub path_style: Option<bool>,
    /// Aliyun OpenAPI drive id (required by `qrypt`).
    #[serde(default)]
    pub drive_id: Option<String>,
    /// Aliyun drive type for `aliyundrive-webdav` (`resource`, `backup`, `default`).
    #[serde(default)]
    pub drive_type: Option<String>,
    /// Aliyun OpenAPI client id (optional; webdav).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Aliyun OpenAPI client secret (optional; redacted).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Aliyun root path (optional; webdav/qrypt).
    #[serde(default)]
    pub root_path: Option<String>,
}

#[cfg(test)]
impl Profile {
    /// Minimal profile for engine unit tests.
    pub fn default_for_test() -> Self {
        Profile {
            provider: ProviderKind::S3,
            engine: None,
            mountpoint: PathBuf::from("/mnt/test"),
            allow_other: false,
            read_only: false,
            credentials_ref: None,
            options: MountOptions::default(),
            bucket: None,
            endpoint: None,
            region: None,
            path_style: None,
            drive_id: None,
            drive_type: None,
            client_id: None,
            client_secret: None,
            root_path: None,
        }
    }
}

/// Optional mount options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MountOptions {
    /// File owner uid.
    pub uid: Option<u32>,
    /// File owner gid.
    pub gid: Option<u32>,
    /// Umask string (e.g. `"077"`).
    pub umask: Option<String>,
    /// Local cache directory.
    pub cache: Option<PathBuf>,
}

impl Config {
    /// Load configuration from `path`. A missing file yields the default.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|err| AppError::Config(format!("cannot read {}: {err}", path.display())))?;
        let mut config = Config::parse(&contents)?;
        config.expand_paths();
        Ok(config)
    }

    /// Load the default configuration file.
    pub fn load_default() -> Result<Self> {
        Config::load(&default_config_path()?)
    }

    /// Parse configuration from a TOML string.
    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).map_err(|err| AppError::Config(err.to_string()))
    }

    /// Write the configuration atomically (temp file + rename).
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    AppError::Config(format!(
                        "cannot create config directory {}: {err}",
                        parent.display()
                    ))
                })?;
            }
        }
        let text = toml::to_string_pretty(self)
            .map_err(|err| AppError::Config(format!("cannot serialize config: {err}")))?;

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(CONFIG_FILE_NAME);
        let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));

        std::fs::write(&temp, text)?;
        if let Err(err) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(err.into());
        }
        Ok(())
    }

    /// Look up a profile by name.
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Expand `~/` prefixes in profile paths.
    fn expand_paths(&mut self) {
        for profile in self.profiles.values_mut() {
            profile.mountpoint = expand_tilde(&profile.mountpoint);
            if let Some(cache) = &profile.options.cache {
                profile.options.cache = Some(expand_tilde(cache));
            }
            if let Some(creds) = &profile.credentials_ref {
                profile.credentials_ref = Some(expand_tilde(creds));
            }
        }
    }
}

/// Resolve the default configuration file path for this platform.
pub fn default_config_path() -> Result<PathBuf> {
    let strategy = choose_base_strategy()
        .map_err(|err| AppError::Config(format!("cannot determine config directory: {err}")))?;
    Ok(strategy
        .config_dir()
        .join(CONFIG_DIR_NAME)
        .join(CONFIG_FILE_NAME))
}

/// Allowed Aliyun drive types (for `aliyundrive-webdav`).
pub const DRIVE_TYPES: &[&str] = &["resource", "backup", "default"];

/// Validate and normalize a drive type (case-insensitive, lowercase output).
pub fn normalize_drive_type(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if DRIVE_TYPES.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(AppError::Config(format!(
            "invalid drive_type `{value}`; expected one of: {}",
            DRIVE_TYPES.join(", ")
        )))
    }
}

/// Expand a leading `~` using the current user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match etcetera::home_dir() {
        Ok(home) => expand_tilde_with_home(path, &home),
        Err(_) => path.to_path_buf(),
    }
}

/// Expand a leading `~` using an explicit home directory (testable).
pub fn expand_tilde_with_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_S3: &str = r#"
[general]
mount_root = "~/mnt"
allow_other = true
engine_auto_fallback = false
log_level = "debug"

[profiles.bucket]
provider = "s3"
engine = "s3fs-fuse"
mountpoint = "~/mnt/bucket"
allow_other = true
read_only = true
bucket = "mybucket"
endpoint = "https://minio.example.com"
region = "us-east-1"
path_style = true
credentials_ref = "~/.config/mount-fs/credentials/bucket.s3fs"
options = { uid = 1000, gid = 1000, umask = "077", cache = "~/.cache/mount-fs/bucket" }
"#;

    #[test]
    fn parses_full_s3_profile() {
        let config = Config::parse(FULL_S3).expect("valid config");
        assert!(config.general.allow_other);
        assert!(!config.general.engine_auto_fallback);
        assert_eq!(config.general.log_level.as_deref(), Some("debug"));

        let profile = config.profile("bucket").expect("bucket profile");
        assert_eq!(profile.provider, ProviderKind::S3);
        assert_eq!(profile.engine.as_deref(), Some("s3fs-fuse"));
        assert_eq!(profile.bucket.as_deref(), Some("mybucket"));
        assert_eq!(
            profile.endpoint.as_deref(),
            Some("https://minio.example.com")
        );
        assert_eq!(profile.region.as_deref(), Some("us-east-1"));
        assert_eq!(profile.path_style, Some(true));
        assert!(profile.read_only);
        assert!(profile.allow_other);
        assert_eq!(profile.options.uid, Some(1000));
        assert_eq!(profile.options.gid, Some(1000));
        assert_eq!(profile.options.umask.as_deref(), Some("077"));
    }

    #[test]
    fn empty_config_uses_defaults() {
        let config = Config::parse("").expect("empty config");
        assert!(config.profiles.is_empty());
        assert_eq!(config.general.mount_root, PathBuf::from("~/mnt"));
        assert!(!config.general.allow_other);
        assert!(config.general.engine_auto_fallback);
    }

    #[test]
    fn expands_tilde_with_home() {
        let home = Path::new("/home/alice");
        assert_eq!(
            expand_tilde_with_home(Path::new("~/mnt/bucket"), home),
            PathBuf::from("/home/alice/mnt/bucket")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("/srv/mnt"), home),
            PathBuf::from("/srv/mnt")
        );
    }

    #[test]
    fn load_expands_tilde_in_profiles() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, FULL_S3).expect("write config");
        let config = Config::load(&path).expect("load config");
        let profile = config.profile("bucket").expect("bucket profile");
        assert_eq!(profile.mountpoint, expand_tilde(Path::new("~/mnt/bucket")));
        assert_eq!(
            profile.options.cache.as_deref(),
            Some(expand_tilde(Path::new("~/.cache/mount-fs/bucket")).as_path())
        );
    }

    #[test]
    fn parses_aliyun_drive_type() {
        let config = Config::parse(
            "[profiles.pan]\nprovider = \"aliyun\"\nmountpoint = \"/mnt/pan\"\ndrive_type = \"resource\"\n",
        )
        .expect("config");
        let profile = config.profile("pan").expect("profile");
        assert_eq!(profile.drive_type.as_deref(), Some("resource"));
    }

    #[test]
    fn parses_auto_install() {
        assert!(!Config::parse("").expect("empty").general.auto_install);
        let config = Config::parse("[general]\nauto_install = true\n").expect("config");
        assert!(config.general.auto_install);
    }

    #[test]
    fn normalizes_drive_type() {
        assert_eq!(
            normalize_drive_type(" RESOURCE ").expect("valid"),
            "resource"
        );
        assert_eq!(normalize_drive_type("Backup").expect("valid"), "backup");
        assert_eq!(normalize_drive_type("default").expect("valid"), "default");
        assert!(normalize_drive_type("bogus").is_err());
    }

    #[test]
    fn rejects_invalid_provider_enum() {
        let contents = "[profiles.x]\nprovider = \"ftp\"\nmountpoint = \"/mnt/x\"\n";
        assert!(Config::parse(contents).is_err());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("nested").join("config.toml");
        let mut config = Config::default();
        config.general.mount_root = PathBuf::from("/srv/mnt");
        config.save_atomic(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.general.mount_root, PathBuf::from("/srv/mnt"));
    }

    #[test]
    fn missing_file_loads_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = Config::load(&temp.path().join("absent.toml")).expect("default");
        assert!(config.profiles.is_empty());
    }
}
