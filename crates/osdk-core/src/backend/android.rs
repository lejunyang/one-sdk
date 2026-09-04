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
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::android::license::{self, Acceptance};
use crate::android::package_xml;
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
/// alongside `ndk` would only create ambiguity, and `emulators` (plural) is a
/// preview-channel side-by-side component that depends on `emulator` and does
/// not fit the one-version-series-per-family model.
pub const SUPPORTED_FAMILIES: &[&str] = &[
    "platform-tools",
    "cmdline-tools",
    "build-tools",
    "ndk",
    "cmake",
    "platforms",
    "emulator",
    "sources",
    SYSTEM_IMAGES_FAMILY,
];

/// The system-image family, published in sub-site manifests rather than the
/// main one.
pub const SYSTEM_IMAGES_FAMILY: &str = "system-images";

pub struct AndroidBackend {
    family: &'static str,
    /// Precomputed `android-<family>` id, owned so `id()` can borrow it.
    id: String,
    /// Dependencies installed during the current `install`, awaiting collection
    /// by the caller. Behind a lock because the registry shares one instance of
    /// each backend across calls.
    side_installed: std::sync::Mutex<Vec<ToolVersion>>,
}

impl AndroidBackend {
    pub fn new(family: &'static str) -> AndroidBackend {
        AndroidBackend {
            family,
            side_installed: std::sync::Mutex::new(Vec::new()),
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
                    Ok(mut manifest) => {
                        // System images are not in the main manifest, so the
                        // sub-sites are only fetched when this backend or a
                        // dependency actually needs them -- six extra requests
                        // is too much to spend on every `osdk install adb`.
                        if self.needs_system_images() {
                            self.merge_system_images(ctx, &source.download_url, &mut manifest)
                                .await;
                        }
                        return Ok(manifest);
                    }
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

    /// Whether this backend's work requires the system-image sub-sites.
    fn needs_system_images(&self) -> bool {
        self.family == SYSTEM_IMAGES_FAMILY
    }

    /// Fold every reachable system-image sub-site into `manifest`.
    ///
    /// A sub-site that cannot be fetched or parsed is logged and skipped: the
    /// variants are independent, and losing (say) `android-tv` must not make
    /// `google_apis` images uninstallable. Mirrors may also legitimately carry
    /// only a subset.
    async fn merge_system_images(&self, ctx: &Ctx, root: &str, manifest: &mut Manifest) {
        for site in repo::SYSTEM_IMAGE_SUB_SITES {
            let mut merged = false;
            for file in site.files {
                let url = site.url(root, file);
                let Ok(xml) = http::get_cached_text(ctx, &url).await else {
                    continue;
                };
                // Archive urls in a sub-site are relative to its own directory,
                // so rebase them onto the repository root while parsing.
                match repo::parse_manifest_with_base(&xml, site.dir) {
                    Ok(sub) => {
                        manifest.merge(sub);
                        merged = true;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            site = site.dir,
                            "system image manifest did not parse: {error}"
                        );
                    }
                }
            }
            if !merged {
                tracing::debug!(site = site.dir, "no usable system image manifest");
            }
        }
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

    #[cfg(feature = "install")]
    /// Install any declared dependencies that are missing or too old.
    ///
    /// Only dependencies inside [`SUPPORTED_FAMILIES`] are actionable; an edge
    /// pointing at a family osdk does not curate is reported and skipped rather
    /// than failing the install, because the requested package itself is still
    /// perfectly installable.
    async fn install_dependencies(
        &self,
        ictx: &InstallCtx<'_>,
        manifest: &Manifest,
        package: &RemotePackage,
    ) -> Result<()> {
        let ctx = ictx.ctx;
        for (dep, min_revision) in manifest.resolve_dependencies(package) {
            let family = dep.family();
            if !SUPPORTED_FAMILIES.contains(&family) {
                tracing::warn!(
                    package = %package.path,
                    dependency = %dep.path,
                    "dependency is outside the families osdk manages; skipping"
                );
                continue;
            }
            let dep_id = format!("{ID_PREFIX}{family}");
            let version = dep.version();

            // Already satisfied? An installed build at or above the declared
            // minimum is enough; reinstalling would waste a large download.
            let Some(static_family) = SUPPORTED_FAMILIES.iter().find(|f| **f == family).copied()
            else {
                tracing::warn!(
                    package = %package.path,
                    dependency = %dep.path,
                    "dependency family is not curated; skipping"
                );
                continue;
            };
            let dep_backend = AndroidBackend::new(static_family);
            if Self::dependency_satisfied(&dep_backend, ctx, min_revision.as_deref()) {
                continue;
            }

            tracing::info!(
                package = %package.path,
                dependency = %dep.path,
                "installing dependency required by the requested package"
            );
            let mut dep_tv = ToolVersion::new(&dep_id, &version);
            // Consent was already gated for the whole closure above, so pass it
            // down rather than re-prompting for a package the user cannot see.
            dep_tv
                .options
                .insert(ACCEPT_ALL_OPTION.to_string(), "true".to_string());
            Box::pin(dep_backend.install(ictx, &dep_tv)).await?;
            // Report it, plus anything it pulled in transitively, so the caller
            // can finish the job (shims) for packages the user never named.
            let mut recorded = dep_backend.take_side_installed();
            recorded.push(dep_tv);
            if let Ok(mut pending) = self.side_installed.lock() {
                pending.extend(recorded);
            }
        }
        Ok(())
    }

    /// Whether an already-installed build satisfies `min_revision`.
    ///
    /// Any complete install counts when the manifest states no bound; when it
    /// does, only a build at or above it counts, because an older emulator is
    /// exactly the case the bound exists to reject.
    fn dependency_satisfied(
        backend: &AndroidBackend,
        ctx: &Ctx,
        min_revision: Option<&str>,
    ) -> bool {
        let Ok(installed) = backend.list_installed(ctx) else {
            return false;
        };
        installed.iter().any(|version| match min_revision {
            None => true,
            Some(min) => repo::compare_versions(version, min) != std::cmp::Ordering::Less,
        })
    }

    /// Whether `meta` describes a reparse point (junction or symlink).
    ///
    /// Windows junctions are directories carrying `FILE_ATTRIBUTE_REPARSE_POINT`;
    /// `is_symlink()` alone does not report them, so a junction we created would
    /// otherwise look like a real directory and never be replaced.
    #[cfg(windows)]
    fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
        false
    }

    /// Write the `package.xml` index Google's tools read, into an installed
    /// package directory.
    ///
    /// ## Why an install has to do this
    ///
    /// osdk knows what it installed, but Google's tools do not ask a manager --
    /// they walk the SDK root and parse a `package.xml` inside each package
    /// directory. Without it a package osdk installed is invisible to them even
    /// though the layout is correct: measured on 37.1.11, `avdmanager create avd`
    /// answers `Package path is not valid. Valid system image paths are:` and
    /// lists nothing, while `sdkmanager --list_installed` shows the same image
    /// happily -- sdkmanager is satisfied by `source.properties`, avdmanager is
    /// not.
    ///
    /// Written into the real install directory rather than through the SDK-root
    /// link so it lives with the payload and survives a relink.
    ///
    /// Never fatal: the index only buys interoperability with Google's tools, so
    /// failing an otherwise good install over it would trade a small gap for a
    /// total one.
    fn write_package_index(
        ctx: &Ctx,
        family: &str,
        version: &str,
        package: Option<&RemotePackage>,
    ) {
        let dir = ctx
            .dirs
            .install_path(&format!("{ID_PREFIX}{family}"), version);
        // The id Google's tools match on. For most families it is the manifest
        // path; a system image's path already contains the whole `;` id.
        let manifest_path = match package {
            Some(package) => package.path.clone(),
            None if family == SYSTEM_IMAGES_FAMILY => {
                format!("{SYSTEM_IMAGES_FAMILY};{version}")
            }
            None => family.to_string(),
        };
        let display_name = package
            .map(|package| package.display_name.clone())
            .unwrap_or_else(|| manifest_path.clone());
        let revision = package
            .map(|package| package.revision.clone())
            .unwrap_or_else(|| version.to_string());
        let license_id = package.and_then(|package| package.license_ref.as_deref());
        match package_xml::write_into(&dir, &manifest_path, &display_name, &revision, license_id) {
            Ok(true) => {}
            Ok(false) => {
                // No `source.properties` means the archive did not describe
                // itself, so any details written would be invented.
                tracing::debug!(
                    family,
                    version,
                    "no source.properties to describe this package; \
                     skipping the package.xml index"
                );
            }
            Err(error) => {
                tracing::warn!(
                    family,
                    version,
                    "could not write the package.xml index Google's tools read: {error}"
                );
            }
        }
    }

    /// Rewrite the `package.xml` index for an already-installed package.
    ///
    /// Exposed for `osdk android sdk-root repair`: packages installed before osdk
    /// wrote this file are invisible to `avdmanager`, and reinstalling gigabytes
    /// to regain an index would be an absurd remedy. Returns whether a file was
    /// written.
    ///
    /// The manifest is deliberately not consulted: repair has to work offline,
    /// and everything the index needs is in the `source.properties` that shipped
    /// inside the archive.
    pub fn repair_package_index(ctx: &Ctx, family: &str, version: &str) -> bool {
        let dir = ctx
            .dirs
            .install_path(&format!("{ID_PREFIX}{family}"), version);
        let manifest_path = if family == SYSTEM_IMAGES_FAMILY {
            format!("{SYSTEM_IMAGES_FAMILY};{version}")
        } else if matches!(family, "platform-tools" | "emulator") {
            family.to_string()
        } else {
            // Versioned families are addressed as `family;version`.
            format!("{family};{version}")
        };
        // Without the manifest the best display name available is the id itself,
        // which is what Google's tools fall back to showing anyway.
        package_xml::write_into(&dir, &manifest_path, &manifest_path, version, None)
            .unwrap_or(false)
    }

    /// Remove the SDK root link a package was exposed through, and any parent
    /// directories the removal leaves empty.
    ///
    /// ## Why this exists
    ///
    /// `link_into_sdk_root` has to have a counterpart. Without one, uninstalling
    /// leaves a junction whose target is gone: measured, the link still answers
    /// `Test-Path sdk\platform-tools` = True while
    /// `sdk\platform-tools\adb.exe` = False. A dangling entry is worse than a
    /// missing one, because everything that probes the layout by existence --
    /// the emulator's SDK root check, `avdmanager`, Gradle's
    /// `sdk.dir` -- concludes the package is present and then fails deeper in,
    /// with an error that points at the SDK rather than at the uninstall.
    ///
    /// Returns whether a link was removed.
    pub fn unlink_from_sdk_root(ctx: &Ctx, family: &str, version: &str) -> bool {
        let Some(relative) = Self::sdk_root_relative_path(family, version) else {
            return false;
        };
        let root = Self::sdk_root(ctx);
        let link = root.join(&relative);
        // `symlink_metadata` deliberately: a dangling junction has no metadata
        // through `metadata()`, and that is exactly the case being cleaned up.
        let Ok(meta) = link.symlink_metadata() else {
            return false;
        };
        let is_link = meta.file_type().is_symlink() || Self::is_reparse_point(&meta);
        if !is_link {
            // A real directory here was never osdk's to delete -- it is either
            // sdkmanager's own copy or user data. Leave it and say so.
            tracing::warn!(
                family,
                version,
                path = %link.display(),
                "not removing this SDK root entry: it is a real directory, not a link \
                 osdk created"
            );
            return false;
        }
        // Removing a junction unlinks it without touching the target. The target
        // is usually already gone by this point, which is the whole point.
        if std::fs::remove_dir(&link)
            .or_else(|_| std::fs::remove_file(&link))
            .is_err()
        {
            return false;
        }
        // Families like `ndk/<version>` and `system-images/<a>/<b>/<c>` nest, so
        // the link's removal can leave empty scaffolding behind. Prune upwards,
        // stopping at the root itself and at the first non-empty directory.
        let mut parent = link.parent().map(Path::to_path_buf);
        while let Some(dir) = parent {
            if dir == root || !dir.starts_with(&root) {
                break;
            }
            let empty = std::fs::read_dir(&dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if !empty || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            parent = dir.parent().map(Path::to_path_buf);
        }
        true
    }

    /// SDK root links whose target no longer exists.
    ///
    /// Read-only counterpart of `prune_dangling_sdk_root_links`; both delegate to
    /// the same walk so that what `show` reports and what `repair` removes can
    /// never disagree.
    pub fn dangling_sdk_root_links(ctx: &Ctx) -> Vec<PathBuf> {
        let root = Self::sdk_root(ctx);
        let mut found = Vec::new();
        Self::walk_dangling(&root, &root, false, &mut found);
        found.sort();
        found
    }

    /// Remove SDK root links whose target no longer exists.
    ///
    /// Distinct from `unlink_from_sdk_root`, which needs to know the family and
    /// version: this walks the root itself, so it also catches links left by an
    /// osdk version that had no unlink step, and links whose package was removed
    /// by something other than `osdk uninstall`.
    ///
    /// Returns the paths that were pruned, relative to the SDK root.
    pub fn prune_dangling_sdk_root_links(ctx: &Ctx) -> Vec<PathBuf> {
        let root = Self::sdk_root(ctx);
        let mut pruned = Vec::new();
        Self::walk_dangling(&root, &root, true, &mut pruned);
        pruned.sort();
        pruned
    }

    /// Find, and optionally remove, dangling links under `dir`.
    ///
    /// One function for both so detection and repair cannot drift apart: a
    /// reporting pass that disagrees with the fixing pass is how "it says it is
    /// fine but it is not" bugs happen.
    fn walk_dangling(root: &Path, dir: &Path, remove: bool, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        for path in children {
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            let is_link = meta.file_type().is_symlink() || Self::is_reparse_point(&meta);
            if is_link {
                // `exists()` follows the link, so a false here is precisely the
                // dangling case: the entry is present but its target is not.
                if !path.exists() {
                    let recorded = if remove {
                        std::fs::remove_dir(&path)
                            .or_else(|_| std::fs::remove_file(&path))
                            .is_ok()
                    } else {
                        true
                    };
                    if recorded {
                        if let Ok(relative) = path.strip_prefix(root) {
                            found.push(relative.to_path_buf());
                        }
                    }
                }
                // Never descend through a link: the payload below it belongs to
                // the package, not to the root's scaffolding.
                continue;
            }
            if meta.is_dir() {
                Self::walk_dangling(root, &path, remove, found);
                // Prune scaffolding this emptied, but only when removing, and
                // never the root itself.
                if remove && path != root {
                    let empty = std::fs::read_dir(&path)
                        .map(|mut entries| entries.next().is_none())
                        .unwrap_or(false);
                    if empty {
                        let _ = std::fs::remove_dir(&path);
                    }
                }
            }
        }
    }

    /// The subdirectory the emulator uses to decide a directory is an SDK root.
    ///
    /// Measured from `emulator -verbose` (37.1.11): it checks `ANDROID_HOME`,
    /// then `ANDROID_SDK_ROOT`, then walks up from its own location, and rejects
    /// every candidate that has no `platform-tools` child -- ending in
    /// `FATAL | Broken AVD system path`. So a root holding only `emulator` and
    /// `system-images` is still invalid, and installing platform-tools is what
    /// makes an AVD bootable.
    const SDK_ROOT_MARKER: &'static str = "platform-tools";

    /// Warn when the shared root is not yet a valid SDK root.
    ///
    /// Only the families that actually boot an AVD care, so this stays quiet for
    /// the rest. It is a warning rather than an error because installing the
    /// pieces in either order has to work.
    fn warn_if_sdk_root_incomplete(ctx: &Ctx, family: &str) {
        if !matches!(family, "emulator" | SYSTEM_IMAGES_FAMILY) {
            return;
        }
        let root = Self::sdk_root(ctx);
        if root.join(Self::SDK_ROOT_MARKER).exists() {
            return;
        }
        tracing::warn!(
            marker = Self::SDK_ROOT_MARKER,
            root = %root.display(),
            "the emulator will not accept this SDK root until platform-tools is \
             installed; run `osdk install android-platform-tools`"
        );
    }

    /// Where license acceptances are recorded for this installation.
    ///
    /// A single shared root (rather than per-package) so that an acceptance
    /// carries across packages the way it does for a conventional SDK install,
    /// and so Gradle can be pointed at one directory.
    pub fn sdk_root(ctx: &Ctx) -> PathBuf {
        ctx.dirs.installs.join("android-sdk")
    }

    /// The path a family must occupy inside a conventional SDK root.
    ///
    /// Google's tools do not look up packages by asking a manager; they walk a
    /// fixed directory layout. `system-images` keeps its four-segment path as
    /// nested directories, which is why this returns a relative path rather
    /// than a single name.
    pub fn sdk_root_relative_path(family: &str, version: &str) -> Option<PathBuf> {
        let path = match family {
            "platform-tools" => PathBuf::from("platform-tools"),
            "emulator" => PathBuf::from("emulator"),
            "cmdline-tools" => PathBuf::from("cmdline-tools").join(version),
            "build-tools" => PathBuf::from("build-tools").join(version),
            "ndk" => PathBuf::from("ndk").join(version),
            "cmake" => PathBuf::from("cmake").join(version),
            // These two are addressed as `platforms;android-35`, so the version
            // already carries the prefix. Adding another produced
            // `platforms/android-android-35`, a path no tool looks in.
            "platforms" => PathBuf::from("platforms").join(api_dir_name(version)),
            "sources" => PathBuf::from("sources").join(api_dir_name(version)),
            SYSTEM_IMAGES_FAMILY => {
                // `system-images;android-35;google_apis;x86_64` -> three dirs.
                let mut path = PathBuf::from(SYSTEM_IMAGES_FAMILY);
                for segment in version.split(';') {
                    if segment.is_empty() {
                        return None;
                    }
                    path = path.join(segment);
                }
                path
            }
            _ => return None,
        };
        Some(path)
    }

    /// Expose an installed package at its conventional location inside the
    /// shared SDK root.
    ///
    /// ## Why this exists
    ///
    /// osdk installs each family into its own versioned directory, but the
    /// emulator refuses to start unless `platform-tools/`, `emulator/` and
    /// `system-images/` are *siblings under one root* -- it walks that layout
    /// itself and reports `Broken AVD system path` otherwise. avdmanager
    /// likewise warns about packages in an "inconsistent location". Rather than
    /// abandoning per-family versioning or copying gigabytes twice, link the
    /// real directory into the layout those tools expect.
    ///
    /// A link failure is not fatal: only the emulator and avdmanager care about
    /// the unified view, so `adb`, the NDK and the build tools keep working.
    pub fn link_into_sdk_root(ctx: &Ctx, family: &str, version: &str) -> Result<()> {
        let Some(relative) = Self::sdk_root_relative_path(family, version) else {
            return Ok(());
        };
        let target = ctx
            .dirs
            .install_path(&format!("{ID_PREFIX}{family}"), version);
        if !target.is_dir() {
            return Ok(());
        }
        let link = Self::sdk_root(ctx).join(&relative);
        if let Some(parent) = link.parent() {
            crate::dirs::create_dir_all(parent)?;
        }
        // An existing entry that already resolves to the same place is left
        // alone; a stale one is replaced so a version switch is picked up.
        if let Ok(existing) = std::fs::canonicalize(&link) {
            if std::fs::canonicalize(&target)
                .map(|t| t == existing)
                .unwrap_or(false)
            {
                return Ok(());
            }
        }
        if let Ok(meta) = link.symlink_metadata() {
            let is_link = meta.file_type().is_symlink() || Self::is_reparse_point(&meta);
            let is_empty_dir = meta.is_dir()
                && std::fs::read_dir(&link)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false);
            if is_link || is_empty_dir {
                // Removing a junction unlinks it without touching the target, so
                // this never reaches the payload it points at.
                let _ = std::fs::remove_dir(&link).or_else(|_| std::fs::remove_file(&link));
            } else {
                // A real directory with contents belongs to something else --
                // most likely a package installed by Google's own sdkmanager.
                // Adopting that path would mean deleting data osdk never owned.
                tracing::warn!(
                    family,
                    version,
                    path = %link.display(),
                    "not linking into the SDK root: a real directory already \
                     occupies this path; remove it to let osdk manage it"
                );
                return Ok(());
            }
        }
        match symlink_dir(&target, &link) {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    family,
                    version,
                    "could not expose the package inside the SDK root: {error}"
                );
                Ok(())
            }
        }
    }
}

/// Create a directory link at `link` pointing to `target`.
///
/// On Windows this is a junction rather than a symlink: creating a symlink needs
/// either Developer Mode or elevation, while a junction needs neither, and the
/// Android tools only ever traverse it.
#[cfg(windows)]
fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    junction_create(target, link)
}

/// Create an NTFS directory junction at `link` resolving to `target`.
///
/// Written against the Win32 reparse-point API rather than
/// `std::os::windows::fs::symlink_dir` because a symlink needs
/// `SeCreateSymbolicLinkPrivilege` (elevation or Developer Mode) while a
/// junction needs no privilege at all, and the Android tools only traverse it.
#[cfg(windows)]
fn junction_create(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    // A junction target must be a fully qualified path in the NT namespace
    // (`\??\C:\...`); a plain path or a `\\?\` prefix is not accepted here.
    let absolute = std::fs::canonicalize(target)?;
    let native = absolute.to_string_lossy().replace("\\\\?\\", "");
    let substitute: Vec<u16> = format!("\\??\\{native}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let print: Vec<u16> = native.encode_utf16().chain(std::iter::once(0)).collect();

    // The junction is a reparse point set on an *existing empty directory*.
    std::fs::create_dir(link)?;

    let substitute_bytes = substitute.len() * 2;
    let print_bytes = print.len() * 2;
    // Path fields, then the two NUL-terminated names back to back.
    let data_len = 8 + substitute_bytes + print_bytes;
    let mut buffer: Vec<u8> = Vec::with_capacity(8 + data_len);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&(data_len as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
                                                   // Lengths exclude the terminating NUL, offsets are byte offsets into the
                                                   // path buffer.
    buffer.extend_from_slice(&((substitute_bytes - 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes()); // PrintNameOffset
    buffer.extend_from_slice(&((print_bytes - 2) as u16).to_le_bytes());
    for unit in substitute.iter().chain(print.iter()) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }

    let wide: Vec<u16> = link
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the handle is
    // closed on every path below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir(link);
        return Err(error);
    }
    // SAFETY: `handle` is a valid directory handle opened for write above, and
    // `buffer` describes exactly `buffer.len()` initialised bytes.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    let result = if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    // SAFETY: closing a handle we opened and no longer use.
    unsafe {
        CloseHandle(handle);
    }
    if result.is_err() {
        let _ = std::fs::remove_dir(link);
    }
    result
}

#[cfg(not(windows))]
fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[async_trait]
impl Backend for AndroidBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn take_side_installed(&self) -> Vec<ToolVersion> {
        self.side_installed
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
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

    #[cfg(feature = "install")]
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

    #[cfg(feature = "install")]
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

                // License gate, before any bytes are fetched. Dependencies are
                // included because they may carry a *different* agreement --
                // system images use vendor-specific licenses -- and consenting
                // to one must not silently consent to another.
                let deps = manifest.resolve_dependencies(package);
                let mut gated: Vec<&RemotePackage> = vec![package];
                gated.extend(deps.iter().map(|(dep, _)| *dep));
                let pending = license::pending(&manifest, &gated, &acceptance, &sdk_root);
                if !pending.is_empty() {
                    return Err(license::blocked_error(&pending));
                }
                license::record_accepted(&manifest, &gated, &acceptance, &sdk_root)?;
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
            Self::write_package_index(ctx, self.family, &tv.version, package);
            Self::link_into_sdk_root(ctx, self.family, &tv.version)?;
            return Ok(());
        }

        let package = self.package(&manifest, &tv.version)?;

        // Satisfy declared dependencies first. Without this a system image
        // installs but cannot boot, because the emulator it names is absent or
        // too old; Google's own sdkmanager resolves these, so a manager that
        // does not leaves the user to discover the gap at runtime.
        self.install_dependencies(ictx, &manifest, package).await?;

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
        // Make the package visible to Google's own tools, then expose it where
        // they expect to find it. Both after extraction, so the directory being
        // described and linked exists.
        Self::write_package_index(ctx, self.family, &tv.version, Some(package));
        Self::link_into_sdk_root(ctx, self.family, &tv.version)?;
        Self::warn_if_sdk_root_incomplete(ctx, self.family);
        Ok(())
    }

    #[cfg(feature = "install")]
    /// Remove the install directory, and the SDK root link that pointed at it.
    ///
    /// The link has to go first: once the payload is deleted the junction becomes
    /// dangling, and a dangling entry still satisfies the existence checks the
    /// emulator, avdmanager and Gradle use, so the package looks installed and
    /// fails deeper in.
    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        Self::unlink_from_sdk_root(ctx, self.family, &tv.version);
        let dir = ctx.dirs.install_path(self.id(), &tv.version);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|error| Error::io(&dir, error))?;
        }
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        if self.family == SYSTEM_IMAGES_FAMILY {
            // A system image is data, not tools: it ships no executables, so
            // exposing its directory would only produce bogus shims.
            return Ok(Vec::new());
        }
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        Ok(match self.family {
            // The NDK exposes its drivers through a toolchain directory rather
            // than a top-level `bin`.
            "ndk" => vec![
                root.join("toolchains")
                    .join("llvm")
                    .join("prebuilt")
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

/// The directory name for a `platforms` / `sources` revision.
///
/// Both are addressed as `platforms;android-35`, so the version osdk records
/// already reads `android-35`. Prefixing unconditionally produced
/// `platforms/android-android-35`, a path neither Gradle nor Google's tools ever
/// look in -- so accept a version that already carries the prefix, and still add
/// one for a bare API level in case a caller passes `35`.
fn api_dir_name(version: &str) -> String {
    if version.starts_with("android-") {
        version.to_string()
    } else {
        format!("android-{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_whose_target_is_gone_is_reported_and_then_pruned() {
        // The bug this locks in, measured before the fix: `osdk uninstall` left a
        // junction behind, so `Test-Path sdk\platform-tools` was still True while
        // `sdk\platform-tools\adb.exe` was False. Everything that probes the
        // layout by existence then believes the package is installed.
        //
        // Runs on every platform: the link is a junction on Windows and a symlink
        // elsewhere, but the states being asserted -- present yet unresolvable --
        // are the same, and both were confirmed on real Linux before this test was
        // widened. The removal path differs though: `rmdir` on a unix symlink
        // fails with ENOTDIR, so the `remove_dir` -> `remove_file` fallback in the
        // walk is load-bearing there, and this test is what guards it.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("android-sdk");
        std::fs::create_dir_all(&root).unwrap();

        // A live link, and a nested one, so pruning cannot be confused with
        // "delete every link".
        let live = temp.path().join("payload-live");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("adb"), b"x").unwrap();
        symlink_dir(&live, &root.join("platform-tools")).unwrap();

        let doomed = temp.path().join("payload-doomed");
        std::fs::create_dir_all(&doomed).unwrap();
        let nested = root
            .join("system-images")
            .join("android-35")
            .join("google_apis");
        std::fs::create_dir_all(&nested).unwrap();
        symlink_dir(&doomed, &nested.join("x86_64")).unwrap();

        // A real directory that osdk never created: it must survive both passes.
        let foreign = root.join("licenses");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("android-sdk-license"), b"hash").unwrap();

        // Nothing is dangling yet.
        let mut found = Vec::new();
        AndroidBackend::walk_dangling(&root, &root, false, &mut found);
        assert!(found.is_empty(), "unexpected: {found:?}");

        // Now the uninstall: the payload goes, the link stays.
        std::fs::remove_dir_all(&doomed).unwrap();
        let link = nested.join("x86_64");
        assert!(
            link.symlink_metadata().is_ok(),
            "the link should still be present -- that is the bug"
        );
        assert!(!link.exists(), "but its target should be gone");

        // Reporting must find exactly that one, and must not remove it.
        let mut found = Vec::new();
        AndroidBackend::walk_dangling(&root, &root, false, &mut found);
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert!(link.symlink_metadata().is_ok(), "show must not mutate");

        // Pruning removes it, and the scaffolding it emptied, but leaves the live
        // link and the foreign directory alone.
        let mut pruned = Vec::new();
        AndroidBackend::walk_dangling(&root, &root, true, &mut pruned);
        assert_eq!(pruned.len(), 1, "pruned: {pruned:?}");
        assert!(link.symlink_metadata().is_err(), "the link should be gone");
        assert!(
            !root.join("system-images").exists(),
            "emptied scaffolding should be pruned too"
        );
        assert!(
            root.join("platform-tools").join("adb").is_file(),
            "the live link must survive"
        );
        assert!(
            foreign.join("android-sdk-license").is_file(),
            "a real directory osdk did not create must survive"
        );
        assert!(root.is_dir(), "the root itself must survive");
    }

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
    fn ndk_bin_path_uses_native_separators_only() {
        // A mixed-separator path is rejected by Windows (os error 123), so the
        // toolchain directory must be joined one component at a time.
        let root = PathBuf::from("root");
        let joined = root
            .join("toolchains")
            .join("llvm")
            .join("prebuilt")
            .join(ndk_prebuilt_dir(crate::platform::Os::Windows))
            .join("bin");
        let rendered = joined.to_string_lossy().to_string();
        assert!(
            !rendered.contains('/'),
            "path must not embed a foreign separator: {rendered}"
        );
        assert_eq!(joined.components().count(), 6);
    }

    #[test]
    fn only_avd_families_care_about_the_sdk_root_marker() {
        // Measured from `emulator -verbose`: platform-tools is the only child it
        // checks when deciding a directory is an SDK root, so that name is the
        // contract and a typo here would silently disable the warning.
        assert_eq!(AndroidBackend::SDK_ROOT_MARKER, "platform-tools");
        // The marker directory is also where the bridge puts platform-tools,
        // otherwise the check would test a path nothing populates.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("platform-tools", "37.0.1"),
            Some(PathBuf::from(AndroidBackend::SDK_ROOT_MARKER))
        );
    }

    #[test]
    fn sdk_root_paths_follow_googles_layout() {
        let p = |family: &str, version: &str| {
            AndroidBackend::sdk_root_relative_path(family, version).map(|p| p.components().count())
        };
        // Single-instance families sit directly under the root.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("platform-tools", "37.0.1"),
            Some(PathBuf::from("platform-tools"))
        );
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("emulator", "37.1.11"),
            Some(PathBuf::from("emulator"))
        );
        // Versioned families nest by version.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("build-tools", "37.0.0"),
            Some(PathBuf::from("build-tools").join("37.0.0"))
        );
        // platforms/sources are addressed as `platforms;android-35`, so the
        // version already reads `android-35`. This assertion used a bare `35`,
        // a shape the manifest never produces, and so passed while the real
        // install landed in `platforms/android-android-UpsideDownCake`.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("platforms", "android-35"),
            Some(PathBuf::from("platforms").join("android-35"))
        );
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("sources", "android-35"),
            Some(PathBuf::from("sources").join("android-35"))
        );
        // A codename revision is a directory name like any other; what must not
        // happen is a doubled prefix.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("platforms", "android-UpsideDownCake"),
            Some(PathBuf::from("platforms").join("android-UpsideDownCake"))
        );
        // A bare API level still gets one prefix, so a caller passing `35`
        // is not silently placed at `platforms/35`.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path("platforms", "35"),
            Some(PathBuf::from("platforms").join("android-35"))
        );
        // A system image expands its `;` path into nested directories, which is
        // exactly where the emulator looks for `system.img`.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path(
                SYSTEM_IMAGES_FAMILY,
                "android-35;google_apis;x86_64"
            ),
            Some(
                PathBuf::from("system-images")
                    .join("android-35")
                    .join("google_apis")
                    .join("x86_64")
            )
        );
        assert_eq!(
            p(SYSTEM_IMAGES_FAMILY, "android-35;google_apis;x86_64"),
            Some(4)
        );
        // Unknown families are not placed at a guessed location.
        assert_eq!(AndroidBackend::sdk_root_relative_path("nope", "1"), None);
        // A malformed system-image version must not yield a partial path.
        assert_eq!(
            AndroidBackend::sdk_root_relative_path(SYSTEM_IMAGES_FAMILY, "android-35;;x86_64"),
            None
        );
    }

    #[test]
    fn system_images_are_registered_but_carry_no_tools() {
        // The image is a disk image, not a toolchain, so it must not contribute
        // shims; `bin_paths` short-circuits on the family before touching Ctx.
        assert!(SUPPORTED_FAMILIES.contains(&SYSTEM_IMAGES_FAMILY));
        assert!(AndroidBackend::owns_id("android-system-images"));
        assert_eq!(
            AndroidBackend::new(SYSTEM_IMAGES_FAMILY).family(),
            SYSTEM_IMAGES_FAMILY
        );
        // Only this family needs the extra sub-site round trips.
        assert!(AndroidBackend::new(SYSTEM_IMAGES_FAMILY).needs_system_images());
        assert!(!AndroidBackend::new("platform-tools").needs_system_images());
        assert!(!AndroidBackend::new("emulator").needs_system_images());
    }

    fn is_link(path: &std::path::Path) -> bool {
        let meta = std::fs::symlink_metadata(path).unwrap();
        AndroidBackend::is_reparse_point(&meta) || meta.file_type().is_symlink()
    }

    #[test]
    fn a_foreign_directory_is_never_replaced_by_a_link() {
        // The real hazard: a user who already installed a system image with
        // Google's sdkmanager has a genuine multi-GB directory exactly where the
        // bridge wants its link. Adopting that path would mean deleting data
        // osdk never owned, so linking must fail rather than clobber it.
        //
        // Holds on both platforms for the same reason: junction creation needs an
        // empty directory it can open, and `symlink(2)` refuses an existing path
        // with EEXIST (confirmed on Linux: "failed to create symbolic link: File
        // exists", with the directory left as a real directory).
        let temp = tempfile::tempdir().unwrap();
        let foreign = temp.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("system.img"), b"precious").unwrap();

        let payload = temp.path().join("payload");
        std::fs::create_dir_all(&payload).unwrap();

        assert!(symlink_dir(&payload, &foreign).is_err());
        assert_eq!(
            std::fs::read(foreign.join("system.img")).unwrap(),
            b"precious".to_vec()
        );
        assert!(!is_link(&foreign));

        // A path with nothing at it links cleanly, which is the normal case.
        let fresh = temp.path().join("fresh");
        symlink_dir(&payload, &fresh).expect("an unoccupied path links cleanly");
        assert!(is_link(&fresh));

        // And a link we own is detected as one, so it can be replaced on a
        // version switch. On Windows that needs the reparse-point check, because
        // `is_symlink()` alone does not report junctions.
        let meta = std::fs::symlink_metadata(&fresh).unwrap();
        #[cfg(windows)]
        assert!(AndroidBackend::is_reparse_point(&meta));
        #[cfg(not(windows))]
        assert!(meta.file_type().is_symlink());
    }

    #[test]
    fn directory_link_resolves_to_its_target_without_copying() {
        // Guards the platform link primitive on both sides: the hand-written
        // reparse-point FFI on Windows, `symlink(2)` elsewhere. A link that does
        // not resolve would silently reintroduce the `Broken AVD system path`
        // failure this bridge exists to fix.
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("marker.txt"), b"payload").unwrap();

        let link = temp.path().join("linked");
        symlink_dir(&target, &link).expect("a directory link is created without elevation");

        // Readable through the link...
        assert_eq!(
            std::fs::read(link.join("marker.txt")).unwrap(),
            b"payload".to_vec()
        );
        // ...and it is a link, not a second copy.
        assert!(is_link(&link));
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );

        // Removing the link must leave the target intact. The call that works
        // differs by platform -- `remove_dir` for a junction, `remove_file` for a
        // symlink -- which is why the production code tries both.
        std::fs::remove_dir(&link)
            .or_else(|_| std::fs::remove_file(&link))
            .expect("the link is removable");
        assert!(target.join("marker.txt").is_file());
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
