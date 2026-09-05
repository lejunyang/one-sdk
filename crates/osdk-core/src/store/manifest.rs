//! Per-install manifest: records every file materialized into an install dir,
//! along with its content hash, mode, and (for symlinks) target. Used to
//! verify installs and to compute the live set for store GC.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Name of the manifest file written at the root of each install dir.
pub const MANIFEST_FILE: &str = ".osdk-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the install root, using forward slashes.
    pub path: String,
    /// blake3 content hash (hex), for regular files. None for symlinks/dirs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Unix mode bits (permissions). 0 if unknown / not applicable.
    #[serde(default)]
    pub mode: u32,
    /// For symlink entries: the link target (verbatim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub tool: String,
    pub version: String,
    /// Which link mode was used to materialize this install.
    pub link_mode: String,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    pub fn new(
        tool: impl Into<String>,
        version: impl Into<String>,
        link_mode: impl Into<String>,
    ) -> Manifest {
        Manifest {
            tool: tool.into(),
            version: version.into(),
            link_mode: link_mode.into(),
            files: Vec::new(),
        }
    }

    pub fn manifest_path(install_dir: &Path) -> PathBuf {
        install_dir.join(MANIFEST_FILE)
    }

    pub fn load(install_dir: &Path) -> Result<Manifest> {
        let p = Self::manifest_path(install_dir);
        let bytes = std::fs::read(&p).map_err(|e| Error::io(&p, e))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save(&self, install_dir: &Path) -> Result<()> {
        let p = Self::manifest_path(install_dir);
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&p, bytes).map_err(|e| Error::io(&p, e))?;
        Ok(())
    }

    /// The set of store hashes referenced by this install.
    pub fn referenced_hashes(&self) -> impl Iterator<Item = &str> {
        self.files.iter().filter_map(|f| f.hash.as_deref())
    }
}

/// What a manifest check found wrong with one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftKind {
    /// The file the manifest recorded is gone.
    Missing,
    /// The bytes on disk hash to something else.
    Modified { expected: String, actual: String },
    /// A regular file was replaced by a link, or a link by a file. Worth
    /// separating from `Modified`: a link can point outside the install.
    KindChanged,
    /// The recorded symlink now points elsewhere.
    TargetChanged { expected: String, actual: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// Path relative to the install root, as recorded.
    pub path: String,
    pub kind: DriftKind,
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            DriftKind::Missing => write!(f, "{}: missing", self.path),
            DriftKind::Modified { .. } => write!(f, "{}: contents changed", self.path),
            DriftKind::KindChanged => write!(f, "{}: file type changed", self.path),
            DriftKind::TargetChanged { expected, actual } => {
                write!(
                    f,
                    "{}: link now points at {actual} not {expected}",
                    self.path
                )
            }
        }
    }
}

impl Manifest {
    /// Re-hash everything this manifest recorded and report what no longer
    /// matches.
    ///
    /// osdk verifies bytes as they are downloaded, but nothing re-checked them
    /// afterwards, so anything that rewrote an install in place -- a tool's own
    /// self-update, a manual edit, a partially restored backup, bit rot -- left
    /// osdk reporting the version it installed while a different binary ran.
    /// The receipt only proves what was fetched once; this proves what is on
    /// disk now.
    ///
    /// Reads every file, so it belongs behind an explicit command rather than
    /// on the execution path.
    pub fn verify(&self, install_dir: &Path) -> Result<Vec<Drift>> {
        let mut drift = Vec::new();
        for entry in &self.files {
            // The manifest stores forward slashes; rebuild the path component
            // by component so it stays inside the install dir on Windows too.
            let mut path = install_dir.to_path_buf();
            for part in entry.path.split('/') {
                if part.is_empty() || part == "." || part == ".." {
                    continue;
                }
                path.push(part);
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    drift.push(Drift {
                        path: entry.path.clone(),
                        kind: DriftKind::Missing,
                    });
                    continue;
                }
                Err(error) => return Err(Error::io(&path, error)),
            };
            match (&entry.symlink, &entry.hash) {
                (Some(expected), _) => {
                    if !metadata.file_type().is_symlink() {
                        drift.push(Drift {
                            path: entry.path.clone(),
                            kind: DriftKind::KindChanged,
                        });
                        continue;
                    }
                    let actual = std::fs::read_link(&path)
                        .map_err(|error| Error::io(&path, error))?
                        .to_string_lossy()
                        .into_owned();
                    if &actual != expected {
                        drift.push(Drift {
                            path: entry.path.clone(),
                            kind: DriftKind::TargetChanged {
                                expected: expected.clone(),
                                actual,
                            },
                        });
                    }
                }
                (None, Some(expected)) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        drift.push(Drift {
                            path: entry.path.clone(),
                            kind: DriftKind::KindChanged,
                        });
                        continue;
                    }
                    let actual = crate::store::hash_file(&path)?;
                    if !actual.eq_ignore_ascii_case(expected) {
                        drift.push(Drift {
                            path: entry.path.clone(),
                            kind: DriftKind::Modified {
                                expected: expected.clone(),
                                actual,
                            },
                        });
                    }
                }
                // Directories carry neither a hash nor a target; existence is
                // all the manifest claimed.
                (None, None) => {}
            }
        }
        Ok(drift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Manifest) {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::new("demo", "1.0.0", "auto");
        for (path, bytes) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, bytes).unwrap();
            manifest.files.push(FileEntry {
                path: (*path).to_string(),
                hash: Some(crate::store::hash_file(&full).unwrap()),
                mode: 0,
                symlink: None,
            });
        }
        (dir, manifest)
    }

    #[test]
    fn a_clean_install_reports_no_drift() {
        let (dir, manifest) = install_with(&[("bin/tool", b"payload"), ("lib/data.bin", b"data")]);
        assert!(manifest.verify(dir.path()).unwrap().is_empty());
        // Verification must not be a one-shot: repeated checks stay clean.
        assert!(manifest.verify(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn an_in_place_rewrite_is_caught() {
        // This is the self-update case: same path, same manifest, different
        // bytes. Nothing else in osdk looks at the bytes after install, so if
        // this check misses it the tool silently runs a version osdk did not
        // install and still reports the old one.
        let (dir, manifest) = install_with(&[("bin/tool", b"v1")]);
        std::fs::write(dir.path().join("bin/tool"), b"v2-self-updated").unwrap();

        let drift = manifest.verify(dir.path()).unwrap();
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0].path, "bin/tool");
        assert!(matches!(drift[0].kind, DriftKind::Modified { .. }));
    }

    #[test]
    fn a_same_length_edit_is_caught() {
        // Length and mtime are the cheap signals, and both can be preserved by
        // an in-place patch, so the check has to hash rather than stat.
        let (dir, manifest) = install_with(&[("bin/tool", b"aaaa")]);
        let path = dir.path().join("bin/tool");
        let before = std::fs::metadata(&path).unwrap();
        // Write through a handle opened for writing, so restoring the timestamp
        // is permitted on Windows as well.
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        {
            use std::io::Write as _;
            let mut file = &file;
            file.write_all(b"bbbb").unwrap();
            file.flush().unwrap();
        }
        file.set_modified(before.modified().unwrap()).unwrap();
        drop(file);

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());

        let drift = manifest.verify(dir.path()).unwrap();
        assert_eq!(drift.len(), 1, "same-length edit must still be caught");
        assert!(matches!(drift[0].kind, DriftKind::Modified { .. }));
    }

    #[test]
    fn a_deleted_file_is_reported_rather_than_erroring() {
        // A half-deleted install must produce a report, not an io error, or
        // the command cannot tell the user what to reinstall.
        let (dir, manifest) = install_with(&[("bin/tool", b"x"), ("bin/other", b"y")]);
        std::fs::remove_file(dir.path().join("bin/other")).unwrap();

        let drift = manifest.verify(dir.path()).unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].path, "bin/other");
        assert_eq!(drift[0].kind, DriftKind::Missing);
    }

    #[test]
    fn a_file_swapped_for_a_link_is_caught_as_a_kind_change() {
        // Reported separately from a content change because a link can point
        // outside the install entirely, which content hashing would follow.
        let (dir, manifest) = install_with(&[("bin/tool", b"payload")]);
        let path = dir.path().join("bin/tool");
        let outside = dir.path().join("elsewhere");
        std::fs::write(&outside, b"payload").unwrap();
        std::fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&outside, &path).is_err() {
            // Unprivileged Windows cannot create symlinks; the rest of the
            // suite still covers the logic.
            return;
        }

        let drift = manifest.verify(dir.path()).unwrap();
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0].kind, DriftKind::KindChanged);
    }

    #[test]
    fn a_manifest_path_cannot_reach_outside_the_install() {
        // The manifest is data on disk, so a crafted or corrupted one must not
        // send the check climbing out of the install directory.
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let victim = root.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();

        let manifest = Manifest {
            tool: "demo".into(),
            version: "1.0.0".into(),
            link_mode: "auto".into(),
            files: vec![FileEntry {
                path: "../victim".into(),
                hash: Some("0".repeat(64)),
                mode: 0,
                symlink: None,
            }],
        };
        let drift = manifest.verify(&install).unwrap();
        // Traversal is stripped, so the path resolves inside the install and is
        // simply absent -- not hashed from outside and not an error.
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].kind, DriftKind::Missing);
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"untouched",
            "the file outside must not be touched"
        );
    }

    #[test]
    fn directory_entries_only_need_to_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        let manifest = Manifest {
            tool: "demo".into(),
            version: "1.0.0".into(),
            link_mode: "auto".into(),
            files: vec![FileEntry {
                path: "empty".into(),
                hash: None,
                mode: 0,
                symlink: None,
            }],
        };
        assert!(manifest.verify(dir.path()).unwrap().is_empty());
    }
}
