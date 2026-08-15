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
    /// Candidate archive URLs, best-first. The pipeline tries each until one
    /// downloads successfully (source failover).
    pub urls: Vec<String>,
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

    // 1. Download to the shared downloads cache, trying candidate URLs in order.
    let archive_path = ctx.dirs.downloads().join(&plan.file_name);
    let label = format!("{}@{}", plan.tool, plan.version);
    if !archive_path.exists() {
        let mut last_err: Option<Error> = None;
        let mut downloaded = false;
        for (i, url) in plan.urls.iter().enumerate() {
            match download::download(ctx.client, url, &archive_path, &label, ctx.show_progress)
                .await
            {
                Ok(()) => {
                    downloaded = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        url = %url,
                        attempt = i + 1,
                        total = plan.urls.len(),
                        "{}",
                        crate::i18n::trf("log.download_failover", &[("err", &e.to_string())])
                    );
                    last_err = Some(e);
                }
            }
        }
        if !downloaded {
            return Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
                tool: plan.tool.clone(),
                tried: plan.urls.len(),
            }));
        }
    }

    // 2. Verify checksum if provided.
    if let Some(cs) = &plan.checksum {
        verify::verify_file(&archive_path, &cs.hex, cs.algo, &plan.file_name)?;
        tracing::info!(file = %plan.file_name, "{}", crate::i18n::tr("log.checksum_verified"));
    } else {
        tracing::debug!(file = %plan.file_name, "no checksum available; skipping verification");
    }

    // 3. Extract into a scratch dir under the cache tmp.
    let scratch = ctx.dirs.tmp().join(format!(
        "{}-{}-{}",
        plan.tool,
        plan.version,
        std::process::id()
    ));
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    create_dir_all(&scratch)?;
    extract::extract(&archive_path, &scratch, plan.kind, plan.strip_root)?;

    // 4. Ingest into CAS + materialize into the install dir.
    let report = ctx.cas.ingest_tree(
        &scratch,
        &install_dir,
        &plan.tool,
        &plan.version,
        ctx.link_mode,
    )?;
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

/// Download a single executable into `<install>/bin/<exe_name>` and mark the
/// install complete. Tries each URL in order (failover), optionally verifying a
/// checksum. Used for standalone binaries (e.g. github: bare binaries).
#[allow(clippy::too_many_arguments)]
pub async fn install_single_binary(
    client: &reqwest::Client,
    dirs: &Dirs,
    tool: &str,
    version: &str,
    urls: &[String],
    exe_name: &str,
    download_name: &str,
    os: crate::platform::Os,
    checksum: Option<&Checksum>,
    show_progress: bool,
) -> Result<()> {
    let install_dir = dirs.install_path(tool, version);
    if install_dir.join(COMPLETE_MARKER).exists() {
        return Ok(());
    }
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }
    let bin_dir = install_dir.join("bin");
    create_dir_all(&bin_dir)?;

    let cached = dirs.downloads().join(download_name);
    let _ = std::fs::remove_file(&cached);
    let mut last_err: Option<Error> = None;
    let mut ok = false;
    for (i, url) in urls.iter().enumerate() {
        match download::download(
            client,
            url,
            &cached,
            &format!("{tool}@{version}"),
            show_progress,
        )
        .await
        {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => {
                tracing::warn!(
                    url = %url,
                    attempt = i + 1,
                    total = urls.len(),
                    "{}",
                    crate::i18n::trf("log.binary_download_failed", &[("err", &e.to_string())])
                );
                last_err = Some(e);
            }
        }
    }
    if !ok {
        return Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: tool.to_string(),
            tried: urls.len(),
        }));
    }

    if let Some(cs) = checksum {
        verify::verify_file(&cached, &cs.hex, cs.algo, download_name)?;
        tracing::info!(file = %download_name, "{}", crate::i18n::tr("log.checksum_verified"));
    }

    let exe_suffix = os.exe_suffix();
    let dest = bin_dir.join(format!("{exe_name}{exe_suffix}"));
    std::fs::copy(&cached, &dest).map_err(|e| Error::io(&dest, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }

    std::fs::write(install_dir.join(COMPLETE_MARKER), b"")
        .map_err(|e| Error::io(install_dir.join(COMPLETE_MARKER), e))?;
    Ok(())
}
