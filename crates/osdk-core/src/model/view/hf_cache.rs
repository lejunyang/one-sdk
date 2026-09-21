//! The Hugging Face hub cache view renderer.
//!
//! Produces the layout the `huggingface_hub` / `transformers` client reads:
//!
//! ```text
//! <view>/<model>/
//!   models--<org>--<repo>/
//!     refs/main                 # the revision (full sha)
//!     snapshots/<revision>/<file>   # working-tree file names, linked to blobs
//!     blobs/<sha256-or-etag>        # content-addressed bytes
//! ```
//!
//! A model downloaded by osdk is identified by provider+repository+revision and
//! already carries per-file hashes, so every field of this shape is available
//! without re-deriving anything from the network.

use std::path::Path;

use crate::error::Result;
use crate::model::view::{place_file, snapshot_file_path, RenderReport};
use crate::model::InstalledModel;
use crate::store::link::LinkMode;

pub(crate) fn render(
    installed: &InstalledModel,
    model_dir: &Path,
    mode: LinkMode,
) -> Result<RenderReport> {
    let mut report = RenderReport::default();

    let repo_dir_name = repo_dir_name(&installed.manifest.repository);
    let repo_dir = model_dir.join(&repo_dir_name);
    let refs_dir = repo_dir.join("refs");
    let blobs_dir = repo_dir.join("blobs");
    let snapshot_dir = repo_dir
        .join("snapshots")
        .join(&installed.manifest.revision);
    crate::dirs::create_dir_all(&refs_dir)?;
    crate::dirs::create_dir_all(&blobs_dir)?;
    crate::dirs::create_dir_all(&snapshot_dir)?;

    // refs/main -> full pinned revision. Use the resolved revision, not the
    // requested one (`main`), so a client reading refs/main sees the same
    // immutable sha osdk locked.
    let ref_path = refs_dir.join("main");
    std::fs::write(&ref_path, installed.manifest.revision.as_bytes())
        .map_err(|e| crate::error::Error::io(&ref_path, e))?;

    for file in &installed.manifest.files {
        // blob name: prefer the verified sha256, fall back to etag; if neither
        // exists we cannot synthesize a blob name honestly, so skip the file and
        // report it (pull with require-checksums avoids this).
        let Some(blob) = file
            .sha256
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(file.etag.as_deref().filter(|s| !s.trim().is_empty()))
        else {
            report.unclassified.push(file.path.clone());
            continue;
        };
        let blob = blob.replace('"', "");

        let source = snapshot_file_path(installed, file)?;
        let blob_path = blobs_dir.join(&blob);
        let used = place_file(&source, &blob_path, mode)?;
        report.note_mode(used);

        // working-tree name inside snapshots/<rev>/, preserving repo subdirs.
        let normalized = file.path.replace('\\', "/");
        let working = snapshot_dir.join(crate::model::safe_relative_path(&normalized)?);
        if let Some(parent) = working.parent() {
            crate::dirs::create_dir_all(parent)?;
        }
        // The working-tree entry itself is a link to the blob, not a second
        // hardlink to the snapshot. Place it relative to the blob when possible
        // (hub caches use relative symlinks); on Windows a junction/hardlink is
        // used by link::materialize. Reuse the same primitive for consistency.
        let _ = link::materialize(&blob_path, &working, mode)?;
        crate::model::view::set_working_readonly(&working)?;
        report.placed.push(normalized);
    }

    report.placed.sort();
    report.unclassified.sort();
    Ok(report)
}

/// `owner/repo` -> `models--owner--repo`.
pub fn repo_dir_name(repository: &str) -> String {
    format!("models--{}", repository.replace('/', "--"))
}

use crate::store::link;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_becomes_the_hub_directory_name() {
        assert_eq!(repo_dir_name("Qwen/Qwen2.5-7B"), "models--Qwen--Qwen2.5-7B");
        assert_eq!(repo_dir_name("owner/repo"), "models--owner--repo");
    }
}
