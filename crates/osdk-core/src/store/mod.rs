//! Content-addressed store (CAS): the dedup engine.
//!
//! Every regular file extracted from an SDK archive is hashed with blake3 and
//! stored once at `store/<aa>/<bb>/<hash>`. Multiple tool versions that share
//! identical files (e.g. node 20.11.0 and 20.11.1) therefore keep only one copy
//! on disk; each install dir is materialized from the store via hardlink /
//! reflink / copy (see [`super::link`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

pub mod link;
pub mod manifest;

use link::{materialize, LinkMode};
use manifest::{FileEntry, Manifest};

pub struct Cas {
    root: PathBuf,
}

/// Outcome of materializing an extracted tree into an install dir.
pub struct MaterializeReport {
    pub manifest: Manifest,
    /// Distinct link modes actually used (for diagnostics).
    pub files_written: usize,
    pub bytes_ingested: u64,
    pub objects_new: usize,
}

impl Cas {
    pub fn new(root: impl Into<PathBuf>) -> Cas {
        Cas { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of a blob given its hex hash, with 2-level fanout.
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let (a, b) = (&hash[0..2], &hash[2..4]);
        self.root.join(a).join(b).join(hash)
    }

    pub fn has_object(&self, hash: &str) -> bool {
        self.object_path(hash).exists()
    }

    /// Ingest a file's bytes into the store, returning its hash. If an object
    /// with the same content already exists, the source is not duplicated.
    /// Returns `(hash, is_new)`.
    fn ingest_file(&self, src: &Path) -> Result<(String, bool, u64)> {
        let hash = hash_file(src)?;
        let obj = self.object_path(&hash);
        let mut size = 0u64;
        if let Ok(m) = std::fs::metadata(src) {
            size = m.len();
        }
        if obj.exists() {
            return Ok((hash, false, size));
        }
        if let Some(parent) = obj.parent() {
            create_dir_all(parent)?;
        }
        // Move into place when possible (same fs), else copy. Use a temp name
        // then atomic rename so concurrent ingests don't see partial objects.
        let tmp = obj.with_extension("tmp");
        match std::fs::rename(src, &obj) {
            Ok(()) => {}
            Err(_) => {
                std::fs::copy(src, &tmp).map_err(|e| Error::io(&tmp, e))?;
                // ignore error if another process won the race
                let _ = std::fs::rename(&tmp, &obj);
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Ok((hash, true, size))
    }

    /// Ingest one file and return its content hash, whether the object was new,
    /// and its size. The source may be moved into the store.
    pub fn ingest(&self, src: &Path) -> Result<(String, bool, u64)> {
        self.ingest_file(src)
    }

    /// Materialize one existing CAS object at `destination`.
    pub fn materialize_object(&self, hash: &str, destination: &Path, mode: LinkMode) -> Result<()> {
        let object = self.object_path(hash);
        if !object.is_file() {
            return Err(Error::other(format!(
                "content-addressed object is missing: {hash}"
            )));
        }
        materialize(&object, destination, mode).map(|_| ())
    }

    /// Ingest an extracted directory tree into the store and materialize it at
    /// `install_dir` using `mode`. Writes and returns the manifest.
    pub fn ingest_tree(
        &self,
        extracted_root: &Path,
        install_dir: &Path,
        tool: &str,
        version: &str,
        mode: LinkMode,
    ) -> Result<MaterializeReport> {
        create_dir_all(install_dir)?;
        let mut manifest = Manifest::new(tool, version, mode.to_string());
        let mut files_written = 0usize;
        let mut bytes_ingested = 0u64;
        let mut objects_new = 0usize;

        for entry in WalkDir::new(extracted_root).follow_links(false) {
            let entry = entry.map_err(|e| Error::other(format!("walkdir: {e}")))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(extracted_root)
                .map_err(|_| Error::other("strip_prefix failed"))?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            let rel_str = rel_to_slash(rel);
            let ft = entry.file_type();
            let dst = install_dir.join(rel);

            if ft.is_dir() {
                create_dir_all(&dst)?;
            } else if ft.is_symlink() {
                let target = std::fs::read_link(path).map_err(|e| Error::io(path, e))?;
                recreate_symlink(path, &target, &dst)?;
                manifest.files.push(FileEntry {
                    path: rel_str,
                    hash: None,
                    mode: 0,
                    symlink: Some(rel_to_slash(&target)),
                });
            } else if ft.is_file() {
                let mode_bits = file_mode(path);
                let (hash, is_new, size) = self.ingest_file(path)?;
                if is_new {
                    objects_new += 1;
                    bytes_ingested += size;
                }
                let obj = self.object_path(&hash);
                materialize(&obj, &dst, mode)?;
                apply_mode(&dst, mode_bits);
                files_written += 1;
                manifest.files.push(FileEntry {
                    path: rel_str,
                    hash: Some(hash),
                    mode: mode_bits,
                    symlink: None,
                });
            }
        }

        manifest.save(install_dir)?;
        Ok(MaterializeReport {
            manifest,
            files_written,
            bytes_ingested,
            objects_new,
        })
    }

    /// Garbage-collect: delete store objects not referenced by any manifest in
    /// `installs_root`. Returns (objects_removed, bytes_removed).
    pub fn gc(&self, installs_root: &Path) -> Result<(usize, u64)> {
        self.gc_roots(&[installs_root])
    }

    /// Garbage-collect objects not referenced by a manifest below any root.
    pub fn gc_roots(&self, roots: &[&Path]) -> Result<(usize, u64)> {
        use std::collections::HashSet;
        let mut live: HashSet<String> = HashSet::new();
        for root in roots {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(root).follow_links(false) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_name() == manifest::MANIFEST_FILE {
                    let install = entry.path().parent().unwrap();
                    let manifest = Manifest::load(install).map_err(|error| {
                        Error::other(format!(
                            "refusing store GC because manifest is corrupt at {}: {error}",
                            install.display()
                        ))
                    })?;
                    for hash in manifest.referenced_hashes() {
                        live.insert(hash.to_string());
                    }
                }
            }
        }

        let mut removed = 0usize;
        let mut bytes = 0u64;
        if self.root.exists() {
            for entry in WalkDir::new(&self.root).follow_links(false) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                // object file names are the hex hash; skip temp files
                if name.ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                if !live.contains(&name) {
                    if let Ok(m) = entry.metadata() {
                        bytes += m.len();
                    }
                    if std::fs::remove_file(entry.path()).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok((removed, bytes))
    }
}

/// Compute the blake3 hash of a file's contents (hex).
pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn rel_to_slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) {}

#[cfg(unix)]
fn recreate_symlink(_source_link: &Path, target: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(dst);
    std::os::unix::fs::symlink(target, dst).map_err(|e| Error::io(dst, e))
}

#[cfg(windows)]
fn recreate_symlink(source_link: &Path, target: &Path, dst: &Path) -> Result<()> {
    recreate_windows_symlink(source_link, target, dst, |target, dst, is_directory| {
        if is_directory {
            std::os::windows::fs::symlink_dir(target, dst)
        } else {
            std::os::windows::fs::symlink_file(target, dst)
        }
    })
}

#[cfg(windows)]
fn recreate_windows_symlink(
    source_link: &Path,
    target: &Path,
    dst: &Path,
    create: impl FnOnce(&Path, &Path, bool) -> std::io::Result<()>,
) -> Result<()> {
    if let Some(parent) = dst.parent() {
        create_dir_all(parent)?;
    }
    if dst.symlink_metadata().is_ok() {
        if dst.is_dir() {
            let _ = std::fs::remove_dir_all(dst);
        } else {
            let _ = std::fs::remove_file(dst);
        }
    }
    let source = if target.is_absolute() {
        target.to_path_buf()
    } else {
        source_link
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    };
    let is_directory = source.is_dir();
    if create(target, dst, is_directory).is_err() {
        if is_directory {
            copy_directory(&source, dst)?;
        } else {
            std::fs::copy(&source, dst).map_err(|e| Error::io(&source, e))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    create_dir_all(destination)?;
    for entry in WalkDir::new(source).min_depth(1) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| Error::other("strip_prefix failed"))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target).map_err(|error| Error::io(&target, error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(p: &Path, b: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(b).unwrap();
    }

    #[test]
    fn dedup_identical_files_across_versions() {
        let td = tempfile::tempdir().unwrap();
        let cas = Cas::new(td.path().join("store"));
        let installs = td.path().join("installs");

        // version A
        let ex_a = td.path().join("ex_a");
        write(&ex_a.join("bin/node"), b"BINARY");
        write(&ex_a.join("README.md"), b"same-doc");
        cas.ingest_tree(
            &ex_a,
            &installs.join("node/20.11.0"),
            "node",
            "20.11.0",
            LinkMode::Copy,
        )
        .unwrap();

        // version B: README identical, binary different
        let ex_b = td.path().join("ex_b");
        write(&ex_b.join("bin/node"), b"BINARY-v2");
        write(&ex_b.join("README.md"), b"same-doc");
        let rep_b = cas
            .ingest_tree(
                &ex_b,
                &installs.join("node/20.11.1"),
                "node",
                "20.11.1",
                LinkMode::Copy,
            )
            .unwrap();

        // README was already in store ⇒ only the new binary is a new object.
        assert_eq!(rep_b.objects_new, 1);

        // store holds exactly 3 objects total: BINARY, same-doc, BINARY-v2
        let count = WalkDir::new(cas.root())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count();
        assert_eq!(count, 3);
    }

    #[test]
    fn gc_removes_unreferenced_after_uninstall() {
        let td = tempfile::tempdir().unwrap();
        let cas = Cas::new(td.path().join("store"));
        let installs = td.path().join("installs");

        let ex = td.path().join("ex");
        write(&ex.join("bin/tool"), b"unique-bytes");
        let inst = installs.join("go/1.22.0");
        cas.ingest_tree(&ex, &inst, "go", "1.22.0", LinkMode::Copy)
            .unwrap();

        // simulate uninstall: remove the install dir (and its manifest)
        std::fs::remove_dir_all(&inst).unwrap();

        let (removed, _) = cas.gc(&installs).unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn gc_refuses_to_delete_when_an_install_manifest_is_corrupt() {
        let temporary = tempfile::tempdir().unwrap();
        let cas = Cas::new(temporary.path().join("store"));
        let installs = temporary.path().join("installs");
        let extracted = temporary.path().join("extracted");
        write(&extracted.join("bin/tool"), b"preserve-me");
        let install = installs.join("tool/1.0.0");
        cas.ingest_tree(&extracted, &install, "tool", "1.0.0", LinkMode::Copy)
            .unwrap();
        std::fs::write(install.join(manifest::MANIFEST_FILE), b"{broken").unwrap();

        let error = cas.gc(&installs).unwrap_err();
        assert!(error.to_string().contains("refusing store GC"));
        assert_eq!(
            WalkDir::new(cas.root())
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
                .count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn relative_symlink_is_preserved_when_available() {
        let temporary = tempfile::tempdir().unwrap();
        let source_link = temporary.path().join("extracted/bin/alias.exe");
        let destination = temporary.path().join("installed/bin/alias.exe");
        let target = Path::new("real.exe");
        std::fs::create_dir_all(source_link.parent().unwrap()).unwrap();
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(source_link.parent().unwrap().join(target), b"runtime").unwrap();
        std::fs::write(destination.parent().unwrap().join(target), b"runtime").unwrap();

        recreate_symlink(&source_link, target, &destination).unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"runtime");
    }

    #[cfg(windows)]
    #[test]
    fn file_and_directory_symlinks_copy_when_permission_is_denied() {
        let temporary = tempfile::tempdir().unwrap();
        let extracted = temporary.path().join("extracted");
        let installed = temporary.path().join("installed");
        let denied = |_: &Path, _: &Path, _: bool| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "fixture",
            ))
        };

        let file_link = extracted.join("bin/alias.exe");
        std::fs::create_dir_all(file_link.parent().unwrap()).unwrap();
        std::fs::write(extracted.join("bin/real.exe"), b"runtime").unwrap();
        let file_destination = installed.join("bin/alias.exe");
        recreate_windows_symlink(&file_link, Path::new("real.exe"), &file_destination, denied)
            .unwrap();
        assert_eq!(std::fs::read(file_destination).unwrap(), b"runtime");

        let directory_link = extracted.join("current");
        std::fs::create_dir_all(extracted.join("versions/1/bin")).unwrap();
        std::fs::write(extracted.join("versions/1/bin/tool.exe"), b"directory").unwrap();
        let directory_destination = installed.join("current");
        recreate_windows_symlink(
            &directory_link,
            Path::new("versions/1"),
            &directory_destination,
            denied,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(directory_destination.join("bin/tool.exe")).unwrap(),
            b"directory"
        );
    }
}
