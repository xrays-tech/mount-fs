//! FUSE probing and `/proc/self/mountinfo` parsing.

use std::io;
use std::path::{Path, PathBuf};

/// Path to the FUSE device.
pub const FUSE_DEVICE: &str = "/dev/fuse";

/// Path to the FUSE configuration file.
pub const FUSE_CONF: &str = "/etc/fuse.conf";

/// Path to the mount table.
pub const MOUNTINFO: &str = "/proc/self/mountinfo";

/// A single parsed `/proc/self/mountinfo` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Mount ID (field 1).
    pub mount_id: u64,
    /// Parent mount ID (field 2).
    pub parent_id: u64,
    /// Mount source after the `-` separator.
    pub source: String,
    /// Mount target (unescaped).
    pub target: PathBuf,
    /// Filesystem type after the `-` separator.
    pub fstype: String,
    /// Per-mount options (field 6), comma-split.
    pub options: Vec<String>,
}

/// Whether `/dev/fuse` exists.
pub fn device_present() -> bool {
    Path::new(FUSE_DEVICE).exists()
}

/// Resolve `fusermount3` on `PATH`.
pub fn fusermount3_path() -> Option<PathBuf> {
    which::which("fusermount3").ok()
}

/// Report whether `/etc/fuse.conf` enables `user_allow_other`.
///
/// Returns `None` when the file does not exist / cannot be read.
pub fn user_allow_other_set() -> Option<bool> {
    let contents = std::fs::read_to_string(FUSE_CONF).ok()?;
    Some(parse_fuse_conf(&contents))
}

/// Parse `fuse.conf` contents, ignoring comments and blank lines.
pub fn parse_fuse_conf(contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        line == "user_allow_other"
    })
}

/// Whether a filesystem type string denotes a FUSE mount.
pub fn is_fuse_fstype(fstype: &str) -> bool {
    fstype == "fuse" || fstype == "fuseblk" || fstype.starts_with("fuse.")
}

/// A source of mount-table entries.
///
/// This abstraction lets the orchestrator be tested hermetically without a real
/// FUSE mount.
pub trait MountSource: Send + Sync {
    /// List current mounts.
    fn list(&self) -> io::Result<Vec<MountEntry>>;
}

/// The real mount source, backed by `/proc/self/mountinfo`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealMountSource;

impl MountSource for RealMountSource {
    fn list(&self) -> io::Result<Vec<MountEntry>> {
        let contents = std::fs::read_to_string(MOUNTINFO)?;
        Ok(parse_mountinfo(&contents))
    }
}

/// Read and parse `/proc/self/mountinfo`.
pub fn list_mounts() -> io::Result<Vec<MountEntry>> {
    RealMountSource.list()
}

/// Parse `/proc/self/mountinfo` contents.
pub fn parse_mountinfo(contents: &str) -> Vec<MountEntry> {
    contents.lines().filter_map(parse_mountinfo_line).collect()
}

fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let mut fields = line.split_whitespace();

    let mount_id = fields.next()?.parse().ok()?;
    let parent_id = fields.next()?.parse().ok()?;
    let _major_minor = fields.next()?;
    let _root = fields.next()?;
    let target = PathBuf::from(unescape_octal(fields.next()?));
    let options = fields
        .next()?
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();

    let mut fstype = String::new();
    let mut source = String::new();
    let mut found_separator = false;
    for field in fields.by_ref() {
        if field == "-" {
            found_separator = true;
            break;
        }
    }
    if found_separator {
        fstype = fields.next()?.to_string();
        source = unescape_octal(fields.next()?);
    }

    Some(MountEntry {
        mount_id,
        parent_id,
        source,
        target,
        fstype,
        options,
    })
}

/// Decode the octal escapes used by the kernel in mount paths (`\040`, ...).
fn unescape_octal(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "36 35 0:32 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw\n",
        "410 25 0:50 / /home/u/mnt/pan rw,nosuid,nodev,relatime shared:220 - fuse.aliyundrive aliyundrive rw,user_id=1000,group_id=1000\n",
        "411 25 0:51 / /home/u/bucket rw,nosuid,nodev,relatime shared:221 - fuse.s3fs mybucket rw,user_id=1000\n",
        "412 25 0:52 / /home/u/my\\040mnt rw,nosuid,nodev,relatime shared:222 - fuse.s3fs mybucket rw\n",
    );

    #[test]
    fn parses_mountinfo_entries() {
        let entries = parse_mountinfo(SAMPLE);
        assert_eq!(entries.len(), 4);

        let sysfs = &entries[0];
        assert_eq!(sysfs.mount_id, 36);
        assert_eq!(sysfs.parent_id, 35);
        assert_eq!(sysfs.fstype, "sysfs");
        assert_eq!(sysfs.target, PathBuf::from("/sys"));
        assert_eq!(
            sysfs.options,
            vec!["rw", "nosuid", "nodev", "noexec", "relatime"]
        );

        let aliyun = &entries[1];
        assert_eq!(aliyun.fstype, "fuse.aliyundrive");
        assert_eq!(aliyun.source, "aliyundrive");
        assert_eq!(aliyun.target, PathBuf::from("/home/u/mnt/pan"));
        assert!(is_fuse_fstype(&aliyun.fstype));

        let s3 = &entries[2];
        assert_eq!(s3.fstype, "fuse.s3fs");
        assert_eq!(s3.source, "mybucket");
        assert_eq!(s3.target, PathBuf::from("/home/u/bucket"));
        assert!(is_fuse_fstype(&s3.fstype));
    }

    #[test]
    fn unescapes_spaces_in_target() {
        let entries = parse_mountinfo(SAMPLE);
        assert_eq!(entries[3].target, PathBuf::from("/home/u/my mnt"));
    }

    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_mountinfo("garbage\n\n").is_empty());
        // Too few fields to reach the target/options.
        assert!(parse_mountinfo("1 2 0:1\n").is_empty());
        // Missing the `-` separator still yields an entry (empty fstype/source).
        let entries = parse_mountinfo("1 2 0:1 / / x\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fstype, "");
    }

    #[test]
    fn detects_fuse_fstypes() {
        assert!(is_fuse_fstype("fuse"));
        assert!(is_fuse_fstype("fuse.s3fs"));
        assert!(is_fuse_fstype("fuse.aliyundrive"));
        assert!(is_fuse_fstype("fuseblk"));
        assert!(is_fuse_fstype("fuse.rclone"));
        assert!(!is_fuse_fstype("ext4"));
    }

    #[test]
    fn parses_fuse_conf_user_allow_other() {
        let contents = "# comment\n\nuser_allow_other\n";
        assert!(parse_fuse_conf(contents));
        assert!(parse_fuse_conf("user_allow_other # inline"));
        assert!(!parse_fuse_conf("# user_allow_other\nmount_max = 1000\n"));
        assert!(!parse_fuse_conf(""));
    }

    #[test]
    fn octal_unescape_handles_tabs_and_backslashes() {
        assert_eq!(unescape_octal("/a\\040b"), "/a b");
        assert_eq!(unescape_octal("/a\\011b"), "/a\tb");
        assert_eq!(unescape_octal("/a\\134b"), "/a\\b");
        assert_eq!(unescape_octal("/plain"), "/plain");
    }
}
