//! Conda packages addressed as `conda:<package>`.
//!
//! A conda package is not a self-contained archive. It declares versioned
//! dependencies (`clang` on linux-64 is a 30 KB metapackage whose real content
//! lives in a closure of ~12 packages, 40 MB and counting), so installing one
//! means solving that closure first. That is why this backend carries a SAT
//! solver while every other archive backend does not.
//!
//! The whole module is behind the `install` feature: resolving and unpacking is
//! a CLI concern, and the shim must never link a solver.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::platform::{Arch, Os, Platform};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

/// The channel used when a definition does not name one.
pub const DEFAULT_CHANNEL: &str = "conda-forge";

/// Upstream channel host. Mirrors below serve the same layout under a prefix.
const UPSTREAM_BASE: &str = "https://conda.anaconda.org";

/// Well-known mirrors of the anaconda.org channels.
///
/// These are the same `Source` records every other backend uses, so
/// `osdk source list/test/pin` works here without a second mechanism. They are
/// defaults, not policy: a user overrides them with `osdk source`.
///
/// They deliberately rank *behind* upstream, which is the opposite of what a
/// latency measurement alone would suggest, because latency is not what
/// dominates this backend. Only upstream publishes CEP-16 sharded repodata; the
/// mirrors serve whole-subdir `repodata.json` files. Measured from Beijing on
/// `conda:clang`:
///
/// | source            | list time | repodata cached |
/// |-------------------|-----------|-----------------|
/// | upstream (shards) |     1.8 s |          1.6 MB |
/// | mirror (full)     |    27.6 s |        445.9 MB |
///
/// The mirrors are genuinely faster per byte (4.5 vs 3.4 MB/s for bulk
/// transfers), but that 1.3x cannot pay for 278x the bytes. They stay in the
/// list as failover for when upstream is unreachable, which is the case they
/// actually help with.
///
/// Every entry was verified to serve `<channel>/noarch/repodata.json.zst`.
/// SJTU is deliberately absent: it refuses connections on `mirror.sjtu.edu.cn`
/// and its `mirrors.sjtug.sjtu.edu.cn` host 404s for anaconda paths, so listing
/// it would only spend a probe timeout before failing over.
const MIRRORS: &[(&str, &str, i32)] = &[
    (
        "tuna",
        "https://mirrors.tuna.tsinghua.edu.cn/anaconda/cloud",
        10,
    ),
    ("bfsu", "https://mirrors.bfsu.edu.cn/anaconda/cloud", 20),
    ("nju", "https://mirror.nju.edu.cn/anaconda/cloud", 30),
];

/// A conda "subdir" — the platform key a channel is partitioned by.
///
/// Conda's own spelling, not osdk's: `win-64`, not `windows-x64`. Only the
/// subdirs conda-forge actually builds for are mapped; anything else has no
/// packages and would produce a confusing empty solve rather than a clear
/// "unsupported platform".
pub fn subdir_for(platform: Platform) -> Option<&'static str> {
    match (platform.os, platform.arch) {
        (Os::Linux, Arch::X64) => Some("linux-64"),
        (Os::Linux, Arch::Arm64) => Some("linux-aarch64"),
        (Os::Macos, Arch::X64) => Some("osx-64"),
        (Os::Macos, Arch::Arm64) => Some("osx-arm64"),
        (Os::Windows, Arch::X64) => Some("win-64"),
        // conda-forge publishes no win-arm64 and no 32-bit builds.
        _ => None,
    }
}

/// A channel reference: either a plain name resolved against a base, or an
/// absolute URL for a channel that lives somewhere else entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRef {
    name: String,
}

impl ChannelRef {
    /// Parse one channel token.
    ///
    /// Names are validated rather than passed through because a channel name
    /// becomes a URL path segment; `..` or a scheme here would redirect the
    /// download somewhere the user did not ask for.
    pub fn parse(value: &str) -> Result<Self> {
        let name = value.trim();
        if name.is_empty() {
            return Err(Error::config("conda channel name must not be empty"));
        }
        if name.len() > 128 {
            return Err(Error::config("conda channel name is too long"));
        }
        if name == "." || name == ".." {
            return Err(Error::config(format!(
                "invalid conda channel name `{name}`"
            )));
        }
        let valid = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid {
            return Err(Error::config(format!(
                "invalid conda channel name `{name}`; expected a plain channel \
                 name such as `conda-forge` or `nvidia`"
            )));
        }
        Ok(Self {
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Parse the `channels` option into an ordered, de-duplicated list.
///
/// Order is meaningful: it is the channel priority the solver uses, so
/// `nvidia,conda-forge` and `conda-forge,nvidia` can legitimately resolve to
/// different packages. De-duplication keeps the first occurrence for that
/// reason.
///
/// Multiple channels exist because single-channel solving cannot express real
/// toolchains: `cuda-toolkit` has no `win-64` build on conda-forge and must
/// come from `nvidia`, while its dependencies still come from conda-forge.
pub fn parse_channels(value: &str) -> Result<Vec<ChannelRef>> {
    let mut out: Vec<ChannelRef> = Vec::new();
    for token in value.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let channel = ChannelRef::parse(token)?;
        if !out.iter().any(|existing| existing.name == channel.name) {
            out.push(channel);
        }
    }
    if out.is_empty() {
        return Err(Error::config(
            "conda option `channels` must name at least one channel",
        ));
    }
    if out.len() > 8 {
        return Err(Error::config(
            "conda option `channels` accepts at most 8 channels",
        ));
    }
    Ok(out)
}

/// Directories inside a conda prefix that can hold executables.
///
/// On Windows a conda prefix classically exposes commands from the prefix root,
/// `Scripts\` and `Library\bin\` -- but that list is not sufficient. Packages
/// cross-built from a unix layout (ripgrep is one: its `rg.exe` installs to
/// `bin\rg.exe`) also use `bin\`, so omitting it makes an install that is
/// present on disk look like it exports no commands at all. Verified by
/// installing `conda:ripgrep` on win-64.
///
/// Only existing directories are returned, so a prefix that lacks one of these
/// does not contribute a dead PATH entry.
fn conda_bin_dirs(root: &std::path::Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        candidates.push(root.to_path_buf());
        candidates.push(root.join("Scripts"));
        candidates.push(root.join("Library").join("bin"));
        candidates.push(root.join("bin"));
    } else {
        candidates.push(root.join("bin"));
    }
    candidates.retain(|candidate| candidate.is_dir());
    candidates
}

/// The archive file name for a package URL.
///
/// Taken from the URL's own path rather than from a server-supplied metadata
/// field, and rejected outright if it could escape the scratch directory it is
/// joined onto. A channel is a remote party; a record claiming to be called
/// `../../evil` must not be able to write outside the download dir.
#[cfg(feature = "install")]
fn archive_file_name(url: &url::Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let decoded = percent_decode(name);
    let candidate = decoded.as_str();
    if candidate.is_empty()
        || candidate == "."
        || candidate == ".."
        || candidate.contains(['/', '\\', ':'])
        || candidate.contains('\0')
    {
        return None;
    }
    Some(decoded)
}

/// Decode `%xx` escapes so a percent-encoded separator cannot slip past the
/// checks above after the filesystem sees it.
#[cfg(feature = "install")]
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a channel base is known to publish CEP-16 sharded repodata.
///
/// Sharded indexes let a query fetch only the packages it asked about
/// (`repodata_shards.msgpack.zst` is 0.41 MB) instead of a whole subdir's
/// `repodata.json` (268 MB for win-64). At the time of writing only
/// anaconda.org serves them; the Chinese mirrors return 404 for the shard
/// index and rattler falls back to full repodata.
///
/// This is a positive list rather than a live probe: getting it wrong only
/// costs the fallback that would have happened anyway, whereas probing every
/// base on every invocation would cost a round trip each time.
fn serves_sharded_repodata(base: &str) -> bool {
    let base = base.trim_end_matches('/');
    base == UPSTREAM_BASE || base.ends_with("//conda.anaconda.org")
}

/// Whether a conda version denotes a final release.
///
/// Decided structurally rather than by substring matching. In conda's version
/// grammar an alphabetic component (`Iden`) is what marks a prerelease, so
/// `1.0.1rc1` and `2.0alpha` are correctly excluded while `2024.06.1` and the
/// `b` in a hypothetical numeric-only version are not misread. A plain
/// `contains("rc")` test would wrongly flag names like `1.0.0-src`.
#[cfg(feature = "install")]
fn is_stable_version(version: &rattler_conda_types::Version) -> bool {
    if version.is_dev() {
        return false;
    }
    // The local segment (after `+`) is build metadata, not a prerelease
    // marker, so only the version proper is inspected.
    !version
        .segments()
        .flat_map(|segment| segment.components())
        .any(|component| component.as_iden().is_some())
}

/// A conda package installed as its own prefix.
#[derive(Debug)]
pub struct CondaBackend {
    id: String,
    package: String,
}

impl CondaBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let id = crate::tool::canonical_dynamic_id(id).ok()?;
        let package = id.strip_prefix("conda:")?.to_string();
        Some(Self { id, package })
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    /// The channels this request solves against, honouring the `channels`
    /// option and falling back to the default channel.
    pub fn channels(&self, options: &BTreeMap<String, String>) -> Result<Vec<ChannelRef>> {
        match options.get("channels") {
            Some(value) => parse_channels(value),
            None => Ok(vec![ChannelRef::parse(DEFAULT_CHANNEL)?]),
        }
    }

    /// Choose the base URL to fetch repodata from.
    ///
    /// A pin (or any non-`Auto` selection) is a deliberate user choice and is
    /// always honoured, even when it costs the sharded index. Otherwise the
    /// first source known to serve shards wins, because that difference is
    /// worth far more than the latency the probe measured.
    #[cfg(feature = "install")]
    fn metadata_base(&self, ctx: &Ctx, sources: &[Source]) -> String {
        // The pin is recorded against this backend's full id (`conda:clang`),
        // not the bare namespace, so it has to be looked up by `self.id()`.
        let user_chose = !matches!(ctx.config.sources.selection, crate::source::Selection::Auto)
            || ctx
                .config
                .tool_sources(self.id())
                .is_some_and(|tool| tool.pin.is_some());
        if !user_chose {
            if let Some(sharded) = sources
                .iter()
                .find(|source| serves_sharded_repodata(&source.download_url))
            {
                return sharded.download_url.trim_end_matches('/').to_string();
            }
        }
        sources
            .first()
            .map(|source| source.download_url.trim_end_matches('/').to_string())
            .unwrap_or_else(|| UPSTREAM_BASE.to_string())
    }

    /// metadata from.
    ///
    /// Pointing the alias at one base is what makes mirroring work for every
    /// channel at once: plain names such as `conda-forge` and `nvidia` resolve
    /// beneath it, so a single choice covers a multi-channel solve without
    /// rewriting each channel individually.
    ///
    /// Source order here is *not* the generic latency ranking. The probe
    /// measures round-trip time to one small file, which would pick a nearby
    /// mirror and then pay 445 MB of whole-subdir `repodata.json` for it; the
    /// sharded index upstream costs 1.6 MB for the same answer. So an
    /// explicitly pinned or user-configured source is honoured, and otherwise
    /// the sharded source is preferred, with the ranked list kept as failover.
    ///
    /// The gateway caches under osdk's own cache dir rather than `~/.conda`,
    /// so this never disturbs a conda installation the user also runs.
    #[cfg(feature = "install")]
    async fn gateway(
        &self,
        ctx: &Ctx,
    ) -> Result<(
        rattler_repodata_gateway::Gateway,
        rattler_conda_types::ChannelConfig,
    )> {
        use rattler_conda_types::ChannelConfig;
        use rattler_repodata_gateway::Gateway;

        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let base = self.metadata_base(ctx, &sources);
        let alias = url::Url::parse(&format!("{base}/")).map_err(|error| {
            Error::config(format!("invalid conda channel base `{base}`: {error}"))
        })?;

        let channel_config = ChannelConfig {
            channel_alias: alias,
            root_dir: ctx.dirs.cache.join("conda"),
        };
        let gateway = Gateway::builder()
            .with_cache_dir(ctx.dirs.cache.join("conda").join("repodata"))
            .with_channel_config(rattler_repodata_gateway::ChannelConfig::default())
            .finish();
        Ok((gateway, channel_config))
    }

    /// Download and unpack every solved package into one prefix.
    ///
    /// Conda's own model is that a prefix is the union of its packages, so all
    /// of them extract into the same directory rather than into per-package
    /// subdirectories.
    ///
    /// Downloads go through osdk's pipeline rather than rattler's HTTP stack:
    /// it already does resumable transfers, retries and progress reporting, and
    /// reusing it keeps conda downloads behaving like every other backend's.
    /// Each archive is verified against the sha256 from `repodata.json` before
    /// it is unpacked -- note that anaconda.org's *file metadata* API reports an
    /// empty sha256, so repodata is the only usable source for these digests.
    #[cfg(feature = "install")]
    async fn materialize(
        &self,
        ctx: &Ctx,
        records: &[rattler_conda_types::RepoDataRecord],
        prefix: &std::path::Path,
    ) -> Result<()> {
        let scratch = prefix.join(".osdk-download");
        std::fs::create_dir_all(&scratch).map_err(|error| Error::io(&scratch, error))?;

        for record in records {
            let file_name = archive_file_name(&record.url).ok_or_else(|| {
                Error::other(format!(
                    "conda record has no usable file name: {}",
                    record.url
                ))
            })?;
            let file_name = file_name.as_str();
            let archive = scratch.join(file_name);

            crate::pipeline::download::download(
                &ctx.client,
                record.url.as_str(),
                &archive,
                file_name,
                ctx.show_progress,
            )
            .await?;

            match record.package_record.sha256 {
                Some(digest) => crate::pipeline::verify::verify_file(
                    &archive,
                    &hex::encode(digest),
                    crate::pipeline::HashAlgo::Sha256,
                    file_name,
                )?,
                // Refuse rather than install unverified bytes: every
                // conda-forge record carries a sha256, so a missing one means
                // something is wrong with the channel, not with this code.
                None => {
                    return Err(Error::other(format!(
                        "conda package `{file_name}` has no sha256 in repodata; refusing to \
                         install unverified bytes"
                    )));
                }
            }

            let target = prefix.to_path_buf();
            let archive_for_task = archive.clone();
            // Extraction is CPU-bound and synchronous; keep it off the runtime.
            tokio::task::spawn_blocking(move || {
                rattler_package_streaming::fs::extract(&archive_for_task, &target)
            })
            .await
            .map_err(|error| Error::other(format!("conda extraction task failed: {error}")))?
            .map_err(|error| {
                Error::other(format!("could not extract conda package `{file_name}`: {error}"))
            })?;

            // The archives are large (a single win-64 clang is 132 MB) and the
            // CAS does not own them, so they go as soon as they are unpacked.
            let _ = std::fs::remove_file(&archive);
        }

        std::fs::remove_dir_all(&scratch).map_err(|error| Error::io(&scratch, error))?;
        Ok(())
    }

    /// Solve the dependency closure for one exact version.
    ///
    /// This is the step that makes conda different from every other backend
    /// here. `conda:clang` at 23.1.1 is a 0.03 MB metapackage on linux-64; the
    /// compiler is in its dependencies, expressed as constraints like
    /// `clang-23 ==23.1.1 default_h7037f76_0` and `libstdcxx >=15`. Nothing
    /// short of a real solve produces a working install.
    ///
    /// Virtual packages describe the host (glibc version, macOS SDK, CUDA
    /// driver). Without them the solver cannot evaluate constraints such as
    /// `__glibc >=2.17` and rejects packages that would in fact run here.
    #[cfg(feature = "install")]
    async fn solve(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Vec<rattler_conda_types::RepoDataRecord>> {
        use rattler_conda_types::{MatchSpec, ParseStrictness};
        use rattler_solve::{SolverImpl as _, SolverTask, resolvo::Solver};

        let (gateway, channel_config) = self.gateway(ctx).await?;
        let channels = self.resolved_channels(&tv.options, &channel_config)?;
        let platforms = Self::query_platforms(ctx)?;

        // Pin the exact version the user asked for, and let the solver choose
        // the build. `==` would also demand an exact build string.
        let spec_text = format!("{}={}", self.package, tv.version);
        let spec = MatchSpec::from_str(&spec_text, ParseStrictness::Lenient)
            .map_err(|error| Error::config(format!("invalid conda spec `{spec_text}`: {error}")))?;

        // Recursive: the whole closure is needed, not just the root match.
        let available = gateway
            .query(channels, platforms, [spec.clone()])
            .recursive(true)
            .await
            .map_err(|error| Error::other(format!("conda repodata query failed: {error}")))?;

        let virtual_packages = rattler_virtual_packages::VirtualPackage::detect(
            &rattler_virtual_packages::VirtualPackageOverrides::default(),
            Some(&ctx.dirs.cache.join("conda").join("virtual-packages")),
        )
        .map_err(|error| Error::other(format!("could not detect virtual packages: {error}")))?
        .into_iter()
        .map(rattler_conda_types::GenericVirtualPackage::from)
        .collect();

        let task = SolverTask {
            virtual_packages,
            specs: vec![spec],
            // Strict priority is what makes `channels = ["nvidia", ...]`
            // meaningful: nvidia's candidates are exhausted before falling
            // back, so an explicitly preferred channel actually wins.
            channel_priority: rattler_solve::ChannelPriority::Strict,
            ..SolverTask::from_iter(&available)
        };

        let solved = Solver
            .solve(task)
            .map_err(|error| Error::other(format!("conda dependency solve failed: {error}")))?;
        Ok(solved.records)
    }

    /// Resolve the configured channels against a channel config.
    #[cfg(feature = "install")]
    fn resolved_channels(
        &self,
        options: &BTreeMap<String, String>,
        channel_config: &rattler_conda_types::ChannelConfig,
    ) -> Result<Vec<rattler_conda_types::Channel>> {
        use rattler_conda_types::Channel;

        self.channels(options)?
            .iter()
            .map(|channel| {
                Channel::from_str(channel.name(), channel_config).map_err(|error| {
                    Error::config(format!(
                        "invalid conda channel `{}`: {error}",
                        channel.name()
                    ))
                })
            })
            .collect()
    }

    /// The subdirs a solve must consider: the platform's own, plus `noarch`.
    ///
    /// `noarch` is not optional. Pure-Python and data-only packages live there
    /// exclusively, so leaving it out makes ordinary dependency closures
    /// unsolvable rather than merely incomplete.
    #[cfg(feature = "install")]
    fn query_platforms(ctx: &Ctx) -> Result<Vec<rattler_conda_types::Platform>> {
        use std::str::FromStr as _;

        let subdir = subdir_for(ctx.platform).ok_or_else(|| {
            Error::other(format!(
                "conda packages are not published for {}",
                ctx.platform
            ))
        })?;
        let native = rattler_conda_types::Platform::from_str(subdir).map_err(|error| {
            Error::other(format!("unsupported conda subdir `{subdir}`: {error}"))
        })?;
        Ok(vec![native, rattler_conda_types::Platform::NoArch])
    }
}

#[async_trait]
impl Backend for CondaBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        let mut sources = vec![Source::official("anaconda", UPSTREAM_BASE)];
        for (id, base, priority) in MIRRORS {
            sources.push(Source::mirror(id, base, *priority));
        }
        sources
    }

    fn probe_url(&self, ctx: &Ctx, source: &Source) -> Option<String> {
        // `noarch/repodata.json.zst` exists on every mirror that carries the
        // channel at all, so a probe that fetches it measures the path an
        // install actually uses rather than the host's front page.
        let _ = ctx;
        Some(format!(
            "{}/{DEFAULT_CHANNEL}/noarch/repodata.json.zst",
            source.download_url.trim_end_matches('/')
        ))
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        use rattler_conda_types::{MatchSpec, PackageName, ParseStrictness};
        use std::str::FromStr as _;

        let (gateway, channel_config) = self.gateway(ctx).await?;
        let channels = self.resolved_channels(&BTreeMap::new(), &channel_config)?;
        let platforms = Self::query_platforms(ctx)?;

        let name = PackageName::from_str(&self.package)
            .map_err(|error| Error::config(format!("invalid conda package name: {error}")))?;
        let spec = MatchSpec::from_str(name.as_normalized(), ParseStrictness::Lenient)
            .map_err(|error| Error::config(format!("invalid conda match spec: {error}")))?;

        // Listing deliberately does not recurse: the user asked which versions
        // of this package exist, not what its dependencies would drag in.
        let records = gateway
            .query(channels, platforms, [spec])
            .recursive(false)
            .await
            .map_err(|error| Error::other(format!("conda repodata query failed: {error}")))?;

        // Sorting and prerelease detection both use conda's own version
        // ordering. Conda versions are not semver (`1.0.1rc1`, `2024.06.1`,
        // epochs such as `1!1.2`), so parsing them as semver would order them
        // wrongly and mislabel release candidates as stable.
        let mut versions: Vec<rattler_conda_types::Version> = Vec::new();
        for repo in records.iter() {
            for record in repo.iter() {
                let version = record.package_record.version.version().clone();
                if !versions.contains(&version) {
                    versions.push(version);
                }
            }
        }
        if versions.is_empty() {
            return Err(Error::other(format!(
                "no conda package `{}` found in the configured channels",
                self.package
            )));
        }
        versions.sort();
        Ok(versions
            .into_iter()
            .map(|version| VersionInfo {
                stable: is_stable_version(&version),
                version: version.to_string(),
                lts: None,
            })
            .collect())
    }

    #[cfg(feature = "install")]
    async fn install(&self, ctx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ctx.ctx;
        let records = self.solve(ctx, tv).await?;
        let root = ctx.dirs.install_path(self.id(), &tv.version);

        // Build into a scratch prefix and move it into place only once every
        // package is unpacked, so an interrupted install never leaves a
        // half-populated prefix that later looks complete.
        let staging = ctx
            .dirs
            .installs
            .join(crate::dirs::sanitize_tool_id(self.id()))
            .join(format!(
                ".staging-{}",
                crate::dirs::sanitize_version_component(&tv.version)
            ));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|error| Error::io(&staging, error))?;
        }
        std::fs::create_dir_all(&staging).map_err(|error| Error::io(&staging, error))?;

        let result = self.materialize(ctx, &records, &staging).await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
            return result;
        }

        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|error| Error::io(&root, error))?;
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }
        std::fs::rename(&staging, &root).map_err(|error| Error::io(&root, error))?;

        // Only now is the version usable: `list_installed`, `is_installed` and
        // the shim all treat this marker as the completion signal, so writing
        // it before the rename would advertise a prefix that is not there yet.
        crate::pipeline::write_complete_marker(&ctx.dirs, self.id(), &tv.version)?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        Ok(conda_bin_dirs(&root))
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(crate::backend::bin_names_in_dirs(
            &self.bin_paths(ctx, tv)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_package_id() {
        let backend = CondaBackend::from_id("conda:cuda-toolkit").expect("valid id");
        assert_eq!(backend.id(), "conda:cuda-toolkit");
        assert_eq!(backend.package(), "cuda-toolkit");
    }

    /// Subdirs use conda's spelling, and platforms conda-forge does not build
    /// for report that rather than solving to an empty set.
    #[test]
    fn maps_only_platforms_conda_actually_builds_for() {
        let cases = [
            (Os::Linux, Arch::X64, Some("linux-64")),
            (Os::Linux, Arch::Arm64, Some("linux-aarch64")),
            (Os::Macos, Arch::X64, Some("osx-64")),
            (Os::Macos, Arch::Arm64, Some("osx-arm64")),
            (Os::Windows, Arch::X64, Some("win-64")),
            // No conda-forge builds exist for these.
            (Os::Windows, Arch::Arm64, None),
            (Os::Linux, Arch::X86, None),
        ];
        for (os, arch, expected) in cases {
            assert_eq!(
                subdir_for(Platform {
                    os,
                    arch,
                    libc: crate::platform::Libc::None,
                }),
                expected,
                "{os:?}/{arch:?}"
            );
        }
    }

    /// Channel order is solver priority, so it must survive parsing intact.
    /// `nvidia,conda-forge` is the combination CUDA needs on Windows, where
    /// conda-forge has no `cuda-toolkit` build at all.
    #[test]
    fn channel_order_is_preserved_and_duplicates_collapse_to_the_first() {
        let channels = parse_channels("nvidia, conda-forge ,nvidia").unwrap();
        let names: Vec<_> = channels.iter().map(ChannelRef::name).collect();
        assert_eq!(names, ["nvidia", "conda-forge"]);
    }

    /// A channel name becomes a URL path segment, so traversal and schemes are
    /// rejected instead of redirecting the download.
    #[test]
    fn channel_names_that_could_redirect_a_download_are_rejected() {
        for bad in [
            "",
            "..",
            ".",
            "conda forge",
            "https://evil.test/pkgs",
            "conda-forge/../../etc",
            "chan\nnel",
        ] {
            assert!(
                ChannelRef::parse(bad).is_err(),
                "`{bad}` should have been rejected"
            );
        }
        for good in ["conda-forge", "nvidia", "bioconda", "my_team", "pkgs.main"] {
            assert!(ChannelRef::parse(good).is_ok(), "`{good}` should be valid");
        }
    }

    #[test]
    fn an_empty_channel_list_is_an_error_rather_than_a_silent_default() {
        assert!(parse_channels("").is_err());
        assert!(parse_channels("  , ,  ").is_err());
    }

    /// Regression: `conda:ripgrep` on win-64 installs its executable to
    /// `bin\rg.exe`, not to `Scripts\` or the prefix root. An earlier revision
    /// listed only the three classic Windows locations, so a correctly
    /// installed tool appeared to export no commands. Every candidate must be
    /// searched on Windows, and only existing dirs may be returned.
    #[test]
    fn windows_prefixes_include_bin_not_just_the_classic_conda_locations() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();

        let dirs = conda_bin_dirs(root);
        assert!(
            dirs.contains(&root.join("bin")),
            "bin/ must be searched on every platform; got {dirs:?}"
        );
        // Non-existent candidates must not become dead PATH entries.
        assert!(!dirs.contains(&root.join("Scripts")));
        assert!(!dirs.contains(&root.join("Library").join("bin")));
    }

    /// A channel is a remote party. The archive name becomes a path under the
    /// scratch dir, so a record that names itself `../../evil` must be refused
    /// rather than allowed to write outside it -- including when the separator
    /// arrives percent-encoded.
    #[test]
    fn archive_names_that_could_escape_the_download_dir_are_refused() {
        let ok = |raw: &str| archive_file_name(&url::Url::parse(raw).unwrap());

        assert_eq!(
            ok("https://conda.anaconda.org/conda-forge/win-64/clang-23.1.1-h1.conda").as_deref(),
            Some("clang-23.1.1-h1.conda")
        );
        assert_eq!(
            ok("https://conda.anaconda.org/conda-forge/linux-64/x-1.tar.bz2").as_deref(),
            Some("x-1.tar.bz2")
        );

        for hostile in [
            // Trailing slash: no file component at all.
            "https://conda.anaconda.org/conda-forge/win-64/",
            // Percent-encoded separators must not survive decoding.
            "https://conda.anaconda.org/c/%2E%2E%2Fevil",
            "https://conda.anaconda.org/c/%2Fetc%2Fpasswd",
            "https://conda.anaconda.org/c/a%5Cb",
            // A bare dot-dot segment.
            "https://conda.anaconda.org/c/..",
        ] {
            assert!(
                ok(hostile).is_none(),
                "`{hostile}` should not yield a usable file name"
            );
        }
    }

    /// The whole point of preferring upstream: only it serves shards. Getting
    /// this wrong costs 445.9 MB and 27.6 s instead of 1.6 MB and 1.8 s.
    #[test]
    fn only_upstream_is_known_to_serve_sharded_repodata() {
        assert!(serves_sharded_repodata(UPSTREAM_BASE));
        assert!(serves_sharded_repodata("https://conda.anaconda.org/"));
        for mirror in MIRRORS {
            assert!(
                !serves_sharded_repodata(mirror.1),
                "{} does not serve shards and must not be treated as if it did",
                mirror.0
            );
        }
    }

    /// Mirrors must rank behind upstream. `Source::official` uses priority 0
    /// and lower wins, so a mirror with a *smaller* number would quietly
    /// become the metadata source and drag in whole-subdir repodata.
    #[test]
    fn mirrors_rank_behind_upstream() {
        let backend = CondaBackend::from_id("conda:ruff").unwrap();
        let sources = backend.default_sources();
        let official = sources
            .iter()
            .find(|source| source.kind == crate::source::SourceKind::Official)
            .expect("an upstream source");
        assert_eq!(official.download_url, UPSTREAM_BASE);
        for source in sources
            .iter()
            .filter(|source| source.kind == crate::source::SourceKind::Mirror)
        {
            assert!(
                source.priority > official.priority,
                "{} must not outrank upstream",
                source.id
            );
        }
    }
}
