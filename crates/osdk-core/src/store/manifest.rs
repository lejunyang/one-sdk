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
