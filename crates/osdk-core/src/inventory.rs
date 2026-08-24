//! Persisted inventory for dynamic tool installs.
//!
//! Each dynamic install root may contain a `.osdk-tool.json` manifest describing
//! the canonical tool id, owned bins, and arbitrary metadata. This module
//! provides strict serde-backed types plus safe helpers to persist, scan, and
//! reason about those manifests without following symlinks.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::version::ToolRequest;

/// Name of the per-install dynamic tool inventory file.
pub const INVENTORY_FILE: &str = ".osdk-tool.json";

const INVENTORY_SCHEMA: u32 = 1;
const DEFAULT_MAX_DEPTH: usize = 8;
const DEFAULT_MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_MANIFESTS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicToolBin {
    /// Exported executable name, e.g. `prettier` or `gh.exe`.
    pub name: String,
    /// Relative path from the install root to the executable.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicToolManifest {
    #[serde(default = "inventory_schema")]
    pub schema: u32,
    /// Canonical dynamic backend id such as `npm:@antfu/ni` or
    /// `github:cli/cli`.
    pub id: String,
    /// Optional resolved version for the install stored at this root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Config keys that may refer to this dynamic tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_keys: Vec<String>,
    /// Binaries exported by this install.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<DynamicToolBin>,
    /// Arbitrary stable metadata for callers that need to annotate installs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDynamicTool {
    pub canonical_id: String,
    pub install_root: PathBuf,
    pub manifest: DynamicToolManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptManifestPolicy {
    /// Refuse the entire scan when any manifest is unreadable or invalid.
    FailClosed,
    /// Skip invalid manifests and report diagnostics.
    CollectDiagnostics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Maximum install-root depth below the scan root.
    pub max_depth: usize,
    /// Maximum accepted manifest size in bytes.
    pub max_manifest_bytes: u64,
    /// Maximum number of manifests to inspect in one scan.
    pub max_manifests: usize,
    /// How to handle corrupt or unreadable manifests.
    pub corrupt_manifest_policy: CorruptManifestPolicy,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            max_manifests: DEFAULT_MAX_MANIFESTS,
            corrupt_manifest_policy: CorruptManifestPolicy::FailClosed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryDiagnosticKind {
    Walk,
    Io,
    ManifestTooLarge,
    InvalidManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryDiagnostic {
    pub path: PathBuf,
    pub kind: InventoryDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub installs: Vec<InstalledDynamicTool>,
    pub diagnostics: Vec<InventoryDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinOwnerCandidate {
    pub bin_name: String,
    pub canonical_id: String,
    pub install_root: PathBuf,
    pub relative_path: String,
}

impl BinOwnerCandidate {
    pub fn absolute_path(&self) -> PathBuf {
        self.install_root.join(&self.relative_path)
    }
}

impl DynamicToolManifest {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        Self {
            schema: inventory_schema(),
            id: id.into(),
            version: None,
            config_keys: Vec::new(),
            bins: Vec::new(),
            metadata: BTreeMap::new(),
        }
        .normalize()
    }

    pub fn manifest_path(install_root: &Path) -> PathBuf {
        install_root.join(INVENTORY_FILE)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.normalize()
    }

    pub fn load(install_root: &Path) -> Result<Self> {
        let path = Self::manifest_path(install_root);
        let bytes = std::fs::read(&path).map_err(|error| Error::io(&path, error))?;
        Self::from_slice(&bytes).map_err(|error| {
            Error::other(format!(
                "invalid tool inventory at {}: {error}",
                path.display()
            ))
        })
    }

    pub fn write_atomic(&self, install_root: &Path) -> Result<()> {
        let path = Self::manifest_path(install_root);
        let normalized = self.clone().normalize()?;
        atomic_write_json(&path, &normalized)
    }

    pub fn normalize(mut self) -> Result<Self> {
        if self.schema != inventory_schema() {
            return Err(Error::config(format!(
                "unsupported dynamic tool inventory schema `{}`",
                self.schema
            )));
        }
        self.id = canonical_dynamic_id(&self.id)?;
        if let Some(version) = &self.version {
            if version.trim().is_empty() {
                return Err(Error::config("dynamic tool version must not be empty"));
            }
        }
        normalize_config_keys(&mut self.config_keys)?;
        normalize_bins(&mut self.bins)?;
        normalize_metadata(&mut self.metadata)?;
        Ok(self)
    }

    pub fn with_metadata_mutation(
        mut self,
        mutate: impl FnOnce(&mut BTreeMap<String, String>),
    ) -> Result<Self> {
        mutate(&mut self.metadata);
        self.normalize()
    }
}

impl ScanReport {
    pub fn installed_ids(&self) -> Vec<String> {
        deduped_ids(
            self.installs
                .iter()
                .map(|install| install.canonical_id.as_str()),
        )
    }
}

pub fn update_manifest_metadata(
    install_root: &Path,
    mutate: impl FnOnce(&mut BTreeMap<String, String>),
) -> Result<DynamicToolManifest> {
    let manifest = DynamicToolManifest::load(install_root)?.with_metadata_mutation(mutate)?;
    manifest.write_atomic(install_root)?;
    Ok(manifest)
}

pub fn remove_manifest_metadata(install_root: &Path, key: &str) -> Result<DynamicToolManifest> {
    update_manifest_metadata(install_root, |metadata| {
        metadata.remove(key);
    })
}

pub fn remove_manifest(install_root: &Path) -> Result<bool> {
    let path = DynamicToolManifest::manifest_path(install_root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io(path, error)),
    }
}

pub fn canonical_dynamic_id(value: &str) -> Result<String> {
    let value = value.trim();
    let (backend, body) = value
        .split_once(':')
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{value}`")))?;
    let backend = canonical_backend_name(backend)?;
    let body = match backend.as_str() {
        "npm" => canonical_npm_package(body)?,
        "github" => canonical_github_repository(body)?,
        _ => canonical_generic_namespaced_body(body)?,
    };
    Ok(format!("{backend}:{body}"))
}

pub fn configured_dynamic_ids<'a>(
    configured_values: impl IntoIterator<Item = (&'a str, &'a str)>,
    config_keys: &[&str],
) -> Vec<String> {
    let relevant_keys: BTreeSet<&str> = config_keys.iter().copied().collect();
    deduped_ids(
        configured_values
            .into_iter()
            .filter_map(|(key, value)| relevant_keys.contains(key).then_some(value)),
    )
}

pub fn configured_and_installed_dynamic_ids<'a>(
    report: &ScanReport,
    configured_values: impl IntoIterator<Item = (&'a str, &'a str)>,
    config_keys: &[&str],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(configured_dynamic_ids(configured_values, config_keys));
    ids.extend(report.installed_ids());
    ids.into_iter().collect()
}

pub fn build_bin_ownership_candidates(
    installs: &[InstalledDynamicTool],
) -> BTreeMap<String, Vec<BinOwnerCandidate>> {
    let mut owners: BTreeMap<String, Vec<BinOwnerCandidate>> = BTreeMap::new();
    for install in installs {
        for bin in &install.manifest.bins {
            owners
                .entry(bin.name.clone())
                .or_default()
                .push(BinOwnerCandidate {
                    bin_name: bin.name.clone(),
                    canonical_id: install.canonical_id.clone(),
                    install_root: install.install_root.clone(),
                    relative_path: bin.path.clone(),
                });
        }
    }
    for candidates in owners.values_mut() {
        candidates.sort_by(|left, right| {
            (
                left.canonical_id.as_str(),
                left.relative_path.as_str(),
                path_sort_key(&left.install_root),
            )
                .cmp(&(
                    right.canonical_id.as_str(),
                    right.relative_path.as_str(),
                    path_sort_key(&right.install_root),
                ))
        });
        candidates.dedup_by(|left, right| {
            left.canonical_id == right.canonical_id
                && left.relative_path == right.relative_path
                && left.install_root == right.install_root
        });
    }
    owners
}

pub fn scan_installs(scan_root: &Path, options: &ScanOptions) -> Result<ScanReport> {
    if !scan_root.exists() {
        return Ok(ScanReport::default());
    }

    let mut manifest_paths = Vec::new();
    let mut diagnostics = Vec::new();
    let walker = walkdir::WalkDir::new(scan_root)
        .follow_links(false)
        .max_depth(options.max_depth.saturating_add(1));

    for entry in walker {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                    continue;
                }
                if entry.file_name() != INVENTORY_FILE {
                    continue;
                }
                manifest_paths.push(entry.into_path());
                if manifest_paths.len() > options.max_manifests {
                    return Err(Error::other(format!(
                        "dynamic tool inventory scan exceeded manifest limit of {} under {}",
                        options.max_manifests,
                        scan_root.display()
                    )));
                }
            }
            Err(error) => handle_scan_problem(
                &mut diagnostics,
                options,
                error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| scan_root.to_path_buf()),
                InventoryDiagnosticKind::Walk,
                error.to_string(),
            )?,
        }
    }

    manifest_paths.sort_by_key(|path| path_sort_key(path));

    let mut installs = Vec::new();
    for manifest_path in manifest_paths {
        let metadata =
            std::fs::metadata(&manifest_path).map_err(|error| Error::io(&manifest_path, error))?;
        if metadata.len() > options.max_manifest_bytes {
            handle_scan_problem(
                &mut diagnostics,
                options,
                manifest_path.clone(),
                InventoryDiagnosticKind::ManifestTooLarge,
                format!(
                    "manifest is {} bytes, larger than the {} byte limit",
                    metadata.len(),
                    options.max_manifest_bytes
                ),
            )?;
            continue;
        }

        let bytes = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    manifest_path.clone(),
                    InventoryDiagnosticKind::Io,
                    error.to_string(),
                )?;
                continue;
            }
        };
        if bytes.len() as u64 > options.max_manifest_bytes {
            handle_scan_problem(
                &mut diagnostics,
                options,
                manifest_path.clone(),
                InventoryDiagnosticKind::ManifestTooLarge,
                format!(
                    "manifest expanded to {} bytes, larger than the {} byte limit",
                    bytes.len(),
                    options.max_manifest_bytes
                ),
            )?;
            continue;
        }

        let manifest = match DynamicToolManifest::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    manifest_path.clone(),
                    InventoryDiagnosticKind::InvalidManifest,
                    error.to_string(),
                )?;
                continue;
            }
        };
        let install_root = manifest_path
            .parent()
            .ok_or_else(|| {
                Error::other(format!(
                    "manifest path has no parent: {}",
                    manifest_path.display()
                ))
            })?
            .to_path_buf();
        installs.push(InstalledDynamicTool {
            canonical_id: manifest.id.clone(),
            install_root,
            manifest,
        });
    }

    installs.sort_by(|left, right| {
        (
            left.canonical_id.as_str(),
            path_sort_key(&left.install_root),
            left.manifest.version.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.canonical_id.as_str(),
                path_sort_key(&right.install_root),
                right.manifest.version.as_deref().unwrap_or(""),
            ))
    });

    Ok(ScanReport {
        installs,
        diagnostics,
    })
}

fn inventory_schema() -> u32 {
    INVENTORY_SCHEMA
}

fn normalize_config_keys(keys: &mut Vec<String>) -> Result<()> {
    for key in keys.iter_mut() {
        *key = key.trim().to_string();
        if key.is_empty() {
            return Err(Error::config("dynamic tool config keys must not be empty"));
        }
    }
    keys.sort();
    keys.dedup();
    Ok(())
}

fn normalize_bins(bins: &mut [DynamicToolBin]) -> Result<()> {
    for bin in bins.iter_mut() {
        bin.name = normalize_bin_name(&bin.name)?;
        bin.path = normalize_relative_bin_path(&bin.path)?;
    }
    bins.sort_by(|left, right| {
        (left.name.as_str(), left.path.as_str()).cmp(&(right.name.as_str(), right.path.as_str()))
    });
    for pair in bins.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(Error::config(format!(
                "duplicate dynamic tool bin `{}`",
                pair[0].name
            )));
        }
    }
    Ok(())
}

fn normalize_metadata(metadata: &mut BTreeMap<String, String>) -> Result<()> {
    let mut normalized = BTreeMap::new();
    for (key, value) in std::mem::take(metadata) {
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(Error::config(
                "dynamic tool metadata keys must not be empty",
            ));
        }
        normalized.insert(key, value);
    }
    *metadata = normalized;
    Ok(())
}

fn normalize_bin_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err(Error::config(format!(
            "dynamic tool bin name must be a single filename: `{value}`"
        )));
    }
    Ok(value.to_string())
}

fn normalize_relative_bin_path(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::config("dynamic tool bin path must not be empty"));
    }
    if value.contains(':') {
        return Err(Error::config(format!(
            "dynamic tool bin path must stay inside the install root: `{value}`"
        )));
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(Error::config(format!(
            "dynamic tool bin path must stay inside the install root: `{value}`"
        )));
    }

    let mut parts = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Error::config(format!(
                "dynamic tool bin path must stay inside the install root: `{value}`"
            )));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn canonical_backend_name(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    {
        return Err(Error::config(format!(
            "invalid dynamic backend namespace `{value}`"
        )));
    }
    Ok(value)
}

fn canonical_npm_package(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if let Some(rest) = normalized.strip_prefix('@') {
        let (scope, name) = rest
            .split_once('/')
            .ok_or_else(|| Error::config(format!("invalid npm package id `{normalized}`")))?;
        if !valid_npm_segment(scope) || !valid_npm_segment(name) || name.contains('/') {
            return Err(Error::config(format!(
                "invalid npm package id `{normalized}`"
            )));
        }
        return Ok(format!("@{scope}/{name}"));
    }
    if !valid_npm_segment(&normalized) {
        return Err(Error::config(format!(
            "invalid npm package id `{normalized}`"
        )));
    }
    Ok(normalized)
}

fn valid_npm_segment(value: &str) -> bool {
    if value.is_empty() || value.len() > 214 {
        return false;
    }
    if value == "." || value == ".." || is_windows_reserved_component(value) {
        return false;
    }
    value.chars().all(valid_npm_segment_char)
}

fn valid_npm_segment_char(ch: char) -> bool {
    ch.is_ascii_lowercase()
        || ch.is_ascii_uppercase()
        || ch.is_ascii_digit()
        || matches!(ch, '-' | '_' | '.')
}

fn is_windows_reserved_component(value: &str) -> bool {
    let trimmed = value.trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn canonical_github_repository(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches(".git").to_ascii_lowercase();
    let (owner, repo) = value
        .split_once('/')
        .ok_or_else(|| Error::config(format!("invalid GitHub repository id `{value}`")))?;
    if !valid_repository_component(owner) || !valid_repository_component(repo) || repo.contains('/')
    {
        return Err(Error::config(format!(
            "invalid GitHub repository id `{value}`"
        )));
    }
    Ok(format!("{owner}/{repo}"))
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains(':')
        && !value.contains('?')
        && !value.contains('#')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn canonical_generic_namespaced_body(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::config("dynamic tool id body must not be empty"));
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(Error::config(format!(
            "dynamic tool id must stay relative and slash-separated: `{value}`"
        )));
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_whitespace) {
            return Err(Error::config(format!(
                "dynamic tool id must stay relative and slash-separated: `{value}`"
            )));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn deduped_ids<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(extract_dynamic_id_from_value)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_dynamic_id_from_value(value: &str) -> Option<String> {
    let request = ToolRequest::parse(value).ok()?;
    request
        .backend
        .contains(':')
        .then_some(request.backend)
        .and_then(|backend| canonical_dynamic_id(&backend).ok())
}

fn handle_scan_problem(
    diagnostics: &mut Vec<InventoryDiagnostic>,
    options: &ScanOptions,
    path: PathBuf,
    kind: InventoryDiagnosticKind,
    message: String,
) -> Result<()> {
    match options.corrupt_manifest_policy {
        CorruptManifestPolicy::FailClosed => Err(Error::other(format!(
            "refusing dynamic tool inventory scan because {} at {}",
            message,
            path.display()
        ))),
        CorruptManifestPolicy::CollectDiagnostics => {
            diagnostics.push(InventoryDiagnostic {
                path,
                kind,
                message,
            });
            Ok(())
        }
    }
}

fn path_sort_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::other(format!("path has no parent: {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;

    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = unique_temporary_path(parent, path.file_name().unwrap_or_default());
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        file.write_all(&bytes)
            .map_err(|error| Error::io(&temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io(&temporary, error))?;
    }
    if let Err(error) = atomic_replace(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn unique_temporary_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let file_name = file_name.to_string_lossy();
    for attempt in 0..1024u32 {
        let candidate = parent.join(format!(
            ".{file_name}.tmp-{}-{}",
            std::process::id(),
            attempt
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!(".{file_name}.tmp-{}-fallback", std::process::id()))
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|error| Error::io(destination, error))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), flags) };
    if result == 0 {
        return Err(Error::io(destination, std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[test]
    fn normalizes_supported_dynamic_ids_and_windows_bin_paths() {
        let mut manifest = DynamicToolManifest::new("npm:@Antfu/Ni").unwrap();
        manifest.version = Some("1.2.3".into());
        manifest.config_keys = vec![" dynamic.tool ".into(), "dynamic.tool".into()];
        manifest.bins = vec![DynamicToolBin {
            name: "ni.exe".into(),
            path: r"bin\ni.exe".into(),
        }];
        manifest.metadata.insert(" owner ".into(), "npm".into());

        let manifest = manifest.normalize().unwrap();
        assert_eq!(manifest.id, "npm:@antfu/ni");
        assert_eq!(manifest.config_keys, vec!["dynamic.tool"]);
        assert_eq!(
            manifest.bins,
            vec![DynamicToolBin {
                name: "ni.exe".into(),
                path: "bin/ni.exe".into()
            }]
        );
        assert_eq!(manifest.metadata.get("owner").unwrap(), "npm");

        assert_eq!(
            canonical_dynamic_id("github:Cli/CLI.git").unwrap(),
            "github:cli/cli"
        );
        assert_eq!(
            canonical_dynamic_id("npm:Prettier").unwrap(),
            "npm:prettier"
        );
    }

    #[test]
    fn rejects_traversal_and_duplicate_bins() {
        let mut manifest = DynamicToolManifest::new("github:cli/cli").unwrap();
        manifest.bins = vec![
            DynamicToolBin {
                name: "gh".into(),
                path: "../gh".into(),
            },
            DynamicToolBin {
                name: "gh".into(),
                path: "bin/gh".into(),
            },
        ];
        let error = manifest.normalize().unwrap_err();
        assert!(error.to_string().contains("install root"));

        let mut duplicate = DynamicToolManifest::new("github:cli/cli").unwrap();
        duplicate.bins = vec![
            DynamicToolBin {
                name: "gh".into(),
                path: "bin/gh".into(),
            },
            DynamicToolBin {
                name: "gh".into(),
                path: "other/gh".into(),
            },
        ];
        let error = duplicate.normalize().unwrap_err();
        assert!(error.to_string().contains("duplicate dynamic tool bin"));
    }

    #[test]
    fn rejects_invalid_npm_package_segments_per_current_policy() {
        for invalid in [
            "npm:foo#bar",
            "npm:foo?bar",
            "npm:foo%2fbar",
            "npm:.",
            "npm:..",
            "npm:CON",
            "npm:prn",
            "npm:Com1",
            "npm:@scope/AUX",
            "npm:@scope/Lpt9",
            &format!("npm:{}", "a".repeat(215)),
            &format!("npm:@scope/{}", "b".repeat(215)),
        ] {
            let error = canonical_dynamic_id(invalid).unwrap_err();
            assert!(
                error.to_string().contains("invalid npm package id"),
                "{invalid}"
            );
        }
    }

    #[test]
    fn writes_loads_updates_and_removes_manifest() {
        let temporary = tempdir().unwrap();
        let install_root = temporary.path().join("tool");
        let mut manifest = DynamicToolManifest::new("github:cli/cli").unwrap();
        manifest.version = Some("2.62.0".into());
        manifest.bins.push(DynamicToolBin {
            name: "gh".into(),
            path: "bin/gh".into(),
        });
        manifest.metadata.insert("channel".into(), "stable".into());

        manifest.write_atomic(&install_root).unwrap();
        let loaded = DynamicToolManifest::load(&install_root).unwrap();
        assert_eq!(loaded.id, "github:cli/cli");
        assert_eq!(loaded.version.as_deref(), Some("2.62.0"));

        let updated = update_manifest_metadata(&install_root, |metadata| {
            metadata.insert("source".into(), "github".into());
        })
        .unwrap();
        assert_eq!(updated.metadata.get("source").unwrap(), "github");

        let updated = remove_manifest_metadata(&install_root, "channel").unwrap();
        assert!(!updated.metadata.contains_key("channel"));

        assert!(remove_manifest(&install_root).unwrap());
        assert!(!remove_manifest(&install_root).unwrap());
    }

    #[test]
    fn scan_fail_closed_or_collects_diagnostics_for_corrupt_manifests() {
        let temporary = tempdir().unwrap();
        let good_root = temporary.path().join("good");
        let bad_root = temporary.path().join("bad");

        let mut good = DynamicToolManifest::new("npm:@antfu/ni").unwrap();
        good.bins.push(DynamicToolBin {
            name: "ni".into(),
            path: "bin/ni".into(),
        });
        good.write_atomic(&good_root).unwrap();

        std::fs::create_dir_all(&bad_root).unwrap();
        std::fs::write(
            DynamicToolManifest::manifest_path(&bad_root),
            br#"{"schema":1,"id":"github:cli/cli","bins":[{"name":"gh","path":"../gh"}]}"#,
        )
        .unwrap();

        let error = scan_installs(temporary.path(), &ScanOptions::default()).unwrap_err();
        assert!(error
            .to_string()
            .contains("refusing dynamic tool inventory scan"));

        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].canonical_id, "npm:@antfu/ni");
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].kind,
            InventoryDiagnosticKind::InvalidManifest
        );
    }

    #[test]
    fn scan_respects_depth_and_size_bounds() {
        let temporary = tempdir().unwrap();
        let deep_root = temporary.path().join("a").join("b").join("c");

        let mut manifest = DynamicToolManifest::new("github:sharkdp/fd").unwrap();
        manifest.metadata.insert("note".into(), "ok".into());
        manifest.write_atomic(&deep_root).unwrap();

        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                max_depth: 2,
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert!(report.installs.is_empty());

        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                max_depth: 3,
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.installs.len(), 1);

        let oversized_root = temporary.path().join("oversized");
        std::fs::create_dir_all(&oversized_root).unwrap();
        let payload = format!(
            "{{\"schema\":1,\"id\":\"github:cli/cli\",\"metadata\":{{\"blob\":\"{}\"}}}}",
            "x".repeat(2048)
        );
        std::fs::write(DynamicToolManifest::manifest_path(&oversized_root), payload).unwrap();
        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                max_depth: 3,
                max_manifest_bytes: 256,
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].kind,
            InventoryDiagnosticKind::ManifestTooLarge
        );
    }

    #[test]
    fn collects_configured_and_installed_dynamic_ids_deterministically() {
        let report = ScanReport {
            installs: vec![
                InstalledDynamicTool {
                    canonical_id: "github:cli/cli".into(),
                    install_root: PathBuf::from("/tmp/cli"),
                    manifest: DynamicToolManifest::new("github:cli/cli").unwrap(),
                },
                InstalledDynamicTool {
                    canonical_id: "github:cli/cli".into(),
                    install_root: PathBuf::from("/tmp/cli-alt"),
                    manifest: DynamicToolManifest::new("github:cli/cli").unwrap(),
                },
                InstalledDynamicTool {
                    canonical_id: "npm:@antfu/ni".into(),
                    install_root: PathBuf::from("/tmp/ni"),
                    manifest: DynamicToolManifest::new("npm:@antfu/ni").unwrap(),
                },
            ],
            diagnostics: Vec::new(),
        };

        let ids = configured_and_installed_dynamic_ids(
            &report,
            [
                ("dynamic.primary", "github:cli/cli@2.62.0"),
                ("dynamic.secondary", "npm:@Antfu/Ni@1"),
                ("ignored", "node"),
            ],
            &["dynamic.primary", "dynamic.secondary"],
        );

        assert_eq!(ids, vec!["github:cli/cli", "npm:@antfu/ni"]);
    }

    #[test]
    fn bin_candidates_are_deterministic_and_preserve_conflicts() {
        let mut github = DynamicToolManifest::new("github:cli/cli").unwrap();
        github.bins = vec![
            DynamicToolBin {
                name: "gh".into(),
                path: "bin/gh".into(),
            },
            DynamicToolBin {
                name: "repo-tool".into(),
                path: "bin/repo-tool".into(),
            },
        ];
        github = github.normalize().unwrap();

        let mut npm = DynamicToolManifest::new("npm:@antfu/ni").unwrap();
        npm.bins = vec![DynamicToolBin {
            name: "gh".into(),
            path: "node_modules/.bin/gh".into(),
        }];
        npm = npm.normalize().unwrap();

        let owners = build_bin_ownership_candidates(&[
            InstalledDynamicTool {
                canonical_id: github.id.clone(),
                install_root: PathBuf::from("/tmp/gh"),
                manifest: github,
            },
            InstalledDynamicTool {
                canonical_id: npm.id.clone(),
                install_root: PathBuf::from("/tmp/ni"),
                manifest: npm,
            },
        ]);

        let gh = owners.get("gh").unwrap();
        assert_eq!(gh.len(), 2);
        assert_eq!(gh[0].canonical_id, "github:cli/cli");
        assert_eq!(gh[1].canonical_id, "npm:@antfu/ni");
        assert_eq!(
            gh[1].absolute_path(),
            PathBuf::from("/tmp/ni").join("node_modules/.bin/gh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_symlinks_or_escape_the_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let outside = tempdir().unwrap();

        let mut outside_manifest = DynamicToolManifest::new("github:outside/tool").unwrap();
        outside_manifest.bins.push(DynamicToolBin {
            name: "outside".into(),
            path: "bin/outside".into(),
        });
        outside_manifest.write_atomic(outside.path()).unwrap();

        let linked_dir = temporary.path().join("linked-dir");
        symlink(outside.path(), &linked_dir).unwrap();

        let linked_manifest = temporary.path().join(INVENTORY_FILE);
        symlink(
            DynamicToolManifest::manifest_path(outside.path()),
            &linked_manifest,
        )
        .unwrap();

        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();

        assert!(report.installs.is_empty());
        assert!(report.diagnostics.is_empty());
    }
}
