//! Python backend: installs prebuilt CPython from astral-sh/python-build-
//! standalone (the same source uv/mise use). Discovery uses a generated
//! version-to-release-tag index, while immutable per-release `SHA256SUMS`
//! documents select and verify platform assets. No GitHub API is required.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PythonBackend;

/// A single asset parsed from `SHA256SUMS`: filename + sha256.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Asset {
    name: String,
    sha256: String,
}

/// The cached catalog for one release tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Catalog {
    tag: String,
    assets: Vec<Asset>,
}

impl PythonBackend {
    /// Match an `install_only` asset for a given python version + host triple.
    /// Asset names look like:
    ///   cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz
    fn asset_matches(name: &str, py_version: &str, triple: &str) -> bool {
        name.starts_with(&format!("cpython-{py_version}+"))
            && name.contains(&format!("-{triple}-install_only"))
            && name.contains("install_only")
            // exclude the free-threaded ("freethreaded") variants by default
            && !name.contains("freethreaded")
            && (name.ends_with(".tar.gz") || name.ends_with(".tar.zst"))
    }
}

#[async_trait]
impl Backend for PythonBackend {
    fn id(&self) -> &str {
        "python"
    }

    fn aliases(&self) -> &[&str] {
        &["py", "cpython"]
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official(
                "astral",
                "https://releases.astral.sh/github/python-build-standalone/releases/download",
            )
            .with_index("https://releases.astral.sh"),
            Source::mirror(
                "ghproxy",
                "https://gh-proxy.com/https://github.com/astral-sh/python-build-standalone/releases/download",
                10,
            )
            .with_index("https://gh-proxy.com"),
            Source::mirror(
                "github",
                "https://github.com/astral-sh/python-build-standalone/releases/download",
                20,
            )
            .with_index("https://github.com"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        let tag = super::python_releases::RELEASES.last()?.1;
        Some(http::join_url(
            &http::join_url(&source.download_url, tag),
            "SHA256SUMS",
        ))
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let triple = ctx.platform.llvm_triple();
        use std::collections::BTreeSet;
        let mut versions: BTreeSet<String> = BTreeSet::new();

        // Merge the generated historical index without any remote API calls.
        for (version, _) in super::python_releases::RELEASES {
            if version_available_on_platform(version, &triple) {
                versions.insert((*version).to_string());
            }
        }

        let mut out: Vec<VersionInfo> = versions
            .into_iter()
            .map(|v| VersionInfo {
                version: v,
                stable: true,
                lts: None,
            })
            .collect();
        out.sort_by(|a, b| cmp_versions(&a.version, &b.version));
        Ok(out)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
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
            return Ok(());
        }
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let triple = ctx.platform.llvm_triple();
        // Resolve a catalog (latest, or an older historical tag) that has this
        // version for the host triple. An explicit `-o tag=YYYYMMDD` pins the
        // PBS release tag (deterministic, no GitHub API needed).
        let tag = match tv.options.get("tag") {
            Some(tag) => tag.as_str(),
            None => super::python_releases::tag_for(&tv.version).ok_or_else(|| {
                Error::VersionResolve {
                    tool: self.id().to_string(),
                    spec: tv.version.clone(),
                    hint: Some(
                        "version is not in the built-in PBS release index; use -o tag=YYYYMMDD"
                            .into(),
                    ),
                }
            })?,
        };
        let catalog = self.fetch_catalog_for_tag(ctx, tag).await?;

        // Find the asset matching this exact python version for the host triple.
        let asset = catalog
            .assets
            .iter()
            .filter(|a| Self::asset_matches(&a.name, &tv.version, &triple))
            .max_by_key(|a| a.name.contains("install_only_stripped"))
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().to_string(),
                spec: tv.version.clone(),
                hint: Some(format!("no install_only asset for {triple}")),
            })?;

        let urls = sources
            .iter()
            .map(|source| {
                let release = http::join_url(&source.download_url, &catalog.tag);
                http::join_url(&release, &asset.name)
            })
            .collect();

        let kind = ArchiveKind::from_name(&asset.name)?;
        // Checksum comes straight from SHA256SUMS — no extra request.
        let checksum = if asset.sha256.len() == 64 {
            Some(Checksum {
                algo: HashAlgo::Sha256,
                hex: asset.sha256.clone(),
            })
        } else {
            None
        };

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name: asset.name.clone(),
            kind,
            checksum,
            strip_root: true, // archives wrap in a `python/` dir
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
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        // PBS layout after stripping `python/`: bin/ on unix, root on windows.
        let dir = match ctx.platform.os {
            Os::Windows => root,
            _ => root.join("bin"),
        };
        Ok(vec![dir])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec![
                "python".into(),
                "python3".into(),
                "pip".into(),
                "pip3".into(),
            ])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".python-version"]
    }
}

impl PythonBackend {
    /// Fetch the catalog for a specific historical tag by reading its
    /// SHA256SUMS (each dated release has its own).
    async fn fetch_catalog_for_tag(&self, ctx: &Ctx, tag: &str) -> Result<Catalog> {
        let cache_file = ctx
            .dirs
            .remote_cache()
            .join(format!("python-{tag}-catalog.json"));
        if let Some(catalog) = read_catalog(&cache_file) {
            return Ok(catalog);
        }
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            let prefix = http::join_url(&source.download_url, tag);
            let url = http::join_url(&prefix, "SHA256SUMS");
            match http::get_cached_text(ctx, &url).await {
                Ok(body) => {
                    let assets = parse_sha256sums(&body);
                    if !assets.is_empty() {
                        let catalog = Catalog {
                            tag: tag.to_string(),
                            assets,
                        };
                        if let Some(parent) = cache_file.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if let Ok(bytes) = serde_json::to_vec_pretty(&catalog) {
                            let _ = std::fs::write(&cache_file, bytes);
                        }
                        return Ok(catalog);
                    }
                    last_err = Some(Error::other("empty SHA256SUMS"));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::other(format!("no SHA256SUMS for tag {tag}"))))
    }
}

fn version_available_on_platform(version: &str, triple: &str) -> bool {
    match triple {
        "x86_64-unknown-linux-gnu" | "x86_64-apple-darwin" | "aarch64-apple-darwin" => true,
        "aarch64-unknown-linux-gnu" => version != "3.8.12",
        "x86_64-pc-windows-msvc" => minimum_for_minor(
            version,
            &[
                ("3.8", "3.8.19"),
                ("3.9", "3.9.19"),
                ("3.10", "3.10.14"),
                ("3.11", "3.11.9"),
                ("3.12", "3.12.3"),
                ("3.13", "3.13.0"),
                ("3.14", "3.14.0"),
            ],
        ),
        "x86_64-unknown-linux-musl" => minimum_for_minor(
            version,
            &[
                ("3.9", "3.9.21"),
                ("3.10", "3.10.16"),
                ("3.11", "3.11.11"),
                ("3.12", "3.12.9"),
                ("3.13", "3.13.2"),
                ("3.14", "3.14.0"),
            ],
        ),
        "aarch64-unknown-linux-musl" => minimum_for_minor(
            version,
            &[
                ("3.9", "3.9.23"),
                ("3.10", "3.10.18"),
                ("3.11", "3.11.13"),
                ("3.12", "3.12.11"),
                ("3.13", "3.13.7"),
                ("3.14", "3.14.0"),
            ],
        ),
        "aarch64-pc-windows-msvc" => minimum_for_minor(
            version,
            &[
                ("3.11", "3.11.13"),
                ("3.12", "3.12.11"),
                ("3.13", "3.13.5"),
                ("3.14", "3.14.0"),
            ],
        ),
        _ => false,
    }
}

fn minimum_for_minor(version: &str, minimums: &[(&str, &str)]) -> bool {
    minimums.iter().any(|(minor, minimum)| {
        (version == *minor || version.starts_with(&format!("{minor}.")))
            && cmp_versions(version, minimum) != std::cmp::Ordering::Less
    })
}

/// Parse a `SHA256SUMS` body: each line is `<hex>  <filename>`.
fn parse_sha256sums(body: &str) -> Vec<Asset> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let hash = match it.next() {
            Some(h) => h,
            None => continue,
        };
        let name = match it.next() {
            Some(n) => n.trim_start_matches('*'),
            None => continue,
        };
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push(Asset {
                name: name.to_string(),
                sha256: hash.to_string(),
            });
        }
    }
    out
}

fn read_catalog(path: &std::path::Path) -> Option<Catalog> {
    let bytes = std::fs::read(path).ok()?;
    let cat: Catalog = serde_json::from_slice(&bytes).ok()?;
    if cat.assets.is_empty() {
        None
    } else {
        Some(cat)
    }
}

/// Compare two dotted numeric versions ascending.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let pb: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    pa.cmp(&pb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_matching() {
        let name = "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz";
        assert!(PythonBackend::asset_matches(
            name,
            "3.12.7",
            "x86_64-unknown-linux-gnu"
        ));
        assert!(!PythonBackend::asset_matches(
            name,
            "3.12.7",
            "aarch64-apple-darwin"
        ));
        let stripped =
            "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz";
        assert!(PythonBackend::asset_matches(
            stripped,
            "3.12.7",
            "x86_64-unknown-linux-gnu"
        ));
        // free-threaded variant excluded
        let ft =
            "cpython-3.13.1+20241016-x86_64-unknown-linux-gnu-freethreaded-install_only.tar.gz";
        assert!(!PythonBackend::asset_matches(
            ft,
            "3.13.1",
            "x86_64-unknown-linux-gnu"
        ));
    }

    #[test]
    fn parse_sums_extracts_assets() {
        let body = "\
391e2bbe4da892fd7dd9f773f42ad8eae82f33d3d4fc8f0025af80b4dfa134b3  cpython-3.10.21+20260814-x86_64-unknown-linux-gnu-install_only.tar.gz
3297691ae34f75fed81ac424e040145fccb0bafe8e581cd5cadbddfa1c0766c0  cpython-3.12.14+20260814-x86_64-unknown-linux-gnu-install_only.tar.gz
not-a-hash  garbage-line
";
        let assets = parse_sha256sums(body);
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].sha256.len(), 64);
        assert!(assets[1].name.contains("3.12.14"));
    }

    #[test]
    fn version_ordering() {
        assert_eq!(cmp_versions("3.9.1", "3.12.0"), std::cmp::Ordering::Less);
        assert_eq!(cmp_versions("3.12.7", "3.12.7"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn generated_release_index_covers_recent_python() {
        assert_eq!(
            super::super::python_releases::tag_for("3.12.14"),
            Some("20260814")
        );
        assert!(version_available_on_platform(
            "3.12.14",
            "aarch64-pc-windows-msvc"
        ));
        assert!(!version_available_on_platform(
            "3.10.21",
            "aarch64-pc-windows-msvc"
        ));
        assert!(!version_available_on_platform(
            "3.12.8",
            "x86_64-unknown-linux-musl"
        ));
        assert!(version_available_on_platform(
            "3.12.9",
            "x86_64-unknown-linux-musl"
        ));
    }
}
