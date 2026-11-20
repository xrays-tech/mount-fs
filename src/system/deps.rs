//! Runtime dependency model (binaries, devices, package hints).

use std::path::Path;

use serde::Serialize;

/// How a dependency is detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DepCheck {
    /// Executable looked up on `PATH`.
    Binary(&'static str),
    /// Filesystem device/path that must exist.
    Device(&'static str),
}

/// A runtime dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Dependency {
    /// Stable identifier.
    pub id: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Detection method.
    pub check: DepCheck,
    /// Package names that provide it (empty for kernel devices).
    pub packages: &'static [&'static str],
}

impl Dependency {
    /// Whether the dependency is currently satisfied.
    pub fn present(&self) -> bool {
        match self.check {
            DepCheck::Binary(name) => which::which(name).is_ok(),
            DepCheck::Device(path) => Path::new(path).exists(),
        }
    }

    /// Human-readable detail for doctor/deps output.
    pub fn detail(&self) -> String {
        match self.check {
            DepCheck::Binary(name) => {
                if let Ok(path) = which::which(name) {
                    format!("{name} ({})", path.display())
                } else {
                    format!("{name} not found on PATH")
                }
            }
            DepCheck::Device(path) => {
                if Path::new(path).exists() {
                    format!("{path} present")
                } else {
                    format!("{path} missing")
                }
            }
        }
    }
}

/// Status of a checked dependency.
#[derive(Debug, Clone, Serialize)]
pub struct DepStatus {
    /// The dependency.
    pub dependency: Dependency,
    /// Whether it is present.
    pub present: bool,
    /// Detail string.
    pub detail: String,
}

/// Check a list of dependencies.
pub fn check(deps: &[Dependency]) -> Vec<DepStatus> {
    deps.iter()
        .map(|dependency| DepStatus {
            dependency: *dependency,
            present: dependency.present(),
            detail: dependency.detail(),
        })
        .collect()
}

/// Dependencies required by every engine.
pub fn common_deps() -> Vec<Dependency> {
    vec![
        Dependency {
            id: "fuse-device",
            description: "FUSE device",
            check: DepCheck::Device("/dev/fuse"),
            packages: &[],
        },
        Dependency {
            id: "fusermount3",
            description: "FUSE 3 unmount helper",
            check: DepCheck::Binary("fusermount3"),
            packages: &["fuse3"],
        },
    ]
}

/// The `rclone` dependency (needed by the webdav bridge, shown by doctor).
pub fn rclone_dependency() -> Dependency {
    Dependency {
        id: "rclone",
        description: "rclone (needed for aliyundrive-webdav)",
        check: DepCheck::Binary("rclone"),
        packages: &["rclone"],
    }
}

/// Dependencies for a specific engine.
pub fn engine_deps(engine_id: &str) -> Vec<Dependency> {
    let mut deps = common_deps();
    if engine_id == "aliyundrive-webdav" {
        deps.push(rclone_dependency());
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_present_and_absent_binaries() {
        let present = Dependency {
            id: "sh",
            description: "shell",
            check: DepCheck::Binary("sh"),
            packages: &[],
        };
        assert!(present.present());

        let absent = Dependency {
            id: "missing",
            description: "missing",
            check: DepCheck::Binary("mount-fs-definitely-not-a-real-binary"),
            packages: &[],
        };
        assert!(!absent.present());
        assert!(absent.detail().contains("not found"));
    }

    #[test]
    fn detects_devices() {
        let present = Dependency {
            id: "null",
            description: "null device",
            check: DepCheck::Device("/dev/null"),
            packages: &[],
        };
        assert!(present.present());

        let absent = Dependency {
            id: "missing-device",
            description: "missing device",
            check: DepCheck::Device("/no/such/mount-fs-device"),
            packages: &[],
        };
        assert!(!absent.present());
    }

    #[test]
    fn webdav_deps_include_rclone() {
        let webdav = engine_deps("aliyundrive-webdav");
        assert!(webdav.iter().any(|dep| dep.id == "rclone"));
        assert!(webdav.iter().any(|dep| dep.id == "fusermount3"));

        let s3 = engine_deps("s3fs-fuse");
        assert!(!s3.iter().any(|dep| dep.id == "rclone"));
    }
}
