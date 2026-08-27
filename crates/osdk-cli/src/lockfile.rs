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
const LOCKED_NPM_INSTALLER_OPTION: &str = "__osdk_npm_installer";
const LOCKED_NPM_SCOPE_OPTION: &str = "__osdk_npm_scope";
const LOCKED_NPM_NATIVE_LOCK_KIND_OPTION: &str = "__osdk_npm_native_lock_kind";
const LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION: &str = "__osdk_npm_native_lock_format";
const LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION: &str = "__osdk_npm_native_lock_sha256";
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
    Metadata(LockedNpmMetadata),
    Sidecar(LockedNpmSidecar),
    Legacy(LegacyLockedNpmGraph),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNpmMetadata {
    pub package: String,
    pub installer: NpmInstaller,
    pub scope: LockScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_lock: Option<LockedNativeLock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNativeLock {
    pub kind: NpmInstaller,
    pub format: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NpmInstaller {
    Aube,
    #[serde(alias = "package-lock", alias = "npm-shrinkwrap")]
    Npm,
    Pnpm,
}

impl NpmInstaller {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "aube" => Ok(Self::Aube),
            "npm" | "package-lock" | "npm-shrinkwrap" => Ok(Self::Npm),
            "pnpm" => Ok(Self::Pnpm),
            _ => anyhow::bail!(osdk_core::t!(
                "err.lock_npm_installer_unsupported",
                installer = value
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Aube => "aube",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockScope {
    Project,
    Global,
}

impl LockScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
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
            Self::Metadata(_) | Self::Legacy(_) => anyhow::bail!(osdk_core::t!(
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
    3
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
    match lockfile.schema {
        2 => validate_schema_two(path, &lockfile, false)?,
        3 => validate_schema_three(&lockfile)?,
        _ => {}
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
                match npm {
                    LockedNpmGraph::Metadata(npm) => inject_npm_metadata(&mut options, npm),
                    LockedNpmGraph::Sidecar(npm) => {
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
                    LockedNpmGraph::Legacy(_) => {
                        npm.sidecar(backend)?;
                        unreachable!("legacy npm metadata is rejected above")
                    }
                }
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

fn inject_npm_metadata(options: &mut BTreeMap<String, String>, npm: &LockedNpmMetadata) {
    options.insert(LOCKED_NPM_PACKAGE_OPTION.into(), npm.package.clone());
    options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        npm.installer.as_str().into(),
    );
    options.insert(LOCKED_NPM_SCOPE_OPTION.into(), npm.scope.as_str().into());
    if let Some(node_version) = &npm.node_version {
        options.insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.clone());
    }
    if let Some(native_lock) = &npm.native_lock {
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            native_lock.kind.as_str().into(),
        );
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            native_lock.format.clone(),
        );
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
            native_lock.sha256.clone(),
        );
    }
}

fn validate_schema_three(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        let node = platform_lock.tools.get("node");
        for (backend, locked) in &platform_lock.tools {
            validate_locked_tool_identity(backend, locked)?;
            match (backend.strip_prefix("npm:"), locked.npm.as_ref()) {
                (Some(package), Some(LockedNpmGraph::Metadata(npm))) => {
                    if locked.artifact.is_some() {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema3_npm_artifact_forbidden",
                            backend = backend,
                            platform = platform
                        ));
                    }
                    if let Some(key) = locked.options.keys().find(|key| {
                        key.starts_with("__osdk_npm_")
                            || key.as_str() == LOCKED_NPM_NODE_VERSION_OPTION
                    }) {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema3_npm_private_option_forbidden",
                            backend = backend,
                            platform = platform,
                            key = key
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
                    if let Some(node_version) = &npm.node_version {
                        validate_exact_node_version(backend, node_version)?;
                        if let Some(node) = node {
                            if node_version != &node.version {
                                anyhow::bail!(osdk_core::t!(
                                    "err.lock_npm_node_version_mismatch",
                                    backend = backend,
                                    platform = platform,
                                    expected = node.version,
                                    actual = node_version
                                ));
                            }
                        }
                    }
                    if let Some(native_lock) = &npm.native_lock {
                        validate_native_lock(backend, native_lock)?;
                    }
                }
                (Some(_), None) => anyhow::bail!(osdk_core::t!(
                    "err.lock_schema3_npm_metadata_missing",
                    backend = backend,
                    platform = platform
                )),
                (Some(_), Some(_)) => anyhow::bail!(osdk_core::t!(
                    "err.lock_schema3_npm_legacy_metadata",
                    backend = backend,
                    platform = platform
                )),
                (None, Some(_)) => anyhow::bail!(osdk_core::t!(
                    "err.lock_non_npm_graph_metadata",
                    backend = backend,
                    platform = platform
                )),
                (None, None) => {}
            }
        }
    }
    Ok(())
}

fn validate_native_lock(backend: &str, native_lock: &LockedNativeLock) -> Result<()> {
    let valid_format = match native_lock.kind {
        NpmInstaller::Aube => native_lock.format == "aube-v9",
        NpmInstaller::Npm => matches!(
            native_lock.format.as_str(),
            "package-lock-v2" | "package-lock-v3" | "npm-shrinkwrap-v2" | "npm-shrinkwrap-v3"
        ),
        NpmInstaller::Pnpm => native_lock.format == "pnpm-v9",
    };
    if !valid_format {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_native_format_unsupported",
            format = native_lock.format,
            backend = backend,
            owner = native_lock.kind.as_str()
        ));
    }
    validate_sha256(backend, &native_lock.sha256)
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
                (Some(_), Some(LockedNpmGraph::Metadata(_))) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_schema2_npm_schema3_metadata",
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
    validate_version_identity(backend, &locked.version)?;
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

fn validate_version_identity(backend: &str, value: &str) -> Result<()> {
    let version = value.trim();
    if version.is_empty()
        || version == "."
        || version == ".."
        || version.contains(['/', '\\'])
        || std::path::Path::new(version).is_absolute()
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_version_unsafe",
            version = value,
            backend = backend
        ));
    }
    Ok(())
}

fn validate_exact_node_version(backend: &str, value: &str) -> Result<()> {
    validate_version_identity(backend, value)?;
    if !matches!(VersionSpec::parse(value), VersionSpec::Exact(version) if version == value) {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_node_version_not_exact",
            backend = backend,
            version = value
        ));
    }
    Ok(())
}

pub fn merge_resolved(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    merge_resolved_with_scope(path, platform, dirs, resolved, LockScope::Project)
}

pub fn merge_resolved_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
    scope: LockScope,
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
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
    }
    let node_version = resolved
        .iter()
        .rev()
        .find_map(|(_, version)| (version.backend == "node").then(|| version.version.clone()));
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    platform_lock.tools.clear();
    for (request, version) in resolved {
        reject_linked_rust(dirs, version)?;
        let npm_metadata = locked_npm_metadata(dirs, version, node_version.as_deref(), scope)?;
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
                npm: npm_metadata.map(LockedNpmGraph::Metadata),
            },
        );
    }
    save(path, &lockfile)
}

#[allow(dead_code)]
pub fn upsert_resolved_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    request: &ToolRequest,
    version: &ToolVersion,
    scope: LockScope,
) -> Result<()> {
    upsert_resolved_many_with_scope(
        path,
        platform,
        dirs,
        std::slice::from_ref(&(request.clone(), version.clone())),
        scope,
    )
}

/// Upsert several exact tool identities and atomically save the lock once.
pub fn upsert_resolved_many_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
    scope: LockScope,
) -> Result<()> {
    for (request, version) in resolved {
        reject_linked_rust(dirs, version)?;
        if request.backend != version.backend {
            anyhow::bail!(
                "cannot lock request `{}` with resolved backend `{}`",
                request.backend,
                version.backend
            );
        }
    }
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
    }
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    let node_version = resolved
        .iter()
        .rev()
        .find_map(|(_, version)| (version.backend == "node").then(|| version.version.clone()))
        .or_else(|| {
            platform_lock
                .tools
                .get("node")
                .map(|node| node.version.clone())
        });
    for (request, version) in resolved {
        let effective_node = if version.backend == "node" {
            Some(version.version.as_str())
        } else {
            version
                .options
                .get(LOCKED_NPM_NODE_VERSION_OPTION)
                .map(String::as_str)
                .or(node_version.as_deref())
        };
        let npm_metadata = locked_npm_metadata(dirs, version, effective_node, scope)?;
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
                npm: npm_metadata.map(LockedNpmGraph::Metadata),
            },
        );
    }
    save(path, &lockfile)
}

/// Remove one tool entry for a platform while preserving every other tool,
/// platform, and model record. Empty platform tables are pruned.
#[allow(dead_code)]
pub fn remove_tool(path: &Path, platform: Platform, backend: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut lockfile = load(path)?;
    let key = platform_key(platform);
    let removed = lockfile
        .platforms
        .get_mut(&key)
        .is_some_and(|platform| platform.tools.remove(backend).is_some());
    if !removed {
        return Ok(false);
    }
    if lockfile
        .platforms
        .get(&key)
        .is_some_and(|platform| platform.tools.is_empty())
    {
        lockfile.platforms.remove(&key);
    }
    save(path, &lockfile)?;
    Ok(true)
}

fn reject_linked_rust(dirs: &osdk_core::dirs::Dirs, version: &ToolVersion) -> Result<()> {
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
    Ok(())
}

pub fn merge_model(path: &Path, manifest: &osdk_core::model::SnapshotManifest) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
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
        .filter(|(key, _)| !key.starts_with("__osdk_") && key.as_str() != "catalog-url")
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

fn reject_unmigratable_schema_one_npm_entries(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock.tools.iter().find(|(backend, locked)| {
            backend.starts_with("npm:") && matches!(locked.npm, Some(LockedNpmGraph::Legacy(_)))
        }) {
            anyhow::bail!(osdk_core::t!(
                "err.lock_schema1_npm_migration_requires_graph",
                backend = backend,
                platform = platform
            ));
        }
    }
    Ok(())
}

fn locked_npm_metadata(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
    node_version: Option<&str>,
    scope: LockScope,
) -> Result<Option<LockedNpmMetadata>> {
    let Some(package) = version.backend.strip_prefix("npm:") else {
        return Ok(None);
    };
    let node_version = node_version
        .or_else(|| {
            version
                .options
                .get(LOCKED_NPM_NODE_VERSION_OPTION)
                .map(String::as_str)
        })
        .map(str::to_owned);
    let installer = version
        .options
        .get(LOCKED_NPM_INSTALLER_OPTION)
        .map(|value| NpmInstaller::parse(value))
        .transpose()?
        .unwrap_or(NpmInstaller::Aube);
    let declared_native_lock = locked_native_lock_from_options(version)?;
    let lock_path = dirs
        .install_path(&version.backend, &version.version)
        .join(NPM_LOCKFILE_PATH);
    let native_lock = if let Some(native_lock) = declared_native_lock {
        Some(native_lock)
    } else if installer == NpmInstaller::Aube && lock_path.is_file() {
        let bytes = read_bounded(&lock_path, MAX_NPM_GRAPH_BYTES).with_context(|| {
            osdk_core::t!("err.npm_lock_payload_read", path = lock_path.display())
        })?;
        Some(LockedNativeLock {
            kind: installer,
            format: match installer {
                NpmInstaller::Aube => NPM_LOCK_FORMAT.into(),
                NpmInstaller::Npm => "package-lock-v3".into(),
                NpmInstaller::Pnpm => "pnpm-v9".into(),
            },
            sha256: osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256),
        })
    } else {
        None
    };
    Ok(Some(LockedNpmMetadata {
        package: package.to_string(),
        installer,
        scope,
        node_version,
        native_lock,
    }))
}

fn locked_native_lock_from_options(version: &ToolVersion) -> Result<Option<LockedNativeLock>> {
    let values = [
        version.options.get(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION),
        version.options.get(LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION),
        version.options.get(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION),
    ];
    if values.iter().all(|value| value.is_none()) {
        return Ok(None);
    }
    let required = |key: &str, value: Option<&String>| {
        value.cloned().ok_or_else(|| {
            anyhow::anyhow!(osdk_core::t!(
                "err.lock_npm_native_metadata_missing",
                key = key
            ))
        })
    };
    let native_lock = LockedNativeLock {
        kind: NpmInstaller::parse(&required(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION, values[0])?)?,
        format: required(LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION, values[1])?,
        sha256: required(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION, values[2])?,
    };
    validate_native_lock(&version.backend, &native_lock)?;
    Ok(Some(native_lock))
}

fn save(path: &Path, lockfile: &Lockfile) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| osdk_core::t!("err.fs_directory_create", path = parent.display()))?;
    }
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(lockfile)?;
    } else if lockfile.schema == 2 {
        validate_schema_two(path, lockfile, true)?;
    }
    let mut lockfile = lockfile.clone();
    migrate_npm_entries_to_schema_three(&mut lockfile)?;
    lockfile.schema = schema_version();
    validate_schema_three(&lockfile)?;
    let text = toml::to_string_pretty(&lockfile)?;
    if text.len() as u64 > MAX_LOCKFILE_BYTES {
        anyhow::bail!(osdk_core::t!(
            "err.lockfile_size_limit_exceeded",
            maximum = MAX_LOCKFILE_BYTES
        ));
    }
    atomic_write(path, text.as_bytes())?;
    Ok(())
}

fn migrate_npm_entries_to_schema_three(lockfile: &mut Lockfile) -> Result<()> {
    for platform_lock in lockfile.platforms.values_mut() {
        for (backend, locked) in &mut platform_lock.tools {
            let Some(package) = backend.strip_prefix("npm:") else {
                continue;
            };
            let metadata = match locked.npm.take() {
                Some(LockedNpmGraph::Metadata(metadata)) => metadata,
                Some(LockedNpmGraph::Sidecar(sidecar)) => LockedNpmMetadata {
                    package: sidecar.package,
                    installer: NpmInstaller::Aube,
                    scope: LockScope::Project,
                    node_version: Some(sidecar.node_version),
                    native_lock: Some(LockedNativeLock {
                        kind: NpmInstaller::Aube,
                        format: sidecar.lock_format,
                        sha256: sidecar.sha256,
                    }),
                },
                Some(LockedNpmGraph::Legacy(_)) | None => {
                    let installer = locked
                        .options
                        .get(LOCKED_NPM_INSTALLER_OPTION)
                        .map(|value| NpmInstaller::parse(value))
                        .transpose()?
                        .unwrap_or(NpmInstaller::Aube);
                    LockedNpmMetadata {
                        package: package.to_string(),
                        installer,
                        scope: LockScope::Project,
                        node_version: locked.options.get(LOCKED_NPM_NODE_VERSION_OPTION).cloned(),
                        native_lock: None,
                    }
                }
            };
            locked.options.retain(|key, _| {
                !key.starts_with("__osdk_npm_") && key != LOCKED_NPM_NODE_VERSION_OPTION
            });
            locked.npm = Some(LockedNpmGraph::Metadata(metadata));
            locked.artifact = None;
        }
    }
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
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing lockfile directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
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
    fn direct_save_migrates_schema_one_npm_to_metadata_only() {
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
        save(&path, &legacy).unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(lock.schema, 3);
        assert!(matches!(
            lock.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));
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
    fn schema_one_npm_without_inline_payload_is_readable_and_migrated() {
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

        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@24").unwrap(),
                ToolVersion::new("node", "24.1.0"),
            )],
        )
        .unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.schema, 3);
        assert!(matches!(
            migrated.platforms["windows-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));

        merge_model(&path, &test_model_manifest()).unwrap();
        assert!(load(&path).unwrap().models.contains_key("qwen"));
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
            .unwrap();
        let LockedNpmGraph::Metadata(npm) = npm else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.package, "@antfu/ni");
        assert_eq!(npm.installer, NpmInstaller::Aube);
        assert_eq!(npm.scope, LockScope::Project);
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
        assert_eq!(
            npm.native_lock,
            Some(LockedNativeLock {
                kind: NpmInstaller::Aube,
                format: NPM_LOCK_FORMAT.into(),
                sha256: expected_sha256.clone(),
            })
        );
        let main_lock = std::fs::read_to_string(&path).unwrap();
        assert!(!main_lock.contains("lockfile ="));
        assert!(!main_lock.contains("lock_sha256"));
        assert!(!main_lock.contains("graph ="));
        assert!(!main_lock.contains("# caf"));
        assert!(!temp.path().join(NPM_GRAPH_DIRECTORY).exists());

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let options = &requests
            .iter()
            .find(|request| request.backend == backend)
            .unwrap()
            .options;
        assert_eq!(options[LOCKED_NPM_PACKAGE_OPTION], "@antfu/ni");
        assert_eq!(options[LOCKED_NPM_INSTALLER_OPTION], "aube");
        assert_eq!(options[LOCKED_NPM_SCOPE_OPTION], "project");
        assert_eq!(options[LOCKED_NPM_NATIVE_LOCK_KIND_OPTION], "aube");
        assert_eq!(
            options[LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION],
            NPM_LOCK_FORMAT
        );
        assert_eq!(
            options[LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION],
            expected_sha256
        );
        assert_eq!(options[LOCKED_NPM_NODE_VERSION_OPTION], "24.1.0");
        assert!(!options.contains_key(LOCKED_NPM_LOCKFILE_OPTION));
        assert!(!options.contains_key(LOCKED_NPM_LOCK_FORMAT_OPTION));
        assert!(!options.contains_key(LOCKED_NPM_LOCK_SHA256_OPTION));
    }

    #[test]
    fn npm_lock_records_optional_node_and_ignores_stale_artifact_receipts() {
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
        merge_resolved(&path, linux(), &dirs, &npm_only).unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools[backend]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.node_version.is_none());

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
            .unwrap();
        let LockedNpmGraph::Metadata(npm) = npm else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
        assert_eq!(lock.platforms["linux-x64"].tools["node"].version, "24.1.0");
    }

    #[test]
    fn scoped_writer_records_global_scope_without_a_graph_reference() {
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
        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n").unwrap();
        merge_resolved_with_scope(&path, linux(), &dirs, &resolved, LockScope::Global).unwrap();

        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.scope, LockScope::Global);
        assert!(!std::fs::read_to_string(path).unwrap().contains("graph ="));
    }

    #[test]
    fn scoped_upsert_preserves_other_tools_platforms_and_models() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let mut initial = Lockfile::default();
        initial.platforms.insert(
            "windows-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "node".into(),
                    LockedTool {
                        request: "20".into(),
                        version: "20.19.0".into(),
                        ..LockedTool::default()
                    },
                )]),
            },
        );
        initial.platforms.insert(
            "linux-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "rust".into(),
                    LockedTool {
                        request: "stable".into(),
                        version: "1.98.0".into(),
                        ..LockedTool::default()
                    },
                )]),
            },
        );
        initial.models.insert("qwen".into(), test_locked_model());
        save(&path, &initial).unwrap();

        let request = ToolRequest::parse("npm:prettier@3.6.2").unwrap();
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "24.1.0".into());
        upsert_resolved_with_scope(&path, linux(), &dirs, &request, &version, LockScope::Global)
            .unwrap();

        let lock = load(&path).unwrap();
        assert!(lock.platforms["windows-x64"].tools.contains_key("node"));
        assert!(lock.platforms["linux-x64"].tools.contains_key("rust"));
        assert!(lock.models.contains_key("qwen"));
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.scope, LockScope::Global);
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
    }

    #[test]
    fn removing_one_tool_preserves_other_lock_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
                (
                    ToolRequest::parse("go@1.24.0").unwrap(),
                    ToolVersion::new("go", "1.24.0"),
                ),
            ],
        )
        .unwrap();
        merge_model(&path, &test_model_manifest()).unwrap();

        assert!(remove_tool(&path, linux(), "go").unwrap());
        assert!(!remove_tool(&path, linux(), "missing").unwrap());
        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools.contains_key("node"));
        assert!(!lock.platforms["linux-x64"].tools.contains_key("go"));
        assert!(lock.models.contains_key("qwen"));
    }

    #[test]
    fn schema_two_sidecar_remains_frozen_read_compatible_and_migrates_without_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let payload = b"lockfileVersion: '9.0'\n";
        let sha256 = osdk_core::pipeline::verify::hash_bytes(payload, HashAlgo::Sha256);
        let text = valid_schema_two_npm_lock_with_sha256(&sha256);
        std::fs::write(&path, text).unwrap();
        let sidecar = npm_graph_path(&path, &npm_graph_relative_path(&sha256));
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, payload).unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let options = &requests
            .iter()
            .find(|request| request.backend == "npm:prettier")
            .unwrap()
            .options;
        assert_eq!(options[LOCKED_NPM_LOCKFILE_OPTION].as_bytes(), payload);

        merge_model(&path, &test_model_manifest()).unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(lock.schema, 3);
        assert!(matches!(
            lock.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(!rewritten.contains("graph ="));
        assert!(sidecar.is_file());
    }

    #[test]
    fn schema_two_write_refuses_missing_sidecar_before_replacing_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let before = std::fs::read(&path).unwrap();

        let error = merge_model(&path, &test_model_manifest()).unwrap_err();
        assert!(format!("{error:#}").contains("reading npm graph sidecar"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn schema_three_rejects_payload_paths_and_non_exact_node_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let valid = valid_schema_three_npm_lock();

        std::fs::write(
            &path,
            valid.replace(
                "scope = \"project\"",
                "scope = \"project\"\ngraph = \"legacy.yaml\"",
            ),
        )
        .unwrap();
        assert!(format!("{:#}", load(&path).unwrap_err()).contains("did not match any variant"));

        std::fs::write(
            &path,
            valid.replace("node_version = \"24.1.0\"", "node_version = \"24\""),
        )
        .unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("non-exact node version"));
    }

    #[test]
    fn schema_three_native_lock_owner_can_differ_from_selected_installer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_three_npm_lock()
            .replace("kind = \"aube\"", "kind = \"pnpm\"")
            .replace("format = \"aube-v9\"", "format = \"pnpm-v9\"");
        std::fs::write(&path, text).unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 reads npm metadata");
        };
        assert_eq!(npm.installer, NpmInstaller::Aube);
        assert_eq!(npm.native_lock.as_ref().unwrap().kind, NpmInstaller::Pnpm);
    }

    #[test]
    fn schema_three_rejects_future_native_lock_formats() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        for (kind, format) in [
            ("aube", "aube-v10"),
            ("pnpm", "pnpm-v10"),
            ("npm", "package-lock-v4"),
            ("npm", "npm-shrinkwrap-v4"),
        ] {
            let text = valid_schema_three_npm_lock()
                .replace("kind = \"aube\"", &format!("kind = \"{kind}\""))
                .replace("format = \"aube-v9\"", &format!("format = \"{format}\""));
            std::fs::write(&path, text).unwrap();
            let error = load(&path).unwrap_err().to_string();
            assert!(
                error.contains(format),
                "unexpected error for {format}: {error}"
            );
        }
    }

    #[test]
    fn schema_three_errors_render_in_chinese() {
        let missing = osdk_core::i18n::interpolate(
            &osdk_core::i18n::trl(
                osdk_core::i18n::Lang::Zh,
                "err.lock_schema3_npm_metadata_missing",
            ),
            &[("backend", "npm:prettier"), ("platform", "linux-x64")],
        );
        assert_eq!(
            missing,
            "平台 `linux-x64` 上的 schema 3 npm 条目 `npm:prettier` 缺少 npm 元数据"
        );

        let format = osdk_core::i18n::interpolate(
            &osdk_core::i18n::trl(
                osdk_core::i18n::Lang::Zh,
                "err.lock_npm_native_format_unsupported",
            ),
            &[
                ("format", "future-v10"),
                ("backend", "npm:prettier"),
                ("owner", "pnpm"),
            ],
        );
        assert!(format.contains("原生 npm 锁文件格式 `future-v10` 不受支持"));
        assert!(format.contains("`npm:prettier`"));
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
    fn npm_merge_allows_metadata_without_an_installed_native_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        merge_resolved(
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
        .unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.native_lock.is_none());
    }

    #[test]
    fn npm_merge_hashes_non_utf8_native_lock_without_persisting_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, [0xff, 0xfe]).unwrap();

        merge_resolved(
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
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(char::REPLACEMENT_CHARACTER));
        assert!(!text.contains("lockfile ="));
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.native_lock.is_some());
    }

    #[test]
    fn npm_metadata_accepts_declared_pnpm_native_lock_without_aube_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_INSTALLER_OPTION.into(), "pnpm".into());
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(), "pnpm".into());
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            "pnpm-v9".into(),
        );
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(), "b".repeat(64));
        merge_resolved_with_scope(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(ToolRequest::parse("npm:prettier@3.6.2").unwrap(), version)],
            LockScope::Global,
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.installer, NpmInstaller::Pnpm);
        assert_eq!(npm.scope, LockScope::Global);
        assert_eq!(npm.native_lock.as_ref().unwrap().format, "pnpm-v9");
    }

    #[test]
    fn package_lock_kind_alias_is_normalized_to_npm_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_INSTALLER_OPTION.into(), "npm".into());
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            "package-lock".into(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            "package-lock-v2".into(),
        );
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(), "c".repeat(64));
        merge_resolved_with_scope(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(ToolRequest::parse("npm:prettier@3.6.2").unwrap(), version)],
            LockScope::Project,
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"npm\""));
        assert!(!text.contains("kind = \"package-lock\""));
        assert!(text.contains("format = \"package-lock-v2\""));
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
        valid_schema_two_npm_lock_with_sha256(&"a".repeat(64))
    }

    fn valid_schema_two_npm_lock_with_sha256(sha256: &str) -> String {
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

    fn valid_schema_three_npm_lock() -> String {
        let sha256 = "a".repeat(64);
        format!(
            r#"
schema = 3

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "aube"
scope = "project"
node_version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier".npm.native_lock]
kind = "aube"
format = "aube-v9"
sha256 = "{sha256}"
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

    fn test_locked_model() -> LockedModel {
        LockedModel {
            provider: osdk_core::model::ProviderId::HuggingFace,
            repository: "Qwen/Qwen2.5-7B-Instruct".into(),
            requested_revision: "main".into(),
            revision: "abc123".into(),
            endpoint: "https://huggingface.co".into(),
            variant: Some("safetensors".into()),
            files: vec![LockedModelFile {
                path: "config.json".into(),
                size: 10,
                sha256: "sha256".into(),
            }],
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
