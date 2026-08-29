//! Persisted inventory for dynamic tool installs.
//!
//! Each dynamic install root may contain a `.osdk-install.json` manifest describing
//! its complete install identity and owned bins. This module
//! provides strict serde-backed types plus safe helpers to persist, scan, and
//! reason about those manifests without following symlinks.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::tool::InstallIdentity;
use crate::version::ToolRequest;

/// Name of the per-install dynamic tool inventory file.
pub const INVENTORY_FILE: &str = ".osdk-install.json";
/// Previous inventory filename. It is detected for migration/reporting only.
pub const LEGACY_INVENTORY_FILE: &str = ".osdk-tool.json";

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
    pub schema: u32,
    pub identity: InstallIdentity,
    /// Binaries exported by this install.
    #[serde(default)]
    pub bins: Vec<DynamicToolBin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDynamicTool {
    pub canonical_id: String,
    pub install_root: PathBuf,
    pub manifest: DynamicToolManifest,
    root_identity: FileIdentity,
    manifest_identity: FileIdentity,
}

impl InstalledDynamicTool {
    /// Revalidate that the install root and manifest path still name the same
    /// filesystem objects accepted by the scanner. Consumers must call this
    /// immediately before trusting bins or execution metadata from the record.
    pub fn revalidate(&self) -> Result<()> {
        validate_regular_directory_path(&self.install_root)
            .map_err(|error| Error::io(&self.install_root, error))?;
        let current_root = FileIdentity::from_path(&self.install_root, FileKind::Directory)
            .map_err(|error| Error::io(&self.install_root, error))?;
        if current_root != self.root_identity {
            return Err(Error::other(format!(
                "dynamic install root changed after inventory scan: {}",
                self.install_root.display()
            )));
        }
        let manifest_path = DynamicToolManifest::manifest_path(&self.install_root);
        let current_manifest = FileIdentity::from_path(&manifest_path, FileKind::File)
            .map_err(|error| Error::io(&manifest_path, error))?;
        if current_manifest != self.manifest_identity {
            return Err(Error::other(format!(
                "dynamic install manifest changed after inventory scan: {}",
                manifest_path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    canonical_path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, Copy)]
enum FileKind {
    Directory,
    File,
}

impl FileIdentity {
    fn from_path(path: &Path, kind: FileKind) -> std::io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        let valid_kind = match kind {
            FileKind::Directory => metadata.is_dir(),
            FileKind::File => metadata.is_file(),
        };
        if metadata.file_type().is_symlink() || !valid_kind {
            return Err(std::io::Error::other(format!(
                "inventory path is not a regular non-symlink {kind:?}: {}",
                path.display()
            )));
        }
        Ok(Self {
            canonical_path: dunce::canonicalize(path)?,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt as _;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt as _;
                metadata.ino()
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDynamicInstall {
    pub install_root: PathBuf,
    pub manifest_path: PathBuf,
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
    pub legacy_installs: Vec<LegacyDynamicInstall>,
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
    pub fn from_identity(identity: InstallIdentity) -> Result<Self> {
        Self {
            schema: INVENTORY_SCHEMA,
            identity,
            bins: Vec::new(),
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
        validate_regular_directory_path(install_root)
            .map_err(|error| Error::io(install_root, error))?;
        let bytes = read_stable_regular_file(&path, DEFAULT_MAX_MANIFEST_BYTES)
            .map_err(|error| Error::io(&path, error))?;
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
        if self.schema != INVENTORY_SCHEMA {
            return Err(Error::config(format!(
                "unsupported dynamic install inventory schema `{}`",
                self.schema
            )));
        }
        self.identity.validate()?;
        normalize_bins(&mut self.bins)?;
        Ok(self)
    }

    pub fn matches_identity(&self, identity: &InstallIdentity) -> bool {
        &self.identity == identity
    }
}

impl ScanReport {
    pub fn installed_ids(&self) -> Vec<String> {
        deduped_ids(
            self.installs
                .iter()
                .map(|install| install.manifest.identity.tool.as_str()),
        )
    }
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
    crate::tool::canonical_dynamic_id(value)
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
        // A report is a snapshot. Refuse entries whose root or manifest was
        // replaced after scanning instead of exporting stale ownership data.
        if install.revalidate().is_err() {
            continue;
        }
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
    match std::fs::symlink_metadata(scan_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScanReport::default());
        }
        Err(error) => return Err(Error::io(scan_root, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(Error::other(format!(
                "dynamic tool inventory root must be a non-symlink directory: {}",
                scan_root.display()
            )));
        }
        Ok(_) => {}
    }
    validate_regular_directory_path(scan_root).map_err(|error| Error::io(scan_root, error))?;
    let canonical_scan_root =
        dunce::canonicalize(scan_root).map_err(|error| Error::io(scan_root, error))?;
    let absolute_scan_root = if scan_root.is_absolute() {
        scan_root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| Error::io(scan_root, error))?
            .join(scan_root)
    };
    if canonical_scan_root != absolute_scan_root {
        return Err(Error::other(format!(
            "dynamic tool inventory root must be canonical: expected {}, found {}",
            canonical_scan_root.display(),
            scan_root.display()
        )));
    }

    let mut manifest_paths = Vec::new();
    let mut legacy_installs = Vec::new();
    let mut diagnostics = Vec::new();
    let walker = walkdir::WalkDir::new(scan_root)
        .follow_links(false)
        .max_depth(options.max_depth.saturating_add(1))
        .into_iter()
        // Hidden directories below the install root are implementation state
        // (`.locks`, atomic staging/backup trees, and similar). The inventory
        // file itself is intentionally hidden, so filter directories only.
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !entry.file_name().to_string_lossy().starts_with('.')
        });

    for entry in walker {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                    continue;
                }
                if entry.file_name() == LEGACY_INVENTORY_FILE {
                    let manifest_path = entry.into_path();
                    if let Some(install_root) = manifest_path.parent() {
                        legacy_installs.push(LegacyDynamicInstall {
                            install_root: install_root.to_path_buf(),
                            manifest_path,
                        });
                    }
                    if manifest_paths.len() + legacy_installs.len() > options.max_manifests {
                        return Err(Error::other(format!(
                            "dynamic tool inventory scan exceeded manifest limit of {} under {}",
                            options.max_manifests,
                            scan_root.display()
                        )));
                    }
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
        let install_root = manifest_path
            .parent()
            .ok_or_else(|| {
                Error::other(format!(
                    "manifest path has no parent: {}",
                    manifest_path.display()
                ))
            })?
            .to_path_buf();
        if let Err(error) = validate_regular_directory_path_from(scan_root, &install_root) {
            handle_scan_problem(
                &mut diagnostics,
                options,
                install_root.clone(),
                InventoryDiagnosticKind::Io,
                error.to_string(),
            )?;
            continue;
        }
        let canonical_install_root = match dunce::canonicalize(&install_root) {
            Ok(root) if root.starts_with(&canonical_scan_root) => root,
            Ok(root) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    install_root.clone(),
                    InventoryDiagnosticKind::InvalidManifest,
                    format!(
                        "dynamic install root resolves outside {} to {}",
                        canonical_scan_root.display(),
                        root.display()
                    ),
                )?;
                continue;
            }
            Err(error) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    install_root.clone(),
                    InventoryDiagnosticKind::Io,
                    error.to_string(),
                )?;
                continue;
            }
        };
        let absolute_install_root = if install_root.is_absolute() {
            install_root.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| Error::io(&install_root, error))?
                .join(&install_root)
        };
        if canonical_install_root != absolute_install_root {
            handle_scan_problem(
                &mut diagnostics,
                options,
                install_root.clone(),
                InventoryDiagnosticKind::InvalidManifest,
                format!(
                    "dynamic install root is not canonical: expected {}",
                    canonical_install_root.display()
                ),
            )?;
            continue;
        }

        let bytes = match read_stable_regular_file(&manifest_path, options.max_manifest_bytes) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    manifest_path.clone(),
                    InventoryDiagnosticKind::ManifestTooLarge,
                    format!(
                        "manifest is larger than the {} byte limit",
                        options.max_manifest_bytes
                    ),
                )?;
                continue;
            }
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
        let canonical_root = crate::dirs::InstallLocator::is_canonical_install_root(
            scan_root,
            &manifest.identity,
            &install_root,
        );
        if !matches!(canonical_root, Ok(true)) {
            handle_scan_problem(
                &mut diagnostics,
                options,
                manifest_path.clone(),
                InventoryDiagnosticKind::InvalidManifest,
                canonical_root.err().map_or_else(
                    || "dynamic install manifest is not stored under its identity root".into(),
                    |error| error.to_string(),
                ),
            )?;
            continue;
        }
        let root_identity = match FileIdentity::from_path(&install_root, FileKind::Directory) {
            Ok(identity) => identity,
            Err(error) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    install_root.clone(),
                    InventoryDiagnosticKind::Io,
                    error.to_string(),
                )?;
                continue;
            }
        };
        let manifest_identity = match FileIdentity::from_path(&manifest_path, FileKind::File) {
            Ok(identity) => identity,
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
        installs.push(InstalledDynamicTool {
            canonical_id: manifest.identity.tool.clone(),
            install_root,
            manifest,
            root_identity,
            manifest_identity,
        });
    }

    let mut validated_legacy_installs = Vec::with_capacity(legacy_installs.len());
    for legacy in legacy_installs {
        if let Err(error) = validate_regular_directory_path_from(scan_root, &legacy.install_root) {
            handle_scan_problem(
                &mut diagnostics,
                options,
                legacy.manifest_path.clone(),
                InventoryDiagnosticKind::Io,
                error.to_string(),
            )?;
            continue;
        }
        match std::fs::symlink_metadata(&legacy.manifest_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                validated_legacy_installs.push(legacy);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                handle_scan_problem(
                    &mut diagnostics,
                    options,
                    legacy.manifest_path.clone(),
                    InventoryDiagnosticKind::Io,
                    error.to_string(),
                )?;
            }
        }
    }
    let mut legacy_installs = validated_legacy_installs;

    installs.sort_by(|left, right| {
        (
            left.canonical_id.as_str(),
            path_sort_key(&left.install_root),
            left.manifest.identity.version.as_str(),
        )
            .cmp(&(
                right.canonical_id.as_str(),
                path_sort_key(&right.install_root),
                right.manifest.identity.version.as_str(),
            ))
    });

    legacy_installs.sort_by(|left, right| {
        path_sort_key(&left.manifest_path).cmp(&path_sort_key(&right.manifest_path))
    });
    Ok(ScanReport {
        installs,
        legacy_installs,
        diagnostics,
    })
}

/// Read a bounded regular file without following a final-component symlink and
/// reject path replacement while the bytes are being consumed. Security-
/// sensitive sidecar readers share this primitive with inventory scanning so
/// they all have the same TOCTOU guarantees.
pub(crate) fn read_stable_regular_file(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(std::io::Error::other(
            "path must be a regular non-symlink file",
        ));
    }
    if path_metadata.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "file exceeds its size limit",
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || !same_file_metadata(&path_metadata, &opened_metadata) {
        return Err(std::io::Error::other(
            "file changed while it was being opened",
        ));
    }
    let opened_handle = same_file::Handle::from_file(file.try_clone()?)?;

    let capacity = opened_metadata.len().min(max_bytes) as usize;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "file exceeds its size limit",
        ));
    }

    let current_metadata = std::fs::symlink_metadata(path)?;
    if current_metadata.file_type().is_symlink()
        || !current_metadata.is_file()
        || !same_file_metadata(&opened_metadata, &current_metadata)
        || same_file::Handle::from_path(path)? != opened_handle
    {
        return Err(std::io::Error::other(
            "file path changed while it was being read",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_file_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.file_type() == right.file_type()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

fn validate_regular_directory_path(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => continue,
            std::path::Component::Normal(_) => validate_regular_directory(&current)?,
            _ => {
                return Err(std::io::Error::other(format!(
                    "inventory path contains a non-canonical component: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_regular_directory_path_from(base: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(base).map_err(|_| {
        std::io::Error::other(format!(
            "{} is outside inventory root {}",
            path.display(),
            base.display()
        ))
    })?;
    validate_regular_directory(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(component) => current.push(component),
            _ => {
                return Err(std::io::Error::other(format!(
                    "inventory path contains a non-canonical component: {}",
                    path.display()
                )));
            }
        }
        validate_regular_directory(&current)?;
    }
    Ok(())
}

fn validate_regular_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other(format!(
            "inventory path component must be a non-symlink directory: {}",
            path.display()
        )));
    }
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
mod install_manifest_tests {
    use super::*;
    use crate::tool::{InstallDependency, InstallDependencyKind, InstallIdentity, InstallScope};

    fn identity() -> InstallIdentity {
        InstallIdentity::new(
            "npm:Prettier",
            "3.6.2",
            "linux-x64",
            InstallScope::Isolated,
            &BTreeMap::from([("installer".into(), "AUBE".into())]),
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "24.1.0".into(),
                identity: None,
            }],
            BTreeMap::from([("root-sri".into(), "sha512-example".into())]),
        )
        .unwrap()
    }

    #[test]
    fn new_manifest_round_trips_and_rejects_unknown_or_tampered_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let mut manifest = DynamicToolManifest::from_identity(identity()).unwrap();
        manifest.bins.push(DynamicToolBin {
            name: "prettier".into(),
            path: "bin/prettier".into(),
        });
        manifest.write_atomic(temporary.path()).unwrap();
        let loaded = DynamicToolManifest::load(temporary.path()).unwrap();
        assert_eq!(loaded, manifest.normalize().unwrap());
        assert!(loaded.matches_identity(&identity()));

        let path = DynamicToolManifest::manifest_path(temporary.path());
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["identity"]["version"] = "3.6.3".into();
        assert!(DynamicToolManifest::from_slice(&serde_json::to_vec(&json).unwrap()).is_err());
        json["unknown"] = true.into();
        assert!(DynamicToolManifest::from_slice(&serde_json::to_vec(&json).unwrap()).is_err());
    }

    #[test]
    fn legacy_files_are_detected_but_never_parsed_as_installs() {
        let temporary = tempfile::tempdir().unwrap();
        let legacy = temporary.path().join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(LEGACY_INVENTORY_FILE), b"not even json").unwrap();

        let current_identity = identity();
        let current = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&current_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &current_identity.version,
            ))
            .join(crate::dirs::install_id_component(&current_identity.install_id).unwrap());
        DynamicToolManifest::from_identity(current_identity)
            .unwrap()
            .write_atomic(&current)
            .unwrap();
        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.legacy_installs.len(), 1);
        assert!(report.diagnostics.is_empty());
        assert_eq!(report.legacy_installs[0].install_root, legacy);
    }

    #[test]
    fn bins_are_normalized_and_confined() {
        let mut manifest = DynamicToolManifest::from_identity(identity()).unwrap();
        manifest.bins.push(DynamicToolBin {
            name: " prettier ".into(),
            path: r"bin\prettier".into(),
        });
        assert_eq!(manifest.normalize().unwrap().bins[0].path, "bin/prettier");

        let mut escaped = DynamicToolManifest::from_identity(identity()).unwrap();
        escaped.bins.push(DynamicToolBin {
            name: "prettier".into(),
            path: "../prettier".into(),
        });
        assert!(escaped.normalize().is_err());
    }

    #[test]
    fn scan_rejects_current_manifest_at_wrong_identity_root() {
        let temporary = tempfile::tempdir().unwrap();
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(&temporary.path().join("wrong"))
            .unwrap();
        assert!(scan_installs(temporary.path(), &ScanOptions::default()).is_err());
        let report = scan_installs(
            temporary.path(),
            &ScanOptions {
                corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert!(report.installs.is_empty());
        assert_eq!(report.diagnostics.len(), 1);
    }

    #[test]
    fn scan_accepts_only_the_identity_derived_root() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest_identity = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&manifest_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &manifest_identity.version,
            ))
            .join(crate::dirs::install_id_component(&manifest_identity.install_id).unwrap());
        DynamicToolManifest::from_identity(manifest_identity.clone())
            .unwrap()
            .write_atomic(&root)
            .unwrap();

        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].install_root, root);
        assert_eq!(report.installs[0].manifest.identity, manifest_identity);
    }

    #[test]
    fn load_rejects_a_non_regular_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest_path = DynamicToolManifest::manifest_path(temporary.path());
        std::fs::create_dir(&manifest_path).unwrap();
        assert!(DynamicToolManifest::load(temporary.path()).is_err());
    }

    #[test]
    fn scan_ignores_hidden_transaction_directories() {
        let temporary = tempfile::tempdir().unwrap();
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(&temporary.path().join("tool/.stage"))
            .unwrap();
        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        assert!(report.installs.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_manifest_or_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(outside.path())
            .unwrap();
        symlink(outside.path(), temporary.path().join("linked-dir")).unwrap();
        symlink(
            DynamicToolManifest::manifest_path(outside.path()),
            temporary.path().join(INVENTORY_FILE),
        )
        .unwrap();
        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        assert!(report.installs.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_a_symlinked_scan_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real = temporary.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = temporary.path().join("linked");
        symlink(&real, &linked).unwrap();

        let error = scan_installs(&linked, &ScanOptions::default()).unwrap_err();
        assert!(error.to_string().contains("non-symlink directory"));
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_a_symlinked_manifest() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(outside.path())
            .unwrap();
        symlink(
            DynamicToolManifest::manifest_path(outside.path()),
            DynamicToolManifest::manifest_path(temporary.path()),
        )
        .unwrap();

        assert!(DynamicToolManifest::load(temporary.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_a_symlinked_identity_component() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let manifest_identity = identity();
        let relative_root = crate::dirs::sanitize_tool_id(&manifest_identity.tool)
            .join(crate::dirs::sanitize_version_component(
                &manifest_identity.version,
            ))
            .join(crate::dirs::install_id_component(&manifest_identity.install_id).unwrap());
        let outside_root = outside.path().join(&relative_root);
        DynamicToolManifest::from_identity(manifest_identity)
            .unwrap()
            .write_atomic(&outside_root)
            .unwrap();

        let linked_tool = temporary.path().join("npm");
        symlink(outside.path().join("npm"), &linked_tool).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn scanned_install_rejects_a_changed_manifest_path() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest_identity = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&manifest_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &manifest_identity.version,
            ))
            .join(crate::dirs::install_id_component(&manifest_identity.install_id).unwrap());
        DynamicToolManifest::from_identity(manifest_identity)
            .unwrap()
            .write_atomic(&root)
            .unwrap();
        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        report.installs[0].revalidate().unwrap();

        let path = DynamicToolManifest::manifest_path(&root);
        let replacement = root.join("replacement");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        assert!(report.installs[0].revalidate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn scanned_install_rejects_a_changed_root_path() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let manifest_identity = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&manifest_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &manifest_identity.version,
            ))
            .join(crate::dirs::install_id_component(&manifest_identity.install_id).unwrap());
        DynamicToolManifest::from_identity(manifest_identity)
            .unwrap()
            .write_atomic(&root)
            .unwrap();
        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        let moved = temporary.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        symlink(&moved, &root).unwrap();

        assert!(report.installs[0].revalidate().is_err());
    }
}
