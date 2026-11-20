//! Hermetic tests for the M4 configuration/credential UX.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

use mount_fs::config::{Config, ProviderKind};
use mount_fs::core::config_ux::{self, NewProfile};
use mount_fs::core::creds::resolve_s3_credentials;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(temp.path()).unwrap_or_else(|_| temp.path().to_path_buf());
        let config_dir = root.join("config");
        fs::create_dir_all(config_dir.join("mount-fs")).expect("config dir");
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
        Fixture {
            config_path: config_dir.join("mount-fs").join("config.toml"),
            _temp: temp,
        }
    }
}

fn s3_profile() -> NewProfile {
    NewProfile {
        provider: ProviderKind::S3,
        engine: Some("s3fs-fuse".to_string()),
        mountpoint: PathBuf::from("/tmp/x"),
        bucket: Some("b".to_string()),
        endpoint: Some("https://e".to_string()),
        region: Some("r".to_string()),
        path_style: true,
        drive_id: None,
        drive_type: None,
        allow_other: false,
        read_only: false,
    }
}

#[test]
fn profile_add_set_remove_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();

    config_ux::add_profile(&fixture.config_path, "demo", s3_profile(), false).expect("add");
    let config = Config::load(&fixture.config_path).expect("load");
    let profile = config.profile("demo").expect("profile");
    assert_eq!(profile.bucket.as_deref(), Some("b"));
    assert_eq!(profile.endpoint.as_deref(), Some("https://e"));

    assert!(
        config_ux::add_profile(&fixture.config_path, "demo", s3_profile(), false).is_err(),
        "must refuse to overwrite without --force"
    );
    config_ux::add_profile(&fixture.config_path, "demo", s3_profile(), true).expect("force add");

    config_ux::set_profile_fields(
        &fixture.config_path,
        "demo",
        &[
            "region=us-east-2".to_string(),
            "read_only=true".to_string(),
            "bucket=b2".to_string(),
        ],
    )
    .expect("set");
    let config = Config::load(&fixture.config_path).expect("load");
    let profile = config.profile("demo").expect("profile");
    assert_eq!(profile.region.as_deref(), Some("us-east-2"));
    assert!(profile.read_only);
    assert_eq!(profile.bucket.as_deref(), Some("b2"));

    let err = config_ux::set_profile_fields(
        &fixture.config_path,
        "demo",
        &["secret_key=ignored".to_string()],
    )
    .expect_err("secret key rejected");
    assert!(err.to_string().contains("login"), "{err}");

    config_ux::remove_profile(&fixture.config_path, "demo").expect("remove");
    let config = Config::load(&fixture.config_path).expect("load");
    assert!(config.profile("demo").is_none());
    assert!(config_ux::remove_profile(&fixture.config_path, "demo").is_err());
}

#[test]
fn s3_login_writes_0600_and_logout_removes() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();

    config_ux::add_profile(&fixture.config_path, "demo", s3_profile(), false).expect("add");
    let cred_path = config_ux::login_s3(&fixture.config_path, "demo", "AK", "SK").expect("login");
    assert!(cred_path.exists());
    assert_eq!(fs::read_to_string(&cred_path).expect("read"), "AK:SK\n");
    let mode = fs::metadata(&cred_path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let config = Config::load(&fixture.config_path).expect("load");
    let profile = config.profile("demo").expect("profile").clone();
    assert_eq!(
        profile.credentials_ref.as_deref(),
        Some(cred_path.as_path())
    );

    let (access, secret) = resolve_s3_credentials(&profile).expect("resolve");
    assert_eq!(access, "AK");
    assert_eq!(secret, "SK");

    // Group/world-readable files are rejected.
    fs::set_permissions(&cred_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(resolve_s3_credentials(&profile).is_err());
    fs::set_permissions(&cred_path, fs::Permissions::from_mode(0o600)).expect("chmod");

    let removed = config_ux::logout(&fixture.config_path, "s3", "demo").expect("logout");
    assert!(removed.contains(&cred_path));
    assert!(!cred_path.exists(), "credential file should be removed");

    let config = Config::load(&fixture.config_path).expect("load");
    let profile = config.profile("demo").expect("profile survives logout");
    assert!(profile.credentials_ref.is_none());
}

#[test]
fn show_and_get_never_print_secret_values() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let fixture = Fixture::new();

    config_ux::add_profile(&fixture.config_path, "demo", s3_profile(), false).expect("add");
    let secret = "TOPSECRET_VALUE";
    let cred_path =
        config_ux::login_s3(&fixture.config_path, "demo", "AKIA", secret).expect("login");

    let config = Config::load(&fixture.config_path).expect("load");
    let show = config_ux::render_config(&config, Some("demo")).expect("show");
    assert!(!show.contains(secret), "show leaked secret: {show}");
    assert!(show.contains("credentials_ref = file:"), "show: {show}");

    let shown_ref =
        config_ux::get_value(&config, "profiles.demo.credentials_ref").expect("get ref");
    assert!(!shown_ref.contains(secret));
    assert!(shown_ref.contains("file:"));

    assert_eq!(
        config_ux::get_value(&config, "profiles.demo.region").expect("get region"),
        "r"
    );
    assert_eq!(config_ux::redact("refresh_token", "abc"), "<redacted>");
    assert_eq!(config_ux::redact("region", "us-east-1"), "us-east-1");

    let _ = cred_path;
}
