//! The Hugging Face hub cache view planner.
//!
//! Produces the layout `huggingface_hub` reads:
//!
//! ```text
//! models--<org>--<repo>/
//!   refs/main                 # pinned revision (full sha)
//!   blobs/<hash>              # content-addressed bytes
//!   snapshots/<revision>/<file>   # working-tree names, linked to the bytes
//! ```
//!
//! The repo dir sits directly under the view root (what `HF_HOME` points at),
//! NOT under the logical osdk model name. The blob and the working-tree entry
//! are both materialized with the shared link primitive, so they resolve to
//! the same CAS bytes without copying.

use crate::error::Result;
use crate::model::view::{snapshot_file_path, Plan};
use crate::model::InstalledModel;

/// `owner/repo` -> `models--owner--repo`.
pub fn repo_dir_name(repository: &str) -> String {
    format!("models--{}", repository.replace('/', "--"))
}

pub fn plan(installed: &InstalledModel) -> Result<Plan> {
    let mut plan = Plan::default();
    let repo = repo_dir_name(&installed.manifest.repository);
    let revision = &installed.manifest.revision;

    plan.write(format!("{repo}/refs/main"), revision.as_bytes().to_vec());

    for file in &installed.manifest.files {
        let normalized = file.path.replace('\\', "/");
        let source = snapshot_file_path(installed, &file.path)?;
        match file
            .sha256
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(file.etag.as_deref().filter(|s| !s.trim().is_empty()))
        {
            Some(blob) => {
                let blob = blob.replace('"', "");
                plan.link(format!("{repo}/blobs/{blob}"), source.clone());
                plan.link(format!("{repo}/snapshots/{revision}/{normalized}"), source);
            }
            // No content hash/etag: cannot honestly name a blob, so skip and
            // report (pull with checksums avoids this).
            None => plan.unclassified.push(file.path.clone()),
        }
    }

    plan.files.sort_by(|a, b| a.relative.cmp(&b.relative));
    plan.unclassified.sort();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_becomes_the_hub_directory_name() {
        assert_eq!(repo_dir_name("Qwen/Qwen2.5-7B"), "models--Qwen--Qwen2.5-7B");
        assert_eq!(repo_dir_name("owner/repo"), "models--owner--repo");
    }
}
