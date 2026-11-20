//! `aliyundrive-webdav` engine: a 2-process WebDAV → rclone bridge.
//!
//! The upstream tool is a WebDAV *server*, not a FUSE filesystem. Mounting
//! therefore runs the server as a sidecar on a loopback port, then starts
//! `rclone mount` against it. Secrets stay in env/0600 files, never argv.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::VersionReq;

use crate::config::Profile;
use crate::core::creds;
use crate::engines::{
    Capabilities, CredentialRetention, DetectResult, EffectiveMount, Engine, InstallMethod,
    InstallPlan, MountPlan, MountSecrets, PlannedProcess, Readiness, ReleaseAsset, TokenKind,
    UnmountStrategy,
};
use crate::error::{AppError, Result};
use crate::system::distro::Distro;
use crate::system::download::ArchiveKind;
use crate::system::net;
use crate::system::pkg::PackageManager;

const PROGRAM: &str = "aliyundrive-webdav";
const RCLONE: &str = "rclone";
const VERSION: &str = "2.3.3";
const MIN_VERSION: &str = ">=2.3.3";

/// `aliyundrive-webdav` + rclone bridge engine.
#[derive(Debug, Default)]
pub struct AliyundriveWebdavEngine;

impl AliyundriveWebdavEngine {
    /// Create the engine.
    pub fn new() -> Self {
        AliyundriveWebdavEngine
    }

    /// Pinned release for the current architecture, when available.
    pub fn release_asset() -> Option<ReleaseAsset> {
        let arch = std::env::consts::ARCH;
        if arch != "x86_64" && arch != "aarch64" {
            return None;
        }
        let file = format!("aliyundrive-webdav-v{VERSION}.{arch}-unknown-linux-musl.tar.gz");
        Some(ReleaseAsset {
            version: VERSION.to_string(),
            url: format!(
                "https://github.com/messense/aliyundrive-webdav/releases/download/v{VERSION}/{file}"
            ),
            kind: ArchiveKind::TarGz,
            sha256: crate::system::checksums::lookup(PROGRAM, VERSION, arch),
            binary: PROGRAM.to_string(),
        })
    }

    /// WebDAV server argv (pure).
    pub fn server_argv(
        workdir: &Path,
        port: u16,
        read_only: bool,
        root: Option<&str>,
        drive_type: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut argv = vec![
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            port.to_string(),
            "--workdir".to_string(),
            workdir.display().to_string(),
        ];
        if read_only {
            argv.push("--read-only".to_string());
        }
        if let Some(root) = root.filter(|root| !root.is_empty()) {
            argv.push("--root".to_string());
            argv.push(root.to_string());
        }
        if let Some(drive_type) = drive_type {
            let drive_type = crate::config::normalize_drive_type(drive_type)?;
            argv.push("--drive-type".to_string());
            argv.push(drive_type);
        }
        Ok(argv)
    }

    /// `rclone mount` argv (pure).
    pub fn rclone_argv(config: &Path, mountpoint: &Path, allow_other: bool) -> Vec<String> {
        let mut argv = vec![
            "mount".to_string(),
            "--config".to_string(),
            config.display().to_string(),
            "mountfs:".to_string(),
            mountpoint.display().to_string(),
            "--vfs-cache-mode".to_string(),
            "full".to_string(),
        ];
        if allow_other {
            argv.push("--allow-other".to_string());
        }
        argv.push("--no-update-modtime".to_string());
        argv
    }

    /// Render the rclone config (pure; `pass` must already be obscured).
    pub fn render_rclone_conf(port: u16, user: &str, obscured_pass: &str) -> String {
        format!(
            "[mountfs]\ntype = webdav\nurl = http://127.0.0.1:{port}/\nvendor = other\nuser = {user}\npass = {obscured_pass}\n"
        )
    }

    fn workdir(effective: &EffectiveMount) -> PathBuf {
        effective
            .runtime_dir
            .join(creds::webdav_workdir_name(&effective.profile_name))
    }
}

/// Generate `len` random bytes as lowercase hex.
pub fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    let from_urandom = File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !from_urandom {
        let mut state = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
            ^ u64::from(std::process::id());
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
    }
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn obscure(password: &str) -> Result<String> {
    let output = Command::new(RCLONE)
        .arg("obscure")
        .arg(password)
        .output()
        .map_err(|err| {
            AppError::NotFound(format!(
                "`rclone` is required for the aliyundrive-webdav bridge: {err}"
            ))
        })?;
    if !output.status.success() {
        return Err(AppError::Unsupported(format!(
            "`rclone obscure` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let obscured = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if obscured.is_empty() {
        return Err(AppError::Unsupported(
            "`rclone obscure` returned an empty password".to_string(),
        ));
    }
    Ok(obscured)
}

impl Engine for AliyundriveWebdavEngine {
    fn id(&self) -> &'static str {
        "aliyundrive-webdav"
    }

    fn detect(&self) -> DetectResult {
        crate::engines::probe_binary(PROGRAM)
    }

    fn min_version(&self) -> Option<VersionReq> {
        VersionReq::parse(MIN_VERSION).ok()
    }

    fn install_plan(&self, _distro: &Distro, _pkg: PackageManager) -> InstallPlan {
        let notes = vec![
            "Optional engine: WebDAV server bridged to FUSE via rclone (2 processes, 1 port)."
                .to_string(),
            "Requires the external `rclone` binary.".to_string(),
            "The bridge runs the server as a sidecar and rclone as the FUSE process.".to_string(),
        ];

        match Self::release_asset() {
            Some(release) => InstallPlan {
                method: InstallMethod::ReleaseArchive,
                packages: Vec::new(),
                requires_root: false,
                notes,
                source_commands: Vec::new(),
                release: Some(release),
            },
            None => InstallPlan {
                method: InstallMethod::Unavailable,
                requires_root: false,
                notes,
                ..InstallPlan::default()
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_write: true,
            allow_other: true,
            foreground: true,
        }
    }

    fn token_kind(&self) -> TokenKind {
        TokenKind::OpenApi
    }

    fn runtime_deps(&self) -> Vec<crate::system::deps::Dependency> {
        crate::system::deps::engine_deps(self.id())
    }

    fn credential_retention(&self) -> CredentialRetention {
        CredentialRetention::Temp
    }

    fn plan_mount(
        &self,
        profile: &Profile,
        effective: &EffectiveMount,
        secrets: &MountSecrets,
    ) -> Result<MountPlan> {
        let token = secrets
            .refresh_token
            .as_deref()
            .ok_or_else(|| AppError::Credential("missing Aliyun refresh token".to_string()))?;

        let workdir = Self::workdir(effective);
        let result = (|| -> Result<MountPlan> {
            creds::create_private_dir(&workdir)?;
            creds::write_private_file(&workdir.join("refresh_token"), token.as_bytes())?;

            let port = net::free_loopback_port().map_err(|err| {
                AppError::Unsupported(format!("cannot reserve a loopback port: {err}"))
            })?;
            let user = format!("mountfs{}", random_hex(6));
            let password = random_hex(16);
            let obscured = obscure(&password)?;

            let config_path = workdir.join("rclone.conf");
            let config = Self::render_rclone_conf(port, &user, &obscured);
            creds::write_private_file(&config_path, config.as_bytes())?;

            let mut server_env = vec![
                ("REFRESH_TOKEN".to_string(), token.to_string()),
                ("WEBDAV_AUTH_USER".to_string(), user),
                ("WEBDAV_AUTH_PASSWORD".to_string(), password),
            ];
            if let Some(client_id) = profile.client_id.as_deref().filter(|id| !id.is_empty()) {
                server_env.push(("CLIENT_ID".to_string(), client_id.to_string()));
            }
            if let Some(secret) = profile
                .client_secret
                .as_deref()
                .filter(|secret| !secret.is_empty())
            {
                server_env.push(("CLIENT_SECRET".to_string(), secret.to_string()));
            }

            let server_program = which::which(PROGRAM).unwrap_or_else(|_| PathBuf::from(PROGRAM));
            let rclone_program = which::which(RCLONE).unwrap_or_else(|_| PathBuf::from(RCLONE));

            Ok(MountPlan {
                program: rclone_program,
                argv: Self::rclone_argv(&config_path, &effective.mountpoint, effective.allow_other),
                env: Vec::new(),
                mountpoint: effective.mountpoint.clone(),
                unmount: UnmountStrategy::FuseMount3,
                credential_path: Some(workdir.clone()),
                sidecars: vec![PlannedProcess {
                    program: server_program,
                    argv: Self::server_argv(
                        &workdir,
                        port,
                        effective.read_only,
                        profile.root_path.as_deref(),
                        profile.drive_type.as_deref(),
                    )?,
                    env: server_env,
                    readiness: Readiness::TcpPort(port),
                }],
            })
        })();

        if result.is_err() {
            creds::remove_artifact(&workdir);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_argv_keeps_token_out() {
        let argv = AliyundriveWebdavEngine::server_argv(
            Path::new("/run/user/1000/mount-fs/webdav-pan"),
            12345,
            true,
            Some("/drive/root"),
            None,
        )
        .expect("argv");
        assert!(argv.contains(&"127.0.0.1".to_string()));
        assert!(argv.contains(&"12345".to_string()));
        assert!(argv.contains(&"--read-only".to_string()));
        assert!(argv.contains(&"/drive/root".to_string()));
        assert!(!argv.iter().any(|arg| arg.contains("REFRESH")));
    }

    #[test]
    fn server_argv_includes_drive_type_when_set() {
        let argv = AliyundriveWebdavEngine::server_argv(
            Path::new("/run/user/1000/mount-fs/webdav-pan"),
            12345,
            false,
            None,
            Some("RESOURCE"),
        )
        .expect("argv");
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--drive-type", "resource"]));
    }

    #[test]
    fn server_argv_omits_drive_type_when_unset() {
        let argv = AliyundriveWebdavEngine::server_argv(
            Path::new("/run/user/1000/mount-fs/webdav-pan"),
            12345,
            false,
            None,
            None,
        )
        .expect("argv");
        assert!(!argv.iter().any(|arg| arg == "--drive-type"));
    }

    #[test]
    fn server_argv_rejects_invalid_drive_type() {
        assert!(AliyundriveWebdavEngine::server_argv(
            Path::new("/run/user/1000/mount-fs/webdav-pan"),
            12345,
            false,
            None,
            Some("bogus"),
        )
        .is_err());
    }

    #[test]
    fn rclone_argv_uses_config() {
        let argv = AliyundriveWebdavEngine::rclone_argv(
            Path::new("/run/user/1000/mount-fs/webdav-pan/rclone.conf"),
            Path::new("/home/u/mnt/pan"),
            true,
        );
        assert_eq!(argv[0], "mount");
        assert!(argv.contains(&"--config".to_string()));
        assert!(argv.contains(&"--vfs-cache-mode".to_string()));
        assert!(argv.contains(&"--allow-other".to_string()));
        assert!(argv.contains(&"--no-update-modtime".to_string()));
    }

    #[test]
    fn rclone_conf_uses_obscured_password() {
        let conf = AliyundriveWebdavEngine::render_rclone_conf(8080, "user1", "obscured-x");
        assert!(conf.contains("[mountfs]"));
        assert!(conf.contains("url = http://127.0.0.1:8080/"));
        assert!(conf.contains("pass = obscured-x"));
        assert!(!conf.contains("plaintext"));
    }

    #[test]
    fn random_hex_is_hex_of_requested_length() {
        let value = random_hex(8);
        assert_eq!(value.len(), 16);
        assert!(value.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
