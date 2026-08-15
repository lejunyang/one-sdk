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
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

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
    fn file_token(ctx: &Ctx) -> String {
        let arch = ctx.platform.arch.node_token();
        match ctx.platform.os {
            Os::Linux => format!("linux-{arch}"),
            Os::Macos => format!("osx-{arch}-tar"),
            Os::Windows => format!("win-{arch}-zip"),
        }
    }

    /// The archive filename + kind for a version on the current platform.
    fn archive_for(ctx: &Ctx, version: &str) -> (String, ArchiveKind) {
        let os = ctx.platform.os.node_token();
        let arch = ctx.platform.arch.node_token();
        match ctx.platform.os {
            Os::Windows => (format!("node-v{version}-{os}-{arch}.zip"), ArchiveKind::Zip),
            _ => (
                format!("node-v{version}-{os}-{arch}.tar.gz"),
                ArchiveKind::TarGz,
            ),
        }
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
        let token = Self::file_token(ctx);

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
            };
            pipeline::run(&plan, &pctx).await?;
            return Ok(());
        }
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let version = &tv.version;
        let (file_name, kind) = Self::archive_for(ctx, version);

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
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
        };
        pipeline::run(&plan, &pctx).await?;
        Ok(())
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
