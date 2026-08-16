//! Node.js backend: downloads official prebuilt archives (or from a mirror),
//! verified against `SHASUMS256.txt`. This is the reference archive-based
//! backend proving the whole M2 stack.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::Result;
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{select_version, ToolRequest, ToolVersion, VersionInfo, VersionSpec};

pub struct NodeBackend;

#[derive(Debug, Deserialize)]
struct NodeRelease {
    version: String, // e.g. "v20.11.1"
    #[serde(default)]
    files: Vec<String>,
    /// LTS is either `false` or a codename string.
    #[serde(default)]
    lts: LtsField,
}

#[derive(Debug, Default)]
enum LtsField {
    #[default]
    No,
    Named(String),
}

impl<'de> Deserialize<'de> for LtsField {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(LtsField::Named(s)),
            _ => Ok(LtsField::No),
        }
    }
}

impl NodeBackend {
    /// The node file token for the current platform, e.g. `linux-x64`,
    /// `osx-arm64-tar`, `win-x64-zip`.
    fn target_arch(ctx: &Ctx, options: &BTreeMap<String, String>) -> Result<Arch> {
        let Some(value) = options.get("arch") else {
            return Ok(ctx.platform.arch);
        };
        Arch::parse_node(value).ok_or_else(|| {
            crate::error::Error::config(format!(
                "unsupported Node architecture `{value}` (expected x64|arm64|x86|arm)"
            ))
        })
    }

    fn file_token(ctx: &Ctx, arch: Arch) -> String {
        let arch = arch.node_token();
        match ctx.platform.os {
            Os::Linux => format!("linux-{arch}"),
            Os::Macos => format!("osx-{arch}-tar"),
            Os::Windows => format!("win-{arch}-zip"),
        }
    }

    /// The archive filename + kind for a version on the current platform.
    fn archive_for(ctx: &Ctx, arch: Arch, version: &str) -> (String, ArchiveKind) {
        let os = ctx.platform.os.node_token();
        let arch = arch.node_token();
        match ctx.platform.os {
            Os::Windows => (format!("node-v{version}-{os}-{arch}.zip"), ArchiveKind::Zip),
            _ => (
                format!("node-v{version}-{os}-{arch}.tar.gz"),
                ArchiveKind::TarGz,
            ),
        }
    }

    fn corepack_enabled(ctx: &Ctx, tv: &ToolVersion) -> Result<bool> {
        match tv.options.get("corepack").map(String::as_str) {
            Some("true" | "1" | "yes" | "on") => Ok(true),
            Some("false" | "0" | "no" | "off") => Ok(false),
            Some(value) => Err(crate::error::Error::config(format!(
                "invalid Node corepack option `{value}` (expected true|false)"
            ))),
            None => Ok(ctx.config.settings.node.corepack),
        }
    }

    fn enable_corepack(ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        if !Self::corepack_enabled(ctx, tv)? {
            return Ok(());
        }
        let bin_dir = match ctx.platform.os {
            Os::Windows => ctx.dirs.install_path("node", &tv.version),
            _ => ctx.dirs.install_path("node", &tv.version).join("bin"),
        };
        let executable = match ctx.platform.os {
            Os::Windows => bin_dir.join("corepack.cmd"),
            _ => bin_dir.join("corepack"),
        };
        if !executable.is_file() {
            return Err(crate::error::Error::other(format!(
                "Node {} does not include Corepack at {}",
                tv.version,
                executable.display()
            )));
        }
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin_dir.clone()];
        paths.extend(std::env::split_paths(&inherited));
        let mut env = BTreeMap::new();
        env.insert(
            "PATH".into(),
            std::env::join_paths(paths)
                .map_err(|error| crate::error::Error::other(error.to_string()))?
                .to_string_lossy()
                .into_owned(),
        );
        let install_directory = bin_dir.display().to_string();
        match ctx.platform.os {
            Os::Windows => {
                let script = executable.display().to_string();
                crate::process::run(
                    "cmd",
                    &[
                        "/D",
                        "/S",
                        "/C",
                        &script,
                        "enable",
                        "--install-directory",
                        &install_directory,
                    ],
                    &env,
                    Some(&bin_dir),
                )
            }
            _ => crate::process::run(
                &executable.display().to_string(),
                &["enable", "--install-directory", &install_directory],
                &env,
                Some(&bin_dir),
            ),
        }
    }

    fn complete_install(ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        if let Err(error) = Self::enable_corepack(ctx, tv) {
            let install = ctx.dirs.install_path("node", &tv.version);
            let _ = std::fs::remove_dir_all(install);
            return Err(error);
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for NodeBackend {
    fn id(&self) -> &str {
        "node"
    }

    fn aliases(&self) -> &[&str] {
        &["nodejs"]
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("official", "https://nodejs.org/dist/")
                .with_index("https://nodejs.org/dist/index.json"),
            Source::mirror("npmmirror", "https://npmmirror.com/mirrors/node/", 10)
                .with_index("https://npmmirror.com/mirrors/node/index.json"),
            Source::mirror(
                "tuna",
                "https://mirrors.tuna.tsinghua.edu.cn/nodejs-release/",
                20,
            )
            .with_index("https://mirrors.tuna.tsinghua.edu.cn/nodejs-release/index.json"),
            Source::mirror("ustc", "https://mirrors.ustc.edu.cn/node/", 30)
                .with_index("https://mirrors.ustc.edu.cn/node/index.json"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        // index.json is a good representative object (~300KB).
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        // Only offer releases that ship an asset for the current platform.
        let token = Self::file_token(ctx, ctx.platform.arch);

        // Union versions across all reachable sources: mirrors can be stale and
        // lag the official index, so merging avoids a fast-but-stale mirror
        // hiding a version another source already has.
        use std::collections::BTreeMap;
        let mut merged: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut any_ok = false;
        let mut last_err: Option<crate::error::Error> = None;
        for source in &sources {
            let index_url = source
                .index_url
                .clone()
                .unwrap_or_else(|| http::join_url(&source.download_url, "index.json"));
            let releases: Vec<NodeRelease> = match http::get_cached_json(ctx, &index_url).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(source = %source.id, "{}", crate::i18n::trf("log.index_fetch_failover", &[("err", &e.to_string())]));
                    last_err = Some(e);
                    continue;
                }
            };
            any_ok = true;
            for r in releases {
                if !(r.files.is_empty() || r.files.iter().any(|f| f == &token)) {
                    continue;
                }
                let version = r.version.trim_start_matches('v').to_string();
                if version.is_empty() {
                    continue;
                }
                let lts = match r.lts {
                    LtsField::Named(s) => Some(s.to_lowercase()),
                    LtsField::No => None,
                };
                merged.entry(version).or_insert(lts);
            }
            // The fastest reachable source usually suffices; only consult more
            // sources if it produced nothing. Stop once we have a populated set.
            if !merged.is_empty() {
                // Peek: does a later source add anything? We keep it cheap by
                // continuing only when the primary is a known-laggy mirror is
                // hard to detect, so we merge just the primary + official.
                if source.kind == crate::source::SourceKind::Official {
                    break;
                }
                // also fold in the official source (if present) for freshness
                if let Some(official) = sources
                    .iter()
                    .find(|s| s.kind == crate::source::SourceKind::Official)
                {
                    if official.id != source.id {
                        if let Some(idx) = &official.index_url {
                            if let Ok(rel) =
                                http::get_cached_json::<Vec<NodeRelease>>(ctx, idx).await
                            {
                                for r in rel {
                                    if !(r.files.is_empty() || r.files.iter().any(|f| f == &token))
                                    {
                                        continue;
                                    }
                                    let v = r.version.trim_start_matches('v').to_string();
                                    if v.is_empty() {
                                        continue;
                                    }
                                    let lts = match r.lts {
                                        LtsField::Named(s) => Some(s.to_lowercase()),
                                        LtsField::No => None,
                                    };
                                    merged.entry(v).or_insert(lts);
                                }
                            }
                        }
                    }
                }
                break;
            }
        }

        if !any_ok {
            return Err(
                last_err.unwrap_or_else(|| crate::error::Error::NoUsableSource {
                    tool: self.id().to_string(),
                    tried: sources.len(),
                }),
            );
        }

        // Sort ascending (oldest-first) by numeric components.
        let mut out: Vec<VersionInfo> = merged
            .into_iter()
            .map(|(version, lts)| VersionInfo {
                version,
                stable: true,
                lts,
            })
            .collect();
        out.sort_by(|a, b| crate::backend::python::cmp_versions(&a.version, &b.version));
        Ok(out)
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let target_arch = Self::target_arch(ctx, &req.options)?;
        let corepack = match req.options.get("corepack") {
            Some(value) => match value.as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => {
                    return Err(crate::error::Error::config(format!(
                        "invalid Node corepack option `{value}` (expected true|false)"
                    )))
                }
            },
            None => ctx.config.settings.node.corepack,
        };
        if let VersionSpec::Exact(version) = &req.spec {
            let mut resolved = ToolVersion::new(self.id(), version);
            resolved.options = req.options.clone();
            resolved
                .options
                .insert("arch".into(), target_arch.node_token().into());
            resolved
                .options
                .insert("corepack".into(), corepack.to_string());
            return Ok(resolved);
        }
        let target_ctx = Ctx {
            dirs: ctx.dirs.clone(),
            platform: crate::platform::Platform {
                arch: target_arch,
                ..ctx.platform
            },
            config: ctx.config.clone(),
            client: ctx.client.clone(),
            cas: ctx.cas.clone(),
            show_progress: ctx.show_progress,
        };
        let versions = self.list_remote_versions(&target_ctx).await?;
        let chosen = select_version(&req.spec, &versions).ok_or_else(|| {
            crate::error::Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some(format!(
                    "no matching version ships a {} asset",
                    target_arch.node_token()
                )),
            }
        })?;
        let mut resolved = ToolVersion::new(self.id(), &chosen.version);
        resolved.options = req.options.clone();
        resolved
            .options
            .insert("arch".into(), target_arch.node_token().into());
        resolved
            .options
            .insert("corepack".into(), corepack.to_string());
        Ok(resolved)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let target_arch = Self::target_arch(ctx, &tv.options)?;
        if target_arch != ctx.platform.arch {
            return Err(crate::error::Error::config(format!(
                "cannot execute Node {} artifacts on host {}; cross-architecture download-only mode is not available",
                target_arch.node_token(),
                ctx.platform.arch.node_token()
            )));
        }
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
            Self::complete_install(ctx, tv)?;
            return Ok(());
        }
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let version = &tv.version;
        let (file_name, kind) = Self::archive_for(ctx, target_arch, version);

        // Build a download URL from every candidate source (best-first) so the
        // pipeline can fail over: <base>/v<version>/<file_name>.
        let urls: Vec<String> = sources
            .iter()
            .map(|s| {
                let base = http::join_url(&s.download_url, &format!("v{version}"));
                http::join_url(&base, &file_name)
            })
            .collect();

        // Fetch SHASUMS256.txt for verification from the best source (fall back
        // through the others if needed).
        let mut checksum = None;
        for s in &sources {
            let base = http::join_url(&s.download_url, &format!("v{version}"));
            let shasums_url = http::join_url(&base, "SHASUMS256.txt");
            if let Ok(body) = http::get_cached_text(ctx, &shasums_url).await {
                if let Some(h) = pipeline::verify::find_shasum(&body, &file_name) {
                    checksum = Some(Checksum {
                        algo: HashAlgo::Sha256,
                        hex: h.to_string(),
                    });
                    break;
                }
            }
        }

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: version.clone(),
            urls,
            file_name,
            kind,
            checksum,
            strip_root: true,
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
        Self::complete_install(ctx, tv)?;
        Ok(())
    }

    fn ensure_post_install(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        Self::enable_corepack(ctx, tv)
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        // Unix: bin/. Windows: binaries sit at the archive root.
        let dir = match ctx.platform.os {
            Os::Windows => root,
            _ => root.join("bin"),
        };
        Ok(vec![dir])
    }

    fn exec_env(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        // node ships node, npm, npx, and (recent versions) corepack.
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            // Fallback to the canonical set if the dir isn't populated yet.
            Ok(vec!["node".into(), "npm".into(), "npx".into()])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".nvmrc", ".node-version"]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::{Libc, Platform};
    use crate::store::Cas;

    fn test_ctx(root: &std::path::Path) -> Ctx {
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
                settings: Settings::default(),
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
    async fn resolution_records_effective_arch_and_corepack() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.node.corepack = true;
        let request = ToolRequest::parse("node@20.11.1").unwrap();
        let resolved = NodeBackend.resolve_version(&ctx, &request).await.unwrap();
        assert_eq!(resolved.options["arch"], "x64");
        assert_eq!(resolved.options["corepack"], "true");

        let mut cross = ToolRequest::parse("node@20.11.1").unwrap();
        cross.options.insert("arch".into(), "arm64".into());
        let resolved = NodeBackend.resolve_version(&ctx, &cross).await.unwrap();
        assert_eq!(resolved.options["arch"], "arm64");
        let error = NodeBackend
            .install(&InstallCtx { ctx: &ctx }, &resolved)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cross-architecture"));
    }

    #[cfg(unix)]
    #[test]
    fn corepack_uses_managed_binary_and_failure_removes_install() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let mut version = ToolVersion::new("node", "20.11.1");
        version.options.insert("corepack".into(), "true".into());
        let install = ctx.dirs.install_path("node", &version.version);
        let bin = install.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("corepack");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s\\n' \"$PATH\" \"$*\" > '{}'\n",
                temp.path().join("corepack.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        NodeBackend::complete_install(&ctx, &version).unwrap();
        let log = std::fs::read_to_string(temp.path().join("corepack.log")).unwrap();
        assert!(log.starts_with(&bin.display().to_string()));
        assert!(log.contains("enable --install-directory"));

        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        let error = NodeBackend::complete_install(&ctx, &version).unwrap_err();
        assert!(error.to_string().contains("failed"));
        assert!(!install.exists());
    }
}
