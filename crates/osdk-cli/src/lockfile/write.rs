//! `write` free functions split from lockfile.rs.

use super::*;

/// Drop a model entry from a project's lock, preserving everything else.
///
/// Needed because `model remove` deletes local snapshots but left the lock
/// claiming them, so the next `model sync` would faithfully pull back exactly
/// what the user had just removed. Pruning is per entry rather than a rewrite of
/// the section: a lock also holds other platforms' tools and other models, and a
/// stale entry is not a reason to touch them.
pub fn remove_model(path: &Path, name: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut lockfile = load(path)?;
    if lockfile.models.remove(name).is_none() {
        return Ok(false);
    }
    save(path, &lockfile)?;
    Ok(true)
}

pub(crate) fn inject_npm_metadata(options: &mut BTreeMap<String, String>, npm: &LockedNpmMetadata) {
    options.insert(LOCKED_NPM_PACKAGE_OPTION.into(), npm.package.clone());
    options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        npm.installer.as_str().into(),
    );
    options.insert(LOCKED_NPM_SCOPE_OPTION.into(), npm.scope.as_str().into());
    if let Some(node_version) = &npm.node_version {
        options.insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.clone());
    }
    if let Some(native_lock) = &npm.native_lock {
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            native_lock.kind.as_str().into(),
        );
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            native_lock.format.clone(),
        );
        options.insert(
            LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
            native_lock.sha256.clone(),
        );
    }
}

pub(crate) fn inject_native_metadata(
    backend: &str,
    options: &mut BTreeMap<String, String>,
    native: &LockedNativeTool,
) {
    options.insert(LOCKED_NATIVE_RUNTIME_OPTION.into(), native.runtime.clone());
    options.insert(
        LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(),
        native.runtime_version.clone(),
    );
    options.insert(
        LOCKED_NATIVE_REPLAY_OPTION.into(),
        native.replay.as_str().into(),
    );
    if let Some(source) = &native.source {
        let key = if backend.starts_with("go:") {
            osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION
        } else {
            osdk_core::backend::cargo_package::LOCKED_CARGO_INDEX_OPTION
        };
        options.insert(key.into(), source.clone());
    }
    if let Some(module) = &native.module {
        options.insert(
            osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION.into(),
            module.clone(),
        );
    }
}

pub(crate) fn installed_artifact_receipt(
    dirs: &osdk_core::dirs::Dirs,
    platform: Platform,
    version: &ToolVersion,
) -> Result<Option<osdk_core::pipeline::ArtifactReceipt>> {
    if !version.backend.starts_with("github:") && !version.backend.starts_with("http:") {
        return Ok(osdk_core::pipeline::artifact_receipt(
            dirs,
            &version.backend,
            &version.version,
        ));
    }
    if version.backend.starts_with("http:") {
        if osdk_core::pipeline::locked_artifact(version)?.is_some() {
            let locator = osdk_core::backend::http::HttpBackend::install_locator_for(
                dirs,
                platform,
                &version.backend,
                version,
            )?;
            let root = locator.install_root();
            if !root.exists() {
                return Ok(None);
            }
            if !osdk_core::backend::http::HttpBackend::install_candidate_is_valid(
                dirs,
                root,
                locator.identity(),
            )? {
                anyhow::bail!(
                    "cannot lock {}@{} because its exact HTTP install is incomplete or invalid",
                    version.backend,
                    version.version
                );
            }
            return Ok(osdk_core::pipeline::artifact_receipt_at(root));
        }
        let expected_options =
            osdk_core::backend::dynamic::identity_options(&version.backend, &version.options)?;
        let report = osdk_core::inventory::scan_installs(
            &dirs.installs,
            &osdk_core::inventory::ScanOptions::default(),
        )?;
        let mut candidates = Vec::new();
        for install in report.installs {
            let identity = &install.manifest.identity;
            if identity.tool != version.backend
                || identity.version != version.version
                || identity.platform != platform.to_string()
                || identity.scope != osdk_core::tool::InstallScope::Isolated
                || identity.material_options != expected_options
            {
                continue;
            }
            match osdk_core::backend::http::HttpBackend::install_candidate_is_valid(
                dirs,
                &install.install_root,
                identity,
            ) {
                Ok(true) => candidates.push(install.install_root),
                Ok(false) => {}
                Err(error) => return Err(anyhow::Error::new(error)),
            }
        }
        return match candidates.as_slice() {
            [] => Ok(None),
            [root] => Ok(osdk_core::pipeline::artifact_receipt_at(root)),
            _ => anyhow::bail!(
                "cannot lock {}@{} because multiple complete HTTP artifact identities match; remove the unwanted variant or provide exact locked artifact metadata",
                version.backend,
                version.version
            ),
        };
    }
    if osdk_core::pipeline::locked_artifact(version)?.is_some() {
        let locator = osdk_core::backend::github::github_install_locator_for(
            dirs,
            platform,
            &version.backend,
            version,
        )?;
        let root = locator.install_root();
        if !root.exists() {
            return Ok(None);
        }
        if !osdk_core::backend::github::github_install_candidate_is_valid_for_dirs(
            dirs,
            root,
            locator.identity(),
        )? {
            anyhow::bail!(
                "cannot lock {}@{} because its exact GitHub install is incomplete or invalid",
                version.backend,
                version.version
            );
        }
        return Ok(osdk_core::pipeline::artifact_receipt_at(root));
    }

    let expected_options =
        osdk_core::backend::dynamic::identity_options(&version.backend, &version.options)?;
    let report = osdk_core::inventory::scan_installs(
        &dirs.installs,
        &osdk_core::inventory::ScanOptions::default(),
    )?;
    let mut candidates = Vec::new();
    for install in report.installs {
        let identity = &install.manifest.identity;
        if identity.tool != version.backend
            || identity.version != version.version
            || identity.platform != platform.to_string()
            || identity.scope != osdk_core::tool::InstallScope::Isolated
            || identity.material_options != expected_options
        {
            continue;
        }
        match osdk_core::backend::github::github_install_candidate_is_valid_for_dirs(
            dirs,
            &install.install_root,
            identity,
        ) {
            Ok(true) => candidates.push(install.install_root),
            Ok(false) => {}
            Err(error) => return Err(anyhow::Error::new(error)),
        }
    }
    match candidates.as_slice() {
        [] => Ok(None),
        [root] => Ok(osdk_core::pipeline::artifact_receipt_at(root)),
        _ => anyhow::bail!(
            "cannot lock {}@{} because multiple complete GitHub artifact identities match; remove the unwanted variant or provide exact locked artifact metadata",
            version.backend,
            version.version
        ),
    }
}

pub fn merge_resolved(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    merge_resolved_with_scope(path, platform, dirs, resolved, LockScope::Project)
}

pub fn merge_resolved_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
    scope: LockScope,
) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
        }
    };
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
    }
    let node_version = resolved
        .iter()
        .rev()
        .find_map(|(_, version)| (version.backend == "node").then(|| version.version.clone()));
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    platform_lock.tools.clear();
    for (request, version) in resolved {
        reject_linked_rust(dirs, version)?;
        let npm_metadata = locked_npm_metadata(dirs, version, node_version.as_deref(), scope)?;
        let mut options = lock_options(version)?;
        if npm_metadata.is_some() {
            options.remove("node_version");
        }
        let artifact = locked_artifact_for(dirs, platform, version)?;
        platform_lock.tools.insert(
            request.backend.clone(),
            LockedTool {
                request: request.spec.to_string(),
                version: version.version.clone(),
                options,
                artifact,
                npm: npm_metadata.map(LockedNpmGraph::Metadata),
                native: locked_native_metadata(version)?,
                pypi: locked_pypi_metadata(dirs, platform, version),
                conda: locked_conda_metadata(dirs, platform, version)?,
            },
        );
    }
    save(path, &lockfile)
}

#[allow(dead_code)]
pub fn upsert_resolved_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    request: &ToolRequest,
    version: &ToolVersion,
    scope: LockScope,
) -> Result<()> {
    upsert_resolved_many_with_scope(
        path,
        platform,
        dirs,
        std::slice::from_ref(&(request.clone(), version.clone())),
        scope,
    )
}

/// Upsert several exact tool identities and atomically save the lock once.
pub fn upsert_resolved_many_with_scope(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
    scope: LockScope,
) -> Result<()> {
    for (request, version) in resolved {
        reject_linked_rust(dirs, version)?;
        if request.backend != version.backend {
            anyhow::bail!(
                "cannot lock request `{}` with resolved backend `{}`",
                request.backend,
                version.backend
            );
        }
    }
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
    }
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    let node_version = resolved
        .iter()
        .rev()
        .find_map(|(_, version)| (version.backend == "node").then(|| version.version.clone()))
        .or_else(|| {
            platform_lock
                .tools
                .get("node")
                .map(|node| node.version.clone())
        });
    for (request, version) in resolved {
        let effective_node = if version.backend == "node" {
            Some(version.version.as_str())
        } else {
            version
                .options
                .get(LOCKED_NPM_NODE_VERSION_OPTION)
                .map(String::as_str)
                .or(node_version.as_deref())
        };
        let npm_metadata = locked_npm_metadata(dirs, version, effective_node, scope)?;
        let mut options = lock_options(version)?;
        if npm_metadata.is_some() {
            options.remove("node_version");
        }
        let artifact = locked_artifact_for(dirs, platform, version)?;
        platform_lock.tools.insert(
            request.backend.clone(),
            LockedTool {
                request: request.spec.to_string(),
                version: version.version.clone(),
                options,
                artifact,
                npm: npm_metadata.map(LockedNpmGraph::Metadata),
                native: locked_native_metadata(version)?,
                pypi: locked_pypi_metadata(dirs, platform, version),
                conda: locked_conda_metadata(dirs, platform, version)?,
            },
        );
    }
    save(path, &lockfile)
}

/// Remove one tool entry for a platform while preserving every other tool,
/// platform, and model record. Empty platform tables are pruned.
#[allow(dead_code)]
pub fn remove_tool(path: &Path, platform: Platform, backend: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut lockfile = load(path)?;
    let key = platform_key(platform);
    let removed = lockfile
        .platforms
        .get_mut(&key)
        .is_some_and(|platform| platform.tools.remove(backend).is_some());
    if !removed {
        return Ok(false);
    }
    if lockfile
        .platforms
        .get(&key)
        .is_some_and(|platform| platform.tools.is_empty())
    {
        lockfile.platforms.remove(&key);
    }
    save(path, &lockfile)?;
    Ok(true)
}

/// Record a materialized application environment in the lock.
///
/// Paths are stored relative to the lock and normalized to `/`: this value gets
/// committed and read on other platforms, where `\` is not a separator at all.
/// Digests come from the files as they are on disk right now, after the install,
/// so the entry describes what was actually consumed rather than what was
/// intended.
pub fn merge_deps(path: &Path, provider: &str, record: DepsRecord<'_>) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    let lock_dir = path.parent().unwrap_or(Path::new("."));

    let manifest_sha256 = osdk_core::deps::file_sha256(record.manifest)?;
    // The kind comes from the file name via the provider table, so a provider
    // added later cannot end up mislabelled here.
    let mut native_lock = None;
    if let Some(lock) = record.native_lock {
        if let Some(kind) = osdk_core::deps::native_lock_kind(lock) {
            native_lock = Some(LockedDepsNativeLock {
                kind: kind.to_string(),
                path: relative_to_lock(lock_dir, lock),
                sha256: osdk_core::deps::file_sha256(lock)?,
            });
        }
    }

    lockfile.deps.insert(
        provider.to_string(),
        LockedDeps {
            installer: record.installer,
            installer_version: record.installer_version,
            runtime: record.runtime,
            manifest: relative_to_lock(lock_dir, record.manifest),
            manifest_sha256,
            native_lock,
            run: record.run,
            index: record.index,
            allow_build_from_source: record.allow_build_from_source,
        },
    );
    let _ = record.project_root;
    save(path, &lockfile)
}

/// A path relative to the lock's directory, with `/` separators.
///
/// Falls back to the normalized absolute path when the target is not under the
/// lock -- recording something wrong would be worse than recording something
/// verbose, and a path outside the project is a real (if unusual) configuration.
pub(crate) fn relative_to_lock(lock_dir: &Path, target: &Path) -> String {
    let relative = target.strip_prefix(lock_dir).unwrap_or(target);
    relative.to_string_lossy().replace('\\', "/")
}

pub fn merge_model(path: &Path, manifest: &osdk_core::model::SnapshotManifest) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile::default()
    };
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(&lockfile)?;
    }
    lockfile.models.insert(
        manifest.name.clone(),
        LockedModel {
            provider: manifest.provider,
            repository: manifest.repository.clone(),
            requested_revision: manifest.requested_revision.clone(),
            revision: manifest.revision.clone(),
            // Same reason the tool artifact URL is normalized: `--endpoint` and
            // `HF_ENDPOINT` exist so *this* machine can reach a provider through
            // a mirror, and recording that choice made it everybody's. A model
            // reference is provider + repository + immutable revision; the host
            // that served those bytes is not part of the identity, and the file
            // digests already pin the content. So a mirror endpoint is recorded
            // as the provider's own, and anything osdk cannot map to a provider
            // is left as-is rather than reattributed.
            endpoint: osdk_core::model::source::canonical_provider_endpoint(
                manifest.provider,
                &manifest.endpoint,
            ),
            variant: manifest.variant.clone(),
            files: manifest
                .files
                .iter()
                .map(|file| {
                    Ok(LockedModelFile {
                        path: file.path.clone(),
                        size: file.size,
                        sha256: file.sha256.clone().ok_or_else(|| {
                            anyhow::anyhow!("model lock requires SHA-256 for {}", file.path)
                        })?,
                    })
                })
                .collect::<Result<_>>()?,
            views: BTreeMap::new(),
        },
    );
    save(path, &lockfile)
}

/// Record the consumer views declared for one model into an existing lock
/// entry. Kept separate from [`merge_model`] so a bare `model pull` (no
/// declaration) leaves `views` empty and existing callers/tests stay simple,
/// while a pull driven by `[models.<name>.views]` can persist the declaration.
///
/// This is the read/write symmetry AGENTS.md demands: the field is not
/// write-only. `model sync` reads it back (`locked_models`) so a replay on
/// another machine knows which views to rebuild.
pub fn set_model_views(
    path: &Path,
    name: &str,
    views: BTreeMap<String, LockedModelView>,
) -> Result<bool> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        return Ok(false);
    };
    let Some(entry) = lockfile.models.get_mut(name) else {
        return Ok(false);
    };
    if entry.views == views {
        return Ok(false);
    }
    entry.views = views;
    save(path, &lockfile)?;
    Ok(true)
}

/// Build the lock form of a model's view declarations from resolved config,
/// keyed the same way the lock stores them (consumer -> view entry). Shared by
/// the pull path so config and lock never encode the mapping differently.
pub fn locked_views_from_declaration(
    declaration_views: &BTreeMap<String, osdk_core::config::ModelViewDeclaration>,
) -> BTreeMap<String, LockedModelView> {
    declaration_views
        .iter()
        .map(|(consumer, view)| {
            (
                consumer.clone(),
                LockedModelView {
                    profile: view.profile.clone(),
                    map: view.map.clone(),
                },
            )
        })
        .collect()
}

/// The artifact section for one resolved tool, with its URL pointing upstream.
///
/// The pipeline records whichever candidate URL actually downloaded, which on a
/// mirrored network is a mirror. That is right for the on-disk receipt -- it
/// documents what this machine did -- but wrong for `osdk.lock`, which is
/// committed and replayed elsewhere: it turned one machine's fastest host into
/// everybody's locked source. Observed in a lock generated here: go pinned to
/// `golang.google.cn`, java to `gh-proxy.com/https://github.com/...`.
///
/// Rewriting happens only through mirror/upstream pairs the backends declare
/// about themselves, so a custom source -- which osdk cannot map to any upstream
/// -- is left exactly as recorded rather than reattributed to a host that never
/// served it. The checksum is untouched: mirrors serve the same bytes, and if one
/// does not, that is precisely what the checksum exists to catch.
pub(crate) fn locked_artifact_for(
    dirs: &osdk_core::dirs::Dirs,
    platform: Platform,
    version: &ToolVersion,
) -> Result<Option<LockedArtifact>> {
    if version.backend.starts_with("npm:") {
        return Ok(None);
    }
    let subdir = version.options.get("catalog-subdir").cloned();
    let build = |receipt: osdk_core::pipeline::ArtifactReceipt| LockedArtifact {
        url: receipt.url,
        file_name: receipt.file_name,
        checksum: receipt.checksum,
        subdir: subdir.clone(),
        evidence: receipt.evidence,
    };
    let resolved = osdk_core::pipeline::locked_artifact(version)?.map(build);
    let artifact = installed_artifact_receipt(dirs, platform, version)?
        .map(build)
        .or(resolved);
    Ok(artifact.map(|mut artifact| {
        artifact.url = osdk_core::source::canonical_upstream_url(
            &artifact.url,
            &mirror_upstream_pairs_for_lock(dirs),
        );
        artifact
    }))
}

/// Mirror pairs for URL normalization, loaded once per process.
///
/// `Registry::load` reads the declarative backend directory, so calling it per
/// tool would re-read it for every entry in the lock. The set is a property of
/// the installed backends, not of any one entry.
pub(crate) fn mirror_upstream_pairs_for_lock(
    dirs: &osdk_core::dirs::Dirs,
) -> Vec<(String, String)> {
    static PAIRS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    PAIRS
        .get_or_init(
            || match osdk_core::backend::registry::Registry::load(dirs) {
                Ok(registry) => osdk_core::source::select::all_mirror_upstream_pairs(&registry),
                // A registry that cannot be read is not a reason to fail writing a
                // lock; it only means no rewriting is available, so URLs stay as
                // recorded rather than being silently mangled.
                Err(_) => Vec::new(),
            },
        )
        .clone()
}

pub(crate) fn public_options(options: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    options
        .iter()
        .filter(|(key, _)| {
            !key.starts_with("__osdk_")
                && key.as_str() != "catalog-url"
                && !CONSENT_OPTIONS.contains(&key.as_str())
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub(crate) fn lock_options(version: &ToolVersion) -> Result<BTreeMap<String, String>> {
    if version.backend.starts_with("go:") {
        return osdk_core::backend::dynamic::identity_options(&version.backend, &version.options)
            .map_err(anyhow::Error::from);
    }
    Ok(public_options(&version.options))
}

/// Read the installer identity of a `pypi:` entry being locked.
///
/// Returns `None` for every other backend, and for a pypi install whose
/// environment receipt cannot be read -- a lockfile entry without this section is
/// the pre-existing behaviour and stays valid, so a missing receipt must not fail
/// the whole lock.
///
/// A replayed request already carries the recorded installer, and that outranks
/// anything this machine can observe. Otherwise re-running `lock` on a machine
/// without uv would rewrite `installer = "uv"` to `"pip"` and commit the weaker
/// fact -- the recorded promise quietly replaced by the local situation.
///
/// Failing that, the facts come from the receipt osdk writes next to the
/// environment (`.osdk-pypi-env.json`) rather than from a fresh probe: the
/// question is what built *this* environment, which a later probe cannot answer.
pub(crate) fn locked_pypi_metadata(
    dirs: &osdk_core::dirs::Dirs,
    platform: osdk_core::platform::Platform,
    version: &ToolVersion,
) -> Option<LockedPypiTool> {
    if !version.backend.starts_with("pypi:") {
        return None;
    }

    // Preserve what a replay carried in. This is also where `PypiInstaller::parse`
    // earns its place: clippy reported it as never used, and the honest reading of
    // that warning was not "delete it" but "the read-back path is missing". npm has
    // had this from the start -- see `locked_npm_metadata`.
    if let Some(recorded) = version.options.get(LOCKED_PYPI_INSTALLER_OPTION) {
        let installer = PypiInstaller::parse(recorded).ok()?;
        return Some(LockedPypiTool {
            installer,
            uv_version: version.options.get(LOCKED_PYPI_UV_VERSION_OPTION).cloned(),
            python_version: version
                .options
                .get(LOCKED_PYPI_PYTHON_VERSION_OPTION)
                .cloned(),
        });
    }

    // Only the host platform's installs are on disk to inspect. Locking for
    // another platform must not silently borrow this machine's installer.
    if platform != osdk_core::platform::Platform::current() {
        return None;
    }

    let receipt = read_pypi_env_receipt(dirs, version)?;
    let installer = match receipt.creator {
        osdk_core::backend::pypi::EnvCreator::Uv => PypiInstaller::Uv,
        osdk_core::backend::pypi::EnvCreator::Stdlib => PypiInstaller::Pip,
    };
    // The interpreter path ends in the version directory osdk installed it
    // under, which is where the minor version comes from.
    let python_version = python_version_from_interpreter(&receipt.interpreter);
    let uv_version = match installer {
        PypiInstaller::Uv => installed_uv_version(dirs),
        PypiInstaller::Pip => None,
    };
    Some(LockedPypiTool {
        installer,
        uv_version,
        python_version,
    })
}

/// Load the environment receipt for an installed pypi tool.
pub(crate) fn read_pypi_env_receipt(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
) -> Option<osdk_core::backend::pypi::EnvReceipt> {
    // A dynamic install lives one level below the version directory, under its
    // install-id fingerprint. Reading the version directory directly finds
    // nothing -- the same off-by-one-level mistake that has bitten the install
    // root, `list_installed`, and uv discovery in turn.
    let root = dirs.install_path(&version.backend, &version.version);
    let entries = std::fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry
            .path()
            .join(osdk_core::backend::pypi::ENV_RECEIPT_FILE);
        let Ok(text) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        if let Ok(receipt) = serde_json::from_str::<osdk_core::backend::pypi::EnvReceipt>(&text) {
            return Some(receipt);
        }
    }
    None
}

/// Pull the interpreter version out of an osdk-managed interpreter path.
///
/// osdk installs interpreters at `installs/python/<version>/...`, so the segment
/// after `python` is the version. A path outside that layout (a user-supplied
/// interpreter) yields `None` rather than a guess.
///
/// Both separators are split on rather than going through `Path::components`,
/// which honours only the host's own. A receipt records the interpreter path of
/// whichever machine built the environment, and a lockfile is meant to be read on
/// another: on Unix a recorded Windows path is a single component, so the version
/// segment was invisible and such an entry reported no version at all. osdk's
/// install layout is identical on both platforms, so the segment rule below is
/// what decides, not the host that happens to be reading.
pub(crate) fn python_version_from_interpreter(interpreter: &str) -> Option<String> {
    let mut components = interpreter
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .peekable();
    while let Some(component) = components.next() {
        if component != "python" {
            continue;
        }
        let next = *components.peek()?;
        // Guard against matching a directory that merely sits next to a file
        // called `python`: the following segment has to look like a version.
        if next.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return Some(next.to_string());
        }
    }
    None
}

/// The version of the uv that osdk manages, if one is installed.
pub(crate) fn installed_uv_version(dirs: &osdk_core::dirs::Dirs) -> Option<String> {
    let root = dirs.data.join("installs").join("pypi").join("uv");
    let mut newest: Option<String> = None;
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_str()?.to_string();
        newest = match newest {
            None => Some(name),
            Some(current) => {
                if osdk_core::backend::pypi::compare_pep440(&name, &current)
                    == std::cmp::Ordering::Greater
                {
                    Some(name)
                } else {
                    Some(current)
                }
            }
        };
    }
    newest
}

pub(crate) fn locked_npm_metadata(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
    node_version: Option<&str>,
    scope: LockScope,
) -> Result<Option<LockedNpmMetadata>> {
    let Some(package) = version.backend.strip_prefix("npm:") else {
        return Ok(None);
    };
    let node_version = node_version
        .or_else(|| {
            version
                .options
                .get(LOCKED_NPM_NODE_VERSION_OPTION)
                .map(String::as_str)
        })
        .map(str::to_owned);
    let installer = version
        .options
        .get(LOCKED_NPM_INSTALLER_OPTION)
        .map(|value| NpmInstaller::parse(value))
        .transpose()?
        .unwrap_or(NpmInstaller::Npm);
    let declared_native_lock = locked_native_lock_from_options(version)?;
    let lock_path = dirs
        .install_path(&version.backend, &version.version)
        .join(NPM_LOCKFILE_PATH);
    let native_lock = if let Some(native_lock) = declared_native_lock {
        Some(native_lock)
    } else if installer == NpmInstaller::Npm && lock_path.is_file() {
        let bytes = read_bounded(&lock_path, MAX_NPM_GRAPH_BYTES).with_context(|| {
            osdk_core::t!("err.npm_lock_payload_read", path = lock_path.display())
        })?;
        Some(LockedNativeLock {
            kind: installer,
            format: match installer {
                NpmInstaller::Npm => NPM_LOCK_FORMAT.into(),
                NpmInstaller::Pnpm => "pnpm-v9".into(),
            },
            sha256: osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256),
        })
    } else {
        None
    };
    Ok(Some(LockedNpmMetadata {
        package: package.to_string(),
        installer,
        scope,
        node_version,
        native_lock,
    }))
}

/// Read the solved-closure identity of an installed `conda:` prefix.
///
/// Taken from the install inventory rather than recomputed: the digest is only
/// knowable after a solve, and re-solving here would both hit the network during
/// `lock` and risk recording a closure different from the one actually
/// installed. `conda_locked_identity` is the same lookup `bin_paths` and the shim
/// already perform, so the lock cannot disagree with what runs.
///
/// Returns `None` when nothing is installed -- `lock` may legitimately run
/// before install, and an absent prefix is not a reason to fail. It does *not*
/// invent a placeholder digest: a lock entry claiming a closure it never saw
/// would be worse than one admitting it has none.
pub(crate) fn locked_conda_metadata(
    dirs: &osdk_core::dirs::Dirs,
    platform: Platform,
    version: &ToolVersion,
) -> Result<Option<LockedCondaTool>> {
    if !version.backend.starts_with("conda:") {
        return Ok(None);
    }
    let Some(identity) =
        osdk_core::backend::conda::installed_identity(dirs, platform, &version.backend, version)?
    else {
        return Ok(None);
    };
    let Some(closure) = identity.materials.get("artifact-checksum") else {
        return Ok(None);
    };
    // The package count is carried in the material file name the backend
    // synthesizes (`conda-closure-<n>.json`); parse it rather than storing a
    // second source of truth that could drift from the digest.
    let packages = identity
        .materials
        .get("artifact-file")
        .and_then(|file| {
            file.strip_prefix("conda-closure-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .and_then(|count| count.parse::<usize>().ok())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "conda install `{}@{}` has no recognizable closure material; reinstall it before locking",
                version.backend,
                version.version
            )
        })?;
    Ok(Some(LockedCondaTool {
        closure: closure.clone(),
        packages,
    }))
}

pub(crate) fn locked_native_metadata(version: &ToolVersion) -> Result<Option<LockedNativeTool>> {
    let Some(expected_runtime) = native_runtime_for_backend(&version.backend) else {
        return Ok(None);
    };
    let runtime = version.options.get(LOCKED_NATIVE_RUNTIME_OPTION);
    let runtime_version = version.options.get(LOCKED_NATIVE_RUNTIME_VERSION_OPTION);
    let replay = version.options.get(LOCKED_NATIVE_REPLAY_OPTION);
    if runtime.is_none() && runtime_version.is_none() && replay.is_none() {
        return Ok(None);
    }
    let required = |key: &str, value: Option<&String>| {
        value
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("native lock metadata is missing `{key}`"))
    };
    let metadata = LockedNativeTool {
        runtime: required(LOCKED_NATIVE_RUNTIME_OPTION, runtime)?,
        runtime_version: required(LOCKED_NATIVE_RUNTIME_VERSION_OPTION, runtime_version)?,
        replay: NativeReplay::parse(&required(LOCKED_NATIVE_REPLAY_OPTION, replay)?)?,
        source: if expected_runtime == "go" {
            version
                .options
                .get(osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION)
                .cloned()
        } else {
            version
                .options
                .get(osdk_core::backend::cargo_package::LOCKED_CARGO_INDEX_OPTION)
                .cloned()
        },
        module: version
            .options
            .get(osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION)
            .cloned(),
    };
    if metadata.runtime != expected_runtime {
        anyhow::bail!(
            "native tool `{}` requires runtime `{expected_runtime}`, got `{}`",
            version.backend,
            metadata.runtime
        );
    }
    validate_version_identity(&metadata.runtime, &metadata.runtime_version)?;
    if expected_runtime == "rust" {
        validate_exact_rust_version(&version.backend, &metadata.runtime_version)?;
        match (
            version.backend.starts_with("cargo:https://"),
            metadata.source.as_deref(),
        ) {
            (true, Some(_)) => anyhow::bail!(
                "Cargo Git tool `{}` cannot carry a registry source",
                version.backend
            ),
            (false, Some(source)) => validate_cargo_registry_source(&version.backend, source)?,
            _ => {}
        }
        if metadata.module.is_some() {
            anyhow::bail!(
                "Cargo tool `{}` cannot carry a Go module root",
                version.backend
            );
        }
    } else {
        validate_exact_go_version(&version.backend, &metadata.runtime_version)?;
        let source = metadata.source.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Go tool `{}` is missing its locked proxy source",
                version.backend
            )
        })?;
        osdk_core::backend::go_package::validate_go_proxy(source)?;
        if metadata.module.is_none() {
            anyhow::bail!(
                "Go tool `{}` is missing its locked module root",
                version.backend
            );
        }
    }
    Ok(Some(metadata))
}

pub(crate) fn locked_native_lock_from_options(
    version: &ToolVersion,
) -> Result<Option<LockedNativeLock>> {
    let values = [
        version.options.get(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION),
        version.options.get(LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION),
        version.options.get(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION),
    ];
    if values.iter().all(|value| value.is_none()) {
        return Ok(None);
    }
    let required = |key: &str, value: Option<&String>| {
        value.cloned().ok_or_else(|| {
            anyhow::anyhow!(osdk_core::t!(
                "err.lock_npm_native_metadata_missing",
                key = key
            ))
        })
    };
    let native_lock = LockedNativeLock {
        kind: NpmInstaller::parse(&required(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION, values[0])?)?,
        format: required(LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION, values[1])?,
        sha256: required(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION, values[2])?,
    };
    validate_native_lock(&version.backend, &native_lock)?;
    Ok(Some(native_lock))
}

pub(crate) fn save(path: &Path, lockfile: &Lockfile) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| osdk_core::t!("err.fs_directory_create", path = parent.display()))?;
    }
    if lockfile.schema == 1 {
        reject_unmigratable_schema_one_npm_entries(lockfile)?;
        reject_native_backends_before_schema_four(lockfile)?;
        reject_native_metadata_before_schema_four(lockfile)?;
    } else if lockfile.schema == 2 {
        validate_schema_two(path, lockfile, true)?;
    } else if lockfile.schema == 3 {
        validate_schema_three(lockfile)?;
    }
    let mut lockfile = lockfile.clone();
    migrate_npm_entries_to_schema_three(&mut lockfile)?;
    lockfile.schema = schema_version();
    validate_schema_four(&lockfile)?;
    let text = toml::to_string_pretty(&lockfile)?;
    if text.len() as u64 > MAX_LOCKFILE_BYTES {
        anyhow::bail!(osdk_core::t!(
            "err.lockfile_size_limit_exceeded",
            maximum = MAX_LOCKFILE_BYTES
        ));
    }
    atomic_write(path, text.as_bytes())?;
    Ok(())
}

pub(crate) fn migrate_npm_entries_to_schema_three(lockfile: &mut Lockfile) -> Result<()> {
    for platform_lock in lockfile.platforms.values_mut() {
        for (backend, locked) in &mut platform_lock.tools {
            let Some(package) = backend.strip_prefix("npm:") else {
                continue;
            };
            let metadata = match locked.npm.take() {
                Some(LockedNpmGraph::Metadata(metadata)) => metadata,
                Some(LockedNpmGraph::Sidecar(sidecar)) => LockedNpmMetadata {
                    package: sidecar.package,
                    installer: NpmInstaller::Npm,
                    scope: LockScope::Project,
                    node_version: Some(sidecar.node_version),
                    native_lock: Some(LockedNativeLock {
                        kind: NpmInstaller::Npm,
                        format: sidecar.lock_format,
                        sha256: sidecar.sha256,
                    }),
                },
                Some(LockedNpmGraph::Legacy(_)) | None => {
                    let installer = locked
                        .options
                        .get(LOCKED_NPM_INSTALLER_OPTION)
                        .map(|value| NpmInstaller::parse(value))
                        .transpose()?
                        .unwrap_or(NpmInstaller::Npm);
                    LockedNpmMetadata {
                        package: package.to_string(),
                        installer,
                        scope: LockScope::Project,
                        node_version: locked.options.get(LOCKED_NPM_NODE_VERSION_OPTION).cloned(),
                        native_lock: None,
                    }
                }
            };
            locked.options.retain(|key, _| {
                !key.starts_with("__osdk_npm_") && key != LOCKED_NPM_NODE_VERSION_OPTION
            });
            locked.npm = Some(LockedNpmGraph::Metadata(metadata));
            locked.artifact = None;
        }
    }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| osdk_core::t!("err.fs_directory_create", path = parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(osdk_core::t!(
                "err.lock_atomic_path_filename_missing",
                path = path.display()
            ))
        })?
        .to_string_lossy();
    let (temporary, mut file) = loop {
        let serial = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.tmp-{}-{serial}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    osdk_core::t!("err.fs_file_create", path = temporary.display())
                });
            }
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| osdk_core::t!("err.fs_file_write", path = temporary.display()))?;
        file.sync_all()
            .with_context(|| osdk_core::t!("err.fs_file_sync", path = temporary.display()))?;
        drop(file);
        atomic_replace(&temporary, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing lockfile directory {}", parent.display()))
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| osdk_core::t!("err.fs_file_replace", path = destination.display()))
}

#[cfg(windows)]
pub(crate) fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| osdk_core::t!("err.fs_file_replace", path = destination.display()));
    }
    Ok(())
}
