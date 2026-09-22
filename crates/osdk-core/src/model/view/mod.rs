//! Consumer-shaped views over immutable model snapshots.
//!
//! A snapshot is laid out the way the upstream repository is (e.g. a diffusion
//! repo with `unet/`, `vae/`, `text_encoder/` side by side). Consumers expect a
//! different shape: ComfyUI wants `<root>/<category>/<file>`, the Hugging Face
//! hub client wants `models--org--repo/{refs,blobs,snapshots}`. A *view* renders
//! one of those shapes from a snapshot without copying its bytes -- each entry
//! is a link (hardlink on the same volume, a copy across volumes, or a
//! directory junction/symlink for whole-component directories).
//!
//! Views never carry content of their own: rebuilding or removing a view must
//! not touch the snapshot or the CAS objects behind it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::model::{safe_relative_path, validate_model_name, InstalledModel, ModelStore};
use crate::store::link::LinkMode;

pub mod comfyui;
pub mod hf_cache;
pub mod state;

pub use hf_cache::repo_dir_name;
pub use state::{ViewConsumer, ViewEntrySpec, ViewState};

/// A consumer view shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewKind {
    /// ComfyUI's `<models>/<category>/...` layout.
    Comfyui,
    /// The Hugging Face hub cache layout
    /// (`models--org--repo/{refs,blobs,snapshots}`).
    HfCache,
}

impl ViewKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comfyui => "comfyui",
            Self::HfCache => "hf-cache",
        }
    }

    pub fn all() -> &'static [ViewKind] {
        &[Self::Comfyui, Self::HfCache]
    }
}

impl std::fmt::Display for ViewKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ViewKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "comfyui" => Ok(Self::Comfyui),
            "hf-cache" | "hfcache" => Ok(Self::HfCache),
            other => Err(Error::config(format!(
                "unknown model view kind `{other}` (expected comfyui|hf-cache)"
            ))),
        }
    }
}

/// One model's membership in one view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    pub model: String,
    /// Explicit prefix mappings supplied by the user
    /// (repo path prefix -> consumer category). Empty means "use the
    /// renderer's built-in conventions".
    pub map: BTreeMap<String, String>,
}

/// How a planned entry is materialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceSource {
    /// Link/copy the snapshot file at this absolute path.
    Link(PathBuf),
    /// Write these literal bytes (a tiny generated file such as a hub refs
    /// pointer).
    Write(Vec<u8>),
}

/// One planned entry: a root-relative consumer path and how to fill it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Consumer-relative path with `/` separators.
    pub relative: String,
    pub source: PlaceSource,
}

/// Result of planning a render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    /// Snapshot files the renderer could not assign. fail-closed: not placed.
    pub unclassified: Vec<String>,
}

/// How a single repository-relative file maps into a consumer view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub category: String,
    pub file_name: String,
}

impl Plan {
    pub fn link(&mut self, relative: impl Into<String>, source: PathBuf) {
        self.files.push(PlannedFile {
            relative: relative.into(),
            source: PlaceSource::Link(source),
        });
    }

    pub fn write(&mut self, relative: impl Into<String>, bytes: Vec<u8>) {
        self.files.push(PlannedFile {
            relative: relative.into(),
            source: PlaceSource::Write(bytes),
        });
    }
}

/// Outcome after materializing a plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderReport {
    pub placed: Vec<String>,
    pub unclassified: Vec<String>,
    pub copies: usize,
}

/// Persisted ownership of every consumer-relative path in a view profile.
///
/// ComfyUI categories are shared across models, so a rebuild cannot delete a
/// category directory -- that would remove another model's links. This records
/// which model owns each placed path, so incremental rebuilds and per-model
/// removal touch only that model's entries; it also makes same-filename
/// collisions across models detectable instead of last-write-wins.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ViewManifest {
    #[serde(default)]
    owners: BTreeMap<String, String>,
}

const VIEW_MANIFEST_FILE: &str = ".osdk-view.json";

pub struct ViewStore {
    dirs: Dirs,
    models: ModelStore,
}

impl ViewStore {
    pub fn new(dirs: Dirs, cas: std::sync::Arc<crate::store::Cas>, link_mode: LinkMode) -> Self {
        let models = ModelStore::new(dirs.clone(), cas, link_mode);
        Self { dirs, models }
    }

    pub fn view_root(&self, kind: ViewKind, profile: &str) -> Result<PathBuf> {
        validate_profile(profile)?;
        Ok(self.dirs.model_views().join(kind.as_str()).join(profile))
    }

    /// Render (incrementally) the listed models. Models not pulled are skipped.
    pub fn render(
        &self,
        kind: ViewKind,
        profile: &str,
        entries: &[ViewEntry],
    ) -> Result<BTreeMap<String, RenderReport>> {
        let root = self.view_root(kind, profile)?;
        crate::dirs::create_dir_all(&root)?;
        if kind == ViewKind::Comfyui {
            for category in comfyui::categories() {
                crate::dirs::create_dir_all(&root.join(category))?;
            }
        }

        let mut manifest = self.load_manifest(&root)?;
        let mut reports = BTreeMap::new();

        for entry in entries {
            validate_model_name(&entry.model)?;
            let Ok(installed) = self.models.current(&entry.model) else {
                continue;
            };

            // Drop this model's previously-owned paths first.
            self.remove_owned(&root, &mut manifest, &entry.model)?;

            let plan = match kind {
                ViewKind::Comfyui => comfyui::plan(&installed, &entry.map)?,
                ViewKind::HfCache => hf_cache::plan(&installed)?,
            };

            let mut report = RenderReport {
                unclassified: plan.unclassified,
                ..Default::default()
            };
            for file in plan.files {
                let destination = join_consumer(&root, &file.relative)?;
                if let Some(owner) = manifest.owners.get(&file.relative) {
                    if owner != &entry.model {
                        return Err(Error::other(format!(
                            "view path `{}` is already provided by model `{owner}`; \
                             rename a model or map it to a different category",
                            file.relative
                        )));
                    }
                }
                match file.source {
                    PlaceSource::Link(source) => {
                        let used = place_file(&source, &destination, self.models.link_mode)?;
                        if used == LinkMode::Copy {
                            report.copies += 1;
                        }
                    }
                    PlaceSource::Write(bytes) => {
                        if let Some(parent) = destination.parent() {
                            crate::dirs::create_dir_all(parent)?;
                        }
                        std::fs::write(&destination, &bytes)
                            .map_err(|e| Error::io(&destination, e))?;
                    }
                }
                manifest
                    .owners
                    .insert(file.relative.clone(), entry.model.clone());
                report.placed.push(file.relative);
            }

            report.placed.sort();
            report.unclassified.sort();
            reports.insert(entry.model.clone(), report);
        }

        self.save_manifest(&root, &manifest)?;
        Ok(reports)
    }

    /// Remove one model's entries, or the whole profile.
    pub fn remove(&self, kind: ViewKind, profile: &str, model: Option<&str>) -> Result<bool> {
        let root = self.view_root(kind, profile)?;
        if !root.exists() {
            return Ok(false);
        }
        match model {
            Some(model) => {
                validate_model_name(model)?;
                let mut manifest = self.load_manifest(&root)?;
                let had = manifest.owners.values().any(|m| m == model);
                self.remove_owned(&root, &mut manifest, model)?;
                self.save_manifest(&root, &manifest)?;
                Ok(had)
            }
            None => {
                crate::store::dirlink::remove_tree_links_first(&root)?;
                std::fs::remove_dir_all(&root).map_err(|e| Error::io(&root, e))?;
                Ok(true)
            }
        }
    }

    pub fn link_mode(&self) -> LinkMode {
        self.models.link_mode
    }

    /// Whether a model has a current snapshot pulled locally.
    pub fn model_exists(&self, name: &str) -> Result<bool> {
        match self.models.current(name) {
            Ok(_) => Ok(true),
            // current() wraps every missing-file path in the structured Io
            // variant (current.json missing => NotFound).
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(Error::PlainIo(io)) if io.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(other) => Err(other),
        }
    }

    fn load_manifest(&self, root: &Path) -> Result<ViewManifest> {
        let path = root.join(VIEW_MANIFEST_FILE);
        if !path.is_file() {
            return Ok(ViewManifest::default());
        }
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::other(format!("invalid {VIEW_MANIFEST_FILE}: {e}")))
    }

    fn save_manifest(&self, root: &Path, manifest: &ViewManifest) -> Result<()> {
        let path = root.join(VIEW_MANIFEST_FILE);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))
    }

    /// Delete every consumer-relative path owned by `model`, dropping entries.
    fn remove_owned(&self, root: &Path, manifest: &mut ViewManifest, model: &str) -> Result<()> {
        let owned: Vec<String> = manifest
            .owners
            .iter()
            .filter_map(|(path, owner)| (owner == model).then_some(path.clone()))
            .collect();
        for relative in owned {
            let path = join_consumer(root, &relative)?;
            match path.symlink_metadata() {
                Ok(meta) if crate::store::dirlink::is_link(&meta) && meta.is_dir() => {
                    crate::store::dirlink::remove(&path)?;
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => {}
            }
            manifest.owners.remove(&relative);
        }
        Ok(())
    }
}

fn join_consumer(root: &Path, relative: &str) -> Result<PathBuf> {
    Ok(root.join(safe_relative_path(relative)?))
}

fn validate_profile(profile: &str) -> Result<()> {
    if profile.is_empty()
        || profile == "."
        || profile == ".."
        || !profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(Error::config(format!(
            "invalid model view profile `{profile}` (letters, digits, dot, dash, underscore)"
        )));
    }
    Ok(())
}

/// Place one file with the chosen link mode and mark it read-only.
pub(crate) fn place_file(source: &Path, destination: &Path, mode: LinkMode) -> Result<LinkMode> {
    if let Some(parent) = destination.parent() {
        crate::dirs::create_dir_all(parent)?;
    }
    let used = crate::store::link::materialize(source, destination, mode)?;
    set_readonly(destination)?;
    Ok(used)
}

#[cfg(unix)]
fn set_readonly(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() & 0o555);
    std::fs::set_permissions(path, perms).map_err(|e| Error::io(path, e))
}

#[cfg(windows)]
fn set_readonly(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mut perms = meta.permissions();
    if !perms.readonly() {
        perms.set_readonly(true);
        std::fs::set_permissions(path, perms).map_err(|e| Error::io(path, e))?;
    }
    Ok(())
}

/// Resolve a snapshot file's absolute path within the materialized snapshot.
pub(crate) fn snapshot_file_path(installed: &InstalledModel, relative: &str) -> Result<PathBuf> {
    Ok(installed.path.join(safe_relative_path(relative)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;
    use crate::model::{DownloadedModelFile, ModelStore, ProviderId, SnapshotIdentity};
    use crate::store::Cas;
    use std::sync::Arc;

    fn stores(root: &Path) -> (ModelStore, ViewStore) {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let cas = Arc::new(Cas::new(dirs.store.clone()));
        let models = ModelStore::new(dirs.clone(), cas.clone(), LinkMode::Copy);
        let views = ViewStore::new(dirs.clone(), cas, LinkMode::Copy);
        (models, views)
    }

    /// `src_rel` is where the downloaded bytes live in the scratch area;
    /// `repo_rel` is the path inside the repository (what the classifier sees).
    fn file_in(root: &Path, src_rel: &str, repo_rel: &str, bytes: &[u8]) -> DownloadedModelFile {
        let source = root.join(src_rel);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, bytes).unwrap();
        DownloadedModelFile {
            path: repo_rel.into(),
            source,
            size: bytes.len() as u64,
            sha256: None,
            etag: None,
        }
    }

    fn publish(models: &ModelStore, _root: &Path, name: &str, files: Vec<DownloadedModelFile>) {
        models
            .publish(
                SnapshotIdentity {
                    name: name.into(),
                    provider: ProviderId::HuggingFace,
                    repository: format!("owner/{name}"),
                    requested_revision: "main".into(),
                    revision: "abc123".into(),
                    endpoint: "https://huggingface.co".into(),
                    variant: None,
                },
                files,
            )
            .unwrap();
    }

    #[test]
    fn comfyui_shared_categories_survive_rebuild_and_model_removal() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        publish(
            &models,
            &scratch,
            "a",
            vec![file_in(
                &scratch,
                "src-a/vae-a.safetensors",
                "vae/vae-a.safetensors",
                b"A",
            )],
        );
        publish(
            &models,
            &scratch,
            "b",
            vec![file_in(
                &scratch,
                "src-b/vae-b.safetensors",
                "vae/vae-b.safetensors",
                b"B",
            )],
        );
        let map = BTreeMap::new();
        let entries = vec![
            ViewEntry {
                model: "a".into(),
                map: map.clone(),
            },
            ViewEntry {
                model: "b".into(),
                map,
            },
        ];
        let root = views.view_root(ViewKind::Comfyui, "default").unwrap();
        views
            .render(ViewKind::Comfyui, "default", &entries)
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("vae/vae-a.safetensors")).unwrap(),
            b"A"
        );
        assert_eq!(
            std::fs::read(root.join("vae/vae-b.safetensors")).unwrap(),
            b"B"
        );

        // Rebuild model a only; b's link must remain (the shared-dir bug).
        views
            .render(ViewKind::Comfyui, "default", &[entries[0].clone()])
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("vae/vae-b.safetensors")).unwrap(),
            b"B"
        );

        views
            .remove(ViewKind::Comfyui, "default", Some("a"))
            .unwrap();
        assert!(!root.join("vae/vae-a.safetensors").exists());
        assert_eq!(
            std::fs::read(root.join("vae/vae-b.safetensors")).unwrap(),
            b"B"
        );
        assert_eq!(models.verify("a").unwrap().revision, "abc123");
    }

    #[test]
    fn same_filename_across_models_is_a_reported_collision_not_an_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        publish(
            &models,
            &scratch,
            "a",
            vec![file_in(
                &scratch,
                "src-a/vae.safetensors",
                "vae/vae.safetensors",
                b"A",
            )],
        );
        publish(
            &models,
            &scratch,
            "b",
            vec![file_in(
                &scratch,
                "src-b/vae.safetensors",
                "vae/vae.safetensors",
                b"B",
            )],
        );
        let map = BTreeMap::new();
        let err = views
            .render(
                ViewKind::Comfyui,
                "default",
                &[
                    ViewEntry {
                        model: "a".into(),
                        map: map.clone(),
                    },
                    ViewEntry {
                        model: "b".into(),
                        map,
                    },
                ],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("already provided by model"),
            "{err}"
        );
    }

    /// Two models that each contribute a *different* filename coexist; when
    /// they contribute the *same* consumer-relative path the second render is
    /// rejected and, critically, the first model's link is left byte-intact.
    /// This is the "two models, one shared view root" case the P1 aggregation
    /// rewrite exists to protect. Asserted by reading the artifact (the file
    /// bytes still belong to model a), not by the error alone.
    #[test]
    fn collision_leaves_the_first_models_link_intact_and_is_atomic_per_model() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        publish(
            &models,
            &scratch,
            "a",
            vec![file_in(
                &scratch,
                "src-a/vae.safetensors",
                "vae/vae.safetensors",
                b"AAA",
            )],
        );
        publish(
            &models,
            &scratch,
            "b",
            vec![file_in(
                &scratch,
                "src-b/vae.safetensors",
                "vae/vae.safetensors",
                b"BBB",
            )],
        );
        let map = BTreeMap::new();
        let entry_a = ViewEntry {
            model: "a".into(),
            map: map.clone(),
        };
        let entry_b = ViewEntry {
            model: "b".into(),
            map,
        };
        let root = views.view_root(ViewKind::Comfyui, "default").unwrap();
        let collision = root.join("vae/vae.safetensors");

        // a alone renders fine.
        views
            .render(ViewKind::Comfyui, "default", std::slice::from_ref(&entry_a))
            .unwrap();
        assert_eq!(std::fs::read(&collision).unwrap(), b"AAA");

        // Trying to render both must fail rather than overwrite a's bytes.
        let err = views
            .render(
                ViewKind::Comfyui,
                "default",
                &[entry_a.clone(), entry_b.clone()],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("already provided by model"),
            "{err}"
        );
        // Read the artifact: a's link must still point at a's bytes.
        assert_eq!(
            std::fs::read(&collision).unwrap(),
            b"AAA",
            "a rejected collision must not overwrite the incumbent link"
        );

        // Rebuilding b alone (which would place the colliding path into an
        // empty-of-a profile is fine, but in THIS root a still owns it) must
        // also be rejected: ownership is read from the on-disk manifest, not
        // just the entry list passed to one render call.
        let err = views
            .render(ViewKind::Comfyui, "default", std::slice::from_ref(&entry_b))
            .unwrap_err();
        assert!(
            err.to_string().contains("already provided by model"),
            "{err}"
        );
        assert_eq!(std::fs::read(&collision).unwrap(), b"AAA");
    }

    #[test]
    fn hf_cache_layout_sits_directly_under_the_view_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        let mut f = file_in(&scratch, "q/model.bin", "model.bin", b"hub");
        f.etag = Some("\"e1\"".into());
        publish(&models, &scratch, "q", vec![f]);
        views
            .render(
                ViewKind::HfCache,
                "default",
                &[ViewEntry {
                    model: "q".into(),
                    map: BTreeMap::new(),
                }],
            )
            .unwrap();
        let root = views.view_root(ViewKind::HfCache, "default").unwrap();
        let repo = root.join("models--owner--q");
        assert_eq!(std::fs::read(repo.join("refs/main")).unwrap(), b"abc123");
        assert_eq!(
            std::fs::read(repo.join("snapshots/abc123/model.bin")).unwrap(),
            b"hub"
        );
        assert!(repo.join("blobs/e1").exists());
    }
}
