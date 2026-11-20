//! Process invocation helpers.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// A description of a command to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Program to execute.
    pub program: PathBuf,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    /// Create a spec for `program`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Append a single argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Build a [`Command`] from this spec.
    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    /// Run the command and capture its output.
    pub fn run_capture(&self) -> io::Result<Output> {
        self.to_command().output()
    }
}

/// Spawn a command in the background, appending output to `log_path`.
///
/// Returns the child PID without waiting. The child keeps running after this
/// function returns; callers that care should reap it later.
pub fn spawn_background(spec: &CommandSpec, log_path: &Path) -> io::Result<u32> {
    Ok(spawn_background_child(spec, log_path)?.id())
}

/// Spawn a command in the background and return the [`Child`] handle.
///
/// The log file is created with mode `0600`. Callers may use the returned
/// handle to detect early exit or to kill the process; dropping the handle does
/// not kill the child.
pub fn spawn_background_child(spec: &CommandSpec, log_path: &Path) -> io::Result<Child> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)?;
    let _ = fs::set_permissions(log_path, fs::Permissions::from_mode(0o600));

    spec.to_command()
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
}

/// Send `SIGTERM` to `pid`, ignoring errors (best effort).
pub fn terminate(pid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

/// Read up to `lines` lines from the tail of a log file.
pub fn read_log_tail(path: &Path, lines: usize) -> String {
    let Ok(contents) = fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = contents.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Read field 22 (`starttime`) of `/proc/<pid>/stat`.
pub fn pid_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime(&stat)
}

/// Whether `pid` is alive *and* still the same process incarnation.
///
/// Zombies are treated as dead (they still have `/proc` entries).
pub fn is_alive(pid: u32, expected_starttime: u64) -> bool {
    process_running(pid) && pid_starttime(pid) == Some(expected_starttime)
}

/// Whether a (non-zombie) process currently exists.
pub fn pid_running(pid: u32) -> bool {
    process_running(pid)
}

fn process_running(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rfind(')')
            .and_then(|close| stat.get(close + 1..))
            .and_then(|rest| rest.split_whitespace().next())
            .map(|state| state != "Z" && state != "X")
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Parse `starttime` from a `/proc/<pid>/stat` line.
///
/// The `comm` field may contain spaces and parentheses; it is bounded by the
/// first `(` and the last `)`, so parsing starts after the final `)`.
fn parse_starttime(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); starttime is field 22, index 19.
    fields.get(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_command_spec() {
        let spec = CommandSpec::new("/usr/bin/s3fs")
            .arg("mybucket")
            .args(["/mnt/x"])
            .env("FOO", "bar");
        assert_eq!(spec.program, PathBuf::from("/usr/bin/s3fs"));
        assert_eq!(spec.args, vec!["mybucket", "/mnt/x"]);
        assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn parses_starttime() {
        let stat =
            "1234 (bash) S 1 1234 1234 0 -1 4194560 1234 0 0 0 10 5 0 0 20 0 1 0 987654 12345 4096";
        assert_eq!(parse_starttime(stat), Some(987654));
    }

    #[test]
    fn parses_starttime_with_spaces_in_comm() {
        let stat = "1234 (my cool (prog)) S 1 1234 1234 0 -1 4194560 1234 0 0 0 10 5 0 0 20 0 1 0 424242 12345 4096";
        assert_eq!(parse_starttime(stat), Some(424242));
    }

    #[test]
    fn rejects_truncated_stat() {
        assert_eq!(parse_starttime("1234 (bash) S 1"), None);
        assert_eq!(parse_starttime("no parenthesis"), None);
    }

    #[test]
    fn current_process_starttime_is_readable() {
        let pid = std::process::id();
        let starttime = pid_starttime(pid).expect("current process stat");
        assert!(is_alive(pid, starttime));
        assert!(!is_alive(pid, starttime + 1));
    }
}
