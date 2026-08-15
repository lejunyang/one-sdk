//! Install pipeline orchestrator: download → verify → extract → CAS ingest →
//! materialize. Shared by all archive-based backends (node/go/python/java,
//! standalone pnpm/yarn).

use std::path::PathBuf;

use crate::dirs::{create_dir_all, Dirs};
use crate::error::{Error, Result};
use crate::lock::FileLock;
use crate::store::link::LinkMode;
use crate::store::Cas;

pub mod download;
pub mod extract;
pub mod verify;

pub use extract::ArchiveKind;
pub use verify::HashAlgo;

/// Everything a backend must supply to install one version via the pipeline.
pub struct InstallPlan {
    pub tool: String,
    pub version: String,
    /// Direct URL of the archive to download.
    pub url: String,
    /// Filename to save the archive as (used for kind detection + naming).
    pub file_name: String,
    pub kind: ArchiveKind,
    /// Optional expected checksum for the archive.
    pub checksum: Option<Checksum>,
    /// Whether to strip a single top-level directory during extraction.
    pub strip_root: bool,
}

pub struct Checksum {
    pub algo: HashAlgo,
    pub hex: String,
}

/// Context passed into the pipeline run.
pub struct PipelineCtx<'a> {
    pub client: &'a reqwest::Client,
    pub dirs: &'a Dirs,
    pub cas: &'a Cas,
    pub link_mode: LinkMode,
    pub show_progress: bool,
}

/// Marker file written at the install root once an install is complete.
const COMPLETE_MARKER: &str = ".osdk-complete";

/// Run the full pipeline for one plan. Returns the install directory.
pub async fn run(plan: &InstallPlan, ctx: &PipelineCtx<'_>) -> Result<PathBuf> {
    let install_dir = ctx.dirs.install_path(&plan.tool, &plan.version);

    // Serialize concurrent installs of the same tool@version.
    let lock_path = ctx
        .dirs
        .lock_dir(&plan.tool)
        .join(format!("{}.lock", plan.version));
    let _lock = FileLock::acquire(&lock_path)?;

    // Idempotency: already installed and marked complete.
    if install_dir.join(COMPLETE_MARKER).exists() {
        return Ok(install_dir);
    }
    // Stale/partial dir from a previous failed run: clean it.
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }

    // 1. Download to the shared downloads cache.
    let archive_path = ctx.dirs.downloads().join(&plan.file_name);
    let label = format!("{}@{}", plan.tool, plan.version);
    download::download(ctx.client, &plan.url, &archive_path, &label, ctx.show_progress).await?;

    // 2. Verify checksum if provided.
    if let Some(cs) = &plan.checksum {
        verify::verify_file(&archive_path, &cs.hex, cs.algo, &plan.file_name)?;
    }

    // 3. Extract into a scratch dir under the cache tmp.
    let scratch = ctx
        .dirs
        .tmp()
        .join(format!("{}-{}-{}", plan.tool, plan.version, std::process::id()));
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    create_dir_all(&scratch)?;
    extract::extract(&archive_path, &scratch, plan.kind, plan.strip_root)?;

    // 4. Ingest into CAS + materialize into the install dir.
    let report = ctx
        .cas
        .ingest_tree(&scratch, &install_dir, &plan.tool, &plan.version, ctx.link_mode)?;
    let _ = std::fs::remove_dir_all(&scratch);

    // 5. Finalize.
    std::fs::write(install_dir.join(COMPLETE_MARKER), b"")
        .map_err(|e| Error::io(install_dir.join(COMPLETE_MARKER), e))?;

    tracing::debug!(
        tool = %plan.tool,
        version = %plan.version,
        files = report.files_written,
        new_objects = report.objects_new,
        "install materialized"
    );

    Ok(install_dir)
}

/// Whether a tool@version is installed (complete marker present).
pub fn is_installed(dirs: &Dirs, tool: &str, version: &str) -> bool {
    dirs.install_path(tool, version)
        .join(COMPLETE_MARKER)
        .exists()
}
