//! Package-manager detection and command construction.
//!
//! Detection prefers what is actually installed on `PATH`; the distro-family
//! fallback is only a hint used when no manager binary is found.

use std::fmt;
use std::process::{Command, Stdio};

use crate::system::distro::DistroFamily;

/// A supported system package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// `dnf` (Fedora / RHEL family).
    Dnf,
    /// `apt` (Debian / Ubuntu).
    Apt,
    /// `pacman` (Arch).
    Pacman,
    /// `zypper` (SUSE).
    Zypper,
    /// `apk` (Alpine).
    Apk,
    /// Unknown / none detected.
    Unknown,
}

impl fmt::Display for PackageManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            PackageManager::Dnf => "dnf",
            PackageManager::Apt => "apt",
            PackageManager::Pacman => "pacman",
            PackageManager::Zypper => "zypper",
            PackageManager::Apk => "apk",
            PackageManager::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

impl PackageManager {
    /// Detect the active package manager by probing `PATH`.
    pub fn detect() -> Self {
        const CANDIDATES: &[(&str, PackageManager)] = &[
            ("dnf", PackageManager::Dnf),
            ("yum", PackageManager::Dnf),
            ("apt", PackageManager::Apt),
            ("apt-get", PackageManager::Apt),
            ("pacman", PackageManager::Pacman),
            ("zypper", PackageManager::Zypper),
            ("apk", PackageManager::Apk),
        ];
        for (binary, manager) in CANDIDATES {
            if which_exists(binary) {
                return *manager;
            }
        }
        PackageManager::Unknown
    }

    /// Best-effort mapping from a detected distro family.
    pub fn from_distro_family(family: DistroFamily) -> Self {
        match family {
            DistroFamily::Fedora | DistroFamily::Rhel => PackageManager::Dnf,
            DistroFamily::Debian => PackageManager::Apt,
            DistroFamily::Arch => PackageManager::Pacman,
            DistroFamily::Suse => PackageManager::Zypper,
            DistroFamily::Alpine => PackageManager::Apk,
            DistroFamily::Unknown => PackageManager::Unknown,
        }
    }

    /// Build the argv that installs `packages`, or `None` if unsupported.
    pub fn install_argv(&self, packages: &[&str]) -> Option<Vec<String>> {
        let mut argv: Vec<String> = match self {
            PackageManager::Dnf => vec!["dnf".into(), "install".into(), "-y".into()],
            PackageManager::Apt => vec!["apt".into(), "install".into(), "-y".into()],
            PackageManager::Pacman => vec!["pacman".into(), "-S".into(), "--noconfirm".into()],
            PackageManager::Zypper => vec!["zypper".into(), "-n".into(), "install".into()],
            PackageManager::Apk => vec!["apk".into(), "add".into()],
            PackageManager::Unknown => return None,
        };
        argv.extend(packages.iter().map(|p| (*p).to_string()));
        Some(argv)
    }
}

/// Whether a package appears installed according to the given manager.
pub fn is_installed(manager: PackageManager, package: &str) -> bool {
    let (program, args): (&str, &[&str]) = match manager {
        PackageManager::Dnf | PackageManager::Zypper => ("rpm", &["-q", package]),
        PackageManager::Apt => ("dpkg", &["-s", package]),
        PackageManager::Pacman => ("pacman", &["-Q", package]),
        PackageManager::Apk => ("apk", &["info", "-e", package]),
        PackageManager::Unknown => return false,
    };
    status_success(program, args)
}

/// Run a probe command, discarding output, and report success.
fn status_success(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether `program` resolves on `PATH`.
fn which_exists(program: &str) -> bool {
    which::which(program).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnf_install_argv() {
        let argv = PackageManager::Dnf
            .install_argv(&["s3fs-fuse", "fuse3"])
            .unwrap();
        assert_eq!(argv, vec!["dnf", "install", "-y", "s3fs-fuse", "fuse3"]);
    }

    #[test]
    fn apt_install_argv() {
        let argv = PackageManager::Apt.install_argv(&["s3fs-fuse"]).unwrap();
        assert_eq!(argv, vec!["apt", "install", "-y", "s3fs-fuse"]);
    }

    #[test]
    fn pacman_install_argv() {
        let argv = PackageManager::Pacman.install_argv(&["s3fs-fuse"]).unwrap();
        assert_eq!(argv, vec!["pacman", "-S", "--noconfirm", "s3fs-fuse"]);
    }

    #[test]
    fn zypper_install_argv() {
        let argv = PackageManager::Zypper.install_argv(&["s3fs-fuse"]).unwrap();
        assert_eq!(argv, vec!["zypper", "-n", "install", "s3fs-fuse"]);
    }

    #[test]
    fn apk_install_argv() {
        let argv = PackageManager::Apk.install_argv(&["s3fs-fuse"]).unwrap();
        assert_eq!(argv, vec!["apk", "add", "s3fs-fuse"]);
    }

    #[test]
    fn unknown_has_no_argv() {
        assert_eq!(PackageManager::Unknown.install_argv(&["x"]), None);
    }

    #[test]
    fn fallback_mapping() {
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Fedora),
            PackageManager::Dnf
        );
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Debian),
            PackageManager::Apt
        );
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Arch),
            PackageManager::Pacman
        );
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Suse),
            PackageManager::Zypper
        );
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Alpine),
            PackageManager::Apk
        );
        assert_eq!(
            PackageManager::from_distro_family(DistroFamily::Unknown),
            PackageManager::Unknown
        );
    }

    #[test]
    fn unknown_is_not_installed() {
        assert!(!is_installed(
            PackageManager::Unknown,
            "mount-fs-not-a-real-package"
        ));
    }
}
