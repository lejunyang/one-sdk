//! Install pipeline orchestrator: download → verify → extract → CAS ingest →
//! materialize. Shared by all archive-based backends (node/go/python/java,
//! standalone pnpm/yarn).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dirs::{create_dir_all, Dirs};
use crate::error::{Error, Result};
use crate::lock::FileLock;
use crate::store::link::LinkMode;
use crate::store::Cas;
use crate::verification::{GithubAttestation, VerificationEvidence};
use crate::version::ToolVersion;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactReceipt {
    pub url: String,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<VerificationEvidence>,
}

const ARTIFACT_RECEIPT_FILE: &str = ".osdk-artifact.json";
pub const LOCKED_ARTIFACT_URL_OPTION: &str = "__osdk_artifact_url";
pub const LOCKED_ARTIFACT_FILE_OPTION: &str = "__osdk_artifact_file";
pub const LOCKED_ARTIFACT_CHECKSUM_OPTION: &str = "__osdk_artifact_checksum";

/// Context passed into the pipeline run.
pub struct PipelineCtx<'a> {
    pub client: &'a reqwest::Client,
    pub dirs: &'a Dirs,
    pub cas: &'a Cas,
    pub link_mode: LinkMode,
    pub show_progress: bool,
    pub offline: bool,
    pub require_checksums: bool,
}

pub fn locked_install_plan(
    tool: &str,
    version: &ToolVersion,
    strip_root: bool,
) -> Result<Option<InstallPlan>> {
    let Some(artifact) = locked_artifact(version)? else {
        return Ok(None);
    };
    Ok(Some(InstallPlan {
        tool: tool.to_string(),
        version: version.version.clone(),
        urls: vec![artifact.url],
        kind: ArchiveKind::from_name(&artifact.file_name)?,
        file_name: artifact.file_name,
        checksum: artifact
            .checksum
            .as_deref()
            .map(parse_checksum)
            .transpose()?,
        strip_root,
    }))
}

pub fn locked_artifact(version: &ToolVersion) -> Result<Option<ArtifactReceipt>> {
    let Some(url) = version.options.get(LOCKED_ARTIFACT_URL_OPTION) else {
        return Ok(None);
    };
    let file_name = version
        .options
        .get(LOCKED_ARTIFACT_FILE_OPTION)
        .ok_or_else(|| Error::other("locked artifact is missing its file name"))?
        .clone();
    Ok(Some(ArtifactReceipt {
        url: url.clone(),
        file_name,
        checksum: version
            .options
            .get(LOCKED_ARTIFACT_CHECKSUM_OPTION)
            .cloned(),
        evidence: Vec::new(),
    }))
}

/// Marker file written at the install root once an install is complete.
const COMPLETE_MARKER: &str = ".osdk-complete";

/// Run the full pipeline for one plan. Returns the install directory.
pub async fn run(plan: &InstallPlan, ctx: &PipelineCtx<'_>) -> Result<PathBuf> {
    run_with_attestation(plan, ctx, None).await
}

pub async fn run_with_attestation(
    plan: &InstallPlan,
    ctx: &PipelineCtx<'_>,
    attestation: Option<&GithubAttestation>,
) -> Result<PathBuf> {
    let install_dir = ctx.dirs.install_path(&plan.tool, &plan.version);
    let archive_path = artifact_cache_path(ctx.dirs, &plan.tool, &plan.version, &plan.file_name);

    // Serialize concurrent installs of the same tool@version.
    let lock_path = ctx
        .dirs
        .lock_dir(&plan.tool)
        .join(format!("{}.lock", plan.version));
    let _lock = FileLock::acquire(&lock_path)?;

    // Idempotency: already installed and marked complete.
    if install_dir.join(COMPLETE_MARKER).exists() {
        if let Some(attestation) = attestation {
            let evidence = crate::verification::verify_github_attestation(
                ctx.client,
                ctx.dirs,
                ctx.offline,
                &archive_path,
                attestation,
            )
            .await?;
            merge_artifact_evidence(&install_dir, evidence)?;
        }
        return Ok(install_dir);
    }
    // Stale/partial dir from a previous failed run: clean it.
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }

    // 1. Download to the shared downloads cache, trying candidate URLs in order.
    let label = format!("{}@{}", plan.tool, plan.version);
    if ctx.offline && !archive_path.exists() {
        return Err(Error::other(format!(
            "offline artifact cache miss for {}@{}",
            plan.tool, plan.version
        )));
    }
    let mut selected_url =
        read_cached_source_url(&archive_path).or_else(|| plan.urls.first().cloned());
    if !archive_path.exists() {
        let mut last_err: Option<Error> = None;
        let mut downloaded = false;
        for (i, url) in plan.urls.iter().enumerate() {
            match download::download(ctx.client, url, &archive_path, &label, ctx.show_progress)
                .await
            {
                Ok(()) => {
                    downloaded = true;
                    selected_url = Some(url.clone());
                    write_cached_source_url(&archive_path, url);
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

    // 2. Verify checksum if provided now or persisted from an earlier online
    // install. This keeps offline reinstalls verifiable even when checksum
    // discovery itself requires the network.
    let persisted_checksum = if plan.checksum.is_none() {
        read_cached_checksum(&archive_path)
    } else {
        None
    };
    let verified_checksum = plan.checksum.as_ref().or(persisted_checksum.as_ref());
    if let Some(cs) = verified_checksum {
        verify::verify_file(&archive_path, &cs.hex, cs.algo, &plan.file_name)?;
        write_cached_checksum(&archive_path, cs);
        tracing::info!(file = %plan.file_name, "{}", crate::i18n::tr("log.checksum_verified"));
    } else {
        tracing::debug!(file = %plan.file_name, "no checksum available; skipping verification");
    }

    let evidence = if let Some(attestation) = attestation {
        crate::verification::verify_github_attestation(
            ctx.client,
            ctx.dirs,
            ctx.offline,
            &archive_path,
            attestation,
        )
        .await?
        .into_iter()
        .collect()
    } else {
        Vec::new()
    };
    let authenticated_checksum = evidence
        .first()
        .map(|item| parse_checksum(&item.digest))
        .transpose()?;
    if ctx.require_checksums && verified_checksum.is_none() && authenticated_checksum.is_none() {
        return Err(Error::other(format!(
            "checksum required but unavailable for {}@{} ({})",
            plan.tool, plan.version, plan.file_name
        )));
    }
    if verified_checksum.is_none() {
        if let Some(checksum) = authenticated_checksum.as_ref() {
            write_cached_checksum(&archive_path, checksum);
        }
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
    write_artifact_receipt(
        &install_dir,
        &ArtifactReceipt {
            url: selected_url.unwrap_or_default(),
            file_name: plan.file_name.clone(),
            checksum: verified_checksum
                .map(format_checksum)
                .or_else(|| authenticated_checksum.as_ref().map(format_checksum)),
            evidence,
        },
    )?;
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

pub fn artifact_receipt(dirs: &Dirs, tool: &str, version: &str) -> Option<ArtifactReceipt> {
    let path = dirs.install_path(tool, version).join(ARTIFACT_RECEIPT_FILE);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
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
    offline: bool,
    require_checksums: bool,
    attestation: Option<&GithubAttestation>,
) -> Result<()> {
    let install_dir = dirs.install_path(tool, version);
    let cached = artifact_cache_path(dirs, tool, version, download_name);
    if install_dir.join(COMPLETE_MARKER).exists() {
        if let Some(attestation) = attestation {
            let evidence = crate::verification::verify_github_attestation(
                client,
                dirs,
                offline,
                &cached,
                attestation,
            )
            .await?;
            merge_artifact_evidence(&install_dir, evidence)?;
        }
        return Ok(());
    }
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }
    let bin_dir = install_dir.join("bin");
    create_dir_all(&bin_dir)?;

    if offline && !cached.exists() {
        return Err(Error::other(format!(
            "offline artifact cache miss for {tool}@{version}"
        )));
    }
    if !offline {
        let _ = std::fs::remove_file(&cached);
    }
    let mut last_err: Option<Error> = None;
    let mut ok = false;
    let mut selected_url = read_cached_source_url(&cached).or_else(|| urls.first().cloned());
    if cached.exists() {
        ok = true;
    } else {
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
                    selected_url = Some(url.clone());
                    write_cached_source_url(&cached, url);
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
    }
    if !ok {
        return Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: tool.to_string(),
            tried: urls.len(),
        }));
    }

    let persisted_checksum = if checksum.is_none() {
        read_cached_checksum(&cached)
    } else {
        None
    };
    let verified_checksum = checksum.or(persisted_checksum.as_ref());
    if let Some(cs) = verified_checksum {
        verify::verify_file(&cached, &cs.hex, cs.algo, download_name)?;
        write_cached_checksum(&cached, cs);
        tracing::info!(file = %download_name, "{}", crate::i18n::tr("log.checksum_verified"));
    }
    let evidence = if let Some(attestation) = attestation {
        crate::verification::verify_github_attestation(client, dirs, offline, &cached, attestation)
            .await?
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    let authenticated_checksum = evidence
        .first()
        .map(|item| parse_checksum(&item.digest))
        .transpose()?;
    if require_checksums && verified_checksum.is_none() && authenticated_checksum.is_none() {
        return Err(Error::other(format!(
            "checksum required but unavailable for {tool}@{version} ({download_name})"
        )));
    }
    if verified_checksum.is_none() {
        if let Some(checksum) = authenticated_checksum.as_ref() {
            write_cached_checksum(&cached, checksum);
        }
    }

    let exe_suffix = os.exe_suffix();
    let dest = bin_dir.join(format!("{exe_name}{exe_suffix}"));
    std::fs::copy(&cached, &dest).map_err(|e| Error::io(&dest, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }

    write_artifact_receipt(
        &install_dir,
        &ArtifactReceipt {
            url: selected_url.unwrap_or_default(),
            file_name: download_name.to_string(),
            checksum: verified_checksum
                .map(format_checksum)
                .or_else(|| authenticated_checksum.as_ref().map(format_checksum)),
            evidence,
        },
    )?;
    std::fs::write(install_dir.join(COMPLETE_MARKER), b"")
        .map_err(|e| Error::io(install_dir.join(COMPLETE_MARKER), e))?;
    Ok(())
}

fn write_artifact_receipt(install_dir: &std::path::Path, receipt: &ArtifactReceipt) -> Result<()> {
    let path = install_dir.join(ARTIFACT_RECEIPT_FILE);
    let bytes = serde_json::to_vec_pretty(receipt)?;
    std::fs::write(&path, bytes).map_err(|error| Error::io(path, error))
}

fn merge_artifact_evidence(
    install_dir: &std::path::Path,
    evidence: Option<VerificationEvidence>,
) -> Result<()> {
    let Some(evidence) = evidence else {
        return Ok(());
    };
    let path = install_dir.join(ARTIFACT_RECEIPT_FILE);
    let bytes = std::fs::read(&path).map_err(|error| Error::io(&path, error))?;
    let mut receipt: ArtifactReceipt = serde_json::from_slice(&bytes)?;
    if !receipt.evidence.contains(&evidence) {
        receipt.evidence.push(evidence);
        write_artifact_receipt(install_dir, &receipt)?;
    }
    Ok(())
}

pub fn artifact_cache_path(dirs: &Dirs, tool: &str, version: &str, file_name: &str) -> PathBuf {
    dirs.downloads()
        .join(crate::dirs::sanitize_tool_id(tool))
        .join(version)
        .join(file_name)
}

fn format_checksum(checksum: &Checksum) -> String {
    let algorithm = match checksum.algo {
        HashAlgo::Sha256 => "sha256",
        HashAlgo::Sha512 => "sha512",
        HashAlgo::Blake3 => "blake3",
    };
    format!("{algorithm}:{}", checksum.hex)
}

pub fn parse_checksum(value: &str) -> Result<Checksum> {
    let (algorithm, hex) = value
        .split_once(':')
        .ok_or_else(|| Error::other(format!("invalid locked checksum `{value}`")))?;
    let algo = match algorithm {
        "sha256" => HashAlgo::Sha256,
        "sha512" => HashAlgo::Sha512,
        "blake3" => HashAlgo::Blake3,
        _ => {
            return Err(Error::other(format!(
                "unsupported locked checksum `{algorithm}`"
            )))
        }
    };
    Ok(Checksum {
        algo,
        hex: hex.to_string(),
    })
}

fn checksum_cache_path(archive: &std::path::Path) -> PathBuf {
    let mut path = archive.as_os_str().to_os_string();
    path.push(".checksum");
    PathBuf::from(path)
}

fn source_url_cache_path(archive: &std::path::Path) -> PathBuf {
    let mut path = archive.as_os_str().to_os_string();
    path.push(".source-url");
    PathBuf::from(path)
}

fn write_cached_source_url(archive: &std::path::Path, url: &str) {
    let _ = std::fs::write(source_url_cache_path(archive), url);
}

fn read_cached_source_url(archive: &std::path::Path) -> Option<String> {
    let value = std::fs::read_to_string(source_url_cache_path(archive)).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn write_cached_checksum(archive: &std::path::Path, checksum: &Checksum) {
    let algorithm = match checksum.algo {
        HashAlgo::Sha256 => "sha256",
        HashAlgo::Sha512 => "sha512",
        HashAlgo::Blake3 => "blake3",
    };
    let _ = std::fs::write(
        checksum_cache_path(archive),
        format!("{algorithm} {}\n", checksum.hex),
    );
}

fn read_cached_checksum(archive: &std::path::Path) -> Option<Checksum> {
    let value = std::fs::read_to_string(checksum_cache_path(archive)).ok()?;
    let (algorithm, hex) = value.trim().split_once(' ')?;
    let algo = match algorithm {
        "sha256" => HashAlgo::Sha256,
        "sha512" => HashAlgo::Sha512,
        "blake3" => HashAlgo::Blake3,
        _ => return None,
    };
    Some(Checksum {
        algo,
        hex: hex.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn offline_install_uses_cached_archive() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let archive = dirs
            .downloads()
            .join("fixture")
            .join("1.0.0")
            .join("fixture.tgz");
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            let contents = b"offline";
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "package/bin/tool", &contents[..])
                .unwrap();
            tar.finish().unwrap();
        }
        let checksum = Checksum {
            algo: HashAlgo::Sha256,
            hex: verify::hash_file(&archive, HashAlgo::Sha256).unwrap(),
        };
        let cas = Cas::new(dirs.store.clone());
        let plan = InstallPlan {
            tool: "fixture".into(),
            version: "1.0.0".into(),
            urls: vec!["http://127.0.0.1:9/never-requested".into()],
            file_name: "fixture.tgz".into(),
            kind: ArchiveKind::TarGz,
            checksum: Some(checksum),
            strip_root: true,
        };
        let client = reqwest::Client::new();
        let ctx = PipelineCtx {
            client: &client,
            dirs: &dirs,
            cas: &cas,
            link_mode: LinkMode::Copy,
            show_progress: false,
            offline: true,
            require_checksums: true,
        };

        let install = run(&plan, &ctx).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(install.join("bin/tool")).unwrap(),
            "offline"
        );
        assert!(install.join(COMPLETE_MARKER).is_file());
        assert!(checksum_cache_path(&archive).is_file());
        assert_eq!(
            artifact_receipt(&dirs, "fixture", "1.0.0").unwrap(),
            ArtifactReceipt {
                url: "http://127.0.0.1:9/never-requested".into(),
                file_name: "fixture.tgz".into(),
                checksum: Some(format!(
                    "sha256:{}",
                    verify::hash_file(&archive, HashAlgo::Sha256).unwrap()
                )),
                evidence: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn required_checksum_rejects_unverified_archive() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let archive = dirs
            .downloads()
            .join("fixture")
            .join("2.0.0")
            .join("fixture.tgz");
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        std::fs::write(&archive, b"not-read-before-checksum-gate").unwrap();
        let cas = Cas::new(dirs.store.clone());
        let client = reqwest::Client::new();
        let plan = InstallPlan {
            tool: "fixture".into(),
            version: "2.0.0".into(),
            urls: vec!["http://127.0.0.1:9/never-requested".into()],
            file_name: "fixture.tgz".into(),
            kind: ArchiveKind::TarGz,
            checksum: None,
            strip_root: true,
        };
        let strict = PipelineCtx {
            client: &client,
            dirs: &dirs,
            cas: &cas,
            link_mode: LinkMode::Copy,
            show_progress: false,
            offline: true,
            require_checksums: true,
        };
        let error = run(&plan, &strict).await.unwrap_err();
        assert!(error.to_string().contains("checksum required"));

        let permissive = PipelineCtx {
            require_checksums: false,
            ..strict
        };
        let error = run(&plan, &permissive).await.unwrap_err();
        assert!(!error.to_string().contains("checksum required"));
    }
}
