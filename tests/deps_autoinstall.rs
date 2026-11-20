//! Hermetic tests for dependency commands and `mount --auto-install --dry-run`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    pkg_flag: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_path_buf());
        let bin = root.join("bin");
        let config_dir = root.join("config");
        let state = root.join("state");
        let runtime = root.join("runtime");
        let config_path = config_dir.join("mount-fs").join("config.toml");
        for dir in [
            &bin,
            &config_dir,
            config_path.parent().expect("parent"),
            &state,
            &runtime,
        ] {
            fs::create_dir_all(dir).expect("mkdir");
        }

        write_script(&bin.join("sudo"), "#!/bin/sh\nexit 0\n");
        let pkg_flag = root.join("pkg.ran");
        write_script(
            &bin.join("dnf"),
            &format!("#!/bin/sh\n: > \"{}\"\nexit 0\n", pkg_flag.display()),
        );
        // Intentionally no fusermount3 / qrypt / rclone: both engine and deps are missing.

        let config = format!(
            r#"[profiles.qrypt]
provider = "aliyun"
engine = "qrypt"
mountpoint = "{}"
drive_id = "drive-1"

[profiles.fuse]
provider = "aliyun"
engine = "aliyundrive-fuse"
mountpoint = "{}"
"#,
            root.join("mnt").join("qrypt").display(),
            root.join("mnt").join("fuse").display()
        );
        fs::write(&config_path, config).expect("config");

        // Only the fake bin: host-installed fusermount3/rclone must not satisfy deps.
        std::env::set_var("PATH", bin.display().to_string());
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("ALIYUN_REFRESH_TOKEN", "token");
        std::env::set_var("FAKE_PKG_FLAG", &pkg_flag);

        Fixture {
            _temp: temp,
            config_path,
            pkg_flag,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_mount-fs"))
            .args(["--config", self.config_path.to_str().expect("path")])
            .args(args)
            .output()
            .expect("run mount-fs")
    }
}

fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn mount_auto_install_dry_run_prints_engine_and_dep_commands() {
    let _guard = fixture_lock();
    // A real /dev/fuse is required for the common dependency check. The dry run
    // itself never mounts.
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not present");
        return;
    }

    let fixture = Fixture::new();
    let output = fixture.run(&["mount", "qrypt", "--auto-install", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("Release archive"),
        "engine install not printed: {stdout}"
    );
    assert!(
        stdout.contains("Dependency install command")
            && stdout.contains("sudo -- dnf install -y fuse3"),
        "dependency command not printed: {stdout}"
    );
    assert!(!fixture.pkg_flag.exists(), "dry run executed dnf");
}

#[test]
fn install_no_deps_skips_dependency_installation() {
    let _guard = fixture_lock();
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not present");
        return;
    }

    let fixture = Fixture::new();
    let output = fixture.run(&["install", "qrypt", "--dry-run", "--no-deps"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("Release archive"), "stdout: {stdout}");
    assert!(
        !stdout.contains("Dependency install command"),
        "--no-deps still printed dependencies: {stdout}"
    );
    assert!(!fixture.pkg_flag.exists(), "dry run executed dnf");
}

#[test]
fn auto_install_dry_run_does_not_require_credentials() {
    let _guard = fixture_lock();
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not present");
        return;
    }

    let fixture = Fixture::new();
    // No Aliyun credentials are configured for this scenario.
    std::env::remove_var("ALIYUN_REFRESH_TOKEN");

    let output = fixture.run(&["mount", "fuse", "--auto-install", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("Release archive"),
        "engine install plan missing: {stdout}"
    );
    assert!(
        stdout.contains("Dependency install command") && stdout.contains("fuse3"),
        "dependency plan missing: {stdout}"
    );
    assert!(
        !stdout.contains("credential") && !stderr.contains("credential"),
        "credential error leaked: stdout={stdout} stderr={stderr}"
    );
    assert!(!fixture.pkg_flag.exists(), "dry run executed dnf");
}

fn fixture_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}
