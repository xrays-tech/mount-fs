//! Host system adapters and the `doctor` diagnostics command.
//!
//! M1 provides distro/package-manager detection, privilege planning, FUSE
//! probing, mount-table parsing, process tracking, and advisory file locking.
//! The `doctor` report aggregates the environment checks.

pub mod checksums;
pub mod distro;
pub mod download;
pub mod fuse;
pub mod locking;
pub mod pkg;
pub mod privilege;
pub mod process;
pub mod supervisor;
pub mod systemd;

use std::path::Path;

use crate::error::Result;
use crate::system::distro::DistroFamily;
use crate::system::pkg::PackageManager;
use crate::system::privilege::Escalation;

/// Run the `doctor` report, printing one `[ OK ]`/`[WARN]` line per check.
pub fn doctor(fix: bool) -> Result<()> {
    let distro = distro::detect();
    report_distro(&distro);
    report_package_manager(distro.family);
    report_fuse();
    report_privilege();
    report_fuse_mounts();

    if fix {
        apply_fuse_fix();
    }
    Ok(())
}

/// Safe, minimal `doctor --fix`: only touches `/etc/fuse.conf`.
fn apply_fuse_fix() {
    let action = fuse::fuse_conf_fix_action(fuse::user_allow_other_set(), privilege::is_root());
    match action {
        fuse::FuseConfFix::None => println!("[ OK ] fuse.conf: user_allow_other already enabled"),
        fuse::FuseConfFix::Append => match fuse::append_user_allow_other(Path::new(fuse::FUSE_CONF)) {
            Ok(()) => println!("[ OK ] fuse.conf: appended user_allow_other"),
            Err(err) => println!("[WARN] fuse.conf: could not append user_allow_other ({err})"),
        },
        fuse::FuseConfFix::SuggestCommand => println!(
            "[WARN] fuse.conf: user_allow_other missing; run:\n  sudo sh -c 'echo user_allow_other >> /etc/fuse.conf'"
        ),
    }
}

fn report_distro(distro: &distro::Distro) {
    let name = distro.pretty_name.as_deref().unwrap_or(distro.id.as_str());
    let detail = format!("id={}, family={}", distro.id, distro.family);
    if distro.family == DistroFamily::Unknown {
        println!("[WARN] Distro: {name} ({detail})");
    } else {
        println!("[ OK ] Distro: {name} ({detail})");
    }
}

fn report_package_manager(family: DistroFamily) {
    let manager = PackageManager::detect();
    if manager == PackageManager::Unknown {
        let expected = PackageManager::from_distro_family(family);
        println!("[WARN] Package manager: not detected (expected {expected})");
    } else {
        println!("[ OK ] Package manager: {manager}");
    }
}

fn report_fuse() {
    if fuse::device_present() {
        println!("[ OK ] FUSE device: {} present", fuse::FUSE_DEVICE);
    } else {
        println!("[WARN] FUSE device: {} missing", fuse::FUSE_DEVICE);
    }

    match fuse::fusermount3_path() {
        Some(path) => println!("[ OK ] fusermount3: {}", path.display()),
        None => println!("[WARN] fusermount3: not found on PATH"),
    }

    match fuse::user_allow_other_set() {
        Some(true) => println!("[ OK ] fuse.conf: user_allow_other enabled"),
        Some(false) => println!(
            "[WARN] fuse.conf: user_allow_other not set ({} present)",
            fuse::FUSE_CONF
        ),
        None => println!(
            "[WARN] fuse.conf: {} not found; allow_other mounts require it",
            fuse::FUSE_CONF
        ),
    }
}

fn report_privilege() {
    if privilege::is_root() {
        println!("[ OK ] Privilege: running as root");
        return;
    }
    match privilege::choose_escalation() {
        Ok(Escalation::Sudo) => println!("[ OK ] Privilege: non-interactive sudo available"),
        Ok(Escalation::Pkexec) => println!("[ OK ] Privilege: pkexec available"),
        Ok(Escalation::None) => println!("[ OK ] Privilege: running as root"),
        Err(_) => println!("[WARN] Privilege: no sudo -n or pkexec escalation available"),
    }
}

fn report_fuse_mounts() {
    match fuse::list_mounts() {
        Ok(entries) => {
            let count = entries
                .iter()
                .filter(|entry| fuse::is_fuse_fstype(&entry.fstype))
                .count();
            println!("[ OK ] FUSE mounts: {count} active");
        }
        Err(err) => println!("[WARN] FUSE mounts: cannot read mountinfo ({err})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pretty_name() {
        let contents = "NAME=\"Fedora Linux\"\nID=fedora\nPRETTY_NAME=\"Fedora Linux 45 (Workstation Edition)\"\n";
        let parsed = distro::parse(contents);
        assert_eq!(
            parsed.pretty_name.as_deref(),
            Some("Fedora Linux 45 (Workstation Edition)")
        );
        assert_eq!(parsed.family, DistroFamily::Fedora);
    }

    #[test]
    fn falls_back_to_id_for_name() {
        let contents = "NAME='Example OS'\nID=example\n";
        let parsed = distro::parse(contents);
        assert_eq!(parsed.id, "example");
        assert_eq!(parsed.family, DistroFamily::Unknown);
    }

    #[test]
    fn reports_fuse_conf_state() {
        assert!(fuse::parse_fuse_conf("user_allow_other\n"));
        assert!(!fuse::parse_fuse_conf(""));
    }
}
