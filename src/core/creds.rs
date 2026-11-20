//! Credential resolution and temporary credential files.
//!
//! Credentials are never passed on a command line. For `s3fs-fuse` they are
//! materialized into a `0600` `passwd_file` inside the runtime directory.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dialoguer::{Input, Password};
use etcetera::{choose_base_strategy, BaseStrategy};

use crate::config::Profile;
use crate::error::{AppError, Result};

/// Credential files younger than this are never garbage-collected (a concurrent
/// mount may not have registered them in state yet).
const GC_GRACE: Duration = Duration::from_secs(30);

/// Runtime directories for transient state.
#[derive(Debug, Clone)]
pub struct RuntimeDirs {
    /// Directory for runtime files (credentials, locks, ...).
    pub runtime: PathBuf,
}

impl RuntimeDirs {
    /// Resolve and create the runtime directory.
    ///
    /// Uses `$XDG_RUNTIME_DIR/mount-fs`; falls back to `/tmp/mount-fs-<uid>`
    /// (with a warning) when `XDG_RUNTIME_DIR` is unset. The directory is
    /// always created with mode `0700`.
    pub fn resolve() -> Result<Self> {
        let (dir, fallback) = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(value) if !value.is_empty() => (PathBuf::from(value).join("mount-fs"), false),
            _ => {
                let uid = rustix::process::getuid().as_raw();
                (PathBuf::from(format!("/tmp/mount-fs-{uid}")), true)
            }
        };

        if fallback {
            tracing::warn!(
                path = %dir.display(),
                "XDG_RUNTIME_DIR is not set; using fallback runtime directory"
            );
            create_fallback_dir(&dir)?;
        } else {
            create_private_dir(&dir)?;
        }
        Ok(RuntimeDirs { runtime: dir })
    }
}

/// A temporary credential file removed on drop.
#[derive(Debug)]
pub struct TempCredential {
    /// Path to the credential file.
    pub path: PathBuf,
}

impl TempCredential {
    /// The credential file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the credential file past this handle's lifetime.
    ///
    /// Use this once a mount is confirmed: the file must stay on disk while the
    /// engine is running and is removed explicitly on unmount/rollback instead
    /// of via [`Drop`].
    pub fn persist(self) -> PathBuf {
        let this = std::mem::ManuallyDrop::new(self);
        this.path.clone()
    }
}

impl Drop for TempCredential {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write an s3fs `passwd_file` (`ACCESS:SECRET\n`) with mode `0600`.
pub fn materialize_s3(
    profile_name: &str,
    dirs: &RuntimeDirs,
    access_key: &str,
    secret_key: &str,
) -> Result<TempCredential> {
    create_private_dir(&dirs.runtime)?;

    // Include both a sanitized name and a short stable hash of the original
    // name, so `a-b` and `a_b` cannot alias after sanitization.
    let key = cred_key(profile_name);
    let path = dirs
        .runtime
        .join(format!("creds-{key}-{}.s3fs", std::process::id()));

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| AppError::Credential(format!("cannot create {}: {err}", path.display())))?;
    file.write_all(format!("{access_key}:{secret_key}\n").as_bytes())?;
    file.flush()?;

    // Ensure the mode even if the file pre-existed with broader permissions.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode != 0o600 {
        let _ = std::fs::remove_file(&path);
        return Err(AppError::Credential(format!(
            "credential file {} has mode {mode:o}, expected 600",
            path.display()
        )));
    }

    Ok(TempCredential { path })
}

/// Write `bytes` to `path` with mode 0600.
///
/// Parent directories are created if needed but their existing permissions are
/// not changed (callers that need a private directory use
/// [`create_private_dir`]).
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| AppError::Credential(format!("cannot create {}: {err}", path.display())))?;
    file.write_all(bytes)?;
    file.flush()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode != 0o600 {
        let _ = std::fs::remove_file(path);
        return Err(AppError::Credential(format!(
            "credential file {} has mode {mode:o}, expected 600",
            path.display()
        )));
    }
    Ok(())
}

/// Default S3 credential file path for a profile.
pub fn default_s3_credentials_path(profile_name: &str) -> Result<PathBuf> {
    Ok(credentials_dir()?.join(format!("{}.s3fs", sanitize_profile(profile_name))))
}

/// Read `ACCESS:SECRET` from a 0600 credential file.
pub fn s3_credentials_from_file(path: &Path) -> Result<(String, String)> {
    ensure_private_file(path)?;
    let contents = std::fs::read_to_string(path)?;
    let line = contents.trim();
    let (access, secret) = line.split_once(':').ok_or_else(|| {
        AppError::Credential(format!(
            "credential file {} must contain `ACCESS:SECRET`",
            path.display()
        ))
    })?;
    if access.is_empty() || secret.is_empty() {
        return Err(AppError::Credential(format!(
            "credential file {} has an empty access key or secret",
            path.display()
        )));
    }
    Ok((access.to_string(), secret.to_string()))
}

/// Resolve S3 credentials from `credentials_ref`, the environment, or an
/// interactive prompt (in that order).
pub fn resolve_s3_credentials(profile: &Profile) -> Result<(String, String)> {
    if let Some(path) = &profile.credentials_ref {
        return s3_credentials_from_file(path);
    }

    let access = non_empty_env("AWS_ACCESS_KEY_ID");
    let secret = non_empty_env("AWS_SECRET_ACCESS_KEY");
    if let (Some(access), Some(secret)) = (access, secret) {
        return Ok((access, secret));
    }

    if !std::io::stdin().is_terminal() {
        return Err(AppError::Config(
            "AWS credentials not found; set AWS_ACCESS_KEY_ID and \
             AWS_SECRET_ACCESS_KEY or run interactively"
                .to_string(),
        ));
    }

    let label = profile
        .bucket
        .as_deref()
        .map(|bucket| format!("profile `{bucket}`"))
        .unwrap_or_else(|| "S3 profile".to_string());

    let access: String = Input::<String>::new()
        .with_prompt(format!("AWS access key id for {label}"))
        .interact_text()
        .map_err(|err| AppError::Credential(err.to_string()))?;
    let secret: String = Password::new()
        .with_prompt("AWS secret access key")
        .interact()
        .map_err(|err| AppError::Credential(err.to_string()))?;

    if access.is_empty() || secret.is_empty() {
        return Err(AppError::Credential(
            "AWS access key id and secret access key must not be empty".to_string(),
        ));
    }
    Ok((access, secret))
}

/// Aliyun Drive credentials.
#[derive(Debug, Clone)]
pub struct AliyunCreds {
    /// Refresh token (legacy web or OpenAPI depending on the engine).
    pub refresh_token: String,
    /// Drive id (required by OpenAPI engines).
    pub drive_id: Option<String>,
}

/// Whether an Aliyun token is available without prompting.
pub fn has_aliyun_token(profile: &Profile) -> bool {
    if let Some(path) = &profile.credentials_ref {
        if path.exists() {
            return true;
        }
    }
    non_empty_env("ALIYUN_REFRESH_TOKEN").is_some() || non_empty_env("REFRESH_TOKEN").is_some()
}

/// Resolve Aliyun credentials from a credential file, env, or interactive
/// prompt (in that order).
///
/// `ALIYUN_REFRESH_TOKEN` is the primary environment variable;
/// `REFRESH_TOKEN` is accepted as a fallback for compatibility with the
/// upstream engines.
pub fn resolve_aliyun_credentials(profile: &Profile) -> Result<AliyunCreds> {
    let refresh_token = if let Some(path) = &profile.credentials_ref {
        read_token_file(path)?
    } else if let Some(token) =
        non_empty_env("ALIYUN_REFRESH_TOKEN").or_else(|| non_empty_env("REFRESH_TOKEN"))
    {
        token
    } else if std::io::stdin().is_terminal() {
        Password::new()
            .with_prompt("Aliyun refresh token")
            .interact()
            .map_err(|err| AppError::Credential(err.to_string()))?
    } else {
        return Err(AppError::Config(
            "Aliyun refresh token not found; set `credentials_ref`, \
             ALIYUN_REFRESH_TOKEN/REFRESH_TOKEN, or run interactively"
                .to_string(),
        ));
    };

    if refresh_token.trim().is_empty() {
        return Err(AppError::Credential(
            "Aliyun refresh token must not be empty".to_string(),
        ));
    }

    let drive_id = match profile.drive_id.clone().filter(|id| !id.is_empty()) {
        Some(id) => Some(id),
        None if std::io::stdin().is_terminal() => {
            let id: String = Input::<String>::new()
                .with_prompt("Aliyun drive id (blank to skip)")
                .allow_empty(true)
                .interact_text()
                .map_err(|err| AppError::Credential(err.to_string()))?;
            if id.is_empty() {
                None
            } else {
                Some(id)
            }
        }
        None => None,
    };

    Ok(AliyunCreds {
        refresh_token: refresh_token.trim().to_string(),
        drive_id,
    })
}

/// Directory for persistent credentials (`$XDG_CONFIG_HOME/mount-fs/credentials`).
pub fn credentials_dir() -> Result<PathBuf> {
    let strategy = choose_base_strategy()
        .map_err(|err| AppError::Config(format!("cannot determine config directory: {err}")))?;
    Ok(strategy.config_dir().join("mount-fs").join("credentials"))
}

/// Persistent token path for a profile.
pub fn default_aliyun_token_path(profile_name: &str) -> Result<PathBuf> {
    Ok(credentials_dir()?.join(format!(
        "{}-{}.token",
        sanitize_profile(profile_name),
        profile_nonce(profile_name)
    )))
}

/// Store an Aliyun token in a persistent 0600 file, returning its path.
pub fn store_aliyun_token(profile_name: &str, token: &str) -> Result<PathBuf> {
    if token.trim().is_empty() {
        return Err(AppError::Credential(
            "refusing to store an empty token".to_string(),
        ));
    }
    let dir = credentials_dir()?;
    create_private_dir(&dir)?;
    let path = default_aliyun_token_path(profile_name)?;
    write_private_file(&path, format!("{}\n", token.trim()).as_bytes())?;
    Ok(path)
}

fn ensure_private_file(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| AppError::Credential(format!("cannot read {}: {err}", path.display())))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AppError::Credential(format!(
            "credential file {} is group/world accessible (mode {mode:o}); expected 0600",
            path.display()
        )));
    }
    Ok(())
}

fn read_token_file(path: &Path) -> Result<String> {
    ensure_private_file(path)?;
    let contents = std::fs::read_to_string(path)?;
    Ok(contents.trim().to_string())
}

/// Remove stale credential artifacts.
///
/// This covers all engine families:
/// - `creds-<key>-<pid>.s3fs` (S3),
/// - `qrypt-<key>-<pid>.toml` (qrypt),
/// - `aliyundrive-fuse-<key>-<pid>/refresh_token` (aliyundrive-fuse workdir).
///
/// An artifact is kept if *any* of these holds:
/// - its profile is active in state,
/// - the pid encoded in its name is still alive,
/// - it is younger than [`GC_GRACE`] (a concurrent mount may not have
///   registered it yet).
pub fn gc(dirs: &RuntimeDirs, active_profiles: &[String]) -> Result<()> {
    let entries = match std::fs::read_dir(&dirs.runtime) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let active: Vec<String> = active_profiles
        .iter()
        .map(|profile| cred_key(profile))
        .collect();

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if let Some(rest) = name
            .strip_prefix("creds-")
            .and_then(|rest| rest.strip_suffix(".s3fs"))
        {
            if let Some((key, pid)) = rest.rsplit_once('-') {
                if !retain_artifact(key, pid, &active, &entry.path()) {
                    remove_artifact(&entry.path());
                }
            }
            continue;
        }

        if let Some(rest) = name
            .strip_prefix("qrypt-")
            .and_then(|rest| rest.strip_suffix(".toml"))
        {
            if let Some((key, pid)) = rest.rsplit_once('-') {
                if !retain_artifact(key, pid, &active, &entry.path()) {
                    remove_artifact(&entry.path());
                }
            }
            continue;
        }

        if let Some(rest) = name.strip_prefix("aliyundrive-fuse-") {
            if let Some((key, pid)) = rest.rsplit_once('-') {
                let entry_path = entry.path();
                let token = entry_path.join("refresh_token");
                let age_source = if token.exists() {
                    token.as_path()
                } else {
                    entry_path.as_path()
                };
                if !retain_artifact(key, pid, &active, age_source) {
                    let _ = std::fs::remove_file(&token);
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
    }
    Ok(())
}

fn retain_artifact(key: &str, pid: &str, active: &[String], age_path: &Path) -> bool {
    if active.iter().any(|active_key| active_key == key) {
        return true;
    }
    if pid
        .parse::<u32>()
        .map(|pid| crate::system::process::pid_starttime(pid).is_some())
        .unwrap_or(false)
    {
        return true;
    }
    is_young(age_path)
}

fn is_young(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .map(|age| age < GC_GRACE)
        .unwrap_or(false)
}

/// Remove a credential artifact, including an empty aliyundrive workdir.
pub fn remove_artifact(path: &Path) {
    let _ = std::fs::remove_file(path);
    if path.file_name() == Some(OsStr::new("refresh_token")) {
        if let Some(parent) = path.parent() {
            let is_workdir = parent
                .file_name()
                .map(|name| name.to_string_lossy().starts_with("aliyundrive-fuse-"))
                .unwrap_or(false);
            if is_workdir {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

/// Artifact directory name for an aliyundrive-fuse workdir.
pub fn aliyundrive_workdir_name(profile: &str) -> String {
    format!(
        "aliyundrive-fuse-{}-{}",
        cred_key(profile),
        std::process::id()
    )
}

/// Artifact file name for a qrypt config.
pub fn qrypt_config_name(profile: &str) -> String {
    format!("qrypt-{}-{}.toml", cred_key(profile), std::process::id())
}

/// Stable credential-file key for a profile (`sanitized-name-hash`).
pub fn cred_key(profile: &str) -> String {
    format!("{}-{}", sanitize_profile(profile), profile_nonce(profile))
}

/// Short stable hash of the original profile name.
pub fn profile_nonce(profile: &str) -> String {
    // FNV-1a, truncated to 32 bits / 8 hex chars.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in profile.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// Create a directory with mode 0700 (existing directories are re-secured).
pub fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|err| {
        AppError::Config(format!(
            "cannot create runtime directory {}: {err}",
            path.display()
        ))
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
        AppError::Config(format!(
            "cannot set permissions on runtime directory {}: {err}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Create/validate the `/tmp/mount-fs-<uid>` fallback directory.
///
/// Hardened against pre-existing symlinks and foreign ownership.
fn create_fallback_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AppError::Credential(format!(
                    "refusing to use symlink at fallback runtime path {}",
                    path.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(AppError::Credential(format!(
                    "fallback runtime path {} exists but is not a directory",
                    path.display()
                )));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path).map_err(|err| {
                AppError::Credential(format!(
                    "cannot create fallback runtime directory {}: {err}",
                    path.display()
                ))
            })?;
        }
        Err(err) => return Err(err.into()),
    }

    // Re-check after creation to avoid following a symlink swapped in.
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::Credential(format!(
            "refusing to use symlink at fallback runtime path {}",
            path.display()
        )));
    }
    let euid = rustix::process::geteuid().as_raw();
    if metadata.uid() != euid {
        return Err(AppError::Credential(format!(
            "fallback runtime directory {} is owned by uid {}, expected {}",
            path.display(),
            metadata.uid(),
            euid
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Sanitize a profile name for use in a filename.
pub fn sanitize_profile(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    const DEAD_PID: u32 = 4_000_000_000;

    fn dirs() -> (tempfile::TempDir, RuntimeDirs) {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = RuntimeDirs {
            runtime: temp.path().to_path_buf(),
        };
        (temp, dirs)
    }

    fn set_old(path: &Path) {
        let file = OpenOptions::new().write(true).open(path).expect("open");
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600)),
        )
        .expect("set mtime");
    }

    fn write_cred(dirs: &RuntimeDirs, profile: &str, pid: u32, old: bool) -> PathBuf {
        let path = dirs
            .runtime
            .join(format!("creds-{}-{pid}.s3fs", cred_key(profile)));
        std::fs::write(&path, "x:y\n").expect("write cred");
        if old {
            set_old(&path);
        }
        path
    }

    fn write_aliyun_artifact(dirs: &RuntimeDirs, profile: &str, pid: u32, old: bool) -> PathBuf {
        let dir = dirs
            .runtime
            .join(format!("aliyundrive-fuse-{}-{pid}", cred_key(profile)));
        std::fs::create_dir_all(&dir).expect("workdir");
        let token = dir.join("refresh_token");
        std::fs::write(&token, "token").expect("write token");
        if old {
            set_old(&token);
        }
        dir
    }

    fn write_qrypt_artifact(dirs: &RuntimeDirs, profile: &str, pid: u32, old: bool) -> PathBuf {
        let path = dirs
            .runtime
            .join(format!("qrypt-{}-{pid}.toml", cred_key(profile)));
        std::fs::write(&path, "config").expect("write qrypt config");
        if old {
            set_old(&path);
        }
        path
    }

    #[test]
    fn materialize_writes_0600_and_content() {
        let (_temp, dirs) = dirs();
        let cred = materialize_s3("bucket", &dirs, "AKIAEXAMPLE", "SECRET").expect("materialize");
        let contents = std::fs::read_to_string(cred.path()).expect("read");
        assert_eq!(contents, "AKIAEXAMPLE:SECRET\n");

        let mode = std::fs::metadata(cred.path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn drop_removes_credential_file() {
        let (_temp, dirs) = dirs();
        let cred = materialize_s3("bucket", &dirs, "a", "b").expect("materialize");
        let path = cred.path().to_path_buf();
        assert!(path.exists());
        drop(cred);
        assert!(!path.exists());
    }

    #[test]
    fn gc_keeps_active_profile_even_if_old_and_dead() {
        let (_temp, dirs) = dirs();
        let active = write_cred(&dirs, "live", DEAD_PID, true);

        gc(&dirs, &["live".to_string()]).expect("gc");
        assert!(active.exists());
    }

    #[test]
    fn gc_keeps_file_with_live_pid() {
        let (_temp, dirs) = dirs();
        let live = write_cred(&dirs, "orphan", std::process::id(), true);

        gc(&dirs, &[]).expect("gc");
        assert!(live.exists());
    }

    #[test]
    fn gc_keeps_young_file_with_dead_pid() {
        let (_temp, dirs) = dirs();
        let young = write_cred(&dirs, "young", DEAD_PID, false);

        gc(&dirs, &[]).expect("gc");
        assert!(young.exists());
    }

    #[test]
    fn gc_removes_old_file_with_dead_pid() {
        let (_temp, dirs) = dirs();
        let dead = write_cred(&dirs, "dead", DEAD_PID, true);

        gc(&dirs, &[]).expect("gc");
        assert!(!dead.exists());
    }

    #[test]
    fn gc_on_missing_dir_is_ok() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = RuntimeDirs {
            runtime: temp.path().join("does-not-exist"),
        };
        gc(&dirs, &[]).expect("gc");
    }

    #[test]
    fn sanitizes_profile_names() {
        assert_eq!(sanitize_profile("my/bucket name"), "my-bucket-name");
    }

    #[test]
    fn sanitized_name_collisions_do_not_alias() {
        // These two differ only after sanitization.
        assert_eq!(sanitize_profile("a/b"), sanitize_profile("a b"));
        assert_ne!(cred_key("a/b"), cred_key("a b"));

        let (_temp, dirs) = dirs();
        let first = materialize_s3("a/b", &dirs, "a", "b").expect("first");
        let second = materialize_s3("a b", &dirs, "a", "b").expect("second");
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn credentials_ref_requires_0600() {
        let temp = tempfile::tempdir().expect("tempdir");
        let token_path = temp.path().join("token");
        std::fs::write(&token_path, "TOKEN_VALUE\n").expect("write");
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");

        let mut profile = crate::config::Profile::default_for_test();
        profile.credentials_ref = Some(token_path.clone());

        let creds = resolve_aliyun_credentials(&profile).expect("0600 file");
        assert_eq!(creds.refresh_token, "TOKEN_VALUE");

        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(resolve_aliyun_credentials(&profile).is_err());
    }

    #[test]
    fn gc_sweeps_orphaned_aliyun_artifacts() {
        let (_temp, dirs) = dirs();
        let workdir = write_aliyun_artifact(&dirs, "orphan", DEAD_PID, true);
        let qrypt = write_qrypt_artifact(&dirs, "orphan", DEAD_PID, true);

        gc(&dirs, &[]).expect("gc");
        assert!(!workdir.exists(), "orphaned workdir should be swept");
        assert!(!qrypt.exists(), "orphaned qrypt config should be swept");
    }

    #[test]
    fn gc_keeps_active_and_young_aliyun_artifacts() {
        let (_temp, dirs) = dirs();
        let active = write_aliyun_artifact(&dirs, "live", DEAD_PID, true);
        let young = write_qrypt_artifact(&dirs, "young", DEAD_PID, false);

        gc(&dirs, &["live".to_string()]).expect("gc");
        assert!(active.exists(), "active profile artifact must be kept");
        assert!(young.exists(), "young artifact must be kept");
    }

    #[test]
    fn remove_artifact_removes_empty_workdir() {
        let (_temp, dirs) = dirs();
        let workdir = write_aliyun_artifact(&dirs, "gone", std::process::id(), false);
        let token = workdir.join("refresh_token");
        remove_artifact(&token);
        assert!(!token.exists());
        assert!(!workdir.exists(), "empty workdir should be removed");
    }
}
