//! Project ownership of materialized model snapshots in a shared data directory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::lock::FileLock;
use crate::project::root_id;

use super::view::{ViewKind, ViewState, ViewStore};
use super::{valid_snapshot_key, validate_model_name, write_json_atomic, ModelStore, ProviderId};

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

    pub fn forget_retired(&mut self, name: &str, snapshot: &str) {
        if let Some(keys) = self.retired.get_mut(name) {
            keys.remove(snapshot);
            if keys.is_empty() {
                self.retired.remove(name);
            }
        }
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

    pub fn load_all(&self) -> Result<Vec<ProjectModelRefs>> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(&self.directory, error)),
        };
        let mut projects = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|error| Error::io(&self.directory, error))?
                .path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    Error::other(format!("invalid model reference path: {}", path.display()))
                })?;
            let refs = self.load_path(&path, id)?.ok_or_else(|| {
                Error::other(format!("model reference disappeared: {}", path.display()))
            })?;
            projects.push(refs);
        }
        projects.sort_by(|left, right| left.root.cmp(&right.root));
        Ok(projects)
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

#[derive(Debug)]
pub enum PruneAction {
    RemoveSnapshot,
    RemoveModel,
    Skip {
        reason: String,
        owner: Option<PathBuf>,
    },
}

impl PruneAction {
    fn skip(reason: impl Into<String>) -> Self {
        Self::Skip {
            reason: reason.into(),
            owner: None,
        }
    }
}

#[derive(Debug)]
pub struct PruneEntry {
    pub name: String,
    pub snapshot: String,
    pub path: PathBuf,
    pub action: PruneAction,
}

// Shared caches can live on case-insensitive filesystems. Err on the side of
// retaining data for aliases of the same on-disk name.
pub(crate) fn same_model_name(left: &str, right: &str) -> bool {
    fn component(name: &str) -> &str {
        match name {
            "." | ".." => "_",
            _ => name.trim_end_matches('.'),
        }
    }
    component(left).eq_ignore_ascii_case(component(right))
}

pub fn plan_prune(
    store: &ModelStore,
    project: &ProjectModelRefs,
    registered: &[ProjectModelRefs],
    pending_downloads: &BTreeSet<String>,
) -> Result<Vec<PruneEntry>> {
    if project.retired.is_empty() {
        return Ok(Vec::new());
    }
    let views = ViewStore::new(store.dirs.clone(), store.cas.clone(), store.link_mode);
    let state = ViewState::load(&store.dirs)?;
    let owners = std::iter::once(project)
        .chain(registered.iter().filter(|other| other.root != project.root));
    let mut plan = Vec::new();
    for (name, keys) in &project.retired {
        let snapshots = store.snapshots(name)?;
        let current = store.current_snapshot(name)?;
        let stable = store.model_root(name).join(super::CURRENT_LINK);
        let stable_target = match dunce::canonicalize(&stable) {
            Ok(target) => Some(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::io(&stable, error)),
        };
        let has_view = ViewKind::all().iter().any(|kind| {
            state
                .profiles(*kind)
                .values()
                .flatten()
                .any(|spec| same_model_name(&spec.model, name))
        });
        let mut candidates = Vec::new();
        let snapshot_count = snapshots.len();
        for entry in snapshots
            .into_iter()
            .filter(|entry| keys.contains(&entry.snapshot))
        {
            let is_current = current.as_deref() == Some(entry.snapshot.as_str());
            let owner = owners.clone().find(|other| {
                other.models.iter().any(|(other_name, keys)| {
                    same_model_name(other_name, name) && keys.contains(&entry.snapshot)
                })
            });
            let action = match (entry.manifest, owner) {
                (Err(error), _) => PruneAction::skip(format!("unreadable snapshot manifest: {error}")),
                (Ok(manifest), _) if manifest.schema != 1 || !same_model_name(&manifest.name, name) =>
                    PruneAction::skip("invalid snapshot manifest identity"),
                (Ok(manifest), _) if manifest.provider == ProviderId::Local =>
                    PruneAction::skip("local import"),
                (Ok(_), Some(owner)) => {
                    let root = &owner.root;
                    PruneAction::Skip {
                        reason: format!("still referenced by {}", root.display()),
                        owner: Some(root.clone()),
                    }
                }
                (Ok(_), _) if pending_downloads.contains(name) =>
                    PruneAction::skip("sync changes this model; run prune again after sync"),
                (Ok(_), _) if is_current && has_view =>
                    PruneAction::skip(format!("still in a model view; use `osdk model view remove` or `osdk model unuse {name}`")),
                (Ok(_), _) => {
                    let path = dunce::canonicalize(&entry.path).map_err(|error| Error::io(&entry.path, error))?;
                    if !is_current && stable_target.as_ref() == Some(&path) {
                        PruneAction::skip("still used by the stable current link")
                    } else if !is_current && has_view && views.links_into_snapshot(&state, name, &path)? {
                        PruneAction::skip("still linked from a model view; rebuild or remove that view")
                    } else {
                        PruneAction::RemoveSnapshot
                    }
                }
            };
            candidates.push(PruneEntry {
                name: name.clone(),
                snapshot: entry.snapshot,
                path: entry.path,
                action,
            });
        }
        let remove_tree = candidates.len() == snapshot_count
            && candidates
                .iter()
                .all(|entry| matches!(entry.action, PruneAction::RemoveSnapshot));
        if let Some(index) = candidates
            .iter()
            .position(|entry| current.as_ref() == Some(&entry.snapshot))
        {
            let mut entry = candidates.remove(index);
            if matches!(entry.action, PruneAction::RemoveSnapshot) {
                entry.action = if remove_tree {
                    PruneAction::RemoveModel
                } else {
                    PruneAction::skip("current snapshot; other snapshots must be kept")
                };
            }
            candidates.push(entry);
        }
        plan.extend(candidates);
    }
    Ok(plan)
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
