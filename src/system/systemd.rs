//! systemd user-unit generation and management.
//!
//! We intentionally use a `.service` (not `.mount`) so the engine can run in
//! the foreground with a persistent credential reference.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use etcetera::{choose_base_strategy, BaseStrategy};
use serde::Serialize;

use crate::error::{AppError, Result};

/// Prefix used for generated unit files.
pub const UNIT_PREFIX: &str = "mount-fs-";

/// A generated (or discovered) user unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceUnit {
    /// Profile name.
    pub profile: String,
    /// Unit file path.
    pub path: PathBuf,
}

/// Directory for systemd user units (`$XDG_CONFIG_HOME/systemd/user`).
pub fn unit_dir() -> Result<PathBuf> {
    let strategy = choose_base_strategy()
        .map_err(|err| AppError::Config(format!("cannot determine config directory: {err}")))?;
    Ok(strategy.config_dir().join("systemd").join("user"))
}

/// Unit file path for a profile.
pub fn unit_path(profile: &str) -> Result<PathBuf> {
    Ok(unit_dir()?.join(format!("{UNIT_PREFIX}{}.service", sanitize(profile))))
}

/// Render a systemd user unit (pure).
pub fn render_unit(profile: &str, mountpoint: &Path, exec: &Path) -> String {
    format!(
        "\
[Unit]
Description=mount-fs: {profile}
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
ExecStart={exec} mount {profile} --foreground
ExecStopPost=-fusermount3 -u {mountpoint}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
",
        exec = exec.display(),
        mountpoint = mountpoint.display(),
    )
}

/// Write (or dry-run) the unit for a profile.
pub fn generate(
    profile: &str,
    mountpoint: &Path,
    exec: &Path,
    force: bool,
    dry_run: bool,
) -> Result<PathBuf> {
    let path = unit_path(profile)?;
    if path.exists() && !force {
        return Err(AppError::Config(format!(
            "unit {} already exists; use --force to overwrite",
            path.display()
        )));
    }

    let contents = render_unit(profile, mountpoint, exec);
    if dry_run {
        print!("{contents}");
        println!("Dry run: would write {}", path.display());
        return Ok(path);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    fs::write(&path, contents)?;
    println!("Wrote {}", path.display());
    Ok(path)
}

/// List generated `mount-fs-*` units, sorted by profile.
pub fn list_units() -> Result<Vec<ServiceUnit>> {
    let dir = unit_dir()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut units = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(UNIT_PREFIX) else {
            continue;
        };
        let Some(profile) = rest.strip_suffix(".service") else {
            continue;
        };
        units.push(ServiceUnit {
            profile: profile.to_string(),
            path: entry.path(),
        });
    }
    units.sort_by(|a, b| a.profile.cmp(&b.profile));
    Ok(units)
}

/// Remove a generated unit.
pub fn remove(profile: &str) -> Result<PathBuf> {
    let path = unit_path(profile)?;
    if !path.exists() {
        return Err(AppError::NotFound(format!(
            "service unit {}",
            path.display()
        )));
    }
    fs::remove_file(&path)?;
    Ok(path)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_expected_unit() {
        let unit = render_unit(
            "pan",
            Path::new("/home/u/mnt/pan"),
            Path::new("/usr/local/bin/mount-fs"),
        );
        assert!(unit.contains("Description=mount-fs: pan"));
        assert!(unit.contains("ExecStart=/usr/local/bin/mount-fs mount pan --foreground"));
        assert!(unit.contains("ExecStopPost=-fusermount3 -u /home/u/mnt/pan"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("RestartSec=5"));
        assert!(unit.contains("StartLimitIntervalSec=60"));
        assert!(unit.contains("StartLimitBurst=5"));
    }

    #[test]
    fn render_expands_tilde_before_call() {
        // The caller expands `~`; render must emit the absolute path.
        let expanded =
            crate::config::expand_tilde_with_home(Path::new("~/mnt/pan"), Path::new("/home/u"));
        let unit = render_unit("pan", &expanded, Path::new("/bin/mount-fs"));
        assert!(unit.contains("-u /home/u/mnt/pan"));
        assert!(!unit.contains("~"));
    }

    #[test]
    fn unit_path_uses_config_dir() {
        let path = unit_path("pan").expect("unit path");
        assert!(path
            .to_string_lossy()
            .ends_with("systemd/user/mount-fs-pan.service"));
    }
}
