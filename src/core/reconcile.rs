//! Reconcile persisted state with the kernel mount table.
//!
//! The kernel is authoritative: state records are advisory and may be stale.

use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

use crate::core::state::StateRegistry;
use crate::system::fuse::{is_fuse_fstype, MountEntry};
use crate::system::supervisor::PidRecord;

/// Health/ownership classification of a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountStatus {
    /// Recorded and present in the mount table, with a live engine.
    Healthy,
    /// Recorded but absent from the mount table.
    Stale,
    /// Present in the mount table but the engine process is gone.
    Hung,
    /// Present in the mount table but not managed by `mount-fs`.
    External,
    /// Configured but never mounted.
    #[serde(rename = "not-mounted")]
    NotMounted,
}

impl fmt::Display for MountStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            MountStatus::Healthy => "healthy",
            MountStatus::Stale => "stale",
            MountStatus::Hung => "hung",
            MountStatus::External => "external",
            MountStatus::NotMounted => "not-mounted",
        };
        f.write_str(word)
    }
}

/// A reconciled mount row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reconciled {
    /// Profile name for managed mounts.
    pub profile: Option<String>,
    /// Mountpoint.
    pub mountpoint: PathBuf,
    /// Reconciled status.
    pub status: MountStatus,
    /// Mount source, if present in the mount table.
    pub source: Option<String>,
    /// Filesystem type, if present in the mount table.
    pub fstype: Option<String>,
    /// Engine pid from state, if known.
    pub pid: Option<u32>,
}

fn sidecar_alive(sidecar: &PidRecord, alive: &dyn Fn(u32, Option<u64>) -> bool) -> bool {
    if sidecar.starttime == 0 {
        alive(sidecar.pid, None)
    } else {
        alive(sidecar.pid, Some(sidecar.starttime))
    }
}

/// Reconcile `state` with `mounts`.
///
/// `alive` receives `(pid, starttime)` and reports whether that exact process
/// incarnation is still running.
pub fn reconcile(
    state: &StateRegistry,
    mounts: &[MountEntry],
    alive: &dyn Fn(u32, Option<u64>) -> bool,
) -> Vec<Reconciled> {
    let mut rows = Vec::new();
    let mut managed_mountpoints: Vec<&PathBuf> = Vec::new();

    for record in state.mounts.values() {
        managed_mountpoints.push(&record.mountpoint);

        let found = mounts
            .iter()
            .find(|mount| mount.target == record.mountpoint);

        let row = match found {
            None => Reconciled {
                profile: Some(record.profile.clone()),
                mountpoint: record.mountpoint.clone(),
                status: MountStatus::Stale,
                source: None,
                fstype: None,
                pid: record.pid,
            },
            Some(mount) if !is_fuse_fstype(&mount.fstype) => Reconciled {
                profile: Some(record.profile.clone()),
                mountpoint: record.mountpoint.clone(),
                status: MountStatus::Stale,
                source: Some(mount.source.clone()),
                fstype: Some(mount.fstype.clone()),
                pid: record.pid,
            },
            Some(mount) => {
                let health_main = match record.pid {
                    Some(pid) => alive(pid, record.starttime),
                    None => true,
                };
                let health_sidecars = record
                    .sidecars
                    .iter()
                    .all(|sidecar| sidecar_alive(sidecar, alive));
                let healthy = health_main && health_sidecars;
                Reconciled {
                    profile: Some(record.profile.clone()),
                    mountpoint: record.mountpoint.clone(),
                    status: if healthy {
                        MountStatus::Healthy
                    } else {
                        MountStatus::Hung
                    },
                    source: Some(mount.source.clone()),
                    fstype: Some(mount.fstype.clone()),
                    pid: record.pid,
                }
            }
        };
        rows.push(row);
    }

    for mount in mounts {
        if !is_fuse_fstype(&mount.fstype) {
            continue;
        }
        if managed_mountpoints
            .iter()
            .any(|target| **target == mount.target)
        {
            continue;
        }
        rows.push(Reconciled {
            profile: None,
            mountpoint: mount.target.clone(),
            status: MountStatus::External,
            source: Some(mount.source.clone()),
            fstype: Some(mount.fstype.clone()),
            pid: None,
        });
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::state::MountRecord;
    use std::path::PathBuf;

    fn entry(target: &str, fstype: &str, source: &str) -> MountEntry {
        MountEntry {
            mount_id: 1,
            parent_id: 0,
            source: source.to_string(),
            target: PathBuf::from(target),
            fstype: fstype.to_string(),
            options: Vec::new(),
        }
    }

    fn record(profile: &str, mountpoint: &str, pid: Option<u32>) -> MountRecord {
        MountRecord {
            profile: profile.to_string(),
            provider: "s3".to_string(),
            engine: "s3fs-fuse".to_string(),
            mountpoint: PathBuf::from(mountpoint),
            source: "bucket".to_string(),
            pid,
            starttime: pid.map(|_| 42),
            log_path: None,
            credential_path: None,
            sidecars: Vec::new(),
            mounted_at: 0,
        }
    }

    fn registry(records: Vec<MountRecord>) -> StateRegistry {
        let mut registry = StateRegistry::new();
        for record in records {
            registry.insert(record);
        }
        registry
    }

    #[test]
    fn healthy_when_mount_present_and_pid_alive() {
        let state = registry(vec![record("b", "/mnt/b", Some(10))]);
        let mounts = vec![entry("/mnt/b", "fuse.s3fs", "bucket")];
        let rows = reconcile(&state, &mounts, &|pid, _| pid == 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, MountStatus::Healthy);
        assert_eq!(rows[0].source.as_deref(), Some("bucket"));
    }

    #[test]
    fn stale_when_mount_absent() {
        let state = registry(vec![record("b", "/mnt/b", Some(10))]);
        let rows = reconcile(&state, &[], &|_, _| true);
        assert_eq!(rows[0].status, MountStatus::Stale);
    }

    #[test]
    fn hung_when_pid_dead() {
        let state = registry(vec![record("b", "/mnt/b", Some(10))]);
        let mounts = vec![entry("/mnt/b", "fuse.s3fs", "bucket")];
        let rows = reconcile(&state, &mounts, &|_, _| false);
        assert_eq!(rows[0].status, MountStatus::Hung);
    }

    #[test]
    fn external_for_unmanaged_fuse_mount() {
        let state = registry(vec![]);
        let mounts = vec![
            entry("/mnt/other", "fuse.sshfs", "host"),
            entry("/proc", "proc", "proc"),
        ];
        let rows = reconcile(&state, &mounts, &|_, _| true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, MountStatus::External);
        assert_eq!(rows[0].profile, None);
        assert_eq!(rows[0].mountpoint, PathBuf::from("/mnt/other"));
    }

    #[test]
    fn hung_when_a_sidecar_is_dead() {
        let mut rec = record("b", "/mnt/b", Some(10));
        rec.sidecars = vec![PidRecord {
            pid: 20,
            starttime: 0,
        }];
        let state = registry(vec![rec]);
        let mounts = vec![entry("/mnt/b", "fuse.s3fs", "bucket")];
        let rows = reconcile(&state, &mounts, &|pid, _| pid == 10);
        assert_eq!(rows[0].status, MountStatus::Hung);
    }

    #[test]
    fn non_fuse_mount_at_recorded_target_is_stale() {
        let state = registry(vec![record("b", "/mnt/b", Some(10))]);
        let mounts = vec![entry("/mnt/b", "ext4", "dev")];
        let rows = reconcile(&state, &mounts, &|_, _| true);
        assert_eq!(rows[0].status, MountStatus::Stale);
    }
}
