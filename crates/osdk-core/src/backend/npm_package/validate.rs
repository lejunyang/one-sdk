//! `validate` free functions split from npm_package.rs.

use super::*;

/// npm has no per-package build allowlist, so a `Packages(..)` policy cannot
/// be expressed and stays fail-closed: only an explicit allow-all lets
/// lifecycle scripts run.
pub(crate) fn script_policy(policy: &BuildPolicy) -> ScriptPolicy {
    match policy {
        BuildPolicy::AllowAll => ScriptPolicy::Allow,
        BuildPolicy::Deny | BuildPolicy::Packages(_) => ScriptPolicy::Deny,
    }
}

pub(crate) fn managed_node(ctx: &Ctx, npm_tool: &ToolVersion) -> Result<(PathBuf, String)> {
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

pub(crate) fn selected_node_version(ctx: &Ctx) -> Option<String> {
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

pub(crate) fn inventory_validation_candidate(
    manifest: &DynamicToolManifest,
) -> Option<ToolVersion> {
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

pub(crate) fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

#[cfg(unix)]
pub(crate) fn is_runnable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    is_regular_file(path)
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
pub(crate) fn is_runnable_file(path: &Path) -> bool {
    is_regular_file(path)
}

pub(crate) fn receipt_matches_identity(
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
    let expected_installer = locked_installer
        .or(requested_installer)
        .unwrap_or(ISOLATED_INSTALLER);
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

pub(crate) fn manifest_bins_are_confined(
    install_root: &Path,
    manifest: &DynamicToolManifest,
) -> Result<bool> {
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

pub(crate) fn validate_isolated_install_evidence(
    install_root: &Path,
    package: &str,
    version: &str,
    build_policy: &BuildPolicy,
    receipt: &NpmInstallReceipt,
    locked_graph: Option<&LockedNpmGraph<'_>>,
) -> Result<bool> {
    // Isolated installs are assembled in a synthetic project and record their
    // graph digest instead of a native lock, so a receipt carrying native-lock
    // fields belongs to a global install and must not be reused here.
    if receipt.installer != ISOLATED_INSTALLER
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

pub(crate) fn validate_global_install_evidence(
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

pub(crate) fn global_package_manifest_path(
    install_root: &Path,
    package: &str,
    installer: NpmInstaller,
) -> Option<PathBuf> {
    match installer {
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

pub(crate) fn package_manifest_matches(path: &Path, package: &str, version: &str) -> Result<bool> {
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

pub(crate) fn global_native_lock_path(
    install_root: &Path,
    installer: NpmInstaller,
) -> Option<PathBuf> {
    match installer {
        NpmInstaller::Pnpm => {
            find_unique_descendant(&install_root.join("pnpm-global"), "pnpm-lock.yaml")
        }
        // `npm install --global` resolves against the prefix and never writes a
        // lockfile, so a global npm install has no native lock to record.
        NpmInstaller::Npm | NpmInstaller::Auto => None,
    }
}

pub(crate) fn find_unique_descendant(root: &Path, suffix: &str) -> Option<PathBuf> {
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

pub(crate) fn native_lock_format_matches(
    installer: NpmInstaller,
    format: &str,
    path: &Path,
) -> bool {
    let expected_name = match installer {
        NpmInstaller::Pnpm => "pnpm-lock.yaml",
        NpmInstaller::Npm | NpmInstaller::Auto => return false,
    };
    if path.file_name().and_then(std::ffi::OsStr::to_str) != Some(expected_name) {
        return false;
    }
    match installer {
        NpmInstaller::Pnpm => format == "pnpm-v9",
        NpmInstaller::Npm | NpmInstaller::Auto => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_matches(
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

pub(crate) fn validate_project_manifest(
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
        .get("osdk")
        .and_then(|osdk| osdk.get("allowBuilds"))
        .and_then(serde_json::Value::as_object);
    if actual_allow_builds != expected_allow_builds.as_ref() {
        return Err(Error::other(crate::t!(
            "err.npm_project_manifest_build_policy_mismatch",
            package = package
        )));
    }
    Ok(())
}

pub(crate) fn npm_graph_identity(
    project_dir: &Path,
    package: &str,
    version: &str,
) -> Result<NpmGraphIdentity> {
    let path = project_dir.join(NPM_LOCKFILE_NAME);
    let bytes = crate::inventory::read_stable_regular_file(&path, NPM_NATIVE_LOCK_MAX_BYTES)
        .map_err(|error| Error::io(&path, error))?;
    let lockfile: IdentityLockfile = serde_json::from_slice(&bytes).map_err(|error| {
        Error::other(crate::t!(
            "err.npm_graph_parse_invalid",
            path = path.display(),
            error = error
        ))
    })?;
    // v2 mirrors the tree into a legacy `dependencies` table as well; v3 is
    // `packages`-only. Both keep the fields read below, so either is accepted
    // and anything newer fails closed rather than being guessed at.
    if !matches!(lockfile.lockfile_version, 2 | 3) {
        return Err(Error::other(crate::t!(
            "err.npm_graph_lock_version_unsupported",
            path = path.display(),
            version = lockfile.lockfile_version
        )));
    }
    // npm keys a top-level dependency by its install path. The root project is
    // `""`, which is deliberately not what is wanted here: the tool itself is a
    // dependency of the synthetic project.
    let package_key = format!("node_modules/{package}");
    let root = lockfile
        .packages
        .get(&package_key)
        .ok_or_else(|| Error::other(crate::t!("err.npm_graph_root_missing", package = package)))?;
    let locked_version = root.version.as_deref().unwrap_or_default();
    if locked_version != version {
        return Err(Error::other(crate::t!(
            "err.npm_graph_root_version_mismatch",
            package = package,
            expected = version,
            actual = locked_version
        )));
    }
    let root_integrity = root
        .integrity
        .as_deref()
        .map(str::trim)
        .filter(|integrity| !integrity.is_empty())
        .ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_graph_root_integrity_missing",
                package = package,
                version = version
            ))
        })?
        .to_string();
    // `resolved` is the tarball URL for a registry dependency. It is absent for
    // link/workspace entries, so the canonical spec is used as the fallback.
    let root_tarball = root
        .resolved
        .as_deref()
        .map(str::trim)
        .filter(|resolved| !resolved.is_empty())
        .map(str::to_string);
    Ok(NpmGraphIdentity {
        sha256: pipeline::verify::hash_bytes(&bytes, pipeline::HashAlgo::Sha256),
        root_integrity,
        root_source: root_tarball
            .clone()
            .unwrap_or_else(|| format!("npm:{package}@{version}")),
        root_tarball,
    })
}

pub(crate) fn validate_unlocked_graph_identity(
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

pub(crate) fn format_checksum(checksum: &pipeline::Checksum) -> String {
    format!("{}:{}", checksum.algo.token(), checksum.hex)
}

#[cfg(windows)]
pub(crate) fn node_executable_name() -> &'static str {
    "node.exe"
}

#[cfg(not(windows))]
pub(crate) fn node_executable_name() -> &'static str {
    "node"
}

pub(crate) fn valid_npm_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 214
        && value != "."
        && value != ".."
        && !is_windows_reserved_component(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

pub(crate) fn is_windows_reserved_component(value: &str) -> bool {
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
