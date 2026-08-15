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
        let source = crate::source::select::active_source(ctx, self).await?;
        let index_url = source
            .index_url
            .clone()
            .unwrap_or_else(|| http::join_url(&source.download_url, "index.json"));
        let releases: Vec<NodeRelease> = http::get_json(&ctx.client, &index_url).await?;

        // Only offer releases that ship an asset for the current platform.
        let token = Self::file_token(ctx);

        // index.json is newest-first; VersionInfo list should be oldest-first.
        let mut out: Vec<VersionInfo> = releases
            .into_iter()
            .rev()
            .filter(|r| r.files.is_empty() || r.files.iter().any(|f| f == &token))
            .map(|r| {
                let version = r.version.trim_start_matches('v').to_string();
                let lts = match r.lts {
                    LtsField::Named(s) => Some(s.to_lowercase()),
                    LtsField::No => None,
                };
                VersionInfo {
                    version,
                    stable: true,
                    lts,
                }
            })
            .collect();
        out.retain(|v| !v.version.is_empty());
        Ok(out)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let source = crate::source::select::active_source(ctx, self).await?;
        let version = &tv.version;
        let (file_name, kind) = Self::archive_for(ctx, version);

        // Download URL: <base>/v<version>/<file_name>
        let base = http::join_url(&source.download_url, &format!("v{version}"));
        let url = http::join_url(&base, &file_name);

        // Fetch SHASUMS256.txt for verification.
        let shasums_url = http::join_url(&base, "SHASUMS256.txt");
        let checksum = match http::get_text(&ctx.client, &shasums_url).await {
            Ok(body) => pipeline::verify::find_shasum(&body, &file_name).map(|h| Checksum {
                algo: HashAlgo::Sha256,
                hex: h.to_string(),
            }),
            Err(_) => None,
        };

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: version.clone(),
            url,
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
