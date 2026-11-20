//! Hermetic tests for stale/broken FUSE mountpoint recovery.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::state::StateRegistry;
use mount_fs::system::fuse::{MountEntry, MountSource};
use mount_fs::system::process;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    state_path: PathBuf,
    mountpoint: PathBuf,
    mount_flag: PathBuf,
    fuser_log: PathBuf,
    spawn_log: PathBuf,
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
        let mnt_dir = root.join("mnt");
        let mountpoint = mnt_dir.join("s3");
        for dir in [
            &bin,
            &runtime,
            &state,
            &config_dir,
            config_path.parent().expect("parent"),
            &mnt_dir,
        ] {
            fs::create_dir_all(dir).expect("mkdir");
        }

        // Simulate a stale mountpoint: a dangling symlink (metadata fails)
        // that is nevertheless reported as a FUSE mount by FakeMountSource.
        std::os::unix::fs::symlink(root.join("missing-target"), &mountpoint)
            .expect("dangling symlink");

        let mount_flag = root.join("mount.flag");
        let fuser_log = root.join("fuser.log");
        let spawn_log = root.join("spawn.log");
        fs::write(&mount_flag, b"").expect("stale mount flag");

        write_script(
            &bin.join("s3fs"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "Amazon Simple Storage Service File System V1.97"
  exit 0
fi
echo spawn >> "$FAKE_SPAWN_LOG"
: > "$FAKE_MOUNT_FLAG"
trap 'rm -f "$FAKE_MOUNT_FLAG"' EXIT
sleep 300
"#,
        );
        let fuser = r#"#!/bin/sh
printf '%s\n' "$@" >> "$FAKE_FUSER_LOG"
if [ "$FAKE_FUSER_MODE" = "success" ]; then
  rm -f "$FAKE_MOUNT_FLAG"
  rm -f "$2" 2>/dev/null
  mkdir -p "$2"
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
"#,
            mountpoint.display()
        );
        fs::write(&config_path, config).expect("config");

        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("AWS_ACCESS_KEY_ID", "AK");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "SK");
        std::env::set_var("FAKE_MOUNT_FLAG", &mount_flag);
        std::env::set_var("FAKE_FUSER_LOG", &fuser_log);
        std::env::set_var("FAKE_SPAWN_LOG", &spawn_log);
        std::env::set_var("FAKE_FUSER_MODE", fuser_mode);

        Fixture {
            _temp: temp,
            config_path,
            state_path: state.join("mount-fs").join("mounts.json"),
            mountpoint,
            mount_flag,
            fuser_log,
            spawn_log,
        }
    }

    fn source(&self) -> FakeMountSource {
        FakeMountSource {
            target: self.mountpoint.clone(),
            flag: self.mount_flag.clone(),
        }
    }

    fn fuser_calls(&self) -> String {
        fs::read_to_string(&self.fuser_log).unwrap_or_default()
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
                source: "src".to_string(),
                target: self.target.clone(),
                fstype: "fuse.s3fs".to_string(),
                options: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
    }
}

#[test]
fn stale_mount_is_lazily_unmounted_and_mount_proceeds() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not present");
        return;
    }
    let fixture = Fixture::new("success");
    let source = fixture.source();

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "s3",
        MountOptionsArg::default(),
    )
    .expect("mount after stale recovery");

    let calls = fixture.fuser_calls();
    assert!(calls.contains("-uz"), "expected lazy unmount: {calls}");
    assert!(
        calls.contains(&fixture.mountpoint.display().to_string()),
        "expected mountpoint in fuser args: {calls}"
    );
    assert_eq!(
        fs::read_to_string(&fixture.spawn_log)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "engine should spawn once after recovery"
    );

    let registry = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry.get("s3").expect("state record").clone();
    let pid = record.pid.expect("pid");

    orchestrator::umount(&source, "s3", false, false).expect("umount");
    assert!(process::wait_for_exit(
        pid,
        record.starttime,
        std::time::Duration::from_secs(3)
    ));
}

#[test]
fn stale_mount_failure_returns_clear_error_without_spawning() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not present");
        return;
    }
    let fixture = Fixture::new("fail");
    let source = fixture.source();

    let err = orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "s3",
        MountOptionsArg::default(),
    )
    .expect_err("stale mount must not be ignored");
    let message = err.to_string();
    assert!(message.contains("stale FUSE mount"), "{message}");
    assert!(message.contains("fusermount3 -uz"), "{message}");
    assert!(!fixture.spawn_log.exists(), "engine must not spawn");
}
