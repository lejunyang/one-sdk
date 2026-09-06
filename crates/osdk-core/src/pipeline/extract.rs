//! Archive extraction: tar.gz / tar.xz / tar.zst / zip / 7z, with optional
//! stripping of a single top-level directory (node/go/python archives wrap
//! everything in one root dir like `node-v20-linux-x64/`).
//!
//! `.7z` is only compiled into the install path. Unpacking one costs a second
//! LZMA implementation, and the shim never extracts anything, so both the
//! dependency and the `SevenZ` variant sit behind the `install` feature.

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
    #[cfg(feature = "install")]
    SevenZ,
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
        } else if n.ends_with(".7z") {
            // Recognized even without the `install` feature so the shim reports
            // an unsupported archive rather than a confusing parse failure.
            #[cfg(feature = "install")]
            {
                Ok(ArchiveKind::SevenZ)
            }
            #[cfg(not(feature = "install"))]
            {
                Err(Error::UnsupportedArchive(name.to_string()))
            }
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
        #[cfg(feature = "install")]
        ArchiveKind::SevenZ => {
            unpack_7z(archive, &scratch)?;
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
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}

/// Unpack a `.7z` into `dest`.
///
/// Entry paths come from the archive, so each one is checked to stay inside
/// `dest` before anything is written: a `..` component or an absolute path would
/// otherwise let an archive write outside the extraction scratch directory.
#[cfg(feature = "install")]
fn unpack_7z(archive: &Path, dest: &Path) -> Result<()> {
    let dest = dest.to_path_buf();
    sevenz_rust2::decompress_file_with_extract_fn(archive, &dest, |entry, reader, _unused| {
        let Some(relative) = safe_relative_path(entry.name()) else {
            // Skip rather than abort: mirrors how the zip path ignores entries
            // whose names escape the destination.
            return Ok(true);
        };
        let out_path = dest.join(relative);
        if entry.is_directory() {
            create_dir_all(&out_path).map_err(sevenz_error)?;
            return Ok(true);
        }
        if let Some(parent) = out_path.parent() {
            create_dir_all(parent).map_err(sevenz_error)?;
        }
        let mut out = File::create(&out_path).map_err(|e| {
            sevenz_rust2::Error::Io(e, format!("creating {}", out_path.display()).into())
        })?;
        std::io::copy(reader, &mut out).map_err(|e| {
            sevenz_rust2::Error::Io(e, format!("writing {}", out_path.display()).into())
        })?;
        Ok(true)
    })
    .map_err(|e| Error::other(format!("7z extract: {e}")))?;
    Ok(())
}

/// Convert an archive-supplied entry name into a path that cannot escape the
/// destination, or `None` when it is unusable.
///
/// 7z stores `/`-separated names, but an archive built on Windows can carry
/// `\` too, so both are treated as separators rather than as filename
/// characters.
#[cfg(feature = "install")]
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let mut safe = PathBuf::new();
    for component in name.split(['/', '\\']) {
        match component {
            // Leading `/` yields an empty first component.
            "" | "." => continue,
            ".." => return None,
            other => {
                // A drive-relative or UNC-ish component would make the join
                // absolute on Windows.
                if other.contains(':') {
                    return None;
                }
                safe.push(other);
            }
        }
    }
    if safe.as_os_str().is_empty() {
        None
    } else {
        Some(safe)
    }
}

#[cfg(feature = "install")]
fn sevenz_error(error: Error) -> sevenz_rust2::Error {
    sevenz_rust2::Error::Other(error.to_string().into())
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
        assert_eq!(
            ArchiveKind::from_name("x.tar.gz").unwrap(),
            ArchiveKind::TarGz
        );
        assert_eq!(
            ArchiveKind::from_name("x.tar.xz").unwrap(),
            ArchiveKind::TarXz
        );
        assert_eq!(
            ArchiveKind::from_name("x.tar.zst").unwrap(),
            ArchiveKind::TarZst
        );
        assert_eq!(ArchiveKind::from_name("x.zip").unwrap(), ArchiveKind::Zip);
        assert!(ArchiveKind::from_name("x.rar").is_err());
    }

    /// Windows GCC toolchains publish `.7z` exclusively, so the name must map to
    /// the archive kind rather than falling through to "unsupported".
    #[cfg(feature = "install")]
    #[test]
    fn kind_from_name_recognizes_7z() {
        assert_eq!(
            ArchiveKind::from_name("x86_64-15.1.0-release-posix-seh-ucrt-rt_v12-rev0.7z").unwrap(),
            ArchiveKind::SevenZ
        );
        assert_eq!(ArchiveKind::from_name("X.7Z").unwrap(), ArchiveKind::SevenZ);
    }

    /// Entry names come from the archive, so a crafted `..`, absolute path, or
    /// Windows drive prefix must never resolve to a location outside the
    /// destination.
    #[cfg(feature = "install")]
    #[test]
    fn safe_relative_path_refuses_escapes() {
        for escape in [
            "../outside.txt",
            "a/../../outside.txt",
            "..\\outside.txt",
            "a\\..\\..\\outside.txt",
            "C:\\Windows\\System32\\evil.dll",
            "c:/windows/evil.dll",
            "",
            "/",
            "./",
        ] {
            assert!(
                safe_relative_path(escape).is_none(),
                "expected `{escape}` to be refused"
            );
        }

        // Ordinary entries survive, with both separators treated as such.
        assert_eq!(
            safe_relative_path("bin/gcc.exe").unwrap(),
            PathBuf::from("bin").join("gcc.exe")
        );
        assert_eq!(
            safe_relative_path("/lib/gcc/x.a").unwrap(),
            PathBuf::from("lib").join("gcc").join("x.a")
        );
        assert_eq!(
            safe_relative_path("bin\\ld.exe").unwrap(),
            PathBuf::from("bin").join("ld.exe")
        );
        assert_eq!(
            safe_relative_path("./bin/./as.exe").unwrap(),
            PathBuf::from("bin").join("as.exe")
        );
    }

    /// A real round trip: build a `.7z`, extract it through the pipeline, and
    /// confirm contents, nesting, and `strip_root` all behave like the other
    /// archive kinds.
    #[cfg(feature = "install")]
    #[test]
    fn extracts_a_real_7z_archive_with_strip_root() {
        let temp = tempfile::tempdir().unwrap();
        // `compress_to_path` stores the *contents* of the directory it is given,
        // so wrap the payload one level deeper to produce the single top-level
        // directory that real toolchain archives have.
        let staging = temp.path().join("staging");
        let source = staging.join("toolchain-1.0.0");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir_all(source.join("lib/gcc")).unwrap();
        std::fs::write(source.join("bin/gcc.exe"), b"fake compiler").unwrap();
        std::fs::write(source.join("lib/gcc/libgcc.a"), b"fake archive").unwrap();

        let archive = temp.path().join("toolchain.7z");
        sevenz_rust2::compress_to_path(&staging, &archive).expect("building fixture archive");

        // strip_root lifts the single `toolchain-1.0.0/` wrapper.
        let dest = temp.path().join("stripped");
        extract(&archive, &dest, ArchiveKind::SevenZ, true).unwrap();
        assert_eq!(
            std::fs::read(dest.join("bin/gcc.exe")).unwrap(),
            b"fake compiler"
        );
        assert_eq!(
            std::fs::read(dest.join("lib/gcc/libgcc.a")).unwrap(),
            b"fake archive"
        );
        // The scratch directory must not survive into the install root.
        assert!(!dest.join(".osdk-extract-tmp").exists());

        // Without stripping, the wrapper directory is preserved.
        let kept = temp.path().join("kept");
        extract(&archive, &kept, ArchiveKind::SevenZ, false).unwrap();
        assert!(kept.join("toolchain-1.0.0/bin/gcc.exe").is_file());
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
