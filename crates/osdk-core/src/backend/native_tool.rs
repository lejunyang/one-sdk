//! Shared lifecycle for dynamic tools installed by a managed native runtime.
//!
//! Cargo and Go-module tools both delegate compilation or package resolution to
//! an exact managed runtime. This module owns the common durable boundary: an
//! identity-qualified lock and install root, a sibling staging directory, a
//! receipt binding the provider/runtime/binary bytes, an adjacent metadata seal,
//! and fail-closed reuse, listing, and removal helpers. The seal detects
//! accidental or isolated install-root mutation; it is not a security boundary
//! against a process that can also rewrite the user's osdk state directory.
//! Backend-specific command planning stays in the individual backend modules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::dirs::{Dirs, InstallLocator};
use crate::error::{Error, Result};
use crate::inventory::{DynamicToolBin, DynamicToolManifest, ScanOptions};
use crate::pipeline::HashAlgo;
use crate::platform::Platform;
use crate::tool::{InstallDependency, InstallDependencyKind, InstallIdentity, InstallScope};

pub const NATIVE_TOOL_RECEIPT_FILE: &str = ".osdk-native-receipt.json";
pub const LOCKED_NATIVE_RUNTIME_OPTION: &str = "__osdk_native_runtime";
pub const LOCKED_NATIVE_RUNTIME_VERSION_OPTION: &str = "__osdk_native_runtime_version";
pub const LOCKED_NATIVE_REPLAY_OPTION: &str = "__osdk_native_replay";
const NATIVE_TOOL_RECEIPT_SCHEMA: u32 = 1;
const MAX_NATIVE_TOOL_RECEIPT_BYTES: u64 = 256 * 1024;
const NATIVE_TOOL_SEAL_SUFFIX: &str = ".native-seal";
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolBinReceipt {
    /// Slash-separated path relative to the install root.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolReceipt {
    pub schema: u32,
    /// Backend-specific installer that produced the binaries.
    pub provider: NativeToolProvider,
    /// Exact managed compiler/runtime used by the provider.
    pub runtime: InstallDependency,
    /// Content identity for every published executable.
    pub bins: Vec<NativeToolBinReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeToolSeal {
    schema: u32,
    install_id: String,
    content_blake3: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeToolProvider {
    CargoInstall,
    CargoBinstall,
    GoInstall,
}

const CARGO_PROVIDERS: &[NativeToolProvider] = &[
    NativeToolProvider::CargoInstall,
    NativeToolProvider::CargoBinstall,
];
const GO_PROVIDERS: &[NativeToolProvider] = &[NativeToolProvider::GoInstall];

/// Stable content identity for an installed managed runtime tree. Native
/// backend adapters may bind this value into `InstallDependency::identity`.
pub fn runtime_tree_identity(root: &Path) -> Result<String> {
    hash_runtime_tree(root)
}

/// Lifecycle state derived entirely from inputs known before installation.
#[derive(Debug, Clone)]
pub struct NativeToolLifecycle {
    locator: InstallLocator,
    family: NativeToolFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeToolFamily {
    Cargo,
    Go,
}

impl NativeToolFamily {
    pub fn runtime(self) -> &'static str {
        match self {
            Self::Cargo => "rust",
            Self::Go => "go",
        }
    }

    fn providers(self) -> &'static [NativeToolProvider] {
        match self {
            Self::Cargo => CARGO_PROVIDERS,
            Self::Go => GO_PROVIDERS,
        }
    }
}

/// Result of atomically selecting or preparing one native install identity.
pub enum NativeToolPreparation {
    Reused(PathBuf),
    Staged(NativeToolStage),
}

impl NativeToolLifecycle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dirs: &Dirs,
        platform: Platform,
        tool: &str,
        version: &str,
        options: &BTreeMap<String, String>,
        family: NativeToolFamily,
        runtime: InstallDependency,
        materials: BTreeMap<String, String>,
    ) -> Result<Self> {
        validate_runtime_dependency(&runtime)?;
        validate_family_tool_id(family, tool)?;
        let expected_runtime = family.runtime();
        if runtime.id != expected_runtime {
            return Err(Error::config(format!(
                "native tool `{tool}` requires runtime `{expected_runtime}`, got `{}`",
                runtime.id
            )));
        }
        let identity = InstallIdentity::new(
            tool,
            version,
            platform.to_string(),
            InstallScope::Isolated,
            options,
            vec![runtime],
            materials,
        )?;
        Ok(Self {
            locator: InstallLocator::new(dirs, identity)?,
            family,
        })
    }

    pub fn from_identity(
        dirs: &Dirs,
        family: NativeToolFamily,
        identity: InstallIdentity,
    ) -> Result<Self> {
        if identity.scope != InstallScope::Isolated {
            return Err(Error::config(
                "native dynamic tools require isolated install scope",
            ));
        }
        validate_family_tool_id(family, &identity.tool)?;
        let runtime = exact_runtime_dependency(&identity)?;
        let expected_runtime = family.runtime();
        if runtime.id != expected_runtime {
            return Err(Error::config(format!(
                "native tool `{}` requires runtime `{expected_runtime}`, got `{}`",
                identity.tool, runtime.id
            )));
        }
        Ok(Self {
            locator: InstallLocator::new(dirs, identity)?,
            family,
        })
    }

    pub fn identity(&self) -> &InstallIdentity {
        self.locator.identity()
    }

    pub fn install_root(&self) -> &Path {
        self.locator.install_root()
    }

    pub fn metadata_seal_path(&self) -> PathBuf {
        metadata_seal_path(&self.locator)
    }

    pub async fn acquire_lock(&self) -> Result<crate::lock::FileLock> {
        super::dynamic::acquire_install_lock(&self.locator, "native tool").await
    }

    /// Serialize one complete install attempt. A valid existing install is
    /// returned directly; otherwise a stage retaining the identity lock is
    /// returned so no second provider command can run for the same identity.
    pub async fn prepare(&self, dirs: &Dirs) -> Result<NativeToolPreparation> {
        let lock = self.acquire_lock().await?;
        if let Some(root) = self.reuse(dirs)? {
            return Ok(NativeToolPreparation::Reused(root));
        }
        self.stage_with_lock(lock)
            .map(NativeToolPreparation::Staged)
    }

    /// Validate an already-published install. A missing root is not an error; a
    /// complete but tampered root is rejected instead of silently rebuilt.
    pub fn validate_complete(&self, dirs: &Dirs) -> Result<bool> {
        validate_install_candidate(dirs, self.family, self.install_root(), self.identity())
    }

    /// Backend-hook form of [`Self::validate_complete`].
    pub fn validate_dynamic_install(
        &self,
        dirs: &Dirs,
        install_root: &Path,
        identity: &InstallIdentity,
    ) -> Result<bool> {
        if identity != self.identity() || install_root != self.install_root() {
            return Ok(false);
        }
        validate_install_candidate(dirs, self.family, install_root, identity)
    }

    /// Validate a complete install and either reuse it or fail closed.
    pub fn reuse(&self, dirs: &Dirs) -> Result<Option<PathBuf>> {
        if self.validate_complete(dirs)? {
            Ok(Some(self.install_root().to_path_buf()))
        } else if self.install_root().exists() {
            match std::fs::symlink_metadata(self.install_root().join(".osdk-complete")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(Error::other(format!(
                    "refusing to reuse incomplete or invalid native tool install at {}",
                    self.install_root().display()
                ))),
            }
        } else {
            Ok(None)
        }
    }

    /// Remove stale transaction state after the caller determined that no
    /// valid complete install exists. The identity lock is held by `prepare`.
    fn discard_incomplete_install(&self) -> Result<()> {
        let _ = remove_metadata_seal(&self.locator)?;
        Ok(())
    }

    /// Create a unique, unexposed sibling of the final install directory while
    /// retaining the identity lock for the complete transaction.
    fn stage_with_lock(&self, lock: crate::lock::FileLock) -> Result<NativeToolStage> {
        let final_root = self.install_root();
        let parent = final_root
            .parent()
            .ok_or_else(|| Error::other("native tool install root has no parent"))?;
        create_directory_chain_no_symlinks(parent)?;

        match std::fs::symlink_metadata(final_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::other(format!(
                    "refusing to replace non-directory native tool install root {}",
                    final_root.display()
                )));
            }
            Ok(_) => match std::fs::symlink_metadata(final_root.join(".osdk-complete")) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    return Err(Error::other(format!(
                        "refusing to replace complete native tool install at {}",
                        final_root.display()
                    )));
                }
                Ok(_) => {
                    return Err(Error::other(format!(
                        "refusing to replace native tool install with an unsafe completion marker at {}",
                        final_root.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::remove_dir_all(final_root)
                        .map_err(|error| Error::io(final_root, error))?
                }
                Err(error) => return Err(Error::io(final_root.join(".osdk-complete"), error)),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(final_root, error)),
        }
        self.discard_incomplete_install()?;

        let component = final_root
            .file_name()
            .ok_or_else(|| Error::other("native tool install root has no filename"))?
            .to_string_lossy();
        loop {
            let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let stage_root = parent.join(format!(
                ".{component}.stage-{}-{serial}",
                std::process::id()
            ));
            match std::fs::create_dir(&stage_root) {
                Ok(()) => {
                    return Ok(NativeToolStage {
                        locator: self.locator.clone(),
                        family: self.family,
                        stage_root: Some(stage_root),
                        _lock: lock,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::io(stage_root, error)),
            }
        }
    }

    /// Remove exactly this identity-qualified root. The caller must hold the
    /// lifecycle lock so install and uninstall cannot race.
    pub async fn uninstall(&self) -> Result<bool> {
        let _lock = self.acquire_lock().await?;
        remove_exact_install(&self.locator)
    }
}

/// A native install staging root that is deleted unless publication succeeds.
pub struct NativeToolStage {
    locator: InstallLocator,
    family: NativeToolFamily,
    stage_root: Option<PathBuf>,
    _lock: crate::lock::FileLock,
}

impl std::fmt::Debug for NativeToolStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeToolStage")
            .field("install_id", &self.locator.identity().install_id)
            .field("stage_root", &self.stage_root)
            .finish_non_exhaustive()
    }
}

impl NativeToolStage {
    pub fn path(&self) -> &Path {
        self.stage_root
            .as_deref()
            .expect("published native tool stage has no path")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.path().join("bin")
    }

    /// Clear provider output before an explicitly permitted fallback while
    /// retaining the staging identity and its cross-process lock.
    pub fn reset(&mut self) -> Result<()> {
        let path = self.path().to_path_buf();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| Error::io(&path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::other(format!(
                "refusing to reset unsafe native tool stage {}",
                path.display()
            )));
        }
        create_directory_chain_no_symlinks(
            path.parent()
                .ok_or_else(|| Error::other("native tool stage has no parent"))?,
        )?;
        std::fs::remove_dir_all(&path).map_err(|error| Error::io(&path, error))?;
        std::fs::create_dir(&path).map_err(|error| Error::io(&path, error))
    }

    /// Validate provider output, write receipt/inventory/completion metadata in
    /// that order, and expose the complete tree with one directory rename.
    pub fn publish(mut self, provider: NativeToolProvider) -> Result<PathBuf> {
        if !self.family.providers().contains(&provider) {
            return Err(Error::config(
                "native tool provider does not match its namespace",
            ));
        }
        let stage_root = self
            .stage_root
            .as_deref()
            .expect("published native tool stage has no path");
        super::dynamic::reject_symlinks(stage_root)?;
        for reserved in [
            NATIVE_TOOL_RECEIPT_FILE,
            crate::inventory::INVENTORY_FILE,
            ".osdk-complete",
        ] {
            let path = stage_root.join(reserved);
            if path.exists() {
                return Err(Error::other(format!(
                    "native tool provider wrote reserved metadata path {}",
                    path.display()
                )));
            }
        }

        let (manifest_bins, receipt_bins) = inspect_bins(stage_root)?;
        let runtime = exact_runtime_dependency(self.locator.identity())?.clone();
        write_receipt(
            stage_root,
            &NativeToolReceipt {
                schema: NATIVE_TOOL_RECEIPT_SCHEMA,
                provider,
                runtime,
                bins: receipt_bins,
            },
        )?;
        let mut manifest = DynamicToolManifest::from_identity(self.locator.identity().clone())?;
        manifest.bins = manifest_bins;
        manifest.write_atomic(stage_root)?;
        std::fs::write(stage_root.join(".osdk-complete"), b"")
            .map_err(|error| Error::io(stage_root.join(".osdk-complete"), error))?;

        let final_root = self.locator.install_root();
        write_metadata_seal(&self.locator, stage_root)?;
        if let Err(error) = publish_directory_no_replace(stage_root, final_root) {
            let _ = remove_metadata_seal(&self.locator);
            return Err(error);
        }
        self.stage_root = None;
        Ok(final_root.to_path_buf())
    }
}

impl Drop for NativeToolStage {
    fn drop(&mut self) {
        if let Some(path) = self.stage_root.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

pub fn receipt_path(install_root: &Path) -> PathBuf {
    install_root.join(NATIVE_TOOL_RECEIPT_FILE)
}

pub fn load_receipt(install_root: &Path) -> Result<NativeToolReceipt> {
    let path = receipt_path(install_root);
    let bytes = crate::inventory::read_stable_regular_file(&path, MAX_NATIVE_TOOL_RECEIPT_BYTES)
        .map_err(|error| Error::io(&path, error))?;
    let receipt: NativeToolReceipt = serde_json::from_slice(&bytes)?;
    validate_receipt(&receipt)?;
    Ok(receipt)
}

/// Validate a native candidate without executing its provider or consulting the
/// network. This is the single reuse/activation/shim/lock inspection boundary.
pub fn validate_install_candidate(
    dirs: &Dirs,
    family: NativeToolFamily,
    install_root: &Path,
    identity: &InstallIdentity,
) -> Result<bool> {
    if identity.scope != InstallScope::Isolated
        || !is_regular_file(&install_root.join(".osdk-complete"))
        || !is_regular_file(&DynamicToolManifest::manifest_path(install_root))
        || !is_regular_file(&receipt_path(install_root))
    {
        return Ok(false);
    }
    let locator = InstallLocator::new(dirs, identity.clone())?;
    if !locator.validates_existing_install_root(install_root) {
        return Ok(false);
    }
    super::dynamic::reject_symlinks(install_root)?;
    validate_metadata_seal(&locator)?;
    let manifest = DynamicToolManifest::load(install_root)?;
    if !manifest.matches_identity(identity) {
        return Err(Error::other(format!(
            "native tool install identity mismatch at {}",
            install_root.display()
        )));
    }
    let receipt = load_receipt(install_root)?;
    if !family.providers().contains(&receipt.provider) {
        return Ok(false);
    }
    if &receipt.runtime != exact_runtime_dependency(identity)? {
        return Err(Error::other(format!(
            "native tool runtime receipt does not match install identity at {}",
            install_root.display()
        )));
    }
    if !runtime_is_installed(dirs, &receipt.runtime) {
        return Ok(false);
    }
    let expected_bins = manifest
        .bins
        .iter()
        .map(|bin| {
            validate_relative_bin_path(&bin.path)?;
            let file_name = Path::new(&bin.path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::config("native inventory bin path is not valid UTF-8"))?;
            if executable_stem(file_name)? != bin.name {
                return Err(Error::config(format!(
                    "native inventory bin name does not match its path: `{}`",
                    bin.name
                )));
            }
            Ok(portable_path_key(&bin.path))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let receipt_bins = receipt
        .bins
        .iter()
        .map(|bin| portable_path_key(&bin.path))
        .collect::<BTreeSet<_>>();
    if manifest.bins.len() != expected_bins.len()
        || expected_bins.len() != receipt_bins.len()
        || !expected_bins.iter().all(|path| receipt_bins.contains(path))
    {
        return Err(Error::other(format!(
            "native tool receipt bins do not match inventory at {}",
            install_root.display()
        )));
    }
    for bin in &receipt.bins {
        let path = checked_bin_path(install_root, &bin.path)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| Error::io(&path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != bin.size {
            return Err(Error::other(format!(
                "native tool binary metadata changed at {}",
                path.display()
            )));
        }
        let actual = crate::pipeline::verify::hash_file(&path, HashAlgo::Sha256)?;
        if actual != bin.sha256 {
            return Err(Error::other(format!(
                "native tool binary checksum mismatch at {}",
                path.display()
            )));
        }
    }
    Ok(true)
}

pub fn list_installed(
    dirs: &Dirs,
    platform: Platform,
    family: NativeToolFamily,
    tool: &str,
) -> Result<Vec<String>> {
    let report = crate::inventory::scan_installs(&dirs.installs, &ScanOptions::default())?;
    let mut versions = BTreeSet::new();
    for install in report.installs {
        let identity = &install.manifest.identity;
        if identity.tool != tool
            || identity.platform != platform.to_string()
            || identity.scope != InstallScope::Isolated
        {
            continue;
        }
        if validate_install_candidate(dirs, family, &install.install_root, identity)? {
            versions.insert(identity.version.clone());
        }
    }
    Ok(versions.into_iter().collect())
}

pub fn remove_exact_install(locator: &InstallLocator) -> Result<bool> {
    let root = locator.install_root();
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => remove_metadata_seal(locator),
        Err(error) => Err(Error::io(root, error)),
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || !locator.validates_existing_install_root(root) =>
        {
            Err(Error::other(format!(
                "refusing to remove unsafe native tool install root {}",
                root.display()
            )))
        }
        Ok(_) => {
            std::fs::remove_dir_all(root).map_err(|error| Error::io(root, error))?;
            let _ = remove_metadata_seal(locator)?;
            Ok(true)
        }
    }
}

pub fn exact_runtime_dependency(identity: &InstallIdentity) -> Result<&InstallDependency> {
    let mut runtimes = identity
        .dependencies
        .iter()
        .filter(|dependency| dependency.kind == InstallDependencyKind::Runtime);
    let runtime = runtimes.next().ok_or_else(|| {
        Error::config("native tool install identity requires one exact runtime dependency")
    })?;
    if runtimes.next().is_some() {
        return Err(Error::config(
            "native tool install identity contains multiple runtime dependencies",
        ));
    }
    validate_runtime_dependency(runtime)?;
    Ok(runtime)
}

fn validate_runtime_dependency(runtime: &InstallDependency) -> Result<()> {
    if runtime.kind != InstallDependencyKind::Runtime
        || runtime.id.contains(':')
        || runtime.version.trim().is_empty()
        || runtime.version != runtime.version.trim()
        || runtime.version.chars().any(char::is_control)
        || runtime
            .identity
            .as_deref()
            .is_none_or(|identity| identity.trim().is_empty() || identity != identity.trim())
    {
        return Err(Error::config(
            "native tool runtime dependency must name one exact managed runtime",
        ));
    }
    Ok(())
}

fn validate_family_tool_id(family: NativeToolFamily, tool: &str) -> Result<()> {
    let subject = match family {
        NativeToolFamily::Cargo => tool.strip_prefix("cargo:").filter(|subject| {
            let bytes = subject.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 64
                && bytes[0].is_ascii_lowercase()
                && bytes.iter().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        }),
        NativeToolFamily::Go => tool
            .strip_prefix("go:")
            .filter(|subject| valid_go_module_subject(subject)),
    };
    if subject.is_none() {
        return Err(Error::config(format!(
            "invalid canonical {:?} native tool id `{tool}`",
            family
        )));
    }
    Ok(())
}

fn valid_go_module_subject(subject: &str) -> bool {
    if subject.is_empty()
        || subject.len() > 4096
        || subject.trim() != subject
        || subject.contains(['\\', '@', '?', '#', '%', ':'])
        || subject.chars().any(char::is_control)
        || subject.chars().any(char::is_whitespace)
    {
        return false;
    }
    let mut components = subject.split('/');
    let Some(host) = components.next() else {
        return false;
    };
    let valid_host = host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && label
                    .bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
    valid_host
        && components.all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.ends_with('.')
                && component.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                })
        })
}

fn runtime_is_installed(dirs: &Dirs, runtime: &InstallDependency) -> bool {
    let marker_root = dirs.install_path(&runtime.id, &runtime.version);
    if !is_regular_file(&marker_root.join(".osdk-complete")) {
        return false;
    }
    match runtime.identity.as_deref() {
        None => false,
        Some(expected) => runtime_identity_at(dirs, runtime).as_deref() == Some(expected),
    }
}

fn runtime_identity_at(dirs: &Dirs, runtime: &InstallDependency) -> Option<String> {
    let runtime_root = match runtime.id.as_str() {
        "rust" => dirs.rustup_home().join("toolchains").join(&runtime.version),
        "go" => dirs.install_path("go", &runtime.version),
        _ => return None,
    };
    hash_runtime_tree(&runtime_root).ok()
}

fn hash_runtime_tree(root: &Path) -> Result<String> {
    let canonical_root = dunce::canonicalize(root).map_err(|error| Error::io(root, error))?;
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        if entry.file_type().is_symlink() {
            return Err(Error::other(format!(
                "runtime identity contains a forbidden symlink: {}",
                entry.path().display()
            )));
        }
        if entry.file_type().is_file() {
            let canonical = dunce::canonicalize(entry.path())
                .map_err(|error| Error::io(entry.path(), error))?;
            let relative = canonical.strip_prefix(&canonical_root).map_err(|_| {
                Error::other(format!(
                    "runtime identity path escapes root: {}",
                    entry.path().display()
                ))
            })?;
            files.push((relative.to_path_buf(), canonical));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = blake3::Hasher::new_derive_key("osdk-native-runtime-tree-v1");
    for (relative, path) in files {
        let relative = relative.to_string_lossy().replace('\\', "/");
        hasher.update(&(relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        let digest = crate::pipeline::verify::hash_file(&path, HashAlgo::Sha256)?;
        hasher.update(digest.as_bytes());
    }
    Ok(format!("b3-tree-v1:{}", hasher.finalize().to_hex()))
}

fn inspect_bins(root: &Path) -> Result<(Vec<DynamicToolBin>, Vec<NativeToolBinReceipt>)> {
    let bin_dir = root.join("bin");
    let metadata =
        std::fs::symlink_metadata(&bin_dir).map_err(|error| Error::io(&bin_dir, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::other(format!(
            "native tool provider did not create a regular bin directory at {}",
            bin_dir.display()
        )));
    }
    let mut manifest_bins = Vec::new();
    let mut receipt_bins = Vec::new();
    for entry in std::fs::read_dir(&bin_dir).map_err(|error| Error::io(&bin_dir, error))? {
        let entry = entry.map_err(|error| Error::io(&bin_dir, error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| Error::io(&path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::other(format!(
                "native tool bin entry is not a regular file: {}",
                path.display()
            )));
        }
        if !is_native_executable(&path, &metadata) {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| Error::other("native tool binary name is not valid UTF-8"))?;
        let name = executable_stem(file_name)?;
        let relative = format!("bin/{file_name}");
        manifest_bins.push(DynamicToolBin {
            name,
            path: relative.clone(),
        });
        receipt_bins.push(NativeToolBinReceipt {
            path: relative,
            size: metadata.len(),
            sha256: crate::pipeline::verify::hash_file(&path, HashAlgo::Sha256)?,
        });
    }
    manifest_bins.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
    receipt_bins.sort_by(|left, right| left.path.cmp(&right.path));
    if manifest_bins.is_empty() {
        return Err(Error::other(
            "native tool provider did not publish any executable",
        ));
    }
    let mut names = BTreeSet::new();
    for bin in &manifest_bins {
        if !names.insert(portable_path_key(&bin.name)) {
            return Err(Error::other(
                "native tool provider published duplicate executable names",
            ));
        }
    }
    Ok((manifest_bins, receipt_bins))
}

fn write_receipt(root: &Path, receipt: &NativeToolReceipt) -> Result<()> {
    validate_receipt(receipt)?;
    let path = receipt_path(root);
    let bytes = serde_json::to_vec_pretty(receipt)?;
    std::fs::write(&path, bytes).map_err(|error| Error::io(path, error))
}

fn metadata_seal_path(locator: &InstallLocator) -> PathBuf {
    locator.install_root().with_extension(
        NATIVE_TOOL_SEAL_SUFFIX
            .strip_prefix('.')
            .expect("seal suffix starts with a dot"),
    )
}

fn content_digest(install_root: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-native-install-metadata-v1");
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(install_root).follow_links(false) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        if entry.file_type().is_symlink() {
            return Err(Error::other(format!(
                "native tool content contains a forbidden symlink: {}",
                entry.path().display()
            )));
        }
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(install_root)
                .map_err(|_| Error::other("native tool content escaped its root"))?
                .to_path_buf();
            files.push((relative, entry.into_path()));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, path) in files {
        let relative = relative.to_string_lossy().replace('\\', "/");
        hasher.update(&(relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        let digest = crate::pipeline::verify::hash_file(&path, HashAlgo::Sha256)?;
        hasher.update(digest.as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn write_metadata_seal(locator: &InstallLocator, install_root: &Path) -> Result<()> {
    let path = metadata_seal_path(locator);
    let seal = NativeToolSeal {
        schema: 1,
        install_id: locator.identity().install_id.clone(),
        content_blake3: content_digest(install_root)?,
    };
    let bytes = serde_json::to_vec_pretty(&seal)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| Error::io(&path, error))?;
    use std::io::Write as _;
    file.write_all(&bytes)
        .map_err(|error| Error::io(&path, error))?;
    file.sync_all().map_err(|error| Error::io(&path, error))
}

fn validate_metadata_seal(locator: &InstallLocator) -> Result<()> {
    let path = metadata_seal_path(locator);
    let bytes = crate::inventory::read_stable_regular_file(&path, MAX_NATIVE_TOOL_RECEIPT_BYTES)
        .map_err(|error| Error::io(&path, error))?;
    let seal: NativeToolSeal = serde_json::from_slice(&bytes)?;
    let actual = content_digest(locator.install_root())?;
    if seal.schema != 1
        || seal.install_id != locator.identity().install_id
        || seal.content_blake3 != actual
    {
        return Err(Error::other(format!(
            "native tool metadata seal mismatch at {}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_metadata_seal(locator: &InstallLocator) -> Result<bool> {
    let path = metadata_seal_path(locator);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io(path, error)),
    }
}

fn validate_receipt(receipt: &NativeToolReceipt) -> Result<()> {
    if receipt.schema != NATIVE_TOOL_RECEIPT_SCHEMA {
        return Err(Error::config(format!(
            "unsupported native tool receipt schema `{}`",
            receipt.schema
        )));
    }
    validate_runtime_dependency(&receipt.runtime)?;
    if receipt.bins.is_empty() {
        return Err(Error::config("native tool receipt has no binaries"));
    }
    let mut previous = None;
    let mut portable_paths = BTreeSet::new();
    for bin in &receipt.bins {
        validate_relative_bin_path(&bin.path)?;
        validate_sha256(&bin.sha256)?;
        if previous.is_some_and(|path| path >= bin.path.as_str()) {
            return Err(Error::config(
                "native tool receipt binaries are not canonical",
            ));
        }
        previous = Some(bin.path.as_str());
        if !portable_paths.insert(portable_path_key(&bin.path)) {
            return Err(Error::config(
                "native tool receipt contains a case-insensitive binary path collision",
            ));
        }
    }
    Ok(())
}

fn checked_bin_path(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_bin_path(relative)?;
    let path = root.join(relative);
    let canonical_root = dunce::canonicalize(root).map_err(|error| Error::io(root, error))?;
    let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(Error::config(format!(
            "native tool binary escapes install root: `{relative}`"
        )));
    }
    Ok(path)
}

fn validate_relative_bin_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    let mut components = path.components();
    if components.next() != Some(std::path::Component::Normal("bin".as_ref()))
        || !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || value.contains('\\')
    {
        return Err(Error::config(format!(
            "native tool binary path must be `bin/<name>`: `{value}`"
        )));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::config(format!(
                "native tool binary path is not valid UTF-8: `{value}`"
            ))
        })?;
    validate_portable_filename(name)?;
    Ok(())
}

fn validate_portable_filename(value: &str) -> Result<()> {
    let trimmed = value.trim_end_matches([' ', '.']);
    let device = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = device.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && matches!(&upper[..3], "COM" | "LPT")
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if trimmed.is_empty()
        || trimmed != value
        || reserved
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return Err(Error::config(format!(
            "native tool binary name is not portable: `{value}`"
        )));
    }
    Ok(())
}

fn portable_path_key(value: &str) -> String {
    value.replace('\\', "/").to_lowercase()
}

fn create_directory_chain_no_symlinks(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        ) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(Error::io(&current, error)),
                }
                std::fs::symlink_metadata(&current).map_err(|error| Error::io(&current, error))?
            }
            Err(error) => return Err(Error::io(&current, error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::other(format!(
                "native tool install parent is not a regular directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn publish_directory_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let source_bytes = source.as_os_str().as_bytes();
    let destination_bytes = destination.as_os_str().as_bytes();
    let source_c = std::ffi::CString::new(source_bytes)
        .map_err(|_| Error::config("native tool staging path contains NUL"))?;
    let destination_c = std::ffi::CString::new(destination_bytes)
        .map_err(|_| Error::config("native tool install path contains NUL"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source_c.as_ptr(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(Error::io(destination, error))
}

#[cfg(target_os = "macos")]
fn publish_directory_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let source_c = std::ffi::CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::config("native tool staging path contains NUL"))?;
    let destination_c = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| Error::config("native tool install path contains NUL"))?;
    let result =
        unsafe { libc::renamex_np(source_c.as_ptr(), destination_c.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(Error::io(destination, std::io::Error::last_os_error()))
    }
}

#[cfg(windows)]
fn publish_directory_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

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
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(Error::io(destination, std::io::Error::last_os_error()))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn publish_directory_no_replace(_source: &Path, destination: &Path) -> Result<()> {
    Err(Error::other(format!(
        "atomic no-replace publication is unsupported for native tools on this platform: {}",
        destination.display()
    )))
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::config("native tool binary has an invalid SHA-256"));
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(Error::config(
            "native tool binary SHA-256 must be lowercase",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn is_native_executable(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_native_executable(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

fn executable_stem(file_name: &str) -> Result<String> {
    validate_portable_filename(file_name)?;
    #[cfg(windows)]
    let file_name =
        if file_name.len() > 4 && file_name[file_name.len() - 4..].eq_ignore_ascii_case(".exe") {
            &file_name[..file_name.len() - 4]
        } else {
            return Err(Error::config(
                "native tool executable must use the .exe extension",
            ));
        };
    if file_name.is_empty() {
        return Err(Error::config("native tool executable name is empty"));
    }
    Ok(file_name.to_string())
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Dirs {
        Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap()
    }

    fn lifecycle(root: &Path, runtime_version: &str) -> NativeToolLifecycle {
        let dirs = dirs(root);
        write_runtime(&dirs, runtime_version);
        let runtime_identity =
            runtime_tree_identity(&dirs.install_path("go", runtime_version)).unwrap();
        let identity = InstallIdentity::new(
            "npm:fixture",
            "1.2.3",
            Platform::current().to_string(),
            InstallScope::Isolated,
            &BTreeMap::new(),
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "go".into(),
                version: runtime_version.into(),
                identity: Some(runtime_identity),
            }],
            BTreeMap::new(),
        )
        .unwrap();
        NativeToolLifecycle {
            locator: InstallLocator::new(&dirs, identity).unwrap(),
            family: NativeToolFamily::Go,
        }
    }

    fn cargo_lifecycle(root: &Path, runtime_version: &str) -> NativeToolLifecycle {
        let dirs = dirs(root);
        let runtime_root = dirs.rustup_home().join("toolchains").join(runtime_version);
        std::fs::create_dir_all(&runtime_root).unwrap();
        std::fs::write(runtime_root.join("runtime"), b"rust").unwrap();
        let runtime_identity = runtime_tree_identity(&runtime_root).unwrap();
        let identity = InstallIdentity::new(
            "github:example/ripgrep",
            "14.1.1",
            Platform::current().to_string(),
            InstallScope::Isolated,
            &BTreeMap::new(),
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "rust".into(),
                version: runtime_version.into(),
                identity: Some(runtime_identity),
            }],
            BTreeMap::new(),
        )
        .unwrap();
        NativeToolLifecycle {
            locator: InstallLocator::new(&dirs, identity).unwrap(),
            family: NativeToolFamily::Cargo,
        }
    }

    fn write_runtime(dirs: &Dirs, version: &str) {
        let root = dirs.install_path("go", version);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    fn write_runtime_with_identity(dirs: &Dirs, version: &str, identity: &str) {
        let root = dirs.install_path("go", version);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        std::fs::write(root.join("runtime"), identity).unwrap();
    }

    fn write_executable(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn new_stage(lifecycle: &NativeToolLifecycle) -> NativeToolStage {
        let lock = crate::lock::FileLock::acquire(lifecycle.locator.lock_path()).unwrap();
        lifecycle.stage_with_lock(lock).unwrap()
    }

    #[test]
    fn publishes_and_validates_identity_runtime_and_bin_hashes() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.23.4");
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        let executable = stage.bin_dir().join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        write_executable(&executable, b"native fixture");
        let root = stage.publish(NativeToolProvider::GoInstall).unwrap();

        assert!(lifecycle.validate_complete(&dirs).unwrap());
        let receipt = load_receipt(&root).unwrap();
        assert_eq!(receipt.runtime.version, "1.23.4");
        assert_eq!(receipt.bins.len(), 1);
        assert_eq!(receipt.bins[0].size, 14);
        assert_eq!(receipt.bins[0].sha256.len(), 64);
        assert_eq!(
            list_installed(
                &dirs,
                Platform::current(),
                NativeToolFamily::Go,
                "npm:fixture",
            )
            .unwrap(),
            vec!["1.2.3"]
        );
    }

    #[test]
    fn runtime_version_changes_the_install_root() {
        let temporary = tempfile::tempdir().unwrap();
        let first = lifecycle(temporary.path(), "1.22.0");
        let second = lifecycle(temporary.path(), "1.23.0");
        assert_ne!(first.install_root(), second.install_root());
        assert_ne!(first.identity().install_id, second.identity().install_id);
    }

    #[test]
    fn failed_or_abandoned_stage_is_removed_without_publishing() {
        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        let stage_root = stage.path().to_path_buf();
        drop(stage);
        assert!(!stage_root.exists());
        assert!(!lifecycle.install_root().exists());

        let stage = new_stage(&lifecycle);
        let error = stage.publish(NativeToolProvider::GoInstall).unwrap_err();
        assert!(error.to_string().contains("bin"), "{error}");
        assert!(!lifecycle.install_root().exists());
    }

    #[test]
    fn reset_clears_first_provider_output_without_releasing_the_stage() {
        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = cargo_lifecycle(temporary.path(), "1.91.1");
        let mut stage = new_stage(&lifecycle);
        let first = stage.path().join("partial");
        std::fs::write(&first, b"binstall partial").unwrap();
        stage.reset().unwrap();
        assert!(stage.path().is_dir());
        assert!(!first.exists());
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"cargo fallback",
        );
        assert!(stage
            .publish(NativeToolProvider::CargoInstall)
            .unwrap()
            .is_dir());
    }

    #[test]
    fn provider_set_accepts_only_the_tool_namespace() {
        let temporary = tempfile::tempdir().unwrap();
        let cargo = cargo_lifecycle(temporary.path(), "1.91.1");
        let stage = new_stage(&cargo);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        assert!(stage
            .publish(NativeToolProvider::CargoBinstall)
            .unwrap()
            .is_dir());

        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        let error = stage
            .publish(NativeToolProvider::CargoBinstall)
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn tampered_receipt_fails_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.23.4");
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        let root = stage.publish(NativeToolProvider::GoInstall).unwrap();
        let mut receipt = load_receipt(&root).unwrap();
        receipt.bins[0].sha256 = "0".repeat(64);
        std::fs::write(receipt_path(&root), serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(lifecycle.validate_complete(&dirs).is_err());
    }

    #[test]
    fn tampered_seal_fails_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        stage.publish(NativeToolProvider::GoInstall).unwrap();
        std::fs::write(lifecycle.metadata_seal_path(), b"{}").unwrap();
        assert!(lifecycle.validate_complete(&dirs).is_err());
    }

    #[test]
    fn jointly_rewritten_root_metadata_is_rejected_by_adjacent_seal() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        let root = stage.publish(NativeToolProvider::GoInstall).unwrap();
        let installed_bin = root.join("bin").join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        std::fs::write(&installed_bin, b"replacement").unwrap();
        let mut receipt = load_receipt(&root).unwrap();
        receipt.bins[0].size = 11;
        receipt.bins[0].sha256 =
            crate::pipeline::verify::hash_file(&installed_bin, HashAlgo::Sha256).unwrap();
        std::fs::write(
            receipt_path(&root),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        // The attacker can rewrite every file under the install root, but the
        // adjacent identity-qualified seal still commits to the original bytes.
        assert!(lifecycle.validate_complete(&dirs).is_err());
        assert!(lifecycle.metadata_seal_path().is_file());
    }

    #[test]
    fn unlisted_payload_file_is_bound_by_the_adjacent_seal() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        let root = stage.publish(NativeToolProvider::GoInstall).unwrap();
        std::fs::write(root.join("injected"), b"payload").unwrap();
        assert!(lifecycle.validate_complete(&dirs).is_err());
    }

    #[test]
    fn native_runtime_identity_is_mandatory() {
        let temporary = tempfile::tempdir().unwrap();
        let error = NativeToolLifecycle::new(
            &dirs(temporary.path()),
            Platform::current(),
            "npm:fixture",
            "1.2.3",
            &BTreeMap::new(),
            NativeToolFamily::Go,
            InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "go".into(),
                version: "1.23.4".into(),
                identity: None,
            },
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exact managed runtime"));
    }

    #[test]
    fn family_validation_rejects_malformed_native_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        let runtime_root = dirs.install_path("go", "1.23.4");
        std::fs::create_dir_all(&runtime_root).unwrap();
        std::fs::write(runtime_root.join(".osdk-complete"), b"").unwrap();
        let runtime_identity = runtime_tree_identity(&runtime_root).unwrap();
        for tool in [
            "go:exa$mple.com/tool",
            "go:-example.com/tool",
            "go:example!.com/tool",
        ] {
            let error = NativeToolLifecycle::new(
                &dirs,
                Platform::current(),
                tool,
                "1.2.3",
                &BTreeMap::new(),
                NativeToolFamily::Go,
                InstallDependency {
                    kind: InstallDependencyKind::Runtime,
                    id: "go".into(),
                    version: "1.23.4".into(),
                    identity: Some(runtime_identity.clone()),
                },
                BTreeMap::new(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("invalid canonical"), "{error}");
        }
    }

    #[test]
    fn tampered_bin_and_missing_runtime_fail_reuse() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.23.4");
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        let executable = stage.bin_dir().join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        write_executable(&executable, b"original");
        let root = stage.publish(NativeToolProvider::GoInstall).unwrap();
        std::fs::write(
            &executable.with_file_name(executable.file_name().unwrap()),
            b"bad",
        )
        .ok();
        let installed_bin = root.join("bin").join(executable.file_name().unwrap());
        std::fs::write(&installed_bin, b"tampered").unwrap();
        assert!(lifecycle.validate_complete(&dirs).is_err());

        std::fs::write(&installed_bin, b"original").unwrap();
        std::fs::remove_file(dirs.install_path("go", "1.23.4").join(".osdk-complete")).unwrap();
        assert!(!lifecycle.validate_complete(&dirs).unwrap());
    }

    #[test]
    fn bound_runtime_identity_must_still_match() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime_with_identity(&dirs, "1.23.4", "runtime-a");
        let expected_identity = hash_runtime_tree(&dirs.install_path("go", "1.23.4")).unwrap();
        let identity = InstallIdentity::new(
            "npm:fixture",
            "1.2.3",
            Platform::current().to_string(),
            InstallScope::Isolated,
            &BTreeMap::new(),
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "go".into(),
                version: "1.23.4".into(),
                identity: Some(expected_identity),
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let lifecycle = NativeToolLifecycle {
            locator: InstallLocator::new(&dirs, identity).unwrap(),
            family: NativeToolFamily::Go,
        };
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        stage.publish(NativeToolProvider::GoInstall).unwrap();
        assert!(lifecycle.validate_complete(&dirs).unwrap());

        std::fs::write(
            dirs.install_path("go", "1.23.4").join("runtime"),
            "runtime-b",
        )
        .unwrap();
        assert!(!lifecycle.validate_complete(&dirs).unwrap());
    }

    #[tokio::test]
    async fn prepare_serializes_same_identity_and_reuses_without_a_second_stage() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.23.4");
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let NativeToolPreparation::Staged(first) = lifecycle.prepare(&dirs).await.unwrap() else {
            panic!("first prepare must stage");
        };
        let clone = lifecycle.clone();
        let dirs_clone = dirs.clone();
        let waiter = tokio::spawn(async move { clone.prepare(&dirs_clone).await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        write_executable(
            &first.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"fixture",
        );
        let published = first.publish(NativeToolProvider::GoInstall).unwrap();
        let NativeToolPreparation::Reused(reused) = waiter.await.unwrap().unwrap() else {
            panic!("second prepare must reuse");
        };
        assert_eq!(published, reused);
    }

    #[tokio::test]
    async fn uninstall_removes_only_the_exact_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.22.0");
        write_runtime(&dirs, "1.23.0");
        let first = lifecycle(temporary.path(), "1.22.0");
        let second = lifecycle(temporary.path(), "1.23.0");
        for lifecycle in [&first, &second] {
            let stage = new_stage(lifecycle);
            write_executable(
                &stage.bin_dir().join(if cfg!(windows) {
                    "fixture.exe"
                } else {
                    "fixture"
                }),
                lifecycle.identity().install_id.as_bytes(),
            );
            stage.publish(NativeToolProvider::GoInstall).unwrap();
        }
        assert!(first.uninstall().await.unwrap());
        assert!(!first.install_root().exists());
        assert!(second.install_root().exists());
    }

    #[cfg(unix)]
    #[test]
    fn publish_rejects_case_collisions_and_reserved_windows_names() {
        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(&stage.bin_dir().join("Tool"), b"one");
        write_executable(&stage.bin_dir().join("tool"), b"two");
        let error = stage.publish(NativeToolProvider::GoInstall).unwrap_err();
        assert!(error.to_string().contains("duplicate executable"));

        let stage = new_stage(&lifecycle);
        write_executable(&stage.bin_dir().join("CON"), b"bad");
        let error = stage.publish(NativeToolProvider::GoInstall).unwrap_err();
        assert!(error.to_string().contains("not portable"));
    }

    #[test]
    fn publishing_never_replaces_an_existing_final_root() {
        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let stage = new_stage(&lifecycle);
        write_executable(
            &stage.bin_dir().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }),
            b"new",
        );
        std::fs::create_dir_all(lifecycle.install_root()).unwrap();
        std::fs::write(lifecycle.install_root().join("sentinel"), b"old").unwrap();

        let error = stage.publish(NativeToolProvider::GoInstall).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(
            std::fs::read(lifecycle.install_root().join("sentinel")).unwrap(),
            b"old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_symlinked_install_ancestor() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = lifecycle(temporary.path(), "1.23.4");
        let tool_root = lifecycle
            .install_root()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(tool_root.parent().unwrap()).unwrap();
        symlink(&outside, &tool_root).unwrap();

        let lock = crate::lock::FileLock::acquire(lifecycle.locator.lock_path()).unwrap();
        let error = lifecycle.stage_with_lock(lock).unwrap_err();
        assert!(error.to_string().contains("regular directory"), "{error}");
    }
}
