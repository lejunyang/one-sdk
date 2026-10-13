//! Policy for reproducible download copies only. CAS, installs and native
//! manager stores are never candidates. Unregistered legacy files are reported
//! but never adopted by a sweep.

use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::DownloadCacheSettings;
use crate::dirs::Dirs;
use crate::error::{Error, Result};

const RECENT_SECS: u64 = 600;
const SIDECARS: &[&str] = &["", ".partial", ".partial.json", ".checksum", ".source-url"];
const MODEL_SIDECARS: &[&str] = &["", ".partial", ".partial.json"];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    schema: u32,
    path: String,
    last_used: u64,
    committed: bool,
}

/// Locks live outside downloads and are never unlinked: replacing a lock file
/// would let another process lock a different inode for the same cache.
pub struct Lease(File);

fn control(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("download-policy-v1")
}

fn ensure_control(dirs: &Dirs) -> Result<PathBuf> {
    check_boundaries(dirs)?;
    let root = control(dirs);
    safe_path(&dirs.cache, &root)?;
    std::fs::create_dir_all(&root).map_err(|error| Error::io(&root, error))?;
    Ok(root)
}

fn open_lock(dirs: &Dirs) -> Result<File> {
    let root = ensure_control(dirs)?;
    let path = root.join("active.lock");
    safe_path(&dirs.cache, &path)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| Error::io(&path, error))
}

impl Lease {
    pub fn acquire(dirs: &Dirs) -> Result<Self> {
        let file = open_lock(dirs)?;
        file.lock_shared()
            .map_err(|error| Error::io(control(dirs), error))?;
        Ok(Self(file))
    }

    fn exclusive(dirs: &Dirs) -> Result<Option<Self>> {
        let file = open_lock(dirs)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(Error::io(control(dirs), error)),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Covers the entire backend transaction, including post-processing/rollback.
pub struct Session {
    lease: Lease,
}

impl Session {
    pub async fn begin(ctx: &crate::backend::Ctx) -> Result<Self> {
        automatic(ctx);
        let dirs = ctx.dirs.clone();
        let lease = tokio::task::spawn_blocking(move || Lease::acquire(&dirs))
            .await
            .map_err(|error| Error::other(format!("download cache lock failed: {error}")))??;
        Ok(Self { lease })
    }

    pub fn finish(self, ctx: &crate::backend::Ctx, paths: &[PathBuf]) {
        for path in paths {
            if let Err(error) = record(&ctx.dirs, path, true) {
                tracing::warn!(%error, "could not commit download cache entry");
            }
        }
        drop(self.lease);
        if ctx.config.settings.offline {
            let mut policy = ctx.config.settings.download_cache.clone();
            policy.max_age_secs = 0;
            if let Err(error) = collect(&ctx.dirs, &policy, false) {
                tracing::warn!(%error, "completed download cleanup deferred");
            }
        } else {
            automatic(ctx);
        }
    }
}

fn automatic(ctx: &crate::backend::Ctx) {
    if ctx.config.settings.offline {
        return;
    }
    match collect(&ctx.dirs, &ctx.config.settings.download_cache, false) {
        Ok(report) if report.over_limit => tracing::warn!(
            bytes = report.managed_bytes,
            limit = ?report.max_size_bytes,
            busy = report.busy,
            "download cache exceeds its soft limit; active/recent or unsafe entries are protected"
        ),
        Err(error) => tracing::warn!(%error, "download cache maintenance deferred"),
        _ => {}
    }
}

fn relative(dirs: &Dirs, path: &Path) -> Result<String> {
    let value = path
        .strip_prefix(dirs.downloads())
        .map_err(|_| Error::other("not a download cache path"))?;
    if value.as_os_str().is_empty()
        || value
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::other("unsafe download cache entry"));
    }
    let value = value
        .to_str()
        .ok_or_else(|| Error::other("download cache path is not UTF-8"))?;
    #[cfg(unix)]
    if value.contains('\\') {
        return Err(Error::other("ambiguous download cache path"));
    }
    Ok(value.replace('\\', "/"))
}

fn key(path: &str) -> String {
    blake3::hash(path.as_bytes()).to_hex().to_string()
}

/// Call under a shared lease before consuming or writing an exact artifact.
/// Recording access invalidates a previous successful-use flag until the new
/// installation commits, preserving failed retries under no-retention policy.
pub fn record(dirs: &Dirs, path: &Path, committed: bool) -> Result<()> {
    let relative = relative(dirs, path)?;
    safe_path(&dirs.cache, path)?;
    let root = ensure_control(dirs)?;
    let index = root.join("entries");
    safe_path(&dirs.cache, &index)?;
    std::fs::create_dir_all(&index).map_err(|error| Error::io(&index, error))?;
    let id = key(&relative);
    let index_lock = root.join("index.lock");
    safe_path(&dirs.cache, &index_lock)?;
    let _lock = crate::lock::FileLock::acquire(index_lock)?;
    let target = index.join(format!("{id}.json"));
    safe_path(&dirs.cache, &target)?;
    let entry = Entry {
        schema: 1,
        path: relative,
        last_used: crate::source::now_secs(),
        committed,
    };
    // NamedTempFile uses a unique name and persist replaces atomically on Windows.
    let mut temporary =
        tempfile::NamedTempFile::new_in(&index).map_err(|error| Error::io(&index, error))?;
    serde_json::to_writer(temporary.as_file_mut(), &entry)?;
    temporary
        .persist(&target)
        .map_err(|error| Error::io(target, error.error))?;
    Ok(())
}

fn check_boundaries(dirs: &Dirs) -> Result<()> {
    validate_disposable_directory(dirs, &dirs.downloads())
}

/// Shared boundary guard for explicit native-cache cleanup too. A cache path
/// must never resolve through a link or overlap an installed/persistent root.
pub fn validate_disposable_directory(dirs: &Dirs, path: &Path) -> Result<()> {
    safe_path(&dirs.cache, path)?;
    let downloads = absolute(path)?;
    for protected in [
        dirs.store.clone(),
        dirs.installs.clone(),
        dirs.models(),
        dirs.config.clone(),
        dirs.data.join("skills"),
        dirs.data.join("shims"),
        dirs.data.join("cargo"),
        dirs.data.join("rustup"),
        dirs.data.join("plugins"),
    ] {
        let protected = absolute(&protected)?;
        if downloads.starts_with(&protected) || protected.starts_with(&downloads) {
            return Err(Error::other(
                "download cache overlaps persistent data; maintenance refused",
            ));
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_relative() {
        return absolute(
            &std::env::current_dir()
                .map_err(|error| Error::io(path, error))?
                .join(path),
        );
    }
    if path.exists() {
        return dunce::canonicalize(path).map_err(|error| Error::io(path, error));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::other("invalid cache root"))?;
    Ok(absolute(parent)?.join(
        path.file_name()
            .ok_or_else(|| Error::other("invalid cache path"))?,
    ))
}

/// Refuse links anywhere below the configured cache root, including the root.
/// Platform ancestors (e.g. macOS /var) may legitimately be aliases.
pub(crate) fn safe_path(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| Error::other("cache path escaped root"))?;
    let mut current = root.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            if !matches!(component, Component::Normal(_)) {
                return Err(Error::other("unsafe cache path"));
            }
            current.push(component.as_os_str());
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::other(format!(
                    "symlink in cache path: {}",
                    current.display()
                )))
            }
            // Windows junctions/reparse points are not always reported as symlinks.
            #[cfg(windows)]
            Ok(metadata)
                if {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                } =>
            {
                return Err(Error::other("reparse point in cache path"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&current, error)),
        }
    }
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub schema: u32,
    pub dry_run: bool,
    pub managed_bytes: u64,
    pub completed_bytes: u64,
    pub retry_bytes: u64,
    pub protected_retry_bytes: u64,
    /// Allocated blocks of managed names; shared extents cannot be deduplicated.
    pub allocated_bytes: Option<u64>,
    pub unmanaged_bytes: u64,
    pub max_size_bytes: Option<u64>,
    pub max_age_secs: u64,
    pub over_limit: bool,
    pub busy: bool,
    pub skipped_unsafe: usize,
    pub removed_entries: usize,
    pub selected_entries: usize,
    pub selected_bytes: u64,
    /// Upper estimate only; excludes multiply linked inodes on Unix. Reflink
    /// sharing/compression/delayed filesystem reclamation remain unknowable.
    pub reclaimable_bytes_upper_bound: Option<u64>,
    pub paths: Vec<String>,
}

struct Candidate {
    entry: Entry,
    index: PathBuf,
    paths: Vec<PathBuf>,
    bytes: u64,
    allocated: u64,
    reclaimable: u64,
}

fn candidates(dirs: &Dirs, report: &mut Report) -> Result<Vec<Candidate>> {
    let index = control(dirs).join("entries");
    safe_path(&dirs.cache, &index)?;
    if !index.exists() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for item in std::fs::read_dir(&index).map_err(|error| Error::io(&index, error))? {
        let item = item.map_err(|error| Error::io(&index, error))?;
        let path = item.path();
        let result = (|| -> Result<Candidate> {
            safe_path(&dirs.cache, &path)?;
            let bytes = crate::inventory::read_stable_regular_file(&path, 16 * 1024)
                .map_err(|error| Error::io(&path, error))?;
            let entry: Entry = serde_json::from_slice(&bytes)?;
            if entry.schema != 1
                || path.file_name().and_then(|n| n.to_str())
                    != Some(&format!("{}.json", key(&entry.path)))
            {
                return Err(Error::other("invalid download cache index"));
            }
            let base = dirs.downloads().join(&entry.path);
            if relative(dirs, &base)? != entry.path {
                return Err(Error::other("noncanonical download cache entry"));
            }
            let mut candidate = Candidate {
                entry,
                index: path.clone(),
                paths: Vec::new(),
                bytes: 0,
                allocated: 0,
                reclaimable: 0,
            };
            // Model pulls do not write SDK checksum/source sidecars. Those
            // suffixes may be real model files, including unclaimed legacy
            // downloads, and cannot be inferred to be disposable metadata.
            let model = ["huggingface", "modelscope", "civitai"]
                .iter()
                .any(|provider| {
                    candidate
                        .entry
                        .path
                        .starts_with(&format!("models/{provider}/"))
                });
            for suffix in if model { MODEL_SIDECARS } else { SIDECARS } {
                let file = sibling(&base, suffix);
                safe_path(&dirs.cache, &file)?;
                let metadata = match std::fs::symlink_metadata(&file) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(Error::io(&file, error)),
                };
                if !metadata.is_file() {
                    return Err(Error::other("download entry is not a regular file"));
                }
                candidate.bytes = candidate.bytes.saturating_add(metadata.len());
                if !candidate.entry.committed {
                    // A long interrupted transfer must get its retry grace
                    // from the last write, not from the time it began.
                    if let Ok(modified) = metadata.modified().and_then(|time| {
                        time.duration_since(std::time::UNIX_EPOCH)
                            .map_err(std::io::Error::other)
                    }) {
                        candidate.entry.last_used =
                            candidate.entry.last_used.max(modified.as_secs());
                    }
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let allocated = metadata.blocks().saturating_mul(512);
                    candidate.allocated = candidate.allocated.saturating_add(allocated);
                    if metadata.nlink() == 1 {
                        candidate.reclaimable = candidate.reclaimable.saturating_add(allocated);
                    }
                }
                candidate.paths.push(file);
            }
            Ok(candidate)
        })();
        match result {
            Ok(candidate) => found.push(candidate),
            Err(error) => {
                report.skipped_unsafe += 1;
                tracing::debug!(%error, "protected invalid download entry");
            }
        }
    }
    found.sort_by(|a, b| {
        a.entry
            .last_used
            .cmp(&b.entry.last_used)
            .then(a.entry.path.cmp(&b.entry.path))
    });
    Ok(found)
}

/// Capacity is logical bytes of registered download payloads + sidecars, never
/// an assertion about free disk space. Recent/active downloads may exceed it.
pub fn collect(dirs: &Dirs, policy: &DownloadCacheSettings, dry_run: bool) -> Result<Report> {
    let lease = Lease::exclusive(dirs)?;
    let mut report = Report {
        schema: 1,
        dry_run,
        max_size_bytes: policy.max_size_bytes.bytes(),
        max_age_secs: policy.max_age_secs,
        busy: lease.is_none(),
        allocated_bytes: cfg!(unix).then_some(0),
        reclaimable_bytes_upper_bound: cfg!(unix).then_some(0),
        ..Report::default()
    };
    // Index writes are serialized even while shared installation leases are held.
    let index_lock = control(dirs).join("index.lock");
    safe_path(&dirs.cache, &index_lock)?;
    let _index_lock = crate::lock::FileLock::acquire(index_lock)?;
    let entries = candidates(dirs, &mut report)?;
    for entry in &entries {
        report.managed_bytes = report.managed_bytes.saturating_add(entry.bytes);
        if entry.entry.committed {
            report.completed_bytes = report.completed_bytes.saturating_add(entry.bytes);
        } else {
            report.retry_bytes = report.retry_bytes.saturating_add(entry.bytes);
        }
        if let Some(allocated) = &mut report.allocated_bytes {
            *allocated = allocated.saturating_add(entry.allocated);
        }
    }
    let total = logical_tree_size(&dirs.downloads())?;
    report.unmanaged_bytes = total.saturating_sub(report.managed_bytes);
    let mut remaining = report.managed_bytes;
    let now = crate::source::now_secs();
    let protected = entries
        .iter()
        .filter(|entry| {
            !entry.entry.committed && now.saturating_sub(entry.entry.last_used) < RECENT_SECS
        })
        .fold(0u64, |bytes, entry| bytes.saturating_add(entry.bytes));
    let mut budget_bytes = remaining.saturating_sub(protected);
    report.protected_retry_bytes = protected;
    let mut entries = entries;
    // Reject oversized completed copies before LRU so smaller useful entries survive.
    entries.sort_by_key(|entry| {
        !(entry.entry.committed
            && policy
                .max_size_bytes
                .bytes()
                .is_some_and(|limit| entry.bytes > limit || limit == 0))
    });
    if !report.busy {
        for candidate in entries {
            let age = now.saturating_sub(candidate.entry.last_used);
            let oversized = candidate.entry.committed
                && policy
                    .max_size_bytes
                    .bytes()
                    .is_some_and(|limit| candidate.bytes > limit || limit == 0);
            let expired = policy.max_age_secs > 0 && age >= policy.max_age_secs;
            let over = policy
                .max_size_bytes
                .bytes()
                .is_some_and(|limit| budget_bytes > limit);
            if !oversized
                && ((!candidate.entry.committed && age < RECENT_SECS) || (!expired && !over))
            {
                continue;
            }
            // Recheck every member before any deletion, and only unlink regular
            // files. Locks are never included in sidecars.
            for path in &candidate.paths {
                safe_path(&dirs.cache, path)?;
            }
            if !dry_run {
                for path in &candidate.paths {
                    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
                }
                std::fs::remove_file(&candidate.index)
                    .map_err(|error| Error::io(&candidate.index, error))?;
            }
            remaining = remaining.saturating_sub(candidate.bytes);
            budget_bytes = budget_bytes.saturating_sub(candidate.bytes);
            if !dry_run {
                if candidate.entry.committed {
                    report.completed_bytes = report.completed_bytes.saturating_sub(candidate.bytes);
                } else {
                    report.retry_bytes = report.retry_bytes.saturating_sub(candidate.bytes);
                }
                if let Some(allocated) = &mut report.allocated_bytes {
                    *allocated = allocated.saturating_sub(candidate.allocated);
                }
            }
            report.selected_bytes = report.selected_bytes.saturating_add(candidate.bytes);
            if let Some(reclaimable) = &mut report.reclaimable_bytes_upper_bound {
                *reclaimable = reclaimable.saturating_add(candidate.reclaimable);
            }
            report.selected_entries += 1;
            if !dry_run {
                report.removed_entries += 1;
            }
            report.paths.push(candidate.entry.path);
        }
    }
    if !dry_run {
        report.managed_bytes = remaining;
    }
    report.over_limit = policy
        .max_size_bytes
        .bytes()
        .is_some_and(|limit| remaining > limit);
    Ok(report)
}

fn logical_tree_size(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for item in walkdir::WalkDir::new(root).follow_links(false) {
        let item = item.map_err(|error| Error::other(format!("cache scan: {error}")))?;
        if item.file_type().is_file() {
            total = total.saturating_add(
                item.metadata()
                    .map_err(|error| Error::other(format!("cache stat: {error}")))?
                    .len(),
            );
        }
    }
    Ok(total)
}

/// Keep explicit clean's legacy scope while coordinating with downloads and
/// refusing unsafe roots. Links are skipped, and persistent data is excluded.
pub fn clean(dirs: &Dirs) -> Result<()> {
    let _lease = Lease::exclusive(dirs)?.ok_or_else(|| {
        Error::other("downloads are active; retry cache clean when installation finishes")
    })?;
    if !dirs.downloads().exists() {
        return Ok(());
    }
    for item in walkdir::WalkDir::new(dirs.downloads()).follow_links(false) {
        let item = item.map_err(|error| Error::other(format!("cache scan: {error}")))?;
        if item.file_type().is_file() && !item.file_name().to_string_lossy().ends_with(".osdk-lock")
        {
            safe_path(&dirs.cache, item.path())?;
            std::fs::remove_file(item.path()).map_err(|error| Error::io(item.path(), error))?;
        }
    }
    Ok(())
}

/// Preflight known remaining HTTP bytes before creating/truncating a partial.
/// This is advisory: concurrent writers, unknown lengths, extraction and CAS
/// materialization can still exhaust space later.
pub fn check_free_space(directory: &Path, needed: u64) -> Result<()> {
    let available = free_space(directory)?;
    if needed > available {
        return Err(Error::other(format!("insufficient download space at {}: need {needed} bytes, available {available}; run `osdk cache trim` or choose a larger OSDK_CACHE_DIR", directory.display())));
    }
    Ok(())
}

#[cfg(unix)]
// libc's statvfs field widths differ across supported Unix targets.
#[allow(clippy::unnecessary_cast)]
fn free_space(directory: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| Error::other("invalid disk-space path"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: name is NUL-terminated and stats points to writable statvfs storage.
    if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(Error::io(directory, std::io::Error::last_os_error()));
    }
    // SAFETY: successful statvfs initialized the structure.
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(windows)]
fn free_space(directory: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    // SAFETY: name is terminated; available is writable, other outputs optional.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            name.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(Error::io(directory, std::io::Error::last_os_error()));
    }
    Ok(available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DownloadCacheLimit, UnlimitedCache};

    fn dirs(root: &Path) -> Dirs {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        dirs
    }

    fn policy(bytes: u64) -> DownloadCacheSettings {
        DownloadCacheSettings {
            max_size_bytes: DownloadCacheLimit::Bytes(bytes),
            max_age_secs: 0,
        }
    }

    fn entry(dirs: &Dirs, name: &str, size: u64, age: u64, committed: bool) -> PathBuf {
        let path = dirs.downloads().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = File::create(&path).unwrap();
        file.set_len(size).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(crate::source::now_secs().saturating_sub(age)),
        ))
        .unwrap();
        record(dirs, &path, committed).unwrap();
        let relative = relative(dirs, &path).unwrap();
        let index = control(dirs)
            .join("entries")
            .join(format!("{}.json", key(&relative)));
        let record = Entry {
            schema: 1,
            path: relative,
            last_used: crate::source::now_secs().saturating_sub(age),
            committed,
        };
        std::fs::write(index, serde_json::to_vec(&record).unwrap()).unwrap();
        path
    }

    #[test]
    fn oversized_model_is_removed_after_commit_without_evicting_small_copies() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let small = entry(&dirs, "sdk/tool.tgz", 32, 700, true);
        let model = entry(
            &dirs,
            "models/repo/model.gguf",
            2 * 1024 * 1024 * 1024 + 1,
            0,
            false,
        );
        let cap = DownloadCacheSettings::default();
        let pending = collect(&dirs, &cap, false).unwrap();
        assert!(pending.over_limit);
        assert!(model.exists());
        assert!(small.exists());
        record(&dirs, &model, true).unwrap();
        let preview = collect(&dirs, &cap, true).unwrap();
        assert_eq!(preview.selected_entries, 1);
        assert_eq!(preview.removed_entries, 0);
        assert!(model.exists());
        let actual = collect(&dirs, &cap, false).unwrap();
        assert_eq!(actual.paths, vec!["models/repo/model.gguf"]);
        assert_eq!(actual.managed_bytes, 32);
        assert!(!model.exists());
        assert!(small.exists());
    }

    #[test]
    fn zero_retention_deletes_completed_copies_and_sidecars_but_preserves_recent_retries() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let done = entry(&dirs, "tool/file", 17, 0, true);
        for suffix in SIDECARS.iter().skip(1) {
            std::fs::write(sibling(&done, suffix), b"sidecar").unwrap();
        }
        let lock = sibling(&done, ".osdk-lock");
        std::fs::write(&lock, b"").unwrap();
        let failed = entry(&dirs, "tool/failed", 2, 0, false);
        std::fs::write(sibling(&failed, ".partial"), b"resume").unwrap();
        let report = collect(&dirs, &policy(0), false).unwrap();
        assert_eq!(report.removed_entries, 1);
        for suffix in SIDECARS {
            assert!(!sibling(&done, suffix).exists());
        }
        assert!(lock.exists());
        assert!(failed.exists());
        assert!(sibling(&failed, ".partial").exists());
    }

    #[test]
    fn lru_and_expiry_reclaim_abandoned_partials_as_a_unit() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let old = entry(&dirs, "old/file", 40, 1000, true);
        let newest = entry(&dirs, "new/file", 40, 700, true);
        let failed = entry(&dirs, "failed/file", 10, 900, false);
        std::fs::write(sibling(&failed, ".partial"), b"resume").unwrap();
        std::fs::write(sibling(&failed, ".partial.json"), b"validator").unwrap();
        for suffix in [".partial", ".partial.json"] {
            File::options()
                .write(true)
                .open(sibling(&failed, suffix))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(crate::source::now_secs() - 900),
                ))
                .unwrap();
        }
        let report = collect(&dirs, &policy(50), false).unwrap();
        assert_eq!(report.paths, vec!["old/file", "failed/file"]);
        assert!(!old.exists());
        assert!(!sibling(&failed, ".partial").exists());
        assert!(!sibling(&failed, ".partial.json").exists());
        assert!(newest.exists());
        let unlimited = DownloadCacheSettings {
            max_size_bytes: DownloadCacheLimit::Unlimited(UnlimitedCache::Unlimited),
            max_age_secs: 800,
        };
        assert_eq!(
            collect(&dirs, &unlimited, false).unwrap().removed_entries,
            0
        );
        let expiry = DownloadCacheSettings {
            max_age_secs: 650,
            ..unlimited
        };
        assert_eq!(collect(&dirs, &expiry, false).unwrap().removed_entries, 1);
    }

    #[test]
    fn long_interrupted_download_keeps_grace_from_the_last_partial_write() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = entry(&dirs, "long/model.gguf", 0, 3600, false);
        let partial = sibling(&path, ".partial");
        std::fs::write(&partial, b"recent resume bytes").unwrap();
        let report = collect(&dirs, &policy(0), false).unwrap();
        assert_eq!(report.removed_entries, 0);
        assert_eq!(report.protected_retry_bytes, 19);
        assert!(partial.exists());
    }

    #[test]
    fn legacy_files_and_persistent_roots_are_never_policy_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let legacy = dirs.downloads().join("legacy");
        std::fs::write(&legacy, b"legacy").unwrap();
        for root in [
            &dirs.store,
            &dirs.installs,
            &dirs.models(),
            &dirs.cache.join("pkg"),
        ] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(root.join("keep"), b"unique").unwrap();
        }
        let copy = entry(&dirs, "registered", 5, 0, true);
        let report = collect(&dirs, &policy(0), false).unwrap();
        assert_eq!(report.unmanaged_bytes, 6);
        assert!(legacy.exists());
        assert!(!copy.exists());
        assert!(dirs.store.join("keep").exists());
        assert!(dirs.installs.join("keep").exists());
        assert!(dirs.models().join("keep").exists());
        assert!(dirs.cache.join("pkg/keep").exists());
        let mut overlap = dirs.clone();
        overlap.store = overlap.downloads();
        assert!(collect(&overlap, &policy(0), false).is_err());
    }

    #[test]
    fn model_files_are_not_misclassified_as_sdk_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = entry(
            &dirs,
            "models/huggingface/owner/revision/config",
            3,
            0,
            true,
        );
        let checksum_file = sibling(&path, ".checksum");
        let source_file = sibling(&path, ".source-url");
        std::fs::write(&checksum_file, b"legacy model data").unwrap();
        std::fs::write(&source_file, b"legacy source data").unwrap();
        collect(&dirs, &policy(0), false).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read(&checksum_file).unwrap(), b"legacy model data");
        assert_eq!(std::fs::read(&source_file).unwrap(), b"legacy source data");
    }

    #[test]
    fn held_shared_lease_blocks_trim_and_explicit_clean() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = entry(&dirs, "payload", 20, 0, true);
        let first = Lease::acquire(&dirs).unwrap();
        let second = Lease::acquire(&dirs).unwrap();
        assert!(collect(&dirs, &policy(0), false).unwrap().busy);
        assert!(clean(&dirs).is_err());
        drop(first);
        assert!(collect(&dirs, &policy(0), false).unwrap().busy);
        assert!(path.exists());
        drop(second);
        assert_eq!(
            collect(&dirs, &policy(0), false).unwrap().removed_entries,
            1
        );
    }

    #[test]
    fn lease_child() {
        use std::io::Write;
        let Some(root) = std::env::var_os("OSDK_CACHE_LEASE_TEST_ROOT") else {
            return;
        };
        let dirs = dirs(Path::new(&root));
        let _lease = Lease::acquire(&dirs).unwrap();
        println!("LEASE_READY");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
    }

    #[test]
    fn real_child_process_lease_protects_downloads() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = entry(&dirs, "payload", 20, 0, true);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cache::downloads::tests::lease_child",
                "--nocapture",
            ])
            .env("OSDK_CACHE_LEASE_TEST_ROOT", temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert!(output.read_line(&mut line).unwrap() > 0);
            if line.contains("LEASE_READY") {
                break;
            }
            line.clear();
        }
        assert!(collect(&dirs, &policy(0), false).unwrap().busy);
        assert!(path.exists());
        child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            collect(&dirs, &policy(0), false).unwrap().removed_entries,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_boundaries_and_hardlink_accounting_are_conservative() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let shared = entry(&dirs, "shared", 0, 0, true);
        std::fs::write(&shared, b"shared inode").unwrap();
        let installed = dirs.installs.join("live");
        std::fs::hard_link(&shared, &installed).unwrap();
        let report = collect(&dirs, &policy(0), false).unwrap();
        assert_eq!(report.reclaimable_bytes_upper_bound, Some(0));
        assert_eq!(std::fs::read(&installed).unwrap(), b"shared inode");
        let unsafe_file = entry(&dirs, "unsafe/file", 1, 0, true);
        std::fs::remove_file(&unsafe_file).unwrap();
        symlink(&installed, &unsafe_file).unwrap();
        assert_eq!(collect(&dirs, &policy(0), false).unwrap().skipped_unsafe, 1);
        assert_eq!(std::fs::read(&installed).unwrap(), b"shared inode");
        let downloads = dirs.downloads();
        std::fs::remove_dir_all(&downloads).unwrap();
        symlink(&dirs.installs, &downloads).unwrap();
        assert!(collect(&dirs, &policy(0), false).is_err());
        assert!(clean(&dirs).is_err());
    }

    #[test]
    fn known_disk_shortage_is_separate_from_retention_budget() {
        let temp = tempfile::tempdir().unwrap();
        assert!(check_free_space(temp.path(), 1).is_ok());
        assert!(check_free_space(temp.path(), u64::MAX)
            .unwrap_err()
            .to_string()
            .contains("insufficient download space"));
    }
}
