//! Zig backend: installs official releases listed in `ziglang.org/download/index.json`.
//!
//! Zig earns a built-in backend because it is the one toolchain that makes
//! cross compilation work without a separate sysroot per target: it bundles
//! musl, several glibc versions and mingw-w64 headers and libraries, so
//! `zig cc --target=aarch64-linux-musl` produces a working binary on a host that
//! has no cross toolchain installed at all.
//!
//! The index, not the release page, is the source of truth. Each platform entry
//! carries `tarball`, `shasum` and `size` together, which is exactly the artifact
//! identity a lockfile needs, and it avoids guessing filenames: the layout
//! changed from `zig-linux-x86_64-<version>` to `zig-x86_64-linux-<version>`
//! during 0.14, so a name assembled from tokens would silently break across
//! versions. Zig's GitHub releases carry only source and bootstrap archives, so
//! the generic `github:` backend cannot install it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

pub struct ZigBackend;

/// The `master` key is a rolling nightly rather than a release. It is exposed as
/// a prerelease so `latest` never selects it, but `osdk install zig@master`
/// still works under a permissive prerelease policy.
const MASTER_KEY: &str = "master";

#[derive(Debug, Deserialize)]
struct IndexEntry {
    /// Present for `master`, whose key is not the version number.
    #[serde(default)]
    version: Option<String>,
    #[serde(flatten)]
    platforms: BTreeMap<String, PlatformValue>,
}

/// A platform entry is an object, but sibling keys such as `date`, `docs` and
/// `notes` are plain strings, so the untagged form skips them without failing
/// the whole parse. `Other` exists only to absorb those siblings; its contents
/// are deliberately never read.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PlatformValue {
    Artifact(IndexArtifact),
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, Deserialize, Clone)]
struct IndexArtifact {
    tarball: String,
    shasum: String,
}

impl ZigBackend {
    /// The index's platform key for the running host, e.g. `x86_64-windows`.
    ///
    /// Zig names these with LLVM triple CPU tokens, which is what
    /// [`Arch::llvm_token`] already produces; `i686`/`armv7` are the two cases
    /// where zig differs, so they are mapped explicitly.
    fn platform_key(ctx: &Ctx) -> Option<String> {
        let arch = match ctx.platform.arch {
            Arch::X64 => "x86_64",
            Arch::Arm64 => "aarch64",
            Arch::X86 => "x86",
            // Zig publishes no 32-bit ARM host build.
            Arch::Arm => return None,
        };
        let os = match ctx.platform.os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        };
        Some(format!("{arch}-{os}"))
    }

    fn unsupported_platform(ctx: &Ctx) -> Error {
        Error::UnsupportedPlatform {
            os: format!("{:?}", ctx.platform.os),
            arch: format!("{:?}", ctx.platform.arch),
        }
    }

    /// Pick the artifact for this host out of one index entry.
    fn artifact(entry: &IndexEntry, key: &str) -> Option<IndexArtifact> {
        match entry.platforms.get(key) {
            Some(PlatformValue::Artifact(artifact)) => Some(artifact.clone()),
            _ => None,
        }
    }

    /// Parse the index into a version list, oldest first.
    ///
    /// Versions without an artifact for this host are dropped: offering one and
    /// then failing at install time would be worse than not listing it.
    fn versions_from_index(
        index: &BTreeMap<String, IndexEntry>,
        key: &str,
    ) -> Vec<(String, IndexArtifact)> {
        let mut versions: Vec<(String, IndexArtifact)> = index
            .iter()
            .filter_map(|(name, entry)| {
                let artifact = Self::artifact(entry, key)?;
                let version = if name == MASTER_KEY {
                    // `master` is keyed by name; its real version is inside.
                    entry.version.clone()?
                } else {
                    name.clone()
                };
                Some((version, artifact))
            })
            .collect();
        versions.sort_by(|(left, _), (right, _)| {
            match (semver::Version::parse(left), semver::Version::parse(right)) {
                (Ok(left), Ok(right)) => left.cmp(&right),
                // Keep a deterministic order even for a key semver cannot parse.
                _ => left.cmp(right),
            }
        });
        versions
    }

    fn file_name(artifact: &IndexArtifact) -> Result<String> {
        artifact
            .tarball
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                Error::other(format!(
                    "zig index tarball has no file name: {}",
                    artifact.tarball
                ))
            })
    }

    #[cfg(feature = "install")]
    async fn fetch_index(ctx: &Ctx) -> Result<BTreeMap<String, IndexEntry>> {
        let sources = crate::source::select::ranked_source_list(ctx, &ZigBackend).await?;
        let mut last_err = None;
        for source in &sources {
            let Some(index_url) = source.index_url.clone() else {
                continue;
            };
            match crate::http::get_cached_json::<BTreeMap<String, IndexEntry>>(ctx, &index_url)
                .await
            {
                Ok(index) => return Ok(index),
                Err(error) => {
                    tracing::warn!(source = %source.id, "{}", crate::i18n::trf("log.index_fetch_failover", &[("err", &error.to_string())]));
                    last_err = Some(error);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::other("no reachable source provides the zig index")))
    }
}

#[async_trait]
impl Backend for ZigBackend {
    fn id(&self) -> &str {
        "zig"
    }

    fn default_sources(&self) -> Vec<Source> {
        // The index lists absolute tarball URLs, so `download_url` is only the
        // base a mirror would rewrite; the index URL is what actually matters.
        vec![Source::official("ziglang", "https://ziglang.org/download/")
            .with_index("https://ziglang.org/download/index.json")]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let key = Self::platform_key(ctx).ok_or_else(|| Self::unsupported_platform(ctx))?;
        let index = Self::fetch_index(ctx).await?;
        Ok(Self::versions_from_index(&index, &key)
            .into_iter()
            .map(|(version, _)| VersionInfo {
                stable: semver::Version::parse(&version)
                    .map(|parsed| parsed.pre.is_empty())
                    .unwrap_or(false),
                version,
                lts: None,
            })
            .collect())
    }

    #[cfg(feature = "install")]
    async fn resolve_version(&self, ctx: &Ctx, request: &ToolRequest) -> Result<ToolVersion> {
        let versions = self.list_remote_versions(ctx).await?;
        let selected = crate::version::select_version_with_prerelease(
            &request.spec,
            &versions,
            ctx.config.settings.prerelease,
        )
        .ok_or_else(|| Error::VersionResolve {
            tool: self.id().to_string(),
            spec: request.spec.to_string(),
            hint: Some(
                "zig publishes `master` as a rolling nightly; \
                 `latest` only selects tagged releases"
                    .into(),
            ),
        })?;
        let mut resolved = ToolVersion::new(self.id(), &selected.version);
        resolved.options = request.options.clone();
        Ok(resolved)
    }

    #[cfg(feature = "install")]
    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let plan = if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            plan
        } else {
            let key = Self::platform_key(ctx).ok_or_else(|| Self::unsupported_platform(ctx))?;
            let index = Self::fetch_index(ctx).await?;
            let artifact = Self::versions_from_index(&index, &key)
                .into_iter()
                .find(|(version, _)| version == &tv.version)
                .map(|(_, artifact)| artifact)
                .ok_or_else(|| Error::VersionResolve {
                    tool: self.id().to_string(),
                    spec: tv.version.clone(),
                    hint: Some(format!("the zig index has no {key} build for that version")),
                })?;
            let file_name = Self::file_name(&artifact)?;
            InstallPlan {
                tool: self.id().to_string(),
                version: tv.version.clone(),
                urls: vec![artifact.tarball.clone()],
                kind: ArchiveKind::from_name(&file_name)?,
                file_name,
                checksum: Some(Checksum {
                    algo: HashAlgo::Sha256,
                    hex: artifact.shasum.to_ascii_lowercase(),
                }),
                strip_root: true,
                subdir: None,
            }
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
            require_checksums: ctx.config.settings.require_checksums,
        };
        pipeline::run(&plan, &pctx).await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // The archive puts `zig` at its root, next to `lib/` and `doc/`.
        Ok(vec![ctx.dirs.install_path(self.id(), &tv.version)])
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["zig".into()])
    }

    /// `zig` locates its own standard library and the bundled libc headers
    /// relative to the executable, so no variable is required to make it work.
    /// `ZIG_GLOBAL_CACHE_DIR` is exported only to keep build artifacts inside
    /// osdk's cache instead of the user's home directory.
    fn exec_env(&self, ctx: &Ctx, _tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        Ok(crate::cache::manager_exec_env(
            &ctx.dirs.cache,
            &[("ZIG_GLOBAL_CACHE_DIR", "zig")],
        ))
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".zig-version"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Libc, Platform};

    const INDEX: &str = r#"{
      "master": {
        "version": "0.17.0-dev.2018+ab30a0b9a",
        "date": "2026-09-01",
        "docs": "https://ziglang.org/documentation/master/",
        "src": { "tarball": "https://ziglang.org/builds/zig-0.17.0-dev.tar.xz", "shasum": "aa", "size": "1" },
        "x86_64-windows": {
          "tarball": "https://ziglang.org/builds/zig-x86_64-windows-0.17.0-dev.2018+ab30a0b9a.zip",
          "shasum": "1111111111111111111111111111111111111111111111111111111111111111",
          "size": "97217739"
        }
      },
      "0.16.0": {
        "date": "2026-08-01",
        "notes": "https://ziglang.org/download/0.16.0/release-notes.html",
        "x86_64-windows": {
          "tarball": "https://ziglang.org/download/0.16.0/zig-x86_64-windows-0.16.0.zip",
          "shasum": "68659EB5F1E4EB1437A722F1DD889C5A322C9954607F5EDCF337BC3684A75A7E",
          "size": "97217739"
        },
        "x86_64-linux": {
          "tarball": "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz",
          "shasum": "2222222222222222222222222222222222222222222222222222222222222222",
          "size": "50000000"
        }
      },
      "0.13.0": {
        "x86_64-windows": {
          "tarball": "https://ziglang.org/download/0.13.0/zig-windows-x86_64-0.13.0.zip",
          "shasum": "3333333333333333333333333333333333333333333333333333333333333333",
          "size": "79163968"
        }
      },
      "0.9.1": {
        "aarch64-macos": {
          "tarball": "https://ziglang.org/download/0.9.1/zig-macos-aarch64-0.9.1.tar.xz",
          "shasum": "4444444444444444444444444444444444444444444444444444444444444444",
          "size": "40000000"
        }
      }
    }"#;

    fn index() -> BTreeMap<String, IndexEntry> {
        serde_json::from_str(INDEX).expect("fixture index parses")
    }

    /// The index mixes artifact objects with plain string siblings (`date`,
    /// `docs`, `notes`) and a `src` archive that is not a host build. Parsing
    /// must tolerate all of them rather than failing the whole document.
    #[test]
    fn parses_index_entries_alongside_non_platform_keys() {
        let index = index();
        assert!(index.contains_key("master"));
        let versions = ZigBackend::versions_from_index(&index, "x86_64-windows");
        let names: Vec<&str> = versions.iter().map(|(name, _)| name.as_str()).collect();
        // Sorted oldest first, and `master` reports its inner version.
        assert_eq!(
            names,
            ["0.13.0", "0.16.0", "0.17.0-dev.2018+ab30a0b9a"],
            "0.9.1 has no windows build and must be dropped"
        );
    }

    /// A version with no artifact for this host must not be offered at all.
    #[test]
    fn drops_versions_without_a_build_for_this_host() {
        let versions = ZigBackend::versions_from_index(&index(), "aarch64-macos");
        let names: Vec<&str> = versions.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["0.9.1"]);
    }

    /// The filename layout changed during 0.14 (`zig-windows-x86_64-` became
    /// `zig-x86_64-windows-`), so the name must come from the index rather than
    /// from tokens. This is the regression that would break silently.
    #[test]
    fn file_name_comes_from_the_index_across_both_naming_layouts() {
        let index = index();
        let versions = ZigBackend::versions_from_index(&index, "x86_64-windows");
        let by_version = |want: &str| {
            versions
                .iter()
                .find(|(version, _)| version == want)
                .map(|(_, artifact)| ZigBackend::file_name(artifact).unwrap())
                .unwrap()
        };
        assert_eq!(by_version("0.16.0"), "zig-x86_64-windows-0.16.0.zip");
        assert_eq!(by_version("0.13.0"), "zig-windows-x86_64-0.13.0.zip");
        // And the archive kind follows the real extension.
        assert_eq!(
            ArchiveKind::from_name(&by_version("0.16.0")).unwrap(),
            ArchiveKind::Zip
        );
        let linux = ZigBackend::versions_from_index(&index, "x86_64-linux");
        let (_, artifact) = linux.first().unwrap();
        assert_eq!(
            ArchiveKind::from_name(&ZigBackend::file_name(artifact).unwrap()).unwrap(),
            ArchiveKind::TarXz
        );
    }

    /// `master` is a rolling nightly, so it must not win `latest`.
    #[test]
    fn master_is_a_prerelease_and_never_wins_latest() {
        let versions: Vec<VersionInfo> =
            ZigBackend::versions_from_index(&index(), "x86_64-windows")
                .into_iter()
                .map(|(version, _)| VersionInfo {
                    stable: semver::Version::parse(&version)
                        .map(|parsed| parsed.pre.is_empty())
                        .unwrap_or(false),
                    version,
                    lts: None,
                })
                .collect();
        let latest =
            crate::version::select_version(&crate::version::VersionSpec::Latest, &versions)
                .expect("a stable version is available");
        assert_eq!(latest.version, "0.16.0");
        let master = versions
            .iter()
            .find(|info| info.version.starts_with("0.17.0-dev"))
            .unwrap();
        assert!(!master.stable);
    }

    /// Checksums are uppercase in the index but the pipeline compares lowercase.
    #[test]
    fn shasum_is_normalized_to_lowercase() {
        let versions = ZigBackend::versions_from_index(&index(), "x86_64-windows");
        let (_, artifact) = versions
            .iter()
            .find(|(version, _)| version == "0.16.0")
            .unwrap();
        let checksum = Checksum {
            algo: HashAlgo::Sha256,
            hex: artifact.shasum.to_ascii_lowercase(),
        };
        assert_eq!(
            checksum.hex,
            "68659eb5f1e4eb1437a722f1dd889c5a322c9954607f5edcf337bc3684a75a7e"
        );
    }

    #[test]
    fn platform_keys_use_zig_spellings() {
        let key = |os, arch| {
            ZigBackend::platform_key(&Ctx {
                platform: Platform {
                    os,
                    arch,
                    libc: Libc::Glibc,
                },
                ..test_ctx()
            })
        };
        assert_eq!(
            key(Os::Windows, Arch::X64).as_deref(),
            Some("x86_64-windows")
        );
        assert_eq!(
            key(Os::Macos, Arch::Arm64).as_deref(),
            Some("aarch64-macos")
        );
        assert_eq!(key(Os::Linux, Arch::X64).as_deref(), Some("x86_64-linux"));
        // Zig spells 32-bit x86 `x86`, not `i686`.
        assert_eq!(key(Os::Linux, Arch::X86).as_deref(), Some("x86-linux"));
        // No 32-bit ARM host build exists.
        assert_eq!(key(Os::Linux, Arch::Arm), None);
    }

    fn test_ctx() -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-zig-test/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-zig-test/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-zig-test/config".into()),
            _ => None,
        })
        .unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            },
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        }
    }
}
