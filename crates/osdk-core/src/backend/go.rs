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
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    #[cfg(feature = "install")]
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

    #[cfg(feature = "install")]
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
        // `default_sources` above only governs where the Go *toolchain archive*
        // comes from. Module downloads are a separate channel the go command
        // drives itself through GOPROXY, so mirroring the archive did nothing
        // for `go build` / `go test`: those still went to proxy.golang.org and
        // failed wherever it is unreachable, which reads as osdk's mirror
        // selection "not working" even though it was never in that path.
        //
        // Publish the module proxy as its own ranked source set and hand the
        // best candidate to the go command. `module_proxy_sources` is the
        // configuration-level view (defaults minus disabled, plus custom, sorted
        // by priority) and involves no network: this runs in the shim on every
        // command invocation, where a probe round-trip would be charged to
        // interactive latency. Live probing stays in the install path.
        if let Some(proxy) = self.preferred_module_proxy(ctx) {
            env.insert("GOPROXY".to_string(), proxy);
        }
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

/// The tool id under which the Go *module proxy* sources are configured.
///
/// Deliberately distinct from the `go` backend id: `[sources.go]` selects where
/// the toolchain archive is downloaded from, while this one selects the GOPROXY
/// the go command uses for modules. They are different services with different
/// hosts, and a user disabling a mirror for one must not silently repoint the
/// other. `osdk config` / `[sources."go-modules"]` addresses this set.
pub const GO_MODULE_PROXY_TOOL: &str = "go-modules";

impl GoBackend {
    /// Module-proxy candidates, best-first, without touching the network.
    ///
    /// The upstream default comes first so nothing changes for users who can
    /// reach it; mirrors follow by priority and are what make `go build` work on
    /// a network where proxy.golang.org is blocked. `direct` is appended as the
    /// final fallback so a module absent from a mirror is still fetched from its
    /// origin rather than failing the build.
    fn module_proxy_sources(ctx: &Ctx) -> Vec<Source> {
        crate::source::select::effective_sources_for(
            ctx,
            GO_MODULE_PROXY_TOOL,
            vec![
                Source::official("proxy.golang.org", "https://proxy.golang.org"),
                // Same mirror set the `go:` package backend already ships, kept
                // in sync with it so a module resolves identically whether it is
                // fetched by `osdk use go:<tool>` or by a plain `go build`.
                Source::mirror("goproxy.cn", "https://goproxy.cn", 10),
                Source::mirror("aliyun", "https://mirrors.aliyun.com/goproxy", 20),
            ],
        )
    }

    /// The GOPROXY value to hand the go command, or `None` to leave it alone.
    ///
    /// Returns `None` when the user has already set GOPROXY, so an explicit
    /// choice -- including the policy values `off` and `direct`, which are not
    /// mirrors at all -- always wins over osdk's default. Also returns `None`
    /// when configuration disabled every candidate, because emitting an empty or
    /// `direct`-only value there would silently override that intent.
    fn preferred_module_proxy(&self, ctx: &Ctx) -> Option<String> {
        if std::env::var_os("GOPROXY").is_some() {
            return None;
        }
        let mut endpoints = Self::module_proxy_sources(ctx)
            .into_iter()
            .map(|source| source.download_url.trim_end_matches('/').to_string())
            .filter(|url| crate::backend::go_package::validate_go_proxy(url).is_ok())
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return None;
        }
        endpoints.push("direct".to_string());
        // Join with `|`, not `,`. The separator *is* the fallback policy: after a
        // comma the go command only moves on for 404/410 and treats every other
        // error -- crucially a connection timeout -- as terminal, so a
        // comma-joined list still dies on the first unreachable proxy and the
        // mirrors behind it are never tried. That is exactly the case mirrors
        // exist for. A pipe falls back after any error, including non-HTTP ones.
        //
        // The gatekeeper semantics a comma buys (a private proxy answering 403
        // stops the lookup instead of leaking the module path onward) do not
        // apply here: every entry in this list is a public mirror of the same
        // public module set, so there is no private path to leak. A user who
        // needs the gatekeeper behaviour sets GOPROXY themselves, which the
        // check above leaves untouched.
        Some(endpoints.join("|"))
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sources_exclude_unavailable_ustc_mirror() {
        let sources = GoBackend.default_sources();
        assert_eq!(
            sources
                .iter()
                .map(|source| source.id.as_str())
                .collect::<Vec<_>>(),
            ["official", "aliyun", "google-cn"]
        );
    }
}
