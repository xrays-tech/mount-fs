//! Hermetic tests for the unmount escalation ladder.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use mount_fs::core::creds::{profile_nonce, sanitize_profile};
use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::state::StateRegistry;
use mount_fs::system::fuse::{MountEntry, MountSource};
use mount_fs::system::process;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    state_path: PathBuf,
    runtime_dir: PathBuf,
    root: PathBuf,
    mount_flag: PathBuf,
    order_log: PathBuf,
}

impl Fixture {
    fn new(fuser_mode: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_path_buf());
        let bin = root.join("bin");
        let runtime = root.join("runtime");
        let state = root.join("state");
        let config_dir = root.join("config");
        let config_path = config_dir.join("mount-fs").join("config.toml");
        for dir in [
            &bin,
            &runtime,
            &state,
            &config_dir,
            config_path.parent().expect("parent"),
        ] {
            fs::create_dir_all(dir).expect("mkdir");
        }

        let mount_flag = root.join("mount.flag");
        let order_log = root.join("order.log");

        write_script(
            &bin.join("s3fs"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "Amazon Simple Storage Service File System V1.97"
  exit 0
fi
: > "$FAKE_MOUNT_FLAG"
trap 'echo term >> "$FAKE_ORDER_LOG"; rm -f "$FAKE_MOUNT_FLAG"; exit 0' TERM
while :; do sleep 0.1; done
"#,
        );
        write_script(
            &bin.join("qrypt"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "qrypt 0.7.0"
  exit 0
fi
: > "$FAKE_MOUNT_FLAG"
trap '' TERM
while :; do sleep 0.1; done
"#,
        );
        write_script(
            &bin.join("rclone"),
            r#"#!/bin/sh
if [ "$1" = "obscure" ]; then
  echo "obscured-stub"
  exit 0
fi
if [ "$1" = "mount" ]; then
  : > "$FAKE_MOUNT_FLAG"
  trap 'echo term >> "$FAKE_ORDER_LOG"; rm -f "$FAKE_MOUNT_FLAG"; exit 0' TERM
  while :; do sleep 0.1; done
fi
exit 1
"#,
        );
        write_script(
            &bin.join("aliyundrive-webdav"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "aliyundrive-webdav 2.3.3"
  exit 0
fi
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
time.sleep(300)' "$PORT"
"#,
        );
        let fuser = r#"#!/bin/sh
echo "fuser $*" >> "$FAKE_ORDER_LOG"
if [ "$FAKE_FUSER_MODE" = "success" ]; then
  rm -f "$FAKE_MOUNT_FLAG"
  exit 0
fi
exit 1
"#;
        write_script(&bin.join("fusermount3"), fuser);

        let config = format!(
            r#"[profiles.s3]
provider = "s3"
engine = "s3fs-fuse"
mountpoint = "{}"
bucket = "b"
endpoint = "https://e"
region = "r"

[profiles.qrypt]
provider = "aliyun"
engine = "qrypt"
mountpoint = "{}"
drive_id = "drive-1"

[profiles.webdav]
provider = "aliyun"
engine = "aliyundrive-webdav"
mountpoint = "{}"
"#,
            root.join("mnt").join("s3").display(),
            root.join("mnt").join("qrypt").display(),
            root.join("mnt").join("webdav").display()
        );
        fs::write(&config_path, config).expect("config");

        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("ALIYUN_REFRESH_TOKEN", "token");
        std::env::set_var("AWS_ACCESS_KEY_ID", "AK");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "SK");
        std::env::set_var("FAKE_MOUNT_FLAG", &mount_flag);
        std::env::set_var("FAKE_ORDER_LOG", &order_log);
        std::env::set_var("FAKE_FUSER_MODE", fuser_mode);

        Fixture {
            _temp: temp,
            config_path,
            state_path: state.join("mount-fs").join("mounts.json"),
            runtime_dir: runtime,
            root,
            mount_flag,
            order_log,
        }
    }

    fn source(&self, target: &Path) -> FakeMountSource {
        FakeMountSource {
            target: target.to_path_buf(),
            flag: self.mount_flag.clone(),
        }
    }

    fn lock_path(&self, profile: &str) -> PathBuf {
        self.runtime_dir.join("mount-fs").join(format!(
            "mount-{}-{}.lock",
            sanitize_profile(profile),
            profile_nonce(profile)
        ))
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
                source: "x".to_string(),
                target: self.target.clone(),
                fstype: "fuse.rclone".to_string(),
                options: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
    }
}

fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // `pid_running` treats zombies as dead.
        if !process::pid_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn rclone_unmounts_via_sigterm_when_fusermount_fails() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: python3 required for the fake WebDAV listener");
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new("fail");
    let target = fixture.root.join("mnt").join("webdav");
    let target = fs::canonicalize(&target).unwrap_or(target);
    let source = fixture.source(&target);

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "webdav",
        MountOptionsArg::default(),
    )
    .expect("mount");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry.get("webdav").expect("record").clone();
    let main_pid = record.pid.expect("main pid");
    let sidecar_pid = record.sidecars[0].pid;
    let workdir = record.credential_path.clone().expect("workdir");
    let lock = fixture.lock_path("webdav");
    assert!(workdir.exists());
    assert!(lock.exists());

    // fusermount fails; the ladder must SIGTERM rclone, which removes the flag.
    orchestrator::umount(&source, "webdav", false, false).expect("umount");
    assert!(!fixture.mount_flag.exists());
    assert!(wait_dead(main_pid, Duration::from_secs(3)));
    assert!(wait_dead(sidecar_pid, Duration::from_secs(3)));
    assert!(!workdir.exists(), "temp workdir should be removed");
    assert!(!lock.exists(), "per-profile lock should be removed");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    assert!(registry.get("webdav").is_none());
}

#[test]
fn force_escalates_to_sigkill_and_still_cleans_up() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new("fail");
    let target = fixture.root.join("mnt").join("qrypt");
    let target = fs::canonicalize(&target).unwrap_or(target);
    let source = fixture.source(&target);

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "qrypt",
        MountOptionsArg::default(),
    )
    .expect("mount");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry.get("qrypt").expect("record").clone();
    let main_pid = record.pid.expect("pid");
    let credential = record.credential_path.clone().expect("qrypt config");
    let lock = fixture.lock_path("qrypt");
    assert!(credential.exists());
    assert!(lock.exists());

    // qrypt ignores SIGTERM; --force must SIGKILL and still clean everything.
    let err = orchestrator::umount(&source, "qrypt", true, false).expect_err("must fail");
    assert!(err.to_string().contains("still present"), "{err}");
    assert!(wait_dead(main_pid, Duration::from_secs(3)), "main alive");
    assert!(!credential.exists(), "temp credential should be removed");
    assert!(!lock.exists(), "per-profile lock should be removed");

    // The mount could not be removed, so the record is kept for a retry.
    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    assert!(registry.get("qrypt").is_some());
}

#[test]
fn fusermount_success_path_does_not_kill_first() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new("success");
    let target = fixture.root.join("mnt").join("s3");
    let target = fs::canonicalize(&target).unwrap_or(target);
    let source = fixture.source(&target);

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "s3",
        MountOptionsArg::default(),
    )
    .expect("mount");
    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry.get("s3").expect("record").clone();
    let main_pid = record.pid.expect("pid");
    let credential = record.credential_path.clone().expect("passwd file");
    let lock = fixture.lock_path("s3");

    orchestrator::umount(&source, "s3", false, false).expect("umount");
    assert!(!fixture.mount_flag.exists());
    assert!(wait_dead(main_pid, Duration::from_secs(3)));
    assert!(!credential.exists());
    assert!(!lock.exists());

    let order = fs::read_to_string(&fixture.order_log).expect("order log");
    let fuser = order.find("fuser").expect("fusermount invoked");
    let term = order.find("term").unwrap_or(usize::MAX);
    assert!(
        fuser < term,
        "fusermount should run before SIGTERM: {order}"
    );

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    assert!(registry.get("s3").is_none());
}
