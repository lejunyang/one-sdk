//! The `Backend` trait — the uniform contract every SDK implements — plus the
//! shared contexts and the backend registry.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::Config;
use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::platform::Platform;
use crate::source::Source;
use crate::store::Cas;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

pub mod node;
pub mod registry;

/// Read-only context available to every backend operation.
pub struct Ctx {
    pub dirs: Dirs,
    pub platform: Platform,
    pub config: Config,
    pub client: reqwest::Client,
    pub cas: Arc<Cas>,
    pub show_progress: bool,
}

/// Context for an install (adds progress preference; the pipeline handles locks).
pub struct InstallCtx<'a> {
    pub ctx: &'a Ctx,
}

/// The uniform interface for every SDK. Archive-based backends run the shared
/// pipeline in `install`; delegate backends (rustup/corepack) shell out.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Canonical id, e.g. "node", "go", "python".
    fn id(&self) -> &str;

    /// Alternate names accepted on the CLI (e.g. "nodejs" -> "node").
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// The default source list for this backend (official + mirrors).
    fn default_sources(&self) -> Vec<Source>;

    /// A representative small URL used to speed-probe a source. Given a source's
    /// download base, return a URL to fetch for measuring throughput.
    fn probe_url(&self, ctx: &Ctx, source: &Source) -> Option<String>;

    /// List installable versions (typically parsed from a remote index).
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>>;

    /// Resolve a request (e.g. `20`, `lts`) to a concrete version.
    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        // Default implementation: resolve against list_remote_versions.
        use crate::version::{select_version, VersionSpec};
        if let VersionSpec::Exact(v) = &req.spec {
            return Ok(ToolVersion::new(self.id(), v.clone()));
        }
        let versions = self.list_remote_versions(ctx).await?;
        let chosen = select_version(&req.spec, &versions).ok_or_else(|| Error::VersionResolve {
            tool: self.id().to_string(),
            spec: req.spec.to_string(),
            hint: Some("no matching version found".into()),
        })?;
        let mut tv = ToolVersion::new(self.id(), chosen.version.clone());
        tv.options = req.options.clone();
        Ok(tv)
    }

    /// Install a concrete version.
    async fn install(&self, ctx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()>;

    /// Remove an installed version. Default removes the install dir.
    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let dir = ctx.dirs.install_path(self.id(), &tv.version);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        }
        Ok(())
    }

    /// List locally installed versions (dirs with a complete marker).
    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        let base = ctx.dirs.installs.join(self.id());
        let mut out = Vec::new();
        if base.exists() {
            for entry in std::fs::read_dir(&base).map_err(|e| Error::io(&base, e))? {
                let entry = entry.map_err(|e| Error::io(&base, e))?;
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if crate::pipeline::is_installed(&ctx.dirs, self.id(), &name) {
                    out.push(name);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Directories to add to PATH for an installed version (its `bin/` dirs).
    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>>;

    /// Environment variables to export when this version is active
    /// (e.g. GOROOT, JAVA_HOME).
    fn exec_env(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    /// Executable names this version exposes (used to generate shims).
    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>>;

    /// Idiomatic version files this backend understands (e.g. `.nvmrc`).
    fn idiomatic_files(&self) -> &[&str] {
        &[]
    }
}

/// Convenience: read the executable basenames present in a set of bin dirs.
pub fn bin_names_in_dirs(dirs: &[PathBuf]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut names = BTreeSet::new();
    for dir in dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if is_executable(&path) {
                    if let Some(stem) = exe_stem(&path) {
                        names.insert(stem);
                    }
                }
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("exe") | Some("cmd") | Some("bat")
    )
}

/// Basename without the platform executable extension.
fn exe_stem(path: &std::path::Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        for ext in [".exe", ".cmd", ".bat"] {
            if name.to_ascii_lowercase().ends_with(ext) {
                return Some(name[..name.len() - ext.len()].to_string());
            }
        }
    }
    Some(name)
}
