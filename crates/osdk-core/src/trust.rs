use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PROJECT_CONFIG_NAMES;
use crate::error::{Error, Result};

const TRUST_FILE_NAME: &str = "trusted-configs.toml";

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
    Ok(table
        .keys()
        .any(|key| !matches!(key.as_str(), "tools" | "aliases")))
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
    let mut store = read_store(config_dir)?;
    store
        .configs
        .retain(|existing| existing.path != record.path);
    store.configs.push(record.clone());
    store
        .configs
        .sort_by(|left, right| left.path.cmp(&right.path));
    write_store(config_dir, &store)?;
    Ok(record)
}

pub fn untrust(config_dir: &Path, path: &Path) -> Result<bool> {
    let path = canonical_file(path)?;
    let mut store = read_store(config_dir)?;
    let original = store.configs.len();
    store.configs.retain(|record| record.path != path);
    let removed = store.configs.len() != original;
    if removed {
        write_store(config_dir, &store)?;
    }
    Ok(removed)
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
    let temporary = path.with_extension("toml.tmp");
    let text = toml::to_string_pretty(store)
        .map_err(|error| Error::config(format!("serializing trust store: {error}")))?;
    std::fs::write(&temporary, text).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, &path).map_err(|error| Error::io(&path, error))
}

#[cfg(test)]
mod tests {
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
    }
}
