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
use crate::tool::{InstallIdentity, InstallScope};

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

/// All filesystem locations derived from one validated dynamic install identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLocator {
    identity: InstallIdentity,
    installs_root: PathBuf,
    install_root: PathBuf,
    legacy_install_root: PathBuf,
    legacy_global_install_root: Option<PathBuf>,
    lock_path: PathBuf,
    scratch_root: PathBuf,
}

impl InstallLocator {
    pub fn new(dirs: &Dirs, identity: InstallIdentity) -> Result<Self> {
        identity.validate()?;
        let component = install_id_component(&identity.install_id)?;
        let legacy_install_root = dirs.install_path(&identity.tool, &identity.version);
        let legacy_global_install_root = identity
            .tool
            .strip_prefix("npm:")
            .map(|package| dirs.install_path(&format!("npm-global:{package}"), &identity.version));
        let install_root = match identity.scope {
            InstallScope::Isolated => legacy_install_root.join(&component),
            InstallScope::Global => legacy_global_install_root
                .as_ref()
                .ok_or_else(|| Error::config("global dynamic installs are supported only for npm"))?
                .join(&component),
            InstallScope::ProjectManaged => {
                return Err(Error::config(
                    "project-managed tools do not have an osdk install locator",
                ));
            }
        };
        let lock_path = dirs
            .lock_dir(&identity.tool)
            .join(format!("{component}.lock"));
        let scratch_root = dirs
            .tmp()
            .join(sanitize_tool_id(&identity.tool))
            .join(sanitize_version_component(&identity.version))
            .join(component);
        Ok(Self {
            identity,
            installs_root: dirs.installs.clone(),
            install_root,
            legacy_install_root,
            legacy_global_install_root,
            lock_path,
            scratch_root,
        })
    }

    pub fn identity(&self) -> &InstallIdentity {
        &self.identity
    }

    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    pub(crate) fn installs_root(&self) -> &Path {
        &self.installs_root
    }

    pub fn legacy_install_root(&self) -> &Path {
        &self.legacy_install_root
    }

    pub fn legacy_global_install_root(&self) -> Option<&Path> {
        self.legacy_global_install_root.as_deref()
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn scratch_root(&self) -> &Path {
        &self.scratch_root
    }

    /// Require a discovered manifest to reside at this identity's canonical
    /// fingerprinted root, with no links inside the osdk-managed tree.
    pub fn validates_install_root(&self, root: &Path) -> bool {
        paths_agree(&self.install_root, root)
            && path_has_no_symlink_directories_from(&self.installs_root, root, true)
    }

    /// Require the canonical identity root to exist entirely as real
    /// directories. This is the filesystem trust check for consumers that are
    /// about to execute or remove content from an install.
    pub fn validates_existing_install_root(&self, root: &Path) -> bool {
        paths_agree(&self.install_root, root)
            && path_has_no_symlink_directories_from(&self.installs_root, root, false)
    }

    /// Validate a scanned root using only the configured installs directory.
    pub fn is_canonical_install_root(
        installs: &Path,
        identity: &InstallIdentity,
        root: &Path,
    ) -> Result<bool> {
        identity.validate()?;
        let component = install_id_component(&identity.install_id)?;
        let base_tool = match identity.scope {
            InstallScope::Isolated => identity.tool.clone(),
            InstallScope::Global => {
                let package = identity.tool.strip_prefix("npm:").ok_or_else(|| {
                    Error::config("global dynamic installs are supported only for npm")
                })?;
                format!("npm-global:{package}")
            }
            InstallScope::ProjectManaged => {
                return Err(Error::config(
                    "project-managed tools do not have an osdk install locator",
                ));
            }
        };
        let expected = installs
            .join(sanitize_tool_id(&base_tool))
            .join(sanitize_version_component(&identity.version))
            .join(component);
        Ok(paths_agree(&expected, root)
            && path_has_no_symlink_directories_from(installs, root, false))
    }
}

/// Compare a caller-supplied install root with the root derived from trusted
/// identity fields. The managed path is checked separately from its platform
/// ancestors: macOS exposes temporary directories below the `/var` alias and
/// Windows may return an equivalent long path for an 8.3 path.
fn paths_agree(expected: &Path, actual: &Path) -> bool {
    expected == actual
}

fn path_has_no_symlink_directories_from(base: &Path, path: &Path, allow_missing: bool) -> bool {
    let Ok(relative) = path.strip_prefix(base) else {
        return false;
    };
    let mut current = base.to_path_buf();
    let metadata = match std::fs::symlink_metadata(&current) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }

    for component in relative.components() {
        match component {
            std::path::Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = match std::fs::symlink_metadata(&current) {
                    Ok(metadata) => metadata,
                    Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                        return true;
                    }
                    Err(_) => return false,
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

pub fn install_id_component(install_id: &str) -> Result<String> {
    let Some(digest) = install_id.strip_prefix("b3-v2:") else {
        return Err(Error::config("dynamic install id must use b3-v2"));
    };
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::config(
            "dynamic install id contains an invalid BLAKE3 digest",
        ));
    }
    Ok(format!("b3-v2-{}", digest.to_ascii_lowercase()))
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

/// Cap on how many segments of a tool id become directories. Five keeps every
/// namespaced id in use today expanded in full -- `github:owner/repo`,
/// `npm:@scope/pkg`, and a `go:` import path -- while bounding ids built from a
/// URL, whose segment count is set by the remote server rather than by us.
const MAX_TOOL_ID_SEGMENTS: usize = 5;

/// Marks a folded tail so a hashed component is never mistaken for a literal
/// path segment that happens to look like hex.
const FOLDED_TOOL_ID_PREFIX: &str = "~t1~";

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
///
/// Ids expand one segment per directory because that keeps the install tree
/// readable and greppable, but an id derived from a URL has as many segments as
/// the URL does, and the install root already spends three levels on the tool,
/// the version and the install fingerprint. An `http:` artifact on a path like
/// `/android/cli/{version}/windows_x86_64/android.exe` therefore landed eleven
/// levels below the installs root, past the inventory scanner's depth limit:
/// the install reported success and then every later use failed to find its own
/// receipt. Deep ids keep their leading segments and fold the remainder into one
/// digest, so the tree stays bounded without becoming opaque for the short ids
/// that are the common case.
pub fn sanitize_tool_id(tool: &str) -> PathBuf {
    let parts: Vec<&str> = tool
        .split([':', '/', '\\'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect();
    let mut out = PathBuf::new();
    if parts.len() > MAX_TOOL_ID_SEGMENTS {
        for part in &parts[..MAX_TOOL_ID_SEGMENTS - 1] {
            out.push(part);
        }
        // Hash the joined tail rather than each segment, so two ids that differ
        // only in where a separator falls cannot collide.
        let tail = parts[MAX_TOOL_ID_SEGMENTS - 1..].join("/");
        let digest = blake3::hash(tail.as_bytes()).to_hex();
        out.push(format!("{FOLDED_TOOL_ID_PREFIX}{}", &digest[..32]));
        return out;
    }
    for part in parts {
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
    fn a_deep_tool_id_stays_within_the_inventory_scan_depth() {
        // Regression: an `http:` id expands one directory per URL segment, and
        // Google's Android CLI path pushed the receipt to depth 11 while the
        // inventory scanner stops at 9. The install then succeeded and every
        // later use reported a missing install identity, so the depth budget
        // is a correctness property, not tidiness.
        let deep = "http:https://dl.google.com/android/cli/{version}/windows_x86_64/android.exe";
        let path = sanitize_tool_id(deep);
        let segments = path.components().count();
        assert!(
            segments <= MAX_TOOL_ID_SEGMENTS,
            "{segments} segments: {}",
            path.display()
        );
        // installs + tool segments + version + install fingerprint must leave
        // the receipt reachable by a scanner bounded at DEFAULT_MAX_DEPTH + 1.
        assert!(segments + 3 <= 9, "receipt would sit below the scan depth");

        // Short ids keep one directory per segment: the readable layout is the
        // reason for expanding at all, so folding must not reach them.
        for (id, expected) in [
            ("node", "node"),
            ("github:owner/repo", "github/owner/repo"),
            ("npm:@scope/pkg", "npm/@scope/pkg"),
        ] {
            assert_eq!(
                sanitize_tool_id(id),
                PathBuf::from(expected.replace('/', std::path::MAIN_SEPARATOR_STR)),
                "{id}"
            );
        }
    }

    #[test]
    fn folding_a_tail_keeps_distinct_ids_distinct() {
        // The fold must not merge two different tools into one directory, and
        // hashing per segment would let a moved separator collide. Both of
        // these differ only in the tail.
        let a = sanitize_tool_id("http:https://host/a/b/c/d/e/one.exe");
        let b = sanitize_tool_id("http:https://host/a/b/c/d/e/two.exe");
        assert_ne!(a, b);

        // Same characters, different separator placement.
        let x = sanitize_tool_id("http:https://host/a/b/c/de/f");
        let y = sanitize_tool_id("http:https://host/a/b/c/d/ef");
        assert_ne!(x, y);

        // A folded component is self-identifying and stays a single component.
        let folded = sanitize_tool_id("http:https://host/a/b/c/d/e/f/g");
        let last = folded
            .components()
            .next_back()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap()
            .to_string();
        assert!(last.starts_with(FOLDED_TOOL_ID_PREFIX), "{last}");
        assert!(!last.contains(std::path::MAIN_SEPARATOR), "{last}");

        // Folding is deterministic across calls, or an install would be
        // unreachable after a restart.
        assert_eq!(sanitize_tool_id("http:https://host/a/b/c/d/e/f/g"), folded);
    }

    #[test]
    fn a_folded_id_cannot_escape_the_installs_root() {
        // Traversal segments are dropped before folding, so a crafted URL can
        // neither climb out nor smuggle a separator through the digest.
        let path = sanitize_tool_id("http:https://host/../../../a/b/c/d/e/f");
        for component in path.components() {
            let text = component.as_os_str().to_str().unwrap();
            assert_ne!(text, "..", "{}", path.display());
            assert_ne!(text, ".", "{}", path.display());
        }
        assert!(!path.is_absolute(), "{}", path.display());
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

    #[test]
    fn dynamic_locator_is_fingerprinted_and_keeps_legacy_root_explicit() {
        let mut env = HashMap::new();
        env.insert(env_keys::DATA_DIR.to_string(), "/x/data".to_string());
        env.insert(env_keys::CACHE_DIR.to_string(), "/x/cache".to_string());
        env.insert(env_keys::CONFIG_DIR.to_string(), "/x/config".to_string());
        let dirs = Dirs::resolve_from(|key| env.get(key).cloned()).unwrap();
        let identity = crate::tool::InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            "linux-x64",
            crate::tool::InstallScope::Global,
            &std::collections::BTreeMap::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let locator = InstallLocator::new(&dirs, identity.clone()).unwrap();
        assert_eq!(locator.identity(), &identity);
        assert_eq!(
            locator.install_root().parent(),
            Some(Path::new("/x/data/installs/npm-global/prettier/3.6.2"))
        );
        assert_eq!(
            locator.legacy_global_install_root(),
            Some(Path::new("/x/data/installs/npm-global/prettier/3.6.2"))
        );
        let leaf = locator
            .install_root()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(leaf.starts_with("b3-v2-"));
        assert!(!leaf.contains(':'));
        assert!(locator.lock_path().ends_with(format!("{leaf}.lock")));
        assert!(locator
            .scratch_root()
            .ends_with(Path::new(leaf.as_ref() as &str)));
        assert!(locator.validates_install_root(locator.install_root()));
        assert!(!locator.validates_install_root(locator.legacy_install_root()));

        let unsupported = crate::tool::InstallIdentity::new(
            "github:cli/cli",
            "2.0.0",
            "linux-x64",
            crate::tool::InstallScope::Global,
            &std::collections::BTreeMap::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert!(InstallLocator::new(&dirs, unsupported).is_err());
    }

    #[test]
    fn dynamic_locator_rejects_changed_and_aliased_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().join("data");
        let cache = temporary.path().join("cache");
        let config = temporary.path().join("config");
        let dirs = Dirs {
            store: data.join("store"),
            installs: data.join("installs"),
            data,
            cache,
            config,
        };
        let identity = crate::tool::InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            "linux-x64",
            crate::tool::InstallScope::Isolated,
            &std::collections::BTreeMap::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let locator = InstallLocator::new(&dirs, identity.clone()).unwrap();
        assert!(!locator.validates_install_root(&locator.install_root().join("..")));
        assert!(!InstallLocator::is_canonical_install_root(
            &dirs.installs,
            &identity,
            &dirs.installs.join("changed")
        )
        .unwrap());

        std::fs::create_dir_all(locator.install_root()).unwrap();
        assert!(locator.validates_install_root(locator.install_root()));
        assert!(locator.validates_existing_install_root(locator.install_root()));
    }

    #[cfg(unix)]
    #[test]
    fn dynamic_locator_rejects_a_symlinked_expected_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().join("data");
        let dirs = Dirs {
            store: data.join("store"),
            installs: data.join("installs"),
            data,
            cache: temporary.path().join("cache"),
            config: temporary.path().join("config"),
        };
        let identity = crate::tool::InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            "linux-x64",
            crate::tool::InstallScope::Isolated,
            &std::collections::BTreeMap::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let locator = InstallLocator::new(&dirs, identity.clone()).unwrap();
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir_all(locator.install_root().parent().unwrap()).unwrap();
        symlink(&outside, locator.install_root()).unwrap();

        assert!(!locator.validates_install_root(locator.install_root()));
        assert!(!locator.validates_existing_install_root(locator.install_root()));
        assert!(!InstallLocator::is_canonical_install_root(
            &dirs.installs,
            &identity,
            locator.install_root()
        )
        .unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn dynamic_locator_accepts_a_real_root_below_a_platform_alias() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real_data = temporary.path().join("real");
        std::fs::create_dir(&real_data).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&real_data, &alias).unwrap();
        let dirs = Dirs {
            store: alias.join("store"),
            installs: alias.join("installs"),
            data: alias.clone(),
            cache: alias.join("cache"),
            config: alias.join("config"),
        };
        let identity = crate::tool::InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            "linux-x64",
            crate::tool::InstallScope::Isolated,
            &std::collections::BTreeMap::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let locator = InstallLocator::new(&dirs, identity).unwrap();
        std::fs::create_dir_all(locator.install_root()).unwrap();

        assert!(locator.validates_install_root(locator.install_root()));
        assert!(locator.validates_existing_install_root(locator.install_root()));
    }
}
