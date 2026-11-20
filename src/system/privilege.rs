//! Privilege checks and escalation planning.
//!
//! No privileged command is executed here; this module only decides *how* a
//! command would be escalated.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{AppError, Result};

/// How a privileged command should be launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// Already running as root; no escalation needed.
    None,
    /// Use non-interactive `sudo` (`sudo -n true` succeeded).
    Sudo,
    /// Use `pkexec` (available on `PATH`).
    Pkexec,
}

/// Whether the effective user is root.
pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Choose an escalation strategy.
///
/// Returns:
/// - `Ok(Escalation::None)` when already root.
/// - `Ok(Escalation::Sudo)` when `sudo -n true` succeeds (best effort; a
///   non-interactive probe avoids blocking on a password prompt).
/// - `Ok(Escalation::Pkexec)` when `pkexec` is on `PATH`.
/// - `Err(AppError::Unsupported)` when none of the above is available. Callers
///   should surface this as a warning and avoid privileged operations.
pub fn choose_escalation() -> Result<Escalation> {
    if is_root() {
        return Ok(Escalation::None);
    }
    if sudo_non_interactive() {
        return Ok(Escalation::Sudo);
    }
    if which::which("pkexec").is_ok() {
        return Ok(Escalation::Pkexec);
    }
    Err(AppError::Unsupported(
        "no privilege escalation available (sudo -n failed and pkexec not found)".to_string(),
    ))
}

/// Build an argv that runs `program` with `args` under `escalation`.
pub fn escalated_argv(escalation: Escalation, program: &Path, args: &[String]) -> Vec<String> {
    let program = program.to_string_lossy().into_owned();
    let mut argv = match escalation {
        Escalation::None => vec![program],
        Escalation::Sudo => vec!["sudo".to_string(), "--".to_string(), program],
        Escalation::Pkexec => vec!["pkexec".to_string(), program],
    };
    argv.extend(args.iter().cloned());
    argv
}

/// Best-effort `sudo -n true` probe.
fn sudo_non_interactive() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec![
            "install".to_string(),
            "-y".to_string(),
            "s3fs-fuse".to_string(),
        ]
    }

    #[test]
    fn root_argv_is_unchanged() {
        let argv = escalated_argv(Escalation::None, Path::new("/usr/bin/dnf"), &args());
        assert_eq!(argv, vec!["/usr/bin/dnf", "install", "-y", "s3fs-fuse"]);
    }

    #[test]
    fn sudo_argv_uses_separator() {
        let argv = escalated_argv(Escalation::Sudo, Path::new("/usr/bin/dnf"), &args());
        assert_eq!(
            argv,
            vec!["sudo", "--", "/usr/bin/dnf", "install", "-y", "s3fs-fuse"]
        );
    }

    #[test]
    fn pkexec_argv() {
        let argv = escalated_argv(Escalation::Pkexec, Path::new("/usr/bin/dnf"), &args());
        assert_eq!(
            argv,
            vec!["pkexec", "/usr/bin/dnf", "install", "-y", "s3fs-fuse"]
        );
    }

    #[test]
    fn escalation_choice_is_consistent_with_root() {
        // On a root test runner this must be `None`; otherwise it may be any
        // valid choice (or an error when no escalation tool exists).
        if is_root() {
            assert_eq!(choose_escalation().unwrap(), Escalation::None);
        }
    }
}
