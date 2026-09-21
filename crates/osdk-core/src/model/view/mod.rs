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
use crate::model::{
    safe_relative_path, validate_model_name, InstalledModel, ModelFile, ModelStore,
};
use crate::store::link::{self, LinkMode};

pub mod comfyui;
pub mod hf_cache;

pub use hf_cache::repo_dir_name;

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

/// How a single repository-relative file maps into a consumer view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Category/folder within the consumer layout (e.g. `diffusion_models`).
    pub category: String,
    /// File name inside that category.
    pub file_name: String,
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

/// Result of rendering (or re-rendering) one model into a view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderReport {
    /// Files placed into the view, as consumer-relative `/` paths.
    pub placed: Vec<String>,
    /// Files the renderer could not assign to a category. fail-closed: these are
    /// not placed anywhere and surfaced here for `view doctor`.
    pub unclassified: Vec<String>,
    /// Link modes that were actually used per placed file (for explicit
    /// cross-volume reporting), deduplicated.
    pub modes_used: Vec<LinkMode>,
    /// Number of files that fell back to a byte copy because a link was not
    /// possible (different volume). Reported, never silent.
    pub copies: usize,
}

impl RenderReport {
    fn note_mode(&mut self, mode: LinkMode) {
        if !self.modes_used.contains(&mode) {
            self.modes_used.push(mode);
        }
        if mode == LinkMode::Copy {
            self.copies += 1;
        }
    }
}

/// Manages rendered views under `<data>/views`.
pub struct ViewStore {
    dirs: Dirs,
    models: ModelStore,
}

impl ViewStore {
    pub fn new(dirs: Dirs, cas: std::sync::Arc<crate::store::Cas>, link_mode: LinkMode) -> Self {
        let models = ModelStore::new(dirs.clone(), cas, link_mode);
        Self { dirs, models }
    }

    /// `<data>/views/<kind>/<profile>` -- the stable root a consumer's config
    /// points at. The default profile is `default`.
    pub fn view_root(&self, kind: ViewKind, profile: &str) -> Result<PathBuf> {
        validate_profile(profile)?;
        Ok(self.dirs.model_views().join(kind.as_str()).join(profile))
    }

    /// Render every current model that belongs to this view into it.
    ///
    /// `entries` lists model membership (from config or the lock). Models not
    /// currently pulled are skipped (the view is rebuilt again after sync).
    pub fn render(
        &self,
        kind: ViewKind,
        profile: &str,
        entries: &[ViewEntry],
    ) -> Result<BTreeMap<String, RenderReport>> {
        let root = self.view_root(kind, profile)?;
        crate::dirs::create_dir_all(&root)?;
        let mut reports = BTreeMap::new();
        for entry in entries {
            validate_model_name(&entry.model)?;
            let Ok(installed) = self.models.current(&entry.model) else {
                // Not pulled yet; nothing to render. Not an error.
                continue;
            };
            let model_dir = root.join(&entry.model);
            // Rebuild this model's subtree from scratch so removed files do not
            // linger as stale links.
            if model_dir.exists() {
                crate::store::dirlink::remove_tree_links_first(&model_dir)?;
                std::fs::remove_dir_all(&model_dir).map_err(|e| Error::io(&model_dir, e))?;
            }
            crate::dirs::create_dir_all(&model_dir)?;
            let report = match kind {
                ViewKind::Comfyui => {
                    comfyui::render(&installed, &entry.map, &model_dir, self.models.link_mode)?
                }
                ViewKind::HfCache => {
                    hf_cache::render(&installed, &model_dir, self.models.link_mode)?
                }
            };
            reports.insert(entry.model.clone(), report);
        }
        Ok(reports)
    }

    /// Remove a whole profile's view tree, or one model's subtree within it.
    pub fn remove(&self, kind: ViewKind, profile: &str, model: Option<&str>) -> Result<bool> {
        let root = self.view_root(kind, profile)?;
        let target = match model {
            Some(name) => {
                validate_model_name(name)?;
                root.join(name)
            }
            None => root,
        };
        if !target.exists() {
            return Ok(false);
        }
        crate::store::dirlink::remove_tree_links_first(&target)?;
        std::fs::remove_dir_all(&target).map_err(|e| Error::io(&target, e))?;
        Ok(true)
    }

    pub fn link_mode(&self) -> LinkMode {
        self.models.link_mode
    }
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

/// Place one snapshot file into the view using the chosen link mode, returning
/// the mode actually used. Shared by every renderer so cross-volume fallback
/// and read-only marking behave identically.
///
/// Read-only is set on the placed entry so a consumer that writes in place
/// fails loudly instead of mutating the CAS bytes through a hardlink. A copy is
/// also marked read-only; the user can still delete a view (directory removal
/// does not require unsetting the file's read-only bit on either platform), but
/// overwriting its contents is denied.
pub(crate) fn place_file(source: &Path, destination: &Path, mode: LinkMode) -> Result<LinkMode> {
    if let Some(parent) = destination.parent() {
        crate::dirs::create_dir_all(parent)?;
    }
    let used = link::materialize(source, destination, mode)?;
    set_readonly(destination)?;
    Ok(used)
}

#[cfg(unix)]
fn set_readonly(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mut perms = meta.permissions();
    // Drop write for owner/group/other, keep read (and execute if it had it).
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

/// Apply the read-only bit to a view working-tree entry that was linked
/// separately from the blob-placement path (the HF cache links
/// `snapshots/<rev>/<file>` to a blob, rather than directly to the snapshot).
pub(crate) fn set_working_readonly(path: &Path) -> Result<()> {
    set_readonly(path)
}

/// Resolve a snapshot file's absolute path within the materialized snapshot.
pub(crate) fn snapshot_file_path(installed: &InstalledModel, file: &ModelFile) -> Result<PathBuf> {
    let relative = safe_relative_path(&file.path)?;
    Ok(installed.path.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;
    use crate::model::{DownloadedModelFile, ModelStore, ProviderId, SnapshotIdentity};
    use crate::store::Cas;
    use std::collections::BTreeMap;
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
        // Copy mode makes the view test deterministic on every platform and
        // still exercises the read-only marking that matters for safety.
        let models = ModelStore::new(dirs.clone(), cas, LinkMode::Copy);
        let views = ViewStore::new(
            dirs.clone(),
            Arc::new(Cas::new(dirs.store.clone())),
            LinkMode::Copy,
        );
        (models, views)
    }

    fn write_source(root: &Path, rel: &str, bytes: &[u8]) -> (std::path::PathBuf, u64) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        (path, bytes.len() as u64)
    }

    /// Publish a multi-component diffusion snapshot plus a loose root file.
    fn publish_mix(models: &ModelStore, root: &Path) {
        let mut files = Vec::new();
        for (rel, bytes) in [
            ("unet/diffusion.safetensors", &b"unet-bytes"[..]),
            ("vae/vae.safetensors", &b"vae-bytes"[..]),
            ("text_encoder/te.safetensors", &b"te-bytes"[..]),
            ("loras/style.safetensors", &b"lora-bytes"[..]),
            ("loose.safetensors", &b"loose-bytes"[..]),
            ("config.json", &b"{}"[..]),
        ] {
            let (source, size) = write_source(root, rel, bytes);
            files.push(DownloadedModelFile {
                path: rel.into(),
                source,
                size,
                sha256: None,
                etag: None,
            });
        }
        models
            .publish(
                SnapshotIdentity {
                    name: "mix".into(),
                    provider: ProviderId::HuggingFace,
                    repository: "owner/repo".into(),
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
    fn comfyui_view_places_components_categories_and_is_readonly() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        publish_mix(&models, &scratch);

        // Explicit mapping rescues the loose weight; config.json has no weight
        // category and stays unclassified alongside the loose json.
        let mut map = BTreeMap::new();
        map.insert("loose.safetensors".to_string(), "checkpoints".to_string());
        let entries = vec![ViewEntry {
            model: "mix".into(),
            map,
        }];
        let reports = views
            .render(ViewKind::Comfyui, "default", &entries)
            .unwrap();
        let report = &reports["mix"];

        let root = views.view_root(ViewKind::Comfyui, "default").unwrap();
        let m = root.join("mix");

        // Correct categories, read bytes THROUGH the view.
        assert_eq!(
            std::fs::read(m.join("diffusion_models/diffusion.safetensors")).unwrap(),
            b"unet-bytes"
        );
        assert_eq!(
            std::fs::read(m.join("vae/vae.safetensors")).unwrap(),
            b"vae-bytes"
        );
        assert_eq!(
            std::fs::read(m.join("text_encoders/te.safetensors")).unwrap(),
            b"te-bytes"
        );
        assert_eq!(
            std::fs::read(m.join("checkpoints/loose.safetensors")).unwrap(),
            b"loose-bytes"
        );

        // fail-closed: the loose json is unclassified, not dumped anywhere.
        assert_eq!(report.unclassified, vec!["config.json"]);
        assert!(!m.join("checkpoints/config.json").exists());

        // all 25 category dirs exist
        let mut dir_count = 0usize;
        for entry in std::fs::read_dir(&m).unwrap() {
            if entry.unwrap().file_type().unwrap().is_dir() {
                dir_count += 1;
            }
        }
        assert_eq!(dir_count, 25, "renderer must pre-create every category");

        // read-only: opening the placed file for write must fail.
        let placed = m.join("vae/vae.safetensors");
        assert!(std::fs::OpenOptions::new()
            .write(true)
            .open(&placed)
            .is_err());

        // copy mode reported honestly: four components + the rescued loose
        // weight = 5 placed files (the json stays unclassified). Removal must
        // not harm the snapshot.
        assert_eq!(report.copies, 5);
        views.remove(ViewKind::Comfyui, "default", None).unwrap();
        assert!(!root.exists());
        assert_eq!(models.verify("mix").unwrap().revision, "abc123");
    }

    #[test]
    fn hf_cache_view_has_hub_layout_and_refs() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (models, views) = stores(temp.path());
        let (source, size) = write_source(&scratch, "model.bin", b"hub");
        models
            .publish(
                SnapshotIdentity {
                    name: "q".into(),
                    provider: ProviderId::HuggingFace,
                    repository: "owner/repo".into(),
                    requested_revision: "main".into(),
                    revision: "deadbeef".into(),
                    endpoint: "https://huggingface.co".into(),
                    variant: None,
                },
                vec![DownloadedModelFile {
                    path: "model.bin".into(),
                    source,
                    size,
                    sha256: None,
                    etag: Some("\"etag-1\"".into()),
                }],
            )
            .unwrap();

        let reports = views
            .render(
                ViewKind::HfCache,
                "default",
                &[ViewEntry {
                    model: "q".into(),
                    map: BTreeMap::new(),
                }],
            )
            .unwrap();
        assert!(reports["q"].unclassified.is_empty());

        let root = views.view_root(ViewKind::HfCache, "default").unwrap();
        let repo = root.join("q/models--owner--repo");
        assert_eq!(std::fs::read(repo.join("refs/main")).unwrap(), b"deadbeef");
        // working tree resolves through the blob
        assert_eq!(
            std::fs::read(repo.join("snapshots/deadbeef/model.bin")).unwrap(),
            b"hub"
        );
        assert!(repo.join("blobs/etag-1").exists());
    }

    #[test]
    fn an_unpulled_model_in_entries_is_skipped_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let (_models, views) = stores(temp.path());
        let reports = views
            .render(
                ViewKind::Comfyui,
                "default",
                &[ViewEntry {
                    model: "ghost".into(),
                    map: BTreeMap::new(),
                }],
            )
            .unwrap();
        assert!(reports.is_empty());
    }
}
