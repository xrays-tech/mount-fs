//! Release download, archive extraction, and user-local binary installation.
//!
//! The network call is deliberately thin ([`download`]) so tests never need
//! network access; extraction and installation are pure filesystem work.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{AppError, Result};

/// Recognized release archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.tar.gz` / `.tgz`.
    TarGz,
    /// `.tar.xz` / `.txz`.
    TarXz,
    /// `.zip`.
    Zip,
}

impl ArchiveKind {
    /// Infer the archive format from a filename.
    pub fn from_filename(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Some(ArchiveKind::TarGz)
        } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
            Some(ArchiveKind::TarXz)
        } else if name.ends_with(".zip") {
            Some(ArchiveKind::Zip)
        } else {
            None
        }
    }
}

/// Download `url` to `dest`, writing to a temp file then renaming.
pub fn download(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let response = agent
        .get(url)
        .call()
        .map_err(|err| AppError::Network(format!("GET {url}: {err}")))?;
    let mut reader = response.into_body().into_reader();

    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let temp = dest.with_file_name(format!(".{name}.part-{}", std::process::id()));
    let mut file = File::create(&temp)?;
    io::copy(&mut reader, &mut file)?;
    file.flush()?;
    file.sync_all()?;

    if let Err(err) = fs::rename(&temp, dest) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    Ok(())
}

/// Extract `archive` into `dest_dir`, returning the extracted files.
pub fn extract(archive: &Path, kind: ArchiveKind, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(dest_dir)?;
    match kind {
        ArchiveKind::TarGz => {
            let file = File::open(archive)?;
            let decoder = flate2::read::GzDecoder::new(file);
            extract_tar(decoder, dest_dir)
        }
        ArchiveKind::TarXz => {
            let file = File::open(archive)?;
            let decoder = xz2::read::XzDecoder::new(file);
            extract_tar(decoder, dest_dir)
        }
        ArchiveKind::Zip => extract_zip(archive, dest_dir),
    }
}

fn extract_tar<R: Read>(reader: R, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut archive = tar::Archive::new(reader);
    let mut files = Vec::new();

    for entry in archive
        .entries()
        .map_err(|err| AppError::Archive(err.to_string()))?
    {
        let mut entry = entry.map_err(|err| AppError::Archive(err.to_string()))?;
        let path = entry
            .path()
            .map_err(|err| AppError::Archive(err.to_string()))?
            .into_owned();
        // `unpack_in` refuses path traversal outside `dest_dir`.
        if entry.unpack_in(dest_dir)? && !path.as_os_str().is_empty() {
            files.push(dest_dir.join(path));
        }
    }

    files.retain(|path| path.is_file());
    Ok(files)
}

fn extract_zip(archive: &Path, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| AppError::Archive(err.to_string()))?;
    let mut files = Vec::new();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|err| AppError::Archive(err.to_string()))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let out_path = dest_dir.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&out_path)?;
        io::copy(&mut entry, &mut out)?;
        files.push(out_path);
    }

    Ok(files)
}

/// Default user-local binary directory (`~/.local/bin`).
pub fn default_bin_dir() -> PathBuf {
    etcetera::home_dir()
        .map(|home| home.join(".local").join("bin"))
        .unwrap_or_else(|_| PathBuf::from(".local/bin"))
}

/// Install `src` into `bin_dir` with mode 0755, returning the installed path.
///
/// The binary is copied to a temporary name in the destination directory and
/// then renamed into place, so a crash never leaves a partial executable.
pub fn install_binary(src: &Path, bin_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| AppError::Archive("archive contains a nameless binary".to_string()))?;
    let dest = bin_dir.join(name);
    let temp = bin_dir.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));

    if let Err(err) = fs::copy(src, &temp) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;

    if let Err(err) = fs::rename(&temp, &dest) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn make_tar_gz(path: &Path, name: &str, contents: &[u8]) {
        let file = File::create(path).expect("create tar.gz");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, contents)
            .expect("append");
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    fn make_zip(path: &Path, name: &str, contents: &[u8]) {
        let file = File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(name, options).expect("zip entry");
        zip.write_all(contents).expect("zip write");
        zip.finish().expect("zip finish");
    }

    #[test]
    fn detects_archive_kinds() {
        assert_eq!(
            ArchiveKind::from_filename("x.tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_filename("x.tgz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_filename("x.tar.xz"),
            Some(ArchiveKind::TarXz)
        );
        assert_eq!(ArchiveKind::from_filename("x.zip"), Some(ArchiveKind::Zip));
        assert_eq!(ArchiveKind::from_filename("x.bin"), None);
    }

    #[test]
    fn extracts_tar_gz_and_installs_binary() {
        let temp = tempdir();
        let archive = temp.path().join("tool.tar.gz");
        make_tar_gz(&archive, "tool", b"#!/bin/sh\necho hi\n");

        let dest = temp.path().join("extract");
        let files = extract(&archive, ArchiveKind::TarGz, &dest).expect("extract");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("tool"));

        let bin_dir = temp.path().join("bin");
        let installed = install_binary(&files[0], &bin_dir).expect("install");
        assert_eq!(installed, bin_dir.join("tool"));
        let mode = fs::metadata(&installed)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn extracts_zip() {
        let temp = tempdir();
        let archive = temp.path().join("tool.zip");
        make_zip(&archive, "tool", b"binary");

        let files = extract(&archive, ArchiveKind::Zip, &temp.path().join("out")).expect("extract");
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(&files[0]).expect("read"), b"binary");
    }
}
