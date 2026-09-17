use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PROJECT_CONFIG_NAMES;
use crate::error::{Error, Result};
use crate::lock::FileLock;

const TRUST_FILE_NAME: &str = "trusted-configs.toml";
const TRUST_LOCK_FILE_NAME: &str = "trusted-configs.lock";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrustStore {
    #[serde(default = "schema")]
    schema: u32,
    #[serde(default)]
    configs: Vec<TrustRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustRecord {
    pub path: PathBuf,
    pub hash: String,
}

fn schema() -> u32 {
    1
}

pub fn project_config(start: &Path) -> Result<Option<PathBuf>> {
    let start = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    for directory in start.ancestors() {
        for name in PROJECT_CONFIG_NAMES {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

pub fn resolve_config(path: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    let candidate = path.unwrap_or(cwd);
    if candidate.is_file() {
        return canonical_file(candidate);
    }
    let Some(config) = project_config(candidate)? else {
        return Err(Error::config(format!(
            "no osdk project config found from {}",
            candidate.display()
        )));
    };
    canonical_file(&config)
}

/// One reason a project config needs review, naming the exact key.
///
/// Carrying the key rather than a bare `true` is what lets the refusal say
/// *what* to look at. A person told only "this config is untrusted" has to
/// diff it against nothing; a person told that `settings.verify_signatures`
/// disables signature verification can decide in one glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRequirement {
    /// Dotted path to the offending key, e.g. `settings.verify_signatures`.
    pub key: String,
    /// Why this key is execution-affecting.
    pub reason: TrustReason,
}

/// The capability a key grants. These are the only two things trust gates.
///
/// Declaring *which package* to install is deliberately not here: npm installs
/// pass `--ignore-scripts`, `http:` artifacts require a pinned sha256, and
/// `go:` builds run with `CGO_ENABLED=0`, so a dependency declaration on its
/// own executes nothing the tool's publisher did not already ship. Treating it
/// as dangerous made every added package demand re-approval while teaching
/// nothing -- the gate cried wolf, which is how a real warning gets ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustReason {
    /// Runs arbitrary code on this machine during install.
    ExecutesCode,
    /// Weakens verification of what is installed, or redirects where it comes
    /// from. Dangerous in combination: an unverified mirror is both at once.
    WeakensVerification,
}

impl TrustReason {
    /// The localized explanation shown next to the key.
    pub fn describe(self) -> String {
        match self {
            Self::ExecutesCode => crate::t!("trust.reason.executes_code"),
            Self::WeakensVerification => crate::t!("trust.reason.weakens_verification"),
        }
    }
}

/// `[settings]` keys that can neither execute code nor weaken verification.
///
/// This is an allowlist, and an unlisted key defaults to requiring trust. A
/// newly added setting is therefore fail-closed: forgetting to classify it
/// makes configs ask for review needlessly (visible, mildly annoying, safe)
/// rather than letting a dangerous key through silently.
/// `settings_allowlist_covers_every_field` fails when a field is added without
/// a decision being recorded on either list.
///
/// `offline` is safe in the only direction it can move: it forbids network
/// access, never grants it.
///
/// `node` and `npm` are here after checking what they can actually express.
/// `node` holds a single bool, `corepack`, and `npm` a single two-way choice
/// between the npm and pnpm that osdk itself manages -- and that choice is only
/// the lowest-priority fallback, so it cannot override a project that declares
/// its own installer. See `corepack_and_installer_choice_are_safe` for why the
/// obvious reading of "runs corepack" does not make this a gate.
const SAFE_SETTINGS_KEYS: &[&str] = &[
    "jobs",
    "lang",
    "link_mode",
    "node",
    "npm",
    "offline",
    "prerelease",
    "shims",
    "yes",
];

/// `[settings]` keys that require trust, each with the reason it does.
///
/// Kept explicit rather than derived from "absent from the allowlist" so the
/// refusal can explain itself, and so the coverage test can tell a deliberate
/// classification apart from an omission.
const TRUST_REQUIRING_SETTINGS: &[(&str, TrustReason)] = &[
    ("verify_signatures", TrustReason::WeakensVerification),
    ("require_checksums", TrustReason::WeakensVerification),
    ("attestations", TrustReason::WeakensVerification),
    // Both carry `catalog_url`, a free-form URL that decides which interpreter
    // or runtime bytes get installed, and `python` also carries the
    // `catalog_sha256` that would otherwise pin them.
    ("python", TrustReason::WeakensVerification),
    ("java", TrustReason::WeakensVerification),
];

/// Top-level tables that require trust as a whole, with the reason.
///
/// `syspkg` installs into the machine outside the managed root, may prompt for
/// elevation, and is deliberately not covered by `osdk.lock`. `sources` and
/// `registries` change where subprocesses fetch from.
const TRUST_REQUIRING_TABLES: &[(&str, TrustReason)] = &[
    ("syspkg", TrustReason::ExecutesCode),
    ("sources", TrustReason::WeakensVerification),
    ("registries", TrustReason::WeakensVerification),
];

/// Top-level tables inspected key by key instead of judged as a whole.
///
/// `tools` needs this because a single tool option (`allow_builds`) can still
/// opt into script execution even though the surrounding table is safe.
const INSPECTED_TABLES: &[&str] = &["tools", "aliases", "settings"];

/// The npm tool option that turns lifecycle scripts back on.
const ALLOW_BUILDS_OPTION: &str = "allow_builds";

/// Collect every key in this config that requires review, in reporting order.
///
/// An empty result means the config is safe to load with no trust record.
pub fn trust_requirements(path: &Path) -> Result<Vec<TrustRequirement>> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    Ok(collect_requirements(&value))
}

/// Render the requirements as indented `key -- reason` lines for a message.
pub fn describe_requirements(requirements: &[TrustRequirement]) -> String {
    requirements
        .iter()
        .map(|requirement| {
            format!(
                "\n  {} -- {}",
                requirement.key,
                requirement.reason.describe()
            )
        })
        .collect()
}

fn collect_requirements(value: &toml::Value) -> Vec<TrustRequirement> {
    let Some(table) = value.as_table() else {
        return Vec::new();
    };
    let mut found = Vec::new();

    for (key, value) in table {
        match key.as_str() {
            "settings" => collect_settings_requirements(value, &mut found),
            "tools" => collect_tools_requirements(value, &mut found),
            "aliases" => {}
            other => {
                debug_assert!(!INSPECTED_TABLES.contains(&other));
                let reason = TRUST_REQUIRING_TABLES
                    .iter()
                    .find(|(name, _)| *name == other)
                    .map(|(_, reason)| *reason)
                    // An unrecognized top-level table is fail-closed: a table
                    // this build cannot interpret cannot be shown harmless.
                    .unwrap_or(TrustReason::ExecutesCode);
                found.push(TrustRequirement {
                    key: other.to_string(),
                    reason,
                });
            }
        }
    }
    found
}

fn collect_settings_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(settings) = value.as_table() else {
        // A `settings` key that is not a table is malformed, not safe.
        found.push(TrustRequirement {
            key: "settings".into(),
            reason: TrustReason::WeakensVerification,
        });
        return;
    };
    for key in settings.keys() {
        if SAFE_SETTINGS_KEYS.contains(&key.as_str()) {
            continue;
        }
        let reason = TRUST_REQUIRING_SETTINGS
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, reason)| *reason)
            // Unlisted means unclassified, which is treated as dangerous.
            .unwrap_or(TrustReason::WeakensVerification);
        found.push(TrustRequirement {
            key: format!("settings.{key}"),
            reason,
        });
    }
}

/// Inspect `[tools]` for the one option that grants code execution.
///
/// Declaring `npm:prettier` or `github:cli/cli` is not itself a reason: those
/// installs execute no package-authored code. `allow_builds` is, and it can
/// arrive either as a table field or inline in the request string.
fn collect_tools_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(tools) = value.as_table() else {
        return;
    };
    let inline_allows_builds = |spec: &str| {
        crate::version::ToolRequest::parse(spec).is_ok_and(|request| {
            request
                .options
                .get(ALLOW_BUILDS_OPTION)
                .is_some_and(|value| allow_builds_enabled(value))
        })
    };
    for (name, entry) in tools {
        let allows_builds = match entry {
            toml::Value::String(spec) => inline_allows_builds(spec),
            toml::Value::Table(fields) => {
                let declared = fields.get(ALLOW_BUILDS_OPTION).is_some_and(|value| {
                    value
                        .as_str()
                        .map(allow_builds_enabled)
                        // `allow_builds = true` is the natural TOML spelling.
                        .or_else(|| value.as_bool())
                        .unwrap_or(false)
                });
                declared
                    || fields
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .is_some_and(inline_allows_builds)
            }
            _ => false,
        };
        if allows_builds {
            found.push(TrustRequirement {
                key: format!("tools.{name}.{ALLOW_BUILDS_OPTION}"),
                reason: TrustReason::ExecutesCode,
            });
        }
    }
}

/// Whether an `allow_builds` value actually asks for scripts to run.
///
/// Mirrors the npm backend's own parsing. A package list still ends up denied
/// there (npm has no per-package allowlist), but it states the intent to build,
/// so it is reported rather than silently treated as harmless.
fn allow_builds_enabled(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    !matches!(
        raw.to_ascii_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

/// Hash only the keys that trust actually governs.
///
/// Hashing the whole file made trust much stricter than its own gate: once a
/// config contained a single trust-requiring key, *every* later edit
/// invalidated the record, so bumping a version in `[tools]` -- a change that
/// needs no trust on its own -- demanded re-approval. Both gates now read the
/// same subset, so a record survives exactly the edits that were never gated.
pub fn normalized_hash(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    let governed = governed_subset(&value);
    let normalized = toml::to_string(&governed)
        .map_err(|error| Error::config(format!("normalizing {}: {error}", path.display())))?;
    Ok(blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

/// Project the config down to the keys `trust_requirements` reports on.
///
/// Driven by the same requirement list as the gate, so the two cannot drift: a
/// key that is not a reason to ask for trust is also not a reason to
/// invalidate it.
fn governed_subset(value: &toml::Value) -> toml::Value {
    let mut subset = toml::value::Table::new();
    let Some(table) = value.as_table() else {
        return toml::Value::Table(subset);
    };
    for requirement in collect_requirements(value) {
        let mut segments = requirement.key.split('.');
        let Some(head) = segments.next() else {
            continue;
        };
        let Some(head_value) = table.get(head) else {
            continue;
        };
        match segments.next() {
            // A whole-table reason (`syspkg`, `sources`, an unknown table)
            // pins that table's entire content.
            None => {
                subset.insert(head.to_string(), head_value.clone());
            }
            // A key-level reason pins just that key, leaving unrelated
            // siblings editable.
            Some(field) => {
                let Some(field_value) = head_value.as_table().and_then(|table| table.get(field))
                else {
                    continue;
                };
                let nested = subset
                    .entry(head.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
                if let Some(nested) = nested.as_table_mut() {
                    // For `tools.<name>.allow_builds` this records the whole
                    // entry: the package identity is what approval covered.
                    nested.insert(field.to_string(), field_value.clone());
                }
            }
        }
    }
    toml::Value::Table(subset)
}

pub fn requires_trust(path: &Path) -> Result<bool> {
    Ok(!trust_requirements(path)?.is_empty())
}

pub fn is_trusted(
    config_dir: &Path,
    path: &Path,
    trusted_paths: Option<&OsString>,
) -> Result<bool> {
    let canonical = canonical_file(path)?;
    if trusted_paths
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter_map(|entry| canonical_existing(&entry).ok())
        .any(|entry| canonical == entry || canonical.starts_with(&entry))
    {
        return Ok(true);
    }

    let hash = normalized_hash(&canonical)?;
    Ok(read_store(config_dir)?
        .configs
        .iter()
        .any(|record| record.path == canonical && record.hash == hash))
}

pub fn trust(config_dir: &Path, path: &Path) -> Result<TrustRecord> {
    let path = canonical_file(path)?;
    let record = TrustRecord {
        hash: normalized_hash(&path)?,
        path,
    };
    update_store(config_dir, |store| {
        store
            .configs
            .retain(|existing| existing.path != record.path);
        store.configs.push(record.clone());
        store
            .configs
            .sort_by(|left, right| left.path.cmp(&right.path));
        (record, true)
    })
}

pub fn untrust(config_dir: &Path, path: &Path) -> Result<bool> {
    let path = canonical_file(path)?;
    update_store(config_dir, |store| {
        let original = store.configs.len();
        store.configs.retain(|record| record.path != path);
        let removed = store.configs.len() != original;
        (removed, removed)
    })
}

pub fn list(config_dir: &Path) -> Result<Vec<TrustRecord>> {
    Ok(read_store(config_dir)?.configs)
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    dunce::canonicalize(path).map_err(|error| Error::io(path, error))
}

fn canonical_file(path: &Path) -> Result<PathBuf> {
    let canonical = canonical_existing(path)?;
    if !canonical.is_file() {
        return Err(Error::config(format!(
            "trusted config path is not a file: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TRUST_FILE_NAME)
}

fn store_lock_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TRUST_LOCK_FILE_NAME)
}

fn update_store<T>(
    config_dir: &Path,
    update: impl FnOnce(&mut TrustStore) -> (T, bool),
) -> Result<T> {
    let _lock = FileLock::acquire(store_lock_path(config_dir))?;
    let mut store = read_store(config_dir)?;
    let (result, changed) = update(&mut store);
    if changed {
        write_store(config_dir, &store)?;
    }
    Ok(result)
}

fn read_store(config_dir: &Path) -> Result<TrustStore> {
    let path = store_path(config_dir);
    if !path.is_file() {
        return Ok(TrustStore {
            schema: schema(),
            configs: Vec::new(),
        });
    }
    let text = std::fs::read_to_string(&path).map_err(|error| Error::io(&path, error))?;
    let store: TrustStore = toml::from_str(&text)?;
    if store.schema != schema() {
        return Err(Error::config(format!(
            "unsupported trust store schema {}",
            store.schema
        )));
    }
    Ok(store)
}

fn write_store(config_dir: &Path, store: &TrustStore) -> Result<()> {
    std::fs::create_dir_all(config_dir).map_err(|error| Error::io(config_dir, error))?;
    let path = store_path(config_dir);
    let text = toml::to_string_pretty(store)
        .map_err(|error| Error::config(format!("serializing trust store: {error}")))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".trusted-configs.")
        .suffix(".tmp")
        .tempfile_in(config_dir)
        .map_err(|error| Error::io(config_dir, error))?;
    let temporary_path = temporary.path().to_path_buf();
    temporary
        .write_all(text.as_bytes())
        .map_err(|error| Error::io(&temporary_path, error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| Error::io(&temporary_path, error))?;
    let (temporary_file, temporary_path) = temporary
        .keep()
        .map_err(|error| Error::io(&temporary_path, error.error))?;
    drop(temporary_file);
    if let Err(error) = atomic_replace(&temporary_path, &path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    sync_parent_directory(config_dir)?;
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|error| Error::io(destination, error))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), flags) };
    if result == 0 {
        return Err(Error::io(destination, std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io(parent, error))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    use super::*;

    #[test]
    fn normalized_content_and_canonical_path_define_identity() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");
        std::fs::write(&config, "[sources]\nselection = \"ordered\"\n").unwrap();

        let traversal = repo.join("nested/../osdk.toml");
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        let record = trust(&config_dir, &traversal).unwrap();
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        assert_eq!(record.path, dunce::canonicalize(&config).unwrap());

        std::fs::write(&config, "[sources]\nselection = \"auto\"\n").unwrap();
        assert!(!is_trusted(&config_dir, &config, None).unwrap());
    }

    #[test]
    fn repeated_updates_replace_store_without_using_fixed_temporary_path() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = temp.path().join("osdk.toml");
        let legacy_temporary = config_dir.join("trusted-configs.toml.tmp");
        std::fs::write(&legacy_temporary, "do not overwrite").unwrap();

        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();
        let original = trust(&config_dir, &config).unwrap();
        std::fs::write(&config, "[settings]\nvalue = 2\n").unwrap();
        let updated = trust(&config_dir, &config).unwrap();

        assert_ne!(original.hash, updated.hash);
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        assert_eq!(
            std::fs::read_to_string(&legacy_temporary).unwrap(),
            "do not overwrite"
        );
        assert!(untrust(&config_dir, &config).unwrap());
        assert!(list(&config_dir).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_replaces_an_existing_store() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let config = temp.path().join("osdk.toml");

        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();
        trust(&config_dir, &config).unwrap();
        std::fs::write(&config, "[settings]\nvalue = 2\n").unwrap();
        trust(&config_dir, &config).unwrap();

        assert_eq!(list(&config_dir).unwrap().len(), 1);
        assert!(is_trusted(&config_dir, &config, None).unwrap());
    }

    #[test]
    fn concurrent_trust_updates_preserve_every_record() {
        const WRITERS: usize = 16;

        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let mut configs = Vec::with_capacity(WRITERS);
        for index in 0..WRITERS {
            let config = temp.path().join(format!("osdk-{index}.toml"));
            std::fs::write(&config, format!("[settings]\nvalue = {index}\n")).unwrap();
            configs.push(config);
        }

        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = configs
            .iter()
            .cloned()
            .map(|config| {
                let barrier = Arc::clone(&barrier);
                let config_dir = config_dir.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    trust(&config_dir, &config).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut expected: Vec<_> = configs
            .iter()
            .map(|config| dunce::canonicalize(config).unwrap())
            .collect();
        expected.sort();
        let actual: Vec<_> = list(&config_dir)
            .unwrap()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn trust_and_untrust_wait_for_the_store_lock() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let config = temp.path().join("osdk.toml");
        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();

        let lock = FileLock::acquire(store_lock_path(&config_dir)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let trust_config_dir = config_dir.clone();
        let trust_config = config.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(trust(&trust_config_dir, &trust_config).map(|_| ()))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(250)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lock);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        handle.join().unwrap();

        let lock = FileLock::acquire(store_lock_path(&config_dir)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let untrust_config_dir = config_dir.clone();
        let untrust_config = config.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(untrust(&untrust_config_dir, &untrust_config))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(250)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lock);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap());
        handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolves_to_target_but_repository_move_invalidates_trust() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");
        std::fs::write(&config, "[settings]\nyes = true\n").unwrap();
        trust(&config_dir, &config).unwrap();

        let link = temp.path().join("linked.toml");
        symlink(&config, &link).unwrap();
        assert!(is_trusted(&config_dir, &link, None).unwrap());

        let moved = temp.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        assert!(!is_trusted(&config_dir, &moved.join("osdk.toml"), None).unwrap());
    }

    /// The whole point of the revision: declaring a dependency is not a reason
    /// to ask for approval. Every namespace here installs without running
    /// package-authored code, so a project may add and bump tools freely.
    #[test]
    fn declaring_tools_and_aliases_never_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n[aliases.node]\ndefault = \"20\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());

        for tool in [
            "npm:prettier",
            "github:cli/cli",
            "go:github.com/user/cmd/tool",
            "cargo:ripgrep",
            "pypi:ruff",
            "conda:nasm",
            "http:https://downloads.example.test/tool-{version}",
        ] {
            std::fs::write(&path, format!("[tools]\n{tool:?} = \"1.2.3\"\n")).unwrap();
            assert!(!requires_trust(&path).unwrap(), "{tool}");
        }

        // The table form, including a chosen installer, is equally harmless.
        std::fs::write(
            &path,
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"pnpm\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
    }

    /// `allow_builds` is the one thing inside `[tools]` that grants execution,
    /// in either spelling, and it must be reported against its own key.
    #[test]
    fn allow_builds_requires_trust_in_every_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for body in [
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = true\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"yes\"\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"esbuild\"\n",
            "[tools]\nbundler = \"npm:esbuild[allow_builds=true]@0.21\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{body}");
            assert!(found[0].key.ends_with(".allow_builds"), "{body}");
            assert_eq!(found[0].reason, TrustReason::ExecutesCode, "{body}");
        }

        // A false-ish value leaves scripts denied, so it is not a reason.
        for body in [
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = false\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"off\"\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(!requires_trust(&path).unwrap(), "{body}");
        }
    }

    /// `settings.node` and `settings.npm` do not require trust.
    ///
    /// Both look alarming and are not. `node` holds one bool, `corepack`, which
    /// runs the corepack shipped inside that very Node install -- refusing when
    /// absent rather than fetching it -- and `enable` only writes shims into the
    /// install directory. Turning it off changes which shims exist, not which
    /// bytes are on the machine. Corepack does download a package manager later,
    /// but that is triggered at run time by `packageManager` in `package.json`,
    /// which trust has never governed; gating the bool would not prevent it.
    ///
    /// `npm` holds one choice between the npm and pnpm osdk already manages, and
    /// only as the lowest-priority fallback.
    ///
    /// A gate that cannot stop the thing it names is worse than no gate: it
    /// teaches people to accept trust prompts without reading them, so the one
    /// that really matters gets waved through too.
    #[test]
    fn corepack_and_installer_choice_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for body in [
            "[settings.node]\ncorepack = true\n",
            "[settings.npm]\ndefault_installer = \"pnpm\"\n",
            "[settings]\njobs = 4\n[settings.node]\ncorepack = true\n[settings.npm]\ndefault_installer = \"npm\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(
                !requires_trust(&path).unwrap(),
                "should need no trust: {body}"
            );
        }

        // Editing them must not invalidate an existing record either.
        let config_dir = temp.path().join("state");
        std::fs::write(
            &path,
            "[settings]\nverify_signatures = false\n[settings.node]\ncorepack = false\n",
        )
        .unwrap();
        trust(&config_dir, &path).unwrap();
        std::fs::write(
            &path,
            "[settings]\nverify_signatures = false\n[settings.node]\ncorepack = true\n[settings.npm]\ndefault_installer = \"pnpm\"\n",
        )
        .unwrap();
        assert!(is_trusted(&config_dir, &path, None).unwrap());
    }

    /// Each reported key must carry the reason a person needs to judge it, and
    /// safe siblings in the same table must not be dragged in.
    #[test]
    fn dangerous_keys_are_reported_individually_with_a_reason() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nlang = \"zh\"\nverify_signatures = false\nrequire_checksums = false\n",
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        let keys: Vec<_> = found.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            ["settings.require_checksums", "settings.verify_signatures"]
        );
        assert!(found
            .iter()
            .all(|item| item.reason == TrustReason::WeakensVerification));

        // The rendered message names each key and explains it.
        let described = describe_requirements(&found);
        assert!(described.contains("settings.verify_signatures"));
        assert!(described.contains(&TrustReason::WeakensVerification.describe()));
        assert!(!described.contains("settings.jobs"));
    }

    #[test]
    fn syspkg_sources_and_registries_require_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        std::fs::write(&path, "[syspkg.packages]\n\"winget:Foo\" = \"latest\"\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "syspkg");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);

        for (body, key) in [
            ("[sources]\nselection = \"ordered\"\n", "sources"),
            (
                "[registries.npm]\nurls = [\"https://registry.npmjs.org/\"]\n",
                "registries",
            ),
        ] {
            std::fs::write(&path, body).unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{body}");
            assert_eq!(found[0].key, key);
            assert_eq!(found[0].reason, TrustReason::WeakensVerification, "{body}");
        }
    }

    /// An unknown top-level table, and an unknown `[settings]` key, must both
    /// fail closed. A build that cannot interpret a key cannot clear it.
    #[test]
    fn unrecognized_keys_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        std::fs::write(&path, "[future_capability]\nvalue = 1\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "future_capability");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);

        std::fs::write(&path, "[settings]\nfuture_switch = true\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "settings.future_switch");
    }

    /// Every `Settings` field must appear on exactly one of the two lists.
    ///
    /// Without this, adding a field is silently classified by the fallback.
    /// That fallback is fail-closed so a new field cannot become a hole, but an
    /// unclassified field also cannot explain itself in the refusal, and a
    /// field that *is* safe would needlessly demand approval forever. The field
    /// names come from serializing a default `Settings`, so this test tracks the
    /// struct rather than a hand-copied list that would drift.
    #[test]
    fn settings_allowlist_covers_every_field() {
        let settings = crate::config::Settings::default();
        let serialized = toml::Value::try_from(&settings).unwrap();
        let table = serialized
            .as_table()
            .expect("settings serialize to a table");

        let mut unclassified = Vec::new();
        for key in table.keys() {
            let safe = SAFE_SETTINGS_KEYS.contains(&key.as_str());
            let dangerous = TRUST_REQUIRING_SETTINGS.iter().any(|(name, _)| name == key);
            if safe == dangerous {
                // Either on neither list, or contradictorily on both.
                unclassified.push(key.clone());
            }
        }
        assert!(
            unclassified.is_empty(),
            "these `Settings` fields are not classified for trust: {unclassified:?}. \
             Add each to SAFE_SETTINGS_KEYS or TRUST_REQUIRING_SETTINGS."
        );
    }

    /// Trust identity must follow the gate: edits to keys that never needed
    /// approval must not invalidate a record, and edits to keys that did must.
    #[test]
    fn only_governed_keys_invalidate_a_trust_record() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");

        let write = |body: &str| std::fs::write(&config, body).unwrap();
        write("[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n");
        trust(&config_dir, &config).unwrap();
        assert!(is_trusted(&config_dir, &config, None).unwrap());

        // Bumping a version, adding a dependency, adding an alias and changing
        // a safe setting are all ungated, so trust survives all of them.
        for body in [
            "[tools]\nnode = \"22\"\n\n[settings]\njobs = 4\nverify_signatures = false\n",
            "[tools]\nnode = \"22\"\nformatter = \"npm:prettier@3\"\n\n[settings]\njobs = 4\nverify_signatures = false\n",
            "[tools]\nnode = \"22\"\nformatter = \"npm:prettier@3\"\n[aliases.node]\ndefault = \"22\"\n\n[settings]\njobs = 12\nverify_signatures = false\n",
            "# a comment\n[settings]\nverify_signatures = false\njobs = 12\n[tools]\nnode = '22'\nformatter = \"npm:prettier@3\"\n[aliases.node]\ndefault = \"22\"\n",
        ] {
            write(body);
            assert!(
                is_trusted(&config_dir, &config, None).unwrap(),
                "trust should survive: {body}"
            );
        }

        // Restoring signature verification changes a governed key, so the
        // record no longer matches. Fail-closed applies in both directions:
        // identity is "what was approved", not "is this safer now".
        write("[tools]\nnode = \"22\"\n\n[settings]\njobs = 12\nverify_signatures = true\n");
        assert!(!is_trusted(&config_dir, &config, None).unwrap());

        // Introducing a governed key that was absent at approval time must
        // also invalidate it.
        write("[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n");
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        write(
            "[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n\n[syspkg.packages]\n\"winget:Foo\" = \"latest\"\n",
        );
        assert!(!is_trusted(&config_dir, &config, None).unwrap());
    }

    /// A config with nothing governed has no trust record to invalidate, so it
    /// must never be refused however much it changes.
    #[test]
    fn ungoverned_configs_are_never_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(&path, "[tools]\nnode = \"20\"\n").unwrap();
        let before = normalized_hash(&path).unwrap();
        std::fs::write(
            &path,
            "[tools]\nnode = \"24\"\nformatter = \"npm:prettier@3\"\ncli = \"github:cli/cli@2\"\n[aliases.node]\ndefault = \"24\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
        // Identical because neither version contains a governed key.
        assert_eq!(before, normalized_hash(&path).unwrap());
    }
}
