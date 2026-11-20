//! Lightweight supervision of background engine processes.
//!
//! This is intentionally minimal: it records which process was started and can
//! later tell whether that exact process is still alive. No restart/daemon
//! logic lives here yet.

use serde::{Deserialize, Serialize};

use crate::system::process::pid_starttime;

/// A running engine process recorded for later health checks.
///
/// The `starttime` guards against PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    /// Process ID.
    pub pid: u32,
    /// `starttime` from `/proc/<pid>/stat` at launch.
    pub starttime: u64,
}

impl PidRecord {
    /// Capture a record for `pid`, returning `None` if it is not running.
    pub fn capture(pid: u32) -> Option<Self> {
        pid_starttime(pid).map(|starttime| PidRecord { pid, starttime })
    }

    /// Serialize to JSON (for the M2b state registry).
    pub fn to_json(self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self)
    }

    /// Deserialize from JSON.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Health of a previously recorded process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The same process is still running.
    Running,
    /// No process with that PID exists.
    Dead,
    /// A process exists but its starttime differs (PID reuse).
    Reused,
}

/// Check whether a recorded process is still the same running process.
pub fn check(record: &PidRecord) -> Health {
    match pid_starttime(record.pid) {
        None => Health::Dead,
        Some(starttime) if starttime == record.starttime => Health::Running,
        Some(_) => Health::Reused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_running() {
        let record = PidRecord::capture(std::process::id()).expect("current process");
        assert_eq!(check(&record), Health::Running);
    }

    #[test]
    fn mismatched_starttime_is_reused() {
        let mut record = PidRecord::capture(std::process::id()).expect("current process");
        record.starttime += 1;
        assert_eq!(check(&record), Health::Reused);
    }

    #[test]
    fn missing_pid_is_dead() {
        // PID 0 is never a normal process and has no /proc entry.
        let record = PidRecord {
            pid: 0,
            starttime: 1,
        };
        assert_eq!(check(&record), Health::Dead);
    }

    #[test]
    fn pid_record_roundtrips_through_serde() {
        let record = PidRecord {
            pid: 4321,
            starttime: 123456,
        };
        let text = toml::to_string(&record).expect("serialize record");
        let parsed: PidRecord = toml::from_str(&text).expect("deserialize record");
        assert_eq!(parsed, record);

        let json = record.to_json().expect("serialize json");
        assert_eq!(
            PidRecord::from_json(&json).expect("deserialize json"),
            record
        );
    }
}
