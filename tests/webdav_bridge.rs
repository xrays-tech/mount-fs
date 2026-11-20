//! Hermetic test for the aliyundrive-webdav + rclone 2-process bridge.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::reconcile::MountStatus;
use mount_fs::core::state::StateRegistry;
use mount_fs::system::fuse::{MountEntry, MountSource};
use mount_fs::system::process;

const TOKEN: &str = "ALIYUN_REFRESH_SENTINEL";

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    state_path: PathBuf,
    runtime_dir: PathBuf,
    mountpoint: PathBuf,
    mount_flag: PathBuf,
    server_argv_log: PathBuf,
    server_env_log: PathBuf,
    rclone_argv_log: PathBuf,
    order_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_path_buf());
        let bin = root.join("bin");
        let runtime = root.join("runtime");
        let state = root.join("state");
        let config_dir = root.join("config");
        let config_path = config_dir.join("mount-fs").join("config.toml");
        let mountpoint = root.join("mnt").join("pan");
        for dir in [
            &bin,
            &runtime,
            &state,
            &config_dir,
            config_path.parent().expect("parent"),
        ] {
            fs::create_dir_all(dir).expect("mkdir");
        }

        let server_argv_log = root.join("server.argv");
        let server_env_log = root.join("server.env");
        let server_ready = root.join("server.ready");
        let rclone_argv_log = root.join("rclone.argv");
        let order_log = root.join("order.log");
        let mount_flag = root.join("mount.flag");

        write_script(
            &bin.join("aliyundrive-webdav"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "aliyundrive-webdav 2.3.3"
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_SERVER_ARGV_LOG"
env > "$FAKE_SERVER_ENV_LOG"
PORT=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--port" ]; then PORT="$a"; fi
  prev="$a"
done
exec python3 -c 'import socket,sys,time
s=socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen(16)
open(sys.argv[2], "w").write("ready")
time.sleep(300)' "$PORT" "$FAKE_SERVER_READY"
"#,
        );
        write_script(
            &bin.join("rclone"),
            r#"#!/bin/sh
if [ "$1" = "obscure" ]; then
  # Stub that does not embed the plaintext password.
  echo "obscured-$(printf '%s' "$2" | cksum | cut -d' ' -f1)"
  exit 0
fi
if [ "$1" = "mount" ]; then
  printf '%s\n' "$@" > "$FAKE_RCLONE_ARGV_LOG"
  if [ -f "$FAKE_SERVER_READY" ]; then
    echo server_ready >> "$FAKE_ORDER_LOG"
  else
    echo server_missing >> "$FAKE_ORDER_LOG"
  fi
  : > "$FAKE_MOUNT_FLAG"
  trap 'rm -f "$FAKE_MOUNT_FLAG"' EXIT
  sleep 300
fi
exit 1
"#,
        );
        write_script(
            &bin.join("fusermount3"),
            r#"#!/bin/sh
rm -f "$FAKE_MOUNT_FLAG"
exit 0
"#,
        );

        let config_body = format!(
            r#"[general]
mount_root = "{}"

[profiles.pan]
provider = "aliyun"
engine = "aliyundrive-webdav"
mountpoint = "{}"
client_id = "client-id"
client_secret = "client-secret"
"#,
            root.join("mnt").display(),
            mountpoint.display()
        );
        fs::write(&config_path, config_body).expect("config");

        // Keep the test hermetic: no user-installed engines from ~/.local/bin.
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("ALIYUN_REFRESH_TOKEN", TOKEN);
        std::env::set_var("FAKE_SERVER_ARGV_LOG", &server_argv_log);
        std::env::set_var("FAKE_SERVER_ENV_LOG", &server_env_log);
        std::env::set_var("FAKE_SERVER_READY", &server_ready);
        std::env::set_var("FAKE_RCLONE_ARGV_LOG", &rclone_argv_log);
        std::env::set_var("FAKE_ORDER_LOG", &order_log);
        std::env::set_var("FAKE_MOUNT_FLAG", &mount_flag);

        Fixture {
            _temp: temp,
            config_path,
            state_path: state.join("mount-fs").join("mounts.json"),
            runtime_dir: runtime,
            mountpoint,
            mount_flag,
            server_argv_log,
            server_env_log,
            rclone_argv_log,
            order_log,
        }
    }

    fn source(&self) -> FakeMountSource {
        FakeMountSource {
            target: self.mountpoint.clone(),
            flag: self.mount_flag.clone(),
        }
    }
}

fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

struct FakeMountSource {
    target: PathBuf,
    flag: PathBuf,
}

impl MountSource for FakeMountSource {
    fn list(&self) -> io::Result<Vec<MountEntry>> {
        if self.flag.exists() {
            Ok(vec![MountEntry {
                mount_id: 1,
                parent_id: 0,
                source: "aliyun".to_string(),
                target: self.target.clone(),
                fstype: "fuse.rclone".to_string(),
                options: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
    }
}

fn env_value(log: &str, key: &str) -> Option<String> {
    log.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

fn process_is_dead(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rfind(')')
            .and_then(|close| stat.get(close + 1..))
            .and_then(|rest| rest.split_whitespace().next())
            .map(|state| state == "Z" || state == "X")
            .unwrap_or(true),
    }
}

fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if process_is_dead(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn webdav_bridge_spawns_server_before_rclone_and_cleans_up() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: python3 is required for the fake TCP listener");
        return;
    }

    let fixture = Fixture::new();
    let source = fixture.source();

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "pan",
        MountOptionsArg::default(),
    )
    .expect("mount");

    // Server argv/env: token only in env, never argv.
    let server_argv = fs::read_to_string(&fixture.server_argv_log).expect("server argv");
    assert!(!server_argv.contains(TOKEN), "token leaked: {server_argv}");
    assert!(server_argv.contains("--host"));
    assert!(server_argv.contains("--port"));
    assert!(server_argv.contains("--workdir"));

    let server_env = fs::read_to_string(&fixture.server_env_log).expect("server env");
    assert_eq!(
        env_value(&server_env, "REFRESH_TOKEN").as_deref(),
        Some(TOKEN)
    );
    let webdav_user = env_value(&server_env, "WEBDAV_AUTH_USER").expect("auth user");
    let webdav_pass = env_value(&server_env, "WEBDAV_AUTH_PASSWORD").expect("auth pass");
    assert_eq!(
        env_value(&server_env, "CLIENT_ID").as_deref(),
        Some("client-id")
    );
    assert_eq!(
        env_value(&server_env, "CLIENT_SECRET").as_deref(),
        Some("client-secret")
    );

    // Readiness: rclone ran only after the server was accepting connections.
    let order = fs::read_to_string(&fixture.order_log).expect("order");
    assert!(order.contains("server_ready"), "order: {order}");

    // rclone argv must not contain secrets.
    let rclone_argv = fs::read_to_string(&fixture.rclone_argv_log).expect("rclone argv");
    assert!(!rclone_argv.contains(TOKEN));
    assert!(
        !rclone_argv.contains(&webdav_pass),
        "plaintext pass in argv"
    );
    assert!(rclone_argv.contains("--vfs-cache-mode"));
    assert!(rclone_argv.contains("--no-update-modtime"));

    // State records the main pid and one sidecar.
    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry.get("pan").expect("record").clone();
    let main_pid = record.pid.expect("main pid");
    assert_eq!(record.sidecars.len(), 1, "one sidecar expected");
    let sidecar_pid = record.sidecars[0].pid;
    assert!(
        record.sidecars[0].starttime > 0 || process::pid_starttime(sidecar_pid).is_some(),
        "sidecar starttime"
    );

    // rclone.conf is 0600 and only stores the obscured password.
    let workdir = record.credential_path.clone().expect("workdir");
    assert!(workdir.is_dir(), "workdir artifact expected");
    let conf_path = workdir.join("rclone.conf");
    let conf = fs::read_to_string(&conf_path).expect("rclone.conf");
    assert!(conf.contains("[mountfs]"));
    assert!(conf.contains("pass = obscured-"), "conf: {conf}");
    assert!(!conf.contains(&webdav_pass), "plaintext pass in conf");
    assert_eq!(
        fs::metadata(&conf_path).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    assert!(!webdav_user.is_empty());

    // A dead sidecar classifies the mount as Hung.
    process::terminate(sidecar_pid);
    assert!(wait_dead(sidecar_pid, Duration::from_secs(3)));
    let status = orchestrator::status_with_config(&source, "pan", Some(&fixture.config_path))
        .expect("status");
    assert_eq!(status.status, MountStatus::Hung);

    // Umount kills both processes and removes the temp workdir.
    orchestrator::umount(&source, "pan", false, false).expect("umount");
    assert!(
        wait_dead(main_pid, Duration::from_secs(3)),
        "main still alive"
    );
    assert!(!fixture.mount_flag.exists());
    assert!(!workdir.exists(), "workdir should be removed");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    assert!(registry.get("pan").is_none());

    // No temporary credential leftovers.
    let runtime = fixture.runtime_dir.join("mount-fs");
    if let Ok(entries) = fs::read_dir(runtime) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with("aliyundrive-webdav-"),
                "leftover artifact: {name}"
            );
        }
    }
}
