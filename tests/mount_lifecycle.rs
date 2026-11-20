//! Hermetic mount/umount lifecycle tests.
//!
//! No real FUSE, root privileges, or network are required: engine and
//! fusermount binaries are shell scripts, and mount-table state is supplied by
//! a [`FakeMountSource`] backed by a flag file.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::reconcile::MountStatus;
use mount_fs::core::state::{MountRecord, StateRegistry};
use mount_fs::system::fuse::{MountEntry, MountSource};
use mount_fs::system::process;

const ACCESS_SENTINEL: &str = "AKIA_TEST_ACCESS_SENTINEL";
const SECRET_SENTINEL: &str = "SECRET_SENTINEL_VALUE";

/// Environment variables are process-global; serialize tests that mutate them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    state_path: PathBuf,
    runtime_dir: PathBuf,
    mountpoint: PathBuf,
    argv_log: PathBuf,
    fuser_log: PathBuf,
    mount_flag: PathBuf,
    spawn_log: PathBuf,
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
        let mountpoint = root.join("mnt").join("bucket");
        for dir in [
            &bin,
            &runtime,
            &state,
            &config_dir,
            config_path.parent().expect("config parent"),
        ] {
            fs::create_dir_all(dir).expect("create dir");
        }

        let argv_log = root.join("argv.log");
        let fuser_log = root.join("fuser.log");
        let mount_flag = root.join("mount.flag");
        let spawn_log = root.join("spawn.log");

        write_script(
            &bin.join("s3fs"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "Amazon Simple Storage Service File System V1.97"
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_ARGV_LOG"
echo spawn >> "$FAKE_SPAWN_LOG"
: > "$FAKE_MOUNT_FLAG"
trap 'rm -f "$FAKE_MOUNT_FLAG"' EXIT
sleep 300
"#,
        );
        write_script(
            &bin.join("fusermount3"),
            r#"#!/bin/sh
printf '%s\n' "$@" >> "$FAKE_FUSER_LOG"
rm -f "$FAKE_MOUNT_FLAG"
exit 0
"#,
        );

        let config_body = format!(
            r#"[general]
mount_root = "{}"

[profiles.bucket]
provider = "s3"
engine = "s3fs-fuse"
mountpoint = "{}"
bucket = "mybucket"
endpoint = "https://minio.test"
region = "us-east-1"
path_style = true
"#,
            root.join("mnt").display(),
            mountpoint.display()
        );
        fs::write(&config_path, config_body).expect("write config");

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS_SENTINEL);
        std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET_SENTINEL);
        std::env::set_var("FAKE_ARGV_LOG", &argv_log);
        std::env::set_var("FAKE_FUSER_LOG", &fuser_log);
        std::env::set_var("FAKE_MOUNT_FLAG", &mount_flag);
        std::env::set_var("FAKE_SPAWN_LOG", &spawn_log);

        Fixture {
            _temp: temp,
            config_path,
            state_path: state.join("mount-fs").join("mounts.json"),
            runtime_dir: runtime,
            mountpoint,
            argv_log,
            fuser_log,
            mount_flag,
            spawn_log,
        }
    }

    fn source(&self) -> FakeMountSource {
        self.source_with(Vec::new())
    }

    fn source_with(&self, extra: Vec<MountEntry>) -> FakeMountSource {
        FakeMountSource {
            mountpoint: self.mountpoint.clone(),
            flag: self.mount_flag.clone(),
            extra,
        }
    }

    fn spawn_count(&self) -> usize {
        fs::read_to_string(&self.spawn_log)
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    }

    fn cred_files(&self) -> Vec<PathBuf> {
        let runtime = self.runtime_dir.join("mount-fs");
        let Ok(entries) = fs::read_dir(runtime) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with("creds-bucket-") && name.ends_with(".s3fs")
            })
            .collect()
    }
}

fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
}

struct FakeMountSource {
    mountpoint: PathBuf,
    flag: PathBuf,
    extra: Vec<MountEntry>,
}

impl MountSource for FakeMountSource {
    fn list(&self) -> io::Result<Vec<MountEntry>> {
        let mut entries = self.extra.clone();
        if self.flag.exists() {
            entries.push(MountEntry {
                mount_id: 1,
                parent_id: 0,
                source: "mybucket".to_string(),
                target: self.mountpoint.clone(),
                fstype: "fuse.s3fs".to_string(),
                options: Vec::new(),
            });
        }
        Ok(entries)
    }
}

fn entry(target: &Path, fstype: &str, source: &str) -> MountEntry {
    MountEntry {
        mount_id: 1,
        parent_id: 0,
        source: source.to_string(),
        target: target.to_path_buf(),
        fstype: fstype.to_string(),
        options: Vec::new(),
    }
}

fn record(
    profile: &str,
    mountpoint: &Path,
    pid: Option<u32>,
    starttime: Option<u64>,
) -> MountRecord {
    MountRecord {
        profile: profile.to_string(),
        provider: "s3".to_string(),
        engine: "s3fs-fuse".to_string(),
        mountpoint: mountpoint.to_path_buf(),
        source: format!("{profile}-bucket"),
        pid,
        starttime,
        log_path: None,
        credential_path: None,
        mounted_at: 0,
    }
}

struct KillOnDrop {
    pid: u32,
    starttime: Option<u64>,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let alive = match self.starttime {
            Some(starttime) => process::is_alive(self.pid, starttime),
            None => process::pid_starttime(self.pid).is_some(),
        };
        if alive {
            process::terminate(self.pid);
        }
    }
}

fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_is_dead(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
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

#[test]
fn hermetic_mount_and_umount_lifecycle() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let source = fixture.source();

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "bucket",
        MountOptionsArg::default(),
    )
    .expect("mount");

    // Recorded argv must contain the expected options and no secrets.
    let argv = fs::read_to_string(&fixture.argv_log).expect("argv log");
    assert!(argv.contains("mybucket"), "argv: {argv}");
    assert!(argv.contains("passwd_file="), "argv: {argv}");
    assert!(argv.contains("url=https://minio.test"), "argv: {argv}");
    assert!(argv.contains("use_path_request_style"), "argv: {argv}");
    assert!(argv.contains("region=us-east-1"), "argv: {argv}");
    assert!(argv.contains("sigv4"), "argv: {argv}");
    assert!(
        argv.contains(&fixture.mountpoint.display().to_string()),
        "argv: {argv}"
    );
    assert!(
        !argv.contains(ACCESS_SENTINEL),
        "access key leaked into argv: {argv}"
    );
    assert!(
        !argv.contains(SECRET_SENTINEL),
        "secret leaked into argv: {argv}"
    );

    // Credential file exists with 0600 and exact content.
    let credentials = fixture.cred_files();
    assert_eq!(credentials.len(), 1, "expected one credential file");
    let credential_path = credentials[0].clone();
    assert_eq!(
        fs::read_to_string(&credential_path).expect("read cred"),
        format!("{ACCESS_SENTINEL}:{SECRET_SENTINEL}\n")
    );
    let mode = fs::metadata(&credential_path)
        .expect("cred metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    // State record exists with a live pid.
    let registry = StateRegistry::load_from(&fixture.state_path).expect("load state");
    let record = registry.get("bucket").expect("state record").clone();
    assert_eq!(record.provider, "s3");
    assert_eq!(record.engine, "s3fs-fuse");
    assert_eq!(record.mountpoint, fixture.mountpoint);
    assert_eq!(
        record.credential_path.as_deref(),
        Some(credential_path.as_path())
    );
    let log_path = record.log_path.clone().expect("log path");
    let log_mode = fs::metadata(&log_path)
        .expect("log metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(log_mode, 0o600, "engine log must be 0600");
    let pid = record.pid.expect("pid");
    assert!(record.starttime.is_some(), "starttime recorded");
    let _kill_guard = KillOnDrop {
        pid,
        starttime: record.starttime,
    };

    // Unmount.
    orchestrator::umount(&source, "bucket", false, false).expect("umount");

    let fuser = fs::read_to_string(&fixture.fuser_log).expect("fusermount log");
    assert!(fuser.contains("-u"), "fusermount args: {fuser}");
    assert!(
        fuser.contains(&fixture.mountpoint.display().to_string()),
        "fusermount args: {fuser}"
    );
    assert!(!fixture.mount_flag.exists(), "mount flag should be removed");
    assert!(
        !credential_path.exists(),
        "credential file should be unlinked"
    );

    let registry = StateRegistry::load_from(&fixture.state_path).expect("reload state");
    assert!(registry.get("bucket").is_none(), "state entry removed");

    assert!(
        wait_dead(pid, Duration::from_secs(3)),
        "engine pid {pid} still running after umount"
    );
}

#[test]
fn dry_run_spawns_nothing_and_leaves_no_credentials() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let source = fixture.source();

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "bucket",
        MountOptionsArg {
            dry_run: true,
            ..MountOptionsArg::default()
        },
    )
    .expect("dry run");

    assert!(!fixture.argv_log.exists(), "engine must not be spawned");
    assert!(!fixture.mount_flag.exists(), "no mount flag expected");
    assert!(
        fixture.cred_files().is_empty(),
        "dry run must remove the temp credential"
    );

    let registry = StateRegistry::load_from(&fixture.state_path).expect("load state");
    assert!(registry.get("bucket").is_none(), "no state recorded");
}

#[test]
fn second_mount_is_idempotent_and_does_not_spawn() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let source = fixture.source();

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "bucket",
        MountOptionsArg::default(),
    )
    .expect("first mount");
    assert_eq!(fixture.spawn_count(), 1, "first mount spawns once");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("load state");
    let record = registry.get("bucket").expect("record").clone();
    let _kill_guard = KillOnDrop {
        pid: record.pid.expect("pid"),
        starttime: record.starttime,
    };

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "bucket",
        MountOptionsArg::default(),
    )
    .expect("second mount");

    assert_eq!(
        fixture.spawn_count(),
        1,
        "idempotent mount must not spawn a second engine"
    );

    orchestrator::umount(&source, "bucket", false, false).expect("umount");
}

#[test]
fn reconciles_healthy_stale_hung_external_and_not_mounted() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let mnt_root = fixture
        .mountpoint
        .parent()
        .expect("mount root")
        .to_path_buf();
    let app = mnt_root.join("app");
    let stale = mnt_root.join("stale");
    let hung = mnt_root.join("hung");
    let never = mnt_root.join("never");
    let external = mnt_root.join("external");

    let config_body = format!(
        r#"[general]
mount_root = "{}"

[profiles.app]
provider = "s3"
mountpoint = "{}"
bucket = "app"

[profiles.stale]
provider = "s3"
mountpoint = "{}"
bucket = "stale"

[profiles.hung]
provider = "s3"
mountpoint = "{}"
bucket = "hung"

[profiles.never]
provider = "s3"
mountpoint = "{}"
bucket = "never"

[profiles.ext]
provider = "s3"
mountpoint = "{}"
bucket = "ext"
"#,
        mnt_root.display(),
        app.display(),
        stale.display(),
        hung.display(),
        never.display(),
        external.display()
    );
    fs::write(&fixture.config_path, config_body).expect("write config");

    let our_pid = std::process::id();
    let our_start = process::pid_starttime(our_pid);
    let mut registry = StateRegistry::new();
    registry.insert(record("app", &app, Some(our_pid), our_start));
    registry.insert(record("stale", &stale, None, None));
    registry.insert(record("hung", &hung, Some(4_000_000_000), Some(1)));
    registry.save_to(&fixture.state_path).expect("save state");

    let source = fixture.source_with(vec![
        entry(&app, "fuse.s3fs", "app"),
        entry(&hung, "fuse.s3fs", "hung"),
        entry(&external, "fuse.sshfs", "host"),
    ]);

    let rows = orchestrator::list_with_config(&source, Some(&fixture.config_path)).expect("list");
    let status_of = |name: &str| {
        rows.iter()
            .find(|row| row.profile.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("missing row for {name}"))
            .status
    };

    assert_eq!(status_of("app"), MountStatus::Healthy);
    assert_eq!(status_of("stale"), MountStatus::Stale);
    assert_eq!(status_of("hung"), MountStatus::Hung);
    assert_eq!(status_of("never"), MountStatus::NotMounted);
    assert_eq!(status_of("ext"), MountStatus::External);
    assert!(
        !rows
            .iter()
            .any(|row| row.profile.is_none() && row.mountpoint == external),
        "external mount should be attributed to its configured profile"
    );

    let status = orchestrator::status_with_config(&source, "never", Some(&fixture.config_path))
        .expect("status");
    assert_eq!(status.status, MountStatus::NotMounted);
    assert!(
        orchestrator::status_with_config(&source, "missing", Some(&fixture.config_path)).is_err()
    );

    let json = orchestrator::json_list(&rows).expect("json");
    assert!(json.contains("\"not-mounted\""), "json: {json}");
}

#[test]
fn umount_all_resolves_healthy_and_stale_and_persists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let mnt_root = fixture
        .mountpoint
        .parent()
        .expect("mount root")
        .to_path_buf();
    let stale = mnt_root.join("stale");

    let mut registry = StateRegistry::new();
    registry.insert(record("bucket", &fixture.mountpoint, None, None));
    registry.insert(record("stale", &stale, None, None));
    registry.save_to(&fixture.state_path).expect("save state");

    fs::write(&fixture.mount_flag, b"").expect("simulate mount");
    let source = fixture.source();

    orchestrator::umount(&source, "", false, true).expect("umount --all");

    assert!(!fixture.mount_flag.exists(), "healthy mount was unmounted");
    let fuser = fs::read_to_string(&fixture.fuser_log).expect("fusermount log");
    assert!(fuser.contains("-u"), "fusermount args: {fuser}");

    let registry = StateRegistry::load_from(&fixture.state_path).expect("reload state");
    assert!(
        registry.mounts.is_empty(),
        "registry should be empty: {registry:?}"
    );
}
