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
const RUST_RUNTIME_RECEIPT_FILE: &str = ".osdk-rust-runtime-receipt.json";
const RUST_RUNTIME_RECEIPT_SCHEMA: u32 = 1;
const MAX_RUST_RUNTIME_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;
const GO_RUNTIME_RECEIPT_FILE: &str = ".osdk-go-runtime-receipt.json";
const GO_RUNTIME_RECEIPT_SCHEMA: u32 = 1;
const MAX_GO_RUNTIME_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GO_RUNTIME_FILES: usize = 65_536;
const MAX_GO_RUNTIME_PATH_BYTES: usize = 4 * 1024;
const MAX_RUSTC_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RUST_RUNTIME_FILES: usize = 65_536;
const MAX_RUST_RUNTIME_PATH_BYTES: usize = 4 * 1024;
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

/// Stable content identity for a bounded runtime root. Prefer the cached,
/// layout-aware [`go_runtime_identity`] and [`rust_runtime_identity`] helpers
/// for managed compiler runtimes.
pub fn runtime_tree_identity(root: &Path) -> Result<String> {
    hash_runtime_tree(root)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoRuntimeFileReceipt {
    path: String,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoRuntimeReceipt {
    schema: u32,
    version: String,
    platform: String,
    files: Vec<GoRuntimeFileReceipt>,
    identity: String,
    integrity_blake3: String,
}

#[derive(Debug)]
struct GoRuntimeFile {
    receipt: GoRuntimeFileReceipt,
    absolute: PathBuf,
}

/// Content identity for an exact managed Go SDK.
///
/// The inventory covers the executable toolchain (`bin` and `pkg/tool`), the
/// standard-library sources, runtime libraries, and the runtime version/env
/// files used by `go install`. Symlinks and non-regular payloads fail closed.
/// An integrity-checked receipt caches file hashes; unchanged path/size/mtime
/// metadata avoids re-reading the entire SDK during every activation or shim
/// validation. The receipt itself and other osdk metadata are outside the
/// payload inventory.
pub fn go_runtime_identity(dirs: &Dirs, platform: Platform, version: &str) -> Result<String> {
    let root = dirs.install_path("go", version);
    validate_managed_go_root(&root, platform, version)?;
    let _lock = crate::lock::FileLock::acquire(go_runtime_receipt_lock_path(dirs, version))?;
    validate_managed_go_root(&root, platform, version)?;

    let files = collect_go_runtime_files(&root, &dirs.store, platform)?;
    let receipt_path = root.join(GO_RUNTIME_RECEIPT_FILE);
    match std::fs::symlink_metadata(&receipt_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::io(&receipt_path, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(Error::other(format!(
                "managed Go runtime receipt is not a regular non-symlink file: {}",
                receipt_path.display()
            )));
        }
        Ok(_) => {
            let receipt = load_go_runtime_receipt(&receipt_path)?;
            validate_go_runtime_receipt(&receipt, version, &platform.to_string())?;
            let current = files
                .iter()
                .map(|file| file.receipt.clone())
                .collect::<Vec<_>>();
            if receipt.files == current {
                return Ok(receipt.identity);
            }
        }
    }

    let platform_name = platform.to_string();
    let identity = hash_go_runtime_files(version, &platform_name, &files)?;
    let after = collect_go_runtime_files(&root, &dirs.store, platform)?;
    let before_metadata = files
        .iter()
        .map(|file| file.receipt.clone())
        .collect::<Vec<_>>();
    let after_metadata = after
        .iter()
        .map(|file| file.receipt.clone())
        .collect::<Vec<_>>();
    if before_metadata != after_metadata {
        return Err(Error::other(format!(
            "managed Go runtime changed while its identity was being computed: {}",
            root.display()
        )));
    }
    validate_managed_go_root(&root, platform, version)?;

    let mut receipt = GoRuntimeReceipt {
        schema: GO_RUNTIME_RECEIPT_SCHEMA,
        version: version.to_string(),
        platform: platform_name,
        files: before_metadata,
        identity: identity.clone(),
        integrity_blake3: String::new(),
    };
    receipt.integrity_blake3 = go_runtime_receipt_integrity(&receipt);
    write_go_runtime_receipt_atomic(&receipt_path, &receipt)?;
    Ok(identity)
}

fn validate_managed_go_root(root: &Path, platform: Platform, version: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(root).map_err(|error| Error::io(root, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::other(format!(
            "managed Go runtime root is not a regular directory: {}",
            root.display()
        )));
    }
    if !is_regular_file(&root.join(".osdk-complete")) {
        return Err(Error::config(format!(
            "Go runtime `{version}` is not a complete osdk-managed runtime"
        )));
    }
    for name in ["go", "gofmt"] {
        let path = root
            .join("bin")
            .join(format!("{name}{}", platform.os.exe_suffix()));
        if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            return Err(Error::other(format!(
                "managed Go runtime `{version}` is missing {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn go_runtime_receipt_lock_path(dirs: &Dirs, version: &str) -> PathBuf {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-go-runtime-receipt-lock-v1");
    hash_identity_value(&mut hasher, version.as_bytes());
    dirs.lock_dir("go")
        .join(format!("runtime-{}.lock", hasher.finalize().to_hex()))
}

fn collect_go_runtime_files(
    root: &Path,
    store: &Path,
    platform: Platform,
) -> Result<Vec<GoRuntimeFile>> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for name in ["VERSION", "go.env"] {
        let path = root.join(name);
        if name == "VERSION" || path.exists() {
            insert_go_runtime_file(root, store, &path, &mut paths)?;
        }
    }
    for directory in ["bin", "pkg", "src"] {
        collect_go_runtime_tree(root, store, &root.join(directory), &mut paths, true)?;
    }
    for directory in ["lib", "misc"] {
        let path = root.join(directory);
        if path.exists() {
            collect_go_runtime_tree(root, store, &path, &mut paths, false)?;
        }
    }
    for name in ["go", "gofmt"] {
        insert_go_runtime_file(
            root,
            store,
            &root
                .join("bin")
                .join(format!("{name}{}", platform.os.exe_suffix())),
            &mut paths,
        )?;
    }
    if paths.len() > MAX_GO_RUNTIME_FILES {
        return Err(Error::other(format!(
            "managed Go identity exceeds the {MAX_GO_RUNTIME_FILES} file limit"
        )));
    }
    paths
        .into_iter()
        .map(|(path, absolute)| {
            let (metadata, symlink_target) = go_runtime_file_metadata(root, store, &absolute)?;
            let (modified_seconds, modified_nanoseconds) = modified_parts(&metadata, &absolute)?;
            Ok(GoRuntimeFile {
                receipt: GoRuntimeFileReceipt {
                    path,
                    size: metadata.len(),
                    modified_seconds,
                    modified_nanoseconds,
                    symlink_target,
                },
                absolute,
            })
        })
        .collect()
}

fn collect_go_runtime_tree(
    root: &Path,
    store: &Path,
    directory: &Path,
    paths: &mut BTreeMap<String, PathBuf>,
    require_file: bool,
) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(directory).map_err(|error| Error::io(directory, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::other(format!(
            "managed Go identity directory is unsafe: {}",
            directory.display()
        )));
    }
    let mut found_file = false;
    for entry in walkdir::WalkDir::new(directory).follow_links(false) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        if entry.file_type().is_symlink() {
            go_runtime_file_metadata(root, store, entry.path())?;
            insert_go_runtime_file(root, store, entry.path(), paths)?;
            found_file = true;
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(Error::other(format!(
                "managed Go identity contains a non-regular payload: {}",
                entry.path().display()
            )));
        }
        insert_go_runtime_file(root, store, entry.path(), paths)?;
        found_file = true;
        if paths.len() > MAX_GO_RUNTIME_FILES {
            return Err(Error::other(format!(
                "managed Go identity exceeds the {MAX_GO_RUNTIME_FILES} file limit"
            )));
        }
    }
    if require_file && !found_file {
        return Err(Error::other(format!(
            "managed Go runtime directory is empty: {}",
            directory.display()
        )));
    }
    Ok(())
}

fn insert_go_runtime_file(
    root: &Path,
    store: &Path,
    path: &Path,
    paths: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    go_runtime_file_metadata(root, store, path)?;
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::other(format!(
            "managed Go identity path escapes runtime: {}",
            path.display()
        ))
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| Error::config("managed Go identity contains a non-UTF-8 filename"))?
        .replace('\\', "/");
    if relative.is_empty() || relative.len() > MAX_GO_RUNTIME_PATH_BYTES {
        return Err(Error::config(
            "managed Go identity contains an invalid relative path",
        ));
    }
    paths.insert(relative, path.to_path_buf());
    Ok(())
}

fn go_runtime_file_metadata(
    root: &Path,
    store: &Path,
    path: &Path,
) -> Result<(std::fs::Metadata, Option<String>)> {
    let link_metadata = std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))?;
    if !link_metadata.file_type().is_symlink() {
        if !link_metadata.is_file() {
            return Err(Error::other(format!(
                "managed Go identity path is not a regular file: {}",
                path.display()
            )));
        }
        return Ok((link_metadata, None));
    }

    let target = std::fs::read_link(path).map_err(|error| Error::io(path, error))?;
    let resolved = if target.is_absolute() {
        target.clone()
    } else {
        path.parent()
            .ok_or_else(|| Error::other("managed Go symlink has no parent"))?
            .join(&target)
    };
    let canonical = dunce::canonicalize(&resolved).map_err(|error| Error::io(&resolved, error))?;
    let inside_runtime = canonical.starts_with(root);
    let inside_store = dunce::canonicalize(store)
        .ok()
        .is_some_and(|canonical_store| canonical.starts_with(canonical_store));
    if !inside_runtime && !inside_store {
        return Err(Error::other(format!(
            "managed Go identity symlink escapes osdk-controlled storage: {}",
            path.display()
        )));
    }
    let metadata = std::fs::metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_file() {
        return Err(Error::other(format!(
            "managed Go identity symlink does not resolve to a regular file: {}",
            path.display()
        )));
    }
    let target = target
        .to_str()
        .ok_or_else(|| Error::config("managed Go identity contains a non-UTF-8 symlink"))?
        .replace('\\', "/");
    if target.len() > MAX_GO_RUNTIME_PATH_BYTES {
        return Err(Error::config(
            "managed Go identity contains an overlong symlink target",
        ));
    }
    Ok((metadata, Some(target)))
}

fn hash_go_runtime_files(version: &str, platform: &str, files: &[GoRuntimeFile]) -> Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-go-runtime-essential-v1");
    hash_identity_value(&mut hasher, version.as_bytes());
    hash_identity_value(&mut hasher, platform.as_bytes());
    hash_identity_value(&mut hasher, &(files.len() as u64).to_le_bytes());
    for file in files {
        hash_identity_value(&mut hasher, file.receipt.path.as_bytes());
        hash_identity_value(
            &mut hasher,
            file.receipt
                .symlink_target
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        let digest = crate::pipeline::verify::hash_file(&file.absolute, HashAlgo::Sha256)?;
        hash_identity_value(&mut hasher, digest.as_bytes());
    }
    Ok(format!("b3-go-v1:{}", hasher.finalize().to_hex()))
}

fn go_runtime_receipt_integrity(receipt: &GoRuntimeReceipt) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-go-runtime-receipt-v1");
    hash_identity_value(&mut hasher, &receipt.schema.to_le_bytes());
    hash_identity_value(&mut hasher, receipt.version.as_bytes());
    hash_identity_value(&mut hasher, receipt.platform.as_bytes());
    hash_identity_value(&mut hasher, &(receipt.files.len() as u64).to_le_bytes());
    for file in &receipt.files {
        hash_identity_value(&mut hasher, file.path.as_bytes());
        hash_identity_value(&mut hasher, &file.size.to_le_bytes());
        hash_identity_value(&mut hasher, &file.modified_seconds.to_le_bytes());
        hash_identity_value(&mut hasher, &file.modified_nanoseconds.to_le_bytes());
        hash_identity_value(
            &mut hasher,
            file.symlink_target
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    hash_identity_value(&mut hasher, receipt.identity.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn load_go_runtime_receipt(path: &Path) -> Result<GoRuntimeReceipt> {
    let bytes = crate::inventory::read_stable_regular_file(path, MAX_GO_RUNTIME_RECEIPT_BYTES)
        .map_err(|error| Error::io(path, error))?;
    let receipt: GoRuntimeReceipt = serde_json::from_slice(&bytes)?;
    if receipt.schema != GO_RUNTIME_RECEIPT_SCHEMA
        || receipt.files.is_empty()
        || receipt.files.len() > MAX_GO_RUNTIME_FILES
        || receipt
            .identity
            .strip_prefix("b3-go-v1:")
            .is_none_or(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
        || receipt.integrity_blake3 != go_runtime_receipt_integrity(&receipt)
    {
        return Err(Error::config(format!(
            "managed Go runtime receipt is invalid at {}",
            path.display()
        )));
    }
    Ok(receipt)
}

fn validate_go_runtime_receipt(
    receipt: &GoRuntimeReceipt,
    version: &str,
    platform: &str,
) -> Result<()> {
    if receipt.version != version || receipt.platform != platform {
        return Err(Error::config(
            "managed Go runtime receipt does not match the selected runtime",
        ));
    }
    let mut previous = None;
    for file in &receipt.files {
        if file.path.is_empty()
            || file.path.len() > MAX_GO_RUNTIME_PATH_BYTES
            || file.modified_nanoseconds >= 1_000_000_000
            || previous.is_some_and(|path: &str| path >= file.path.as_str())
            || Path::new(&file.path).is_absolute()
            || Path::new(&file.path)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || file
                .symlink_target
                .as_deref()
                .is_some_and(|target| target.is_empty() || target.len() > MAX_GO_RUNTIME_PATH_BYTES)
        {
            return Err(Error::config(
                "managed Go runtime receipt contains an invalid file inventory",
            ));
        }
        previous = Some(file.path.as_str());
    }
    Ok(())
}

fn write_go_runtime_receipt_atomic(path: &Path, receipt: &GoRuntimeReceipt) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(receipt)?;
    if bytes.len() as u64 > MAX_GO_RUNTIME_RECEIPT_BYTES {
        return Err(Error::other(
            "managed Go runtime receipt exceeds its size limit",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::other(format!("path has no parent: {}", path.display())))?;
    let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{GO_RUNTIME_RECEIPT_FILE}.tmp-{}-{serial}",
        std::process::id()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        use std::io::Write as _;
        file.write_all(&bytes)
            .map_err(|error| Error::io(&temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io(&temporary, error))?;
        atomic_replace_runtime_receipt(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustRuntimeFileReceipt {
    path: String,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustRuntimeReceipt {
    schema: u32,
    version: String,
    platform: String,
    toolchain_root: String,
    files: Vec<RustRuntimeFileReceipt>,
    identity: String,
    integrity_blake3: String,
}

#[derive(Debug)]
struct RustRuntimeFile {
    receipt: RustRuntimeFileReceipt,
    absolute: PathBuf,
}

/// Content identity for the build-critical portion of an exact managed Rust
/// toolchain. The compiler payload comes from rustup's `manifest-rustc-*`, and
/// every regular file below the selected target's `lib` directory is included.
/// Symlinks and non-regular payloads fail closed.
///
/// Hashing a target sysroot can read hundreds of MiB, so the managed runtime's
/// adjacent marker directory holds an atomic receipt. Its full sorted
/// path/size/mtime inventory is checked on every call; unchanged metadata lets
/// us reuse the content identity, while any drift triggers a full rehash. The
/// receipt is an integrity-checked cache for osdk-managed, immutable runtimes,
/// not a same-user security boundary (a process able to rewrite both payload
/// timestamps and osdk state is outside this boundary).
pub fn rust_runtime_identity(dirs: &Dirs, platform: Platform, version: &str) -> Result<String> {
    let marker = dirs.install_path("rust", version);
    validate_managed_rust_marker(&marker, version)?;
    let lock_path = rust_runtime_receipt_lock_path(dirs, version);
    let _lock = crate::lock::FileLock::acquire(&lock_path)?;
    validate_managed_rust_marker(&marker, version)?;

    let root =
        crate::backend::rust::RustBackend::exact_toolchain_dir_for_dirs(dirs, platform, version)
            .ok_or_else(|| Error::NotInstalled {
                tool: "rust".into(),
                version: version.into(),
            })?;
    let root_metadata =
        std::fs::symlink_metadata(&root).map_err(|error| Error::io(&root, error))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(Error::other(format!(
            "managed Rust toolchain root is not a regular directory: {}",
            root.display()
        )));
    }
    let canonical_root = dunce::canonicalize(&root).map_err(|error| Error::io(&root, error))?;
    validate_rust_runtime_directory_path(&canonical_root, &canonical_root.join("bin"))?;
    let root_name = canonical_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::config("managed Rust toolchain path is not valid UTF-8"))?;
    let platform_name = platform.to_string();
    let files = collect_rust_runtime_files(&canonical_root, platform)?;
    let receipt_path = marker.join(RUST_RUNTIME_RECEIPT_FILE);
    match std::fs::symlink_metadata(&receipt_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::io(&receipt_path, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(Error::other(format!(
                "managed Rust runtime receipt is not a regular non-symlink file: {}",
                receipt_path.display()
            )));
        }
        Ok(_) => {
            let receipt = load_rust_runtime_receipt(&receipt_path)?;
            validate_rust_runtime_receipt(&receipt, version, &platform_name, root_name)?;
            let current = files
                .iter()
                .map(|file| file.receipt.clone())
                .collect::<Vec<_>>();
            if receipt.files == current {
                return Ok(receipt.identity);
            }
        }
    }

    let identity = hash_rust_runtime_files(version, &platform_name, root_name, &files)?;
    let after = collect_rust_runtime_files(&canonical_root, platform)?;
    let before_metadata = files
        .iter()
        .map(|file| file.receipt.clone())
        .collect::<Vec<_>>();
    let after_metadata = after
        .iter()
        .map(|file| file.receipt.clone())
        .collect::<Vec<_>>();
    if before_metadata != after_metadata {
        return Err(Error::other(format!(
            "managed Rust runtime changed while its identity was being computed: {}",
            canonical_root.display()
        )));
    }
    validate_managed_rust_marker(&marker, version)?;

    let mut receipt = RustRuntimeReceipt {
        schema: RUST_RUNTIME_RECEIPT_SCHEMA,
        version: version.to_string(),
        platform: platform_name,
        toolchain_root: root_name.to_string(),
        files: before_metadata,
        identity: identity.clone(),
        integrity_blake3: String::new(),
    };
    receipt.integrity_blake3 = rust_runtime_receipt_integrity(&receipt);
    write_rust_runtime_receipt_atomic(&receipt_path, &receipt)?;
    Ok(identity)
}

fn validate_managed_rust_marker(marker: &Path, version: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(marker).map_err(|error| Error::io(marker, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::other(format!(
            "managed Rust marker is not a regular directory: {}",
            marker.display()
        )));
    }
    if !is_regular_file(&marker.join(".osdk-complete"))
        || std::fs::symlink_metadata(marker.join(".osdk-linked")).is_ok()
    {
        return Err(Error::config(format!(
            "Rust runtime `{version}` is not a complete osdk-managed toolchain"
        )));
    }
    Ok(())
}

fn rust_runtime_receipt_lock_path(dirs: &Dirs, version: &str) -> PathBuf {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-rust-runtime-receipt-lock-v1");
    hash_identity_value(&mut hasher, version.as_bytes());
    dirs.lock_dir("rust")
        .join(format!("runtime-{}.lock", hasher.finalize().to_hex()))
}

fn collect_rust_runtime_files(
    canonical_root: &Path,
    platform: Platform,
) -> Result<Vec<RustRuntimeFile>> {
    let bin = canonical_root.join("bin");
    validate_rust_runtime_directory_path(canonical_root, &bin)?;
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for name in ["cargo", "rustc"] {
        let path = bin.join(format!("{name}{}", platform.os.exe_suffix()));
        insert_rust_runtime_file(canonical_root, &path, &mut paths)?;
    }

    let rustlib = canonical_root.join("lib/rustlib");
    validate_rust_runtime_directory_path(canonical_root, &rustlib)?;
    let rustc_manifest = find_rustc_component_manifest(&rustlib, platform)?;
    if let Some(manifest) = rustc_manifest {
        insert_rust_runtime_file(canonical_root, &manifest, &mut paths)?;
        let bytes = crate::inventory::read_stable_regular_file(&manifest, MAX_RUSTC_MANIFEST_BYTES)
            .map_err(|error| Error::io(&manifest, error))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::config("managed Rust rustc manifest is not valid UTF-8"))?;
        for line in text.lines() {
            let relative = line.strip_prefix("file:").ok_or_else(|| {
                Error::config(format!(
                    "managed Rust rustc manifest contains an unsupported entry: {line}"
                ))
            })?;
            let path = checked_rust_manifest_path(canonical_root, relative)?;
            insert_rust_runtime_file(canonical_root, &path, &mut paths)?;
        }
    } else {
        // Minimal fixture toolchains and older layouts may omit rustup's
        // component manifest. Hash the conservative compiler payload instead.
        for name in ["rustdoc", "clippy-driver"] {
            let path = bin.join(format!("{name}{}", platform.os.exe_suffix()));
            if path.exists() {
                insert_rust_runtime_file(canonical_root, &path, &mut paths)?;
            }
        }
        let lib = canonical_root.join("lib");
        collect_rust_runtime_tree(canonical_root, &lib, &mut paths, false)?;
    }

    let target_lib = rustlib.join(platform.llvm_triple()).join("lib");
    collect_rust_runtime_tree(canonical_root, &target_lib, &mut paths, true)?;
    if paths.len() > MAX_RUST_RUNTIME_FILES {
        return Err(Error::other(format!(
            "managed Rust identity exceeds the {MAX_RUST_RUNTIME_FILES} file limit"
        )));
    }

    paths
        .into_iter()
        .map(|(path, absolute)| {
            let metadata = rust_runtime_file_metadata(&absolute)?;
            let (modified_seconds, modified_nanoseconds) = modified_parts(&metadata, &absolute)?;
            Ok(RustRuntimeFile {
                receipt: RustRuntimeFileReceipt {
                    path,
                    size: metadata.len(),
                    modified_seconds,
                    modified_nanoseconds,
                },
                absolute,
            })
        })
        .collect()
}

fn find_rustc_component_manifest(rustlib: &Path, platform: Platform) -> Result<Option<PathBuf>> {
    let canonical_root = rustlib
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::other("managed Rust rustlib path has no toolchain root"))?;
    let exact = rustlib.join(format!("manifest-rustc-{}", platform.llvm_triple()));
    if exact.exists() {
        validate_rust_runtime_file_path(canonical_root, &exact)?;
        return Ok(Some(exact));
    }
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(rustlib).map_err(|error| Error::io(rustlib, error))? {
        let entry = entry.map_err(|error| Error::io(rustlib, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::config(
                "managed Rust rustlib contains a non-UTF-8 filename",
            ));
        };
        if name.starts_with("manifest-rustc-") {
            validate_rust_runtime_file_path(canonical_root, &entry.path())?;
            matches.push(entry.path());
        }
    }
    matches.sort();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(Error::other(format!(
            "managed Rust toolchain has no unambiguous rustc manifest for {}",
            platform.llvm_triple()
        ))),
    }
}

fn checked_rust_manifest_path(canonical_root: &Path, value: &str) -> Result<PathBuf> {
    if value.is_empty()
        || value.len() > MAX_RUST_RUNTIME_PATH_BYTES
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
    {
        return Err(Error::config(
            "managed Rust rustc manifest contains an invalid path",
        ));
    }
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(Error::config(format!(
            "managed Rust rustc manifest contains an unsafe path: {value}"
        )));
    }
    Ok(canonical_root.join(relative))
}

fn collect_rust_runtime_tree(
    canonical_root: &Path,
    directory: &Path,
    paths: &mut BTreeMap<String, PathBuf>,
    require_file: bool,
) -> Result<()> {
    validate_rust_runtime_directory_path(canonical_root, directory)?;
    let mut found_file = false;
    for entry in walkdir::WalkDir::new(directory).follow_links(false) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        if entry.file_type().is_symlink() {
            return Err(Error::other(format!(
                "managed Rust identity contains a forbidden symlink: {}",
                entry.path().display()
            )));
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(Error::other(format!(
                "managed Rust identity contains a non-regular payload: {}",
                entry.path().display()
            )));
        }
        insert_rust_runtime_file(canonical_root, entry.path(), paths)?;
        found_file = true;
        if paths.len() > MAX_RUST_RUNTIME_FILES {
            return Err(Error::other(format!(
                "managed Rust identity exceeds the {MAX_RUST_RUNTIME_FILES} file limit"
            )));
        }
    }
    if require_file && !found_file {
        return Err(Error::other(format!(
            "managed Rust target library is empty: {}",
            directory.display()
        )));
    }
    Ok(())
}

fn insert_rust_runtime_file(
    canonical_root: &Path,
    path: &Path,
    paths: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    validate_rust_runtime_file_path(canonical_root, path)?;
    let canonical = dunce::canonicalize(path).map_err(|error| Error::io(path, error))?;
    let relative = canonical.strip_prefix(canonical_root).map_err(|_| {
        Error::other(format!(
            "managed Rust identity path escapes toolchain: {}",
            path.display()
        ))
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| Error::config("managed Rust identity contains a non-UTF-8 filename"))?;
    let portable = relative.replace('\\', "/");
    if portable.is_empty() || portable.len() > MAX_RUST_RUNTIME_PATH_BYTES {
        return Err(Error::config(
            "managed Rust identity contains an invalid relative path",
        ));
    }
    paths.insert(portable, canonical);
    Ok(())
}

fn validate_rust_runtime_directory_path(canonical_root: &Path, path: &Path) -> Result<()> {
    validate_rust_runtime_path(canonical_root, path, true).map(|_| ())
}

fn validate_rust_runtime_file_path(canonical_root: &Path, path: &Path) -> Result<()> {
    validate_rust_runtime_path(canonical_root, path, false).map(|_| ())
}

fn validate_rust_runtime_path(
    canonical_root: &Path,
    path: &Path,
    expect_directory: bool,
) -> Result<std::fs::Metadata> {
    let relative = path.strip_prefix(canonical_root).map_err(|_| {
        Error::other(format!(
            "managed Rust identity path escapes toolchain: {}",
            path.display()
        ))
    })?;
    let mut current = canonical_root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(Error::other("managed Rust identity path is empty"));
    }
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(Error::other(format!(
                "managed Rust identity path is not canonical: {}",
                path.display()
            )));
        };
        current.push(component);
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|error| Error::io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(Error::other(format!(
                "managed Rust identity contains a forbidden symlink: {}",
                current.display()
            )));
        }
        let final_component = index + 1 == components.len();
        if !final_component && !metadata.is_dir() {
            return Err(Error::other(format!(
                "managed Rust identity path has a non-directory ancestor: {}",
                current.display()
            )));
        }
        if final_component {
            let valid_kind = if expect_directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            };
            if !valid_kind {
                return Err(Error::other(format!(
                    "managed Rust identity path has the wrong file type: {}",
                    current.display()
                )));
            }
            return Ok(metadata);
        }
    }
    Err(Error::other("managed Rust identity path is empty"))
}

fn rust_runtime_file_metadata(path: &Path) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::other(format!(
            "managed Rust identity path is not a regular non-symlink file: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn modified_parts(metadata: &std::fs::Metadata, path: &Path) -> Result<(i64, u32)> {
    let modified = metadata
        .modified()
        .map_err(|error| Error::io(path, error))?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| Error::other("managed Rust file timestamp exceeds i64"))?;
            Ok((seconds, duration.subsec_nanos()))
        }
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| Error::other("managed Rust file timestamp exceeds i64"))?;
            Ok((
                -seconds - i64::from(duration.subsec_nanos() != 0),
                duration.subsec_nanos(),
            ))
        }
    }
}

fn hash_rust_runtime_files(
    version: &str,
    platform: &str,
    root_name: &str,
    files: &[RustRuntimeFile],
) -> Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-rust-runtime-essential-v2");
    hash_identity_value(&mut hasher, version.as_bytes());
    hash_identity_value(&mut hasher, platform.as_bytes());
    hash_identity_value(&mut hasher, root_name.as_bytes());
    hash_identity_value(&mut hasher, &(files.len() as u64).to_le_bytes());
    for file in files {
        hash_identity_value(&mut hasher, file.receipt.path.as_bytes());
        let digest = crate::pipeline::verify::hash_file(&file.absolute, HashAlgo::Sha256)?;
        hash_identity_value(&mut hasher, digest.as_bytes());
    }
    Ok(format!("b3-rust-v2:{}", hasher.finalize().to_hex()))
}

fn rust_runtime_receipt_integrity(receipt: &RustRuntimeReceipt) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-rust-runtime-receipt-v1");
    hash_identity_value(&mut hasher, &receipt.schema.to_le_bytes());
    hash_identity_value(&mut hasher, receipt.version.as_bytes());
    hash_identity_value(&mut hasher, receipt.platform.as_bytes());
    hash_identity_value(&mut hasher, receipt.toolchain_root.as_bytes());
    hash_identity_value(&mut hasher, &(receipt.files.len() as u64).to_le_bytes());
    for file in &receipt.files {
        hash_identity_value(&mut hasher, file.path.as_bytes());
        hash_identity_value(&mut hasher, &file.size.to_le_bytes());
        hash_identity_value(&mut hasher, &file.modified_seconds.to_le_bytes());
        hash_identity_value(&mut hasher, &file.modified_nanoseconds.to_le_bytes());
    }
    hash_identity_value(&mut hasher, receipt.identity.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn load_rust_runtime_receipt(path: &Path) -> Result<RustRuntimeReceipt> {
    let bytes = crate::inventory::read_stable_regular_file(path, MAX_RUST_RUNTIME_RECEIPT_BYTES)
        .map_err(|error| Error::io(path, error))?;
    let receipt: RustRuntimeReceipt = serde_json::from_slice(&bytes)?;
    if receipt.schema != RUST_RUNTIME_RECEIPT_SCHEMA
        || receipt.files.is_empty()
        || receipt.files.len() > MAX_RUST_RUNTIME_FILES
        || receipt
            .identity
            .strip_prefix("b3-rust-v2:")
            .is_none_or(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
        || receipt.integrity_blake3 != rust_runtime_receipt_integrity(&receipt)
    {
        return Err(Error::config(format!(
            "managed Rust runtime receipt is invalid at {}",
            path.display()
        )));
    }
    Ok(receipt)
}

fn validate_rust_runtime_receipt(
    receipt: &RustRuntimeReceipt,
    version: &str,
    platform: &str,
    root_name: &str,
) -> Result<()> {
    if receipt.version != version
        || receipt.platform != platform
        || receipt.toolchain_root != root_name
    {
        return Err(Error::config(
            "managed Rust runtime receipt does not match the selected runtime",
        ));
    }
    let mut previous = None;
    for file in &receipt.files {
        if file.path.is_empty()
            || file.path.len() > MAX_RUST_RUNTIME_PATH_BYTES
            || file.modified_nanoseconds >= 1_000_000_000
            || previous.is_some_and(|path: &str| path >= file.path.as_str())
        {
            return Err(Error::config(
                "managed Rust runtime receipt contains an invalid file inventory",
            ));
        }
        let path = Path::new(&file.path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(Error::config(
                "managed Rust runtime receipt contains an unsafe file path",
            ));
        }
        previous = Some(file.path.as_str());
    }
    Ok(())
}

fn write_rust_runtime_receipt_atomic(path: &Path, receipt: &RustRuntimeReceipt) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(receipt)?;
    if bytes.len() as u64 > MAX_RUST_RUNTIME_RECEIPT_BYTES {
        return Err(Error::other(
            "managed Rust runtime receipt exceeds its size limit",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::other(format!("path has no parent: {}", path.display())))?;
    let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{RUST_RUNTIME_RECEIPT_FILE}.tmp-{}-{serial}",
        std::process::id()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        use std::io::Write as _;
        file.write_all(&bytes)
            .map_err(|error| Error::io(&temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io(&temporary, error))?;
        atomic_replace_runtime_receipt(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn atomic_replace_runtime_receipt(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|error| Error::io(destination, error))
}

#[cfg(windows)]
fn atomic_replace_runtime_receipt(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(Error::io(destination, std::io::Error::last_os_error()));
    }
    Ok(())
}

fn hash_identity_value(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
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
#[allow(clippy::large_enum_variant)]
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

    pub fn lock_path(&self) -> &Path {
        self.locator.lock_path()
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
    if !runtime_is_installed(dirs, &receipt.runtime, identity.platform.as_str()) {
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
        NativeToolFamily::Cargo => tool
            .strip_prefix("cargo:")
            .filter(|_| crate::tool::ToolId::parse(tool).is_ok_and(|id| id.to_string() == tool)),
        NativeToolFamily::Go => tool
            .strip_prefix("go:")
            .filter(|_| crate::tool::ToolId::parse(tool).is_ok_and(|id| id.to_string() == tool)),
    };
    if subject.is_none() {
        return Err(Error::config(format!(
            "invalid canonical {:?} native tool id `{tool}`",
            family
        )));
    }
    Ok(())
}

fn runtime_is_installed(dirs: &Dirs, runtime: &InstallDependency, identity_platform: &str) -> bool {
    let marker_root = dirs.install_path(&runtime.id, &runtime.version);
    if !is_regular_file(&marker_root.join(".osdk-complete")) {
        return false;
    }
    match runtime.identity.as_deref() {
        None => false,
        Some(expected) => {
            runtime_identity_at(dirs, runtime, identity_platform).as_deref() == Some(expected)
        }
    }
}

fn runtime_identity_at(
    dirs: &Dirs,
    runtime: &InstallDependency,
    identity_platform: &str,
) -> Option<String> {
    match runtime.id.as_str() {
        "rust" => {
            let marker = dirs.install_path("rust", &runtime.version);
            if marker.join(".osdk-linked").exists() {
                return None;
            }
            if identity_platform != Platform::current().to_string() {
                return None;
            }
            rust_runtime_identity(dirs, Platform::current(), &runtime.version).ok()
        }
        "go" => {
            if identity_platform != Platform::current().to_string() {
                return None;
            }
            go_runtime_identity(dirs, Platform::current(), &runtime.version).ok()
        }
        _ => None,
    }
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
            go_runtime_identity(&dirs, Platform::current(), runtime_version).unwrap();
        let identity = InstallIdentity::new(
            "go:example.com/acme/fixture",
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
        write_executable(&runtime_root.join("bin/cargo"), b"cargo");
        write_executable(&runtime_root.join("bin/rustc"), b"rustc");
        let target_lib = runtime_root
            .join("lib/rustlib")
            .join(Platform::current().llvm_triple())
            .join("lib");
        std::fs::create_dir_all(&target_lib).unwrap();
        std::fs::write(target_lib.join("libstd-fixture.rlib"), b"std").unwrap();
        std::fs::write(target_lib.join("libcore-fixture.rlib"), b"core").unwrap();
        std::fs::write(target_lib.join("liballoc-fixture.rlib"), b"alloc").unwrap();
        std::fs::write(
            runtime_root.join("lib/librustc_driver-fixture.so"),
            b"driver",
        )
        .unwrap();
        std::fs::write(
            runtime_root.join("lib/rustlib/manifest-rustc-fixture"),
            b"file:bin/rustc\nfile:lib/librustc_driver-fixture.so",
        )
        .unwrap();
        std::fs::write(
            runtime_root.join("lib/rustlib/manifest-rust-std-fixture"),
            b"file:libstd-fixture.rlib",
        )
        .unwrap();
        std::fs::write(
            runtime_root.join("lib/rustlib/manifest-cargo-fixture"),
            b"file:bin/cargo",
        )
        .unwrap();
        let marker = dirs.install_path("rust", runtime_version);
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join(".osdk-complete"), b"").unwrap();
        let runtime_identity =
            rust_runtime_identity(&dirs, Platform::current(), runtime_version).unwrap();
        let identity = InstallIdentity::new(
            "cargo:ripgrep",
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
        for directory in ["bin", "pkg/tool", "src/runtime"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        write_executable(
            &root
                .join("bin")
                .join(format!("go{}", Platform::current().os.exe_suffix())),
            b"go",
        );
        write_executable(
            &root
                .join("bin")
                .join(format!("gofmt{}", Platform::current().os.exe_suffix())),
            b"gofmt",
        );
        write_executable(
            &root
                .join("pkg/tool")
                .join(format!("compile{}", Platform::current().os.exe_suffix())),
            b"compile",
        );
        std::fs::write(root.join("src/runtime/runtime.go"), b"package runtime").unwrap();
        std::fs::write(root.join("VERSION"), format!("go{version}\n")).unwrap();
        std::fs::write(root.join("go.env"), b"GOTOOLCHAIN=local\n").unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    fn write_runtime_with_identity(dirs: &Dirs, version: &str, identity: &str) {
        let root = dirs.install_path("go", version);
        write_runtime(dirs, version);
        std::fs::write(root.join("src/runtime/identity"), identity).unwrap();
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
                "go:example.com/acme/fixture",
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
    fn rust_runtime_identity_covers_complete_build_critical_payloads() {
        let temporary = tempfile::tempdir().unwrap();
        let lifecycle = cargo_lifecycle(temporary.path(), "1.91.1");
        let dirs = dirs(temporary.path());
        let runtime_root = dirs.rustup_home().join("toolchains/1.91.1");
        let first = rust_runtime_identity(&dirs, Platform::current(), "1.91.1").unwrap();
        assert!(first.starts_with("b3-rust-v2:"));
        let receipt_path = dirs
            .install_path("rust", "1.91.1")
            .join(RUST_RUNTIME_RECEIPT_FILE);
        let first_receipt = std::fs::read(&receipt_path).unwrap();

        std::fs::create_dir_all(runtime_root.join("share/doc")).unwrap();
        std::fs::write(runtime_root.join("share/doc/unrelated.html"), b"one").unwrap();
        let unrelated = rust_runtime_identity(&dirs, Platform::current(), "1.91.1").unwrap();
        assert_eq!(first, unrelated);
        assert_eq!(first_receipt, std::fs::read(&receipt_path).unwrap());
        assert_eq!(
            lifecycle.identity().dependencies[0].identity.as_deref(),
            Some(first.as_str())
        );

        write_executable(&runtime_root.join("bin/rustc"), b"changed rustc");
        let changed = rust_runtime_identity(&dirs, Platform::current(), "1.91.1").unwrap();
        assert_ne!(first, changed);

        let lifecycle = cargo_lifecycle(temporary.path(), "1.91.2");
        let runtime_root = dirs.rustup_home().join("toolchains/1.91.2");
        let first = lifecycle.identity().dependencies[0]
            .identity
            .clone()
            .unwrap();
        std::fs::write(
            runtime_root.join("lib/librustc_driver-fixture.so"),
            b"changed driver",
        )
        .unwrap();
        assert_ne!(
            first,
            rust_runtime_identity(&dirs, Platform::current(), "1.91.2").unwrap()
        );

        let lifecycle = cargo_lifecycle(temporary.path(), "1.91.3");
        let runtime_root = dirs.rustup_home().join("toolchains/1.91.3");
        let first = lifecycle.identity().dependencies[0]
            .identity
            .clone()
            .unwrap();
        let target_lib = runtime_root
            .join("lib/rustlib")
            .join(Platform::current().llvm_triple())
            .join("lib/libstd-fixture.rlib");
        std::fs::write(target_lib, b"changed std").unwrap();
        assert_ne!(
            first,
            rust_runtime_identity(&dirs, Platform::current(), "1.91.3").unwrap()
        );

        for (version, file, changed) in [
            ("1.91.4", "libcore-fixture.rlib", b"CORE".as_slice()),
            ("1.91.5", "liballoc-fixture.rlib", b"ALLOC".as_slice()),
        ] {
            let lifecycle = cargo_lifecycle(temporary.path(), version);
            let first = lifecycle.identity().dependencies[0]
                .identity
                .clone()
                .unwrap();
            let path = dirs
                .rustup_home()
                .join("toolchains")
                .join(version)
                .join("lib/rustlib")
                .join(Platform::current().llvm_triple())
                .join("lib")
                .join(file);
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(path, changed).unwrap();
            assert_ne!(
                first,
                rust_runtime_identity(&dirs, Platform::current(), version).unwrap(),
                "{file} mutation must change the runtime identity"
            );
        }
    }

    #[test]
    fn go_runtime_identity_caches_inventory_and_rehashes_build_inputs() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.24.0");
        let root = dirs.install_path("go", "1.24.0");

        let first = go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();
        assert!(first.starts_with("b3-go-v1:"));
        let receipt = root.join(GO_RUNTIME_RECEIPT_FILE);
        let first_receipt = std::fs::read(&receipt).unwrap();
        let second = go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();
        assert_eq!(first, second);
        assert_eq!(first_receipt, std::fs::read(&receipt).unwrap());

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            root.join("src/runtime/runtime.go"),
            b"package runtime // changed",
        )
        .unwrap();
        let changed = go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();
        assert_ne!(first, changed);
    }

    #[test]
    fn go_runtime_identity_rejects_corrupt_receipt() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.24.0");
        go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();
        let receipt = dirs
            .install_path("go", "1.24.0")
            .join(GO_RUNTIME_RECEIPT_FILE);
        std::fs::write(&receipt, b"{}").unwrap();
        assert!(go_runtime_identity(&dirs, Platform::current(), "1.24.0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn go_runtime_identity_accepts_cas_links_and_rejects_external_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let dirs = dirs(temporary.path());
        write_runtime(&dirs, "1.24.0");
        let root = dirs.install_path("go", "1.24.0");
        let source = root.join("src/runtime/runtime.go");
        let cas = dirs.store.join("aa/bb/payload");
        std::fs::create_dir_all(cas.parent().unwrap()).unwrap();
        std::fs::write(&cas, b"package runtime").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&cas, &source).unwrap();
        let first = go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();

        let second_cas = dirs.store.join("cc/dd/payload");
        std::fs::create_dir_all(second_cas.parent().unwrap()).unwrap();
        std::fs::write(&second_cas, b"package runtime").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&second_cas, &source).unwrap();
        let retargeted = go_runtime_identity(&dirs, Platform::current(), "1.24.0").unwrap();
        assert_ne!(first, retargeted);

        std::fs::remove_file(&source).unwrap();
        let outside = temporary.path().join("outside.go");
        std::fs::write(&outside, b"package runtime").unwrap();
        symlink(&outside, &source).unwrap();
        assert!(go_runtime_identity(&dirs, Platform::current(), "1.24.0").is_err());
    }

    #[test]
    fn rust_runtime_identity_rejects_corrupt_or_symlinked_receipts() {
        let temporary = tempfile::tempdir().unwrap();
        cargo_lifecycle(temporary.path(), "1.91.6");
        let dirs = dirs(temporary.path());
        let receipt = dirs
            .install_path("rust", "1.91.6")
            .join(RUST_RUNTIME_RECEIPT_FILE);
        std::fs::write(&receipt, b"{}").unwrap();
        assert!(rust_runtime_identity(&dirs, Platform::current(), "1.91.6").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            std::fs::remove_file(&receipt).unwrap();
            let outside = temporary.path().join("outside-receipt");
            std::fs::write(&outside, b"{}").unwrap();
            symlink(&outside, &receipt).unwrap();
            let error = rust_runtime_identity(&dirs, Platform::current(), "1.91.6").unwrap_err();
            assert!(error.to_string().contains("non-symlink"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rust_runtime_identity_rejects_target_lib_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        cargo_lifecycle(temporary.path(), "1.91.7");
        let dirs = dirs(temporary.path());
        let target_lib = dirs
            .rustup_home()
            .join("toolchains/1.91.7/lib/rustlib")
            .join(Platform::current().llvm_triple())
            .join("lib");
        let outside = temporary.path().join("outside-payload");
        std::fs::write(&outside, b"payload").unwrap();
        symlink(&outside, target_lib.join("libinjected.rlib")).unwrap();
        assert!(rust_runtime_identity(&dirs, Platform::current(), "1.91.7").is_err());
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
            "go:example.com/acme/fixture",
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
        write_runtime(&dirs, "1.23.4");
        let runtime_identity = go_runtime_identity(&dirs, Platform::current(), "1.23.4").unwrap();
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
            executable.with_file_name(executable.file_name().unwrap()),
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
        let expected_identity = go_runtime_identity(&dirs, Platform::current(), "1.23.4").unwrap();
        let identity = InstallIdentity::new(
            "go:example.com/acme/fixture",
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
            dirs.install_path("go", "1.23.4")
                .join("src/runtime/identity"),
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
