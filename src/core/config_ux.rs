//! Configuration and credential UX helpers.
//!
//! All functions operate on an explicit config path (plus the XDG credentials
//! directory) so they can be exercised hermetically in tests.

use std::path::{Path, PathBuf};

use crate::config::{expand_tilde, Config, MountOptions, Profile, ProviderKind};
use crate::core::creds;
use crate::error::{AppError, Result};
use crate::providers;

/// Input for `config profile add`.
#[derive(Debug, Clone)]
pub struct NewProfile {
    /// Provider.
    pub provider: ProviderKind,
    /// Preferred engine.
    pub engine: Option<String>,
    /// Mountpoint.
    pub mountpoint: PathBuf,
    /// S3 bucket.
    pub bucket: Option<String>,
    /// S3 endpoint.
    pub endpoint: Option<String>,
    /// S3 region.
    pub region: Option<String>,
    /// S3 path-style requests.
    pub path_style: bool,
    /// Aliyun drive id.
    pub drive_id: Option<String>,
    /// `allow_other`.
    pub allow_other: bool,
    /// Read-only mount.
    pub read_only: bool,
}

/// Whether a config key name is secret-bearing.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "key"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// Redact a value when its key is secret-bearing.
pub fn redact(key: &str, value: &str) -> String {
    if is_secret_key(key) {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

/// Add a profile. Refuses to overwrite unless `force` is set.
pub fn add_profile(config_path: &Path, name: &str, new: NewProfile, force: bool) -> Result<()> {
    let mut config = Config::load(config_path)?;
    if config.profiles.contains_key(name) && !force {
        return Err(AppError::Config(format!(
            "profile `{name}` already exists; use --force to overwrite"
        )));
    }

    let profile = build_profile(&new)?;
    config.profiles.insert(name.to_string(), profile);
    config.save_atomic(config_path)?;
    println!("Added profile `{name}`");
    Ok(())
}

fn build_profile(new: &NewProfile) -> Result<Profile> {
    match new.provider {
        ProviderKind::S3 => {
            if new.bucket.as_deref().unwrap_or("").is_empty() {
                return Err(AppError::Config("S3 profiles require --bucket".to_string()));
            }
            if let Some(endpoint) = &new.endpoint {
                let is_aws = endpoint.contains("amazonaws.com");
                if !is_aws && new.region.as_deref().unwrap_or("").is_empty() {
                    return Err(AppError::Config(format!(
                        "S3 endpoint `{endpoint}` requires --region for sigv4"
                    )));
                }
            }
        }
        ProviderKind::Aliyun => {
            if new.engine.as_deref() == Some("qrypt")
                && new.drive_id.as_deref().unwrap_or("").is_empty()
            {
                return Err(AppError::Config(
                    "qrypt profiles require --drive-id".to_string(),
                ));
            }
        }
    }

    Ok(Profile {
        provider: new.provider,
        engine: new.engine.clone(),
        mountpoint: expand_tilde(&new.mountpoint),
        allow_other: new.allow_other,
        read_only: new.read_only,
        credentials_ref: None,
        options: MountOptions::default(),
        bucket: new.bucket.clone(),
        endpoint: new.endpoint.clone(),
        region: new.region.clone(),
        path_style: if new.provider == ProviderKind::S3 {
            Some(new.path_style)
        } else {
            None
        },
        drive_id: new.drive_id.clone(),
        client_id: None,
        client_secret: None,
        root_path: None,
    })
}

/// Remove a profile (does not remove its credential file).
pub fn remove_profile(config_path: &Path, name: &str) -> Result<()> {
    let mut config = Config::load(config_path)?;
    if config.profiles.remove(name).is_none() {
        return Err(AppError::NotFound(format!("profile `{name}`")));
    }
    config.save_atomic(config_path)?;
    println!("Removed profile `{name}`");
    Ok(())
}

/// Set non-secret profile fields from `key=value` assignments.
pub fn set_profile_fields(config_path: &Path, name: &str, assignments: &[String]) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let profile = config
        .profiles
        .get_mut(name)
        .ok_or_else(|| AppError::NotFound(format!("profile `{name}`")))?;

    let mut index = 0;
    while index < assignments.len() {
        let token = &assignments[index];
        let (key, value, consumed) = match token.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim(), 1),
            None => {
                let value = assignments
                    .get(index + 1)
                    .ok_or_else(|| AppError::Config(format!("`{token}` expects a value")))?;
                (token.trim(), value.trim(), 2)
            }
        };
        index += consumed;

        if is_secret_key(key) {
            return Err(AppError::Config(format!(
                "refusing to set secret-bearing key `{key}` from argv; use `config login`"
            )));
        }

        match key {
            "mountpoint" => profile.mountpoint = expand_tilde(Path::new(value)),
            "engine" => {
                profile.engine = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                }
            }
            "bucket" => profile.bucket = non_empty(value),
            "endpoint" => profile.endpoint = non_empty(value),
            "region" => profile.region = non_empty(value),
            "drive_id" => profile.drive_id = non_empty(value),
            "client_id" => profile.client_id = non_empty(value),
            "root_path" => profile.root_path = non_empty(value),
            "path_style" => profile.path_style = Some(parse_bool(value)?),
            "allow_other" => profile.allow_other = parse_bool(value)?,
            "read_only" => profile.read_only = parse_bool(value)?,
            other => {
                return Err(AppError::NotFound(format!(
                    "profile key `{other}` is not settable"
                )))
            }
        }
    }

    config.save_atomic(config_path)?;
    println!("Updated profile `{name}`");
    Ok(())
}

/// Read a config value, redacting secret-bearing keys.
pub fn get_value(config: &Config, key: &str) -> Result<String> {
    let value = raw_value(config, key)?;
    Ok(redact(key, &value))
}

fn raw_value(config: &Config, key: &str) -> Result<String> {
    if let Some(field) = key.strip_prefix("general.") {
        return match field {
            "mount_root" => Ok(config.general.mount_root.display().to_string()),
            "allow_other" => Ok(config.general.allow_other.to_string()),
            "engine_auto_fallback" => Ok(config.general.engine_auto_fallback.to_string()),
            "log_level" => Ok(config.general.log_level.clone().unwrap_or_default()),
            other => Err(AppError::NotFound(format!(
                "unknown config key `general.{other}`"
            ))),
        };
    }

    if let Some(rest) = key.strip_prefix("profiles.") {
        let (name, field) = rest
            .split_once('.')
            .ok_or_else(|| AppError::NotFound(format!("unknown config key `{key}`")))?;
        let profile = config
            .profile(name)
            .ok_or_else(|| AppError::NotFound(format!("profile `{name}`")))?;
        return profile_value(profile, field);
    }

    Err(AppError::NotFound(format!("unknown config key `{key}`")))
}

fn profile_value(profile: &Profile, field: &str) -> Result<String> {
    let value = match field {
        "provider" => profile.provider.to_string(),
        "engine" => profile.engine.clone().unwrap_or_default(),
        "mountpoint" => profile.mountpoint.display().to_string(),
        "allow_other" => profile.allow_other.to_string(),
        "read_only" => profile.read_only.to_string(),
        "credentials_ref" => profile
            .credentials_ref
            .as_deref()
            .map(|path| format!("file:{}", path.display()))
            .unwrap_or_default(),
        "bucket" => profile.bucket.clone().unwrap_or_default(),
        "endpoint" => profile.endpoint.clone().unwrap_or_default(),
        "region" => profile.region.clone().unwrap_or_default(),
        "path_style" => profile
            .path_style
            .map(|value| value.to_string())
            .unwrap_or_default(),
        "drive_id" => profile.drive_id.clone().unwrap_or_default(),
        "client_id" => profile.client_id.clone().unwrap_or_default(),
        "client_secret" => profile.client_secret.clone().unwrap_or_default(),
        "root_path" => profile.root_path.clone().unwrap_or_default(),
        "options.uid" => profile
            .options
            .uid
            .map(|value| value.to_string())
            .unwrap_or_default(),
        "options.gid" => profile
            .options
            .gid
            .map(|value| value.to_string())
            .unwrap_or_default(),
        "options.umask" => profile.options.umask.clone().unwrap_or_default(),
        "options.cache" => profile
            .options
            .cache
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        other => return Err(AppError::NotFound(format!("unknown profile key `{other}`"))),
    };
    Ok(value)
}

/// Render the effective configuration with secret values redacted.
pub fn render_config(config: &Config, name: Option<&str>) -> Result<String> {
    let mut out = String::new();
    out.push_str("[general]\n");
    out.push_str(&format!(
        "mount_root = {}\n",
        config.general.mount_root.display()
    ));
    out.push_str(&format!("allow_other = {}\n", config.general.allow_other));
    out.push_str(&format!(
        "engine_auto_fallback = {}\n",
        config.general.engine_auto_fallback
    ));
    out.push_str(&format!(
        "log_level = {}\n",
        config.general.log_level.as_deref().unwrap_or("")
    ));

    let profiles: Vec<(&String, &Profile)> = match name {
        Some(name) => {
            let profile = config
                .profile(name)
                .ok_or_else(|| AppError::NotFound(format!("profile `{name}`")))?;
            vec![(
                config.profiles.get_key_value(name).expect("present").0,
                profile,
            )]
        }
        None => config.profiles.iter().collect(),
    };

    for (profile_name, profile) in profiles {
        out.push_str(&format!("\n[profiles.{profile_name}]\n"));
        let fields = [
            ("provider", profile.provider.to_string()),
            ("engine", profile.engine.clone().unwrap_or_default()),
            ("mountpoint", profile.mountpoint.display().to_string()),
            ("allow_other", profile.allow_other.to_string()),
            ("read_only", profile.read_only.to_string()),
            (
                "credentials_ref",
                profile
                    .credentials_ref
                    .as_deref()
                    .map(|path| format!("file:{}", path.display()))
                    .unwrap_or_default(),
            ),
            ("bucket", profile.bucket.clone().unwrap_or_default()),
            ("endpoint", profile.endpoint.clone().unwrap_or_default()),
            ("region", profile.region.clone().unwrap_or_default()),
            (
                "path_style",
                profile
                    .path_style
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            ("drive_id", profile.drive_id.clone().unwrap_or_default()),
            ("client_id", profile.client_id.clone().unwrap_or_default()),
            (
                "client_secret",
                profile.client_secret.clone().unwrap_or_default(),
            ),
            ("root_path", profile.root_path.clone().unwrap_or_default()),
            (
                "options.uid",
                profile
                    .options
                    .uid
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            (
                "options.gid",
                profile
                    .options
                    .gid
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            (
                "options.umask",
                profile.options.umask.clone().unwrap_or_default(),
            ),
            (
                "options.cache",
                profile
                    .options
                    .cache
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
        ];

        for (key, value) in fields {
            out.push_str(&format!("{key} = {}\n", redact(key, &value)));
        }
    }

    Ok(out)
}

/// Validate one profile, or all profiles, returning `(name, error)` rows.
pub fn validate_report(
    config_path: &Path,
    name: Option<&str>,
) -> Result<Vec<(String, Option<String>)>> {
    let config = Config::load(config_path)?;
    let selected: Vec<(&String, &Profile)> = match name {
        Some(name) => {
            let profile = config
                .profile(name)
                .ok_or_else(|| AppError::NotFound(format!("profile `{name}`")))?;
            vec![(
                config.profiles.get_key_value(name).expect("present").0,
                profile,
            )]
        }
        None => config.profiles.iter().collect(),
    };

    let providers = providers::registry();
    let mut report = Vec::new();
    for (profile_name, profile) in selected {
        let provider = providers
            .iter()
            .find(|provider| provider.id() == profile.provider.to_string());
        let error = match provider {
            Some(provider) => provider.validate(profile).err().map(|err| err.to_string()),
            None => Some(format!(
                "no provider implementation for `{}`",
                profile.provider
            )),
        };
        report.push((profile_name.clone(), error));
    }
    Ok(report)
}

/// Read/write the S3 credential file path for a profile.
pub fn s3_credentials_path(profile_name: &str) -> Result<PathBuf> {
    Ok(creds::credentials_dir()?.join(format!("{}.s3fs", creds::sanitize_profile(profile_name))))
}

/// Store S3 credentials (0600) and point the profile at the file.
pub fn login_s3(
    config_path: &Path,
    profile_name: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<PathBuf> {
    let mut config = Config::load(config_path)?;
    let profile = config
        .profiles
        .get_mut(profile_name)
        .ok_or_else(|| AppError::NotFound(format!("profile `{profile_name}`")))?;
    if profile.provider != ProviderKind::S3 {
        return Err(AppError::Config(format!(
            "profile `{profile_name}` is not an S3 profile"
        )));
    }

    let path = s3_credentials_path(profile_name)?;
    creds::write_private_file(&path, format!("{access_key}:{secret_key}\n").as_bytes())?;
    profile.credentials_ref = Some(path.clone());
    config.save_atomic(config_path)?;
    Ok(path)
}

/// Remove a profile's stored credential file and clear `credentials_ref`.
pub fn logout(config_path: &Path, provider: &str, profile_name: &str) -> Result<Vec<PathBuf>> {
    let mut config = Config::load(config_path)?;
    let profile = config
        .profiles
        .get_mut(profile_name)
        .ok_or_else(|| AppError::NotFound(format!("profile `{profile_name}`")))?;

    if profile.provider.to_string() != provider {
        return Err(AppError::Config(format!(
            "profile `{profile_name}` is not a `{provider}` profile"
        )));
    }

    let mut removed = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = &profile.credentials_ref {
        candidates.push(path.clone());
    }
    match profile.provider {
        ProviderKind::S3 => candidates.push(s3_credentials_path(profile_name)?),
        ProviderKind::Aliyun => {
            if let Ok(path) = creds::default_aliyun_token_path(profile_name) {
                candidates.push(path);
            }
        }
    }

    for path in candidates {
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    profile.credentials_ref = None;
    config.save_atomic(config_path)?;
    Ok(removed)
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(AppError::Config(format!(
            "expected a boolean value, got `{value}`"
        ))),
    }
}
