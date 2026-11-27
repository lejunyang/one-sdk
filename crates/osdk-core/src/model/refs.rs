//! Project ownership of materialized model snapshots in a shared data directory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::lock::FileLock;
use crate::project::root_id;

use super::{valid_snapshot_key, validate_model_name, write_json_atomic};

const SCHEMA: u32 = 1;

type SnapshotKeys = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectModelRefs {
    schema: u32,
    pub root: PathBuf,
    pub lock: PathBuf,
    updated_at: u64,
    models: SnapshotKeys,
    // Hold on to ownership across an ordinary sync and across interrupted
    // cleanup, without treating an abandoned snapshot as a live reference.
    retired: SnapshotKeys,
}

impl ProjectModelRefs {
    pub fn new(root: PathBuf, lock: PathBuf) -> Self {
        Self {
            schema: SCHEMA,
            root,
            lock,
            updated_at: 0,
            models: BTreeMap::new(),
            retired: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, name: &str, snapshot: &str) -> bool {
        let keys = BTreeSet::from([snapshot.to_owned()]);
        if self.models.get(name) == Some(&keys) {
            return false;
        }
        self.retire(name);
        if let Some(retired) = self.retired.get_mut(name) {
            retired.remove(snapshot);
            if retired.is_empty() {
                self.retired.remove(name);
            }
        }
        self.models.insert(name.to_owned(), keys);
        true
    }

    fn retire(&mut self, name: &str) {
        if let Some(keys) = self.models.remove(name) {
            self.retired
                .entry(name.to_owned())
                .or_default()
                .extend(keys);
        }
    }

    pub fn retire_unlocked(&mut self, locked: &BTreeSet<&str>, target: Option<&str>) -> bool {
        let names: Vec<_> = self
            .models
            .keys()
            .filter(|name| target.is_none_or(|target| name.as_str() == target))
            .filter(|name| !locked.contains(name.as_str()))
            .cloned()
            .collect();
        for name in &names {
            self.retire(name);
        }
        !names.is_empty()
    }

    fn validate(&self, id: &str) -> Result<()> {
        if self.schema != SCHEMA
            || !self.root.is_absolute()
            || root_id(&self.root) != id
            || !self.lock.is_absolute()
        {
            return Err(Error::other(
                "invalid project model reference identity or schema",
            ));
        }
        for (name, keys) in self.models.iter().chain(&self.retired) {
            validate_model_name(name)?;
            if keys.is_empty() || keys.iter().any(|key| !valid_snapshot_key(key)) {
                return Err(Error::other(format!(
                    "invalid snapshot keys for model {name}"
                )));
            }
        }
        Ok(())
    }
}

pub struct ModelRefs {
    directory: PathBuf,
}

impl ModelRefs {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            directory: dirs.data.join("model-refs"),
        }
    }

    /// Hold across snapshot publication or removal and the associated edits to
    /// project references. Readers only see complete records via atomic rename.
    pub fn lock(&self) -> Result<FileLock> {
        FileLock::acquire(self.directory.join("registry.lock"))
    }

    pub fn load(&self, root: &Path) -> Result<Option<ProjectModelRefs>> {
        let id = root_id(root);
        self.load_path(&self.directory.join(format!("{id}.json")), &id)
    }

    fn load_path(&self, path: &Path, id: &str) -> Result<Option<ProjectModelRefs>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io(path, error)),
        };
        let refs: ProjectModelRefs = serde_json::from_slice(&bytes).map_err(|error| {
            Error::other(format!(
                "invalid model references at {}: {error}",
                path.display()
            ))
        })?;
        refs.validate(id).map_err(|error| {
            Error::other(format!(
                "invalid model references at {}: {error}",
                path.display()
            ))
        })?;
        Ok(Some(refs))
    }

    /// The caller holds the registry lock.
    pub fn put_project(&self, refs: &mut ProjectModelRefs) -> Result<()> {
        let id = root_id(&refs.root);
        refs.validate(&id)?;
        refs.updated_at = crate::source::now_secs();
        write_json_atomic(&self.directory.join(format!("{id}.json")), refs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_refs_round_trip_and_keep_superseded_snapshot_keys() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("项目 A");
        let lock = root.join("osdk.lock");
        let registry = ModelRefs {
            directory: temporary.path().join("model-refs"),
        };
        let mut refs = ProjectModelRefs::new(root.clone(), lock);
        assert!(registry.load(&root).unwrap().is_none());
        let _guard = registry.lock().unwrap();
        let old = "ab".repeat(12);
        let new = "cd".repeat(12);
        assert!(refs.record("fixture", &old));
        assert!(refs.record("other", &old));
        assert!(refs.record("fixture", &new));
        assert!(!refs.retire_unlocked(&BTreeSet::from(["fixture"]), Some("fixture")));
        registry.put_project(&mut refs).unwrap();
        let mut loaded = registry.load(&root).unwrap().unwrap();
        assert_eq!(loaded.models["fixture"], BTreeSet::from([new]));
        assert_eq!(loaded.models["other"], BTreeSet::from([old.clone()]));
        assert_eq!(loaded.retired["fixture"], BTreeSet::from([old.clone()]));
        assert!(loaded.retire_unlocked(&BTreeSet::from(["fixture"]), None));
        assert_eq!(loaded.retired["other"], BTreeSet::from([old]));
    }
}
