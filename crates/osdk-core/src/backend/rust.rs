//! Rust backend: delegates to rustup (installing rustup into a self-contained
//! home if absent), driving it with the fastest mirror as RUSTUP_DIST_SERVER.
//! This is the hybrid strategy: reuse the official manager for the complex
//! channel/component/target matrix rather than reimplementing it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::process;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct RustBackend;

impl RustBackend {
    /// Env for driving rustup within osdk's self-contained homes + mirror.
    fn rustup_env(ctx: &Ctx) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "RUSTUP_HOME".to_string(),
            ctx.dirs.rustup_home().display().to_string(),
        );
        env.insert(
            "CARGO_HOME".to_string(),
            ctx.dirs.cargo_home().display().to_string(),
        );
        // Fastest mirror wins for the dist server.
        if let Some(src) = Self::pick_source(ctx) {
            env.insert("RUSTUP_DIST_SERVER".to_string(), src.download_url.clone());
            if let Some(update_root) = &src.index_url {
                env.insert("RUSTUP_UPDATE_ROOT".to_string(), update_root.clone());
            }
        }
        env
    }

    /// Choose a source synchronously by priority (no async probe here to keep the
    /// delegate path simple; auto-probe still informs ordering via config).
    fn pick_source(ctx: &Ctx) -> Option<Source> {
        let mut sources = crate::source::select::effective_sources(ctx, &RustBackend);
        // honor an explicit pin
        if let Some(tc) = ctx.config.tool_sources("rust") {
            if let Some(pin) = &tc.pin {
                if let Some(s) = sources.iter().find(|s| &s.id == pin) {
                    return Some(s.clone());
                }
            }
        }
        if sources.is_empty() {
            None
        } else {
            Some(sources.remove(0))
        }
    }

    fn rustup_bin(ctx: &Ctx) -> PathBuf {
        let exe = if ctx.platform.os == crate::platform::Os::Windows {
            "rustup.exe"
        } else {
            "rustup"
        };
        ctx.dirs.cargo_home().join("bin").join(exe)
    }

    /// Ensure rustup is installed into osdk's cargo home. Uses an existing
    /// rustup on PATH if present (via `rustup-init` style is skipped for now;
    /// we require a rustup on PATH or previously bootstrapped).
    fn ensure_rustup(ctx: &Ctx) -> Result<PathBuf> {
        let local = Self::rustup_bin(ctx);
        if local.exists() {
            return Ok(local);
        }
        // Fall back to a system rustup if available.
        if let Ok(p) = which::which("rustup") {
            return Ok(p);
        }
        Err(Error::other(
            "rustup not found. Install rustup first (https://rsproxy.cn/rustup-init.sh), \
             or ensure `rustup` is on PATH; osdk will drive it.",
        ))
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
        let rustup = Self::ensure_rustup(ctx)?;
        let env = Self::rustup_env(ctx);

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
        process::run(&rustup.display().to_string(), &arg_refs, &env, None)?;

        // Record the install so list_installed/bin_paths work: rustup manages
        // toolchains under RUSTUP_HOME/toolchains/<name>. We create a marker
        // dir under our installs tree pointing at that toolchain.
        let install_dir = ctx.dirs.install_path(self.id(), &tv.version);
        crate::dirs::create_dir_all(&install_dir)?;
        std::fs::write(install_dir.join(".osdk-complete"), b"")
            .map_err(|e| Error::io(install_dir.join(".osdk-complete"), e))?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // rustup toolchain bins live at RUSTUP_HOME/toolchains/<name>/bin.
        let toolchain_dir = Self::toolchain_dir(ctx, &tv.version);
        Ok(vec![toolchain_dir.join("bin")])
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
