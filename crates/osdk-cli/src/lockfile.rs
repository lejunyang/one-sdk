use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use osdk_core::backend::npm_package::{
    LOCKED_NPM_LOCKFILE_OPTION, LOCKED_NPM_LOCK_FORMAT_OPTION, LOCKED_NPM_LOCK_SHA256_OPTION,
    LOCKED_NPM_NODE_VERSION_OPTION, LOCKED_NPM_PACKAGE_OPTION,
};
use osdk_core::pipeline::HashAlgo;
use osdk_core::platform::{Arch, Libc, Os, Platform};
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};
use serde::{Deserialize, Serialize};

pub const LOCKFILE_NAME: &str = "osdk.lock";
const NPM_LOCK_FORMAT: &str = "aube-v9";
const NPM_LOCKFILE_PATH: &str = "project/aube-lock.yaml";
const NPM_GRAPH_DIRECTORY: &str = "osdk.lock.d/npm";
const MAX_LOCKFILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NPM_GRAPH_BYTES: u64 = 16 * 1024 * 1024;
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub schema: u32,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformLock>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, LockedModel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlatformLock {
    #[serde(default)]
    pub tools: BTreeMap<String, LockedTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedTool {
    pub request: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<LockedArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<LockedNpmGraph>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LockedNpmGraph {
    Sidecar(LockedNpmSidecar),
    Legacy(LegacyLockedNpmGraph),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNpmSidecar {
    pub package: String,
    pub node_version: String,
    pub lock_format: String,
    pub sha256: String,
    pub graph: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyLockedNpmGraph {
    pub package: String,
    pub lock_format: String,
    pub lock_sha256: String,
    pub lockfile: String,
}

impl LockedNpmGraph {
    fn sidecar(&self, backend: &str) -> Result<&LockedNpmSidecar> {
        match self {
            Self::Sidecar(sidecar) => Ok(sidecar),
            Self::Legacy(_) => anyhow::bail!(osdk_core::t!(
                "err.lock_npm_legacy_inline_regenerate",
                backend = backend
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedArtifact {
    pub url: String,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<osdk_core::verification::VerificationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedModel {
    pub provider: osdk_core::model::ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub files: Vec<LockedModelFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedModelFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

fn schema_version() -> u32 {
    2
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }
}

impl Default for LockedTool {
    fn default() -> Self {
        LockedTool {
            request: "latest".into(),
            version: String::new(),
            options: BTreeMap::new(),
            artifact: None,
            npm: None,
        }
    }
}

pub fn platform_key(platform: Platform) -> String {
    let os = match platform.os {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
        Arch::X86 => "x86",
        Arch::Arm => "arm",
    };
    match platform.libc {
        Libc::Musl => format!("{os}-{arch}-musl"),
        _ => format!("{os}-{arch}"),
    }
}

pub fn platform_for_resolved(host: Platform, resolved: &[(ToolRequest, ToolVersion)]) -> Platform {
    let mut platform = host;
    for (request, version) in resolved {
        if request.backend != "node" {
            continue;
        }
        if let Some(arch) = version
            .options
            .get("arch")
            .and_then(|value| Arch::parse_node(value))
        {
            platform.arch = arch;
        }
    }
    platform
}

pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|directory| directory.join(LOCKFILE_NAME))
        .find(|path| path.is_file())
}

pub fn default_path(start: &Path) -> PathBuf {
    find(start).unwrap_or_else(|| start.join(LOCKFILE_NAME))
}

pub fn load(path: &Path) -> Result<Lockfile> {
    let bytes = read_bounded(path, MAX_LOCKFILE_BYTES)
        .with_context(|| format!("reading lockfile {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| osdk_core::t!("err.lockfile_not_utf8", path = path.display()))?;
    let lockfile: Lockfile =
        toml::from_str(&text).with_context(|| format!("parsing lockfile {}", path.display()))?;
    if !(1..=schema_version()).contains(&lockfile.schema) {
        anyhow::bail!(
            "unsupported lockfile schema {} in {}",
            lockfile.schema,
            path.display()
        );
    }
    if lockfile.schema == schema_version() {
        validate_schema_two(path, &lockfile, false)?;
    }
    Ok(lockfile)
}

pub fn locked_requests(path: &Path, platform: Platform) -> Result<Option<Vec<ToolRequest>>> {
    let lockfile = load(path)?;
    if lockfile.schema == 1 {
        reject_legacy_npm_entries(&lockfile)?;
    }
    let Some(platform_lock) = lockfile.platforms.get(&platform_key(platform)) else {
        return Ok(None);
    };
    let requests = platform_lock
        .tools
        .iter()
        .map(|(backend, locked)| {
            validate_locked_tool_identity(backend, locked)?;
            let mut options = locked.options.clone();
            if let Some(artifact) = &locked.artifact {
                options.insert(
                    osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                    artifact.url.clone(),
                );
                options.insert(
                    osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                    artifact.file_name.clone(),
                );
                if let Some(checksum) = &artifact.checksum {
                    options.insert(
                        osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                        checksum.clone(),
                    );
                }
                if let Some(subdir) = &artifact.subdir {
                    options.insert(
                        osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION.into(),
                        subdir.clone(),
                    );
                }
            }
            if let Some(npm) = &locked.npm {
                let npm = npm.sidecar(backend)?;
                let lockfile = read_npm_graph_sidecar(path, backend, npm)?;
                options.insert(LOCKED_NPM_PACKAGE_OPTION.into(), npm.package.clone());
                options.insert(
                    LOCKED_NPM_LOCK_FORMAT_OPTION.into(),
                    npm.lock_format.clone(),
                );
                options.insert(LOCKED_NPM_LOCK_SHA256_OPTION.into(), npm.sha256.clone());
                options.insert(LOCKED_NPM_LOCKFILE_OPTION.into(), lockfile);
                options.insert(
                    LOCKED_NPM_NODE_VERSION_OPTION.into(),
                    npm.node_version.clone(),
                );
            }
            Ok(ToolRequest {
                backend: backend.clone(),
                spec: VersionSpec::Exact(locked.version.clone()),
                options,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(requests))
}

fn validate_schema_two(path: &Path, lockfile: &Lockfile, read_graphs: bool) -> Result<()> {
    validate_complete_npm_entries(path, lockfile, read_graphs)
}

fn validate_complete_npm_entries(
    path: &Path,
    lockfile: &Lockfile,
    read_graphs: bool,
) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        let node = platform_lock.tools.get("node");
        for (backend, locked) in &platform_lock.tools {
            validate_locked_tool_identity(backend, locked)?;
            match (backend.strip_prefix("npm:"), locked.npm.as_ref()) {
                (Some(package), Some(LockedNpmGraph::Sidecar(npm))) => {
                    if locked.artifact.is_some() {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema2_npm_artifact_forbidden",
                            backend = backend,
                            platform = platform
                        ));
                    }
                    if package.is_empty() || npm.package != package {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_package_mismatch",
                            backend = backend,
                            platform = platform,
                            expected = package,
                            actual = npm.package
                        ));
                    }
                    if npm.lock_format != NPM_LOCK_FORMAT {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_graph_format_unsupported",
                            format = npm.lock_format,
                            backend = backend,
                            platform = platform,
                            expected = NPM_LOCK_FORMAT
                        ));
                    }
                    validate_sha256(backend, &npm.sha256)?;
                    let expected_graph = npm_graph_relative_path(&npm.sha256);
                    if npm.graph != expected_graph {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_graph_path_unsafe_platform",
                            graph = npm.graph,
                            backend = backend,
                            platform = platform,
                            expected = expected_graph
                        ));
                    }
                    let node = node.ok_or_else(|| {
                        anyhow::anyhow!(osdk_core::t!(
                            "err.lock_npm_node_entry_required",
                            backend = backend,
                            platform = platform
                        ))
                    })?;
                    if npm.node_version != node.version {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_node_version_mismatch",
                            backend = backend,
                            platform = platform,
                            expected = node.version,
                            actual = npm.node_version
                        ));
                    }
                    if read_graphs {
                        read_npm_graph_sidecar(path, backend, npm)?;
                    }
                }
                (Some(_), None) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_npm_graph_missing",
                        backend = backend,
                        platform = platform
                    ));
                }
                (Some(_), Some(LockedNpmGraph::Legacy(_))) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_schema2_npm_legacy_inline",
                        backend = backend,
                        platform = platform
                    ));
                }
                (None, Some(_)) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_non_npm_graph_metadata",
                        backend = backend,
                        platform = platform
                    ));
                }
                (None, None) => {}
            }
        }
    }
    Ok(())
}

fn validate_sha256(backend: &str, sha256: &str) -> Result<()> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_graph_sha256_invalid",
            backend = backend,
            sha256 = sha256
        ));
    }
    Ok(())
}

fn npm_graph_relative_path(sha256: &str) -> String {
    format!("{NPM_GRAPH_DIRECTORY}/{sha256}.yaml")
}

fn npm_graph_path(lock_path: &Path, graph: &str) -> PathBuf {
    lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(graph)
}

fn read_npm_graph_sidecar(
    lock_path: &Path,
    backend: &str,
    npm: &LockedNpmSidecar,
) -> Result<String> {
    validate_sha256(backend, &npm.sha256)?;
    let expected_graph = npm_graph_relative_path(&npm.sha256);
    if npm.graph != expected_graph {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_graph_path_unsafe",
            graph = npm.graph,
            backend = backend,
            expected = expected_graph
        ));
    }
    let graph_path = npm_graph_path(lock_path, &npm.graph);
    reject_symlinked_graph_path(lock_path, &graph_path)?;
    let bytes = read_bounded(&graph_path, MAX_NPM_GRAPH_BYTES)
        .with_context(|| osdk_core::t!("err.lock_npm_sidecar_read", path = graph_path.display()))?;
    let actual = osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256);
    if actual != npm.sha256 {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_sidecar_checksum_mismatch",
            backend = backend,
            expected = npm.sha256,
            actual = actual
        ));
    }
    String::from_utf8(bytes).with_context(|| {
        osdk_core::t!("err.lock_npm_sidecar_not_utf8", path = graph_path.display())
    })
}

fn reject_symlinked_graph_path(lock_path: &Path, graph_path: &Path) -> Result<()> {
    let parent = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut current = parent.to_path_buf();
    for component in ["osdk.lock.d", "npm"] {
        current.push(component);
        if std::fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_npm_graph_path_symlink",
                path = current.display()
            ));
        }
    }
    if std::fs::symlink_metadata(graph_path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_sidecar_symlink",
            path = graph_path.display()
        ));
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > maximum {
        anyhow::bail!(osdk_core::t!(
            "err.file_size_limit_exceeded",
            maximum = maximum
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        anyhow::bail!(osdk_core::t!(
            "err.file_size_limit_exceeded",
            maximum = maximum
        ));
    }
    Ok(bytes)
}

fn validate_locked_tool_identity(backend: &str, locked: &LockedTool) -> Result<()> {
    if backend.trim().is_empty()
        || backend.split([':', '/', '\\']).any(|part| {
            part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_whitespace)
        })
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_backend_id_unsafe",
            backend = backend
        ));
    }
    let version = locked.version.trim();
    if version.is_empty()
        || version == "."
        || version == ".."
        || version.contains(['/', '\\'])
        || std::path::Path::new(version).is_absolute()
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_version_unsafe",
            version = locked.version,
            backend = backend
        ));
    }
    if let Some(artifact) = &locked.artifact {
        let file = std::path::Path::new(&artifact.file_name);
        if file.components().count() != 1
            || !matches!(
                file.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_artifact_filename_unsafe",
                file_name = artifact.file_name,
                backend = backend
            ));
        }
        if let Some(subdir) = &artifact.subdir {
            let path = std::path::Path::new(subdir);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                anyhow::bail!(osdk_core::t!(
                    "err.lock_artifact_subdir_unsafe",
                    subdir = subdir,
                    backend = backend
                ));
            }
        }
    }
    Ok(())
}

pub fn merge_resolved(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    };
    if lockfile.schema == 1 {
        reject_legacy_npm_entries(&lockfile)?;
    }
    let node_version = resolved
        .iter()
        .rev()
        .find_map(|(_, version)| (version.backend == "node").then(|| version.version.clone()));
    let mut npm_graphs = Vec::new();
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    platform_lock.tools.clear();
    for (request, version) in resolved {
        if version.backend == "rust"
            && dirs
                .install_path("rust", &version.version)
                .join(".osdk-linked")
                .is_file()
        {
            anyhow::bail!(
                "linked Rust toolchain `{}` is local-only and cannot be written as a reproducible lock artifact",
                version.version
            );
        }
        let npm = locked_npm_graph(dirs, version, node_version.as_deref())?;
        let npm_metadata = npm.as_ref().map(|graph| graph.metadata.clone());
        if let Some(graph) = npm {
            npm_graphs.push(graph);
        }
        let mut options = public_options(&version.options);
        if npm_metadata.is_some() {
            options.remove("node_version");
        }
        let artifact = if version.backend.starts_with("npm:") {
            None
        } else {
            let resolved_artifact =
                osdk_core::pipeline::locked_artifact(version)?.map(|receipt| LockedArtifact {
                    url: receipt.url,
                    file_name: receipt.file_name,
                    checksum: receipt.checksum,
                    subdir: version.options.get("catalog-subdir").cloned(),
                    evidence: receipt.evidence,
                });
            osdk_core::pipeline::artifact_receipt(dirs, &version.backend, &version.version)
                .map(|receipt| LockedArtifact {
                    url: receipt.url,
                    file_name: receipt.file_name,
                    checksum: receipt.checksum,
                    subdir: version.options.get("catalog-subdir").cloned(),
                    evidence: receipt.evidence,
                })
                .or(resolved_artifact)
        };
        platform_lock.tools.insert(
            request.backend.clone(),
            LockedTool {
                request: request.spec.to_string(),
                version: version.version.clone(),
                options,
                artifact,
                npm: npm_metadata.map(LockedNpmGraph::Sidecar),
            },
        );
    }
    save_with_npm_graphs(path, &lockfile, &npm_graphs)
}

pub fn merge_model(path: &Path, manifest: &osdk_core::model::SnapshotManifest) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    if lockfile.schema == 1 {
        reject_legacy_npm_entries(&lockfile)?;
    }
    lockfile.models.insert(
        manifest.name.clone(),
        LockedModel {
            provider: manifest.provider,
            repository: manifest.repository.clone(),
            requested_revision: manifest.requested_revision.clone(),
            revision: manifest.revision.clone(),
            endpoint: manifest.endpoint.clone(),
            variant: manifest.variant.clone(),
            files: manifest
                .files
                .iter()
                .map(|file| {
                    Ok(LockedModelFile {
                        path: file.path.clone(),
                        size: file.size,
                        sha256: file.sha256.clone().ok_or_else(|| {
                            anyhow::anyhow!("model lock requires SHA-256 for {}", file.path)
                        })?,
                    })
                })
                .collect::<Result<_>>()?,
        },
    );
    save(path, &lockfile)
}

fn public_options(options: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    options
        .iter()
        .filter(|(key, _)| !key.starts_with("__osdk_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn reject_legacy_npm_entries(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock
            .tools
            .iter()
            .find(|(backend, _)| backend.starts_with("npm:"))
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_schema1_npm_migration_requires_graph",
                backend = backend,
                platform = platform
            ));
        }
    }
    Ok(())
}

struct PendingNpmGraph {
    metadata: LockedNpmSidecar,
    bytes: Vec<u8>,
}

fn locked_npm_graph(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
    node_version: Option<&str>,
) -> Result<Option<PendingNpmGraph>> {
    let Some(package) = version.backend.strip_prefix("npm:") else {
        return Ok(None);
    };
    let node_version = node_version.ok_or_else(|| {
        anyhow::anyhow!(osdk_core::t!(
            "err.lock_npm_resolved_node_required",
            backend = version.backend
        ))
    })?;
    let lock_path = dirs
        .install_path(&version.backend, &version.version)
        .join(NPM_LOCKFILE_PATH);
    let bytes = read_bounded(&lock_path, MAX_NPM_GRAPH_BYTES)
        .with_context(|| osdk_core::t!("err.npm_lock_payload_read", path = lock_path.display()))?;
    let sha256 = osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256);
    std::str::from_utf8(&bytes).with_context(|| {
        osdk_core::t!("err.npm_lock_payload_not_utf8", path = lock_path.display())
    })?;
    Ok(Some(PendingNpmGraph {
        metadata: LockedNpmSidecar {
            package: package.to_string(),
            node_version: node_version.to_string(),
            lock_format: NPM_LOCK_FORMAT.into(),
            graph: npm_graph_relative_path(&sha256),
            sha256,
        },
        bytes,
    }))
}

fn save(path: &Path, lockfile: &Lockfile) -> Result<()> {
    save_with_npm_graphs(path, lockfile, &[])
}

fn save_with_npm_graphs(
    path: &Path,
    lockfile: &Lockfile,
    npm_graphs: &[PendingNpmGraph],
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| osdk_core::t!("err.fs_directory_create", path = parent.display()))?;
    }
    if lockfile.schema == 1 {
        reject_legacy_npm_entries(lockfile)?;
    }
    let mut lockfile = lockfile.clone();
    lockfile.schema = schema_version();
    validate_schema_two(path, &lockfile, false)?;
    let text = toml::to_string_pretty(&lockfile)?;
    if text.len() as u64 > MAX_LOCKFILE_BYTES {
        anyhow::bail!(osdk_core::t!(
            "err.lockfile_size_limit_exceeded",
            maximum = MAX_LOCKFILE_BYTES
        ));
    }
    for graph in npm_graphs {
        let destination = npm_graph_path(path, &graph.metadata.graph);
        reject_symlinked_graph_path(path, &destination)?;
        atomic_write(&destination, &graph.bytes)?;
    }
    validate_schema_two(path, &lockfile, true)?;
    atomic_write(path, text.as_bytes())?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| osdk_core::t!("err.fs_directory_create", path = parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(osdk_core::t!(
                "err.lock_atomic_path_filename_missing",
                path = path.display()
            ))
        })?
        .to_string_lossy();
    let (temporary, mut file) = loop {
        let serial = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.tmp-{}-{serial}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    osdk_core::t!("err.fs_file_create", path = temporary.display())
                });
            }
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| osdk_core::t!("err.fs_file_write", path = temporary.display()))?;
        file.sync_all()
            .with_context(|| osdk_core::t!("err.fs_file_sync", path = temporary.display()))?;
        drop(file);
        atomic_replace(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| osdk_core::t!("err.fs_file_replace", path = destination.display()))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| osdk_core::t!("err.fs_file_replace", path = destination.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        }
    }

    #[test]
    fn merge_preserves_other_platforms() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut initial = Lockfile {
            schema: 1,
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
        };
        initial.platforms.insert(
            "windows-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "node".into(),
                    LockedTool {
                        request: "20".into(),
                        version: "20.19.0".into(),
                        options: BTreeMap::new(),
                        artifact: None,
                        npm: None,
                    },
                )]),
            },
        );
        save(&path, &initial).unwrap();

        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@20").unwrap(),
                ToolVersion::new("node", "20.20.0"),
            )],
        )
        .unwrap();
        let lockfile = load(&path).unwrap();
        assert_eq!(lockfile.schema, schema_version());
        assert_eq!(lockfile.platforms.len(), 2);
        assert_eq!(
            lockfile.platforms["linux-x64"].tools["node"].version,
            "20.20.0"
        );
        assert_eq!(
            lockfile.platforms["windows-x64"].tools["node"].version,
            "20.19.0"
        );
    }

    #[test]
    fn schema_one_non_npm_lock_is_read_and_migrated_on_merge() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools.node]
request = "20"
version = "20.19.0"
"#,
        )
        .unwrap();

        let legacy = load(&path).unwrap();
        assert_eq!(legacy.schema, 1);
        assert!(legacy.platforms["linux-x64"].tools["node"].npm.is_none());
        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].backend, "node");
        assert_eq!(requests[0].spec, VersionSpec::Exact("20.19.0".into()));

        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@20").unwrap(),
                ToolVersion::new("node", "20.20.0"),
            )],
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().schema, schema_version());
    }

    #[test]
    fn unsupported_schema_versions_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        for schema in [0, schema_version() + 1] {
            std::fs::write(&path, format!("schema = {schema}\n")).unwrap();
            let error = load(&path).unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("unsupported lockfile schema {schema}")));
        }
    }

    #[test]
    fn locked_requests_reject_path_traversal_before_constructing_install_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 2

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "../../../../victim"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "aube-v9"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
graph = "osdk.lock.d/npm/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.yaml"
"#,
        )
        .unwrap();

        let error = locked_requests(&path, linux()).unwrap_err();
        assert!(error.to_string().contains("unsafe version"));
    }

    #[test]
    fn saving_a_schema_one_lock_without_merging_upgrades_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let legacy = Lockfile {
            schema: 1,
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
        };
        save(&path, &legacy).unwrap();
        assert_eq!(load(&path).unwrap().schema, schema_version());
    }

    #[test]
    fn direct_save_rejects_schema_one_npm_without_changing_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, b"sentinel").unwrap();
        let legacy = Lockfile {
            schema: 1,
            platforms: BTreeMap::from([(
                "linux-x64".into(),
                PlatformLock {
                    tools: BTreeMap::from([(
                        "npm:prettier".into(),
                        LockedTool {
                            request: "3".into(),
                            version: "3.6.2".into(),
                            options: BTreeMap::new(),
                            artifact: None,
                            npm: None,
                        },
                    )]),
                },
            )]),
            models: BTreeMap::new(),
        };
        let error = save(&path, &legacy).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
    }

    #[test]
    fn schema_is_required_instead_of_defaulting_to_current() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            "[platforms.linux-x64.tools.node]\nrequest = \"20\"\nversion = \"20.0.0\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err();
        assert!(error.to_string().contains("parsing lockfile"));
    }

    #[test]
    fn schema_one_legacy_npm_is_readable_but_cannot_be_consumed_or_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.windows-x64.tools.node]
request = "20"
version = "20.19.0"

[platforms.windows-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"
"#,
        )
        .unwrap();

        assert_eq!(load(&path).unwrap().schema, 1);
        let error = locked_requests(
            &path,
            Platform {
                os: Os::Windows,
                arch: Arch::X64,
                libc: Libc::None,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));

        let before = std::fs::read(&path).unwrap();
        let error = merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@24").unwrap(),
                ToolVersion::new("node", "24.1.0"),
            )],
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let error = merge_model(&path, &test_model_manifest()).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn load_rejects_oversized_main_lock_before_toml_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_LOCKFILE_BYTES + 1).unwrap();
        let error = load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds maximum size"));
    }

    #[test]
    fn save_rejects_oversized_main_lock_without_replacing_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, b"sentinel").unwrap();
        let lockfile = Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::from([(
                "oversized".into(),
                LockedModel {
                    provider: osdk_core::model::ProviderId::HuggingFace,
                    repository: "x".repeat(MAX_LOCKFILE_BYTES as usize),
                    requested_revision: "main".into(),
                    revision: "abc123".into(),
                    endpoint: "https://huggingface.co".into(),
                    variant: None,
                    files: Vec::new(),
                },
            )]),
        };

        let error = save(&path, &lockfile).unwrap_err();
        assert!(error.to_string().contains("exceeds maximum size"));
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
    }

    #[test]
    fn schema_one_inline_npm_is_readable_but_cannot_be_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools.node]
request = "20"
version = "20.19.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
lock_format = "aube-v9"
lock_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
lockfile = "lockfileVersion: '9.0'"
"#,
        )
        .unwrap();

        let legacy = load(&path).unwrap();
        assert!(matches!(
            legacy.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Legacy(_))
        ));
        let before = std::fs::read(&path).unwrap();
        let error = save(&path, &legacy).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn scoped_npm_graph_round_trips_exact_payload_and_injects_private_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let backend = "npm:@antfu/ni";
        let version_number = "0.21.12";
        let lockfile = "lockfileVersion: '9.0'\r\nimporters:\r\n  .:\r\n    dependencies:\r\n      '@antfu/ni':\r\n        version: 0.21.12\r\n# caf\u{e9}\r\n";
        let installed_lock = dirs
            .install_path(backend, version_number)
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, lockfile.as_bytes()).unwrap();

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("npm:@antfu/ni@0.21.12").unwrap(),
                    ToolVersion::new(backend, version_number),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();

        let expected_sha256 =
            osdk_core::pipeline::verify::hash_bytes(lockfile.as_bytes(), HashAlgo::Sha256);
        assert_eq!(
            expected_sha256,
            "23c7efec1baea86b6efc78164c175fc6cfe3458e5f70f2ec018a94045f94d7b3"
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.schema, schema_version());
        assert!(loaded.platforms["linux-x64"].tools[backend]
            .artifact
            .is_none());
        let npm = loaded.platforms["linux-x64"].tools[backend]
            .npm
            .as_ref()
            .unwrap()
            .sidecar(backend)
            .unwrap();
        assert_eq!(npm.package, "@antfu/ni");
        assert_eq!(npm.node_version, "24.1.0");
        assert_eq!(npm.lock_format, NPM_LOCK_FORMAT);
        assert_eq!(npm.sha256, expected_sha256);
        assert_eq!(npm.graph, format!("osdk.lock.d/npm/{expected_sha256}.yaml"));
        assert_eq!(
            std::fs::read(npm_graph_path(&path, &npm.graph)).unwrap(),
            lockfile.as_bytes()
        );
        let main_lock = std::fs::read_to_string(&path).unwrap();
        assert!(!main_lock.contains("lockfile ="));
        assert!(!main_lock.contains("lock_sha256"));
        assert!(!main_lock.contains("# caf"));

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let options = &requests
            .iter()
            .find(|request| request.backend == backend)
            .unwrap()
            .options;
        assert_eq!(options[LOCKED_NPM_PACKAGE_OPTION], "@antfu/ni");
        assert_eq!(options[LOCKED_NPM_LOCK_FORMAT_OPTION], NPM_LOCK_FORMAT);
        assert_eq!(options[LOCKED_NPM_LOCK_SHA256_OPTION], expected_sha256);
        assert_eq!(options[LOCKED_NPM_NODE_VERSION_OPTION], "24.1.0");
        assert_eq!(
            options[LOCKED_NPM_LOCKFILE_OPTION].as_bytes(),
            lockfile.as_bytes()
        );
    }

    #[test]
    fn npm_lock_requires_a_resolved_node_and_ignores_stale_artifact_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let backend = "npm:prettier";
        let version = "3.6.2";
        let install = dirs.install_path(backend, version);
        std::fs::create_dir_all(install.join("project")).unwrap();
        std::fs::write(install.join(NPM_LOCKFILE_PATH), b"lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(
            install.join(".osdk-artifact.json"),
            r#"{"url":"https://evil.invalid/fake.tgz","file_name":"fake.tgz"}"#,
        )
        .unwrap();

        let npm_only = [(
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolVersion::new(backend, version),
        )];
        let error = merge_resolved(&path, linux(), &dirs, &npm_only).unwrap_err();
        assert!(error
            .to_string()
            .contains("without a resolved `node` entry"));
        assert!(!path.exists());

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                npm_only[0].clone(),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();
        assert!(load(&path).unwrap().platforms["linux-x64"].tools[backend]
            .artifact
            .is_none());
    }

    #[test]
    fn npm_lock_binds_to_the_final_resolved_node_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n").unwrap();

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("node@20.19.0").unwrap(),
                    ToolVersion::new("node", "20.19.0"),
                ),
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let npm = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
            .sidecar("npm:prettier")
            .unwrap();
        assert_eq!(npm.node_version, "24.1.0");
        assert_eq!(lock.platforms["linux-x64"].tools["node"].version, "24.1.0");
    }

    #[test]
    fn replacing_an_npm_graph_does_not_delete_the_old_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();

        let resolved = [
            (
                ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                ToolVersion::new("npm:prettier", "3.6.2"),
            ),
            (
                ToolRequest::parse("node@24.1.0").unwrap(),
                ToolVersion::new("node", "24.1.0"),
            ),
        ];
        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n# first\n").unwrap();
        merge_resolved(&path, linux(), &dirs, &resolved).unwrap();
        let first = npm_graph_path(
            &path,
            &npm_graph_relative_path(&osdk_core::pipeline::verify::hash_bytes(
                b"lockfileVersion: '9.0'\n# first\n",
                HashAlgo::Sha256,
            )),
        );
        assert!(first.is_file());

        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n# second\n").unwrap();
        merge_resolved(&path, linux(), &dirs, &resolved).unwrap();
        assert!(first.is_file());
    }

    #[test]
    fn schema_two_npm_invariants_are_rejected_before_sidecar_read() {
        let cases = [
            ("package = \"other\"", "package mismatch"),
            ("node_version = \"23.0.0\"", "node version mismatch"),
            (
                "lock_format = \"future-v10\"",
                "unsupported npm graph format",
            ),
            (
                "sha256 = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"",
                "invalid SHA-256",
            ),
            ("graph = \"../outside.yaml\"", "unsafe npm graph path"),
        ];
        for (replacement, expected) in cases {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(LOCKFILE_NAME);
            let base = valid_schema_two_npm_lock();
            let text = if replacement.starts_with("package") {
                base.replace("package = \"prettier\"", replacement)
            } else if replacement.starts_with("node_version") {
                base.replace("node_version = \"24.1.0\"", replacement)
            } else if replacement.starts_with("lock_format") {
                base.replace("lock_format = \"aube-v9\"", replacement)
            } else if replacement.starts_with("sha256") {
                base.replace(&format!("sha256 = \"{}\"", "a".repeat(64)), replacement)
            } else {
                base.replace(
                    &format!("graph = \"osdk.lock.d/npm/{}.yaml\"", "a".repeat(64)),
                    replacement,
                )
            };
            std::fs::write(&path, text).unwrap();
            let error = load(&path).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn schema_two_rejects_missing_graph_non_npm_metadata_and_generic_npm_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let missing = valid_schema_two_npm_lock()
            .split("[platforms.linux-x64.tools.\"npm:prettier\".npm]")
            .next()
            .unwrap()
            .to_string();
        std::fs::write(&path, missing).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("missing its dependency graph"));

        let non_npm =
            valid_schema_two_npm_lock().replace("tools.\"npm:prettier\"", "tools.prettier");
        std::fs::write(&path, non_npm).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot carry npm graph metadata"));

        let with_artifact = valid_schema_two_npm_lock().replace(
            "[platforms.linux-x64.tools.\"npm:prettier\".npm]",
            "[platforms.linux-x64.tools.\"npm:prettier\".artifact]\nurl = \"https://evil.invalid/fake.tgz\"\nfile_name = \"fake.tgz\"\n\n[platforms.linux-x64.tools.\"npm:prettier\".npm]",
        );
        std::fs::write(&path, with_artifact).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot carry a generic artifact receipt"));
    }

    #[test]
    fn schema_two_npm_table_rejects_inline_and_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock()
            .replace("graph =", "lockfile = \"inline graph forbidden\"\ngraph =");
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("did not match any variant"));
    }

    #[test]
    fn schema_two_npm_requires_a_matching_node_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock().replace(
            "[platforms.linux-x64.tools.node]\nrequest = \"24\"\nversion = \"24.1.0\"\n\n",
            "",
        );
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(error.to_string().contains("requires a locked `node` entry"));
    }

    #[test]
    fn schema_two_rejects_npm_metadata_without_an_npm_backend() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock().replace("tools.\"npm:prettier\"", "tools.prettier");
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot carry npm graph metadata"));
    }

    #[test]
    fn locked_requests_rejects_missing_tampered_and_oversized_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let missing = locked_requests(&path, linux()).unwrap_err();
        assert!(format!("{missing:#}").contains("reading npm graph sidecar"));

        let sidecar = npm_graph_path(&path, &npm_graph_relative_path(&"a".repeat(64)));
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"tampered").unwrap();
        let tampered = locked_requests(&path, linux()).unwrap_err();
        assert!(tampered.to_string().contains("checksum mismatch"));

        let oversized = std::fs::File::create(&sidecar).unwrap();
        oversized.set_len(MAX_NPM_GRAPH_BYTES + 1).unwrap();
        drop(oversized);
        let oversized = locked_requests(&path, linux()).unwrap_err();
        assert!(format!("{oversized:#}").contains("exceeds maximum size"));
    }

    #[cfg(unix)]
    #[test]
    fn locked_requests_rejects_symlinked_sidecar_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("npm")).unwrap();
        std::fs::create_dir_all(temp.path().join("osdk.lock.d")).unwrap();
        symlink(&outside, temp.path().join("osdk.lock.d/npm")).unwrap();

        let error = locked_requests(&path, linux()).unwrap_err();
        assert!(error.to_string().contains("path contains symlink"));
    }

    #[test]
    fn npm_merge_requires_an_installed_graph_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let error = merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("reading npm lock payload"));
        assert!(error.to_string().contains("aube-lock.yaml"));
        assert!(!path.exists());
    }

    #[test]
    fn npm_merge_rejects_non_utf8_graph_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, [0xff, 0xfe]).unwrap();

        let error = merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("is not UTF-8"));
        assert!(!path.exists());
    }

    #[test]
    fn model_lock_preserves_platforms_and_records_file_digests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("node@20").unwrap(),
                ToolVersion::new("node", "20.20.0"),
            )],
        )
        .unwrap();
        let manifest = test_model_manifest();
        merge_model(&path, &manifest).unwrap();
        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools.contains_key("node"));
        assert_eq!(lock.models["qwen"].revision, "abc123");
        assert_eq!(lock.models["qwen"].files[0].sha256, "sha256");
    }

    #[test]
    fn legacy_model_provider_spelling_remains_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[models.fixture]
provider = "hugging-face"
repository = "owner/repo"
requested_revision = "main"
revision = "abc123"
endpoint = "https://huggingface.co"
files = []
"#,
        )
        .unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(
            lock.models["fixture"].provider,
            osdk_core::model::ProviderId::HuggingFace
        );
    }

    #[test]
    fn locked_requests_restore_exact_versions_and_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("rust", "stable");
        version.options.insert("profile".into(), "minimal".into());
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(ToolRequest::parse("rust@stable").unwrap(), version)],
        )
        .unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].spec, VersionSpec::Exact("stable".into()));
        assert_eq!(requests[0].options["profile"], "minimal");
    }

    #[test]
    fn node_arch_option_selects_the_target_platform_lock() {
        let host = linux();
        let request = ToolRequest::parse("node@20").unwrap();
        let mut version = ToolVersion::new("node", "20.20.0");
        version.options.insert("arch".into(), "arm64".into());
        let target = platform_for_resolved(host, &[(request, version)]);
        assert_eq!(platform_key(target), "linux-arm64");
    }

    #[test]
    fn unresolved_catalog_artifact_is_persisted_with_subdir() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let request = ToolRequest::parse("python@pyodide-3.14.2").unwrap();
        let mut version = ToolVersion::new("python", "pyodide-3.14.2");
        version.options.extend(BTreeMap::from([
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://example.test/pyodide.tar.gz".into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "pyodide.tar.gz".into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{}", "a".repeat(64)),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION.into(),
                "pyodide-root/dist".into(),
            ),
            ("catalog-subdir".into(), "pyodide-root/dist".into()),
        ]));
        merge_resolved(&path, linux(), &dirs, &[(request, version)]).unwrap();
        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["python"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.file_name, "pyodide.tar.gz");
        assert_eq!(artifact.subdir.as_deref(), Some("pyodide-root/dist"));
        let restored = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(
            restored[0].options[osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION],
            "pyodide-root/dist"
        );
    }

    #[test]
    fn linked_rust_toolchain_is_rejected_from_reproducible_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let marker = dirs.install_path("rust", "local-dev");
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join(".osdk-linked"), "/local/toolchain").unwrap();
        let error = merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("rust@local-dev").unwrap(),
                ToolVersion::new("rust", "local-dev"),
            )],
        )
        .unwrap_err();
        assert!(error.to_string().contains("local-only"));
        assert!(!path.exists());
    }

    #[test]
    fn legacy_artifact_without_evidence_still_loads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools."github:cli/cli"]
request = "2.96.0"
version = "2.96.0"

[platforms.linux-x64.tools."github:cli/cli".artifact]
url = "https://example.test/gh.tar.gz"
file_name = "gh.tar.gz"
checksum = "sha256:00"
subdir = "install"
"#,
        )
        .unwrap();

        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools["github:cli/cli"]
            .artifact
            .as_ref()
            .unwrap()
            .evidence
            .is_empty());
        assert_eq!(
            lock.platforms["linux-x64"].tools["github:cli/cli"]
                .artifact
                .as_ref()
                .unwrap()
                .subdir
                .as_deref(),
            Some("install")
        );
    }

    #[test]
    fn lock_persists_verification_evidence_without_trusting_it_as_an_option() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let install = dirs.install_path("github:cli/cli", "2.96.0");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(
            install.join(".osdk-artifact.json"),
            r#"{
  "url": "https://example.test/gh.tar.gz",
  "file_name": "gh.tar.gz",
  "checksum": "sha256:00",
  "evidence": [{
    "kind": "sigstore-bundle+rekor",
    "repository": "cli/cli",
    "issuer": "https://token.actions.githubusercontent.com",
    "digest": "sha256:00"
  }]
}"#,
        )
        .unwrap();
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("github:cli/cli@2.96.0").unwrap(),
                ToolVersion::new("github:cli/cli", "2.96.0"),
            )],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["github:cli/cli"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.evidence.len(), 1);
        assert_eq!(artifact.evidence[0].kind, "sigstore-bundle+rekor");
        assert_eq!(artifact.evidence[0].repository, "cli/cli");

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert!(requests[0]
            .options
            .keys()
            .all(|key| !key.contains("evidence")));
    }

    fn valid_schema_two_npm_lock() -> String {
        let sha256 = "a".repeat(64);
        format!(
            r#"
schema = 2

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "aube-v9"
sha256 = "{sha256}"
graph = "osdk.lock.d/npm/{sha256}.yaml"
"#
        )
    }

    fn test_model_manifest() -> osdk_core::model::SnapshotManifest {
        osdk_core::model::SnapshotManifest {
            schema: 1,
            name: "qwen".into(),
            provider: osdk_core::model::ProviderId::HuggingFace,
            repository: "Qwen/Qwen2.5-7B-Instruct".into(),
            requested_revision: "main".into(),
            revision: "abc123".into(),
            endpoint: "https://huggingface.co".into(),
            variant: Some("safetensors".into()),
            files: vec![osdk_core::model::ModelFile {
                path: "config.json".into(),
                size: 10,
                cas_hash: "blake3".into(),
                sha256: Some("sha256".into()),
                etag: None,
            }],
            created_at: 0,
        }
    }

    fn test_dirs(root: &Path) -> osdk_core::dirs::Dirs {
        osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap()
    }
}
