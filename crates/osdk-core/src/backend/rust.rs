//! Rust backend: delegates to rustup (installing rustup into a self-contained
//! home if absent), driving it with the fastest mirror as RUSTUP_DIST_SERVER.
//! This is the hybrid strategy: reuse the official manager for the complex
//! channel/component/target matrix rather than reimplementing it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, HashAlgo};
use crate::process;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct RustBackend;

impl RustBackend {
    /// Env for driving rustup within osdk's self-contained homes + mirror.
    /// `source` is the chosen dist server (fastest under auto, or the pin).
    fn rustup_env(ctx: &Ctx, source: Option<&Source>) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "RUSTUP_HOME".to_string(),
            ctx.dirs.rustup_home().display().to_string(),
        );
        env.insert(
            "CARGO_HOME".to_string(),
            ctx.dirs.cargo_home().display().to_string(),
        );
        if let Some(src) = source {
            env.insert("RUSTUP_DIST_SERVER".to_string(), src.download_url.clone());
            if let Some(update_root) = &src.index_url {
                env.insert("RUSTUP_UPDATE_ROOT".to_string(), update_root.clone());
            }
        }
        env
    }

    fn rustup_bin(ctx: &Ctx) -> PathBuf {
        let exe = if ctx.platform.os == crate::platform::Os::Windows {
            "rustup.exe"
        } else {
            "rustup"
        };
        ctx.dirs.cargo_home().join("bin").join(exe)
    }

    /// Ensure rustup is installed into osdk's isolated cargo home.
    async fn ensure_rustup(ctx: &Ctx, sources: &[Source]) -> Result<PathBuf> {
        let local = Self::rustup_bin(ctx);
        if local.exists() {
            return Ok(local);
        }
        let file_name = format!("rustup-init{}", ctx.platform.os.exe_suffix());
        let triple = ctx.platform.llvm_triple();
        let cached = ctx
            .dirs
            .downloads()
            .join("rustup")
            .join(&triple)
            .join(&file_name);
        if ctx.config.settings.offline && !cached.exists() {
            return Err(Error::other(
                "offline rustup bootstrap cache miss (install rust once without --offline)",
            ));
        }
        let mut last_err = None;

        for source in sources {
            let update_root = source
                .index_url
                .clone()
                .unwrap_or_else(|| crate::http::join_url(&source.download_url, "rustup"));
            let url = crate::http::join_url(&update_root, &format!("dist/{triple}/{file_name}"));
            let checksum_url = format!("{url}.sha256");
            let checksum = match crate::http::get_cached_text(ctx, &checksum_url).await {
                Ok(body) => match pipeline::verify::parse_sha256_token(&body) {
                    Some(checksum) => checksum,
                    None => {
                        last_err = Some(Error::other(format!(
                            "invalid rustup-init checksum from {checksum_url}"
                        )));
                        continue;
                    }
                },
                Err(error) => {
                    last_err = Some(error);
                    continue;
                }
            };
            match pipeline::download::download(
                &ctx.client,
                &url,
                &cached,
                "rustup-init",
                ctx.show_progress,
            )
            .await
            {
                Ok(()) => {
                    if let Err(error) = pipeline::verify::verify_file(
                        &cached,
                        &checksum,
                        HashAlgo::Sha256,
                        &file_name,
                    ) {
                        let _ = std::fs::remove_file(&cached);
                        last_err = Some(error);
                        continue;
                    }
                }
                Err(error) => {
                    last_err = Some(error);
                    continue;
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o755);
                std::fs::set_permissions(&cached, permissions)
                    .map_err(|error| Error::io(&cached, error))?;
            }
            let env = Self::rustup_env(ctx, Some(source));
            process::run(
                &cached.display().to_string(),
                &[
                    "-y",
                    "--no-modify-path",
                    "--profile",
                    "minimal",
                    "--default-toolchain",
                    "none",
                ],
                &env,
                None,
            )?;
            if local.exists() {
                return Ok(local);
            }
            last_err = Some(Error::other(format!(
                "rustup-init completed but {} was not created",
                local.display()
            )));
        }

        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: "rust".to_string(),
            tried: sources.len(),
        }))
    }
}

#[async_trait]
impl Backend for RustBackend {
    fn id(&self) -> &str {
        "rust"
    }

    fn aliases(&self) -> &[&str] {
        &["rustup"]
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("official", "https://static.rust-lang.org")
                .with_index("https://static.rust-lang.org/rustup"),
            Source::mirror("rsproxy", "https://rsproxy.cn", 5)
                .with_index("https://rsproxy.cn/rustup"),
            Source::mirror("tuna", "https://mirrors.tuna.tsinghua.edu.cn/rustup", 10)
                .with_index("https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        // The stable channel manifest is a good representative object.
        Some(crate::http::join_url(
            &source.download_url,
            "dist/channel-rust-stable.toml",
        ))
    }

    async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        // rustup resolves channels/versions itself; we surface the common
        // channels plus let exact versions pass through resolve_version.
        Ok(vec![
            VersionInfo::stable("stable"),
            VersionInfo::stable("beta"),
            VersionInfo::stable("nightly"),
        ])
    }

    async fn resolve_version(
        &self,
        _ctx: &Ctx,
        req: &crate::version::ToolRequest,
    ) -> Result<ToolVersion> {
        use crate::version::VersionSpec;
        // Pass channels/versions straight through to rustup.
        let version = match &req.spec {
            VersionSpec::Latest => "stable".to_string(),
            VersionSpec::Exact(v) => v.clone(),
            VersionSpec::Prefix(p) => p.clone(),
            VersionSpec::Lts(_) => "stable".to_string(),
            VersionSpec::System => "stable".to_string(),
        };
        let mut tv = ToolVersion::new(self.id(), version);
        tv.options = req.options.clone();
        Ok(tv)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let rustup = Self::ensure_rustup(ctx, &sources).await?;
        let source = sources.first();
        let env = Self::rustup_env(ctx, source);
        if let Some(s) = source {
            tracing::info!(source = %s.id, dist = %s.download_url, "{}", crate::i18n::tr("log.rustup_dist_server"));
        }
        let toolchain_bin = Self::toolchain_dir(ctx, &tv.version).join("bin");
        let rustc = toolchain_bin.join(format!("rustc{}", ctx.platform.os.exe_suffix()));
        if ctx.config.settings.offline && !rustc.exists() {
            return Err(Error::other(format!(
                "offline rust toolchain cache miss for {}",
                tv.version
            )));
        }

        // Install the toolchain. Optional profile/components/targets via options.
        let profile = tv
            .options
            .get("profile")
            .map(|s| s.as_str())
            .unwrap_or("default");
        let mut args: Vec<String> = vec![
            "toolchain".into(),
            "install".into(),
            tv.version.clone(),
            "--profile".into(),
            profile.into(),
        ];
        if let Some(components) = tv.options.get("components") {
            for c in components.split(',').filter(|s| !s.is_empty()) {
                args.push("--component".into());
                args.push(c.to_string());
            }
        }
        if let Some(targets) = tv.options.get("targets") {
            for t in targets.split(',').filter(|s| !s.is_empty()) {
                args.push("--target".into());
                args.push(t.to_string());
            }
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        // rustup may fail at the final "link proxies into CARGO_HOME" step when
        // we point CARGO_HOME at osdk's dir (rustup expects to own it). That's
        // non-fatal for us: we generate our own shims to the toolchain bin dir.
        // So we tolerate a nonzero exit iff the toolchain dir materialized.
        if !rustc.exists() {
            let run_res = process::run(&rustup.display().to_string(), &arg_refs, &env, None);
            if let Err(e) = run_res {
                if !rustc.exists() {
                    return Err(e);
                }
                tracing::debug!("rustup returned an error but the toolchain installed; continuing");
            }
        }

        // Record the install so list_installed/bin_paths work: rustup manages
        // toolchains under RUSTUP_HOME/toolchains/<name>. We create a marker
        // dir under our installs tree pointing at that toolchain.
        let install_dir = ctx.dirs.install_path(self.id(), &tv.version);
        crate::dirs::create_dir_all(&install_dir)?;
        std::fs::write(install_dir.join(".osdk-complete"), b"")
            .map_err(|e| Error::io(install_dir.join(".osdk-complete"), e))?;
        Ok(())
    }

    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let toolchain_dir = Self::toolchain_dir(ctx, &tv.version);
        if toolchain_dir.exists() {
            let rustup = Self::rustup_bin(ctx);
            if !rustup.exists() {
                return Err(Error::other(format!(
                    "cannot uninstall rust toolchain {}: isolated rustup is missing",
                    tv.version
                )));
            }
            let env = Self::rustup_env(ctx, None);
            process::run(
                &rustup.display().to_string(),
                &["toolchain", "uninstall", &tv.version],
                &env,
                None,
            )?;
        }
        let marker_dir = ctx.dirs.install_path(self.id(), &tv.version);
        if marker_dir.exists() {
            std::fs::remove_dir_all(&marker_dir).map_err(|error| Error::io(&marker_dir, error))?;
        }
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // rustup toolchain bins live at RUSTUP_HOME/toolchains/<name>/bin.
        let toolchain_dir = Self::toolchain_dir(ctx, &tv.version);
        Ok(vec![
            toolchain_dir.join("bin"),
            ctx.dirs.cargo_home().join("bin"),
        ])
    }

    fn exec_env(&self, ctx: &Ctx, _tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        env.insert(
            "RUSTUP_HOME".to_string(),
            ctx.dirs.rustup_home().display().to_string(),
        );
        env.insert(
            "CARGO_HOME".to_string(),
            ctx.dirs.cargo_home().display().to_string(),
        );
        Ok(env)
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec![
            "rustc".into(),
            "cargo".into(),
            "rustup".into(),
            "clippy-driver".into(),
            "rustfmt".into(),
        ])
    }

    fn idiomatic_files(&self) -> &[&str] {
        &["rust-toolchain.toml", "rust-toolchain"]
    }
}

impl RustBackend {
    /// The rustup toolchain directory for a version/channel. rustup expands a
    /// bare channel like `stable` into `stable-<host-triple>`.
    fn toolchain_dir(ctx: &Ctx, version: &str) -> PathBuf {
        let toolchains = ctx.dirs.rustup_home().join("toolchains");
        let exact = toolchains.join(version);
        if exact.exists() {
            return exact;
        }
        // Try `<channel>-<host-triple>`.
        let triple = ctx.platform.llvm_triple();
        let with_triple = toolchains.join(format!("{version}-{triple}"));
        if with_triple.exists() {
            return with_triple;
        }
        // Best-effort: find a toolchain dir that starts with the channel name.
        if let Ok(rd) = std::fs::read_dir(&toolchains) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(version) {
                    return entry.path();
                }
            }
        }
        exact
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[tokio::test]
    async fn uninstall_delegates_to_isolated_rustup() {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let log = temp.path().join("rustup.log");
        let rustup = dirs.cargo_home().join("bin/rustup");
        std::fs::create_dir_all(rustup.parent().unwrap()).unwrap();
        std::fs::write(
            &rustup,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&rustup, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dirs.rustup_home().join("toolchains/stable/bin")).unwrap();
        let marker = dirs.install_path("rust", "stable");
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join(".osdk-complete"), b"").unwrap();

        let ctx = Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        RustBackend
            .uninstall(&ctx, &ToolVersion::new("rust", "stable"))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "toolchain uninstall stable"
        );
        assert!(!marker.exists());
    }
}
