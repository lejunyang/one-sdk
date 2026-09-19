//! Updating osdk itself from its own GitHub releases.
//!
//! This is deliberately not a [`Backend`](crate::backend::Backend). A backend
//! installs a tool *into* the store under a version directory, while this
//! replaces the two programs the user is currently running, in place, wherever
//! they happen to live. Registering it would also put a fourteenth entry in
//! `Registry::new()`, whose vtables the shim pays for (see AGENTS.md) in
//! exchange for a tool id no request can name.
//!
//! What it does share with a backend is the source policy: the same official /
//! mirror candidates, the same speed probe, the same probe cache and pin
//! handling, reached through [`crate::source::select`]'s non-backend entry
//! points. A user in a region where github.com is slow gets the mirror here for
//! the same reason, and by the same measurement, as when installing Node.
//!
//! Both binaries are replaced together or not at all: a half-updated pair is a
//! broken installation, because `osdk-shim` resolves installs written by `osdk`.
//! The running executable cannot be deleted on Windows and can be replaced but
//! not truncated on Unix, so the old file is renamed aside and the new one moved
//! into place; a failure part-way rolls the renames back.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind};
use crate::platform::{Arch, Os, Platform};
use crate::source::{select, Source};

/// The repository osdk updates itself from.
///
/// Must stay equal to `workspace.package.repository`; the test
/// `repository_matches_the_package_manifest` fails if the two drift, since a
/// stale value here would download another project's releases.
pub const REPOSITORY: &str = "lejunyang/one-sdk";

/// The tool id used for this downloader's source configuration and probe cache.
///
/// It is deliberately distinct from `github:lejunyang/one-sdk`: pinning or
/// disabling a mirror for osdk's own updates should not silently apply to a
/// `github:` install of the same repository, nor the other way round.
pub const SOURCE_ID: &str = "self";

/// The version this build reports, without a leading `v`.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The programs a release ships. Both are replaced together.
const BINARIES: [&str; 2] = ["osdk", "osdk-shim"];

/// Where the new binaries were written, and what they replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeOutcome {
    pub from_version: String,
    pub to_version: String,
    /// The directory holding the replaced programs.
    pub install_dir: PathBuf,
    /// Program file names, in the order they were replaced.
    pub replaced: Vec<String>,
    /// Whether the release was verified against a published `SHA256SUMS`.
    pub checksum_verified: bool,
}

/// A resolved release: its tag, the asset for this host, and where to get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTarget {
    pub version: String,
    pub asset: String,
    /// Download URLs for the asset, best-first.
    pub urls: Vec<String>,
    /// URLs for the release's `SHA256SUMS`, in the same order.
    pub checksum_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// The default sources for osdk's own releases.
///
/// Identical in shape to [`crate::backend::github::GithubBackend`]'s, because
/// the asset is a GitHub release asset and a CN user needs the same proxy.
pub fn default_sources() -> Vec<Source> {
    vec![
        Source::official("github", "https://github.com/").with_index("https://api.github.com/"),
        Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/"),
    ]
}

/// The effective sources after user configuration (`osdk source add/disable`).
pub fn effective_sources(ctx: &Ctx) -> Vec<Source> {
    select::effective_sources_for(ctx, SOURCE_ID, default_sources())
}

/// The URL whose download speed stands in for a source.
///
/// Unlike the `github:` backend, which declines to probe because the API is
/// rate-limited, this measures the actual release asset of the version already
/// installed: it is the very file an upgrade downloads, it exists on every
/// source that can serve an upgrade at all, and fetching a bounded prefix of it
/// costs no API quota. A source that cannot serve it is one that cannot serve
/// the upgrade either, so failing it is the right answer rather than a false
/// negative.
pub fn probe_url(ctx: &Ctx, source: &Source) -> Option<String> {
    let asset = asset_name(ctx.platform)?;
    Some(http::github_url_for_source(
        source,
        &asset_url(CURRENT_VERSION, &asset),
    ))
}

/// How long a single source gets to start serving the release asset.
///
/// The configured `probe_timeout_ms` defaults to 1.5s, which suits the small
/// version index a backend probes. This probe pulls a release binary instead,
/// and a CN proxy fronting github.com was measured at 6.1s just to first byte;
/// at 1.5s every candidate times out, all of them are marked unreachable, and
/// ranking silently degrades to the fixed priority order -- the exact opposite
/// of choosing the faster route. The window still bounds the command: probes
/// run concurrently, so this is the whole wait, not the wait per source.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// Sources ranked by measured speed, best-first, honoring pins and the cache.
pub async fn ranked_sources(ctx: &Ctx) -> Result<Vec<Source>> {
    select::ranked_source_candidates_with_timeout(
        ctx,
        SOURCE_ID,
        effective_sources(ctx),
        probe_timeout(ctx),
        probe_url,
    )
    .await
}

/// Re-probe every source now and return the fresh ranking.
pub async fn refresh_sources(ctx: &Ctx) -> Result<Vec<crate::source::ProbeResult>> {
    select::refresh_with_timeout(
        ctx,
        SOURCE_ID,
        effective_sources(ctx),
        probe_timeout(ctx),
        probe_url,
    )
    .await
}

/// The probe deadline, honoring a raised `probe_timeout_ms` but never lowering
/// the artifact-sized floor a configured-for-indexes value would impose.
fn probe_timeout(ctx: &Ctx) -> std::time::Duration {
    std::time::Duration::from_millis(ctx.config.sources.probe_timeout_ms).max(PROBE_TIMEOUT)
}

/// The release asset for a host platform, or `None` where osdk publishes none.
///
/// The names match what `.github/workflows/publish.yml` uploads, so a platform
/// absent from that matrix is reported as unsupported instead of 404-ing.
pub fn asset_name(platform: Platform) -> Option<String> {
    let target = match (platform.os, platform.arch) {
        (Os::Linux, Arch::X64) => "x86_64-unknown-linux-gnu",
        (Os::Linux, Arch::Arm64) => "aarch64-unknown-linux-gnu",
        (Os::Macos, Arch::X64) => "x86_64-apple-darwin",
        (Os::Macos, Arch::Arm64) => "aarch64-apple-darwin",
        (Os::Windows, Arch::X64) => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    let extension = if matches!(platform.os, Os::Windows) {
        "zip"
    } else {
        "tar.gz"
    };
    Some(format!("osdk-{target}.{extension}"))
}

fn unsupported_platform(platform: Platform) -> Error {
    Error::UnsupportedPlatform {
        os: format!("{:?}", platform.os),
        arch: format!("{:?}", platform.arch),
    }
}

fn asset_url(version: &str, asset: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{asset}")
}

fn checksums_url(version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{version}/SHA256SUMS")
}

/// Compare two release versions. Non-semver tags fall back to a string
/// comparison so an unparseable tag is never silently treated as newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| semver::Version::parse(value.trim_start_matches('v')).ok();
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => candidate.trim_start_matches('v') != current.trim_start_matches('v'),
    }
}

/// Resolve the version to install: the latest release, or an explicit one.
pub async fn resolve_target(
    ctx: &Ctx,
    requested: Option<&str>,
    sources: &[Source],
) -> Result<ReleaseTarget> {
    let asset = asset_name(ctx.platform).ok_or_else(|| unsupported_platform(ctx.platform))?;
    let version = match requested {
        Some(version) => version.trim().trim_start_matches('v').to_string(),
        None => latest_version(ctx, sources).await?,
    };
    if version.is_empty() {
        return Err(Error::VersionResolve {
            tool: SOURCE_ID.into(),
            spec: requested.unwrap_or("latest").to_string(),
            hint: Some("empty release version".into()),
        });
    }
    Ok(ReleaseTarget {
        urls: http::github_url_candidates(sources, &asset_url(&version, &asset)),
        checksum_urls: http::github_url_candidates(sources, &checksums_url(&version)),
        version,
        asset,
    })
}

async fn latest_version(ctx: &Ctx, sources: &[Source]) -> Result<String> {
    let api = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let urls = http::github_url_candidates(sources, &api);
    let release: LatestRelease = http::get_cached_github_json_from_urls(ctx, &api, &urls).await?;
    let version = release.tag_name.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        return Err(Error::VersionResolve {
            tool: SOURCE_ID.into(),
            spec: "latest".into(),
            hint: Some("the latest release has no tag".into()),
        });
    }
    Ok(version)
}

/// Download, verify, and unpack a release, returning the staging directory that
/// holds the new programs.
///
/// The caller installs from there, so the download and the replacement stay
/// separable: a failed download must never leave a half-replaced pair behind.
pub async fn stage_release(ctx: &Ctx, target: &ReleaseTarget) -> Result<StagedRelease> {
    if ctx.config.settings.offline {
        return Err(Error::other(
            "cannot upgrade osdk while offline; rerun without --offline",
        ));
    }
    let staging = tempfile::Builder::new()
        .prefix("osdk-self-upgrade-")
        .tempdir_in(ctx.dirs.tmp())
        .map_err(|error| Error::io(ctx.dirs.tmp(), error))?;
    let archive = staging.path().join(&target.asset);

    let mut last_error: Option<Error> = None;
    let mut downloaded = false;
    for url in &target.urls {
        match pipeline::download::download(
            &ctx.client,
            url,
            &archive,
            &format!("osdk@{}", target.version),
            ctx.show_progress,
        )
        .await
        {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(error) => {
                tracing::warn!(
                    url = %url,
                    "{}",
                    crate::i18n::trf("log.download_failover", &[("err", &error.to_string())])
                );
                last_error = Some(error);
            }
        }
    }
    if !downloaded {
        return Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: SOURCE_ID.into(),
            tried: target.urls.len(),
        }));
    }

    let checksum = published_checksum(ctx, target).await;
    if let Some(checksum) = &checksum {
        pipeline::verify::verify_file(&archive, &checksum.hex, checksum.algo, &target.asset)?;
        tracing::info!(file = %target.asset, "{}", crate::i18n::tr("log.checksum_verified"));
    } else if ctx.config.settings.require_checksums {
        return Err(Error::other(format!(
            "checksum required but unavailable for osdk@{} ({})",
            target.version, target.asset
        )));
    } else {
        tracing::warn!(file = %target.asset, "{}", crate::i18n::tr("log.self_checksum_missing"));
    }

    let unpacked = staging.path().join("unpacked");
    pipeline::extract::extract(
        &archive,
        &unpacked,
        ArchiveKind::from_name(&target.asset)?,
        true,
    )?;

    let exe_suffix = ctx.platform.os.exe_suffix();
    let mut programs = Vec::new();
    for binary in BINARIES {
        let name = format!("{binary}{exe_suffix}");
        let path = unpacked.join(&name);
        if !path.is_file() {
            return Err(Error::other(format!(
                "release asset {} does not contain {name}",
                target.asset
            )));
        }
        programs.push((name, path));
    }

    Ok(StagedRelease {
        version: target.version.clone(),
        checksum_verified: checksum.is_some(),
        programs,
        _staging: staging,
    })
}

/// A verified, unpacked release waiting to replace the installed programs.
pub struct StagedRelease {
    version: String,
    checksum_verified: bool,
    /// `(file name, staged path)` for every program in the release.
    programs: Vec<(String, PathBuf)>,
    /// Dropped last, removing the staging tree.
    _staging: tempfile::TempDir,
}

impl StagedRelease {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn checksum_verified(&self) -> bool {
        self.checksum_verified
    }
}

/// The directory holding the running `osdk`, which is where `osdk-shim` sits too
/// (both installers put them side by side, and `shim::find_shim_binary` already
/// relies on that).
pub fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|error| Error::other(format!("cannot locate the running osdk: {error}")))?;
    // Resolve the link so an upgrade replaces the real file rather than turning
    // a symlink into a regular file and orphaning the original. `dunce` keeps
    // the result a plain Windows path instead of a `\\?\` one.
    let exe = dunce::canonicalize(&exe).unwrap_or(exe);
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::other("the running osdk has no parent directory"))
}

/// Replace the installed programs with a staged release, all or nothing.
pub fn install(staged: &StagedRelease, install_dir: &Path) -> Result<Vec<String>> {
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut replaced = Vec::new();

    for (name, staged_path) in &staged.programs {
        let destination = install_dir.join(name);
        // A running program cannot be deleted on Windows, and overwriting one
        // in place on Unix can corrupt a process still paging it in. Renaming
        // aside works on both, and leaves the old file recoverable.
        let backup = destination.with_extension(format!("osdk-old-{}", std::process::id()));
        if destination.exists() {
            if let Err(error) = std::fs::rename(&destination, &backup) {
                rollback(&backups);
                return Err(Error::io(&destination, error));
            }
            backups.push((destination.clone(), backup));
        }
        if let Err(error) = copy_program(staged_path, &destination) {
            rollback(&backups);
            return Err(error);
        }
        replaced.push(name.clone());
    }

    // Only once every program is in place: a leftover backup is harmless, while
    // deleting one early would make a later failure unrecoverable.
    for (_, backup) in &backups {
        remove_replaced_program(backup);
    }
    Ok(replaced)
}

/// Copy across filesystems (the staging dir may be on another volume), then set
/// the executable bit the archive carried.
fn copy_program(source: &Path, destination: &Path) -> Result<()> {
    std::fs::copy(source, destination).map_err(|error| Error::io(destination, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| Error::io(destination, error))?;
    }
    Ok(())
}

fn rollback(backups: &[(PathBuf, PathBuf)]) {
    for (destination, backup) in backups.iter().rev() {
        let _ = std::fs::remove_file(destination);
        let _ = std::fs::rename(backup, destination);
    }
}

/// Remove a replaced program, tolerating the Windows case where the old file is
/// still mapped by the running process and cannot be deleted until it exits.
fn remove_replaced_program(backup: &Path) {
    if std::fs::remove_file(backup).is_ok() {
        return;
    }
    tracing::debug!(
        path = %backup.display(),
        "could not remove the replaced program yet; it is left for the next upgrade to clean up"
    );
}

/// Delete `*.osdk-old-*` leftovers from an earlier upgrade whose replaced file
/// was still running at the time.
pub fn clean_replaced_programs(install_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(install_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_leftover = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.starts_with("osdk-old-"));
        if is_leftover && path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The release's published SHA-256 for this asset, if `SHA256SUMS` is reachable.
async fn published_checksum(ctx: &Ctx, target: &ReleaseTarget) -> Option<pipeline::Checksum> {
    for url in &target.checksum_urls {
        let Ok(body) = http::get_text(&ctx.client, url).await else {
            continue;
        };
        if let Some(hex) = pipeline::verify::find_shasum(&body, &target.asset) {
            return Some(pipeline::Checksum {
                algo: pipeline::HashAlgo::Sha256,
                hex: hex.to_string(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Libc;

    fn platform(os: Os, arch: Arch) -> Platform {
        Platform {
            os,
            arch,
            libc: Libc::None,
        }
    }

    #[test]
    fn asset_names_match_the_published_release_matrix() {
        // These are exactly the artifact names publish.yml uploads. A mismatch
        // here is a 404 at upgrade time, which no test of ours would otherwise
        // catch until a user hit it.
        assert_eq!(
            asset_name(platform(Os::Linux, Arch::X64)).unwrap(),
            "osdk-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name(platform(Os::Linux, Arch::Arm64)).unwrap(),
            "osdk-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name(platform(Os::Macos, Arch::X64)).unwrap(),
            "osdk-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name(platform(Os::Macos, Arch::Arm64)).unwrap(),
            "osdk-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name(platform(Os::Windows, Arch::X64)).unwrap(),
            "osdk-x86_64-pc-windows-msvc.zip"
        );
        // No release is published for these, so an upgrade must say so rather
        // than download a 404 page.
        assert!(asset_name(platform(Os::Windows, Arch::Arm64)).is_none());
        assert!(asset_name(platform(Os::Linux, Arch::X86)).is_none());
    }

    #[test]
    fn self_sources_carry_the_same_github_mirror_as_tool_downloads() {
        let sources = default_sources();
        let ids: Vec<&str> = sources.iter().map(|source| source.id.as_str()).collect();
        assert_eq!(ids, vec!["github", "ghproxy"]);
        // The proxy must be able to rewrite both the API and the asset host,
        // otherwise resolving `latest` would bypass the mirror entirely.
        let proxied = http::github_url_candidates(
            &sources,
            "https://api.github.com/repos/lejunyang/one-sdk/releases/latest",
        );
        assert_eq!(
            proxied,
            vec![
                "https://api.github.com/repos/lejunyang/one-sdk/releases/latest".to_string(),
                "https://gh-proxy.com/https://api.github.com/repos/lejunyang/one-sdk/releases/latest"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn probe_measures_the_release_asset_through_each_source() {
        // The probe must hit the same host and path an upgrade downloads from,
        // or a mirror could measure fast on its front page and then fail to
        // serve the asset.
        let sources = default_sources();
        let asset = asset_name(platform(Os::Linux, Arch::X64)).unwrap();
        let official = http::github_url_for_source(&sources[0], &asset_url("0.0.1", &asset));
        let mirrored = http::github_url_for_source(&sources[1], &asset_url("0.0.1", &asset));
        assert_eq!(
            official,
            "https://github.com/lejunyang/one-sdk/releases/download/v0.0.1/osdk-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            mirrored,
            "https://gh-proxy.com/https://github.com/lejunyang/one-sdk/releases/download/v0.0.1/osdk-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn the_probe_window_fits_an_artifact_not_an_index() {
        // Measured against the real sources: github.com answered in 1.4s and a
        // CN proxy in 6.1s, both well past the 1.5s default that suits a
        // version index. Under that default every source times out, all of them
        // are recorded unreachable, and selection quietly falls back to the
        // fixed priority order -- so the mirror choice stops being a
        // measurement at all. Raising the setting must still be honoured.
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        assert_eq!(ctx.config.sources.probe_timeout_ms, 1_500);
        assert!(probe_timeout(&ctx) >= std::time::Duration::from_secs(7));

        ctx.config.sources.probe_timeout_ms = 30_000;
        assert_eq!(probe_timeout(&ctx), std::time::Duration::from_secs(30));
    }

    #[test]
    fn repository_matches_the_package_manifest() {
        // `repository` is what the release URLs are built from. If the manifest
        // moves and this constant does not, every upgrade silently downloads
        // some other project's binaries.
        let repository = env!("CARGO_PKG_REPOSITORY");
        assert_eq!(
            repository.trim_end_matches('/').trim_end_matches(".git"),
            format!("https://github.com/{REPOSITORY}")
        );
    }

    #[test]
    fn only_a_higher_version_counts_as_newer() {
        assert!(is_newer("0.0.2", "0.0.1"));
        assert!(is_newer("v0.1.0", "0.0.9"));
        assert!(!is_newer("0.0.1", "0.0.1"));
        assert!(!is_newer("v0.0.1", "0.0.1"));
        assert!(!is_newer("0.0.1", "0.0.2"));
        // A pre-release sorts below its own release, as semver requires.
        assert!(!is_newer("0.0.2-rc.1", "0.0.2"));
        assert!(is_newer("0.0.2-rc.1", "0.0.1"));
        // An unparseable tag is only "newer" when it differs, never by ordering.
        assert!(is_newer("nightly", "0.0.1"));
        assert!(!is_newer("nightly", "nightly"));
    }

    #[test]
    fn install_replaces_every_program_and_keeps_the_old_ones_recoverable() {
        let temporary = tempfile::tempdir().unwrap();
        let install_dir = temporary.path().join("bin");
        std::fs::create_dir_all(&install_dir).unwrap();
        let exe_suffix = std::env::consts::EXE_SUFFIX;
        for binary in BINARIES {
            std::fs::write(install_dir.join(format!("{binary}{exe_suffix}")), b"old").unwrap();
        }

        let staged = staged_fixture(temporary.path(), b"new");
        let replaced = install(&staged, &install_dir).unwrap();

        assert_eq!(replaced.len(), BINARIES.len());
        for binary in BINARIES {
            let installed = install_dir.join(format!("{binary}{exe_suffix}"));
            assert_eq!(std::fs::read(&installed).unwrap(), b"new");
        }
        // Backups are cleaned up on success, so an upgrade leaves no debris.
        clean_replaced_programs(&install_dir);
        let leftovers: Vec<_> = std::fs::read_dir(&install_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("osdk-old-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_failed_replacement_restores_every_program_it_had_already_moved() {
        // Regression guard for the worst outcome this code can produce: osdk
        // updated, osdk-shim not, which leaves a shim that cannot read the new
        // osdk's installs. The pair must go back to its old state instead.
        let temporary = tempfile::tempdir().unwrap();
        let install_dir = temporary.path().join("bin");
        std::fs::create_dir_all(&install_dir).unwrap();
        let exe_suffix = std::env::consts::EXE_SUFFIX;
        for binary in BINARIES {
            std::fs::write(install_dir.join(format!("{binary}{exe_suffix}")), b"old").unwrap();
        }

        let mut staged = staged_fixture(temporary.path(), b"new");
        // Make the second program unreadable by pointing at a missing file, so
        // the first one has already been replaced when the failure happens.
        let missing = temporary.path().join("missing-program");
        staged.programs[1].1 = missing;

        let error = install(&staged, &install_dir).unwrap_err();
        assert!(matches!(error, Error::Io { .. }), "{error:?}");
        for binary in BINARIES {
            let installed = install_dir.join(format!("{binary}{exe_suffix}"));
            assert_eq!(
                std::fs::read(&installed).unwrap(),
                b"old",
                "{binary} was left replaced after a failed upgrade"
            );
        }
    }

    fn test_ctx(root: &Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config: crate::config::Config::default(),
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }

    fn staged_fixture(root: &Path, contents: &[u8]) -> StagedRelease {
        let staging = tempfile::Builder::new()
            .prefix("osdk-self-upgrade-test-")
            .tempdir_in(root)
            .unwrap();
        let exe_suffix = std::env::consts::EXE_SUFFIX;
        let programs = BINARIES
            .iter()
            .map(|binary| {
                let name = format!("{binary}{exe_suffix}");
                let path = staging.path().join(&name);
                std::fs::write(&path, contents).unwrap();
                (name, path)
            })
            .collect();
        StagedRelease {
            version: "9.9.9".into(),
            checksum_verified: true,
            programs,
            _staging: staging,
        }
    }
}
