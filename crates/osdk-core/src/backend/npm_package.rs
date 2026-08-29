use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::aube_host::{
    self, EmbeddedFrozenInstallRequest, EmbeddedInstallRequest, EmbeddedLockGraphRequest,
};
use crate::backend::{Backend, Ctx, InstallCtx};
use crate::config::ToolConfigOrigin;
use crate::error::{Error, Result};
use crate::inventory::{DynamicToolBin, DynamicToolManifest};
use crate::npm_tools::{
    NpmInstaller, ToolScope, LOCKED_NPM_INSTALLER_OPTION, LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION,
    LOCKED_NPM_NATIVE_LOCK_KIND_OPTION, LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION,
    LOCKED_NPM_SCOPE_OPTION,
};
use crate::pipeline;
use crate::source::Source;
use crate::tool::{InstallDependency, InstallDependencyKind, InstallIdentity, InstallScope};
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

const PROVIDER: &str = "npm-package";
const GLOBAL_INSTALL_NAMESPACE: &str = "npm-global";
const PROJECT_DIR: &str = "project";
const AUBE_DIR: &str = "aube";
const AUBE_CACHE_VERSION: &str = "v1";
const CACHE_DIR: &str = "cache";
const AUBE_LOCKFILE_NAME: &str = "aube-lock.yaml";
const AUBE_LOCK_FORMAT: &str = "aube-v9";
const PROJECT_NPM_BIN_ROOT: &str = ".osdk/npm-bin";
const PROJECT_NPM_BIN_GENERATIONS: &str = "generations";
const PROJECT_NPM_BIN_CURRENT: &str = "current";
const PROJECT_NPM_BIN_MANIFEST: &str = "manifest.json";
const PROJECT_NPM_BIN_BIN_DIR: &str = "bin";
const PROJECT_NPM_BIN_SCHEMA: u32 = 1;
const PROJECT_NPM_BIN_MAX_JSON_BYTES: u64 = 1024 * 1024;
const PROJECT_NPM_LOCKFILE: &str = "osdk.lock";
const PROJECT_NPM_LOCK_MAX_BYTES: u64 = 16 * 1024 * 1024;
const PROJECT_NPM_LAUNCHER_MAX_BYTES: u64 = 256 * 1024;
const NPM_INSTALL_RECEIPT_FILE: &str = ".osdk-npm-receipt.json";
const NPM_INSTALL_RECEIPT_SCHEMA: u32 = 1;
const NPM_INSTALL_RECEIPT_MAX_BYTES: u64 = 64 * 1024;
const NPM_PACKAGE_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
const NPM_NATIVE_LOCK_MAX_BYTES: u64 = 16 * 1024 * 1024;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const OSDK_PROJECT_NPM_CMD_MARKER: &str = ":: osdk-project-npm-bin v1";
static NEXT_PROJECT_NPM_BIN_TEMPORARY: AtomicU64 = AtomicU64::new(0);

pub const LOCKED_NPM_PACKAGE_OPTION: &str = "__osdk_npm_package";
pub const LOCKED_NPM_LOCK_FORMAT_OPTION: &str = "__osdk_npm_lock_format";
pub const LOCKED_NPM_LOCK_SHA256_OPTION: &str = "__osdk_npm_lock_sha256";
pub const LOCKED_NPM_LOCKFILE_OPTION: &str = "__osdk_npm_lockfile";
pub const LOCKED_NPM_NODE_VERSION_OPTION: &str = "__osdk_node_version";

/// One resolved project npm selection recorded in the curated bin manifest.
/// `configured_spec` comes from the already-trusted project configuration;
/// `version` is the exact installed package version that was validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectNpmBinSelection {
    pub backend: String,
    pub configured_spec: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectNpmBinManifest {
    schema: u32,
    generation: String,
    platform: String,
    selections: Vec<ProjectNpmBinSelection>,
    bins: Vec<ProjectNpmBinManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectNpmBinManifestEntry {
    name: String,
    backend: String,
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectNpmBinCurrent {
    schema: u32,
    generation: String,
}

#[derive(Debug, Deserialize)]
struct ProjectNpmLockfile {
    schema: u32,
    #[serde(default)]
    platforms: BTreeMap<String, ProjectNpmPlatformLock>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectNpmPlatformLock {
    #[serde(default)]
    tools: BTreeMap<String, ProjectNpmLockedTool>,
}

#[derive(Debug, Deserialize)]
struct ProjectNpmLockedTool {
    request: String,
    version: String,
    #[serde(default)]
    npm: Option<ProjectNpmLockedMetadata>,
}

#[derive(Debug, Deserialize)]
struct ProjectNpmLockedMetadata {
    package: String,
    scope: String,
}

#[derive(Debug)]
struct ValidatedProjectNpmBin {
    name: String,
    project_relative_target: PathBuf,
}

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

/// Backend-specific evidence observed after an npm install. None of these
/// fields select the physical install root unless the corresponding digest was
/// already present in the request and therefore included in `InstallIdentity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmInstallReceipt {
    pub schema: u32,
    pub provider: String,
    pub package: String,
    pub installer: String,
    pub node_version: String,
    pub build_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_integrity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_lock_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_lock_sha256: Option<String>,
}

struct UnlockedNpmResolution {
    sources: Vec<Source>,
    root_checksum: pipeline::Checksum,
    root_urls: Vec<String>,
}

pub fn npm_receipt_path(install_root: &Path) -> PathBuf {
    install_root.join(NPM_INSTALL_RECEIPT_FILE)
}

fn write_npm_receipt(install_root: &Path, receipt: &NpmInstallReceipt) -> Result<()> {
    let path = npm_receipt_path(install_root);
    let bytes = serde_json::to_vec_pretty(receipt)?;
    std::fs::write(&path, bytes).map_err(|error| Error::io(path, error))
}

pub fn load_npm_receipt(install_root: &Path) -> Result<NpmInstallReceipt> {
    let path = npm_receipt_path(install_root);
    let bytes = crate::inventory::read_stable_regular_file(&path, NPM_INSTALL_RECEIPT_MAX_BYTES)
        .map_err(|error| Error::io(&path, error))?;
    let receipt: NpmInstallReceipt = serde_json::from_slice(&bytes)?;
    if receipt.schema != NPM_INSTALL_RECEIPT_SCHEMA {
        return Err(Error::config(format!(
            "unsupported npm install receipt schema `{}`",
            receipt.schema
        )));
    }
    Ok(receipt)
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
        let id = crate::inventory::canonical_dynamic_id(id).ok()?;
        let package = id.strip_prefix("npm:")?.to_string();
        Some(Self { id, package })
    }

    /// Complete identity for an osdk-owned npm install. Project-managed npm
    /// packages deliberately never call this path or write this manifest.
    pub fn install_identity(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        scope: ToolScope,
    ) -> Result<InstallIdentity> {
        let node_version = tv
            .options
            .get(LOCKED_NPM_NODE_VERSION_OPTION)
            .cloned()
            .or_else(|| selected_node_version(ctx))
            .ok_or_else(|| Error::other(crate::t!("err.npm_dynamic_managed_node_required")))?;
        let dependencies = vec![InstallDependency {
            kind: InstallDependencyKind::Runtime,
            id: "node".into(),
            version: node_version,
            identity: None,
        }];
        let mut materials = BTreeMap::new();
        if let Some(digest) = tv.options.get(LOCKED_NPM_LOCK_SHA256_OPTION) {
            materials.insert("lock-graph-sha256".into(), digest.to_ascii_lowercase());
        }
        // Compact schema-3 native-lock metadata can be reconstructed from an
        // install after publication, so it is verification evidence rather than
        // a locator input. Including it would make the install root change after
        // the first successful install. The receipt binds the digest back to the
        // observed native lock on every reuse/selection path. The legacy frozen
        // graph digest is different: it exists only as a pre-install replay input.
        InstallIdentity::new(
            self.id(),
            &tv.version,
            ctx.platform.to_string(),
            match scope {
                ToolScope::Project => InstallScope::Isolated,
                ToolScope::Global => InstallScope::Global,
            },
            &tv.options,
            dependencies,
            materials,
        )
    }

    pub fn install_locator(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        scope: ToolScope,
    ) -> Result<crate::dirs::InstallLocator> {
        crate::dirs::InstallLocator::new(&ctx.dirs, self.install_identity(ctx, tv, scope)?)
    }

    pub fn isolated_install_root_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        Ok(self
            .install_locator(ctx, tv, ToolScope::Project)?
            .install_root()
            .to_path_buf())
    }

    pub fn global_install_root_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        Ok(self
            .install_locator(ctx, tv, ToolScope::Global)?
            .install_root()
            .to_path_buf())
    }

    /// Version-only npm roots are legacy detection locations. They never
    /// authorize reuse or execution.
    pub fn legacy_isolated_install_root(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs.install_path(self.id(), version)
    }

    pub fn legacy_global_install_root_path(&self, ctx: &Ctx, version: &str) -> PathBuf {
        ctx.dirs.install_path(
            &format!("{GLOBAL_INSTALL_NAMESPACE}:{}", self.package),
            version,
        )
    }

    pub fn project_dir_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        Ok(self.isolated_install_root_for(ctx, tv)?.join(PROJECT_DIR))
    }

    pub fn global_project_dir_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        Ok(self.global_install_root_for(ctx, tv)?.join(PROJECT_DIR))
    }

    /// Resolve the install root that should serve the current selection. An
    /// explicit lock scope wins, followed by config provenance. With no scope
    /// signal, the compatibility isolated install wins and global is a
    /// fallback. This avoids deriving scope from a deduplicated version list.
    pub fn selected_install_root(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Option<PathBuf>> {
        let scope = self.selected_scope(ctx, tv)?.unwrap_or(ToolScope::Project);
        let root = match scope {
            ToolScope::Project => self.isolated_install_root_for(ctx, tv)?,
            ToolScope::Global => self.global_install_root_for(ctx, tv)?,
        };
        self.validate_completed_install(ctx, tv, scope, &root)
            .map(|available| available.then_some(root))
    }

    /// Resolve a path for `osdk where` using an already-resolved selection.
    /// Explicit scope metadata on `tv` wins over config provenance; without
    /// either signal, this preserves the historical isolated-first fallback.
    pub fn where_install_root_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Option<PathBuf>> {
        self.selected_install_root(ctx, tv)
    }

    /// List complete versions available to the scope selected by explicit
    /// `ToolVersion` metadata or config provenance. This prevents callers
    /// from resolving a range against the union of project and global roots.
    pub fn list_installed_for(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(self
            .list_installed_identities_for(ctx, tv)?
            .into_iter()
            .map(|candidate| candidate.version)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    /// Return complete installed npm selections without collapsing distinct
    /// runtime/material identities that happen to share the same package
    /// version. Lifecycle callers must retain the selected `ToolVersion` so a
    /// later active Node change cannot redirect `where` or `uninstall`.
    pub fn list_installed_identities_for(
        &self,
        ctx: &Ctx,
        selector: &ToolVersion,
    ) -> Result<Vec<ToolVersion>> {
        match self.selected_scope(ctx, selector)? {
            Some(ToolScope::Project) => {
                self.complete_identities_under(ctx, selector, ToolScope::Project)
            }
            Some(ToolScope::Global) => {
                self.complete_identities_under(ctx, selector, ToolScope::Global)
            }
            None => {
                let mut candidates =
                    self.complete_identities_under(ctx, selector, ToolScope::Project)?;
                candidates.extend(self.complete_identities_under(
                    ctx,
                    selector,
                    ToolScope::Global,
                )?);
                candidates.sort_by(|left, right| {
                    (&left.version, &left.options).cmp(&(&right.version, &right.options))
                });
                candidates.dedup();
                Ok(candidates)
            }
        }
    }

    /// Explicitly scope an installed-version query without fabricating lock
    /// metadata. Primarily useful for commands such as `where --global`.
    pub fn list_installed_for_scope(&self, ctx: &Ctx, scope: ToolScope) -> Result<Vec<String>> {
        self.list_manifest_versions(ctx, Some(scope))
    }

    /// Compatibility query for callers that only have an exact version. It
    /// intentionally has no explicit scope signal, but can still recover one
    /// from config provenance. New callers should retain a `ToolVersion` and
    /// use [`Self::where_install_root_for`].
    pub fn where_install_root(&self, ctx: &Ctx, version: &str) -> Result<Option<PathBuf>> {
        let mut tv = ToolVersion::new(self.id(), version);
        if let Some(request) = crate::shim::dynamic_request_from_config(ctx, self.id()) {
            tv.options = request.options;
        }
        self.where_install_root_for(ctx, &tv)
    }

    /// Locate a global install, including the pre-isolation layout where a
    /// global manifest occupied the compatibility isolated root.
    pub fn existing_global_install_root(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<PathBuf>> {
        let global = self.global_install_root_for(ctx, tv)?;
        if global.join(".osdk-complete").is_file()
            && self.validate_completed_install(ctx, tv, ToolScope::Global, &global)?
        {
            return Ok(Some(global));
        }
        Ok(None)
    }

    /// Locate a complete pre-isolation global install occupying the legacy
    /// isolated root. Callers can use this as a compatibility source while
    /// reinstalling into the canonical global root. The directory must not be
    /// renamed directly because older Aube installs may contain absolute bin
    /// symlinks rooted at the old location.
    pub fn legacy_global_install_root(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<PathBuf>> {
        let _ = (ctx, tv);
        Ok(None)
    }

    /// Remove only a pre-isolation global root after a replacement global
    /// install has been fully published. Isolated installs are never eligible.
    pub fn remove_legacy_global_install(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<bool> {
        let _ = (ctx, tv);
        Ok(false)
    }

    /// Remove one global npm install without touching the isolated sibling.
    /// The legacy location is eligible only when its manifest explicitly says
    /// `scope = global`.
    pub fn uninstall_global(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<bool> {
        let canonical = self.global_install_root_for(ctx, tv)?;
        let root = if canonical.join(".osdk-complete").is_file()
            && self.validate_completed_install(ctx, tv, ToolScope::Global, &canonical)?
        {
            Some(canonical)
        } else {
            self.legacy_global_install_root(ctx, tv)?
        };
        let Some(root) = root else {
            return Ok(false);
        };
        let _ = crate::inventory::remove_manifest(&root);
        std::fs::remove_dir_all(&root).map_err(|error| Error::io(&root, error))?;
        Ok(true)
    }

    fn selected_scope(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Option<ToolScope>> {
        if let Some(scope) = tv.options.get(LOCKED_NPM_SCOPE_OPTION) {
            return scope.parse().map(Some);
        }
        let mut matched_project = false;
        let mut matched_global = false;
        for (key, value) in &ctx.config.tools {
            let matches = key == self.id()
                || ToolRequest::parse(value).is_ok_and(|request| request.backend == self.id);
            if !matches {
                continue;
            }
            match ctx.config.tool_origins.get(key) {
                Some(ToolConfigOrigin::ProjectConfig(_) | ToolConfigOrigin::ToolVersions(_)) => {
                    matched_project = true;
                }
                Some(ToolConfigOrigin::GlobalConfig(_)) => matched_global = true,
                None if ctx.config.global_tool_configs.contains_key(key) => matched_global = true,
                None => {}
            }
        }
        Ok(if matched_project {
            Some(ToolScope::Project)
        } else if matched_global {
            Some(ToolScope::Global)
        } else {
            None
        })
    }

    /// Validate a complete npm install before reuse, selection, recovery, or
    /// execution. The identity-addressed path is necessary but not sufficient:
    /// the receipt, package graph/native lock, published bins, and exact Node
    /// runtime dependency must all still agree with the manifest.
    pub fn validate_completed_install(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        scope: ToolScope,
        root: &Path,
    ) -> Result<bool> {
        if !is_regular_file(&root.join(".osdk-complete")) {
            return Ok(false);
        }
        let Some(manifest) = self.manifest_at(ctx, root, tv)? else {
            return Ok(false);
        };
        let expected_scope = match scope {
            ToolScope::Project => InstallScope::Isolated,
            ToolScope::Global => InstallScope::Global,
        };
        if manifest.identity.scope != expected_scope
            || !manifest.matches_identity(&self.install_identity(ctx, tv, scope)?)
        {
            return Ok(false);
        }
        let Some(node_version) = exact_node_dependency(&manifest.identity) else {
            return Ok(false);
        };
        if !managed_node_is_runnable(ctx, node_version)? {
            return Ok(false);
        }
        let receipt = match load_npm_receipt(root) {
            Ok(receipt) => receipt,
            Err(_) => return Ok(false),
        };
        if !receipt_matches_identity(self, tv, scope, &manifest.identity, &receipt)?
            || !manifest_bins_are_confined(root, &manifest)?
        {
            return Ok(false);
        }
        match scope {
            ToolScope::Project => validate_isolated_install_evidence(
                root,
                &self.package,
                &tv.version,
                &Self::build_policy(tv)?,
                &receipt,
                self.locked_graph(tv)?.as_ref(),
            ),
            ToolScope::Global => {
                validate_global_install_evidence(root, &self.package, &tv.version, &receipt)
            }
        }
    }

    fn complete_identities_under(
        &self,
        ctx: &Ctx,
        selector: &ToolVersion,
        scope: ToolScope,
    ) -> Result<Vec<ToolVersion>> {
        let base = match scope {
            ToolScope::Project => ctx
                .dirs
                .installs
                .join(crate::dirs::sanitize_tool_id(self.id())),
            ToolScope::Global => ctx
                .dirs
                .installs
                .join(crate::dirs::sanitize_tool_id(&format!(
                    "{GLOBAL_INSTALL_NAMESPACE}:{}",
                    self.package
                ))),
        };
        let mut candidates = Vec::new();
        if !base.exists() {
            return Ok(candidates);
        }
        let report = crate::inventory::scan_installs(
            &ctx.dirs.installs,
            &crate::inventory::ScanOptions::default(),
        )?;
        let expected_options =
            crate::backend::dynamic::identity_options(self.id(), &selector.options)?;
        for install in report.installs {
            if !install.install_root.starts_with(&base)
                || install.manifest.identity.tool != self.id
                || install.manifest.identity.platform != ctx.platform.to_string()
                || install.manifest.identity.material_options != expected_options
                || install.manifest.identity.scope
                    != match scope {
                        ToolScope::Project => InstallScope::Isolated,
                        ToolScope::Global => InstallScope::Global,
                    }
            {
                continue;
            }
            let mut candidate = if selector
                .options
                .contains_key(LOCKED_NPM_NODE_VERSION_OPTION)
            {
                selector.clone()
            } else {
                let Some(candidate) = inventory_validation_candidate(&install.manifest) else {
                    continue;
                };
                candidate
            };
            candidate.version = install.manifest.identity.version.clone();
            candidate
                .options
                .insert(LOCKED_NPM_SCOPE_OPTION.into(), scope.as_str().into());
            if self.validate_completed_install(ctx, &candidate, scope, &install.install_root)? {
                candidates.push(candidate);
            }
        }
        candidates.sort_by(|left, right| {
            (&left.version, &left.options).cmp(&(&right.version, &right.options))
        });
        candidates.dedup();
        Ok(candidates)
    }

    fn list_manifest_versions(&self, ctx: &Ctx, scope: Option<ToolScope>) -> Result<Vec<String>> {
        let report = crate::inventory::scan_installs(
            &ctx.dirs.installs,
            &crate::inventory::ScanOptions::default(),
        )?;
        let expected_scope = scope.map(|scope| match scope {
            ToolScope::Project => InstallScope::Isolated,
            ToolScope::Global => InstallScope::Global,
        });
        let mut versions = BTreeSet::new();
        for install in report.installs {
            if install.manifest.identity.tool != self.id
                || install.manifest.identity.platform != ctx.platform.to_string()
                || !expected_scope.is_none_or(|scope| install.manifest.identity.scope == scope)
            {
                continue;
            }
            let scope = match install.manifest.identity.scope {
                InstallScope::Isolated => ToolScope::Project,
                InstallScope::Global => ToolScope::Global,
                InstallScope::ProjectManaged => continue,
            };
            let Some(candidate) = inventory_validation_candidate(&install.manifest) else {
                continue;
            };
            if self.validate_completed_install(ctx, &candidate, scope, &install.install_root)? {
                versions.insert(candidate.version);
            }
        }
        Ok(versions.into_iter().collect())
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    /// Validate that this exact package owns at least one runnable bin in a
    /// user project. This checks package.json#bin and then validates the
    /// corresponding launcher target instead of accepting an unrelated entry
    /// that merely happens to exist in node_modules/.bin.
    pub fn validate_project_package_bins(
        &self,
        project_dir: &Path,
        expected_version: &str,
    ) -> Result<Vec<String>> {
        Ok(self
            .validated_project_package_bins(project_dir, expected_version)?
            .into_iter()
            .map(|bin| bin.name)
            .collect())
    }

    fn validated_project_package_bins(
        &self,
        project_dir: &Path,
        expected_version: &str,
    ) -> Result<Vec<ValidatedProjectNpmBin>> {
        let package_dir = package_install_dir(project_dir, &self.package);
        let manifest_path = package_dir.join("package.json");
        let bytes = crate::inventory::read_stable_regular_file(
            &manifest_path,
            NPM_PACKAGE_MANIFEST_MAX_BYTES,
        )
        .map_err(|error| Error::io(&manifest_path, error))?;
        let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
        if manifest.get("name").and_then(serde_json::Value::as_str) != Some(&self.package)
            || manifest.get("version").and_then(serde_json::Value::as_str) != Some(expected_version)
        {
            return Err(Error::other(format!(
                "installed project package identity mismatch: expected {}@{}",
                self.package, expected_version
            )));
        }
        let entries = package_bin_entries(&manifest, &self.package)?;
        let bin_dir = project_dir.join("node_modules/.bin");
        let canonical_project =
            dunce::canonicalize(project_dir).map_err(|error| Error::io(project_dir, error))?;
        let canonical_package =
            dunce::canonicalize(&package_dir).map_err(|error| Error::io(&package_dir, error))?;
        if !canonical_package.starts_with(&canonical_project) {
            return Err(Error::other(format!(
                "installed npm package {} resolves outside project {}",
                self.package,
                canonical_project.display()
            )));
        }
        let mut validated = Vec::with_capacity(entries.len());
        for (name, relative_target) in &entries {
            if relative_target.is_absolute()
                || relative_target.as_os_str().is_empty()
                || relative_target.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(Error::other(format!(
                    "npm package {} declares unsafe bin path {}",
                    self.package,
                    relative_target.display()
                )));
            }
            let declared_path = package_dir.join(relative_target);
            let declared_target = dunce::canonicalize(&declared_path)
                .map_err(|error| Error::io(&declared_path, error))?;
            if !declared_target.is_file() || !declared_target.starts_with(&canonical_package) {
                return Err(Error::other(crate::t!(
                    "err.npm_bin_outside_install_root",
                    name = name,
                    path = canonical_package.display()
                )));
            }
            let launcher = global_bin_entry(&bin_dir, name).ok_or_else(|| {
                Error::other(crate::t!(
                    "err.npm_bin_target_unresolved",
                    name = name,
                    path = bin_dir.display()
                ))
            })?;
            #[cfg(not(windows))]
            validate_unix_project_launcher(&launcher, name, &self.package, &declared_target)?;
            #[cfg(windows)]
            validate_windows_project_launcher(
                &bin_dir,
                &launcher,
                name,
                &self.package,
                &declared_target,
            )?;
            let project_relative_target = declared_target
                .strip_prefix(&canonical_project)
                .map_err(|_| {
                    Error::other(format!(
                        "npm package {} bin `{name}` resolves outside project {}",
                        self.package,
                        canonical_project.display()
                    ))
                })?
                .to_path_buf();
            validated.push(ValidatedProjectNpmBin {
                name: name.clone(),
                project_relative_target,
            });
        }
        Ok(validated)
    }

    pub fn aube_cache_dir(ctx: &Ctx) -> PathBuf {
        ctx.dirs
            .cache
            .join(AUBE_DIR)
            .join(AUBE_CACHE_VERSION)
            .join(CACHE_DIR)
    }

    pub fn aube_store_dir(ctx: &Ctx) -> PathBuf {
        ctx.dirs.store.join(AUBE_DIR)
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

    pub fn write_empty_project_manifest(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        Self::write_project_manifest(
            &self.project_dir_for(ctx, tv)?,
            None,
            &Self::build_policy(tv)?,
        )
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
        // Schema 3 intentionally carries package/installer/scope metadata
        // without the old frozen graph payload. Package identity alone must
        // therefore not opt into the legacy graph reader.
        if values[1..].iter().all(|value| value.is_none()) {
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

    fn validate_compact_lock_metadata(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        // Scope alone is also used as an in-process routing hint by explicit
        // compatibility commands; it is not compact lock metadata.
        let metadata_present = tv.options.contains_key(LOCKED_NPM_PACKAGE_OPTION)
            || tv
                .options
                .contains_key(crate::npm_tools::LOCKED_NPM_INSTALLER_OPTION)
            || tv
                .options
                .contains_key(crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_KIND_OPTION)
            || tv
                .options
                .contains_key(crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION)
            || tv
                .options
                .contains_key(crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION);
        if !metadata_present || tv.options.contains_key(LOCKED_NPM_LOCKFILE_OPTION) {
            return Ok(());
        }
        let package = tv
            .options
            .get(LOCKED_NPM_PACKAGE_OPTION)
            .ok_or_else(|| Error::other("compact npm lock metadata is missing package identity"))?;
        if package != &self.package {
            return Err(Error::other(format!(
                "compact npm lock package mismatch: expected {}, found {package}",
                self.package
            )));
        }
        let scope = tv.options.get(crate::npm_tools::LOCKED_NPM_SCOPE_OPTION);
        let installer = tv
            .options
            .get(crate::npm_tools::LOCKED_NPM_INSTALLER_OPTION)
            .ok_or_else(|| Error::other("compact npm lock metadata is missing installer"))?;
        let root = match scope.map(String::as_str) {
            Some("global") => self
                .existing_global_install_root(ctx, tv)?
                .unwrap_or(self.global_install_root_for(ctx, tv)?),
            Some("project") => return Ok(()),
            _ => return Err(Error::other("compact npm lock metadata has invalid scope")),
        };
        let manifest = DynamicToolManifest::load(&root)?;
        let receipt = load_npm_receipt(&root)?;
        if manifest.identity.scope != InstallScope::Global || receipt.installer != *installer {
            return Err(Error::other(format!(
                "global npm install metadata does not match the lock for {}@{}",
                self.id, tv.version
            )));
        }
        let keys = [
            crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
            crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION,
            crate::npm_tools::LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION,
        ];
        let present = keys
            .iter()
            .map(|key| tv.options.get(*key))
            .collect::<Vec<_>>();
        if present.iter().all(|value| value.is_none()) {
            return Ok(());
        }
        if present.iter().any(|value| value.is_none()) {
            return Err(Error::other(
                "compact npm native-lock metadata is incomplete",
            ));
        }
        let expected_format = present[1].expect("validated above");
        let expected_digest = present[2].expect("validated above");
        if receipt.native_lock_format.as_ref() != Some(expected_format)
            || receipt.native_lock_sha256.as_ref() != Some(expected_digest)
        {
            return Err(Error::other(format!(
                "global npm native lock metadata does not match the lock for {}@{}",
                self.id, tv.version
            )));
        }
        Ok(())
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
        let project_dir = self.project_dir_for(ctx, tv)?;
        let build_policy = Self::build_policy(tv)?;
        Self::write_project_manifest(
            &project_dir,
            Some((&self.package, &tv.version)),
            &build_policy,
        )?;
        Self::write_project_npmrc(&project_dir, Some(&source.download_url))?;
        aube_host::prepare_lock_graph(EmbeddedLockGraphRequest {
            project_dir: &project_dir,
            cache_dir: Self::aube_cache_dir(ctx),
            store_dir: Self::aube_store_dir(ctx),
            node_bin_dir: managed_node(ctx, tv)?.0,
            registry: Some(source.download_url.clone()),
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

    #[allow(clippy::too_many_arguments)]
    fn build_manifest(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        install_root: &Path,
        bin_dir: &Path,
        node_version: &str,
        build_policy: &BuildPolicy,
        graph_identity: &NpmGraphIdentity,
    ) -> Result<DynamicToolManifest> {
        let identity = self.install_identity(ctx, tv, ToolScope::Project)?;
        let mut manifest = DynamicToolManifest::from_identity(identity)?;
        manifest.bins = discover_bins(install_root, bin_dir)?;
        write_npm_receipt(
            install_root,
            &NpmInstallReceipt {
                schema: NPM_INSTALL_RECEIPT_SCHEMA,
                provider: PROVIDER.into(),
                package: self.package.clone(),
                installer: "aube".into(),
                node_version: node_version.into(),
                build_policy: build_policy.identity(),
                graph_sha256: Some(graph_identity.sha256.clone()),
                root_integrity: Some(graph_identity.root_integrity.clone()),
                root_source: Some(graph_identity.root_source.clone()),
                native_lock_format: None,
                native_lock_sha256: None,
            },
        )?;
        manifest.normalize()
    }

    /// Validate a synthetic-project install produced by an explicitly managed
    /// npm-compatible installer and publish the common dynamic inventory.
    ///
    /// The caller owns the package-manager invocation and its native lock. This
    /// method deliberately reuses the same confined bin-target validation as
    /// embedded Aube installs, so native npm/pnpm cannot publish paths outside
    /// the osdk install root.
    pub fn finalize_global_install(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        bin_dir: &Path,
        node_version: &str,
        installer: &str,
        native_lock: Option<(&str, &str)>,
    ) -> Result<DynamicToolManifest> {
        let install_root = self.global_install_root_for(ctx, tv)?;
        self.finalize_global_install_at(
            ctx,
            tv,
            &install_root,
            bin_dir,
            node_version,
            installer,
            native_lock,
        )
    }

    /// Validate and finalize a global install assembled in an arbitrary root.
    /// This lets callers publish through a staging directory: package/bin
    /// validation, inventory creation, and the completion marker all happen
    /// before the staging directory is swapped into its final path.
    #[allow(clippy::too_many_arguments)]
    pub fn finalize_global_install_at(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        install_root: &Path,
        bin_dir: &Path,
        node_version: &str,
        installer: &str,
        native_lock: Option<(&str, &str)>,
    ) -> Result<DynamicToolManifest> {
        let package_dir = if installer == "npm" {
            #[cfg(windows)]
            let modules = install_root.join("node_modules");
            #[cfg(not(windows))]
            let modules = install_root.join("lib/node_modules");
            modules.join(&self.package)
        } else if installer == "aube" {
            package_install_dir(&install_root.join(PROJECT_DIR), &self.package)
        } else {
            install_root.to_path_buf()
        };
        if !package_dir.is_dir() {
            return Err(Error::other(crate::t!(
                "err.npm_install_package_missing",
                package = self.package,
                path = install_root.display()
            )));
        }
        let identity = self.install_identity(ctx, tv, ToolScope::Global)?;
        let mut manifest = DynamicToolManifest::from_identity(identity)?;
        manifest.bins = discover_global_bins(install_root, bin_dir)?;
        write_npm_receipt(
            install_root,
            &NpmInstallReceipt {
                schema: NPM_INSTALL_RECEIPT_SCHEMA,
                provider: PROVIDER.into(),
                package: self.package.clone(),
                installer: installer.into(),
                node_version: node_version.into(),
                build_policy: Self::build_policy(tv)?.identity(),
                graph_sha256: None,
                root_integrity: None,
                root_source: None,
                native_lock_format: native_lock.map(|value| value.0.to_string()),
                native_lock_sha256: native_lock.map(|value| value.1.to_string()),
            },
        )?;
        let manifest = manifest.normalize()?;
        manifest.write_atomic(install_root)?;
        std::fs::write(install_root.join(".osdk-complete"), b"")
            .map_err(|error| Error::io(install_root.join(".osdk-complete"), error))?;
        Ok(manifest)
    }

    fn manifest_at(
        &self,
        ctx: &Ctx,
        install_root: &Path,
        tv: &ToolVersion,
    ) -> Result<Option<DynamicToolManifest>> {
        let path = DynamicToolManifest::manifest_path(install_root);
        if !path.is_file() {
            return Ok(None);
        }
        let manifest = DynamicToolManifest::load(install_root)?;
        let scope = match manifest.identity.scope {
            InstallScope::Isolated => ToolScope::Project,
            InstallScope::Global => ToolScope::Global,
            InstallScope::ProjectManaged => return Ok(None),
        };
        if !manifest.matches_identity(&self.install_identity(ctx, tv, scope)?) {
            return Err(Error::other(format!(
                "npm inventory identity mismatch at {}",
                path.display()
            )));
        }
        Ok(Some(manifest))
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
        crate::backend::dynamic::validate_options(self.id(), &req.options)?;
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
        self.validate_compact_lock_metadata(ctx, tv)?;
        let locator = self.install_locator(ctx, tv, ToolScope::Project)?;
        let install_root = locator.install_root().to_path_buf();
        let project_dir = install_root.join(PROJECT_DIR);
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
            locator.identity(),
            &self.package,
            locked_graph.as_ref(),
            &build_policy,
        )? {
            return Ok(());
        }
        let _lock = crate::lock::FileLock::acquire(locator.lock_path())?;
        if install_matches(
            &install_root,
            locator.identity(),
            &self.package,
            locked_graph.as_ref(),
            &build_policy,
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
                cache_dir: Self::aube_cache_dir(ctx),
                store_dir: Self::aube_store_dir(ctx),
                node_bin_dir,
                registry: None,
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
                    cache_dir: Self::aube_cache_dir(ctx),
                    store_dir: Self::aube_store_dir(ctx),
                    node_bin_dir: node_bin_dir.clone(),
                    registry: Some(source.download_url.clone()),
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
            &install_root,
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
        let install_root = self.isolated_install_root_for(ctx, tv)?;
        if !install_root.exists() {
            return Ok(());
        }
        let _ = crate::inventory::remove_manifest(&install_root);
        std::fs::remove_dir_all(&install_root).map_err(|error| Error::io(&install_root, error))
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        self.list_manifest_versions(ctx, None)
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let Some(install_root) = self.selected_install_root(ctx, tv)? else {
            return Ok(Vec::new());
        };
        if let Some(manifest) = self.manifest_at(ctx, &install_root, tv)? {
            return Ok(manifest
                .bins
                .iter()
                .filter_map(|bin| install_root.join(&bin.path).parent().map(Path::to_path_buf))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect());
        }
        Ok(vec![install_root
            .join(PROJECT_DIR)
            .join("node_modules/.bin")])
    }

    fn exec_env(&self, ctx: &Ctx, _tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let mut env = crate::cache::manager_exec_env(
            &ctx.dirs.cache,
            &[
                ("npm_config_cache", "npm"),
                ("npm_config_store_dir", "npm-store"),
            ],
        );
        env.insert(
            "npm_config_cache".into(),
            Self::aube_cache_dir(ctx).display().to_string(),
        );
        env.insert(
            "npm_config_store_dir".into(),
            Self::aube_store_dir(ctx).display().to_string(),
        );
        Ok(env)
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        if let Some(install_root) = self.selected_install_root(ctx, tv)? {
            if let Some(manifest) = self.manifest_at(ctx, &install_root, tv)? {
                return Ok(manifest.bins.into_iter().map(|bin| bin.name).collect());
            }
        }
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

    fn dynamic_install_identity(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<InstallIdentity>> {
        let scope = self.selected_scope(ctx, tv)?.unwrap_or(ToolScope::Project);
        self.install_identity(ctx, tv, scope).map(Some)
    }

    fn validate_dynamic_install(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        install_root: &Path,
        identity: &InstallIdentity,
    ) -> Result<bool> {
        let Some(node_version) = exact_node_dependency(identity) else {
            return Ok(false);
        };
        if !managed_node_is_runnable(ctx, node_version)? {
            return Err(Error::other(crate::t!(
                "err.shim_managed_node_required",
                tool = self.id()
            )));
        }
        let scope = match identity.scope {
            InstallScope::Isolated => ToolScope::Project,
            InstallScope::Global => ToolScope::Global,
            InstallScope::ProjectManaged => return Ok(false),
        };
        self.validate_completed_install(ctx, tv, scope, install_root)
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

fn inventory_validation_candidate(manifest: &DynamicToolManifest) -> Option<ToolVersion> {
    let mut candidate = ToolVersion::new(&manifest.identity.tool, &manifest.identity.version);
    candidate.options = manifest.identity.material_options.clone();
    candidate.options.insert(
        LOCKED_NPM_NODE_VERSION_OPTION.into(),
        exact_node_dependency(&manifest.identity)?.to_string(),
    );
    if let Some(digest) = manifest.identity.materials.get("lock-graph-sha256") {
        candidate
            .options
            .insert(LOCKED_NPM_LOCK_SHA256_OPTION.into(), digest.clone());
    }
    Some(candidate)
}

pub(crate) fn exact_node_dependency(identity: &InstallIdentity) -> Option<&str> {
    let mut dependencies = identity.dependencies.iter().filter(|dependency| {
        dependency.kind == InstallDependencyKind::Runtime && dependency.id == "node"
    });
    let dependency = dependencies.next()?;
    dependencies
        .next()
        .is_none()
        .then_some(dependency.version.as_str())
}

pub(crate) fn managed_node_is_runnable(ctx: &Ctx, version: &str) -> Result<bool> {
    let node = crate::backend::node::NodeBackend;
    let tool = ToolVersion::new("node", version);
    Ok(node.bin_paths(ctx, &tool)?.into_iter().any(|directory| {
        is_runnable_file(&directory.join(node_executable_name()))
            && is_regular_file(
                &ctx.dirs
                    .install_path("node", version)
                    .join(".osdk-complete"),
            )
    }))
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

#[cfg(unix)]
fn is_runnable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    is_regular_file(path)
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_runnable_file(path: &Path) -> bool {
    is_regular_file(path)
}

fn receipt_matches_identity(
    backend: &NpmPackageBackend,
    tv: &ToolVersion,
    scope: ToolScope,
    identity: &InstallIdentity,
    receipt: &NpmInstallReceipt,
) -> Result<bool> {
    let Some(node_version) = exact_node_dependency(identity) else {
        return Ok(false);
    };
    let requested_installer = identity
        .material_options
        .get("installer")
        .map(String::as_str);
    let locked_installer = tv
        .options
        .get(LOCKED_NPM_INSTALLER_OPTION)
        .map(String::as_str);
    if requested_installer.is_some()
        && locked_installer.is_some()
        && requested_installer != locked_installer
    {
        return Ok(false);
    }
    let expected_installer = locked_installer.or(requested_installer).unwrap_or("aube");
    if receipt.provider != PROVIDER
        || receipt.package != backend.package
        || receipt.node_version != node_version
        || receipt.installer != expected_installer
        || receipt.build_policy != NpmPackageBackend::build_policy(tv)?.identity()
    {
        return Ok(false);
    }

    let native = [
        tv.options.get(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION),
        tv.options.get(LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION),
        tv.options.get(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION),
    ];
    if native.iter().any(|value| value.is_some()) {
        if native.iter().any(|value| value.is_none()) {
            return Ok(false);
        }
        if scope != ToolScope::Global
            || receipt.native_lock_format.as_ref() != native[1]
            || receipt.native_lock_sha256.as_ref() != native[2]
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn manifest_bins_are_confined(install_root: &Path, manifest: &DynamicToolManifest) -> Result<bool> {
    if manifest.bins.is_empty() {
        return Ok(false);
    }
    let canonical_root =
        dunce::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    for bin in &manifest.bins {
        let path = install_root.join(&bin.path);
        let Ok(canonical) = dunce::canonicalize(&path) else {
            return Ok(false);
        };
        if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_isolated_install_evidence(
    install_root: &Path,
    package: &str,
    version: &str,
    build_policy: &BuildPolicy,
    receipt: &NpmInstallReceipt,
    locked_graph: Option<&LockedNpmGraph<'_>>,
) -> Result<bool> {
    if receipt.installer != "aube"
        || receipt.native_lock_format.is_some()
        || receipt.native_lock_sha256.is_some()
    {
        return Ok(false);
    }
    let project_dir = install_root.join(PROJECT_DIR);
    if validate_project_manifest(&project_dir, package, version, build_policy).is_err()
        || NpmPackageBackend::validate_install_layout(&project_dir, package).is_err()
    {
        return Ok(false);
    }
    let graph = match npm_graph_identity(&project_dir, package, version) {
        Ok(graph) => graph,
        Err(_) => return Ok(false),
    };
    if receipt.graph_sha256.as_deref() != Some(graph.sha256.as_str())
        || receipt.root_integrity.as_deref() != Some(graph.root_integrity.as_str())
        || receipt.root_source.as_deref() != Some(graph.root_source.as_str())
    {
        return Ok(false);
    }
    if let Some(locked_graph) = locked_graph {
        let expected = pipeline::verify::hash_bytes(
            locked_graph.lockfile.as_bytes(),
            pipeline::HashAlgo::Sha256,
        );
        if graph.sha256 != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_global_install_evidence(
    install_root: &Path,
    package: &str,
    version: &str,
    receipt: &NpmInstallReceipt,
) -> Result<bool> {
    if receipt.graph_sha256.is_some()
        || receipt.root_integrity.is_some()
        || receipt.root_source.is_some()
    {
        return Ok(false);
    }
    let installer = match receipt.installer.parse::<NpmInstaller>() {
        Ok(NpmInstaller::Auto) | Err(_) => return Ok(false),
        Ok(installer) => installer,
    };
    let package_manifest = match global_package_manifest_path(install_root, package, installer) {
        Some(path) => path,
        None => return Ok(false),
    };
    if !package_manifest_matches(&package_manifest, package, version)? {
        return Ok(false);
    }

    let native_lock = global_native_lock_path(install_root, installer);
    match (
        receipt.native_lock_format.as_deref(),
        receipt.native_lock_sha256.as_deref(),
        native_lock,
    ) {
        (None, None, None) => Ok(installer == NpmInstaller::Npm),
        (Some(format), Some(digest), Some(path)) => {
            let actual =
                crate::inventory::read_stable_regular_file(&path, NPM_NATIVE_LOCK_MAX_BYTES)
                    .map(|bytes| pipeline::verify::hash_bytes(&bytes, pipeline::HashAlgo::Sha256));
            Ok(native_lock_format_matches(installer, format, &path)
                && actual.is_ok_and(|actual| actual == digest))
        }
        _ => Ok(false),
    }
}

fn global_package_manifest_path(
    install_root: &Path,
    package: &str,
    installer: NpmInstaller,
) -> Option<PathBuf> {
    match installer {
        NpmInstaller::Aube => {
            Some(package_install_dir(&install_root.join(PROJECT_DIR), package).join("package.json"))
        }
        NpmInstaller::Npm => {
            #[cfg(windows)]
            let modules = install_root.join("node_modules");
            #[cfg(not(windows))]
            let modules = install_root.join("lib/node_modules");
            Some(modules.join(package).join("package.json"))
        }
        NpmInstaller::Pnpm => find_unique_descendant(
            &install_root.join("pnpm-global"),
            &format!("/node_modules/{package}/package.json"),
        ),
        NpmInstaller::Auto => None,
    }
}

fn package_manifest_matches(path: &Path, package: &str, version: &str) -> Result<bool> {
    let manifest: serde_json::Value =
        match crate::inventory::read_stable_regular_file(path, NPM_PACKAGE_MANIFEST_MAX_BYTES)
            .map_err(|error| Error::io(path, error))
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(Error::from))
        {
            Ok(manifest) => manifest,
            Err(_) => return Ok(false),
        };
    Ok(
        manifest.get("name").and_then(serde_json::Value::as_str) == Some(package)
            && manifest.get("version").and_then(serde_json::Value::as_str) == Some(version),
    )
}

fn global_native_lock_path(install_root: &Path, installer: NpmInstaller) -> Option<PathBuf> {
    match installer {
        NpmInstaller::Aube => {
            let path = install_root.join(PROJECT_DIR).join(AUBE_LOCKFILE_NAME);
            path.is_file().then_some(path)
        }
        NpmInstaller::Pnpm => {
            find_unique_descendant(&install_root.join("pnpm-global"), "pnpm-lock.yaml")
        }
        NpmInstaller::Npm | NpmInstaller::Auto => None,
    }
}

fn find_unique_descendant(root: &Path, suffix: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    let suffix = suffix.replace('\\', "/");
    let mut matches = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            let portable = path.to_string_lossy().replace('\\', "/");
            portable == suffix.trim_start_matches('/') || portable.ends_with(&suffix)
        });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn native_lock_format_matches(installer: NpmInstaller, format: &str, path: &Path) -> bool {
    let expected_name = match installer {
        NpmInstaller::Aube => "aube-lock.yaml",
        NpmInstaller::Pnpm => "pnpm-lock.yaml",
        NpmInstaller::Npm | NpmInstaller::Auto => return false,
    };
    if path.file_name().and_then(std::ffi::OsStr::to_str) != Some(expected_name) {
        return false;
    }
    match installer {
        NpmInstaller::Aube => format == AUBE_LOCK_FORMAT,
        NpmInstaller::Pnpm => format == "pnpm-v9",
        NpmInstaller::Npm | NpmInstaller::Auto => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn install_matches(
    install_root: &Path,
    expected_identity: &InstallIdentity,
    expected_package: &str,
    locked_graph: Option<&LockedNpmGraph<'_>>,
    build_policy: &BuildPolicy,
) -> Result<bool> {
    if !is_regular_file(&install_root.join(".osdk-complete")) {
        return Ok(false);
    }
    let manifest = match DynamicToolManifest::load(install_root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let node_version = exact_node_dependency(expected_identity)
        .ok_or_else(|| Error::other("npm install identity is missing its Node dependency"))?;
    let receipt = match load_npm_receipt(install_root) {
        Ok(receipt) => receipt,
        Err(_) => return Ok(false),
    };
    if !manifest.matches_identity(expected_identity)
        || receipt.provider != PROVIDER
        || receipt.package != expected_package
        || receipt.node_version != node_version
        || receipt.build_policy != build_policy.identity()
        || !manifest_bins_are_confined(install_root, &manifest)?
    {
        return Ok(false);
    }

    let project_dir = install_root.join(PROJECT_DIR);
    if validate_project_manifest(
        &project_dir,
        expected_package,
        &expected_identity.version,
        build_policy,
    )
    .is_err()
        || NpmPackageBackend::validate_install_layout(&project_dir, expected_package).is_err()
    {
        return Ok(false);
    }
    let graph_identity =
        match npm_graph_identity(&project_dir, expected_package, &expected_identity.version) {
            Ok(identity) => identity,
            Err(_) => return Ok(false),
        };
    if receipt.graph_sha256.as_deref() != Some(graph_identity.sha256.as_str())
        || receipt.root_integrity.as_deref() != Some(graph_identity.root_integrity.as_str())
        || receipt.root_source.as_deref() != Some(graph_identity.root_source.as_str())
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
    let bytes = crate::inventory::read_stable_regular_file(&path, NPM_PACKAGE_MANIFEST_MAX_BYTES)
        .map_err(|error| Error::io(&path, error))?;
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
    let bytes = crate::inventory::read_stable_regular_file(&path, NPM_NATIVE_LOCK_MAX_BYTES)
        .map_err(|error| Error::io(&path, error))?;
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

fn package_bin_entries(
    manifest: &serde_json::Value,
    package: &str,
) -> Result<Vec<(String, PathBuf)>> {
    let mut entries = match manifest.get("bin") {
        Some(serde_json::Value::String(path)) if !path.is_empty() => vec![(
            package.rsplit('/').next().unwrap_or(package).to_string(),
            PathBuf::from(path),
        )],
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .map(|(name, path)| {
                let path = path.as_str().filter(|path| !path.is_empty()).ok_or_else(|| {
                    Error::other(format!(
                        "npm package {package} declares a non-string or empty bin target for `{name}`"
                    ))
                })?;
                Ok((name.clone(), PathBuf::from(path)))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) | None => Vec::new(),
    };
    for (name, _) in &entries {
        if !valid_project_bin_name(name) {
            return Err(Error::other(format!(
                "npm package {package} declares unsafe bin name `{name}`"
            )));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    #[cfg(windows)]
    {
        let mut folded = BTreeSet::new();
        if entries
            .iter()
            .any(|(name, _)| !folded.insert(name.to_ascii_lowercase()))
        {
            return Err(Error::other(format!(
                "npm package {package} declares colliding Windows bin names"
            )));
        }
    }
    if entries.is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_dynamic_no_validated_executables",
            tool = format!("npm:{package}")
        )));
    }
    Ok(entries)
}

fn valid_project_bin_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && !is_windows_reserved_component(name)
}

#[cfg(not(windows))]
fn validate_unix_project_launcher(
    launcher: &Path,
    name: &str,
    package: &str,
    declared_target: &Path,
) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(launcher).map_err(|error| Error::io(launcher, error))?;
    if metadata.file_type().is_symlink() {
        let actual = dunce::canonicalize(launcher).map_err(|error| Error::io(launcher, error))?;
        if actual == declared_target {
            return Ok(());
        }
        return Err(Error::other(format!(
            "project launcher `{name}` does not point at the bin declared by {package}"
        )));
    }
    if !metadata.is_file() || metadata.len() > PROJECT_NPM_LAUNCHER_MAX_BYTES {
        return Err(Error::other(format!(
            "project launcher `{name}` for {package} is not a small regular wrapper"
        )));
    }
    let text = std::fs::read_to_string(launcher).map_err(|error| Error::io(launcher, error))?;
    if let Some(relative) = text
        .lines()
        .find_map(|line| line.strip_prefix("# aube-bin-shim v2 target="))
    {
        let relative = PathBuf::from(relative);
        if relative.is_absolute()
            || relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(Error::other(format!(
                "project launcher `{name}` for {package} has an unsafe Aube target"
            )));
        }
        let target = launcher.parent().unwrap_or(Path::new("")).join(relative);
        let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
        return (actual == declared_target).then_some(()).ok_or_else(|| {
            Error::other(format!(
                "project launcher `{name}` does not execute the bin declared by {package}"
            ))
        });
    }
    let target = parse_unix_exec_wrapper(&text, launcher.parent().unwrap_or(Path::new("")))
        .ok_or_else(|| {
            Error::other(format!(
                "project launcher `{name}` for {package} cannot be resolved safely"
            ))
        })?;
    let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
    if actual == declared_target {
        Ok(())
    } else {
        Err(Error::other(format!(
            "project launcher `{name}` does not execute the bin declared by {package}"
        )))
    }
}

#[cfg(not(windows))]
fn parse_unix_exec_wrapper(text: &str, wrapper_dir: &Path) -> Option<PathBuf> {
    let mut command = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("#!") || line.starts_with('#') {
            continue;
        }
        if line.contains([';', '&', '|', '>', '<', '`']) || line.contains("$(") {
            return None;
        }
        let words = shell_words(line)?;
        if words.len() != 4
            || words[0] != "exec"
            || words[1] != "node"
            || !matches!(words[3].as_str(), "$@" | "${@}")
            || command.replace(words[2].clone()).is_some()
        {
            return None;
        }
    }
    let raw = command?;
    let expanded = raw
        .strip_prefix("$basedir/")
        .or_else(|| raw.strip_prefix("${basedir}/"))
        .map(|suffix| wrapper_dir.join(suffix))
        .unwrap_or_else(|| PathBuf::from(raw));
    Some(expanded)
}

#[cfg(not(windows))]
fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Some(expected), actual) if actual == expected => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, actual) if actual.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (Some('\''), actual) => current.push(actual),
            (_, '\\') => current.push(chars.next()?),
            (_, actual) => current.push(actual),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

#[cfg(windows)]
fn validate_windows_project_launcher(
    bin_dir: &Path,
    launcher: &Path,
    name: &str,
    package: &str,
    declared_target: &Path,
) -> Result<()> {
    let extension = launcher
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), Some("cmd") | Some("bat")) {
        return Err(Error::other(format!(
            "project launcher `{name}` for {package} is an opaque Windows executable"
        )));
    }
    for shadow in [bin_dir.join(format!("{name}.exe")), bin_dir.join(name)] {
        if shadow.exists() {
            return Err(Error::other(format!(
                "project launcher `{name}` for {package} has an opaque Windows shadow"
            )));
        }
    }
    let target = parse_windows_project_wrapper(launcher)?;
    let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
    if actual != declared_target {
        return Err(Error::other(format!(
            "project launcher `{name}` does not execute the bin declared by {package}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn parse_windows_project_wrapper(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_file() || metadata.len() > PROJECT_NPM_LAUNCHER_MAX_BYTES {
        return Err(Error::other(format!(
            "project npm wrapper is not a small regular file: {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    if text.contains(OSDK_PROJECT_NPM_CMD_MARKER) {
        let relative = parse_osdk_project_cmd_wrapper(&text).ok_or_else(|| {
            Error::other(format!(
                "invalid osdk project npm wrapper template: {}",
                path.display()
            ))
        })?;
        return path
            .parent()
            .map(|parent| parent.join(relative.replace('\\', "/")))
            .ok_or_else(|| {
                Error::other(format!(
                    "project npm wrapper does not execute a declared target: {}",
                    path.display()
                ))
            });
    }
    let lower = text.to_ascii_lowercase();
    if text.contains('\0') || lower.contains("powershell") || lower.contains("cmd /") {
        return Err(Error::other(format!(
            "project npm wrapper contains an unsupported command: {}",
            path.display()
        )));
    }
    let recognized = is_aube_windows_wrapper(&text) || is_npm_windows_wrapper(&text);
    if !recognized {
        return Err(Error::other(format!(
            "project npm wrapper does not match a recognized safe template: {}",
            path.display()
        )));
    }
    let normalized = text.replace("%dp0%", "%~dp0");
    let marker = "\"%~dp0\\";
    let mut targets = normalized
        .match_indices(marker)
        .filter_map(|(start, _)| {
            let suffix = &normalized[start + marker.len()..];
            let end = suffix.find('\"')?;
            let relative = &suffix[..end];
            let rest = suffix[end + 1..].trim_start();
            rest.starts_with("%*").then(|| relative.to_string())
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return Err(Error::other(format!(
            "project npm wrapper cannot be resolved unambiguously: {}",
            path.display()
        )));
    }
    let relative = PathBuf::from(
        targets
            .pop_first()
            .expect("validated one target")
            .replace('\\', "/"),
    );
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::other(format!(
            "project npm wrapper has an unsafe target: {}",
            path.display()
        )));
    }
    path.parent()
        .map(|parent| parent.join(relative))
        .ok_or_else(|| {
            Error::other(format!(
                "project npm wrapper does not execute a declared target: {}",
                path.display()
            ))
        })
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn render_osdk_project_cmd_wrapper(relative_target: &str) -> Result<String> {
    if !valid_osdk_project_cmd_target(relative_target) {
        return Err(Error::other(
            "project npm bin path cannot be represented safely in cmd.exe",
        ));
    }
    Ok(format!(
        "@echo off\r\n{OSDK_PROJECT_NPM_CMD_MARKER}\r\nnode \"%~dp0{relative_target}\" %*\r\n"
    ))
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn parse_osdk_project_cmd_wrapper(text: &str) -> Option<&str> {
    let prefix = format!("@echo off\r\n{OSDK_PROJECT_NPM_CMD_MARKER}\r\nnode \"%~dp0");
    let relative_target = text.strip_prefix(&prefix)?.strip_suffix("\" %*\r\n")?;
    valid_osdk_project_cmd_target(relative_target).then_some(relative_target)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn valid_osdk_project_cmd_target(relative_target: &str) -> bool {
    !relative_target.is_empty()
        && !relative_target.starts_with(['/', '\\'])
        && relative_target.as_bytes().get(1) != Some(&b':')
        && !relative_target.contains('/')
        && !relative_target.chars().any(|character| {
            character.is_control()
                || matches!(character, '%' | '!' | '"' | '&' | '|' | '<' | '>' | '^')
        })
}

#[cfg(windows)]
fn is_aube_windows_wrapper(text: &str) -> bool {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    if lines.next() != Some("@SETLOCAL") {
        return false;
    }
    let mut line = lines.next();
    if line.is_some_and(|line| line.starts_with("@SET NODE_PATH=")) {
        line = lines.next();
    }
    let Some(if_line) = line else {
        return false;
    };
    if if_line.starts_with("@\"%~dp0\\") && if_line.ends_with("\" %*") {
        return lines.next().is_none();
    }
    if !if_line.starts_with("@IF EXIST \"%~dp0\\") || !if_line.ends_with(".exe\" (") {
        return false;
    }
    let Some(local) = lines.next() else {
        return false;
    };
    if !local.starts_with("  \"%~dp0\\") || !local.ends_with("\" %*") {
        return false;
    }
    if lines.next() != Some(") ELSE (") || lines.next() != Some("  @SET PATHEXT=%PATHEXT:;.JS;=;%")
    {
        return false;
    }
    let Some(fallback) = lines.next() else {
        return false;
    };
    fallback.starts_with("  ")
        && fallback.contains(" \"%~dp0\\")
        && fallback.ends_with("\" %*")
        && lines.next() == Some(")")
        && lines.next().is_none()
}

#[cfg(windows)]
fn is_npm_windows_wrapper(text: &str) -> bool {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    for expected in [
        "@ECHO off",
        "GOTO start",
        ":find_dp0",
        "SET dp0=%~dp0",
        "EXIT /b",
        ":start",
        "SETLOCAL",
        "CALL :find_dp0",
    ] {
        if lines.next() != Some(expected) {
            return false;
        }
    }
    let remaining = lines.collect::<Vec<_>>().join("\n");
    if remaining.contains("\nGOTO ")
        || remaining.contains("\nCALL ")
        || remaining.contains("\nSTART ")
    {
        return false;
    }
    if remaining.contains("IF EXIST \"%dp0%\\") {
        remaining.contains("SET \"_prog=")
            && remaining.contains("SET PATHEXT=%PATHEXT:;.JS;=;%")
            && remaining
                .contains("endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"")
    } else {
        remaining
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            == 1
            && remaining.contains("\"%dp0%\\")
            && remaining.trim_end().ends_with("\" %*")
    }
}

fn discover_bins(install_root: &Path, bin_dir: &Path) -> Result<Vec<DynamicToolBin>> {
    let canonical_root =
        std::fs::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    let mut bins = Vec::new();
    for name in discover_bin_names(bin_dir)? {
        let absolute = resolve_bin_target(bin_dir, &name)?;
        let relative = absolute
            .strip_prefix(&canonical_root)
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

fn discover_global_bins(install_root: &Path, bin_dir: &Path) -> Result<Vec<DynamicToolBin>> {
    let canonical_root =
        std::fs::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    let canonical_bin_dir =
        std::fs::canonicalize(bin_dir).map_err(|error| Error::io(bin_dir, error))?;
    let relative_bin_dir = canonical_bin_dir
        .strip_prefix(&canonical_root)
        .map_err(|_| {
            Error::other(crate::t!(
                "err.npm_bin_outside_install_root",
                name = bin_dir.display(),
                path = install_root.display()
            ))
        })?;
    let mut bins = Vec::new();
    for name in discover_bin_names(bin_dir)? {
        let absolute = resolve_bin_target(bin_dir, &name)?;
        absolute.strip_prefix(&canonical_root).map_err(|_| {
            Error::other(crate::t!(
                "err.npm_bin_outside_install_root",
                name = name,
                path = install_root.display()
            ))
        })?;
        let entry = global_bin_entry(bin_dir, &name).ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_bin_target_unresolved",
                name = name,
                path = bin_dir.display()
            ))
        })?;
        let file_name = entry.file_name().ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_bin_target_unresolved",
                name = name,
                path = bin_dir.display()
            ))
        })?;
        let relative = relative_bin_dir.join(file_name);
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

fn global_bin_entry(bin_dir: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.exe")),
        bin_dir.join(format!("{name}.bat")),
        bin_dir.join(name),
    ];
    #[cfg(not(windows))]
    let candidates = [bin_dir.join(name)];
    candidates.into_iter().find(|candidate| candidate.exists())
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

/// Path to the atomically replaced pointer for the active curated project npm
/// bin generation. Callers that need a rollback snapshot should read this
/// file before publishing a new generation.
pub fn project_bin_current_path(project_root: &Path) -> PathBuf {
    project_root
        .join(PROJECT_NPM_BIN_ROOT)
        .join(PROJECT_NPM_BIN_CURRENT)
}

/// Publish an immutable, osdk-owned bin generation for a project npm tool.
/// Existing selections are retained only while their exact configured specs
/// still match the trusted configuration supplied by the caller.
pub fn publish_project_bin_generation(
    project_root: &Path,
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    validate_project_npm_selection_for_project(selection, configured_specs, project_root)?;

    let canonical_project =
        dunce::canonicalize(project_root).map_err(|error| Error::io(project_root, error))?;
    let root = prepare_project_bin_root(project_root, &canonical_project)?;
    let _lock = crate::lock::FileLock::acquire(root.join("publish.lock"))?;
    let existing_manifest = read_current_project_bin_manifest(project_root)?;
    let mut selections = match existing_manifest {
        Some((_, manifest)) => manifest
            .selections
            .into_iter()
            .filter(|existing| {
                existing.backend != selection.backend
                    && configured_specs.get(&existing.backend) == Some(&existing.configured_spec)
            })
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    selections.push(selection.clone());
    selections.sort_by(|left, right| left.backend.cmp(&right.backend));
    selections.dedup_by(|left, right| left.backend == right.backend);
    let mut bins = Vec::new();
    let mut names = BTreeSet::new();
    for selected in &selections {
        validate_project_npm_selection_for_project(selected, configured_specs, project_root)?;
        let backend = NpmPackageBackend::from_id(&selected.backend).ok_or_else(|| {
            Error::other(format!(
                "invalid npm backend in project bin selection: {}",
                selected.backend
            ))
        })?;
        for bin in backend.validated_project_package_bins(project_root, &selected.version)? {
            let conflict_key = project_bin_conflict_key(&bin.name);
            if !names.insert(conflict_key) {
                return Err(Error::other(format!(
                    "project npm bin `{}` is declared by more than one configured package",
                    bin.name
                )));
            }
            bins.push(ProjectNpmBinManifestEntry {
                name: bin.name,
                backend: selected.backend.clone(),
                target: portable_relative_path(&bin.project_relative_target)?,
            });
        }
    }
    bins.sort_by(|left, right| left.name.cmp(&right.name));

    let generation = project_bin_generation_id(&selections, &bins)?;
    let manifest = ProjectNpmBinManifest {
        schema: PROJECT_NPM_BIN_SCHEMA,
        generation: generation.clone(),
        platform: project_bin_platform().into(),
        selections,
        bins,
    };

    let generations = root.join(PROJECT_NPM_BIN_GENERATIONS);
    ensure_project_bin_directory(&generations, &canonical_project, true)?;
    let generation_dir = generations.join(&generation);
    if generation_dir.exists() {
        validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
    } else {
        let staging = unique_project_bin_path(&generations, "stage");
        std::fs::create_dir(&staging).map_err(|error| Error::io(&staging, error))?;
        let build_result = (|| {
            let bin_dir = staging.join(PROJECT_NPM_BIN_BIN_DIR);
            std::fs::create_dir(&bin_dir).map_err(|error| Error::io(&bin_dir, error))?;
            for entry in &manifest.bins {
                let target = canonical_project.join(path_from_portable(&entry.target)?);
                write_curated_project_launcher(&bin_dir, &entry.name, &target)?;
            }
            write_project_bin_json(&staging.join(PROJECT_NPM_BIN_MANIFEST), &manifest)
        })();
        if let Err(error) = build_result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&staging, &generation_dir) {
            if !generation_dir.exists() {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(Error::io(&generation_dir, error));
            }
            let _ = std::fs::remove_dir_all(&staging);
            validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
        }
    }
    validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;

    let current = ProjectNpmBinCurrent {
        schema: PROJECT_NPM_BIN_SCHEMA,
        generation,
    };
    write_project_bin_json(&root.join(PROJECT_NPM_BIN_CURRENT), &current)?;
    Ok(generation_dir.join(PROJECT_NPM_BIN_BIN_DIR))
}

/// Resolve the active curated project bin directory after validating it
/// against trusted configuration and the current filesystem. No state is
/// changed; a missing current pointer means no curated generation is active.
pub fn validated_project_bin_dir(
    project_root: &Path,
    configured_specs: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    let Some((generation_dir, manifest)) = read_current_project_bin_manifest(project_root)? else {
        return Ok(None);
    };
    for selection in &manifest.selections {
        validate_project_npm_selection_for_project(selection, configured_specs, project_root)?;
    }
    if manifest.selections.iter().any(|selection| {
        configured_specs.get(&selection.backend) != Some(&selection.configured_spec)
    }) {
        return Err(Error::other(
            "project npm bin manifest does not match trusted project configuration",
        ));
    }
    let canonical_project =
        dunce::canonicalize(project_root).map_err(|error| Error::io(project_root, error))?;
    let root = ensure_project_bin_directory(
        &project_root.join(PROJECT_NPM_BIN_ROOT),
        &canonical_project,
        false,
    )?;
    let generations = ensure_project_bin_directory(
        &root.join(PROJECT_NPM_BIN_GENERATIONS),
        &canonical_project,
        false,
    )?;
    if !generation_dir.starts_with(&generations) {
        return Err(Error::other(
            "project npm bin generation resolves outside its owned directory",
        ));
    }
    validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
    Ok(Some(generation_dir.join(PROJECT_NPM_BIN_BIN_DIR)))
}

fn validate_project_npm_selection(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<()> {
    validate_project_npm_selection_identity(selection, configured_specs)?;
    if !matches!(
        npm_spec_satisfaction(&selection.configured_spec, &selection.version),
        Some(true)
    ) {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

fn validate_project_npm_selection_for_project(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
    project_root: &Path,
) -> Result<()> {
    validate_project_npm_selection_identity(selection, configured_specs)?;
    if validate_project_npm_selection(selection, configured_specs).is_ok() {
        return Ok(());
    }
    if !npm_channel_spec(&selection.configured_spec)
        || !project_lock_binds_selection(project_root, selection)?
    {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

fn validate_project_npm_selection_identity(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<()> {
    if NpmPackageBackend::from_id(&selection.backend)
        .is_none_or(|backend| backend.id() != selection.backend)
        || selection.configured_spec.trim().is_empty()
        || selection.version.trim().is_empty()
        || configured_specs.get(&selection.backend) != Some(&selection.configured_spec)
    {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

fn npm_channel_spec(spec: &str) -> bool {
    let spec = spec.trim();
    !spec.is_empty()
        && spec
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        && npm_spec_satisfaction(spec, "0.0.0").is_none()
}

fn project_lock_binds_selection(
    project_root: &Path,
    selection: &ProjectNpmBinSelection,
) -> Result<bool> {
    let path = project_root.join(PROJECT_NPM_LOCKFILE);
    let bytes = match crate::inventory::read_stable_regular_file(&path, PROJECT_NPM_LOCK_MAX_BYTES)
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(&path, error)),
    };
    let lock: ProjectNpmLockfile = toml::from_str(std::str::from_utf8(&bytes).map_err(|_| {
        Error::other(format!("project npm lock is not UTF-8: {}", path.display()))
    })?)?;
    if lock.schema != 3 {
        return Ok(false);
    }
    let package = NpmPackageBackend::from_id(&selection.backend)
        .expect("selection backend was validated before lock lookup")
        .package;
    Ok(lock.platforms.values().any(|platform| {
        platform
            .tools
            .get(&selection.backend)
            .is_some_and(|locked| {
                locked.request == selection.configured_spec
                    && locked.version == selection.version
                    && locked.npm.as_ref().is_some_and(|npm| {
                        npm.package == package && npm.scope == ToolScope::Project.as_str()
                    })
            })
    }))
}

fn npm_spec_satisfaction(configured_spec: &str, exact_version: &str) -> Option<bool> {
    let configured_spec = configured_spec.trim();
    let exact_version = exact_version
        .trim()
        .strip_prefix('v')
        .unwrap_or(exact_version.trim());
    let Ok(version) = semver::Version::parse(exact_version) else {
        return Some(false);
    };
    if let Ok(exact) =
        semver::Version::parse(configured_spec.strip_prefix('v').unwrap_or(configured_spec))
    {
        return Some(exact == version);
    }
    let numeric_prefix = configured_spec
        .strip_prefix('v')
        .unwrap_or(configured_spec)
        .split('.')
        .collect::<Vec<_>>();
    if !numeric_prefix.is_empty()
        && numeric_prefix.len() < 3
        && numeric_prefix
            .iter()
            .all(|component| !component.is_empty() && component.chars().all(|c| c.is_ascii_digit()))
    {
        let exact_components = [version.major.to_string(), version.minor.to_string()];
        return Some(
            numeric_prefix
                .iter()
                .zip(exact_components.iter())
                .all(|(expected, actual)| *expected == actual),
        );
    }

    // Config entries use npm's version vocabulary. A successful semver
    // requirement parse covers caret/tilde/comparator ranges and numeric
    // prefixes such as `3` and `3.6`. Symbolic dist-tags (`latest`, `beta`,
    // custom channels) are deliberately rejected here: their meaning cannot
    // be reconstructed from an exact installed version alone.
    npm_semver_requirements(configured_spec).map(|requirements| {
        requirements
            .iter()
            .any(|requirement| requirement.matches(&version))
    })
}

fn npm_semver_requirements(spec: &str) -> Option<Vec<semver::VersionReq>> {
    spec.split("||")
        .map(|alternative| {
            let alternative = alternative.trim();
            if alternative.is_empty() || !npm_range_shape(alternative) {
                return None;
            }
            let normalized = alternative
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(", ");
            semver::VersionReq::parse(&normalized).ok()
        })
        .collect()
}

fn npm_range_shape(spec: &str) -> bool {
    spec.chars().all(|character| {
        character.is_ascii_digit()
            || character.is_ascii_whitespace()
            || matches!(
                character,
                '.' | '-' | '+' | '*' | 'x' | 'X' | '^' | '~' | '<' | '>' | '='
            )
    })
}

fn read_current_project_bin_manifest(
    project_root: &Path,
) -> Result<Option<(PathBuf, ProjectNpmBinManifest)>> {
    let current_path = project_bin_current_path(project_root);
    let current = match read_project_bin_json::<ProjectNpmBinCurrent>(&current_path) {
        Ok(current) => current,
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if current.schema != PROJECT_NPM_BIN_SCHEMA || !valid_generation_id(&current.generation) {
        return Err(Error::other(format!(
            "invalid project npm bin pointer at {}",
            current_path.display()
        )));
    }
    let generation_dir = project_root
        .join(PROJECT_NPM_BIN_ROOT)
        .join(PROJECT_NPM_BIN_GENERATIONS)
        .join(&current.generation);
    let generation_metadata = std::fs::symlink_metadata(&generation_dir)
        .map_err(|error| Error::io(&generation_dir, error))?;
    if !generation_metadata.is_dir() || generation_metadata.file_type().is_symlink() {
        return Err(Error::other(format!(
            "project npm bin generation is not an owned directory: {}",
            generation_dir.display()
        )));
    }
    let generation_dir =
        dunce::canonicalize(&generation_dir).map_err(|error| Error::io(&generation_dir, error))?;
    let manifest_path = generation_dir.join(PROJECT_NPM_BIN_MANIFEST);
    let manifest = read_project_bin_json::<ProjectNpmBinManifest>(&manifest_path)?;
    if manifest.schema != PROJECT_NPM_BIN_SCHEMA
        || manifest.generation != current.generation
        || manifest.platform != project_bin_platform()
    {
        return Err(Error::other(format!(
            "project npm bin manifest identity mismatch at {}",
            manifest_path.display()
        )));
    }
    Ok(Some((generation_dir, manifest)))
}

fn prepare_project_bin_root(project_root: &Path, canonical_project: &Path) -> Result<PathBuf> {
    let osdk = project_root.join(".osdk");
    ensure_project_bin_directory(&osdk, canonical_project, true)?;
    ensure_project_bin_directory(
        &project_root.join(PROJECT_NPM_BIN_ROOT),
        canonical_project,
        true,
    )
}

fn ensure_project_bin_directory(
    path: &Path,
    canonical_project: &Path,
    create: bool,
) -> Result<PathBuf> {
    if create {
        std::fs::create_dir(path)
            .or_else(|error| {
                (error.kind() == std::io::ErrorKind::AlreadyExists)
                    .then_some(())
                    .ok_or(error)
            })
            .map_err(|error| Error::io(path, error))?;
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::other(format!(
            "project npm bin path is not an owned directory: {}",
            path.display()
        )));
    }
    let canonical = dunce::canonicalize(path).map_err(|error| Error::io(path, error))?;
    if !canonical.starts_with(canonical_project) {
        return Err(Error::other(format!(
            "project npm bin directory escapes project: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn validate_project_bin_generation(
    canonical_project: &Path,
    generation_dir: &Path,
    expected_manifest: &ProjectNpmBinManifest,
) -> Result<()> {
    if expected_manifest.selections.is_empty()
        || expected_manifest.bins.is_empty()
        || !valid_generation_id(&expected_manifest.generation)
        || expected_manifest.schema != PROJECT_NPM_BIN_SCHEMA
        || expected_manifest.platform != project_bin_platform()
        || generation_dir.file_name().and_then(|name| name.to_str())
            != Some(expected_manifest.generation.as_str())
        || project_bin_generation_id(&expected_manifest.selections, &expected_manifest.bins)?
            != expected_manifest.generation
    {
        return Err(Error::other("invalid project npm bin generation identity"));
    }
    let actual_manifest = read_project_bin_json::<ProjectNpmBinManifest>(
        &generation_dir.join(PROJECT_NPM_BIN_MANIFEST),
    )?;
    if &actual_manifest != expected_manifest {
        return Err(Error::other(format!(
            "project npm bin generation manifest mismatch at {}",
            generation_dir.display()
        )));
    }

    let resolved_bins =
        declared_project_bin_entries(canonical_project, &expected_manifest.selections)?;
    let declared_manifest_entries = resolved_bins
        .iter()
        .map(|(entry, _)| entry.clone())
        .collect::<Vec<_>>();
    if declared_manifest_entries != expected_manifest.bins {
        return Err(Error::other(
            "project npm bin manifest no longer matches installed package declarations",
        ));
    }

    let mut expected_files = BTreeSet::new();
    expected_files.insert(PROJECT_NPM_BIN_MANIFEST.to_string());
    expected_files.insert(PROJECT_NPM_BIN_BIN_DIR.to_string());
    for (entry, target) in resolved_bins {
        let launcher_name = curated_launcher_name(&entry.name);
        expected_files.insert(format!("{PROJECT_NPM_BIN_BIN_DIR}/{launcher_name}"));
        validate_curated_project_launcher(
            &generation_dir
                .join(PROJECT_NPM_BIN_BIN_DIR)
                .join(launcher_name),
            &target,
        )?;
    }

    let actual_files = project_bin_file_set(generation_dir)?;
    if actual_files != expected_files {
        return Err(Error::other(format!(
            "project npm bin generation contains unexpected or missing files at {}",
            generation_dir.display()
        )));
    }
    Ok(())
}

fn project_bin_generation_id(
    selections: &[ProjectNpmBinSelection],
    bins: &[ProjectNpmBinManifestEntry],
) -> Result<String> {
    let identity = serde_json::to_vec(&(
        PROJECT_NPM_BIN_SCHEMA,
        project_bin_platform(),
        selections,
        bins,
    ))?;
    Ok(pipeline::verify::hash_bytes(
        &identity,
        pipeline::HashAlgo::Sha256,
    ))
}

fn declared_project_bin_entries(
    canonical_project: &Path,
    selections: &[ProjectNpmBinSelection],
) -> Result<Vec<(ProjectNpmBinManifestEntry, PathBuf)>> {
    let mut resolved = Vec::new();
    let mut seen_backends = BTreeSet::new();
    let mut seen_bins = BTreeSet::new();
    let mut previous_backend = None;
    for selection in selections {
        if selection.configured_spec.trim().is_empty()
            || selection.version.trim().is_empty()
            || !seen_backends.insert(selection.backend.clone())
            || previous_backend
                .as_ref()
                .is_some_and(|previous| previous >= &selection.backend)
        {
            return Err(Error::other("invalid project npm bin selection manifest"));
        }
        previous_backend = Some(selection.backend.clone());
        let backend = NpmPackageBackend::from_id(&selection.backend)
            .filter(|backend| backend.id() == selection.backend)
            .ok_or_else(|| Error::other("invalid npm backend in project bin manifest"))?;
        let package_dir = package_install_dir(canonical_project, backend.package());
        let canonical_package =
            dunce::canonicalize(&package_dir).map_err(|error| Error::io(&package_dir, error))?;
        if !canonical_package.starts_with(canonical_project) {
            return Err(Error::other(format!(
                "installed npm package {} resolves outside project",
                backend.package()
            )));
        }
        let package_json = package_dir.join("package.json");
        let bytes = crate::inventory::read_stable_regular_file(
            &package_json,
            NPM_PACKAGE_MANIFEST_MAX_BYTES,
        )
        .map_err(|error| Error::io(&package_json, error))?;
        let package_manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
        if package_manifest
            .get("name")
            .and_then(serde_json::Value::as_str)
            != Some(backend.package())
            || package_manifest
                .get("version")
                .and_then(serde_json::Value::as_str)
                != Some(selection.version.as_str())
        {
            return Err(Error::other(format!(
                "installed project package identity mismatch: expected {}@{}",
                backend.package(),
                selection.version
            )));
        }
        for (name, relative) in package_bin_entries(&package_manifest, backend.package())? {
            if relative.is_absolute()
                || relative.as_os_str().is_empty()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(Error::other(format!(
                    "npm package {} declares unsafe bin path {}",
                    backend.package(),
                    relative.display()
                )));
            }
            if !seen_bins.insert(project_bin_conflict_key(&name)) {
                return Err(Error::other(format!(
                    "project npm bin `{name}` is declared by more than one configured package"
                )));
            }
            let declared = package_dir.join(relative);
            let target =
                dunce::canonicalize(&declared).map_err(|error| Error::io(&declared, error))?;
            if !target.is_file()
                || !target.starts_with(&canonical_package)
                || !target.starts_with(canonical_project)
            {
                return Err(Error::other(format!(
                    "project npm bin target escapes its package: {}",
                    declared.display()
                )));
            }
            let entry = ProjectNpmBinManifestEntry {
                name,
                backend: selection.backend.clone(),
                target: portable_relative_path(
                    target
                        .strip_prefix(canonical_project)
                        .map_err(|_| Error::other("project npm bin target escapes project"))?,
                )?,
            };
            resolved.push((entry, target));
        }
    }
    resolved.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    Ok(resolved)
}

fn project_bin_file_set(root: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|error| {
            Error::other(format!(
                "reading project npm bin generation {}: {error}",
                root.display()
            ))
        })?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::other("project npm bin entry escaped its generation"))?;
        let portable = portable_relative_path(relative)?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| Error::io(entry.path(), error))?;
        if metadata.is_dir() {
            if portable != PROJECT_NPM_BIN_BIN_DIR {
                return Err(Error::other(format!(
                    "unexpected directory in project npm bin generation: {portable}"
                )));
            }
        } else if !metadata.is_file() && !metadata.file_type().is_symlink() {
            return Err(Error::other(format!(
                "unsupported file type in project npm bin generation: {portable}"
            )));
        }
        files.insert(portable);
    }
    Ok(files)
}

fn valid_generation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn project_bin_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else {
        "unix"
    }
}

fn project_bin_conflict_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

fn portable_relative_path(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(Error::other(format!(
            "unsafe project npm bin relative path: {}",
            path.display()
        )));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn path_from_portable(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\\') {
        return Err(Error::other("invalid project npm bin target path"));
    }
    let path = PathBuf::from(value);
    portable_relative_path(&path)?;
    Ok(path)
}

fn curated_launcher_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    }
}

#[cfg(not(windows))]
fn write_curated_project_launcher(bin_dir: &Path, name: &str, target: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let launcher = bin_dir.join(name);
    let relative = relative_path_from(bin_dir, target).ok_or_else(|| {
        Error::other(format!(
            "cannot construct relative project npm bin target for {}",
            target.display()
        ))
    })?;
    symlink(&relative, &launcher).map_err(|error| Error::io(&launcher, error))
}

#[cfg(windows)]
fn write_curated_project_launcher(bin_dir: &Path, name: &str, target: &Path) -> Result<()> {
    let launcher = bin_dir.join(format!("{name}.cmd"));
    let relative = relative_path_from(bin_dir, target).ok_or_else(|| {
        Error::other(format!(
            "cannot construct relative project npm bin target for {}",
            target.display()
        ))
    })?;
    let target = relative.to_string_lossy().replace('/', "\\");
    let contents = render_osdk_project_cmd_wrapper(&target)?;
    std::fs::write(&launcher, contents).map_err(|error| Error::io(&launcher, error))
}

fn validate_curated_project_launcher(launcher: &Path, target: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        let metadata =
            std::fs::symlink_metadata(launcher).map_err(|error| Error::io(launcher, error))?;
        if !metadata.file_type().is_symlink()
            || dunce::canonicalize(launcher).map_err(|error| Error::io(launcher, error))? != target
        {
            return Err(Error::other(format!(
                "invalid curated project npm launcher at {}",
                launcher.display()
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let actual = parse_windows_project_wrapper(launcher)?;
        let actual = dunce::canonicalize(&actual).map_err(|error| Error::io(&actual, error))?;
        if actual != target {
            return Err(Error::other(format!(
                "invalid curated project npm launcher at {}",
                launcher.display()
            )));
        }
        Ok(())
    }
}

fn relative_path_from(base: &Path, target: &Path) -> Option<PathBuf> {
    let base = base.components().collect::<Vec<_>>();
    let target = target.components().collect::<Vec<_>>();
    let shared = base
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn read_project_bin_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = crate::inventory::read_stable_regular_file(path, PROJECT_NPM_BIN_MAX_JSON_BYTES)
        .map_err(|error| Error::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn write_project_bin_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::other(format!(
            "project npm bin path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    let temporary = unique_project_bin_path(parent, "metadata");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        file.write_all(&bytes)
            .map_err(|error| Error::io(&temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io(&temporary, error))?;
    }
    if let Err(error) = atomic_replace_project_bin(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn unique_project_bin_path(parent: &Path, label: &str) -> PathBuf {
    loop {
        let nonce = NEXT_PROJECT_NPM_BIN_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{label}-{}-{nonce}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

#[cfg(not(windows))]
fn atomic_replace_project_bin(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|error| Error::io(destination, error))
}

#[cfg(windows)]
fn atomic_replace_project_bin(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(Error::io(destination, std::io::Error::last_os_error()));
    }
    Ok(())
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

    #[cfg(unix)]
    fn write_project_package_fixture(
        project: &Path,
        package: &str,
        version: &str,
        bin_name: &str,
    ) -> PathBuf {
        let package_dir = package_install_dir(project, package);
        let target = package_dir.join("bin/tool.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": package,
                "version": version,
                "bin": { bin_name: "bin/tool.js" }
            }))
            .unwrap(),
        )
        .unwrap();
        target
    }

    #[test]
    fn osdk_windows_project_wrapper_round_trips_exact_target() {
        let target = "..\\..\\..\\node_modules\\prettier\\bin\\prettier.cjs";
        let wrapper = render_osdk_project_cmd_wrapper(target).unwrap();

        assert_eq!(
            wrapper,
            format!("@echo off\r\n{OSDK_PROJECT_NPM_CMD_MARKER}\r\nnode \"%~dp0{target}\" %*\r\n")
        );
        assert_eq!(parse_osdk_project_cmd_wrapper(&wrapper), Some(target));
    }

    #[test]
    fn osdk_windows_project_wrapper_rejects_appended_commands() {
        let target = "..\\pkg\\cli.js";
        let wrapper = render_osdk_project_cmd_wrapper(target).unwrap();

        assert!(parse_osdk_project_cmd_wrapper(&format!("{wrapper}calc.exe\r\n")).is_none());
        assert!(
            parse_osdk_project_cmd_wrapper(&wrapper.replace(" %*", " %* & calc.exe")).is_none()
        );
        assert!(render_osdk_project_cmd_wrapper("..\\pkg\\cli.js&calc.exe").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn project_package_bins_accept_exact_symlink_and_reject_opaque_regular_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let target = write_project_package_fixture(project, "prettier", "3.6.2", "prettier");
        let launcher = project.join("node_modules/.bin/prettier");
        symlink(&target, &launcher).unwrap();
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        assert_eq!(
            backend
                .validate_project_package_bins(project, "3.6.2")
                .unwrap(),
            vec!["prettier"]
        );

        std::fs::remove_file(&launcher).unwrap();
        std::fs::write(&launcher, b"#!/bin/sh\necho not-the-target\n").unwrap();
        assert!(backend
            .validate_project_package_bins(project, "3.6.2")
            .is_err());

        std::fs::write(
            &launcher,
            format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", target.display()),
        )
        .unwrap();
        assert_eq!(
            backend
                .validate_project_package_bins(project, "3.6.2")
                .unwrap(),
            vec!["prettier"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn curated_project_bin_generation_is_persistent_and_rejects_extra_files() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let target = write_project_package_fixture(project, "prettier", "3.6.2", "prettier");
        symlink(&target, project.join("node_modules/.bin/prettier")).unwrap();
        let selection = ProjectNpmBinSelection {
            backend: "npm:prettier".into(),
            configured_spec: "^3.6".into(),
            version: "3.6.2".into(),
        };
        let configured =
            BTreeMap::from([(selection.backend.clone(), selection.configured_spec.clone())]);
        let bin = publish_project_bin_generation(project, &selection, &configured).unwrap();
        assert_eq!(
            validated_project_bin_dir(project, &configured).unwrap(),
            Some(bin.clone())
        );

        let configured_with_unpublished = BTreeMap::from([
            (selection.backend.clone(), selection.configured_spec.clone()),
            ("npm:legacy".into(), "1".into()),
        ]);
        assert_eq!(
            validated_project_bin_dir(project, &configured_with_unpublished).unwrap(),
            Some(bin.clone())
        );

        std::fs::write(bin.join("unexpected"), b"payload").unwrap();
        assert!(validated_project_bin_dir(project, &configured).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn curated_project_bin_generation_rejects_symlinked_owned_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let target = write_project_package_fixture(project, "prettier", "3.6.2", "prettier");
        symlink(&target, project.join("node_modules/.bin/prettier")).unwrap();
        symlink(outside.path(), project.join(".osdk")).unwrap();
        let selection = ProjectNpmBinSelection {
            backend: "npm:prettier".into(),
            configured_spec: "3.6.2".into(),
            version: "3.6.2".into(),
        };
        let configured =
            BTreeMap::from([(selection.backend.clone(), selection.configured_spec.clone())]);

        assert!(publish_project_bin_generation(project, &selection, &configured).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn curated_project_selection_enforces_npm_version_constraints() {
        for (spec, exact) in [
            ("3.6.2", "3.6.2"),
            ("v3.6.2", "3.6.2"),
            ("3", "3.6.2"),
            ("3.6", "3.6.2"),
            ("^3.6.0", "3.9.1"),
            ("~3.6.0", "3.6.9"),
            (">=3.6.0 <4", "3.8.0"),
            ("^2 || ^3.6", "3.7.0"),
        ] {
            assert_eq!(
                npm_spec_satisfaction(spec, exact),
                Some(true),
                "{spec} -> {exact}"
            );
        }

        for (spec, exact) in [
            ("3.6.2", "3.6.3"),
            ("3.6", "3.7.0"),
            ("^3.6.0", "4.0.0"),
            ("~3.6.0", "3.7.0"),
            (">=3.6.0 <4", "4.0.0"),
            ("^3.6.0", "not-exact"),
        ] {
            assert_eq!(
                npm_spec_satisfaction(spec, exact),
                Some(false),
                "{spec} -> {exact}"
            );
        }
        for channel in ["latest", "beta", "next-1"] {
            assert_eq!(npm_spec_satisfaction(channel, "3.6.2"), None);
        }
        assert_eq!(npm_spec_satisfaction("workspace:*", "3.6.2"), None);
        assert_eq!(npm_spec_satisfaction("not a range", "3.6.2"), None);
    }

    #[test]
    fn curated_project_channel_requires_matching_project_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let selection = ProjectNpmBinSelection {
            backend: "npm:prettier".into(),
            configured_spec: "latest".into(),
            version: "3.6.2".into(),
        };
        let configured =
            BTreeMap::from([(selection.backend.clone(), selection.configured_spec.clone())]);
        assert!(
            validate_project_npm_selection_for_project(&selection, &configured, project).is_err()
        );

        std::fs::write(
            project.join(PROJECT_NPM_LOCKFILE),
            r#"
schema = 3

[platforms.linux-x64.tools."npm:prettier"]
request = "latest"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "aube"
scope = "project"
"#,
        )
        .unwrap();
        validate_project_npm_selection_for_project(&selection, &configured, project).unwrap();

        let mismatched = ProjectNpmBinSelection {
            version: "3.6.3".into(),
            ..selection
        };
        assert!(
            validate_project_npm_selection_for_project(&mismatched, &configured, project).is_err()
        );
    }

    #[test]
    fn aube_storage_is_shared_across_packages_versions_and_scopes() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let cases = [
            ("npm:prettier", "3.6.2"),
            ("npm:prettier", "3.6.1"),
            ("npm:@antfu/ni", "0.21.12"),
        ];
        let paths = cases.map(|(id, version)| {
            let backend = NpmPackageBackend::from_id(id).unwrap();
            let env = backend
                .exec_env(&ctx, &ToolVersion::new(id, version))
                .unwrap();
            (
                PathBuf::from(&env["npm_config_cache"]),
                PathBuf::from(&env["npm_config_store_dir"]),
            )
        });

        assert!(paths.iter().all(|path| path == &paths[0]));
        assert_eq!(paths[0].0, temporary.path().join("cache/aube/v1/cache"));
        assert_eq!(paths[0].1, temporary.path().join("store/aube"));
    }

    #[test]
    fn isolated_and_global_install_roots_are_distinct_for_the_same_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:@antfu/ni").unwrap();
        let version = npm_test_version(&backend, "1.0.0");
        let isolated = backend
            .install_locator(&ctx, &version, ToolScope::Project)
            .unwrap();
        let global = backend
            .install_locator(&ctx, &version, ToolScope::Global)
            .unwrap();

        assert_ne!(isolated.install_root(), global.install_root());
        assert_eq!(
            isolated.install_root(),
            backend
                .legacy_isolated_install_root(&ctx, &version.version)
                .join(crate::dirs::install_id_component(&isolated.identity().install_id).unwrap())
        );
        assert_eq!(
            global.install_root(),
            backend
                .legacy_global_install_root_path(&ctx, &version.version)
                .join(crate::dirs::install_id_component(&global.identity().install_id).unwrap())
        );
        assert_eq!(isolated.identity().scope, InstallScope::Isolated);
        assert_eq!(global.identity().scope, InstallScope::Global);
    }

    const TEST_NODE_VERSION: &str = "20.10.0";

    fn npm_test_version(backend: &NpmPackageBackend, version: &str) -> ToolVersion {
        npm_test_version_with_node(backend, version, TEST_NODE_VERSION)
    }

    fn npm_test_version_with_node(
        backend: &NpmPackageBackend,
        version: &str,
        node_version: &str,
    ) -> ToolVersion {
        let mut tool = ToolVersion::new(backend.id(), version);
        tool.options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.into());
        tool
    }

    fn with_scope(mut version: ToolVersion, scope: ToolScope) -> ToolVersion {
        version
            .options
            .insert(LOCKED_NPM_SCOPE_OPTION.into(), scope.as_str().into());
        version
    }

    fn write_scope_fixture(
        backend: &NpmPackageBackend,
        ctx: &Ctx,
        version: &ToolVersion,
        scope: ToolScope,
        bin_name: &str,
    ) -> PathBuf {
        let locator = backend.install_locator(ctx, version, scope).unwrap();
        let root = locator.install_root().to_path_buf();
        let relative_bin = match scope {
            ToolScope::Project => format!("project/node_modules/.bin/{bin_name}"),
            ToolScope::Global => format!("bin/{bin_name}"),
        };
        let bin = root.join(&relative_bin);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        if scope == ToolScope::Project {
            let project = root.join(PROJECT_DIR);
            NpmPackageBackend::write_project_manifest(
                &project,
                Some((backend.package(), &version.version)),
                &NpmPackageBackend::build_policy(version).unwrap(),
            )
            .unwrap();
            std::fs::create_dir_all(package_install_dir(&project, backend.package())).unwrap();
            let lockfile = npm_test_lockfile(
                backend.package(),
                &version.version,
                "sha512-fixture-integrity",
            );
            std::fs::write(project.join(AUBE_LOCKFILE_NAME), &lockfile).unwrap();
        } else {
            let project = root.join(PROJECT_DIR);
            let package = package_install_dir(&project, backend.package());
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("package.json"),
                serde_json::to_vec(&serde_json::json!({
                    "name": backend.package(),
                    "version": version.version,
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::write(
                project.join(AUBE_LOCKFILE_NAME),
                npm_test_lockfile(
                    backend.package(),
                    &version.version,
                    "sha512-fixture-integrity",
                ),
            )
            .unwrap();
        }
        write_node_fixture(ctx, &version.options[LOCKED_NPM_NODE_VERSION_OPTION]);
        let mut manifest = DynamicToolManifest::from_identity(locator.identity().clone()).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: bin_name.into(),
            path: relative_bin,
        }];
        manifest.write_atomic(&root).unwrap();
        write_npm_receipt(
            &root,
            &NpmInstallReceipt {
                schema: NPM_INSTALL_RECEIPT_SCHEMA,
                provider: PROVIDER.into(),
                package: backend.package().into(),
                installer: "aube".into(),
                node_version: version.options[LOCKED_NPM_NODE_VERSION_OPTION].clone(),
                build_policy: NpmPackageBackend::build_policy(version).unwrap().identity(),
                graph_sha256: (scope == ToolScope::Project).then(|| {
                    pipeline::verify::hash_file(
                        &root.join(PROJECT_DIR).join(AUBE_LOCKFILE_NAME),
                        pipeline::HashAlgo::Sha256,
                    )
                    .unwrap()
                }),
                root_integrity: (scope == ToolScope::Project)
                    .then(|| "sha512-fixture-integrity".into()),
                root_source: (scope == ToolScope::Project)
                    .then(|| format!("npm:{}@{}", backend.package(), version.version)),
                native_lock_format: (scope == ToolScope::Global).then(|| AUBE_LOCK_FORMAT.into()),
                native_lock_sha256: (scope == ToolScope::Global).then(|| {
                    pipeline::verify::hash_file(
                        &root.join(PROJECT_DIR).join(AUBE_LOCKFILE_NAME),
                        pipeline::HashAlgo::Sha256,
                    )
                    .unwrap()
                }),
            },
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    fn write_node_fixture(ctx: &Ctx, version: &str) -> PathBuf {
        let root = ctx.dirs.install_path("node", version);
        let bin = if cfg!(windows) {
            root.join(node_executable_name())
        } else {
            root.join("bin").join(node_executable_name())
        };
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"node fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        bin
    }

    #[test]
    fn completed_selection_rejects_marker_symlink_tampered_receipt_and_missing_node() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let root = write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "prettier");

        assert_eq!(
            backend.selected_install_root(&ctx, &version).unwrap(),
            Some(root.clone())
        );

        let marker = root.join(".osdk-complete");
        std::fs::remove_file(&marker).unwrap();
        #[cfg(unix)]
        {
            let marker_target = temporary.path().join("complete");
            std::fs::write(&marker_target, b"").unwrap();
            std::os::unix::fs::symlink(&marker_target, &marker).unwrap();
            assert!(backend
                .selected_install_root(&ctx, &version)
                .unwrap()
                .is_none());
            std::fs::remove_file(&marker).unwrap();
        }
        std::fs::write(&marker, b"").unwrap();

        let mut receipt = load_npm_receipt(&root).unwrap();
        receipt.package = "typescript".into();
        write_npm_receipt(&root, &receipt).unwrap();
        assert!(backend
            .selected_install_root(&ctx, &version)
            .unwrap()
            .is_none());

        receipt.package = backend.package().into();
        write_npm_receipt(&root, &receipt).unwrap();
        std::fs::remove_dir_all(ctx.dirs.install_path("node", TEST_NODE_VERSION)).unwrap();
        assert!(backend
            .selected_install_root(&ctx, &version)
            .unwrap()
            .is_none());
        assert!(backend
            .list_installed_for(&ctx, &version)
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn completed_selection_rejects_symlinked_or_oversized_receipt_and_native_lock() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");

        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "prettier");
        let receipt_path = npm_receipt_path(&isolated);
        let receipt_copy = temporary.path().join("receipt.json");
        std::fs::copy(&receipt_path, &receipt_copy).unwrap();
        std::fs::remove_file(&receipt_path).unwrap();
        symlink(&receipt_copy, &receipt_path).unwrap();
        assert!(!backend
            .validate_completed_install(&ctx, &version, ToolScope::Project, &isolated)
            .unwrap());
        std::fs::remove_file(&receipt_path).unwrap();
        std::fs::write(
            &receipt_path,
            vec![b'x'; (NPM_INSTALL_RECEIPT_MAX_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(!backend
            .validate_completed_install(&ctx, &version, ToolScope::Project, &isolated)
            .unwrap());

        let global_version = with_scope(version.clone(), ToolScope::Global);
        let global = write_scope_fixture(
            &backend,
            &ctx,
            &global_version,
            ToolScope::Global,
            "prettier",
        );
        let lock_path = global.join(PROJECT_DIR).join(AUBE_LOCKFILE_NAME);
        let lock_copy = temporary.path().join("native-lock.yaml");
        std::fs::copy(&lock_path, &lock_copy).unwrap();
        std::fs::remove_file(&lock_path).unwrap();
        symlink(&lock_copy, &lock_path).unwrap();
        assert!(!backend
            .validate_completed_install(&ctx, &global_version, ToolScope::Global, &global)
            .unwrap());
    }

    #[test]
    fn installed_version_listing_preserves_manifest_node_identity_after_active_node_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let installed = npm_test_version_with_node(&backend, "3.6.2", "20.10.0");
        let root = write_scope_fixture(&backend, &ctx, &installed, ToolScope::Project, "prettier");
        write_node_fixture(&ctx, "22.14.0");
        ctx.config.tools.insert("node".into(), "22.14.0".into());

        let unbound = ToolVersion::new(backend.id(), "selection");
        assert_eq!(
            backend.list_installed_for(&ctx, &unbound).unwrap(),
            vec!["3.6.2"]
        );

        let selected = ToolVersion::new(backend.id(), "3.6.2");
        assert_ne!(
            backend.isolated_install_root_for(&ctx, &selected).unwrap(),
            root
        );
        assert!(backend
            .selected_install_root(&ctx, &selected)
            .unwrap()
            .is_none());

        assert_eq!(
            backend.selected_install_root(&ctx, &installed).unwrap(),
            Some(root)
        );
    }

    #[test]
    fn installed_identity_listing_preserves_manifest_node_identity_after_active_node_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let installed = npm_test_version_with_node(&backend, "3.6.2", "20.10.0");
        let root = write_scope_fixture(&backend, &ctx, &installed, ToolScope::Project, "prettier");
        write_node_fixture(&ctx, "22.14.0");
        ctx.config.tools.insert("node".into(), "22.14.0".into());

        let hint = ToolVersion::new(backend.id(), "selection");
        let candidates = backend.list_installed_identities_for(&ctx, &hint).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].version, "3.6.2");
        assert_eq!(
            candidates[0].options[LOCKED_NPM_NODE_VERSION_OPTION],
            "20.10.0"
        );
        assert_eq!(
            backend
                .where_install_root_for(&ctx, &candidates[0])
                .unwrap(),
            Some(root)
        );
    }

    #[test]
    fn same_version_option_and_runtime_variants_have_distinct_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let baseline = npm_test_version(&backend, "3.6.2");
        let mut installer = baseline.clone();
        installer.options.insert("installer".into(), "aube".into());
        let mut builds = baseline.clone();
        builds
            .options
            .insert("allow_builds".into(), "esbuild".into());
        let mut newer_node = baseline.clone();
        newer_node
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.14.0".into());

        let roots = [&baseline, &installer, &builds, &newer_node]
            .map(|version| backend.isolated_install_root_for(&ctx, version).unwrap());
        for left in 0..roots.len() {
            for right in left + 1..roots.len() {
                assert_ne!(roots[left], roots[right]);
            }
        }
    }

    #[tokio::test]
    async fn scopes_coexist_and_uninstall_commands_are_isolated() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "isolated-bin");
        let global = write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "global-bin");

        assert_eq!(
            backend.list_installed_for(&ctx, &version).unwrap(),
            vec!["3.6.2"]
        );
        assert_eq!(
            backend.where_install_root_for(&ctx, &version).unwrap(),
            Some(isolated.clone())
        );
        backend.uninstall(&ctx, &version).await.unwrap();
        assert!(!isolated.exists());
        assert!(global.exists());
        let global_version = with_scope(version.clone(), ToolScope::Global);
        assert_eq!(
            backend
                .where_install_root_for(&ctx, &global_version)
                .unwrap(),
            Some(global.clone())
        );
        assert!(backend.uninstall_global(&ctx, &global_version).unwrap());
        assert!(!global.exists());
    }

    #[test]
    fn explicit_scope_selects_only_that_install_root() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "isolated-bin");
        let global = write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "global-bin");

        let isolated_version = with_scope(version.clone(), ToolScope::Project);
        assert_eq!(
            backend
                .selected_install_root(&ctx, &isolated_version)
                .unwrap(),
            Some(isolated.clone())
        );
        assert_eq!(
            backend.bin_names(&ctx, &isolated_version).unwrap(),
            vec!["isolated-bin"]
        );

        let global_version = with_scope(version, ToolScope::Global);
        assert_eq!(
            backend
                .selected_install_root(&ctx, &global_version)
                .unwrap(),
            Some(global.clone())
        );
        assert_eq!(
            backend
                .where_install_root_for(&ctx, &global_version)
                .unwrap(),
            Some(global)
        );
        assert_eq!(
            backend.bin_names(&ctx, &global_version).unwrap(),
            vec!["global-bin"]
        );
    }

    #[test]
    fn scoped_queries_filter_versions_before_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        write_scope_fixture(
            &backend,
            &ctx,
            &npm_test_version(&backend, "3.9.0"),
            ToolScope::Project,
            "isolated-bin",
        );
        write_scope_fixture(
            &backend,
            &ctx,
            &npm_test_version(&backend, "3.8.0"),
            ToolScope::Global,
            "global-bin",
        );

        let global = with_scope(
            npm_test_version(&backend, "identity-selection"),
            ToolScope::Global,
        );
        assert_eq!(
            backend.list_installed_for(&ctx, &global).unwrap(),
            vec!["3.8.0"]
        );

        let project = with_scope(
            npm_test_version(&backend, "identity-selection"),
            ToolScope::Project,
        );
        assert_eq!(
            backend.list_installed_for(&ctx, &project).unwrap(),
            vec!["3.9.0"]
        );
        assert_eq!(
            backend
                .list_installed_for(&ctx, &npm_test_version(&backend, "identity-selection"))
                .unwrap(),
            vec!["3.8.0", "3.9.0"]
        );
    }

    #[test]
    fn invalid_v2_manifest_scope_fails_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let locator = backend
            .install_locator(&ctx, &version, ToolScope::Project)
            .unwrap();
        let isolated = locator.install_root();
        std::fs::create_dir_all(isolated).unwrap();
        let manifest = DynamicToolManifest::from_identity(locator.identity().clone()).unwrap();
        let mut value = serde_json::to_value(manifest).unwrap();
        value["identity"]["scope"] = serde_json::json!("future-scope");
        std::fs::write(
            DynamicToolManifest::manifest_path(isolated),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        std::fs::write(isolated.join(".osdk-complete"), b"").unwrap();

        assert!(backend.where_install_root_for(&ctx, &version).is_err());
        assert!(backend.list_installed_for(&ctx, &version).is_err());
    }

    #[test]
    fn explicit_scope_does_not_fall_back_to_the_other_root() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "isolated-bin");
        let global_version = with_scope(version.clone(), ToolScope::Global);
        assert!(backend
            .selected_install_root(&ctx, &global_version)
            .unwrap()
            .is_none());

        std::fs::remove_dir_all(&isolated).unwrap();
        write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "global-bin");
        let isolated_version = with_scope(version, ToolScope::Project);
        assert!(backend
            .selected_install_root(&ctx, &isolated_version)
            .unwrap()
            .is_none());
    }

    #[test]
    fn project_indirection_wins_over_a_global_direct_key_for_the_same_backend() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let global_key = backend.id().to_string();
        let project_key = "tool.formatter".to_string();
        ctx.config.tools = BTreeMap::from([
            (global_key.clone(), "3.6.2".into()),
            (project_key.clone(), "npm:prettier@3.6.2".into()),
        ]);
        ctx.config.tool_origins = BTreeMap::from([
            (
                global_key.clone(),
                ToolConfigOrigin::GlobalConfig(temporary.path().join("config/config.toml")),
            ),
            (
                project_key,
                ToolConfigOrigin::ProjectConfig(temporary.path().join("project/osdk.toml")),
            ),
        ]);
        ctx.config
            .global_tool_configs
            .insert(global_key, crate::config::ToolConfigEntry::legacy("3.6.2"));
        let version = npm_test_version(&backend, "3.6.2");
        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "isolated-bin");
        write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "global-bin");

        assert_eq!(
            backend.selected_install_root(&ctx, &version).unwrap(),
            Some(isolated)
        );
        assert_eq!(
            backend.bin_names(&ctx, &version).unwrap(),
            vec!["isolated-bin"]
        );
    }

    #[test]
    fn where_query_uses_config_provenance_and_explicit_scope_wins() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let key = backend.id().to_string();
        ctx.config.tools.insert(key.clone(), "3.6.2".into());
        ctx.config.tool_origins.insert(
            key.clone(),
            ToolConfigOrigin::GlobalConfig(temporary.path().join("config/config.toml")),
        );
        ctx.config
            .global_tool_configs
            .insert(key, crate::config::ToolConfigEntry::legacy("3.6.2"));
        let version = npm_test_version(&backend, "3.6.2");
        let isolated =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Project, "isolated-bin");
        let global = write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "global-bin");

        assert_eq!(
            backend.where_install_root_for(&ctx, &version).unwrap(),
            Some(global)
        );

        let mut explicit_project = version;
        explicit_project.options.insert(
            LOCKED_NPM_SCOPE_OPTION.into(),
            ToolScope::Project.as_str().into(),
        );
        assert_eq!(
            backend
                .where_install_root_for(&ctx, &explicit_project)
                .unwrap(),
            Some(isolated)
        );
    }

    fn write_legacy_inventory_fixture(root: &Path, id: &str, version: &str, scope: &str) {
        let bin = root.join("bin/prettier");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"legacy executable").unwrap();
        std::fs::write(
            root.join(crate::inventory::LEGACY_INVENTORY_FILE),
            serde_json::to_vec(&serde_json::json!({
                "schema": 2,
                "id": id,
                "version": version,
                "bins": [{"name": "prettier", "path": "bin/prettier"}],
                "metadata": {"scope": scope}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    #[tokio::test]
    async fn legacy_osdk_tool_inventory_is_never_reused_executed_or_uninstalled() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let legacy_isolated = backend.legacy_isolated_install_root(&ctx, &version.version);
        let legacy_global = backend.legacy_global_install_root_path(&ctx, &version.version);
        write_legacy_inventory_fixture(&legacy_isolated, backend.id(), &version.version, "project");
        write_legacy_inventory_fixture(&legacy_global, backend.id(), &version.version, "global");

        let identity = backend
            .install_identity(&ctx, &version, ToolScope::Project)
            .unwrap();
        assert!(!install_matches(
            &legacy_isolated,
            &identity,
            backend.package(),
            None,
            &BuildPolicy::Deny,
        )
        .unwrap());
        assert!(backend
            .where_install_root_for(&ctx, &version)
            .unwrap()
            .is_none());
        assert!(backend.bin_paths(&ctx, &version).unwrap().is_empty());
        assert!(backend.bin_names(&ctx, &version).is_err());

        let global_version = with_scope(version.clone(), ToolScope::Global);
        assert!(backend
            .where_install_root_for(&ctx, &global_version)
            .unwrap()
            .is_none());
        assert!(backend.bin_paths(&ctx, &global_version).unwrap().is_empty());
        assert!(backend.bin_names(&ctx, &global_version).is_err());
        assert!(backend
            .list_installed_for_scope(&ctx, ToolScope::Project)
            .unwrap()
            .is_empty());
        assert!(backend
            .list_installed_for_scope(&ctx, ToolScope::Global)
            .unwrap()
            .is_empty());
        assert!(backend
            .legacy_global_install_root(&ctx, &global_version)
            .unwrap()
            .is_none());

        backend.uninstall(&ctx, &version).await.unwrap();
        assert!(!backend.uninstall_global(&ctx, &global_version).unwrap());
        assert!(legacy_isolated
            .join(crate::inventory::LEGACY_INVENTORY_FILE)
            .is_file());
        assert!(legacy_global
            .join(crate::inventory::LEGACY_INVENTORY_FILE)
            .is_file());
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
        tool.options.insert(
            LOCKED_NPM_NODE_VERSION_OPTION.into(),
            TEST_NODE_VERSION.into(),
        );
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
        backend: &NpmPackageBackend,
        ctx: &Ctx,
        version: &ToolVersion,
        build_policy: &BuildPolicy,
        lockfile: &str,
    ) -> PathBuf {
        let install_root = backend.isolated_install_root_for(ctx, version).unwrap();
        let project_dir = install_root.join(PROJECT_DIR);
        NpmPackageBackend::write_project_manifest(
            &project_dir,
            Some((backend.package(), &version.version)),
            build_policy,
        )
        .unwrap();
        std::fs::write(project_dir.join(AUBE_LOCKFILE_NAME), lockfile).unwrap();
        std::fs::create_dir_all(package_install_dir(&project_dir, backend.package())).unwrap();
        std::fs::create_dir_all(project_dir.join("node_modules/.bin")).unwrap();
        let bin = project_dir.join("node_modules/.bin/fixture");
        std::fs::write(&bin, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write_node_fixture(ctx, &version.options[LOCKED_NPM_NODE_VERSION_OPTION]);
        let graph_identity =
            npm_graph_identity(&project_dir, backend.package(), &version.version).unwrap();
        let mut manifest = DynamicToolManifest::from_identity(
            backend
                .install_identity(ctx, version, ToolScope::Project)
                .unwrap(),
        )
        .unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: "fixture".into(),
            path: "project/node_modules/.bin/fixture".into(),
        }];
        manifest.write_atomic(&install_root).unwrap();
        write_npm_receipt(
            &install_root,
            &NpmInstallReceipt {
                schema: NPM_INSTALL_RECEIPT_SCHEMA,
                provider: PROVIDER.into(),
                package: backend.package().into(),
                installer: "aube".into(),
                node_version: version.options[LOCKED_NPM_NODE_VERSION_OPTION].clone(),
                build_policy: build_policy.identity(),
                graph_sha256: Some(graph_identity.sha256),
                root_integrity: Some(graph_identity.root_integrity),
                root_source: Some(graph_identity.root_source),
                native_lock_format: None,
                native_lock_sha256: None,
            },
        )
        .unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();
        install_root
    }

    #[test]
    fn completed_install_is_reused_only_for_exact_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let lockfile = npm_test_lockfile(
            "prettier",
            "3.6.2",
            "sha512-I7AIg5boAr5R0FFtJ6rCfD+LFsWHp81dolrFD8S79U9tb8Az2nGrJncnMSnys+bpQJfRUzqs9hnA81OAA3hCuQ==",
        );
        let version = locked_version("npm:prettier", "prettier", "3.6.2", &lockfile);
        let install_root =
            write_reusable_install(&backend, &ctx, &version, &BuildPolicy::Deny, &lockfile);
        let graph = LockedNpmGraph {
            lockfile: &lockfile,
        };
        let identity = backend
            .install_identity(&ctx, &version, ToolScope::Project)
            .unwrap();

        assert!(install_matches(
            &install_root,
            &identity,
            "prettier",
            Some(&graph),
            &BuildPolicy::Deny,
        )
        .unwrap());
        for (tool, package, requested_version, node) in [
            ("npm:typescript", "prettier", "3.6.2", TEST_NODE_VERSION),
            ("npm:prettier", "typescript", "3.6.2", TEST_NODE_VERSION),
            ("npm:prettier", "prettier", "3.6.1", TEST_NODE_VERSION),
            ("npm:prettier", "prettier", "3.6.2", "20.9.0"),
        ] {
            let options = BTreeMap::from([(LOCKED_NPM_NODE_VERSION_OPTION.into(), node.into())]);
            let mismatched_identity = InstallIdentity::new(
                tool,
                requested_version,
                ctx.platform.to_string(),
                InstallScope::Isolated,
                &options,
                vec![InstallDependency {
                    kind: InstallDependencyKind::Runtime,
                    id: "node".into(),
                    version: node.into(),
                    identity: None,
                }],
                identity.materials.clone(),
            )
            .unwrap();
            assert!(!install_matches(
                &install_root,
                &mismatched_identity,
                package,
                Some(&graph),
                &BuildPolicy::Deny,
            )
            .unwrap());
        }
        assert!(!install_matches(
            &install_root,
            &identity,
            "prettier",
            Some(&LockedNpmGraph {
                lockfile: "different"
            }),
            &BuildPolicy::Deny,
        )
        .unwrap());
        let changed_build_identity = InstallIdentity::new(
            backend.id(),
            &version.version,
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &BTreeMap::from([
                (
                    LOCKED_NPM_NODE_VERSION_OPTION.into(),
                    TEST_NODE_VERSION.into(),
                ),
                ("allow_builds".into(), "esbuild".into()),
            ]),
            identity.dependencies.clone(),
            identity.materials.clone(),
        )
        .unwrap();
        assert!(!install_matches(
            &install_root,
            &changed_build_identity,
            "prettier",
            Some(&graph),
            &BuildPolicy::Packages(vec!["esbuild".into()]),
        )
        .unwrap());

        let changed_installer_identity = InstallIdentity::new(
            backend.id(),
            &version.version,
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &BTreeMap::from([("installer".into(), "aube".into())]),
            identity.dependencies.clone(),
            identity.materials.clone(),
        )
        .unwrap();
        assert!(!install_matches(
            &install_root,
            &changed_installer_identity,
            "prettier",
            Some(&graph),
            &BuildPolicy::Deny,
        )
        .unwrap());
        let default_spelling_identity = InstallIdentity::new(
            backend.id(),
            &version.version,
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &BTreeMap::from([("allow_builds".into(), "false".into())]),
            identity.dependencies.clone(),
            identity.materials.clone(),
        )
        .unwrap();
        assert!(install_matches(
            &install_root,
            &default_spelling_identity,
            "prettier",
            Some(&graph),
            &BuildPolicy::Deny,
        )
        .unwrap());
    }

    #[test]
    fn unlocked_reuse_validates_persisted_graph_and_root_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let lockfile = npm_test_lockfile("prettier", "3.6.2", "sha512-root-integrity");
        let version = npm_test_version(&backend, "3.6.2");
        let install_root =
            write_reusable_install(&backend, &ctx, &version, &BuildPolicy::Deny, &lockfile);
        let identity = backend
            .install_identity(&ctx, &version, ToolScope::Project)
            .unwrap();
        assert!(install_matches(
            &install_root,
            &identity,
            "prettier",
            None,
            &BuildPolicy::Deny,
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
            &identity,
            "prettier",
            None,
            &BuildPolicy::Deny,
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
        assert!(backend.locked_graph(&partial).unwrap().is_none());

        partial.options.insert(
            LOCKED_NPM_LOCK_FORMAT_OPTION.into(),
            AUBE_LOCK_FORMAT.into(),
        );
        assert!(backend
            .locked_graph(&partial)
            .unwrap_err()
            .to_string()
            .contains(LOCKED_NPM_LOCK_SHA256_OPTION));

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
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
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
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
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
        let version = npm_test_version(&backend, "3.6.2");
        let install_root = backend.isolated_install_root_for(&ctx, &version).unwrap();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("without a locked npm dependency graph"));
        assert!(!install_root.exists());
    }

    #[tokio::test]
    async fn marker_only_offline_install_is_not_reused() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let install_root = backend.isolated_install_root_for(&ctx, &version).unwrap();
        std::fs::create_dir_all(&install_root).unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("managed Node 20.10.0 has no executable bin directory"));
        assert!(!DynamicToolManifest::manifest_path(&install_root).exists());
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
    fn discovers_project_and_global_bins_through_aliased_install_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let physical_parent = temporary.path().join("physical");
        let aliased_parent = temporary.path().join("aliased");
        std::fs::create_dir_all(&physical_parent).unwrap();
        symlink(&physical_parent, &aliased_parent).unwrap();

        let install_root = aliased_parent.join("install");
        let physical_install_root = physical_parent.join("install");
        let package_dir = physical_install_root.join("project/node_modules/prettier/bin");
        let project_bin_dir = physical_install_root.join("project/node_modules/.bin");
        let global_bin_dir = physical_install_root.join("bin");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&project_bin_dir).unwrap();
        std::fs::create_dir_all(&global_bin_dir).unwrap();
        let script = package_dir.join("prettier.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
        symlink(
            "../prettier/bin/prettier.js",
            project_bin_dir.join("prettier"),
        )
        .unwrap();
        symlink(
            "../project/node_modules/prettier/bin/prettier.js",
            global_bin_dir.join("prettier"),
        )
        .unwrap();

        assert_eq!(
            discover_bins(&install_root, &project_bin_dir).unwrap(),
            vec![DynamicToolBin {
                name: "prettier".into(),
                path: "project/node_modules/prettier/bin/prettier.js".into(),
            }]
        );
        assert_eq!(
            discover_global_bins(&install_root, &global_bin_dir).unwrap(),
            vec![DynamicToolBin {
                name: "prettier".into(),
                path: "bin/prettier".into(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_inventory_drives_native_bin_paths_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = with_scope(npm_test_version(&backend, "3.6.2"), ToolScope::Global);
        let install_root =
            write_scope_fixture(&backend, &ctx, &version, ToolScope::Global, "prettier");
        let bin_dir = install_root.join("bin");

        assert_eq!(backend.bin_paths(&ctx, &version).unwrap(), vec![bin_dir]);
        assert_eq!(
            backend.bin_names(&ctx, &version).unwrap(),
            vec!["prettier".to_string()]
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

        let global_bin_dir = install_root.join("bin");
        std::fs::create_dir_all(&global_bin_dir).unwrap();
        symlink(&outside_script, global_bin_dir.join("prettier")).unwrap();
        let error = discover_global_bins(&install_root, &global_bin_dir).unwrap_err();
        assert!(error.to_string().contains("outside install root"));
    }

    #[test]
    fn manifest_records_install_identity_and_receipt_records_graph_evidence() {
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
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version_with_node(&backend, "3.0.0", "24.0.0");
        let install_root = backend.isolated_install_root_for(&ctx, &version).unwrap();
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
                &install_root,
                &bin_dir,
                "24.0.0",
                &BuildPolicy::Deny,
                &graph_identity,
            )
            .unwrap();

        assert_eq!(
            manifest.identity,
            backend
                .install_identity(&ctx, &version, ToolScope::Project)
                .unwrap()
        );
        assert_eq!(manifest.schema, 1);
        let receipt = load_npm_receipt(&install_root).unwrap();
        assert_eq!(
            receipt,
            NpmInstallReceipt {
                schema: 1,
                provider: PROVIDER.into(),
                package: "prettier".into(),
                installer: "aube".into(),
                node_version: "24.0.0".into(),
                build_policy: "deny".into(),
                graph_sha256: Some("graph-sha256".into()),
                root_integrity: Some("sha512-root-integrity".into()),
                root_source: Some("npm-registry".into()),
                native_lock_format: None,
                native_lock_sha256: None,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn finalize_global_install_at_publishes_only_the_supplied_root() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let ctx = offline_test_ctx(temporary.path());
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = npm_test_version(&backend, "3.6.2");
        let staging = temporary.path().join("stage");
        let package = staging.join("project/node_modules/prettier");
        let bin_dir = staging.join("bin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"prettier","version":"3.6.2"}"#,
        )
        .unwrap();
        let executable = bin_dir.join("prettier");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let manifest = backend
            .finalize_global_install_at(
                &ctx,
                &version,
                &staging,
                &bin_dir,
                "20.10.0",
                "aube",
                Some(("aube-v9", "digest")),
            )
            .unwrap();

        assert_eq!(manifest.identity.scope, InstallScope::Global);
        assert_eq!(
            manifest.identity,
            backend
                .install_identity(&ctx, &version, ToolScope::Global)
                .unwrap()
        );
        assert!(DynamicToolManifest::manifest_path(&staging).is_file());
        assert!(staging.join(".osdk-complete").is_file());
        assert_eq!(
            load_npm_receipt(&staging).unwrap(),
            NpmInstallReceipt {
                schema: 1,
                provider: PROVIDER.into(),
                package: "prettier".into(),
                installer: "aube".into(),
                node_version: TEST_NODE_VERSION.into(),
                build_policy: "deny".into(),
                graph_sha256: None,
                root_integrity: None,
                root_source: None,
                native_lock_format: Some("aube-v9".into()),
                native_lock_sha256: Some("digest".into()),
            }
        );
        assert!(!backend
            .global_install_root_for(&ctx, &version)
            .unwrap()
            .exists());
        assert!(!backend
            .isolated_install_root_for(&ctx, &version)
            .unwrap()
            .exists());
    }
}
