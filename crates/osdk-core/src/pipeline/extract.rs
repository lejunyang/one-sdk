//! Archive extraction: tar.gz / tar.xz / tar.zst / zip, with optional stripping
//! of a single top-level directory (node/go/python archives wrap everything in
//! one root dir like `node-v20-linux-x64/`).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    TarXz,
    TarZst,
    Zip,
}

impl ArchiveKind {
    /// Guess the archive kind from a filename/URL.
    pub fn from_name(name: &str) -> Result<ArchiveKind> {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
            Ok(ArchiveKind::TarGz)
        } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
            Ok(ArchiveKind::TarXz)
        } else if n.ends_with(".tar.zst") || n.ends_with(".tzst") {
            Ok(ArchiveKind::TarZst)
        } else if n.ends_with(".zip") {
            Ok(ArchiveKind::Zip)
        } else {
            Err(Error::UnsupportedArchive(name.to_string()))
        }
    }
}

/// Extract `archive` into `dest`. If `strip_root` is true and the archive has a
/// single top-level directory, its contents are lifted up one level.
pub fn extract(archive: &Path, dest: &Path, kind: ArchiveKind, strip_root: bool) -> Result<()> {
    create_dir_all(dest)?;
    // Extract into a scratch dir first, then optionally strip the root while
    // moving into `dest`.
    let scratch = dest.join(".osdk-extract-tmp");
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    create_dir_all(&scratch)?;

    match kind {
        ArchiveKind::TarGz => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = flate2::read::GzDecoder::new(BufReader::new(f));
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::TarXz => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = xz2::read::XzDecoder::new(BufReader::new(f));
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::TarZst => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = zstd::stream::read::Decoder::new(BufReader::new(f))
                .map_err(|e| Error::io(archive, e))?;
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::Zip => {
            unpack_zip(archive, &scratch)?;
        }
    }

    // Move (with optional root strip) from scratch into dest.
    let source_root = if strip_root {
        match single_child_dir(&scratch)? {
            Some(child) => child,
            None => scratch.clone(),
        }
    } else {
        scratch.clone()
    };

    move_dir_contents(&source_root, dest)?;
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.unpack(dest).map_err(|e| Error::io(dest, e))?;
    Ok(())
}

fn unpack_zip(archive: &Path, dest: &Path) -> Result<()> {
    let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f))
        .map_err(|e| Error::other(format!("zip open: {e}")))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::other(format!("zip entry {i}: {e}")))?;
        let out_path = match entry.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue,
        };
        if entry.is_dir() {
            create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                create_dir_all(parent)?;
            }
            let mut out = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| Error::io(&out_path, e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}

/// If `dir` contains exactly one entry and it is a directory, return it.
fn single_child_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let mut children = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        children.push(entry.path());
    }
    if children.len() == 1 && children[0].is_dir() {
        Ok(Some(children.remove(0)))
    } else {
        Ok(None)
    }
}

/// Move everything inside `from` into `to` (merging). Uses rename when possible.
fn move_dir_contents(from: &Path, to: &Path) -> Result<()> {
    create_dir_all(to)?;
    for entry in std::fs::read_dir(from).map_err(|e| Error::io(from, e))? {
        let entry = entry.map_err(|e| Error::io(from, e))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        // Skip our own scratch dir if source_root == dest (strip_root == false path)
        if src == *to {
            continue;
        }
        if dst.exists() {
            let _ = std::fs::remove_dir_all(&dst);
            let _ = std::fs::remove_file(&dst);
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => {}
            Err(_) => {
                // cross-dir fallback: recursive copy
                copy_recursive(&src, &dst)?;
            }
        }
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        create_dir_all(dst)?;
        for entry in std::fs::read_dir(src).map_err(|e| Error::io(src, e))? {
            let entry = entry.map_err(|e| Error::io(src, e))?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst).map_err(|e| Error::io(dst, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_name() {
        assert_eq!(ArchiveKind::from_name("x.tar.gz").unwrap(), ArchiveKind::TarGz);
        assert_eq!(ArchiveKind::from_name("x.tar.xz").unwrap(), ArchiveKind::TarXz);
        assert_eq!(ArchiveKind::from_name("x.tar.zst").unwrap(), ArchiveKind::TarZst);
        assert_eq!(ArchiveKind::from_name("x.zip").unwrap(), ArchiveKind::Zip);
        assert!(ArchiveKind::from_name("x.rar").is_err());
    }

    #[test]
    fn extract_targz_with_root_strip() {
        // Build a tar.gz with a single root dir: pkg/bin/tool, pkg/README
        let td = tempfile::tempdir().unwrap();
        let archive = td.path().join("a.tar.gz");
        {
            let f = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut add = |name: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, data).unwrap();
            };
            add("pkg/README", b"hi");
            add("pkg/bin/tool", b"binary");
            b.finish().unwrap();
        }
        let dest = td.path().join("out");
        extract(&archive, &dest, ArchiveKind::TarGz, true).unwrap();
        // root "pkg/" should be stripped
        assert!(dest.join("README").exists());
        assert!(dest.join("bin/tool").exists());
        assert_eq!(std::fs::read(dest.join("README")).unwrap(), b"hi");
    }
}
