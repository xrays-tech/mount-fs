//! Hermetic test for `config login aliyun --qr`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use mount_fs::config::Config;

const TOKEN: &str = "QR_SENTINEL_TOKEN";

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
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

        let script = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "aliyundrive-webdav 2.3.3"
  exit 0
fi
if [ "$1" = "qr" ] && [ "$2" = "login" ]; then
  printf 'QR-CODE-ART\nPlease scan the qrcode to login in 120 seconds\n\nrefresh_token:\n\n%s\n' "$FAKE_QR_TOKEN"
  exit 0
fi
exit 1
"#;
        let path = bin.join("aliyundrive-webdav");
        fs::write(&path, script).expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        let config = format!(
            r#"[profiles.aliyun]
provider = "aliyun"
engine = "aliyundrive-webdav"
mountpoint = "{}"

[profiles.bucket]
provider = "s3"
engine = "s3fs-fuse"
mountpoint = "{}"
bucket = "b"
endpoint = "https://e"
region = "r"
"#,
            root.join("mnt").join("aliyun").display(),
            root.join("mnt").join("bucket").display()
        );
        fs::write(&config_path, config).expect("config");

        // Keep the test hermetic: no user-installed engines from ~/.local/bin.
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("FAKE_QR_TOKEN", TOKEN);

        Fixture {
            _temp: temp,
            config_path,
        }
    }
}

#[test]
fn qr_login_captures_token_without_printing_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mount-fs"))
        .args([
            "--config",
            fixture.config_path.to_str().expect("path"),
            "config",
            "login",
            "aliyun",
            "--profile",
            "aliyun",
            "--qr",
        ])
        .output()
        .expect("run mount-fs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "login failed: stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("QR-CODE-ART"), "QR not shown: {stdout}");
    assert!(!stdout.contains(TOKEN), "token was printed: {stdout}");
    assert!(!stderr.contains(TOKEN), "token was logged: {stderr}");

    let config = Config::load(&fixture.config_path).expect("config");
    let profile = config.profile("aliyun").expect("profile");
    let token_path = profile
        .credentials_ref
        .clone()
        .expect("credentials_ref updated");
    assert!(token_path.exists());
    assert_eq!(
        fs::read_to_string(&token_path).expect("read token"),
        format!("{TOKEN}\n")
    );
    assert_eq!(
        fs::metadata(&token_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // `config show` must not reveal the token either.
    let show = Command::new(env!("CARGO_BIN_EXE_mount-fs"))
        .args([
            "--config",
            fixture.config_path.to_str().expect("path"),
            "config",
            "show",
            "aliyun",
        ])
        .output()
        .expect("show");
    let shown = String::from_utf8_lossy(&show.stdout);
    assert!(!shown.contains(TOKEN), "config show leaked token: {shown}");
}

#[test]
fn qr_flag_is_rejected_for_s3() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mount-fs"))
        .args([
            "--config",
            fixture.config_path.to_str().expect("path"),
            "config",
            "login",
            "s3",
            "--profile",
            "bucket",
            "--qr",
        ])
        .output()
        .expect("run mount-fs");

    assert!(!output.status.success(), "s3 --qr should fail");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("--qr") && combined.contains("aliyun"),
        "unexpected error: {combined}"
    );
}
