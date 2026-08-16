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
use crate::version::{select_version, ToolRequest, ToolVersion, VersionInfo};

pub struct PythonBackend;

pub fn select_installed(spec: &str, installed: &[String]) -> Option<String> {
    super::python_catalog::select_installed(spec, installed)
}

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

    async fn resolve_version(&self, ctx: &Ctx, request: &ToolRequest) -> Result<ToolVersion> {
        if request
            .options
            .contains_key(pipeline::LOCKED_ARTIFACT_URL_OPTION)
        {
            if let crate::version::VersionSpec::Exact(identity) = &request.spec {
                let mut resolved = ToolVersion::new(self.id(), identity);
                resolved.options = request.options.clone();
                return Ok(resolved);
            }
        }
        let parsed = super::python_catalog::PythonRequest::parse(request)?;
        if parsed.implementation == "cpython"
            && parsed.variant == "default"
            && !parsed.explicit_prerelease
            && ctx.config.settings.python.catalog_url.is_none()
            && !matches!(
                ctx.config.settings.prerelease,
                crate::config::PrereleasePolicy::Allow
            )
        {
            let versions = self.list_remote_versions(ctx).await?;
            let chosen =
                select_version(&parsed.spec, &versions).ok_or_else(|| Error::VersionResolve {
                    tool: self.id().into(),
                    spec: parsed.spec.to_string(),
                    hint: Some("no matching stable CPython version found".into()),
                })?;
            if super::python_catalog::is_prerelease(&chosen.version)
                && matches!(
                    ctx.config.settings.prerelease,
                    crate::config::PrereleasePolicy::Never
                )
            {
                return Err(Error::VersionResolve {
                    tool: self.id().into(),
                    spec: parsed.spec.to_string(),
                    hint: Some("pre-release Python versions are disabled".into()),
                });
            }
            let mut resolved = ToolVersion::new(self.id(), &chosen.version);
            resolved.options = super::python_catalog::resolved_options(&parsed, &chosen.version);
            resolved.options.extend(request.options.clone());
            return Ok(resolved);
        }

        let catalog = super::python_catalog::load(ctx).await?;
        let (mut resolved, _) = super::python_catalog::resolve_catalog(&catalog, &parsed, ctx)?;
        resolved.options.extend(request.options.clone());
        Ok(resolved)
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
            if let Err(error) = ensure_python_aliases(ctx, tv) {
                let _ = std::fs::remove_dir_all(ctx.dirs.install_path(self.id(), &tv.version));
                return Err(error);
            }
            return Ok(());
        }
        let implementation = tv
            .options
            .get("implementation")
            .map(String::as_str)
            .unwrap_or("cpython");
        let variant = tv
            .options
            .get("variant")
            .map(String::as_str)
            .unwrap_or("default");
        let python_version = tv
            .options
            .get("python-version")
            .cloned()
            .unwrap_or_else(|| tv.version.clone());
        if implementation != "cpython"
            || variant != "default"
            || super::python_catalog::is_prerelease(&python_version)
            || tv.options.get("catalog").map(String::as_str) == Some("true")
        {
            let request = ToolRequest {
                backend: self.id().into(),
                spec: crate::version::VersionSpec::Exact(format!(
                    "{implementation}-{python_version}+{variant}"
                )),
                options: tv.options.clone(),
            };
            let parsed = super::python_catalog::PythonRequest::parse(&request)?;
            let catalog = super::python_catalog::load(ctx).await?;
            let (_, entry) = super::python_catalog::resolve_catalog(&catalog, &parsed, ctx)?;
            let entry = entry.ok_or_else(|| Error::other("python catalog entry missing"))?;
            let plan = super::python_catalog::install_plan(&tv.version, &entry)?;
            let pctx = PipelineCtx {
                client: &ctx.client,
                dirs: &ctx.dirs,
                cas: &ctx.cas,
                link_mode: ctx.config.settings.link_mode,
                show_progress: ctx.show_progress,
                offline: ctx.config.settings.offline,
                require_checksums: true,
            };
            pipeline::run(&plan, &pctx).await?;
            if let Err(error) = ensure_python_aliases(ctx, tv) {
                let _ = std::fs::remove_dir_all(ctx.dirs.install_path(self.id(), &tv.version));
                return Err(error);
            }
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
            subdir: None,
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
        if tv.version.starts_with("pyodide-") {
            return Ok(vec![root]);
        }
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
            if tv.version.starts_with("pypy-") {
                Ok(vec!["pypy".into(), "pypy3".into(), "python".into()])
            } else if tv.version.starts_with("graalpy-") {
                Ok(vec!["graalpy".into(), "python".into(), "python3".into()])
            } else if tv.version.starts_with("pyodide-") {
                Ok(vec!["python".into()])
            } else if tv.version.contains("+freethreaded") {
                Ok(vec!["pythont".into(), "python3t".into()])
            } else {
                Ok(vec![
                    "python".into(),
                    "python3".into(),
                    "pip".into(),
                    "pip3".into(),
                ])
            }
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

fn ensure_python_aliases(ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
    let paths = PythonBackend.bin_paths(ctx, tv)?;
    let Some(directory) = paths.first() else {
        return Ok(());
    };
    let candidates: &[&str] = if tv.version.starts_with("pypy-") {
        &["pypy3", "pypy"]
    } else if tv.version.starts_with("graalpy-") {
        &["graalpy"]
    } else if tv.version.starts_with("pyodide-") {
        &["python"]
    } else {
        return Ok(());
    };
    let Some(source) = candidates
        .iter()
        .map(|name| directory.join(format!("{name}{}", ctx.platform.os.exe_suffix())))
        .find(|path| path.is_file())
    else {
        return Err(Error::other(format!(
            "installed Python identity {} has no executable in {}",
            tv.version,
            directory.display()
        )));
    };
    for name in ["python", "python3"] {
        let destination = directory.join(format!("{name}{}", ctx.platform.os.exe_suffix()));
        if destination.exists() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = source
                .file_name()
                .ok_or_else(|| Error::other("python alias source has no filename"))?;
            symlink(target, &destination).map_err(|error| Error::io(&destination, error))?;
        }
        #[cfg(windows)]
        {
            std::fs::copy(&source, &destination).map_err(|error| Error::io(&destination, error))?;
        }
    }
    Ok(())
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
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, PrereleasePolicy, PythonSettings, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::{Arch, Libc, Platform};
    use crate::store::Cas;

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

    fn write_fixture_archive(path: &std::path::Path, subdir: Option<&str>, executable: &str) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let archive_path = match subdir {
            Some(subdir) => format!("root/{subdir}/{executable}"),
            None => format!("root/{executable}"),
        };
        let contents = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, archive_path, &contents[..])
            .unwrap();
        archive.finish().unwrap();
    }

    fn fixture_ctx(root: &std::path::Path, catalog_path: &std::path::Path, digest: &str) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            },
            config: Config {
                settings: Settings {
                    offline: true,
                    prerelease: PrereleasePolicy::Allow,
                    python: PythonSettings {
                        catalog_url: Some(catalog_path.display().to_string()),
                        catalog_sha256: Some(digest.into()),
                    },
                    ..Default::default()
                },
                sources: SourcesConfig::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[tokio::test]
    async fn catalog_implementations_and_variants_install_offline_and_coexist() {
        let temp = tempfile::tempdir().unwrap();
        let fixtures = [
            ("cpython", "3.14.7", "default", None, "bin/python3"),
            ("cpython", "3.14.7", "freethreaded", None, "bin/python3t"),
            ("pypy", "3.11.15", "default", None, "bin/pypy3"),
            ("graalpy", "3.12.0", "default", None, "bin/graalpy"),
            (
                "pyodide",
                "3.14.2",
                "default",
                Some("pyodide-root/dist"),
                "python",
            ),
        ];
        let mut entries = Vec::new();
        for (index, (implementation, version, variant, subdir, executable)) in
            fixtures.iter().enumerate()
        {
            let file_name = format!("{implementation}-{index}.tar.gz");
            let archive = temp.path().join(&file_name);
            write_fixture_archive(&archive, *subdir, executable);
            let sha256 = pipeline::verify::hash_file(&archive, HashAlgo::Sha256).unwrap();
            entries.push(serde_json::json!({
                "implementation": implementation,
                "version": version,
                "variant": variant,
                "os": if *implementation == "pyodide" { "emscripten" } else { "linux" },
                "arch": if *implementation == "pyodide" { "wasm32" } else { "x86_64" },
                "libc": if *implementation == "pyodide" { "musl" } else { "gnu" },
                "url": format!("file://{}", archive.display()),
                "sha256": sha256,
                "subdir": subdir,
            }));
        }
        let catalog = serde_json::json!({
            "schema": 1,
            "source": "offline fixture",
            "source_sha256": "fixture",
            "entries": entries,
        });
        let catalog_bytes = serde_json::to_vec_pretty(&catalog).unwrap();
        let catalog_path = temp.path().join("catalog.json");
        std::fs::write(&catalog_path, &catalog_bytes).unwrap();
        let digest = pipeline::verify::hash_bytes(&catalog_bytes, HashAlgo::Sha256);
        let ctx = fixture_ctx(temp.path(), &catalog_path, &digest);

        for (implementation, version, variant, _, _) in fixtures {
            let request_value = if implementation == "cpython" {
                if variant == "default" {
                    format!("python@{version}")
                } else {
                    format!("python@cpython-{version}+{variant}")
                }
            } else {
                format!("python@{implementation}-{version}")
            };
            let request = ToolRequest::parse(&request_value).unwrap();
            let resolved = PythonBackend.resolve_version(&ctx, &request).await.unwrap();
            let catalog = super::super::python_catalog::load(&ctx).await.unwrap();
            let parsed = super::super::python_catalog::PythonRequest::parse(&request).unwrap();
            let (_, entry) =
                super::super::python_catalog::resolve_catalog(&catalog, &parsed, &ctx).unwrap();
            let entry = entry.unwrap();
            let file_name = entry
                .url
                .split('/')
                .next_back()
                .unwrap()
                .replace("%2B", "+");
            let cached =
                pipeline::artifact_cache_path(&ctx.dirs, "python", &resolved.version, &file_name);
            std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
            std::fs::copy(entry.url.trim_start_matches("file://"), &cached).unwrap();
            PythonBackend
                .install(&InstallCtx { ctx: &ctx }, &resolved)
                .await
                .unwrap();
            assert!(pipeline::is_installed(
                &ctx.dirs,
                "python",
                &resolved.version
            ));
        }

        assert!(ctx
            .dirs
            .install_path("python", "3.14.7")
            .join("bin/python3")
            .is_file());
        assert!(ctx
            .dirs
            .install_path("python", "cpython-3.14.7+freethreaded")
            .join("bin/python3t")
            .is_file());
        assert!(ctx
            .dirs
            .install_path("python", "pypy-3.11.15")
            .join("bin/pypy3")
            .is_file());
        assert!(ctx
            .dirs
            .install_path("python", "graalpy-3.12.0")
            .join("bin/graalpy")
            .is_file());
        assert!(ctx
            .dirs
            .install_path("python", "pyodide-3.14.2")
            .join("python")
            .is_file());
    }
}
