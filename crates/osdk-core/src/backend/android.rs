//! Android SDK backend: installs Android SDK packages (NDK, platform-tools,
//! build-tools, cmdline-tools, cmake) from Google's repository.
//!
//! ## Tool ids
//!
//! Packages are addressed as `android-<family>`, e.g. `android-platform-tools`
//! or `android-ndk`. The version is the package's manifest revision, so
//! `osdk install android-ndk@29.0.14206865` maps to the manifest path
//! `ndk;29.0.14206865`.
//!
//! These are fixed ids rather than a `android:<subject>` namespace: the
//! family list is curated and ships with osdk, whereas the `:` form is
//! reserved for dynamic backends whose subject is an arbitrary upstream
//! package name.
//!
//! ## Integrity
//!
//! The manifests publish a per-archive SHA-1 and nothing stronger. That is
//! weaker than every other osdk source, so this backend is the only place
//! [`HashAlgo::Sha1`] is produced, and it always supplies a checksum rather than
//! ever installing unverified bytes.
//!
//! ## Licensing
//!
//! Most packages are gated behind an agreement. Installation refuses to proceed
//! until the caller has explicitly accepted, either on this invocation or in a
//! recorded prior one; see [`crate::android::license`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::android::license::{self, Acceptance};
use crate::android::repo::{self, Channel, Manifest, RemotePackage};
use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

/// Tool id prefix for Android SDK packages.
pub const ID_PREFIX: &str = "android-";

/// Option key used to pass blanket license acceptance through a `ToolVersion`.
pub const ACCEPT_ALL_OPTION: &str = "accept-licenses";

/// Option key used to pass specific accepted license ids (comma separated).
pub const ACCEPT_IDS_OPTION: &str = "accept-license";

/// Option key used to opt into non-stable channels.
pub const CHANNEL_OPTION: &str = "channel";

/// The package families osdk exposes.
///
/// Deliberately curated rather than "every family in the manifest":
/// `ndk-bundle` is the superseded pre-side-by-side layout and including it
/// alongside `ndk` would only create ambiguity, and the system-image families
/// live in separate sub-site manifests.
pub const SUPPORTED_FAMILIES: &[&str] = &[
    "platform-tools",
    "cmdline-tools",
    "build-tools",
    "ndk",
    "cmake",
    "platforms",
    "emulator",
    "sources",
];

pub struct AndroidBackend {
    family: &'static str,
    /// Precomputed `android-<family>` id, owned so `id()` can borrow it.
    id: String,
}

impl AndroidBackend {
    pub fn new(family: &'static str) -> AndroidBackend {
        AndroidBackend {
            family,
            id: format!("{ID_PREFIX}{family}"),
        }
    }

    /// All backends osdk should register.
    pub fn all() -> Vec<AndroidBackend> {
        SUPPORTED_FAMILIES
            .iter()
            .map(|family| AndroidBackend::new(family))
            .collect()
    }

    /// The tool id, e.g. `android-ndk`.
    fn tool_id(&self) -> &str {
        &self.id
    }

    /// The manifest package family this backend manages.
    pub fn family(&self) -> &'static str {
        self.family
    }

    /// Whether `id` names an Android SDK backend.
    pub fn owns_id(id: &str) -> bool {
        id.strip_prefix(ID_PREFIX)
            .is_some_and(|family| SUPPORTED_FAMILIES.contains(&family))
    }

    /// Fetch and parse the newest repository manifest available from `sources`.
    ///
    /// Probes generations newest-first and returns the first that parses. Older
    /// generations stay online indefinitely, so this both finds the current one
    /// and keeps working when Google publishes a newer one.
    pub async fn manifest(&self, ctx: &Ctx) -> Result<Manifest> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            for generation in repo::MANIFEST_GENERATIONS {
                let url = repo::manifest_url(&source.download_url, *generation);
                let xml = match http::get_cached_text(ctx, &url).await {
                    Ok(body) => body,
                    Err(error) => {
                        // A missing generation is the normal case for the
                        // probes above the current one; keep descending.
                        last_err = Some(error);
                        continue;
                    }
                };
                match repo::parse_manifest(&xml) {
                    Ok(manifest) => return Ok(manifest),
                    Err(error) => {
                        tracing::warn!(
                            source = %source.id,
                            generation = *generation,
                            "android manifest did not parse: {error}"
                        );
                        last_err = Some(error);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.tool_id().to_string(),
            tried: sources.len(),
        }))
    }

    /// The highest channel a request opted into. Defaults to stable only.
    fn requested_channel(tv: &ToolVersion) -> Channel {
        tv.options
            .get(CHANNEL_OPTION)
            .and_then(|value| match value.trim() {
                "stable" => Some(Channel::Stable),
                "beta" => Some(Channel::Beta),
                "dev" => Some(Channel::Dev),
                "canary" => Some(Channel::Canary),
                _ => None,
            })
            .unwrap_or(Channel::Stable)
    }

    /// Reconstruct the acceptance the caller passed through install options.
    fn acceptance(tv: &ToolVersion) -> Acceptance {
        let accept_all = tv
            .options
            .get(ACCEPT_ALL_OPTION)
            .map(|value| value != "false")
            .unwrap_or(false);
        let ids: Vec<String> = tv
            .options
            .get(ACCEPT_IDS_OPTION)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Acceptance::from_flags(accept_all, &ids)
    }

    /// Resolve the manifest package for a concrete version.
    pub fn package<'m>(&self, manifest: &'m Manifest, version: &str) -> Result<&'m RemotePackage> {
        // Families with a version tail address packages as `family;version`;
        // single-package families (platform-tools) have no tail.
        if let Some(found) = manifest.package(&format!("{};{version}", self.family)) {
            return Ok(found);
        }
        if let Some(found) = manifest
            .family(self.family)
            .into_iter()
            .find(|p| p.version() == version)
        {
            return Ok(found);
        }
        Err(Error::VersionResolve {
            tool: self.tool_id().to_string(),
            spec: version.to_string(),
            hint: Some(format!(
                "no {} package with this revision in the Android repository",
                self.family
            )),
        })
    }

    /// Where license acceptances are recorded for this installation.
    ///
    /// A single shared root (rather than per-package) so that an acceptance
    /// carries across packages the way it does for a conventional SDK install,
    /// and so Gradle can be pointed at one directory.
    pub fn sdk_root(ctx: &Ctx) -> PathBuf {
        ctx.dirs.installs.join("android-sdk")
    }
}

#[async_trait]
impl Backend for AndroidBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("google", repo::GOOGLE_REPO_ROOT),
            // Verified 2026-09 as the only reachable public mirror, serving a
            // byte-identical manifest. Ranked below the official source because
            // measured throughput was lower; source selection re-probes anyway.
            Source::mirror(
                "tencent",
                "https://mirrors.cloud.tencent.com/AndroidSDK/",
                10,
            ),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        // The manifest itself is the natural probe target: every source must
        // serve it, and its size is representative.
        Some(repo::manifest_url(&source.download_url, 4))
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let manifest = self.manifest(ctx).await?;
        let packages = manifest.family(self.family);
        if packages.is_empty() {
            return Err(Error::other(format!(
                "the Android repository lists no `{}` packages",
                self.family
            )));
        }
        Ok(packages
            .into_iter()
            .filter(|package| !package.obsolete)
            .map(|package| VersionInfo {
                version: package.version(),
                stable: package.channel == Channel::Stable,
                lts: None,
            })
            .collect())
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        // A locked plan pins the exact artifact, but it must never stand in for
        // consent: lock files are committed and replayed on other machines, and
        // deliberately carry no acceptance. Every machine agrees for itself, so
        // the gate runs before the plan is replayed rather than after.
        let locked = pipeline::locked_install_plan(self.id(), tv, true)?;
        let manifest = self.manifest(ctx).await?;
        let sdk_root = Self::sdk_root(ctx);
        let acceptance = Self::acceptance(tv);

        // Google prunes superseded revisions, so a pinned version can outlive
        // its manifest entry. That is not a reason to skip the gate.
        let package = match self.package(&manifest, &tv.version) {
            Ok(package) => Some(package),
            Err(error) => {
                if locked.is_none() {
                    return Err(error);
                }
                None
            }
        };

        match package {
            Some(package) => {
                // Refuse non-stable packages unless the request opted in, so a
                // preview build is never installed by a bare version request.
                let allowed = Self::requested_channel(tv);
                if package.channel > allowed {
                    return Err(Error::other(format!(
                        "{} is published on the {} channel; pass channel={} to install it",
                        package.path,
                        package.channel.as_str(),
                        package.channel.as_str()
                    )));
                }

                // License gate, before any bytes are fetched.
                let pending = license::pending(&manifest, &[package], &acceptance, &sdk_root);
                if !pending.is_empty() {
                    return Err(license::blocked_error(&pending));
                }
                license::record_accepted(&manifest, &[package], &acceptance, &sdk_root)?;
            }
            None => {
                // Without a manifest entry the applicable agreement is unknown,
                // so no stored record can prove consent. Require it in this
                // invocation instead of assuming it.
                if acceptance == Acceptance::None {
                    return Err(Error::other(format!(
                        "{} {} is pinned by the lock file but is no longer in Google's \\
manifest, so the license it requires cannot be determined; pass \\
accept-licenses=true to install it anyway",
                        self.id(),
                        tv.version
                    )));
                }
            }
        }

        if let Some(plan) = locked {
            pipeline::run(&plan, &pipeline_ctx(ctx)).await?;
            return Ok(());
        }

        let package = self.package(&manifest, &tv.version)?;

        let archive = package.archive_for(&ctx.platform).ok_or_else(|| {
            Error::other(format!(
                "{} has no archive for {}",
                package.path,
                repo::host_os_token(ctx.platform.os)
            ))
        })?;

        let urls: Vec<String> = crate::source::select::ranked_source_list(ctx, self)
            .await?
            .iter()
            .map(|source| http::join_url(&source.download_url, &archive.url))
            .collect();

        let file_name = archive
            .url
            .rsplit('/')
            .next()
            .unwrap_or(&archive.url)
            .to_string();
        let kind = ArchiveKind::from_name(&file_name)?;

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name,
            kind,
            // Android archives wrap their contents in a single directory
            // (`platform-tools/`, `android-ndk-r29/`, ...).
            strip_root: true,
            checksum: Some(archive.pipeline_checksum()),
            subdir: None,
        };
        pipeline::run(&plan, &pipeline_ctx(ctx)).await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        Ok(match self.family {
            // The NDK exposes its drivers through a toolchain directory rather
            // than a top-level `bin`.
            "ndk" => vec![
                root.join("toolchains/llvm/prebuilt")
                    .join(ndk_prebuilt_dir(ctx.platform.os))
                    .join("bin"),
                root.clone(),
            ],
            "cmdline-tools" => vec![root.join("bin")],
            "cmake" => vec![root.join("bin")],
            // platform-tools, build-tools and emulator put executables at the
            // archive root.
            _ => vec![root],
        })
    }

    fn exec_env(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        match self.family {
            "ndk" => {
                // Both names are in active use by different build systems.
                env.insert("ANDROID_NDK_ROOT".into(), root.display().to_string());
                env.insert("ANDROID_NDK_HOME".into(), root.display().to_string());
            }
            _ => {
                // Point tooling at the shared SDK root that holds the license
                // records, not at the individual package directory.
                let sdk_root = Self::sdk_root(ctx);
                env.insert("ANDROID_SDK_ROOT".into(), sdk_root.display().to_string());
            }
        }
        Ok(env)
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if !discovered.is_empty() {
            return Ok(discovered);
        }
        // Before the first install there is nothing to scan, so name the
        // executables each family is expected to provide.
        Ok(match self.family {
            "platform-tools" => vec!["adb".into(), "fastboot".into()],
            "cmdline-tools" => vec!["avdmanager".into(), "sdkmanager".into(), "lint".into()],
            "build-tools" => vec!["aapt2".into(), "d8".into(), "apksigner".into()],
            "emulator" => vec!["emulator".into()],
            "cmake" => vec!["cmake".into()],
            _ => Vec::new(),
        })
    }
}

fn pipeline_ctx<'c>(ctx: &'c Ctx) -> PipelineCtx<'c> {
    PipelineCtx {
        client: &ctx.client,
        dirs: &ctx.dirs,
        cas: &ctx.cas,
        link_mode: ctx.config.settings.link_mode,
        show_progress: ctx.show_progress,
        offline: ctx.config.settings.offline,
        require_checksums: ctx.config.settings.require_checksums,
    }
}

/// The NDK's prebuilt toolchain directory name for the host.
///
/// Always the x86_64 flavour: Google ships no arm64 host toolchain in the
/// Windows/Linux archives, and the macOS one runs under Rosetta on Apple
/// silicon.
fn ndk_prebuilt_dir(os: crate::platform::Os) -> &'static str {
    use crate::platform::Os;
    match os {
        Os::Windows => "windows-x86_64",
        Os::Macos => "darwin-x86_64",
        Os::Linux => "linux-x86_64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_are_namespaced_per_family() {
        assert_eq!(AndroidBackend::new("ndk").tool_id(), "android-ndk");
        assert_eq!(
            AndroidBackend::new("platform-tools").tool_id(),
            "android-platform-tools"
        );
    }

    #[test]
    fn every_supported_family_gets_a_backend() {
        let backends = AndroidBackend::all();
        assert_eq!(backends.len(), SUPPORTED_FAMILIES.len());
        // The superseded pre-side-by-side NDK layout stays out.
        assert!(!SUPPORTED_FAMILIES.contains(&"ndk-bundle"));
        assert!(SUPPORTED_FAMILIES.contains(&"ndk"));
        assert!(SUPPORTED_FAMILIES.contains(&"platform-tools"));
    }

    #[test]
    fn owns_id_recognises_only_supported_families() {
        assert!(AndroidBackend::owns_id("android-ndk"));
        assert!(AndroidBackend::owns_id("android-platform-tools"));
        // Not a curated family.
        assert!(!AndroidBackend::owns_id("android-ndk-bundle"));
        assert!(!AndroidBackend::owns_id("android-nope"));
        // Other backends are untouched.
        assert!(!AndroidBackend::owns_id("node"));
        assert!(!AndroidBackend::owns_id("npm:prettier"));
    }

    #[test]
    fn google_is_the_default_source_and_the_mirror_ranks_below_it() {
        let sources = AndroidBackend::new("ndk").default_sources();
        assert_eq!(sources[0].id, "google");
        assert_eq!(sources[0].download_url, repo::GOOGLE_REPO_ROOT);
        assert_eq!(sources[1].id, "tencent");
        assert!(sources[1].priority > sources[0].priority);
    }

    #[test]
    fn acceptance_defaults_to_nothing_accepted() {
        let tv = ToolVersion::new("android-ndk", "29.0.1");
        assert_eq!(AndroidBackend::acceptance(&tv), Acceptance::None);
    }

    #[test]
    fn accept_all_option_is_recognised() {
        let mut tv = ToolVersion::new("android-ndk", "29.0.1");
        tv.options.insert(ACCEPT_ALL_OPTION.into(), "true".into());
        assert_eq!(AndroidBackend::acceptance(&tv), Acceptance::All);
    }

    #[test]
    fn accept_ids_option_is_split_on_commas() {
        let mut tv = ToolVersion::new("android-ndk", "29.0.1");
        tv.options.insert(
            ACCEPT_IDS_OPTION.into(),
            "android-sdk-license, android-sdk-preview-license".into(),
        );
        let acceptance = AndroidBackend::acceptance(&tv);
        assert!(acceptance.covers("android-sdk-license"));
        assert!(acceptance.covers("android-sdk-preview-license"));
        assert!(!acceptance.covers("some-other-license"));
    }

    #[test]
    fn channel_defaults_to_stable_only() {
        let tv = ToolVersion::new("android-ndk", "29.0.1");
        assert_eq!(AndroidBackend::requested_channel(&tv), Channel::Stable);
        let mut opted = ToolVersion::new("android-ndk", "30.0.1");
        opted.options.insert(CHANNEL_OPTION.into(), "beta".into());
        assert_eq!(AndroidBackend::requested_channel(&opted), Channel::Beta);
        // An unrecognised value must not silently widen the channel.
        let mut bogus = ToolVersion::new("android-ndk", "30.0.1");
        bogus.options.insert(CHANNEL_OPTION.into(), "wat".into());
        assert_eq!(AndroidBackend::requested_channel(&bogus), Channel::Stable);
    }

    #[test]
    fn ndk_prebuilt_dir_matches_google_layout() {
        use crate::platform::Os;
        // Names come from the NDK archive layout, which is x86_64 even on arm
        // hosts (Rosetta / emulation).
        assert_eq!(ndk_prebuilt_dir(Os::Windows), "windows-x86_64");
        assert_eq!(ndk_prebuilt_dir(Os::Macos), "darwin-x86_64");
        assert_eq!(ndk_prebuilt_dir(Os::Linux), "linux-x86_64");
    }
}
