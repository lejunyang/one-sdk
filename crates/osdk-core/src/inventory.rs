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
    /// Whether the requested package installed this command itself.
    ///
    /// Backends that share one install root between a package and its
    /// dependency closure -- conda resolves 16 packages for `clang` -- set this
    /// false for the closure's commands. Those stay in the manifest so
    /// `[shims] include` can still reach them and `where --bins` can list them;
    /// they are only withheld from shims by default. Backends where the
    /// distinction is meaningless leave it true, preserving the previous
    /// behaviour of exporting everything.
    #[serde(default = "owned_by_default", skip_serializing_if = "is_owned")]
    pub owned: bool,
}

fn owned_by_default() -> bool {
    true
}

fn is_owned(owned: &bool) -> bool {
    *owned
}

impl Default for DynamicToolBin {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            // Matches the serde default: only backends that share a prefix
            // with a dependency closure ever set this false.
            owned: true,
        }
    }
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

impl ScanOptions {
    /// Keep scanning past a damaged manifest, reporting it as a diagnostic.
    ///
    /// For the paths that enumerate or reconcile rather than resolve one tool for
    /// execution: `list`, shim ownership, `reshim`, and crucially `uninstall`.
    /// Under the fail-closed default a single unparsable file made all of those
    /// fail, so one damaged install left every other dynamic tool unusable and
    /// blocked the very command needed to remove it -- the only way out was
    /// deleting the directory by hand.
    ///
    /// This is not a weaker guarantee, because the scan was never what made an
    /// install trustworthy: anything about to be executed still goes through
    /// `validated_dynamic_install`, which re-reads the manifest and re-checks the
    /// identity, completion marker and provider evidence on its own. Skipping a
    /// broken neighbour therefore hides nothing; it only stops it from denying
    /// service to the rest.
    pub fn tolerant() -> Self {
        Self {
            corrupt_manifest_policy: CorruptManifestPolicy::CollectDiagnostics,
            ..Self::default()
        }
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            max_manifests: DEFAULT_MAX_MANIFESTS,
            // Stays fail-closed: resolving a specific tool that is about to be
            // executed must refuse a damaged inventory rather than guess. The
            // enumeration paths that only list or reconcile use
            // `ScanOptions::tolerant()` instead -- see its comment.
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

/// Scan only one tool's subtree instead of the whole installs root.
///
/// A backend's `list_installed` only ever cares about its own installs, but
/// scanning from the installs root walks every other tool as well: on a machine
/// with zig and the Android SDK installed that is ~80,000 unrelated files per
/// call, and `reshim` makes one such call per backend per version. Because the
/// layout is `installs/<sanitized tool id>/<version>/<fingerprint>/`, the tool's
/// own subtree is addressable directly, which turns those scans into a fraction
/// of the work while returning the same installs.
///
/// A missing directory is not an error -- a tool that was never installed simply
/// has no subtree, and `scan_installs` already reports that as an empty report.
/// Scan only one tool's subtree, without weakening identity validation.
///
/// `reshim` calls `list_installed` once per backend per version, and scanning
/// from the installs root walked every unrelated tool each time; on a machine
/// with large SDKs installed that turned into tens of thousands of directory
/// reads per call for installs that were then filtered out anyway.
///
/// Note that the identity root stays `installs_root`: an install root is
/// canonical relative to the installs root, so validating against the subtree
/// would drop the tool component and reject every manifest.
pub fn scan_installs_for_tool(
    installs_root: &Path,
    tool: &str,
    options: &ScanOptions,
) -> Result<ScanReport> {
    let tool_root = installs_root.join(crate::dirs::sanitize_tool_id(tool));
    if !tool_root.exists() {
        return Ok(ScanReport::default());
    }
    scan_installs_within(installs_root, &tool_root, options)
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
        // Only what the requested package installed itself can claim a name.
        // A conda prefix also holds its dependency closure, so six packages
        // that merely depend on the msys2 runtime each list `bash`; counting
        // those made 34 ordinary commands (`bash`, `sh`, `kill`, `iconv` ...)
        // look contested and refused to route any of them. Generation and
        // reconciliation already filter on `owned` -- ownership has to agree
        // with them, or the shim on disk and the shim we route to disagree.
        //
        // Unowned bins stay reachable: `shims.include` names one explicitly,
        // and once included it belongs to that package, so a genuine clash
        // between two real owners still surfaces as a conflict.
        for bin in install.manifest.bins.iter().filter(|bin| bin.owned) {
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
    scan_installs_within(scan_root, scan_root, options)
}

/// `identity_root` is what an install root is checked to be canonical against;
/// `scan_root` is only where the walk starts. They differ when scanning a single
/// tool's subtree -- see `scan_installs_for_tool`.
fn scan_installs_within(
    identity_root: &Path,
    scan_root: &Path,
    options: &ScanOptions,
) -> Result<ScanReport> {
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
    let mut manifest_paths = Vec::new();
    let mut legacy_installs = Vec::new();
    let mut diagnostics = Vec::new();
    // Nearly all of the walking went into subtrees that cannot hold a manifest.
    // On this machine android-sdk is 4,309 directories and zig 2,242, neither
    // with a single manifest, against 1,602 under conda which holds all 14 --
    // and `hook-env` pays for it on every shell prompt. A dynamic id always
    // contains a `:`, and `sanitize_tool_id` splits on it, so
    // `installs/<first segment>` is exactly the namespace; a fixed tool
    // (`node`, `zig`, `android-sdk`) never writes an install manifest.
    //
    // Those subtrees are still entered, just not descended into: a manifest
    // placed where it does not belong must keep being found and rejected, since
    // that check is what stops a planted manifest from claiming a command. So
    // the shallow limit below is deliberately 2 (the directory plus what sits
    // directly inside it) rather than skipping the tree outright.
    //
    // Only a walk starting at the installs root can read a namespace off the
    // first segment. `scan_installs_for_tool` starts inside one tool's subtree,
    // where depth 1 is a *version* directory -- applying this there would prune
    // the very tool being scanned.
    let namespace_aware = identity_root == scan_root;
    let full_depth = options.max_depth.saturating_add(1);
    let mut walker = walkdir::WalkDir::new(scan_root)
        .follow_links(false)
        .max_depth(full_depth)
        .into_iter()
        .filter_entry(move |entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                return true;
            }
            // Hidden directories below the install root are implementation state
            // (`.locks`, atomic staging/backup trees, and similar). The inventory
            // file itself is intentionally hidden, so filter directories only.
            if entry.file_name().to_string_lossy().starts_with('.') {
                return false;
            }
            if !namespace_aware || entry.depth() < 2 {
                return true;
            }
            // At depth >= 2 the first segment is known, so stop descending when
            // it is not a namespace that could own a dynamic install.
            entry
                .path()
                .strip_prefix(scan_root)
                .ok()
                .and_then(|relative| relative.components().next())
                .map(|first| {
                    crate::tool::is_dynamic_install_directory(&first.as_os_str().to_string_lossy())
                })
                .unwrap_or(true)
        });

    // An install root's whole payload is unpacked *below* the manifest, and
    // `is_canonical_install_root` only ever accepts
    // `installs/<tool>/<version>/<install_id>` -- so nothing inside an install
    // can itself be a canonical install root. Descending into one therefore
    // cannot find another manifest, it only walks the payload: one conda prefix
    // carries a full Python/mingw distribution. Measured on this machine, this
    // pruning takes the walk from 8,268 directories and 84,352 stat calls down
    // to 7,268 and 65,736 -- real, but only 12% of the waste, because it can
    // only fire inside an install. The namespace filter above is what removes
    // the rest. Manual iteration (rather than a `for` loop) is what lets us
    // call `skip_current_dir`.
    while let Some(entry) = walker.next() {
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
                    // Same reasoning as the current-format branch below: an
                    // install's payload cannot contain another install root.
                    walker.skip_current_dir();
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
                // Stop descending: the rest of this directory is the installed
                // payload, and no canonical install root can live inside it.
                walker.skip_current_dir();
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
        if let Err(error) = validate_regular_directory_path_from(identity_root, &install_root) {
            handle_scan_problem(
                &mut diagnostics,
                options,
                install_root.clone(),
                InventoryDiagnosticKind::Io,
                error.to_string(),
            )?;
            continue;
        }
        match dunce::canonicalize(&install_root) {
            Ok(root) if root.starts_with(&canonical_scan_root) => {}
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
            identity_root,
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
        if let Err(error) = validate_regular_directory_path_from(identity_root, &legacy.install_root)
        {
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
    // Reject a link at the security boundary itself, but resolve platform
    // aliases above it before inspecting ancestors. macOS normally places
    // temporary directories below `/var` -> `/private/var`, while Windows can
    // canonicalize an 8.3 path to its long spelling. Neither changes the
    // identity or containment of the managed directory tree.
    validate_regular_directory(path)?;
    let canonical = dunce::canonicalize(path)?;
    let mut current = PathBuf::new();
    for component in canonical.components() {
        current.push(component.as_os_str());
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => continue,
            std::path::Component::Normal(_) => validate_regular_directory(&current)?,
            _ => {
                return Err(std::io::Error::other(format!(
                    "inventory path contains a non-canonical component: {}",
                    canonical.display()
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
            &BTreeMap::from([("installer".into(), "PNPM".into())]),
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
            ..Default::default()
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
            ..Default::default()
        });
        assert_eq!(manifest.normalize().unwrap().bins[0].path, "bin/prettier");

        let mut escaped = DynamicToolManifest::from_identity(identity()).unwrap();
        escaped.bins.push(DynamicToolBin {
            name: "prettier".into(),
            path: "../prettier".into(),
            ..Default::default()
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
        // The default stays fail-closed for execution paths.
        assert!(scan_installs(temporary.path(), &ScanOptions::default()).is_err());
        // The tolerant option keeps walking and reports the bad entry instead.
        let report = scan_installs(temporary.path(), &ScanOptions::tolerant()).unwrap();
        assert!(report.installs.is_empty());
        assert_eq!(report.diagnostics.len(), 1);
    }

    #[test]
    fn a_static_backend_subtree_is_probed_but_never_walked() {
        // 静态工具（node/zig/android-sdk）永远不写 install manifest，而它们
        // 恰恰是遍历量的主体：本机 android-sdk 4,309 个目录、zig 2,242 个，
        // 两者加起来占全部遍历量的一多半，却一个 manifest 都没有。
        //
        // 这里用一个「放在深处、identity 与路径不符」的 manifest 当探针：
        // 走到它，fail-closed 扫描必然报错；扫描成功就证明没走进去。
        let temporary = tempfile::tempdir().unwrap();
        let buried = temporary
            .path()
            .join("zig")
            .join("0.13.0")
            .join("deadbeef");
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(&buried)
            .unwrap();

        // 同时放一个真实安装，确认扫描本身仍然在工作 —— 否则「没报错」也
        // 可能是因为扫描什么都没做。
        let real = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&real.tool))
            .join(crate::dirs::sanitize_version_component(&real.version))
            .join(crate::dirs::install_id_component(&real.install_id).unwrap());
        DynamicToolManifest::from_identity(real).unwrap().write_atomic(&root).unwrap();

        let report = scan_installs(temporary.path(), &ScanOptions::default())
            .expect("静态 backend 的子树被走穿了，撞上了埋在深处的 manifest");
        assert_eq!(report.installs.len(), 1);
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn a_manifest_sitting_directly_in_a_static_directory_is_still_rejected() {
        // 上一条测的是「不下探」，这条测的是「仍然浅探」。
        //
        // 两者是一对：只按 namespace 白名单硬跳过整棵树的话，放错位置的
        // manifest 会变成静默通过 —— 那是防止伪造 manifest 劫持命令路由的
        // 防线，不能为性能牺牲。所以非 namespace 目录仍要进去看一层。
        let temporary = tempfile::tempdir().unwrap();
        DynamicToolManifest::from_identity(identity())
            .unwrap()
            .write_atomic(&temporary.path().join("node"))
            .unwrap();
        assert!(scan_installs(temporary.path(), &ScanOptions::default()).is_err());
    }

    #[test]
    fn one_damaged_manifest_does_not_hide_healthy_installs() {
        // The regression that motivated the default change: a single unparsable
        // manifest used to make the scan fail outright, so every healthy install
        // disappeared with it. Two tools are needed to catch it -- with only the
        // damaged one present, an empty result looks correct either way.
        let temporary = tempfile::tempdir().unwrap();
        let healthy = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&healthy.tool))
            .join(crate::dirs::sanitize_version_component(&healthy.version))
            .join(crate::dirs::install_id_component(&healthy.install_id).unwrap());
        DynamicToolManifest::from_identity(healthy.clone())
            .unwrap()
            .write_atomic(&root)
            .unwrap();

        let damaged = temporary
            .path()
            .join("conda")
            .join("broken")
            .join("1.0")
            .join("b3-v2-deadbeef");
        std::fs::create_dir_all(&damaged).unwrap();
        std::fs::write(
            DynamicToolManifest::manifest_path(&damaged),
            br#"{"schema":"#,
        )
        .unwrap();

        let report = scan_installs(temporary.path(), &ScanOptions::tolerant()).unwrap();
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].manifest.identity.tool, healthy.tool);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].kind,
            InventoryDiagnosticKind::InvalidManifest
        );
    }

    /// The scoped scan must find the same installs the full scan finds.
    ///
    /// The first attempt passed the tool subtree as the scan root, which also
    /// became the root that install paths were validated against -- so
    /// `<tool>/<version>/<id>` lost its tool component and every manifest was
    /// rejected as "not stored under its identity root". `list_installed` then
    /// returned an empty list rather than an error, which is exactly the kind of
    /// silent wrong answer that looks like "nothing is installed".
    #[test]
    fn scoped_scan_finds_what_the_full_scan_finds() {
        let temporary = tempfile::tempdir().unwrap();
        let wanted = identity();
        let root = temporary
            .path()
            .join(crate::dirs::sanitize_tool_id(&wanted.tool))
            .join(crate::dirs::sanitize_version_component(&wanted.version))
            .join(crate::dirs::install_id_component(&wanted.install_id).unwrap());
        DynamicToolManifest::from_identity(wanted.clone())
            .unwrap()
            .write_atomic(&root)
            .unwrap();

        let scoped =
            scan_installs_for_tool(temporary.path(), &wanted.tool, &ScanOptions::default()).unwrap();
        assert_eq!(
            scoped.installs.len(),
            1,
            "scoped scan must still validate the identity root against the installs root"
        );
        assert_eq!(scoped.installs[0].install_root, root);

        let full = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        assert_eq!(
            full.installs.iter().map(|i| &i.install_root).collect::<Vec<_>>(),
            scoped.installs.iter().map(|i| &i.install_root).collect::<Vec<_>>(),
        );
    }

    /// The point of scoping: an unrelated tool's subtree must not be walked.
    /// Without scoping this returns two installs.
    #[test]
    fn scoped_scan_ignores_other_tools() {
        let temporary = tempfile::tempdir().unwrap();
        for (tool, version) in [("npm:Prettier", "3.6.2"), ("conda:nasm", "2.16.3")] {
            let other = InstallIdentity::new(
                tool,
                version,
                "linux-x64",
                InstallScope::Isolated,
                &BTreeMap::new(),
                Vec::new(),
                BTreeMap::new(),
            )
            .unwrap();
            let root = temporary
                .path()
                .join(crate::dirs::sanitize_tool_id(&other.tool))
                .join(crate::dirs::sanitize_version_component(&other.version))
                .join(crate::dirs::install_id_component(&other.install_id).unwrap());
            DynamicToolManifest::from_identity(other)
                .unwrap()
                .write_atomic(&root)
                .unwrap();
        }

        let scoped =
            scan_installs_for_tool(temporary.path(), "conda:nasm", &ScanOptions::default()).unwrap();
        assert_eq!(scoped.installs.len(), 1);
        assert_eq!(scoped.installs[0].manifest.identity.tool, "conda:nasm");
        assert_eq!(
            scan_installs(temporary.path(), &ScanOptions::default())
                .unwrap()
                .installs
                .len(),
            2,
            "fixture must really contain two tools, or this proves nothing"
        );
    }

    /// A tool that has never been installed has no subtree at all; that is an
    /// empty inventory, not an error.
    #[test]
    fn scoped_scan_of_a_missing_tool_is_empty() {
        let temporary = tempfile::tempdir().unwrap();
        let report =
            scan_installs_for_tool(temporary.path(), "conda:absent", &ScanOptions::default())
                .unwrap();
        assert!(report.installs.is_empty());
        assert!(report.diagnostics.is_empty());
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
    fn scan_accepts_a_real_root_below_a_symlinked_platform_ancestor() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real_parent = temporary.path().join("real");
        std::fs::create_dir(&real_parent).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&real_parent, &alias).unwrap();
        let scan_root = alias.join("installs");

        let manifest_identity = identity();
        let root = scan_root
            .join(crate::dirs::sanitize_tool_id(&manifest_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &manifest_identity.version,
            ))
            .join(crate::dirs::install_id_component(&manifest_identity.install_id).unwrap());
        DynamicToolManifest::from_identity(manifest_identity)
            .unwrap()
            .write_atomic(&root)
            .unwrap();

        assert!(DynamicToolManifest::load(&root).is_ok());
        let report = scan_installs(&scan_root, &ScanOptions::default()).unwrap();
        assert_eq!(report.installs.len(), 1);
        assert_eq!(report.installs[0].install_root, root);
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

    /// Write an install whose manifest exports `bins` as `(name, owned)`, so a
    /// test can describe ownership without repeating the directory layout.
    fn write_install(base: &Path, tool: &str, bins: &[(&str, bool)]) {
        let install_identity = InstallIdentity::new(
            tool,
            "1.0.0",
            "linux-x64",
            InstallScope::Isolated,
            &BTreeMap::new(),
            Vec::new(),
            BTreeMap::from([("root-sri".into(), "sha512-example".into())]),
        )
        .unwrap();
        let root = base
            .join(crate::dirs::sanitize_tool_id(&install_identity.tool))
            .join(crate::dirs::sanitize_version_component(
                &install_identity.version,
            ))
            .join(crate::dirs::install_id_component(&install_identity.install_id).unwrap());
        let mut manifest = DynamicToolManifest::from_identity(install_identity).unwrap();
        for (name, owned) in bins {
            // The scanner confines bins to real files under the root, so the
            // executable has to exist before the manifest names it.
            let relative = format!("bin/{name}");
            let absolute = root.join(&relative);
            std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            std::fs::write(&absolute, b"#!/bin/sh\n").unwrap();
            manifest.bins.push(DynamicToolBin {
                name: (*name).into(),
                path: relative,
                owned: *owned,
            });
        }
        manifest.write_atomic(&root).unwrap();
    }

    /// A dynamic install lives one level below `Dirs::install_path`, so the
    /// completion-marker check used for fixed tools can never find it.
    ///
    /// `exec` used to reinstall an already-present dynamic tool on every call
    /// (~10 s each time, rewriting the completion marker) because the fast
    /// path was switched off for any id containing a `:`. The offline check
    /// that replaced it depends on this layout, so pin the layout down.
    #[test]
    fn a_dynamic_install_root_sits_below_the_fixed_tool_install_path() {
        let temporary = tempfile::tempdir().unwrap();
        let identity = InstallIdentity::new(
            "conda:ninja",
            "1.13.2",
            "linux-x64",
            InstallScope::Isolated,
            &BTreeMap::new(),
            Vec::new(),
            BTreeMap::from([("root-sri".into(), "sha512-example".into())]),
        )
        .unwrap();
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_INSTALL_DIR" => Some(temporary.path().display().to_string()),
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        // Go through the real locator, not a hand-rolled path join: the layout
        // rule under test lives in `InstallLocator`, so a test that rebuilds
        // the path itself would pass no matter what the locator does.
        let install_root = crate::dirs::InstallLocator::new(&dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();

        // What `is_installed` would look at for a fixed tool.
        let fixed_style_path = dirs.install_path(&identity.tool, &identity.version);
        assert_ne!(
            install_root, fixed_style_path,
            "a dynamic install root must not equal <tool>/<version>: that is the \
             path `is_installed` probes, and if they coincided the marker check \
             would silently start deciding dynamic installs"
        );
        assert_eq!(
            install_root.parent().unwrap(),
            fixed_style_path,
            "the dynamic root is exactly one level below <tool>/<version>"
        );
        assert_eq!(
            install_root.file_name().unwrap().to_string_lossy(),
            crate::dirs::install_id_component(&identity.install_id).unwrap(),
            "and that extra level is the install id"
        );
    }

    /// The install id is a fingerprint over resolved `materials` (archive
    /// digests), which are only known after a network round-trip. So an
    /// offline "is it already installed?" check must compare version and
    /// identity options instead of recomputing the id.
    ///
    /// Without this test, someone could "simplify" that check into
    /// recomputing the id locally; it would compile, and every functional
    /// test would stay green, but the fast path would silently never hit --
    /// `exec` would quietly go back to reinstalling on every call.
    #[test]
    fn the_install_id_depends_on_materials_that_only_a_download_can_supply() {
        let options = BTreeMap::new();
        let with_materials = InstallIdentity::new(
            "conda:ninja",
            "1.13.2",
            "linux-x64",
            InstallScope::Isolated,
            &options,
            Vec::new(),
            BTreeMap::from([("root-sri".into(), "sha512-example".into())]),
        )
        .unwrap();
        let without_materials = InstallIdentity::new(
            "conda:ninja",
            "1.13.2",
            "linux-x64",
            InstallScope::Isolated,
            &options,
            Vec::new(),
            BTreeMap::new(),
        )
        .unwrap();

        assert_ne!(
            with_materials.install_id, without_materials.install_id,
            "materials feed the fingerprint, so the id cannot be derived \
             offline from tool+version+options alone"
        );
        // The parts that *are* knowable offline must agree, since those are
        // what the fast path matches on.
        assert_eq!(with_materials.version, without_materials.version);
        assert_eq!(
            with_materials.material_options,
            without_materials.material_options
        );
    }

    fn owners_of(base: &Path, name: &str) -> Vec<String> {
        let report = scan_installs(base, &ScanOptions::default()).unwrap();
        build_bin_ownership_candidates(&report.installs)
            .get(name)
            .map(|candidates| {
                candidates
                    .iter()
                    .map(|candidate| candidate.canonical_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn unowned_closure_bins_do_not_claim_ownership() {
        // Six conda packages share one msys2 runtime closure, so all six list
        // `bash` while only `m2-bash` installed it. Counting the closure made
        // 34 commands look contested and the shim refused to route any of them.
        let temporary = tempfile::tempdir().unwrap();
        write_install(temporary.path(), "conda:m2-bash", &[("bash", true)]);
        write_install(
            temporary.path(),
            "conda:m2-sed",
            &[("sed", true), ("bash", false)],
        );
        write_install(
            temporary.path(),
            "conda:m2-grep",
            &[("grep", true), ("bash", false)],
        );

        assert_eq!(owners_of(temporary.path(), "bash"), ["conda:m2-bash"]);
        // The packages keep their own commands.
        assert_eq!(owners_of(temporary.path(), "sed"), ["conda:m2-sed"]);
        assert_eq!(owners_of(temporary.path(), "grep"), ["conda:m2-grep"]);
    }

    #[test]
    fn two_real_owners_still_conflict() {
        // Guards the fix against overshooting into "just take the first owner":
        // silently picking a winner between two genuine owners would run the
        // wrong compiler, which is worse than refusing to route.
        let temporary = tempfile::tempdir().unwrap();
        write_install(temporary.path(), "conda:gcc_win-64", &[("gcc", true)]);
        write_install(temporary.path(), "conda:m2w64-gcc", &[("gcc", true)]);

        assert_eq!(
            owners_of(temporary.path(), "gcc"),
            ["conda:gcc_win-64", "conda:m2w64-gcc"]
        );
    }

    #[test]
    fn ownership_ignores_manifest_bin_order() {
        // Ownership must not depend on where the owned entry sits in the list,
        // otherwise the answer changes with an unrelated packaging change.
        let temporary = tempfile::tempdir().unwrap();
        write_install(
            temporary.path(),
            "conda:m2-gawk",
            &[("bash", false), ("awk", true), ("sh", false)],
        );
        write_install(
            temporary.path(),
            "conda:m2-bash",
            &[("bash", true), ("sh", false)],
        );

        assert_eq!(owners_of(temporary.path(), "bash"), ["conda:m2-bash"]);
        assert_eq!(owners_of(temporary.path(), "awk"), ["conda:m2-gawk"]);
        // Nobody owns `sh` here, so it is claimed by no one rather than by both.
        assert!(owners_of(temporary.path(), "sh").is_empty());
    }

    /// The scan must stop at a manifest instead of walking the payload below it.
    ///
    /// `is_canonical_install_root` only accepts
    /// `installs/<tool>/<version>/<install_id>`, so nothing inside an install
    /// can be an install root -- descending merely stat-s the unpacked payload.
    /// One conda prefix ships a whole Python/mingw distribution, so on a real
    /// tree this walked 8,268 directories and 84,352 entries instead of 466 and
    /// 4,483, and `hook-env` paid it at every prompt.
    ///
    /// The probe is a manifest-named file planted *inside* the install. Pruning
    /// means it is never opened; without pruning the scan reads it, fails to
    /// parse it, and reports a diagnostic. Its depth must stay inside the scan's
    /// own limit -- planted deeper than `max_depth` the depth cut-off hides it
    /// first, the assertion holds either way, and the test silently proves
    /// nothing.
    #[test]
    fn scan_stops_at_a_manifest_and_never_walks_the_payload() {
        let temporary = tempfile::tempdir().unwrap();
        write_install(temporary.path(), "conda:cmake", &[("cmake", true)]);

        // conda/cmake/<version>/<install_id>
        let version_dir = std::fs::read_dir(temporary.path().join("conda").join("cmake"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let install_root = std::fs::read_dir(&version_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            DynamicToolManifest::manifest_path(&install_root).is_file(),
            "fixture must place a manifest at the install root"
        );

        // Two levels below the install root: comfortably within the depth limit,
        // so only pruning can keep the scan from reading it.
        let payload = install_root.join("share").join("doc");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join(INVENTORY_FILE), b"not a manifest").unwrap();
        let probe_depth = payload
            .strip_prefix(temporary.path())
            .unwrap()
            .components()
            .count();
        assert!(
            probe_depth <= DEFAULT_MAX_DEPTH,
            "probe at depth {probe_depth} must stay inside the scan's own depth \
             limit of {DEFAULT_MAX_DEPTH}, or the depth cut-off hides it and this \
             test proves nothing"
        );

        let report = scan_installs(temporary.path(), &ScanOptions::tolerant()).unwrap();

        assert_eq!(
            report.installs.len(),
            1,
            "expected only the real install: {:?}",
            report
                .installs
                .iter()
                .map(|install| install.canonical_id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            report.diagnostics.is_empty(),
            "a pruned scan must never open the payload, so the planted file \
             cannot produce a diagnostic; got: {:?}",
            report.diagnostics
        );
    }

    /// Pruning must not hide a sibling install that shares a parent directory.
    ///
    /// Guards against overshooting into "skip the whole tool directory": two
    /// versions of one tool, and two install ids of one version, are siblings of
    /// each other, not nested, so all of them must still be found.
    #[test]
    fn pruning_still_finds_every_sibling_install() {
        let temporary = tempfile::tempdir().unwrap();
        write_install(temporary.path(), "conda:nasm", &[("nasm", true)]);
        write_install(temporary.path(), "conda:ninja", &[("ninja", true)]);
        write_install(temporary.path(), "conda:m2-bash", &[("bash", true)]);

        let report = scan_installs(temporary.path(), &ScanOptions::default()).unwrap();
        let mut found = report
            .installs
            .iter()
            .map(|install| install.canonical_id.clone())
            .collect::<Vec<_>>();
        found.sort();
        assert_eq!(found, ["conda:m2-bash", "conda:nasm", "conda:ninja"]);
    }
}
