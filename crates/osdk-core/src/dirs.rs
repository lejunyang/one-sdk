//! Resolution of osdk's on-disk directories (data / store / installs / shims /
//! cache), honoring `OSDK_*` env overrides, then falling back to the platform
//! conventions provided by the `directories` crate (XDG on Linux).
//!
//! Layout under the data dir:
//! ```text
//! $OSDK_DATA_DIR (default ~/.local/share/osdk)
//! ├── store/                 content-addressed blobs   (OSDK_STORE_DIR)
//! ├── installs/<tool>/<ver>/ materialized tool versions (OSDK_INSTALL_DIR)
//! ├── models/<name>/          materialized model snapshots
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
    /// Materialized model snapshots and per-model current-revision markers.
    pub fn models(&self) -> PathBuf {
        self.data.join("models")
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
        self.installs
            .join(sanitize_tool_id(tool))
            .join(sanitize_version_component(version))
    }

    /// Directory holding per-version install locks for a tool.
    pub fn lock_dir(&self, tool: &str) -> PathBuf {
        self.installs.join(sanitize_tool_id(tool)).join(".locks")
    }

    pub fn user_config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// User-scoped lockfile. Keeping it beside `config.toml` gives global tool
    /// pins a deterministic lock independent of the current working directory.
    pub fn user_lock_file(&self) -> PathBuf {
        self.config.join("osdk.lock")
    }

    /// Create the core directory tree (idempotent).
    pub fn ensure(&self) -> Result<()> {
        for d in [
            &self.data,
            &self.cache,
            &self.config,
            &self.store,
            &self.installs,
            &self.models(),
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

const ENCODED_VERSION_PREFIX: &str = "~v1~";

/// Encode a version label as one collision-resistant, portable filesystem
/// component. Common lowercase semver labels remain readable. Other labels use
/// a self-identifying prefix followed by percent-encoded UTF-8 bytes, avoiding
/// ambiguity with legacy names that contain literal percent escapes.
pub fn sanitize_version_component(version: &str) -> String {
    let portable = !version.is_empty()
        && version.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b'+')
        })
        && version != "."
        && version != ".."
        && !version.ends_with('.')
        && !is_windows_reserved_name(version);
    if portable {
        return version.to_string();
    }

    let mut out = String::with_capacity(ENCODED_VERSION_PREFIX.len() + version.len() * 3);
    out.push_str(ENCODED_VERSION_PREFIX);
    for byte in version.bytes() {
        use std::fmt::Write as _;
        write!(&mut out, "%{byte:02X}").expect("writing to String cannot fail");
    }
    out
}

/// Decode a component produced by [`sanitize_version_component`]. Unprefixed,
/// malformed, non-UTF-8, and non-canonical values are treated as legacy names
/// and returned unchanged.
pub fn decode_version_component(component: &str) -> String {
    let Some(encoded) = component.strip_prefix(ENCODED_VERSION_PREFIX) else {
        return component.to_string();
    };
    if encoded.len() % 3 != 0 {
        return component.to_string();
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 3);
    for chunk in bytes.as_chunks::<3>().0 {
        if chunk[0] != b'%' {
            return component.to_string();
        }
        let Some(high) = hex_value(chunk[1]) else {
            return component.to_string();
        };
        let Some(low) = hex_value(chunk[2]) else {
            return component.to_string();
        };
        decoded.push(high * 16 + low);
    }
    let Ok(decoded) = String::from_utf8(decoded) else {
        return component.to_string();
    };
    if sanitize_version_component(&decoded) == component {
        decoded
    } else {
        component.to_string()
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or_default();
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || stem.strip_prefix("com").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("lpt").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
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
        assert_eq!(d.models(), PathBuf::from("/x/data/models"));
        assert_eq!(d.user_lock_file(), PathBuf::from("/x/cfg/osdk.lock"));
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

    #[test]
    fn version_components_cannot_escape_install_root() {
        assert_eq!(sanitize_version_component("20.1.0"), "20.1.0");
        assert_eq!(
            sanitize_version_component("../../victim"),
            "~v1~%2E%2E%2F%2E%2E%2F%76%69%63%74%69%6D"
        );
        assert_eq!(sanitize_version_component(".."), "~v1~%2E%2E");
    }

    #[test]
    fn version_encoding_is_collision_resistant_and_portable() {
        let values = [
            "release/2026",
            "release_2026",
            r"release\2026",
            "release%2F2026",
            "Release/2026",
            "con",
            "con.txt",
            "version.",
        ];
        let encoded = values
            .iter()
            .map(|value| sanitize_version_component(value))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(encoded.len(), values.len());
        assert!(encoded.iter().all(|value| !value.contains(['/', '\\'])));
    }

    #[test]
    fn version_encoding_round_trips_without_literal_percent_ambiguity() {
        for version in [
            "20.1.0",
            "release/2026",
            "Release-2026",
            r"release\2026",
            "release%2F2026",
            "版本/二〇二六",
            "",
        ] {
            assert_eq!(
                decode_version_component(&sanitize_version_component(version)),
                version
            );
        }
        for legacy in ["release%2F2026", "~v1~broken", "~v1~%FF", "~v1~%2f"] {
            assert_eq!(decode_version_component(legacy), legacy);
        }
    }
}
