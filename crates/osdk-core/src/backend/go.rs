//! Go backend: downloads official archives from go.dev/dl (or a mirror),
//! verified against the per-file sha256 in the JSON index.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct GoBackend;

#[derive(Debug, Deserialize)]
struct GoRelease {
    version: String, // e.g. "go1.22.5"
    #[serde(default)]
    stable: bool,
    #[serde(default)]
    files: Vec<GoFile>,
}

#[derive(Debug, Deserialize, Clone)]
struct GoFile {
    filename: String,
    os: String,
    arch: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    kind: String, // "archive" | "installer" | "source"
}

impl GoBackend {
    fn matches_platform(f: &GoFile, ctx: &Ctx) -> bool {
        f.kind == "archive"
            && f.os == ctx.platform.os.go_token()
            && f.arch == ctx.platform.arch.go_token()
    }
}

#[async_trait]
impl Backend for GoBackend {
    fn id(&self) -> &str {
        "go"
    }

    fn aliases(&self) -> &[&str] {
        &["golang"]
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("official", "https://go.dev/dl/")
                .with_index("https://go.dev/dl/?mode=json&include=all"),
            // Aliyun mirrors the archives; it has no ?mode=json index, so we
            // reuse the official index for discovery and only swap the download
            // host. (index_url points at official.)
            Source::mirror("aliyun", "https://mirrors.aliyun.com/golang/", 10)
                .with_index("https://go.dev/dl/?mode=json&include=all"),
            Source::mirror("google-cn", "https://golang.google.cn/dl/", 20)
                .with_index("https://golang.google.cn/dl/?mode=json&include=all"),
            Source::mirror("ustc", "https://mirrors.ustc.edu.cn/golang/", 30)
                .with_index("https://go.dev/dl/?mode=json&include=all"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            let index_url = match &source.index_url {
                Some(u) => u.clone(),
                None => continue,
            };
            let releases: Vec<GoRelease> = match http::get_cached_json(ctx, &index_url).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(source = %source.id, "{}", crate::i18n::trf("log.go_index_fetch_failed", &[("err", &e.to_string())]));
                    last_err = Some(e);
                    continue;
                }
            };
            // go.dev lists newest-first; produce oldest-first.
            let mut out: Vec<VersionInfo> = releases
                .into_iter()
                .rev()
                .filter(|r| r.files.iter().any(|f| Self::matches_platform(f, ctx)))
                .map(|r| VersionInfo {
                    version: normalize_go_version(&r.version),
                    stable: r.stable,
                    lts: None,
                })
                .collect();
            out.retain(|v| !v.version.is_empty());
            return Ok(out);
        }
        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id().to_string(),
            tried: sources.len(),
        }))
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
        let go_ver = format!("go{version}");

        // Discover the exact archive filename + sha256 from an index.
        let (file, _idx_source) = self.find_file(ctx, &sources, &go_ver).await?;

        let urls: Vec<String> = sources
            .iter()
            .map(|s| http::join_url(&s.download_url, &file.filename))
            .collect();

        let kind = ArchiveKind::from_name(&file.filename)?;
        let checksum = if file.sha256.is_empty() {
            None
        } else {
            Some(Checksum {
                algo: HashAlgo::Sha256,
                hex: file.sha256.clone(),
            })
        };

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: version.clone(),
            urls,
            file_name: file.filename.clone(),
            kind,
            checksum,
            strip_root: true, // archives wrap everything in a `go/` dir
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
        // Archive contains `go/bin`, and we strip the `go/` root, so bin is at
        // <install>/bin.
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
    }

    fn exec_env(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        env.insert("GOROOT".to_string(), root.display().to_string());
        Ok(env)
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec!["go".into(), "gofmt".into()])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &["go.mod", ".go-version"]
    }
}

impl GoBackend {
    /// Find the platform archive file for `go_ver` (e.g. "go1.22.5") by trying
    /// each source's index in order.
    async fn find_file(
        &self,
        ctx: &Ctx,
        sources: &[Source],
        go_ver: &str,
    ) -> Result<(GoFile, String)> {
        let mut last_err: Option<Error> = None;
        for source in sources {
            let index_url = match &source.index_url {
                Some(u) => u.clone(),
                None => continue,
            };
            let releases: Vec<GoRelease> = match http::get_cached_json(ctx, &index_url).await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            if let Some(rel) = releases.iter().find(|r| r.version == go_ver) {
                if let Some(f) = rel.files.iter().find(|f| Self::matches_platform(f, ctx)) {
                    return Ok((f.clone(), source.id.clone()));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::VersionResolve {
            tool: self.id().to_string(),
            spec: go_ver.to_string(),
            hint: Some("no archive for this platform in any source index".into()),
        }))
    }
}

/// Strip the leading `go` from a go.dev version string: "go1.22.5" -> "1.22.5".
fn normalize_go_version(v: &str) -> String {
    v.strip_prefix("go").unwrap_or(v).to_string()
}

/// Go binaries live at archive root on Windows too (go/bin), so no special case
/// beyond the shared `bin` join is needed.
#[allow(dead_code)]
fn _windows_note(_os: Os) {}
