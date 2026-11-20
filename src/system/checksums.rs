//! Embedded checksum registry for release artifacts.
//!
//! Checksums are pinned at mount-fs release time (not fetched from the same
//! channel as the artifact) so a compromised mirror cannot silently swap a
//! binary. Update this table when bumping a bundled engine.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

/// Look up the pinned sha256 for `engine`/`version`/`arch`.
///
/// `arch` is a Rust architecture string (`x86_64`, `aarch64`).
pub fn lookup(engine: &str, version: &str, arch: &str) -> Option<&'static str> {
    match (engine, version, arch) {
        ("aliyundrive-fuse", "0.1.14", "x86_64") => {
            Some("039129ceba4c5492a22557803dea7ec66b93adb0e1a6809e35275b36670b86e2")
        }
        ("aliyundrive-fuse", "0.1.14", "aarch64") => {
            Some("8e39d89788954d7bea99abca3ecf9e5a56febc7791ccf3d27c8b30a4eb661424")
        }
        ("qrypt", "0.7.0", "x86_64") => {
            Some("93187ae6f4e280cf7f5f1a6b757de0a6c7792354cdfa1f25652e0dea560ff99e")
        }
        ("qrypt", "0.7.0", "aarch64") => {
            Some("b55f4d4bc9e091684f047e87f5dab5fca7a0edc2cdf6fb7dbb408de176a11489")
        }
        ("aliyundrive-webdav", "2.3.3", "x86_64") => {
            Some("768122795280e54a818d74d47c6ae71ba8584f0d0c41193bf1d959cf5d3b6cb8")
        }
        ("aliyundrive-webdav", "2.3.3", "aarch64") => {
            Some("10abbe819b02b538aa9c65c5bad07be04209bdf507f3b1a40525b58b015f9e53")
        }
        _ => None,
    }
}

/// Hex-encode the sha256 of `path`.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// Verify that `path` matches `expected` (case-insensitive hex).
pub fn verify(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(AppError::Integrity(format!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_seeded_hashes() {
        assert!(lookup("aliyundrive-fuse", "0.1.14", "x86_64").is_some());
        assert!(lookup("qrypt", "0.7.0", "aarch64").is_some());
        assert!(lookup("aliyundrive-webdav", "2.3.3", "x86_64").is_some());
        assert!(lookup("qrypt", "9.9.9", "x86_64").is_none());
        assert!(lookup("nope", "0.7.0", "x86_64").is_none());
    }

    #[test]
    fn verify_accepts_correct_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("file");
        std::fs::write(&path, b"hello").expect("write");
        // sha256("hello")
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify(&path, expected).expect("verify");
        verify(&path, &expected.to_uppercase()).expect("verify uppercase");
    }

    #[test]
    fn verify_rejects_wrong_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("archive.tar.gz");
        std::fs::write(&path, b"download contents").expect("write");
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify(&path, wrong).expect_err("should reject");
        assert!(matches!(err, AppError::Integrity(_)));
    }
}
