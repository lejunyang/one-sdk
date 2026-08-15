//! Resolution of osdk's on-disk directories (data / store / installs / shims /
//! cache), honoring `OSDK_*` env overrides, then falling back to the platform
//! conventions provided by the `directories` crate (XDG on Linux).
//!
//! Layout under the data dir:
//! ```text
//! $OSDK_DATA_DIR (default ~/.local/share/osdk)
//! ├── store/                 content-addressed blobs   (OSDK_STORE_DIR)
//! ├── installs/<tool>/<ver>/ materialized tool versions (OSDK_INSTALL_DIR)
//! ├── shims/                 shim launchers + osdk-shim
//! ├── rustup/  cargo/        self-contained homes for delegate backends
//! └── plugins/               future external backends
//!
//! $OSDK_CACHE_DIR (default ~/.cache/osdk)
//! ├── downloads/  tmp/  remote/  sources/
//! ```

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Environment variable names for directory overrides.
pub mod env_keys {
    pub const DATA_DIR: &str = "OSDK_DATA_DIR";
    pub const CACHE_DIR: &str = "OSDK_CACHE_DIR";
    pub const CONFIG_DIR: &str = "OSDK_CONFIG_DIR";
    pub const STORE_DIR: &str = "OSDK_STORE_DIR";
    pub const INSTALL_DIR: &str = "OSDK_INSTALL_DIR";
}

#[derive(Debug, Clone)]
pub struct Dirs {
    /// Root for persistent state (installs, store, shims).
    pub data: PathBuf,
    /// Root for disposable cache (downloads, extraction scratch, indices).
    pub cache: PathBuf,
    /// Root for user config files.
    pub config: PathBuf,
    /// Content-addressed store. Defaults to `data/store` (same volume ⇒
    /// hardlinks work out of the box).
    pub store: PathBuf,
    /// Where materialized versions live. Defaults to `data/installs`.
    pub installs: PathBuf,
}

impl Dirs {
    /// Resolve directories from env overrides + platform defaults.
    pub fn resolve() -> Result<Dirs> {
        Self::resolve_from(|k| std::env::var(k).ok())
    }

    /// Resolve using a custom env lookup (used by tests).
    pub fn resolve_from(getenv: impl Fn(&str) -> Option<String>) -> Result<Dirs> {
        let proj = directories::ProjectDirs::from("", "", "osdk");

        let data = match getenv(env_keys::DATA_DIR) {
            Some(v) => PathBuf::from(v),
            None => proj
                .as_ref()
                .map(|p| p.data_dir().to_path_buf())
                .ok_or_else(|| Error::config("cannot determine data dir; set OSDK_DATA_DIR"))?,
        };
        let cache = match getenv(env_keys::CACHE_DIR) {
            Some(v) => PathBuf::from(v),
            None => proj
                .as_ref()
                .map(|p| p.cache_dir().to_path_buf())
                .ok_or_else(|| Error::config("cannot determine cache dir; set OSDK_CACHE_DIR"))?,
        };
        let config = match getenv(env_keys::CONFIG_DIR) {
            Some(v) => PathBuf::from(v),
            None => proj
                .as_ref()
                .map(|p| p.config_dir().to_path_buf())
                .ok_or_else(|| Error::config("cannot determine config dir; set OSDK_CONFIG_DIR"))?,
        };

        let store = getenv(env_keys::STORE_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("store"));
        let installs = getenv(env_keys::INSTALL_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("installs"));

        Ok(Dirs {
            data,
            cache,
            config,
            store,
            installs,
        })
    }

    pub fn shims(&self) -> PathBuf {
        self.data.join("shims")
    }
    pub fn plugins(&self) -> PathBuf {
        self.data.join("plugins")
    }
    /// Self-contained rustup home for the delegate rust backend.
    pub fn rustup_home(&self) -> PathBuf {
        self.data.join("rustup")
    }
    /// Self-contained cargo home for the delegate rust backend.
    pub fn cargo_home(&self) -> PathBuf {
        self.data.join("cargo")
    }

    pub fn downloads(&self) -> PathBuf {
        self.cache.join("downloads")
    }
    pub fn tmp(&self) -> PathBuf {
        self.cache.join("tmp")
    }
    /// Cached remote indices (version lists) with TTL.
    pub fn remote_cache(&self) -> PathBuf {
        self.cache.join("remote")
    }
    /// Cached source speed-probe results with TTL.
    pub fn sources_cache(&self) -> PathBuf {
        self.cache.join("sources")
    }

    /// Install directory for a specific tool version.
    pub fn install_path(&self, tool: &str, version: &str) -> PathBuf {
        self.installs.join(sanitize_tool_id(tool)).join(version)
    }

    /// Directory holding per-version install locks for a tool.
    pub fn lock_dir(&self, tool: &str) -> PathBuf {
        self.installs.join(sanitize_tool_id(tool)).join(".locks")
    }

    pub fn user_config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// Create the core directory tree (idempotent).
    pub fn ensure(&self) -> Result<()> {
        for d in [
            &self.data,
            &self.cache,
            &self.config,
            &self.store,
            &self.installs,
            &self.shims(),
            &self.downloads(),
            &self.tmp(),
            &self.remote_cache(),
            &self.sources_cache(),
        ] {
            create_dir_all(d)?;
        }
        Ok(())
    }
}

pub(crate) fn create_dir_all(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).map_err(|e| Error::io(p, e))
}

/// Map a (possibly namespaced) tool id to a filesystem-safe nested path
/// component. e.g. `github:owner/repo` -> `github/owner/repo`. `:` is replaced
/// (invalid on Windows) and path traversal is neutralized.
pub fn sanitize_tool_id(tool: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in tool.split([':', '/', '\\']) {
        let part = part.trim();
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push("_");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_overrides_win() {
        let mut env = HashMap::new();
        env.insert(env_keys::DATA_DIR.to_string(), "/x/data".to_string());
        env.insert(env_keys::CACHE_DIR.to_string(), "/x/cache".to_string());
        env.insert(env_keys::CONFIG_DIR.to_string(), "/x/cfg".to_string());
        let d = Dirs::resolve_from(|k| env.get(k).cloned()).unwrap();
        assert_eq!(d.data, PathBuf::from("/x/data"));
        assert_eq!(d.store, PathBuf::from("/x/data/store"));
        assert_eq!(d.installs, PathBuf::from("/x/data/installs"));
        assert_eq!(d.shims(), PathBuf::from("/x/data/shims"));
        assert_eq!(
            d.install_path("node", "20.1.0"),
            PathBuf::from("/x/data/installs/node/20.1.0")
        );
    }

    #[test]
    fn store_dir_can_be_split_off() {
        let mut env = HashMap::new();
        env.insert(env_keys::DATA_DIR.to_string(), "/x/data".to_string());
        env.insert(env_keys::CACHE_DIR.to_string(), "/x/cache".to_string());
        env.insert(env_keys::CONFIG_DIR.to_string(), "/x/cfg".to_string());
        env.insert(env_keys::STORE_DIR.to_string(), "/big/store".to_string());
        let d = Dirs::resolve_from(|k| env.get(k).cloned()).unwrap();
        assert_eq!(d.store, PathBuf::from("/big/store"));
    }
}
