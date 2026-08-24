use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::backend::aube_host::{
    self, EmbeddedFrozenInstallRequest, EmbeddedInstallRequest, EmbeddedLockGraphRequest,
};
use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::inventory::{DynamicToolBin, DynamicToolManifest};
use crate::pipeline;
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

const PROVIDER: &str = "npm-package";
const PROJECT_DIR: &str = "project";
const STORE_DIR: &str = "store";
const CACHE_DIR: &str = "cache";
const METADATA_PROVIDER: &str = "provider";
const METADATA_PACKAGE: &str = "package";
const METADATA_RUNTIME: &str = "runtime";
const METADATA_RECEIPT_URL: &str = "artifact_receipt_url";
const METADATA_RECEIPT_FILE: &str = "artifact_receipt_file";
const METADATA_RECEIPT_CHECKSUM: &str = "artifact_receipt_checksum";
const AUBE_LOCKFILE_NAME: &str = "aube-lock.yaml";
const AUBE_LOCK_FORMAT: &str = "aube-v9";

pub const LOCKED_NPM_PACKAGE_OPTION: &str = "__osdk_npm_package";
pub const LOCKED_NPM_LOCK_FORMAT_OPTION: &str = "__osdk_npm_lock_format";
pub const LOCKED_NPM_LOCK_SHA256_OPTION: &str = "__osdk_npm_lock_sha256";
pub const LOCKED_NPM_LOCKFILE_OPTION: &str = "__osdk_npm_lockfile";

pub struct NpmPackageBackend {
    id: String,
    package: String,
}

#[derive(Debug)]
struct LockedNpmGraph<'a> {
    lockfile: &'a str,
}

impl NpmPackageBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let package = id.strip_prefix("npm:")?;
        validate_npm_package_name(package)?;
        Some(Self {
            id: format!("npm:{package}"),
            package: package.to_string(),
        })
    }

    fn install_root(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs.install_path(self.id(), version)
    }

    fn project_dir(&self, ctx: &Ctx, version: &str) -> PathBuf {
        self.install_root(ctx, version).join(PROJECT_DIR)
    }

    fn aube_cache_dir(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs
            .cache
            .join("aube")
            .join(crate::dirs::sanitize_tool_id(self.id()))
            .join(version)
            .join(CACHE_DIR)
    }

    fn aube_store_dir(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs
            .cache
            .join("aube")
            .join(crate::dirs::sanitize_tool_id(self.id()))
            .join(version)
            .join(STORE_DIR)
    }

    fn package_spec(&self, tv: &ToolVersion) -> String {
        format!("{}@{}", self.package, tv.version)
    }

    fn build_policy(tv: &ToolVersion) -> Result<BuildPolicy> {
        let Some(raw) = tv.options.get("allow_builds") else {
            return Ok(BuildPolicy::Deny);
        };
        let raw = raw.trim();
        if raw.is_empty()
            || matches!(
                raw.to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            )
        {
            return Ok(BuildPolicy::Deny);
        }
        if matches!(
            raw.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ) {
            return Ok(BuildPolicy::AllowAll);
        }
        let packages = raw
            .split(',')
            .map(str::trim)
            .filter(|package| !package.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if packages.is_empty() {
            return Err(Error::config(
                "allow_builds must be a boolean or a non-empty package list",
            ));
        }
        Ok(BuildPolicy::Packages(packages))
    }

    fn write_project_manifest(
        project_dir: &Path,
        dependency: Option<(&str, &str)>,
        build_policy: &BuildPolicy,
    ) -> Result<()> {
        std::fs::create_dir_all(project_dir).map_err(|error| Error::io(project_dir, error))?;
        let mut manifest = serde_json::json!({
            "name": "osdk-dynamic-npm-tool",
            "private": true
        });
        if let Some((package, version)) = dependency {
            manifest["dependencies"] = serde_json::Value::Object(
                [(
                    package.to_string(),
                    serde_json::Value::String(version.to_string()),
                )]
                .into_iter()
                .collect(),
            );
        }
        if let BuildPolicy::Packages(packages) = build_policy {
            let allow_builds = packages
                .iter()
                .map(|package| (package.clone(), serde_json::Value::Bool(true)))
                .collect::<serde_json::Map<_, _>>();
            manifest["aube"] = serde_json::json!({ "allowBuilds": allow_builds });
        }
        let package_json = project_dir.join("package.json");
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        std::fs::write(&package_json, bytes).map_err(|error| Error::io(&package_json, error))
    }

    fn locked_graph<'a>(&self, tv: &'a ToolVersion) -> Result<Option<LockedNpmGraph<'a>>> {
        let values = [
            tv.options.get(LOCKED_NPM_PACKAGE_OPTION),
            tv.options.get(LOCKED_NPM_LOCK_FORMAT_OPTION),
            tv.options.get(LOCKED_NPM_LOCK_SHA256_OPTION),
            tv.options.get(LOCKED_NPM_LOCKFILE_OPTION),
        ];
        if values.iter().all(|value| value.is_none()) {
            return Ok(None);
        }

        let required = |key, value: Option<&'a String>| {
            value.map(String::as_str).ok_or_else(|| {
                Error::other(format!(
                    "locked npm graph is missing private option `{key}`"
                ))
            })
        };
        let package = required(LOCKED_NPM_PACKAGE_OPTION, values[0])?;
        let format = required(LOCKED_NPM_LOCK_FORMAT_OPTION, values[1])?;
        let sha256 = required(LOCKED_NPM_LOCK_SHA256_OPTION, values[2])?;
        let lockfile = required(LOCKED_NPM_LOCKFILE_OPTION, values[3])?;

        if tv.backend != self.id || package != self.package {
            return Err(Error::other(format!(
                "locked npm graph identity mismatch: expected {} for package {}, got {} for package {}",
                self.id, self.package, tv.backend, package
            )));
        }
        if format != AUBE_LOCK_FORMAT {
            return Err(Error::other(format!(
                "unsupported locked npm graph format `{format}` for {}; expected {AUBE_LOCK_FORMAT}",
                self.id
            )));
        }
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::other(format!(
                "locked npm graph for {} has an invalid SHA-256 digest",
                self.id
            )));
        }
        let actual = pipeline::verify::hash_bytes(lockfile.as_bytes(), pipeline::HashAlgo::Sha256);
        if actual != sha256 {
            return Err(Error::ChecksumMismatch {
                name: format!("locked npm graph for {}", self.id),
                expected: sha256.to_string(),
                actual,
            });
        }

        Ok(Some(LockedNpmGraph { lockfile }))
    }

    fn restore_locked_project(
        &self,
        project_dir: &Path,
        tv: &ToolVersion,
        build_policy: &BuildPolicy,
        graph: &LockedNpmGraph<'_>,
    ) -> Result<()> {
        Self::write_project_manifest(
            project_dir,
            Some((&self.package, &tv.version)),
            build_policy,
        )?;
        let lockfile_path = project_dir.join(AUBE_LOCKFILE_NAME);
        std::fs::write(&lockfile_path, graph.lockfile.as_bytes())
            .map_err(|error| Error::io(lockfile_path, error))
    }

    pub async fn prepare_lock_graph(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        if tv.backend != self.id {
            return Err(Error::other(format!(
                "cannot prepare npm lock graph for {} using resolved tool {}",
                self.id, tv.backend
            )));
        }
        let lock_path = ctx
            .dirs
            .lock_dir(self.id())
            .join(format!("{}.lock", tv.version));
        let _lock = crate::lock::FileLock::acquire(lock_path)?;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let source = sources.first().ok_or_else(|| Error::NoUsableSource {
            tool: self.id().to_string(),
            tried: 0,
        })?;
        let project_dir = self.project_dir(ctx, &tv.version);
        let build_policy = Self::build_policy(tv)?;
        Self::write_project_manifest(
            &project_dir,
            Some((&self.package, &tv.version)),
            &build_policy,
        )?;
        Self::write_project_npmrc(&project_dir, Some(&source.download_url))?;
        aube_host::prepare_lock_graph(EmbeddedLockGraphRequest {
            project_dir: &project_dir,
            cache_dir: self.aube_cache_dir(ctx, &tv.version),
            store_dir: self.aube_store_dir(ctx, &tv.version),
            node_bin_dir: managed_node_bin_dir(ctx)?,
            offline: ctx.config.settings.offline,
        })
        .await?;
        let lockfile_path = project_dir.join(AUBE_LOCKFILE_NAME);
        if !lockfile_path.is_file() {
            return Err(Error::other(format!(
                "aube did not produce a lock graph for {}@{}",
                self.package, tv.version
            )));
        }
        Ok(lockfile_path)
    }

    fn write_project_npmrc(project_dir: &Path, url: Option<&str>) -> Result<()> {
        let npmrc = project_dir.join(".npmrc");
        if let Some(url) = url {
            let contents = format!("registry={url}\n");
            std::fs::write(&npmrc, contents).map_err(|error| Error::io(&npmrc, error))?;
        } else if npmrc.exists() {
            std::fs::remove_file(&npmrc).map_err(|error| Error::io(&npmrc, error))?;
        }
        Ok(())
    }

    fn validate_install_layout(project_dir: &Path, package: &str) -> Result<PathBuf> {
        let package_dir = package_install_dir(project_dir, package);
        if !package_dir.is_dir() {
            return Err(Error::other(format!(
                "embedded npm install did not materialize {} under {}",
                package,
                project_dir.display()
            )));
        }
        let bin_dir = project_dir.join("node_modules").join(".bin");
        if !bin_dir.is_dir() {
            return Err(Error::other(format!(
                "embedded npm install did not produce node_modules/.bin for {}",
                package
            )));
        }
        Ok(bin_dir)
    }

    fn build_manifest(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        bin_dir: &Path,
        receipt: Option<&pipeline::ArtifactReceipt>,
    ) -> Result<DynamicToolManifest> {
        let install_root = self.install_root(ctx, &tv.version);
        let mut manifest = DynamicToolManifest::new(self.id())?;
        manifest.version = Some(tv.version.clone());
        manifest.bins = discover_bins(&install_root, bin_dir)?;
        manifest
            .metadata
            .insert(METADATA_PROVIDER.into(), PROVIDER.into());
        manifest
            .metadata
            .insert(METADATA_PACKAGE.into(), self.package.clone());
        manifest
            .metadata
            .insert(METADATA_RUNTIME.into(), "node".into());
        if let Some(receipt) = receipt {
            if !receipt.url.is_empty() {
                manifest
                    .metadata
                    .insert(METADATA_RECEIPT_URL.into(), receipt.url.clone());
            }
            if !receipt.file_name.is_empty() {
                manifest
                    .metadata
                    .insert(METADATA_RECEIPT_FILE.into(), receipt.file_name.clone());
            }
            if let Some(checksum) = &receipt.checksum {
                manifest
                    .metadata
                    .insert(METADATA_RECEIPT_CHECKSUM.into(), checksum.clone());
            }
        }
        manifest.normalize()
    }
}

#[async_trait]
impl Backend for NpmPackageBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        crate::backend::npm_cli::NpmBackend.default_sources()
    }

    fn probe_url(&self, ctx: &Ctx, source: &Source) -> Option<String> {
        crate::backend::npm_cli::NpmBackend.probe_url(ctx, source)
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, &self.package).await?;
        Ok(versions
            .into_iter()
            .map(|version| VersionInfo {
                stable: !version.contains('-'),
                version,
                lts: None,
            })
            .collect())
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        crate::npm::resolve_package_version(ctx, &sources, &self.package, self.id(), req).await
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let install_root = self.install_root(ctx, &tv.version);
        let project_dir = self.project_dir(ctx, &tv.version);
        if install_root.join(".osdk-complete").is_file() {
            return Ok(());
        }
        let lock_path = ctx
            .dirs
            .lock_dir(self.id())
            .join(format!("{}.lock", tv.version));
        let _lock = crate::lock::FileLock::acquire(lock_path)?;
        if install_root.join(".osdk-complete").is_file() {
            return Ok(());
        }
        let locked_graph = self.locked_graph(tv)?;
        let locked_receipt = if locked_graph.is_some() {
            pipeline::locked_artifact(tv)?
        } else {
            None
        };
        if ctx.config.settings.offline && locked_graph.is_none() {
            return Err(Error::other(format!(
                "cannot install {}@{} offline without a locked npm dependency graph; run `osdk lock` online first",
                self.id, tv.version
            )));
        }
        let unlocked_resolution = if locked_graph.is_none() {
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, &self.package, &tv.version).await?;
            if dist.checksum.is_none() {
                return Err(Error::other(format!(
                    "npm package {}@{} has no supported SRI checksum",
                    self.package, tv.version
                )));
            }
            Some((sources, dist))
        } else {
            None
        };
        if install_root.exists() {
            let _ = std::fs::remove_dir_all(&install_root);
        }
        std::fs::create_dir_all(&install_root).map_err(|error| Error::io(&install_root, error))?;

        let build_policy = Self::build_policy(tv)?;
        let node_bin_dir = managed_node_bin_dir(ctx)?;
        let receipt = if let Some(graph) = locked_graph {
            self.restore_locked_project(&project_dir, tv, &build_policy, &graph)?;
            Self::write_project_npmrc(&project_dir, None)?;
            let request = EmbeddedFrozenInstallRequest {
                project_dir: &project_dir,
                cache_dir: self.aube_cache_dir(ctx, &tv.version),
                store_dir: self.aube_store_dir(ctx, &tv.version),
                node_bin_dir,
                scripts_enabled: !matches!(build_policy, BuildPolicy::Deny),
                dangerously_allow_all_builds: matches!(build_policy, BuildPolicy::AllowAll),
                offline: ctx.config.settings.offline,
            };
            if let Err(error) = aube_host::install_frozen(request).await {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
            locked_receipt
        } else {
            let (sources, dist) = unlocked_resolution
                .expect("unlocked npm resolution is prepared before mutating the install root");
            let package_spec = self.package_spec(tv);
            let mut last_error = None;
            let mut selected_registry = None;
            for source in &sources {
                if install_root.exists() {
                    let _ = std::fs::remove_dir_all(&install_root);
                }
                std::fs::create_dir_all(&install_root)
                    .map_err(|error| Error::io(&install_root, error))?;
                Self::write_project_manifest(&project_dir, None, &build_policy)?;
                Self::write_project_npmrc(&project_dir, Some(&source.download_url))?;
                let request = EmbeddedInstallRequest {
                    project_dir: &project_dir,
                    packages: std::slice::from_ref(&package_spec),
                    cache_dir: self.aube_cache_dir(ctx, &tv.version),
                    store_dir: self.aube_store_dir(ctx, &tv.version),
                    node_bin_dir: node_bin_dir.clone(),
                    scripts_enabled: !matches!(build_policy, BuildPolicy::Deny),
                    dangerously_allow_all_builds: matches!(build_policy, BuildPolicy::AllowAll),
                    offline: false,
                };
                match aube_host::install_packages(request).await {
                    Ok(()) => {
                        last_error = None;
                        selected_registry = Some(source.download_url.clone());
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }
            if let Some(error) = last_error {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
            Some(pipeline::ArtifactReceipt {
                // Root package identity reported by registries. The complete,
                // integrity-checked transitive graph remains in aube-lock.yaml.
                url: selected_registry.unwrap_or_else(|| dist.urls[0].clone()),
                file_name: format!("{}-{}.tgz", self.package.replace('/', "__"), tv.version),
                checksum: dist.checksum.as_ref().map(format_checksum),
                evidence: Vec::new(),
            })
        };

        let bin_dir = match Self::validate_install_layout(&project_dir, &self.package) {
            Ok(bin_dir) => bin_dir,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        };
        let manifest = match self.build_manifest(ctx, tv, &bin_dir, receipt.as_ref()) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        };
        manifest.write_atomic(&install_root)?;
        if let Some(receipt) = &receipt {
            let receipt_path = install_root.join(".osdk-artifact.json");
            let receipt_bytes = serde_json::to_vec_pretty(receipt)?;
            std::fs::write(&receipt_path, receipt_bytes)
                .map_err(|error| Error::io(&receipt_path, error))?;
        }
        std::fs::write(install_root.join(".osdk-complete"), b"")
            .map_err(|error| Error::io(install_root.join(".osdk-complete"), error))?;
        Ok(())
    }

    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let install_root = self.install_root(ctx, &tv.version);
        if !install_root.exists() {
            return Ok(());
        }
        let _ = crate::inventory::remove_manifest(&install_root);
        std::fs::remove_dir_all(&install_root).map_err(|error| Error::io(&install_root, error))
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![self
            .project_dir(ctx, &tv.version)
            .join("node_modules")
            .join(".bin")])
    }

    fn exec_env(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let mut env = crate::cache::manager_exec_env(
            &ctx.dirs.cache,
            &[
                ("npm_config_cache", "npm"),
                ("npm_config_store_dir", "npm-store"),
            ],
        );
        env.insert(
            "npm_config_cache".into(),
            self.aube_cache_dir(ctx, &tv.version).display().to_string(),
        );
        env.insert(
            "npm_config_store_dir".into(),
            self.aube_store_dir(ctx, &tv.version).display().to_string(),
        );
        Ok(env)
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let mut names = Vec::new();
        for path in paths {
            names.extend(discover_bin_names(&path)?);
        }
        names.sort();
        names.dedup();
        if names.is_empty() {
            return Err(Error::other(format!(
                "dynamic npm tool {} exposes no validated executables",
                self.id()
            )));
        }
        Ok(names)
    }
}

fn validate_npm_package_name(package: &str) -> Option<()> {
    if let Some(rest) = package.strip_prefix('@') {
        let (scope, name) = rest.split_once('/')?;
        if !valid_npm_segment(scope) || !valid_npm_segment(name) || name.contains('/') {
            return None;
        }
        return Some(());
    }
    if !valid_npm_segment(package) {
        return None;
    }
    Some(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildPolicy {
    Deny,
    Packages(Vec<String>),
    AllowAll,
}

fn managed_node_bin_dir(ctx: &Ctx) -> Result<PathBuf> {
    let node = crate::backend::node::NodeBackend;
    let versions = node.list_installed(ctx)?;
    let version = versions
        .last()
        .ok_or_else(|| Error::other("dynamic npm tools require a managed Node installation"))?;
    let tool = ToolVersion::new("node", version);
    node.bin_paths(ctx, &tool)?
        .into_iter()
        .find(|path| path.join(node_executable_name()).is_file())
        .ok_or_else(|| {
            Error::other(format!(
                "managed Node {version} has no executable bin directory"
            ))
        })
}

#[cfg(windows)]
fn node_executable_name() -> &'static str {
    "node.exe"
}
#[cfg(not(windows))]
fn node_executable_name() -> &'static str {
    "node"
}

fn valid_npm_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('@')
        && !value.contains(':')
        && !value.chars().any(char::is_whitespace)
}

fn package_install_dir(project_dir: &Path, package: &str) -> PathBuf {
    let node_modules = project_dir.join("node_modules");
    if let Some(rest) = package.strip_prefix('@') {
        let (scope, name) = rest.split_once('/').expect("validated scoped package");
        return node_modules.join(format!("@{scope}")).join(name);
    }
    node_modules.join(package)
}

fn discover_bins(install_root: &Path, bin_dir: &Path) -> Result<Vec<DynamicToolBin>> {
    let mut bins = Vec::new();
    for name in discover_bin_names(bin_dir)? {
        let absolute = resolve_bin_target(bin_dir, &name)?;
        let relative = absolute
            .strip_prefix(install_root)
            .map_err(|_| {
                Error::other(format!(
                    "bin `{name}` resolves outside install root {}",
                    install_root.display()
                ))
            })?
            .to_path_buf();
        bins.push(DynamicToolBin {
            name,
            path: relative.to_string_lossy().replace('\\', "/"),
        });
    }
    if bins.is_empty() {
        return Err(Error::other(format!(
            "no executable bins discovered under {}",
            bin_dir.display()
        )));
    }
    Ok(bins)
}

fn discover_bin_names(bin_dir: &Path) -> Result<Vec<String>> {
    let read_dir = std::fs::read_dir(bin_dir).map_err(|error| Error::io(bin_dir, error))?;
    let mut names = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|error| Error::io(bin_dir, error))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        #[cfg(windows)]
        let name = {
            let lower = file_name.to_ascii_lowercase();
            let Some(stripped) = lower
                .strip_suffix(".cmd")
                .or_else(|| lower.strip_suffix(".exe"))
                .or_else(|| lower.strip_suffix(".bat"))
            else {
                continue;
            };
            stripped.to_string()
        };
        #[cfg(not(windows))]
        let name = file_name.to_string();
        names.push(name);
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn resolve_bin_target(bin_dir: &Path, name: &str) -> Result<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.exe")),
        bin_dir.join(format!("{name}.bat")),
    ];
    #[cfg(not(windows))]
    let candidates = [bin_dir.join(name)];

    for candidate in candidates {
        if candidate.exists() {
            let target =
                std::fs::canonicalize(&candidate).map_err(|error| Error::io(&candidate, error))?;
            if target.is_file() {
                return Ok(target);
            }
        }
    }
    Err(Error::other(format!(
        "unable to resolve executable target for `{name}` in {}",
        bin_dir.display()
    )))
}

fn format_checksum(checksum: &pipeline::Checksum) -> String {
    let algorithm = match checksum.algo {
        pipeline::HashAlgo::Sha256 => "sha256",
        pipeline::HashAlgo::Sha512 => "sha512",
        pipeline::HashAlgo::Blake3 => "blake3",
    };
    format!("{algorithm}:{}", checksum.hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_and_unscoped_names() {
        assert!(NpmPackageBackend::from_id("npm:prettier").is_some());
        assert!(NpmPackageBackend::from_id("npm:@antfu/ni").is_some());
        assert!(NpmPackageBackend::from_id("npm:@antfu").is_none());
        assert!(NpmPackageBackend::from_id("npm:@antfu/ni/extra").is_none());
    }

    #[test]
    fn package_install_dir_tracks_scope_layout() {
        let root = PathBuf::from("/tmp/install/project");
        assert_eq!(
            package_install_dir(&root, "prettier"),
            root.join("node_modules/prettier")
        );
        assert_eq!(
            package_install_dir(&root, "@antfu/ni"),
            root.join("node_modules/@antfu/ni")
        );
    }

    #[test]
    fn build_policy_is_deny_by_default_and_supports_package_allowlists() {
        let version = ToolVersion::new("npm:prettier", "3.0.0");
        assert_eq!(
            NpmPackageBackend::build_policy(&version).unwrap(),
            BuildPolicy::Deny
        );

        let mut version = version;
        version
            .options
            .insert("allow_builds".into(), "esbuild, sharp".into());
        assert_eq!(
            NpmPackageBackend::build_policy(&version).unwrap(),
            BuildPolicy::Packages(vec!["esbuild".into(), "sharp".into()])
        );

        version.options.insert("allow_builds".into(), "true".into());
        assert_eq!(
            NpmPackageBackend::build_policy(&version).unwrap(),
            BuildPolicy::AllowAll
        );
    }

    #[test]
    fn project_manifest_records_only_explicit_build_allowlist() {
        let temporary = tempfile::tempdir().unwrap();
        NpmPackageBackend::write_project_manifest(
            temporary.path(),
            None,
            &BuildPolicy::Packages(vec!["esbuild".into(), "sharp".into()]),
        )
        .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temporary.path().join("package.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["aube"]["allowBuilds"]["esbuild"], true);
        assert_eq!(manifest["aube"]["allowBuilds"]["sharp"], true);
        assert!(manifest.get("dependencies").is_none());
    }

    fn locked_version(backend: &str, package: &str, version: &str, lockfile: &str) -> ToolVersion {
        let mut tool = ToolVersion::new(backend, version);
        tool.options
            .insert(LOCKED_NPM_PACKAGE_OPTION.into(), package.into());
        tool.options.insert(
            LOCKED_NPM_LOCK_FORMAT_OPTION.into(),
            AUBE_LOCK_FORMAT.into(),
        );
        tool.options.insert(
            LOCKED_NPM_LOCK_SHA256_OPTION.into(),
            pipeline::verify::hash_bytes(lockfile.as_bytes(), pipeline::HashAlgo::Sha256),
        );
        tool.options
            .insert(LOCKED_NPM_LOCKFILE_OPTION.into(), lockfile.into());
        tool
    }

    #[test]
    fn locked_graph_validates_identity_format_and_exact_bytes() {
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let lockfile = "lockfileVersion: '9.0'\n# preserve trailing newline\n";
        let version = locked_version("npm:prettier", "prettier", "3.6.2", lockfile);
        assert_eq!(
            backend.locked_graph(&version).unwrap().unwrap().lockfile,
            lockfile
        );

        let mut mismatched_package = version.clone();
        mismatched_package
            .options
            .insert(LOCKED_NPM_PACKAGE_OPTION.into(), "typescript".into());
        assert!(backend
            .locked_graph(&mismatched_package)
            .unwrap_err()
            .to_string()
            .contains("identity mismatch"));

        let mut unsupported_format = version.clone();
        unsupported_format
            .options
            .insert(LOCKED_NPM_LOCK_FORMAT_OPTION.into(), "pnpm-v8".into());
        assert!(backend
            .locked_graph(&unsupported_format)
            .unwrap_err()
            .to_string()
            .contains("unsupported locked npm graph format"));

        let mut tampered = version;
        tampered.options.insert(
            LOCKED_NPM_LOCKFILE_OPTION.into(),
            "lockfileVersion: '9.0'\n# changed\n".into(),
        );
        assert!(matches!(
            backend.locked_graph(&tampered),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn locked_graph_rejects_partial_and_noncanonical_digests() {
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let mut partial = ToolVersion::new("npm:prettier", "3.6.2");
        partial
            .options
            .insert(LOCKED_NPM_PACKAGE_OPTION.into(), "prettier".into());
        assert!(backend
            .locked_graph(&partial)
            .unwrap_err()
            .to_string()
            .contains(LOCKED_NPM_LOCK_FORMAT_OPTION));

        let lockfile = "lockfileVersion: '9.0'\n";
        let mut uppercase = locked_version("npm:prettier", "prettier", "3.6.2", lockfile);
        let digest = uppercase.options[LOCKED_NPM_LOCK_SHA256_OPTION].to_uppercase();
        uppercase
            .options
            .insert(LOCKED_NPM_LOCK_SHA256_OPTION.into(), digest);
        assert!(backend
            .locked_graph(&uppercase)
            .unwrap_err()
            .to_string()
            .contains("invalid SHA-256"));
    }

    #[test]
    fn restoring_locked_project_preserves_lock_bytes_and_exact_manifest_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = NpmPackageBackend::from_id("npm:@antfu/ni").unwrap();
        let lockfile = "lockfileVersion: '9.0'\nimporters: {}\n";
        let version = locked_version("npm:@antfu/ni", "@antfu/ni", "0.21.12", lockfile);
        let graph = backend.locked_graph(&version).unwrap().unwrap();
        backend
            .restore_locked_project(
                temporary.path(),
                &version,
                &BuildPolicy::Packages(vec!["esbuild".into()]),
                &graph,
            )
            .unwrap();

        let package_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temporary.path().join("package.json")).unwrap())
                .unwrap();
        assert_eq!(package_json["dependencies"]["@antfu/ni"], "0.21.12");
        assert_eq!(package_json["aube"]["allowBuilds"]["esbuild"], true);
        assert_eq!(
            std::fs::read(temporary.path().join(AUBE_LOCKFILE_NAME)).unwrap(),
            lockfile.as_bytes()
        );
    }

    fn offline_test_ctx(root: &Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let mut settings = crate::config::Settings::default();
        settings.offline = true;
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings,
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[tokio::test]
    async fn offline_install_without_graph_fails_before_metadata_or_node_access() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("without a locked npm dependency graph"));
        assert!(!backend.install_root(&ctx, &version.version).exists());
    }

    #[tokio::test]
    async fn already_installed_offline_tool_does_not_require_graph() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let install_root = backend.install_root(&ctx, &version.version);
        std::fs::create_dir_all(&install_root).unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();

        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();
    }

    #[test]
    fn writes_and_removes_project_npmrc() {
        let temporary = tempfile::tempdir().unwrap();
        NpmPackageBackend::write_project_npmrc(
            temporary.path(),
            Some("https://registry.example.test/"),
        )
        .unwrap();
        let npmrc = temporary.path().join(".npmrc");
        assert_eq!(
            std::fs::read_to_string(&npmrc).unwrap(),
            "registry=https://registry.example.test/\n"
        );

        NpmPackageBackend::write_project_npmrc(temporary.path(), None).unwrap();
        assert!(!npmrc.exists());
    }

    #[cfg(unix)]
    #[test]
    fn discovers_bins_inside_install_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let install_root = temporary.path().join("installs/npm/prettier/3.0.0");
        let package_dir = install_root.join("project/node_modules/prettier/bin");
        let bin_dir = install_root.join("project/node_modules/.bin");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = package_dir.join("prettier.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
        symlink("../prettier/bin/prettier.js", bin_dir.join("prettier")).unwrap();

        let bins = discover_bins(&install_root, &bin_dir).unwrap();
        assert_eq!(
            bins,
            vec![DynamicToolBin {
                name: "prettier".into(),
                path: "project/node_modules/prettier/bin/prettier.js".into(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bins_that_escape_install_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let install_root = temporary.path().join("installs/npm/prettier/3.0.0");
        let bin_dir = install_root.join("project/node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let outside_script = outside.path().join("prettier.js");
        std::fs::write(&outside_script, "#!/usr/bin/env node\n").unwrap();
        symlink(&outside_script, bin_dir.join("prettier")).unwrap();

        let error = discover_bins(&install_root, &bin_dir).unwrap_err();
        assert!(error.to_string().contains("outside install root"));
    }

    #[test]
    fn manifest_records_receipt_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let install_root = root.join("data/installs/npm/prettier/3.0.0");
        let bin_dir = install_root.join("project/node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let package_dir = install_root.join("project/node_modules/prettier/bin");
            std::fs::create_dir_all(&package_dir).unwrap();
            let script = package_dir.join("prettier.js");
            std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
            symlink("../prettier/bin/prettier.js", bin_dir.join("prettier")).unwrap();
        }
        #[cfg(windows)]
        {
            let package_dir = install_root.join("project/node_modules/prettier/bin");
            std::fs::create_dir_all(&package_dir).unwrap();
            let script = package_dir.join("prettier.js");
            std::fs::write(&script, "console.log('ok')\n").unwrap();
            std::fs::write(bin_dir.join("prettier.cmd"), "@echo off\r\n").unwrap();
        }

        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: crate::config::Settings::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.0.0");
        let receipt = pipeline::ArtifactReceipt {
            url: "https://registry.example.test/prettier/-/prettier-3.0.0.tgz".into(),
            file_name: "prettier-3.0.0.tgz".into(),
            checksum: Some("sha512:deadbeef".into()),
            evidence: Vec::new(),
        };

        let manifest = backend
            .build_manifest(&ctx, &version, &bin_dir, Some(&receipt))
            .unwrap();

        assert_eq!(manifest.metadata.get(METADATA_PROVIDER).unwrap(), PROVIDER);
        assert_eq!(manifest.metadata.get(METADATA_PACKAGE).unwrap(), "prettier");
        assert_eq!(
            manifest.metadata.get(METADATA_RECEIPT_FILE).unwrap(),
            "prettier-3.0.0.tgz"
        );
    }
}
