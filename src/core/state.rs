//! Persistent mount-state registry.
//!
//! This is an advisory record of mounts started by `mount-fs`. The
//! authoritative source for what is actually mounted remains the kernel mount
//! table (see [`crate::core::reconcile`]).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use etcetera::{choose_base_strategy, BaseStrategy};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::system::locking::FileLock;

/// Directory name under the state directory.
pub const STATE_DIR_NAME: &str = "mount-fs";

/// Registry file name.
pub const STATE_FILE_NAME: &str = "mounts.json";

/// A mount started by `mount-fs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    /// Profile name.
    pub profile: String,
    /// Provider id.
    pub provider: String,
    /// Engine id.
    pub engine: String,
    /// Local mountpoint.
    pub mountpoint: PathBuf,
    /// Remote source (bucket / drive).
    pub source: String,
    /// Engine process id, if running in the background.
    pub pid: Option<u32>,
    /// `starttime` guard for the pid.
    pub starttime: Option<u64>,
    /// Log file path.
    pub log_path: Option<PathBuf>,
    /// Materialized credential file path.
    pub credential_path: Option<PathBuf>,
    /// Unix timestamp (seconds) when the mount was started.
    pub mounted_at: u64,
}

/// The persisted registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRegistry {
    /// Registry schema version.
    pub version: u32,
    /// Mounts keyed by profile name.
    pub mounts: BTreeMap<String, MountRecord>,
}

impl Default for StateRegistry {
    fn default() -> Self {
        StateRegistry {
            version: 1,
            mounts: BTreeMap::new(),
        }
    }
}

impl StateRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        StateRegistry::default()
    }

    /// Load the registry from the default location.
    pub fn load() -> Result<Self> {
        StateRegistry::load_from(&default_state_path()?)
    }

    /// Load the registry from `path`; a missing file yields an empty registry.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(StateRegistry::default());
        }
        let contents = std::fs::read_to_string(path).map_err(|err| {
            AppError::Config(format!("cannot read state file {}: {err}", path.display()))
        })?;
        serde_json::from_str(&contents).map_err(|err| {
            AppError::Config(format!("cannot parse state file {}: {err}", path.display()))
        })
    }

    /// Save the registry to the default location.
    pub fn save_atomic(&self) -> Result<()> {
        self.save_to(&default_state_path()?)
    }

    /// Save the registry atomically (temp file + rename) under an exclusive
    /// lock on `mounts.lock`.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(dir)?;
        let dir = dir.to_path_buf();
        with_lock(&dir, || self.save_unlocked(path))
    }

    /// Serialize and atomically write without acquiring the state lock.
    fn save_unlocked(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| AppError::Config(format!("cannot serialize state: {err}")))?;
        atomic_write_private(path, text.as_bytes())
    }

    /// Look up a record by profile.
    pub fn get(&self, profile: &str) -> Option<&MountRecord> {
        self.mounts.get(profile)
    }

    /// Insert or replace a record.
    pub fn insert(&mut self, record: MountRecord) -> Option<MountRecord> {
        self.mounts.insert(record.profile.clone(), record)
    }

    /// Remove a record.
    pub fn remove(&mut self, profile: &str) -> Option<MountRecord> {
        self.mounts.remove(profile)
    }
}

/// Run `f` while holding an exclusive lock on `<state_dir>/mounts.lock`.
pub fn with_lock<T>(state_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    create_private_dir(state_dir)?;
    let _lock = FileLock::acquire(state_dir.join("mounts.lock"))?;
    f()
}

/// Load, mutate, and atomically persist the registry while holding its lock.
///
/// This is the only correct way to mutate the registry: a plain load +
/// `save_to` pair can lose concurrent updates. Mutations are persisted even if
/// `f` returns an error (so partial progress such as per-profile umount results
/// is not lost).
pub fn with_locked_registry<T>(
    state_dir: &Path,
    f: impl FnOnce(&mut StateRegistry) -> Result<T>,
) -> Result<T> {
    create_private_dir(state_dir)?;
    let _lock = FileLock::acquire(state_dir.join("mounts.lock"))?;
    let path = state_dir.join(STATE_FILE_NAME);
    let mut registry = StateRegistry::load_from(&path)?;
    let result = f(&mut registry);
    registry.save_unlocked(&path)?;
    result
}

/// [`with_locked_registry`] against the default state directory.
pub fn update_registry<T>(f: impl FnOnce(&mut StateRegistry) -> Result<T>) -> Result<T> {
    with_locked_registry(&default_state_dir()?, f)
}

/// Current Unix time in seconds.
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Default state directory (`$XDG_STATE_HOME/mount-fs`).
pub fn default_state_dir() -> Result<PathBuf> {
    let strategy = choose_base_strategy()
        .map_err(|err| AppError::Config(format!("cannot determine state directory: {err}")))?;
    let base = strategy
        .state_dir()
        .ok_or_else(|| AppError::Config("no state directory available".to_string()))?;
    Ok(base.join(STATE_DIR_NAME))
}

/// Default registry path.
pub fn default_state_path() -> Result<PathBuf> {
    Ok(default_state_dir()?.join(STATE_FILE_NAME))
}

/// Default log path for a profile (`.../mount-fs/logs/<profile>.log`).
pub fn default_log_path(profile: &str) -> Result<PathBuf> {
    Ok(default_state_dir()?
        .join("logs")
        .join(format!("{profile}.log")))
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STATE_FILE_NAME);
    let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));

    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
    }
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;

    if let Err(err) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(err.into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|err| {
        AppError::Config(format!(
            "cannot create state directory {}: {err}",
            path.display()
        ))
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
        AppError::Config(format!(
            "cannot set permissions on state directory {}: {err}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> MountRecord {
        MountRecord {
            profile: "bucket".to_string(),
            provider: "s3".to_string(),
            engine: "s3fs-fuse".to_string(),
            mountpoint: PathBuf::from("/tmp/mnt/bucket"),
            source: "mybucket".to_string(),
            pid: Some(4321),
            starttime: Some(123456),
            log_path: Some(PathBuf::from("/tmp/state/logs/bucket.log")),
            credential_path: Some(PathBuf::from("/tmp/run/creds-bucket-1.s3fs")),
            mounted_at: 1_700_000_000,
        }
    }

    #[test]
    fn roundtrips_through_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state").join(STATE_FILE_NAME);

        let mut registry = StateRegistry::new();
        registry.insert(sample_record());
        registry.save_to(&path).expect("save");

        let loaded = StateRegistry::load_from(&path).expect("load");
        assert_eq!(loaded.get("bucket"), Some(&sample_record()));
    }

    #[test]
    fn missing_file_is_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = StateRegistry::load_from(&temp.path().join("absent.json")).expect("load");
        assert!(registry.mounts.is_empty());
        assert_eq!(registry.version, 1);
    }

    #[test]
    fn save_creates_private_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state").join(STATE_FILE_NAME);
        let mut registry = StateRegistry::new();
        registry.insert(sample_record());
        registry.save_to(&path).expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let dir_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn lock_file_is_created() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state").join(STATE_FILE_NAME);
        let registry = StateRegistry::new();
        registry.save_to(&path).expect("save");
        assert!(path.with_file_name("mounts.lock").exists());
    }

    #[test]
    fn insert_get_remove() {
        let mut registry = StateRegistry::new();
        assert!(registry.get("bucket").is_none());
        registry.insert(sample_record());
        assert!(registry.get("bucket").is_some());
        assert!(registry.remove("bucket").is_some());
        assert!(registry.get("bucket").is_none());
    }

    #[test]
    fn with_locked_registry_persists_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("state");

        with_locked_registry(&dir, |registry| {
            registry.insert(sample_record());
            Ok(())
        })
        .expect("mutate");

        let loaded = StateRegistry::load_from(&dir.join(STATE_FILE_NAME)).expect("load");
        assert_eq!(loaded.get("bucket"), Some(&sample_record()));
    }

    #[test]
    fn with_locked_registry_persists_on_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("state");

        let result: Result<()> = with_locked_registry(&dir, |registry| {
            registry.insert(sample_record());
            Err(AppError::Config("simulated failure".to_string()))
        });
        assert!(result.is_err());

        let loaded = StateRegistry::load_from(&dir.join(STATE_FILE_NAME)).expect("load");
        assert!(loaded.get("bucket").is_some(), "mutation persisted anyway");
    }
}
