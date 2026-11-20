//! Distribution detection from `/etc/os-release`.
//!
//! The detected [`DistroFamily`] drives package-manager selection and
//! install-strategy decisions in later milestones.

use std::fmt;
use std::path::Path;

/// Path to the Linux OS release file.
pub const OS_RELEASE_PATH: &str = "/etc/os-release";

/// Coarse distribution family used to pick install strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistroFamily {
    /// Fedora Linux.
    Fedora,
    /// RHEL and derivatives (CentOS, Rocky, Alma, Oracle Linux).
    Rhel,
    /// Debian and derivatives (Ubuntu).
    Debian,
    /// Arch Linux and derivatives (Manjaro).
    Arch,
    /// openSUSE / SUSE Linux Enterprise.
    Suse,
    /// Alpine Linux.
    Alpine,
    /// Anything unrecognized.
    Unknown,
}

impl fmt::Display for DistroFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            DistroFamily::Fedora => "fedora",
            DistroFamily::Rhel => "rhel",
            DistroFamily::Debian => "debian",
            DistroFamily::Arch => "arch",
            DistroFamily::Suse => "suse",
            DistroFamily::Alpine => "alpine",
            DistroFamily::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

/// Parsed `/etc/os-release` fields relevant to install planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    /// `ID` value (e.g. `fedora`), or `unknown` when unavailable.
    pub id: String,
    /// `ID_LIKE` tokens.
    pub id_like: Vec<String>,
    /// `VERSION_ID` value, if present.
    pub version_id: Option<String>,
    /// `PRETTY_NAME` value, if present.
    pub pretty_name: Option<String>,
    /// Mapped distribution family.
    pub family: DistroFamily,
}

impl Distro {
    /// A placeholder used when `/etc/os-release` cannot be read.
    pub fn unknown() -> Self {
        Distro {
            id: "unknown".to_string(),
            id_like: Vec::new(),
            version_id: None,
            pretty_name: None,
            family: DistroFamily::Unknown,
        }
    }
}

/// Detect the running distribution by reading `/etc/os-release`.
///
/// Returns [`Distro::unknown`] if the file is missing or unreadable.
pub fn detect() -> Distro {
    detect_from(Path::new(OS_RELEASE_PATH))
}

/// Detect from a specific `os-release`-style path (test/injection hook).
pub fn detect_from(path: &Path) -> Distro {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse(&contents),
        Err(_) => Distro::unknown(),
    }
}

/// Parse `os-release` contents into a [`Distro`].
pub fn parse(contents: &str) -> Distro {
    let mut id = String::new();
    let mut id_like = Vec::new();
    let mut version_id = None;
    let mut pretty_name = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value);
        match key {
            "ID" => id = value,
            "ID_LIKE" => id_like = value.split_whitespace().map(str::to_string).collect(),
            "VERSION_ID" => version_id = Some(value),
            "PRETTY_NAME" => pretty_name = Some(value),
            _ => {}
        }
    }

    if id.is_empty() {
        id = "unknown".to_string();
    }
    let family = classify(&id, &id_like);

    Distro {
        id,
        id_like,
        version_id,
        pretty_name,
        family,
    }
}

/// Map an `ID`/`ID_LIKE` token to a family.
fn family_for_token(token: &str) -> Option<DistroFamily> {
    let token = token.to_ascii_lowercase();
    let family = match token.as_str() {
        "fedora" => DistroFamily::Fedora,
        "rhel" | "centos" | "rocky" | "alma" | "almalinux" | "ol" => DistroFamily::Rhel,
        "debian" | "ubuntu" => DistroFamily::Debian,
        "arch" | "manjaro" | "endeavouros" => DistroFamily::Arch,
        "sles" | "sled" | "suse" => DistroFamily::Suse,
        "alpine" => DistroFamily::Alpine,
        other if other.starts_with("opensuse") || other.starts_with("sles") => DistroFamily::Suse,
        _ => return None,
    };
    Some(family)
}

/// Classify from `ID` first, then each `ID_LIKE` token in order.
fn classify(id: &str, id_like: &[String]) -> DistroFamily {
    if let Some(family) = family_for_token(id) {
        return family;
    }
    for token in id_like {
        if let Some(family) = family_for_token(token) {
            return family;
        }
    }
    DistroFamily::Unknown
}

/// Strip matching single or double quotes from a value.
fn unquote(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fedora() {
        let contents = "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=45\nPRETTY_NAME=\"Fedora Linux 45 (Workstation Edition)\"\n";
        let distro = parse(contents);
        assert_eq!(distro.id, "fedora");
        assert_eq!(distro.version_id.as_deref(), Some("45"));
        assert_eq!(
            distro.pretty_name.as_deref(),
            Some("Fedora Linux 45 (Workstation Edition)")
        );
        assert_eq!(distro.family, DistroFamily::Fedora);
    }

    #[test]
    fn parses_ubuntu_via_id_like() {
        let contents = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n";
        let distro = parse(contents);
        assert_eq!(distro.id, "ubuntu");
        assert_eq!(distro.id_like, vec!["debian".to_string()]);
        assert_eq!(distro.family, DistroFamily::Debian);
    }

    #[test]
    fn parses_debian() {
        let contents = "ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n";
        assert_eq!(parse(contents).family, DistroFamily::Debian);
    }

    #[test]
    fn parses_arch() {
        let contents = "ID=arch\nPRETTY_NAME=\"Arch Linux\"\n";
        assert_eq!(parse(contents).family, DistroFamily::Arch);
    }

    #[test]
    fn parses_rocky_as_rhel() {
        let contents =
            "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nPRETTY_NAME=\"Rocky Linux 9.4\"\n";
        let distro = parse(contents);
        assert_eq!(distro.id, "rocky");
        assert_eq!(distro.family, DistroFamily::Rhel);
    }

    #[test]
    fn parses_alpine() {
        let contents = "ID=alpine\nPRETTY_NAME=\"Alpine Linux v3.20\"\n";
        assert_eq!(parse(contents).family, DistroFamily::Alpine);
    }

    #[test]
    fn parses_opensuse_as_suse() {
        let contents = "ID=\"opensuse-leap\"\nPRETTY_NAME=\"openSUSE Leap 15.6\"\n";
        assert_eq!(parse(contents).family, DistroFamily::Suse);
    }

    #[test]
    fn unknown_for_empty() {
        let distro = parse("");
        assert_eq!(distro.id, "unknown");
        assert_eq!(distro.family, DistroFamily::Unknown);
    }

    #[test]
    fn unknown_for_unrecognized_id() {
        let contents = "ID=plan9\nPRETTY_NAME=\"Plan 9\"\n";
        assert_eq!(parse(contents).family, DistroFamily::Unknown);
    }
}
