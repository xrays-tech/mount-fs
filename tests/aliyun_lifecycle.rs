//! Hermetic Aliyun Drive lifecycle tests.
//!
//! No network, root, or real FUSE: fake `aliyundrive-fuse`/`qrypt` scripts
//! record their argv/env, and mount-table state is faked.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mount_fs::config::Config;
use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::state::StateRegistry;
use mount_fs::providers::registry;
use mount_fs::system::fuse::{MountEntry, MountSource};
use mount_fs::system::process;

const REFRESH_SENTINEL: &str = "REFRESH_SENTINEL_VALUE";
const DRIVE_ID: &str = "drive-123";

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    state_path: PathBuf,
    runtime_dir: PathBuf,
    bin_dir: PathBuf,
    mount_flag: PathBuf,
    argv_log: PathBuf,
    env_log: PathBuf,
    pan_mountpoint: PathBuf,
    qrypt_mountpoint: PathBuf,
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
        let pan_mountpoint = root.join("mnt").join("pan");
        let qrypt_mountpoint = root.join("mnt").join("panq");
        for dir in [
            &bin,
            &runtime,
            &state,
            &config_dir,
            config_path.parent().expect("parent"),
        ] {
            fs::create_dir_all(dir).expect("create dir");
        }

        let argv_log = root.join("argv.log");
        let env_log = root.join("env.log");
        let mount_flag = root.join("mount.flag");

        write_script(
            &bin.join("aliyundrive-fuse"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "aliyundrive-fuse 0.1.14"
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_ARGV_LOG"
env > "$FAKE_ENV_LOG"
: > "$FAKE_MOUNT_FLAG"
trap 'rm -f "$FAKE_MOUNT_FLAG"' EXIT
sleep 300
"#,
        );
        write_script(
            &bin.join("qrypt"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "qrypt 0.7.0"
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_ARGV_LOG"
: > "$FAKE_MOUNT_FLAG"
trap 'rm -f "$FAKE_MOUNT_FLAG"' EXIT
sleep 300
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
engine_auto_fallback = true

[profiles.pan]
provider = "aliyun"
engine = "aliyundrive-fuse"
mountpoint = "{}"

[profiles.panq]
provider = "aliyun"
engine = "qrypt"
mountpoint = "{}"
drive_id = "{}"

[profiles.fallback]
provider = "aliyun"
engine = "qrypt"
mountpoint = "{}"
drive_id = "{}"
"#,
            root.join("mnt").display(),
            pan_mountpoint.display(),
            qrypt_mountpoint.display(),
            DRIVE_ID,
            root.join("mnt").join("fallback").display(),
            DRIVE_ID
        );
        fs::write(&config_path, config_body).expect("write config");

        // Keep the test hermetic: no user-installed engines from ~/.local/bin.
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("ALIYUN_REFRESH_TOKEN", REFRESH_SENTINEL);
        std::env::set_var("FAKE_ARGV_LOG", &argv_log);
        std::env::set_var("FAKE_ENV_LOG", &env_log);
        std::env::set_var("FAKE_MOUNT_FLAG", &mount_flag);

        Fixture {
            _temp: temp,
            config_path,
            state_path: state.join("mount-fs").join("mounts.json"),
            runtime_dir: runtime,
            bin_dir: bin,
            mount_flag,
            argv_log,
            env_log,
            pan_mountpoint,
            qrypt_mountpoint,
        }
    }

    fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    fn artifact_entries(&self) -> Vec<PathBuf> {
        let runtime = self.runtime_dir.join("mount-fs");
        let Ok(entries) = fs::read_dir(runtime) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with("aliyundrive-fuse-")
                    || name.starts_with("qrypt-")
                    || name.starts_with("creds-")
            })
            .collect()
    }

    fn source(&self, target: &Path) -> FakeMountSource {
        FakeMountSource {
            target: target.to_path_buf(),
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
                fstype: "fuse.aliyundrive".to_string(),
                options: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
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

fn file_mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn aliyun_engines_keep_secrets_out_of_argv() {
    let fixture = Fixture::new();

    // Engine selection prefers aliyundrive-fuse when installed.
    let config = Config::load(&fixture.config_path).expect("config");
    let pan = config.profile("pan").expect("pan profile");
    let provider = registry()
        .into_iter()
        .find(|provider| provider.id() == "aliyun")
        .expect("aliyun provider");
    let mut unspecified = pan.clone();
    unspecified.engine = None;
    let selected = provider.select_engine(&unspecified).expect("select engine");
    assert_eq!(selected.id(), "aliyundrive-fuse");

    // --- aliyundrive-fuse ---
    let source = fixture.source(&fixture.pan_mountpoint);

    // Read-write is refused for a read-only engine.
    let rw_error = orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "pan",
        MountOptionsArg::default(),
    )
    .expect_err("read-only engine must reject rw");
    assert!(
        rw_error.to_string().contains("read-only"),
        "unexpected error: {rw_error}"
    );
    assert!(!fixture.mount_flag.exists());

    // Dry run must not leave any artifact behind.
    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "pan",
        MountOptionsArg {
            read_only: Some(true),
            dry_run: true,
            ..MountOptionsArg::default()
        },
    )
    .expect("dry run pan");
    assert!(
        fixture.artifact_entries().is_empty(),
        "dry run left artifacts"
    );

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "pan",
        MountOptionsArg {
            read_only: Some(true),
            ..MountOptionsArg::default()
        },
    )
    .expect("mount pan");

    let argv = fs::read_to_string(&fixture.argv_log).expect("argv log");
    assert!(!argv.contains(REFRESH_SENTINEL), "secret leaked: {argv}");
    assert!(!argv.contains("-r"), "legacy -r flag used: {argv}");
    assert!(!argv.contains("--refresh-token"), "-r flag used: {argv}");
    assert!(argv.contains("--workdir"), "argv: {argv}");

    let env = fs::read_to_string(&fixture.env_log).expect("env log");
    assert!(
        env.contains(&format!("REFRESH_TOKEN={REFRESH_SENTINEL}")),
        "env log missing token: {env}"
    );

    let registry_state = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry_state.get("pan").expect("pan record").clone();
    let token_path = record.credential_path.clone().expect("token path");
    assert!(token_path.exists());
    assert!(
        token_path.to_string_lossy().contains("aliyundrive-fuse"),
        "unexpected token path: {}",
        token_path.display()
    );
    assert_eq!(file_mode(&token_path), 0o600);
    assert_eq!(
        fs::read_to_string(&token_path).expect("token"),
        REFRESH_SENTINEL
    );
    let _kill_guard = KillOnDrop {
        pid: record.pid.expect("pid"),
        starttime: record.starttime,
    };

    orchestrator::umount(&source, "pan", false, false).expect("umount pan");
    assert!(!token_path.exists(), "token file should be unlinked");
    assert!(!fixture.mount_flag.exists());

    // --- qrypt ---
    let source = fixture.source(&fixture.qrypt_mountpoint);
    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "panq",
        MountOptionsArg::default(),
    )
    .expect("mount panq");

    let argv = fs::read_to_string(&fixture.argv_log).expect("argv log");
    assert!(!argv.contains(REFRESH_SENTINEL), "secret leaked: {argv}");
    assert!(argv.contains("--config"), "argv: {argv}");
    assert!(argv.contains("--mount-point"), "argv: {argv}");

    let registry_state = StateRegistry::load_from(&fixture.state_path).expect("state");
    let record = registry_state.get("panq").expect("panq record").clone();
    let config_file = record.credential_path.clone().expect("qrypt config");
    assert!(config_file.exists());
    assert_eq!(file_mode(&config_file), 0o600);
    let rendered = fs::read_to_string(&config_file).expect("read qrypt config");
    assert!(rendered.contains(REFRESH_SENTINEL), "config: {rendered}");
    assert!(rendered.contains(DRIVE_ID), "config: {rendered}");
    assert!(rendered.contains("[[mounts]]"), "config: {rendered}");
    let _kill_guard = KillOnDrop {
        pid: record.pid.expect("pid"),
        starttime: record.starttime,
    };

    orchestrator::umount(&source, "panq", false, false).expect("umount panq");
    assert!(!config_file.exists(), "qrypt config should be unlinked");
    assert!(!fixture.mount_flag.exists());

    // --- engine_auto_fallback: on ---
    let fallback_mountpoint = fixture
        .pan_mountpoint
        .parent()
        .expect("mount root")
        .join("fallback");
    let source = fixture.source(&fallback_mountpoint);
    fs::remove_file(fixture.bin_dir().join("qrypt")).expect("remove qrypt");

    orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "fallback",
        MountOptionsArg {
            read_only: Some(true),
            ..MountOptionsArg::default()
        },
    )
    .expect("fallback mount");
    let registry_state = StateRegistry::load_from(&fixture.state_path).expect("state");
    let fallback_record = registry_state
        .get("fallback")
        .expect("fallback record")
        .clone();
    assert_eq!(fallback_record.engine, "aliyundrive-fuse");
    let _fallback_guard = KillOnDrop {
        pid: fallback_record.pid.expect("pid"),
        starttime: fallback_record.starttime,
    };
    orchestrator::umount(&source, "fallback", false, false).expect("umount fallback");

    // --- engine_auto_fallback: off ---
    let config_body = fs::read_to_string(&fixture.config_path).expect("read config");
    fs::write(
        &fixture.config_path,
        config_body.replace(
            "engine_auto_fallback = true",
            "engine_auto_fallback = false",
        ),
    )
    .expect("rewrite config");
    let err = orchestrator::mount(
        &source,
        Some(&fixture.config_path),
        "fallback",
        MountOptionsArg {
            read_only: Some(true),
            ..MountOptionsArg::default()
        },
    )
    .expect_err("fallback disabled");
    assert!(err.to_string().contains("not installed"), "{err}");

    assert!(fixture.runtime_dir.exists());
}
