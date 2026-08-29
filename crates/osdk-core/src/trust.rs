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

pub fn normalized_hash(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    let normalized = toml::to_string(&value)
        .map_err(|error| Error::config(format!("normalizing {}: {error}", path.display())))?;
    Ok(blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

pub fn requires_trust(path: &Path) -> Result<bool> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    let Some(table) = value.as_table() else {
        return Ok(false);
    };
    let dynamic_tool_activation = table
        .get("tools")
        .and_then(toml::Value::as_table)
        .is_some_and(|tools| {
            tools.iter().any(|(key, value)| {
                is_recognized_dynamic_tool(key)
                    || value.as_str().is_some_and(is_recognized_dynamic_request)
                    || value
                        .as_table()
                        .and_then(|entry| entry.get("version"))
                        .and_then(toml::Value::as_str)
                        .is_some_and(is_recognized_dynamic_request)
            })
        });
    Ok(dynamic_tool_activation
        || table
            .keys()
            .any(|key| !matches!(key.as_str(), "tools" | "aliases")))
}

fn is_recognized_dynamic_tool(value: &str) -> bool {
    crate::tool::ToolId::parse(value).is_ok_and(|tool| tool.is_dynamic())
}

fn is_recognized_dynamic_request(value: &str) -> bool {
    crate::version::ToolRequest::parse(value)
        .is_ok_and(|request| is_recognized_dynamic_tool(&request.backend))
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

    #[test]
    fn safe_pins_and_aliases_do_not_require_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n[aliases.node]\ndefault = \"20\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
        std::fs::write(&path, "[tools]\nnode = \"20\"\n[settings]\nyes = true\n").unwrap();
        assert!(requires_trust(&path).unwrap());
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n[registries.npm]\nurls = [\"https://registry.npmjs.org/\"]\n",
        )
        .unwrap();
        assert!(requires_trust(&path).unwrap());
    }

    #[test]
    fn npm_project_tool_activation_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"aube\"\n",
        )
        .unwrap();
        assert!(requires_trust(&path).unwrap());

        std::fs::write(&path, "[tools]\nformatter = \"npm:prettier@3\"\n").unwrap();
        assert!(requires_trust(&path).unwrap());
    }

    #[test]
    fn http_project_tool_activation_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools.\"http:https://downloads.example.test/tool-{version}\"]\nversion = \"1.2.3\"\nsha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
        )
        .unwrap();
        assert!(requires_trust(&path).unwrap());

        std::fs::write(
            &path,
            "[tools]\nfixture = \"http:https://downloads.example.test/tool-{version}[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3\"\n",
        )
        .unwrap();
        assert!(requires_trust(&path).unwrap());
    }

    #[test]
    fn every_recognized_dynamic_namespace_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        for tool in [
            "npm:prettier",
            "github:cli/cli",
            "http:https://downloads.example.test/tool-{version}",
        ] {
            std::fs::write(&path, format!("[tools]\n{tool:?} = \"1.2.3\"\n")).unwrap();
            assert!(requires_trust(&path).unwrap(), "{tool}");
        }
        std::fs::write(&path, "[tools]\nfixture = \"unknown:tool@1.2.3\"\n").unwrap();
        assert!(!requires_trust(&path).unwrap());
    }
}
