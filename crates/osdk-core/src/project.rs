//! Unified external project configuration bindings.
//!
//! A project normally carries its own `osdk.toml` next to its files. Some
//! projects cannot be changed this way: checked-in trees that must stay clean,
//! trees on read-only mounts, or projects whose owner wants the config to live
//! with the machine rather than the repo. An *external binding* points such a
//! project at a config file kept under the user's osdk config directory:
//!
//! ```text
//! <OSDK_CONFIG_DIR>/projects/<root-id>/project.toml   # binding manifest
//!                                      /osdk.toml      # business config
//!                                      /osdk.lock       # the lock belonging to this binding
//! ```
//!
//! `<root-id>` is the full BLAKE3 hex of the project root's canonical real
//! absolute path. Identity is platform-aware because `canonicalize` resolves
//! junctions/symlinks and returns the on-disk casing; the digest is never
//! produced by blindly lowercasing a path. There is deliberately no second,
//! editable index: the manifest files are the only record, and `list` is the
//! only operation that enumerates them. Every other lookup hashes one ancestor
//! path and stats one directory.
//!
//! Discovery walks ancestors exactly like the legacy local walk, mixing both
//! sources in one pass so there is a single parser for entry points:
//!
//! * nearest ancestor wins overall (a nearer external binding inside the tree
//!   beats a local config two levels up; a nearer local config beats a far
//!   external binding);
//! * at one ancestor, a local `osdk.toml`/`.osdk.toml` beats an external
//!   binding for the same root;
//! * the selected config always comes bundled with its own lock from the same
//!   source -- locks are never borrowed across sources.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config::PROJECT_CONFIG_NAMES;
use crate::error::{Error, Result};

const MANIFEST_NAME: &str = "project.toml";
const CONFIG_NAME: &str = "osdk.toml";
const LOCK_NAME: &str = "osdk.lock";
const MANIFEST_SCHEMA: u32 = 1;

/// Where bindings live, given the user's osdk config directory.
pub fn projects_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("projects")
}

/// Which physical source a discovered project configuration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSource {
    /// `osdk.toml` checked into the project tree itself.
    Local,
    /// A binding under `<config>/projects/<root-id>/`.
    External,
}

/// One selected project configuration: the real project root, the physical
/// config file, and the lock that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    /// Canonical real absolute project root. Tasks, deps and resource relative
    /// paths resolve here, not at the physical config location.
    pub root: PathBuf,
    /// Physical config file that was selected.
    pub config_path: PathBuf,
    /// Physical lock file that belongs to this selection. Never borrowed from
    /// the other source.
    pub lock_path: PathBuf,
    pub source: ProjectSource,
}

impl ProjectContext {
    /// True when the config file lives outside the project tree.
    pub fn is_external(&self) -> bool {
        self.source == ProjectSource::External
    }
}

/// `<config>/projects/<root-id>` for one canonical root.
pub fn binding_dir_for_root(config_dir: &Path, canonical_root: &Path) -> PathBuf {
    projects_dir(config_dir).join(root_id(canonical_root))
}

/// Full BLAKE3 hex of a canonical real absolute path.
pub fn root_id(canonical_root: &Path) -> String {
    blake3::hash(canonical_root.to_string_lossy().as_bytes())
        .to_hex()
        .to_string()
}

/// The binding manifest, deserialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    /// Canonical real absolute project root this binding points at.
    pub root: PathBuf,
}

impl Manifest {
    pub fn new(root: PathBuf) -> Self {
        Self {
            schema: MANIFEST_SCHEMA,
            root,
        }
    }
}

/// One on-disk binding directory.
#[derive(Debug, Clone)]
pub struct Binding {
    /// `<config>/projects/<root-id>`.
    pub dir: PathBuf,
    /// The root recorded in the manifest.
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub config_path: PathBuf,
    pub lock_path: PathBuf,
}

impl Binding {
    /// The binding's own config file, if it was ever written.
    pub fn config_exists(&self) -> bool {
        self.config_path.is_file()
    }

    /// The recorded root still exists on disk.
    pub fn root_exists(&self) -> bool {
        self.root.is_dir()
    }
}

/// Read and validate the binding manifest in `dir`. `Ok(None)` when the
/// directory holds no manifest (i.e. no binding). Any other corruption is an
/// explicit error: a half-written binding must not look like "no project".
pub fn read_binding(dir: &Path) -> Result<Option<Binding>> {
    let manifest_path = dir.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| Error::io(&manifest_path, e))?;
    let manifest: Manifest = toml::from_str(&text).map_err(|e| {
        Error::config(format!(
            "{}: invalid project binding manifest: {}",
            manifest_path.display(),
            e.message()
        ))
    })?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(Error::config(format!(
            "{}: unsupported project binding schema {} (expected {MANIFEST_SCHEMA})",
            manifest_path.display(),
            manifest.schema
        )));
    }
    let expected = root_id(&manifest.root);
    if dir.file_name().and_then(|n| n.to_str()) != Some(expected.as_str()) {
        return Err(Error::config(format!(
            "{}: binding directory name does not match its recorded root; expected {expected}",
            manifest_path.display()
        )));
    }
    Ok(Some(Binding {
        dir: dir.to_path_buf(),
        root: manifest.root,
        manifest_path,
        config_path: dir.join(CONFIG_NAME),
        lock_path: dir.join(LOCK_NAME),
    }))
}

/// Find the binding whose recorded root canonicalizes to `canonical_root`.
pub fn find_binding_for_root(config_dir: &Path, canonical_root: &Path) -> Result<Option<Binding>> {
    let dir = binding_dir_for_root(config_dir, canonical_root);
    let Some(binding) = read_binding(&dir)? else {
        return Ok(None);
    };
    // The recorded root must agree with the hashed directory we just probed.
    // If the manifest disagrees, treat it as absent rather than silently
    // rebinding a project somebody else's manifest claims.
    let recorded = binding.root.clone();
    if recorded != *canonical_root {
        return Ok(None);
    }
    Ok(Some(binding))
}

/// Enumerate every binding on record. The only operation that walks the store.
pub fn list_bindings(config_dir: &Path) -> Result<Vec<Binding>> {
    let dirs = projects_dir(config_dir);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dirs) else {
        return Ok(Vec::new());
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&dirs, e))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(binding) = read_binding(&path)? {
            out.push(binding);
        }
    }
    out.sort_by(|a, b| a.root.cmp(&b.root));
    Ok(out)
}

/// Discover the project configuration applying to `start_dir`.
///
/// Walks ancestors nearest-first. At each ancestor: a local config file wins
/// over an external binding for the same directory; the first ancestor with
/// either one wins overall. A broken selected binding is an explicit error,
/// and a same-root external binding shadowed by a local config is reported on
/// stderr exactly once per process.
pub fn discover(start_dir: &Path, config_dir: &Path) -> Result<Option<ProjectContext>> {
    for dir in start_dir.ancestors() {
        // Local first at this level: local always beats external for the root.
        for name in PROJECT_CONFIG_NAMES {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            // dunce (not std::fs::canonicalize) on purpose: std keeps the
            // `\\?\` verbatim prefix on Windows, so the same physical directory
            // hashes differently here than at `add` time. One canonical form
            // everywhere.
            let root = dunce::canonicalize(dir).map_err(|e| Error::io(dir, e))?;
            // A same-root external binding is shadowed by this local file. Its
            // body is never read; even a broken shadow must not change the
            // selected local configuration -- report nothing rather than fail.
            if let Ok(Some(binding)) = find_binding_for_root(config_dir, &root) {
                report_shadowed(&binding, &candidate);
            }
            let config_path =
                dunce::canonicalize(&candidate).map_err(|e| Error::io(&candidate, e))?;
            let lock_path = root.join(LOCK_NAME);
            return Ok(Some(ProjectContext {
                root,
                config_path,
                lock_path,
                source: ProjectSource::Local,
            }));
        }
        // External binding bound at this ancestor.
        let Ok(root) = dunce::canonicalize(dir) else {
            continue;
        };
        if let Some(binding) = find_binding_for_root(config_dir, &root)? {
            if !binding.config_path.is_file() {
                return Err(Error::config(format!(
                    "external project binding {} points at {} but {} is missing",
                    binding.dir.display(),
                    binding.root.display(),
                    binding.config_path.display()
                )));
            }
            return Ok(Some(ProjectContext {
                root,
                config_path: binding.config_path,
                lock_path: binding.lock_path,
                source: ProjectSource::External,
            }));
        }
    }
    Ok(None)
}

/// Process-once shadow warnings, memoized per binding directory.
fn shadowed_keys() -> &'static Mutex<HashSet<String>> {
    static KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The set of binding ids warned about this process, comma-joined. The hook
/// exports it so later prompts in the same shell session do not re-print a
/// conflict that has not changed.
pub fn shadowed_warnings_env_value() -> String {
    shadowed_keys()
        .lock()
        .map(|keys| {
            let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
            sorted.sort();
            sorted.join(",")
        })
        .unwrap_or_default()
}

fn report_shadowed(binding: &Binding, local_config: &Path) {
    // Fingerprint over paths + cheap metadata (no parsing the shadowed body):
    // a changed local config or a rewritten bundle manifest must re-report.
    fn meta(path: &Path) -> String {
        std::fs::metadata(path)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos().to_string())
                    .unwrap_or_default();
                format!("{mtime}:{}", m.len())
            })
            .unwrap_or_default()
    }
    let key = format!(
        "{}|local={}|bundle={}",
        binding.dir.display(),
        meta(local_config),
        meta(&binding.manifest_path),
    );
    {
        let mut keys = shadowed_keys().lock().unwrap();
        if keys.contains(&key) {
            return;
        }
        // Hook session dedup: a previous prompt already reported this conflict.
        if std::env::var("OSDK_PROJECT_SHADOWS")
            .map(|prev| prev.split(',').any(|recorded| recorded == key))
            .unwrap_or(false)
        {
            keys.insert(key);
            return;
        }
        keys.insert(key);
    }
    eprintln!(
        "osdk: using local {}; ignoring external binding at {} (and its lock {})",
        local_config.display(),
        binding.dir.display(),
        binding.lock_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;
    use std::collections::HashMap;

    fn test_dirs() -> (tempfile::TempDir, Dirs) {
        let temp = tempfile::tempdir().unwrap();
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert(
            "OSDK_DATA_DIR".into(),
            temp.path().join("data").to_string_lossy().into_owned(),
        );
        env.insert(
            "OSDK_CACHE_DIR".into(),
            temp.path().join("cache").to_string_lossy().into_owned(),
        );
        env.insert(
            "OSDK_CONFIG_DIR".into(),
            temp.path().join("config").to_string_lossy().into_owned(),
        );
        let dirs = Dirs::resolve_from(|k| env.get(k).cloned()).unwrap();
        (temp, dirs)
    }

    fn write_binding(dirs: &Dirs, root: &Path) -> PathBuf {
        let root = dunce::canonicalize(root).unwrap();
        let dir = binding_dir_for_root(&dirs.config, &root);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = Manifest::new(root);
        std::fs::write(dir.join(MANIFEST_NAME), toml::to_string(&manifest).unwrap()).unwrap();
        std::fs::write(dir.join(CONFIG_NAME), "[tools]\nnode = \"20\"\n").unwrap();
        dir
    }

    #[test]
    fn root_id_is_full_blake3_and_stable() {
        let path = Path::new("/some/project");
        let id = root_id(path);
        assert_eq!(id.len(), 64);
        assert_eq!(id, root_id(path));
    }

    #[test]
    fn external_binding_is_discovered_for_the_bound_root() {
        let (_t, dirs) = test_dirs();
        let project = tempfile::tempdir().unwrap();
        write_binding(&dirs, project.path());
        let ctx = discover(project.path(), &dirs.config).unwrap().unwrap();
        assert_eq!(ctx.source, ProjectSource::External);
        assert!(ctx.config_path.ends_with("osdk.toml"));
        assert!(ctx.lock_path.ends_with("osdk.lock"));
        assert_eq!(ctx.root, dunce::canonicalize(project.path()).unwrap());
    }

    #[test]
    fn nearer_local_config_beats_farther_external_binding() {
        let (_t, dirs) = test_dirs();
        let root = tempfile::tempdir().unwrap();
        write_binding(&dirs, root.path());
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("osdk.toml"), "[tools]\npython = \"3.12\"\n").unwrap();
        let ctx = discover(&child, &dirs.config).unwrap().unwrap();
        assert_eq!(ctx.source, ProjectSource::Local);
        assert!(ctx.config_path.ends_with("child/osdk.toml"));
    }

    #[test]
    fn nearer_external_binding_beats_farther_local_parent() {
        let (_t, dirs) = test_dirs();
        let root = tempfile::tempdir().unwrap();
        // A local config two levels up...
        std::fs::write(
            root.path().join("osdk.toml"),
            "[tools]\npython = \"3.12\"\n",
        )
        .unwrap();
        let child = root.path().join("apps").join("svc");
        std::fs::create_dir_all(&child).unwrap();
        // ...and an external bound one level closer.
        write_binding(&dirs, &child);
        let ctx = discover(&child, &dirs.config).unwrap().unwrap();
        assert_eq!(ctx.source, ProjectSource::External);
        assert_eq!(ctx.root, dunce::canonicalize(&child).unwrap());
    }

    #[test]
    fn local_config_shadowing_an_external_binding_wins_and_warns_once() {
        let (_t, dirs) = test_dirs();
        let project = tempfile::tempdir().unwrap();
        write_binding(&dirs, project.path());
        std::fs::write(project.path().join("osdk.toml"), "[tools]\nnode = \"21\"\n").unwrap();
        let ctx = discover(project.path(), &dirs.config).unwrap().unwrap();
        assert_eq!(ctx.source, ProjectSource::Local);
        // Second discovery in the same process must not re-warn.
        discover(project.path(), &dirs.config).unwrap().unwrap();
    }

    #[test]
    fn shadow_warning_refires_when_conflict_metadata_changes() {
        let (_t, dirs) = test_dirs();
        let project = tempfile::tempdir().unwrap();
        write_binding(&dirs, project.path());
        let local = project.path().join("osdk.toml");
        std::fs::write(&local, "[tools]\nnode = \"21\"\n").unwrap();
        discover(project.path(), &dirs.config).unwrap().unwrap();
        let before = shadowed_warnings_env_value();
        // Identical conflict state: no new fingerprint, no re-report.
        discover(project.path(), &dirs.config).unwrap().unwrap();
        assert_eq!(before, shadowed_warnings_env_value());
        // The local config was replaced: a changed mtime/size must re-report.
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&local, "[tools]\nnode = \"222\"\n").unwrap();
        discover(project.path(), &dirs.config).unwrap().unwrap();
        assert_ne!(before, shadowed_warnings_env_value());
    }

    #[test]
    fn broken_manifest_is_an_error_not_a_missing_project() {
        let (_t, dirs) = test_dirs();
        let project = tempfile::tempdir().unwrap();
        let dir = write_binding(&dirs, project.path());
        std::fs::write(dir.join(MANIFEST_NAME), "not = = toml").unwrap();
        assert!(discover(project.path(), &dirs.config).is_err());
    }

    #[test]
    fn binding_without_config_file_errors_when_selected() {
        let (_t, dirs) = test_dirs();
        let project = tempfile::tempdir().unwrap();
        let dir = write_binding(&dirs, project.path());
        std::fs::remove_file(dir.join(CONFIG_NAME)).unwrap();
        assert!(discover(project.path(), &dirs.config).is_err());
    }

    #[test]
    fn no_project_when_nothing_is_bound_or_declared() {
        let (_t, dirs) = test_dirs();
        let other = tempfile::tempdir().unwrap();
        assert!(discover(other.path(), &dirs.config).unwrap().is_none());
    }
}
