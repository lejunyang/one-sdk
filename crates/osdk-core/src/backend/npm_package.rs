use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

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
const METADATA_NODE_VERSION: &str = "node_version";
const METADATA_LOCK_SHA256: &str = "lock_sha256";
const METADATA_BUILD_POLICY: &str = "build_policy";
const METADATA_ROOT_INTEGRITY: &str = "root_integrity";
const METADATA_ROOT_SOURCE: &str = "root_source";
const AUBE_LOCKFILE_NAME: &str = "aube-lock.yaml";
const AUBE_LOCK_FORMAT: &str = "aube-v9";

pub const LOCKED_NPM_PACKAGE_OPTION: &str = "__osdk_npm_package";
pub const LOCKED_NPM_LOCK_FORMAT_OPTION: &str = "__osdk_npm_lock_format";
pub const LOCKED_NPM_LOCK_SHA256_OPTION: &str = "__osdk_npm_lock_sha256";
pub const LOCKED_NPM_LOCKFILE_OPTION: &str = "__osdk_npm_lockfile";
pub const LOCKED_NPM_NODE_VERSION_OPTION: &str = "__osdk_node_version";

pub struct NpmPackageBackend {
    id: String,
    package: String,
}

#[derive(Debug)]
struct LockedNpmGraph<'a> {
    lockfile: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmGraphIdentity {
    sha256: String,
    root_integrity: String,
    root_source: String,
    root_tarball: Option<String>,
}

struct UnlockedNpmResolution {
    sources: Vec<Source>,
    root_checksum: pipeline::Checksum,
    root_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IdentityLockfile {
    importers: BTreeMap<String, IdentityImporter>,
    packages: BTreeMap<String, IdentityPackage>,
}

#[derive(Debug, Deserialize)]
struct IdentityImporter {
    dependencies: BTreeMap<String, IdentityDependency>,
}

#[derive(Debug, Deserialize)]
struct IdentityDependency {
    specifier: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct IdentityPackage {
    resolution: IdentityResolution,
}

#[derive(Debug, Deserialize)]
struct IdentityResolution {
    integrity: String,
    #[serde(default)]
    tarball: Option<String>,
}

impl NpmPackageBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let package = id.strip_prefix("npm:")?;
        validate_npm_package_name(package)?;
        let package = package.to_ascii_lowercase();
        Some(Self {
            id: format!("npm:{package}"),
            package,
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
            .join(crate::dirs::sanitize_version_component(version))
            .join(CACHE_DIR)
    }

    fn aube_store_dir(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs
            .cache
            .join("aube")
            .join(crate::dirs::sanitize_tool_id(self.id()))
            .join(crate::dirs::sanitize_version_component(version))
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
        let mut packages = raw
            .split(',')
            .map(str::trim)
            .filter(|package| !package.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if packages.is_empty()
            || packages
                .iter()
                .any(|package| validate_npm_package_name(package).is_none())
        {
            return Err(Error::config(crate::t!("err.npm_allow_builds_invalid")));
        }
        packages.sort();
        packages.dedup();
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
                Error::other(crate::t!("err.npm_lock_graph_option_missing", key = key))
            })
        };
        let package = required(LOCKED_NPM_PACKAGE_OPTION, values[0])?;
        let format = required(LOCKED_NPM_LOCK_FORMAT_OPTION, values[1])?;
        let sha256 = required(LOCKED_NPM_LOCK_SHA256_OPTION, values[2])?;
        let lockfile = required(LOCKED_NPM_LOCKFILE_OPTION, values[3])?;

        if tv.backend != self.id || package != self.package {
            return Err(Error::other(crate::t!(
                "err.npm_lock_graph_identity_mismatch",
                expected_tool = self.id,
                expected_package = self.package,
                actual_tool = tv.backend,
                actual_package = package
            )));
        }
        if format != AUBE_LOCK_FORMAT {
            return Err(Error::other(crate::t!(
                "err.npm_lock_graph_format_unsupported",
                format = format,
                tool = self.id,
                expected = AUBE_LOCK_FORMAT
            )));
        }
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::other(crate::t!(
                "err.npm_lock_graph_digest_invalid",
                tool = self.id
            )));
        }
        let actual = pipeline::verify::hash_bytes(lockfile.as_bytes(), pipeline::HashAlgo::Sha256);
        if actual != sha256 {
            return Err(Error::ChecksumMismatch {
                name: crate::t!("label.npm_lock_graph", tool = self.id),
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
            return Err(Error::other(crate::t!(
                "err.npm_lock_graph_tool_mismatch",
                expected = self.id,
                actual = tv.backend
            )));
        }
        let lock_path = ctx.dirs.lock_dir(self.id()).join(format!(
            "{}.lock",
            crate::dirs::sanitize_version_component(&tv.version)
        ));
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
            node_bin_dir: managed_node(ctx, tv)?.0,
            offline: ctx.config.settings.offline,
        })
        .await?;
        let lockfile_path = project_dir.join(AUBE_LOCKFILE_NAME);
        if !lockfile_path.is_file() {
            return Err(Error::other(crate::t!(
                "err.npm_lock_graph_not_produced",
                package = self.package,
                version = tv.version
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
            return Err(Error::other(crate::t!(
                "err.npm_install_package_missing",
                package = package,
                path = project_dir.display()
            )));
        }
        let bin_dir = project_dir.join("node_modules").join(".bin");
        if !bin_dir.is_dir() {
            return Err(Error::other(crate::t!(
                "err.npm_install_bin_dir_missing",
                package = package
            )));
        }
        Ok(bin_dir)
    }

    fn build_manifest(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        bin_dir: &Path,
        node_version: &str,
        build_policy: &BuildPolicy,
        graph_identity: &NpmGraphIdentity,
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
        manifest
            .metadata
            .insert(METADATA_NODE_VERSION.into(), node_version.into());
        manifest
            .metadata
            .insert(METADATA_BUILD_POLICY.into(), build_policy.identity());
        manifest
            .metadata
            .insert(METADATA_LOCK_SHA256.into(), graph_identity.sha256.clone());
        manifest.metadata.insert(
            METADATA_ROOT_INTEGRITY.into(),
            graph_identity.root_integrity.clone(),
        );
        manifest.metadata.insert(
            METADATA_ROOT_SOURCE.into(),
            graph_identity.root_source.clone(),
        );
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
        let mut version =
            crate::npm::resolve_package_version(ctx, &sources, &self.package, self.id(), req)
                .await?;
        if let Some(node_version) = selected_node_version(ctx) {
            version
                .options
                .entry(LOCKED_NPM_NODE_VERSION_OPTION.into())
                .or_insert(node_version);
        }
        Ok(version)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let install_root = self.install_root(ctx, &tv.version);
        let project_dir = self.project_dir(ctx, &tv.version);
        let locked_graph = self.locked_graph(tv)?;
        let build_policy = Self::build_policy(tv)?;
        if ctx.config.settings.offline
            && locked_graph.is_none()
            && !install_root.join(".osdk-complete").is_file()
        {
            return Err(Error::other(crate::t!(
                "err.npm_offline_lock_graph_required",
                tool = self.id,
                version = tv.version
            )));
        }
        let (node_bin_dir, node_version) = managed_node(ctx, tv)?;
        if install_matches(
            &install_root,
            self.id(),
            &self.package,
            &tv.version,
            locked_graph.as_ref(),
            &build_policy,
            &node_version,
        )? {
            return Ok(());
        }
        let lock_path = ctx.dirs.lock_dir(self.id()).join(format!(
            "{}.lock",
            crate::dirs::sanitize_version_component(&tv.version)
        ));
        let _lock = crate::lock::FileLock::acquire(lock_path)?;
        if install_matches(
            &install_root,
            self.id(),
            &self.package,
            &tv.version,
            locked_graph.as_ref(),
            &build_policy,
            &node_version,
        )? {
            return Ok(());
        }
        if ctx.config.settings.offline && locked_graph.is_none() {
            return Err(Error::other(crate::t!(
                "err.npm_offline_lock_graph_required",
                tool = self.id,
                version = tv.version
            )));
        }
        let unlocked_resolution = if locked_graph.is_none() {
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, &self.package, &tv.version).await?;
            let root_checksum = dist.checksum.ok_or_else(|| {
                Error::other(crate::t!(
                    "err.npm_package_sri_missing",
                    package = self.package,
                    version = tv.version
                ))
            })?;
            Some(UnlockedNpmResolution {
                sources,
                root_checksum,
                root_urls: dist.urls,
            })
        } else {
            None
        };
        if install_root.exists() {
            let _ = std::fs::remove_dir_all(&install_root);
        }
        std::fs::create_dir_all(&install_root).map_err(|error| Error::io(&install_root, error))?;

        if let Some(graph) = locked_graph.as_ref() {
            self.restore_locked_project(&project_dir, tv, &build_policy, graph)?;
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
        } else {
            let resolution = unlocked_resolution
                .as_ref()
                .expect("unlocked npm resolution is prepared before mutating the install root");
            let package_spec = self.package_spec(tv);
            let mut last_error = None;
            for source in &resolution.sources {
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
        }

        let bin_dir = match Self::validate_install_layout(&project_dir, &self.package) {
            Ok(bin_dir) => bin_dir,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        };
        let graph_identity = match npm_graph_identity(&project_dir, &self.package, &tv.version) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        };
        if let Some(graph) = locked_graph.as_ref() {
            let expected =
                pipeline::verify::hash_bytes(graph.lockfile.as_bytes(), pipeline::HashAlgo::Sha256);
            if graph_identity.sha256 != expected {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(Error::ChecksumMismatch {
                    name: crate::t!("label.npm_lock_graph", tool = self.id),
                    expected,
                    actual: graph_identity.sha256,
                });
            }
        } else if let Some(resolution) = unlocked_resolution.as_ref() {
            if let Err(error) = validate_unlocked_graph_identity(
                &graph_identity,
                &resolution.root_checksum,
                &resolution.root_urls,
                &self.package,
                &tv.version,
            ) {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        }
        let manifest = match self.build_manifest(
            ctx,
            tv,
            &bin_dir,
            &node_version,
            &build_policy,
            &graph_identity,
        ) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&install_root);
                return Err(error);
            }
        };
        manifest.write_atomic(&install_root)?;
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
            return Err(Error::other(crate::t!(
                "err.npm_dynamic_no_validated_executables",
                tool = self.id()
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

impl BuildPolicy {
    fn identity(&self) -> String {
        match self {
            Self::Deny => "deny".into(),
            Self::AllowAll => "allow-all".into(),
            Self::Packages(packages) => format!("packages:{}", packages.join(",")),
        }
    }
}

fn managed_node(ctx: &Ctx, npm_tool: &ToolVersion) -> Result<(PathBuf, String)> {
    let node = crate::backend::node::NodeBackend;
    let requested = npm_tool
        .options
        .get(LOCKED_NPM_NODE_VERSION_OPTION)
        .cloned()
        .or_else(|| selected_node_version(ctx));
    let version = requested
        .as_ref()
        .ok_or_else(|| Error::other(crate::t!("err.npm_dynamic_managed_node_required")))?;
    let tool = ToolVersion::new("node", version);
    let bin = node
        .bin_paths(ctx, &tool)?
        .into_iter()
        .find(|path| path.join(node_executable_name()).is_file())
        .ok_or_else(|| {
            Error::other(crate::t!(
                "err.managed_node_bin_dir_missing",
                version = version
            ))
        })?;
    Ok((bin, version.clone()))
}

fn selected_node_version(ctx: &Ctx) -> Option<String> {
    let node = crate::backend::node::NodeBackend;
    let versions = node.list_installed(ctx).ok()?;
    if versions.is_empty() {
        return None;
    }
    ctx.config
        .tools
        .get("node")
        .and_then(|spec| {
            let infos = versions
                .iter()
                .map(crate::version::VersionInfo::stable)
                .collect::<Vec<_>>();
            crate::version::select_version(&crate::version::VersionSpec::parse(spec), &infos)
                .map(|version| version.version.clone())
        })
        .or_else(|| {
            versions.into_iter().max_by(|left, right| {
                crate::backend::python::cmp_versions(left, right).then_with(|| left.cmp(right))
            })
        })
}

fn install_matches(
    install_root: &Path,
    expected_id: &str,
    expected_package: &str,
    expected_version: &str,
    locked_graph: Option<&LockedNpmGraph<'_>>,
    build_policy: &BuildPolicy,
    node_version: &str,
) -> Result<bool> {
    if !install_root.join(".osdk-complete").is_file() {
        return Ok(false);
    }
    let manifest = match DynamicToolManifest::load(install_root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    if manifest.id != expected_id
        || manifest.version.as_deref() != Some(expected_version)
        || manifest.metadata.get(METADATA_PROVIDER).map(String::as_str) != Some(PROVIDER)
        || manifest.metadata.get(METADATA_PACKAGE).map(String::as_str) != Some(expected_package)
        || manifest.metadata.get(METADATA_RUNTIME).map(String::as_str) != Some("node")
        || manifest
            .metadata
            .get(METADATA_NODE_VERSION)
            .map(String::as_str)
            != Some(node_version)
        || manifest.metadata.get(METADATA_BUILD_POLICY) != Some(&build_policy.identity())
    {
        return Ok(false);
    }

    let project_dir = install_root.join(PROJECT_DIR);
    if validate_project_manifest(
        &project_dir,
        expected_package,
        expected_version,
        build_policy,
    )
    .is_err()
        || NpmPackageBackend::validate_install_layout(&project_dir, expected_package).is_err()
    {
        return Ok(false);
    }
    let graph_identity = match npm_graph_identity(&project_dir, expected_package, expected_version)
    {
        Ok(identity) => identity,
        Err(_) => return Ok(false),
    };
    if manifest.metadata.get(METADATA_LOCK_SHA256) != Some(&graph_identity.sha256)
        || manifest.metadata.get(METADATA_ROOT_INTEGRITY) != Some(&graph_identity.root_integrity)
        || manifest.metadata.get(METADATA_ROOT_SOURCE) != Some(&graph_identity.root_source)
    {
        return Ok(false);
    }
    if let Some(graph) = locked_graph {
        let expected =
            pipeline::verify::hash_bytes(graph.lockfile.as_bytes(), pipeline::HashAlgo::Sha256);
        return Ok(graph_identity.sha256 == expected);
    }
    Ok(true)
}

fn validate_project_manifest(
    project_dir: &Path,
    package: &str,
    version: &str,
    build_policy: &BuildPolicy,
) -> Result<()> {
    let path = project_dir.join("package.json");
    let bytes = std::fs::read(&path).map_err(|error| Error::io(&path, error))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    if manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(package))
        .and_then(serde_json::Value::as_str)
        != Some(version)
    {
        return Err(Error::other(crate::t!(
            "err.npm_project_manifest_identity_mismatch",
            package = package,
            version = version
        )));
    }
    let expected_allow_builds = match build_policy {
        BuildPolicy::Packages(packages) => Some(
            packages
                .iter()
                .map(|package| (package.clone(), serde_json::Value::Bool(true)))
                .collect::<serde_json::Map<_, _>>(),
        ),
        BuildPolicy::Deny | BuildPolicy::AllowAll => None,
    };
    let actual_allow_builds = manifest
        .get("aube")
        .and_then(|aube| aube.get("allowBuilds"))
        .and_then(serde_json::Value::as_object);
    if actual_allow_builds != expected_allow_builds.as_ref() {
        return Err(Error::other(crate::t!(
            "err.npm_project_manifest_build_policy_mismatch",
            package = package
        )));
    }
    Ok(())
}

fn npm_graph_identity(
    project_dir: &Path,
    package: &str,
    version: &str,
) -> Result<NpmGraphIdentity> {
    let path = project_dir.join(AUBE_LOCKFILE_NAME);
    let bytes = std::fs::read(&path).map_err(|error| Error::io(&path, error))?;
    let lockfile: IdentityLockfile = serde_yaml::from_slice(&bytes).map_err(|error| {
        Error::other(crate::t!(
            "err.npm_graph_parse_invalid",
            path = path.display(),
            error = error
        ))
    })?;
    let dependency = lockfile
        .importers
        .get(".")
        .and_then(|importer| importer.dependencies.get(package))
        .ok_or_else(|| Error::other(crate::t!("err.npm_graph_root_missing", package = package)))?;
    if dependency.specifier != version
        || !(dependency.version == version
            || dependency
                .version
                .strip_prefix(version)
                .is_some_and(|suffix| suffix.starts_with('(')))
    {
        return Err(Error::other(crate::t!(
            "err.npm_graph_root_version_mismatch",
            package = package,
            expected = version,
            actual = dependency.version
        )));
    }
    // Aube v9 keeps peer context on the importer's resolved value and in
    // `snapshots`, while `packages` remains keyed by canonical name/version.
    let package_key = format!("{package}@{version}");
    let root = lockfile.packages.get(&package_key).ok_or_else(|| {
        Error::other(crate::t!(
            "err.npm_graph_resolved_root_missing",
            package = package,
            version = version
        ))
    })?;
    if root.resolution.integrity.trim().is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_graph_root_integrity_missing",
            package = package,
            version = version
        )));
    }
    let root_tarball = root.resolution.tarball.clone();
    Ok(NpmGraphIdentity {
        sha256: pipeline::verify::hash_bytes(&bytes, pipeline::HashAlgo::Sha256),
        root_integrity: root.resolution.integrity.clone(),
        root_source: root_tarball
            .clone()
            .unwrap_or_else(|| format!("npm:{package}@{version}")),
        root_tarball,
    })
}

fn validate_unlocked_graph_identity(
    identity: &NpmGraphIdentity,
    expected_checksum: &pipeline::Checksum,
    expected_urls: &[String],
    package: &str,
    version: &str,
) -> Result<()> {
    let actual_checksum =
        pipeline::verify::parse_sri(&identity.root_integrity).ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_graph_root_integrity_invalid",
                package = package,
                version = version
            ))
        })?;
    if &actual_checksum != expected_checksum {
        return Err(Error::ChecksumMismatch {
            name: format!("npm root package {package}@{version}"),
            expected: format_checksum(expected_checksum),
            actual: format_checksum(&actual_checksum),
        });
    }
    if let Some(tarball) = identity.root_tarball.as_ref() {
        if !expected_urls.iter().any(|url| url == tarball) {
            return Err(Error::other(crate::t!(
                "err.npm_graph_root_source_mismatch",
                package = package,
                version = version
            )));
        }
    }
    Ok(())
}

fn format_checksum(checksum: &pipeline::Checksum) -> String {
    let algorithm = match checksum.algo {
        pipeline::HashAlgo::Sha256 => "sha256",
        pipeline::HashAlgo::Sha512 => "sha512",
        pipeline::HashAlgo::Blake3 => "blake3",
    };
    format!("{algorithm}:{}", checksum.hex)
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
        && value.len() <= 214
        && value != "."
        && value != ".."
        && !is_windows_reserved_component(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn is_windows_reserved_component(value: &str) -> bool {
    let trimmed = value.trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
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
                Error::other(crate::t!(
                    "err.npm_bin_outside_install_root",
                    name = name,
                    path = install_root.display()
                ))
            })?
            .to_path_buf();
        bins.push(DynamicToolBin {
            name,
            path: relative.to_string_lossy().replace('\\', "/"),
        });
    }
    if bins.is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_bins_not_discovered",
            path = bin_dir.display()
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
    Err(Error::other(crate::t!(
        "err.npm_bin_target_unresolved",
        name = name,
        path = bin_dir.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_and_unscoped_names() {
        let unscoped = NpmPackageBackend::from_id("npm:Prettier").unwrap();
        assert_eq!(unscoped.id, "npm:prettier");
        assert_eq!(unscoped.package, "prettier");
        let scoped = NpmPackageBackend::from_id("npm:@Antfu/Ni").unwrap();
        assert_eq!(scoped.id, "npm:@antfu/ni");
        assert_eq!(scoped.package, "@antfu/ni");
        assert!(NpmPackageBackend::from_id("npm:@antfu").is_none());
        assert!(NpmPackageBackend::from_id("npm:@antfu/ni/extra").is_none());
        for invalid in [
            "npm:foo#bar",
            "npm:foo?bar",
            "npm:foo%2fbar",
            "npm:foo bar",
            "npm:foo\\bar",
            "npm:.",
            "npm:..",
            "npm:CON",
            "npm:@scope/AUX",
        ] {
            assert!(NpmPackageBackend::from_id(invalid).is_none(), "{invalid}");
        }
        assert!(NpmPackageBackend::from_id(&format!("npm:{}", "a".repeat(215))).is_none());
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
            .insert("allow_builds".into(), "Sharp, esbuild, sharp".into());
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

    fn npm_test_lockfile(package: &str, version: &str, integrity: &str) -> String {
        format!(
            "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies:\n      '{package}':\n        specifier: {version}\n        version: {version}\n\npackages:\n  '{package}@{version}':\n    resolution: {{integrity: {integrity}}}\n"
        )
    }

    fn write_reusable_install(
        install_root: &Path,
        id: &str,
        package: &str,
        version: &str,
        node_version: &str,
        build_policy: &BuildPolicy,
        lockfile: &str,
    ) {
        let project_dir = install_root.join(PROJECT_DIR);
        NpmPackageBackend::write_project_manifest(
            &project_dir,
            Some((package, version)),
            build_policy,
        )
        .unwrap();
        std::fs::write(project_dir.join(AUBE_LOCKFILE_NAME), lockfile).unwrap();
        std::fs::create_dir_all(package_install_dir(&project_dir, package)).unwrap();
        std::fs::create_dir_all(project_dir.join("node_modules/.bin")).unwrap();
        let identity = npm_graph_identity(&project_dir, package, version).unwrap();
        let mut manifest = DynamicToolManifest::new(id).unwrap();
        manifest.version = Some(version.into());
        manifest
            .metadata
            .insert(METADATA_PROVIDER.into(), PROVIDER.into());
        manifest
            .metadata
            .insert(METADATA_PACKAGE.into(), package.into());
        manifest
            .metadata
            .insert(METADATA_RUNTIME.into(), "node".into());
        manifest
            .metadata
            .insert(METADATA_NODE_VERSION.into(), node_version.into());
        manifest
            .metadata
            .insert(METADATA_BUILD_POLICY.into(), build_policy.identity());
        manifest
            .metadata
            .insert(METADATA_LOCK_SHA256.into(), identity.sha256);
        manifest
            .metadata
            .insert(METADATA_ROOT_INTEGRITY.into(), identity.root_integrity);
        manifest
            .metadata
            .insert(METADATA_ROOT_SOURCE.into(), identity.root_source);
        manifest.write_atomic(install_root).unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();
    }

    #[test]
    fn completed_install_is_reused_only_for_exact_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let install_root = temporary.path().join("install");
        let lockfile = npm_test_lockfile(
            "prettier",
            "3.6.2",
            "sha512-I7AIg5boAr5R0FFtJ6rCfD+LFsWHp81dolrFD8S79U9tb8Az2nGrJncnMSnys+bpQJfRUzqs9hnA81OAA3hCuQ==",
        );
        write_reusable_install(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            "20.10.0",
            &BuildPolicy::Deny,
            &lockfile,
        );
        let graph = LockedNpmGraph {
            lockfile: &lockfile,
        };

        assert!(install_matches(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            Some(&graph),
            &BuildPolicy::Deny,
            "20.10.0"
        )
        .unwrap());
        for (id, package, version, node) in [
            ("npm:typescript", "prettier", "3.6.2", "20.10.0"),
            ("npm:prettier", "typescript", "3.6.2", "20.10.0"),
            ("npm:prettier", "prettier", "3.6.1", "20.10.0"),
            ("npm:prettier", "prettier", "3.6.2", "20.9.0"),
        ] {
            assert!(!install_matches(
                &install_root,
                id,
                package,
                version,
                Some(&graph),
                &BuildPolicy::Deny,
                node,
            )
            .unwrap());
        }
        assert!(!install_matches(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            Some(&LockedNpmGraph {
                lockfile: "different"
            }),
            &BuildPolicy::Deny,
            "20.10.0"
        )
        .unwrap());
        assert!(!install_matches(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            Some(&graph),
            &BuildPolicy::Packages(vec!["esbuild".into()]),
            "20.10.0"
        )
        .unwrap());
    }

    #[test]
    fn unlocked_reuse_validates_persisted_graph_and_root_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let install_root = temporary.path().join("install");
        let lockfile = npm_test_lockfile("prettier", "3.6.2", "sha512-root-integrity");
        write_reusable_install(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            "20.10.0",
            &BuildPolicy::Deny,
            &lockfile,
        );
        assert!(install_matches(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            None,
            &BuildPolicy::Deny,
            "20.10.0"
        )
        .unwrap());

        let tampered = lockfile.replace("sha512-root-integrity", "sha512-tampered");
        std::fs::write(
            install_root.join(PROJECT_DIR).join(AUBE_LOCKFILE_NAME),
            tampered,
        )
        .unwrap();
        assert!(!install_matches(
            &install_root,
            "npm:prettier",
            "prettier",
            "3.6.2",
            None,
            &BuildPolicy::Deny,
            "20.10.0"
        )
        .unwrap());
    }

    #[test]
    fn graph_identity_handles_scoped_root_with_peer_context() {
        let temporary = tempfile::tempdir().unwrap();
        let project_dir = temporary.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let lockfile = "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies:\n      '@scope/tool':\n        specifier: 1.2.3\n        version: 1.2.3(peer@4.5.6)\n\npackages:\n  '@scope/tool@1.2.3':\n    resolution: {integrity: sha512-root, tarball: https://registry.example.test/tool.tgz}\n";
        std::fs::write(project_dir.join(AUBE_LOCKFILE_NAME), lockfile).unwrap();

        let identity = npm_graph_identity(&project_dir, "@scope/tool", "1.2.3").unwrap();
        assert_eq!(identity.root_integrity, "sha512-root");
        assert_eq!(
            identity.root_source,
            "https://registry.example.test/tool.tgz"
        );
    }

    #[test]
    fn unlocked_graph_identity_must_match_registry_sri_and_known_tarball() {
        let integrity = "sha512-I7AIg5boAr5R0FFtJ6rCfD+LFsWHp81dolrFD8S79U9tb8Az2nGrJncnMSnys+bpQJfRUzqs9hnA81OAA3hCuQ==";
        let expected = pipeline::verify::parse_sri(integrity).unwrap();
        let tarball = "https://registry.example.test/prettier/-/prettier-3.6.2.tgz";
        let identity = NpmGraphIdentity {
            sha256: "graph".into(),
            root_integrity: integrity.into(),
            root_source: tarball.into(),
            root_tarball: Some(tarball.into()),
        };
        assert!(validate_unlocked_graph_identity(
            &identity,
            &expected,
            &[tarball.into()],
            "prettier",
            "3.6.2"
        )
        .is_ok());

        let mut mismatched_integrity = identity.clone();
        mismatched_integrity.root_integrity = "sha256-YWJj".into();
        assert!(matches!(
            validate_unlocked_graph_identity(
                &mismatched_integrity,
                &expected,
                &[tarball.into()],
                "prettier",
                "3.6.2"
            ),
            Err(Error::ChecksumMismatch { .. })
        ));

        let mut mismatched_source = identity;
        mismatched_source.root_tarball = Some("https://evil.example.test/tool.tgz".into());
        assert!(validate_unlocked_graph_identity(
            &mismatched_source,
            &expected,
            &[tarball.into()],
            "prettier",
            "3.6.2"
        )
        .is_err());
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
        let settings = crate::config::Settings {
            offline: true,
            ..Default::default()
        };
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
    async fn marker_only_offline_install_is_not_reused() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let install_root = backend.install_root(&ctx, &version.version);
        std::fs::create_dir_all(&install_root).unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("dynamic npm tools require a managed Node installation"));
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
    fn manifest_records_only_npm_graph_identity_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let data_root = root.join("data");
        std::fs::create_dir_all(&data_root).unwrap();
        let data_root = std::fs::canonicalize(data_root).unwrap();
        let graph_identity = NpmGraphIdentity {
            sha256: "graph-sha256".into(),
            root_integrity: "sha512-root-integrity".into(),
            root_source: "npm-registry".into(),
            root_tarball: None,
        };
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(data_root.display().to_string()),
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
        let install_root = backend.install_root(&ctx, &version.version);
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

        let manifest = backend
            .build_manifest(
                &ctx,
                &version,
                &bin_dir,
                "24.0.0",
                &BuildPolicy::Deny,
                &graph_identity,
            )
            .unwrap();

        assert_eq!(manifest.metadata.get(METADATA_PROVIDER).unwrap(), PROVIDER);
        assert_eq!(manifest.metadata.get(METADATA_PACKAGE).unwrap(), "prettier");
        assert_eq!(
            manifest.metadata.get(METADATA_ROOT_INTEGRITY).unwrap(),
            "sha512-root-integrity"
        );
        assert_eq!(
            manifest.metadata.get(METADATA_ROOT_SOURCE).unwrap(),
            "npm-registry"
        );
        assert_eq!(
            manifest.metadata.get(METADATA_LOCK_SHA256).unwrap(),
            "graph-sha256"
        );
        assert_eq!(manifest.metadata.len(), 8);
    }
}
