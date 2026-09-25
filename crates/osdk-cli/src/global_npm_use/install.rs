//! `install` free functions split from global_npm_use.rs.

use super::*;

/// Install, globally select, lock, and expose one dynamic npm tool.
pub async fn install(
    app: &mut App,
    mut request: ToolRequest,
    requested_spec: Option<String>,
) -> Result<()> {
    if !request.backend.starts_with("npm:") {
        return Err(anyhow!(
            "global npm installer requires an npm:<package> request"
        ));
    }
    let requested_installer = npm_tools::installer_from_request_options(&request.options)?;
    let cwd = std::env::current_dir().context("getting current dir for global npm install")?;
    let plan = npm_tools::plan_npm_installer(
        &cwd,
        requested_installer,
        ToolScope::Global,
        app.ctx.config.settings.npm.default_installer,
    )?;
    let backend = NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    let effective = expand_alias(app, &request)?;
    let mut mutation_lock = None;
    let mut final_layout = None;
    let mut runtime = None;
    let mut version = None;
    let mut staged_install = None;

    // An exact request identifies its canonical root without consulting a
    // registry. Recover that root first, then reuse it when its recorded Node
    // identity is backed by a locally installed runtime. This path must stay
    // free of source probes, downloads, post-install hooks, and helpers.
    if let VersionSpec::Exact(exact) = &effective.spec {
        if let Some(existing_runtime) =
            load_existing_managed_runtime(app, backend.id(), exact, plan.installer)?
        {
            set_runtime_request_options(&mut request, plan.installer, &existing_runtime);
            let mut candidate = ToolVersion::new(backend.id(), exact.clone());
            candidate.options = request.options.clone();
            candidate
                .options
                .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());
            let candidate_layout = GlobalInstallLayout::for_root(
                backend.global_install_root_for(&app.ctx, &candidate)?,
                plan.installer,
                app.ctx.platform,
            );
            let lock = acquire_global_version_lock(app, &backend, &candidate)?;
            recover_interrupted_promotion_serialized(&app.ctx.dirs, &candidate_layout.root)?;
            let installed_native_lock = native_lock_path(&candidate_layout, plan.installer);
            if completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &candidate,
                plan.installer,
                &existing_runtime.node_version.version,
                &candidate_layout,
                installed_native_lock.as_deref(),
            )? {
                let native = installed_native_lock
                    .as_deref()
                    .filter(|path| path.is_file())
                    .map(|path| read_native_lock(path, plan.installer))
                    .transpose()?;
                inject_native_metadata(
                    &mut candidate,
                    plan.installer,
                    &existing_runtime.node_version.version,
                    native.as_ref(),
                );
                runtime = Some(existing_runtime);
                version = Some(candidate);
            }
            mutation_lock = Some(lock);
            final_layout = Some(candidate_layout);
        }
    }

    if version.is_none() {
        let package_spec = format!("{}@{}", backend.package(), request.spec);
        let (registry_manager, registry_alias, registry_command) = match plan.installer {
            NpmInstaller::Npm => (PackageManager::Npm, "npm", "install"),
            NpmInstaller::Pnpm => (PackageManager::Pnpm, "pnpm", "add"),
            NpmInstaller::Auto => {
                unreachable!("global planning always produces a concrete installer")
            }
        };
        // This must precede runtime preparation and npm package resolution. A
        // private/authenticated native registry cannot be reproduced inside
        // the isolated global installer.
        let selected_registry = plan_isolated_global_registry(
            app,
            &app.ctx.dirs.data.join("global-registry-preflight"),
            registry_manager,
            registry_alias,
            &[registry_command.into(), package_spec],
        )
        .await?;
        apply_source_override(app, &request.backend);
        let prepared_runtime = ensure_managed_runtime(app, plan.installer).await?;
        set_runtime_request_options(&mut request, plan.installer, &prepared_runtime);

        if app.refresh_sources {
            select::refresh(&app.ctx, &backend).await?;
        }
        let effective = expand_alias(app, &request)?;
        let mut resolved = backend
            .resolve_version(&app.ctx, &effective)
            .await
            .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
        resolved.options = request.options.clone();
        resolved
            .options
            .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());
        let resolved_layout = GlobalInstallLayout::for_root(
            backend.global_install_root_for(&app.ctx, &resolved)?,
            plan.installer,
            app.ctx.platform,
        );
        if let Some(existing_layout) = &final_layout {
            if existing_layout.root != resolved_layout.root {
                return Err(anyhow!(
                    "exact global npm request resolved to an unexpected version: {}",
                    resolved.version
                ));
            }
        } else {
            mutation_lock = Some(acquire_global_version_lock(app, &backend, &resolved)?);
            recover_interrupted_promotion_serialized(&app.ctx.dirs, &resolved_layout.root)?;
            final_layout = Some(resolved_layout.clone());
        }

        let installed_native_lock = native_lock_path(&resolved_layout, plan.installer);
        if !completed_install_matches_at(
            app.ctx.platform,
            &backend,
            &resolved,
            plan.installer,
            &prepared_runtime.node_version.version,
            &resolved_layout,
            installed_native_lock.as_deref(),
        )? {
            let staged = StagedInstall::begin(&resolved_layout.root)?;
            let staged_layout = GlobalInstallLayout::for_root(
                staged.root().to_path_buf(),
                plan.installer,
                app.ctx.platform,
            );
            run_global_install(
                app,
                &backend,
                &resolved,
                plan.installer,
                &prepared_runtime,
                &staged_layout,
                selected_registry.as_deref(),
            )
            .await?;
            normalize_global_bins(&backend, &resolved, plan.installer, &staged_layout)?;
            validate_global_package_identity(&backend, &resolved, plan.installer, &staged_layout)?;
            let staged_native_lock = native_lock_path(&staged_layout, plan.installer);
            let native = staged_native_lock
                .as_deref()
                .filter(|path| path.is_file())
                .map(|path| read_native_lock(path, plan.installer))
                .transpose()?;
            inject_native_metadata(
                &mut resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                native.as_ref(),
            );
            backend
                .finalize_global_install_at(
                    &app.ctx,
                    &resolved,
                    &staged_layout.root,
                    &staged_layout.bin,
                    &prepared_runtime.node_version.version,
                    plan.installer.as_str(),
                    native
                        .as_ref()
                        .map(|native| (native.format.as_str(), native.sha256.as_str())),
                )
                .map_err(anyhow::Error::new)?;
            if !completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                &staged_layout,
                staged_native_lock.as_deref(),
            )? {
                return Err(anyhow!(
                    "staged global npm install failed completed-state validation at {}",
                    staged_layout.root.display()
                ));
            }
            staged_install = Some(staged);
        } else {
            let native = installed_native_lock
                .as_deref()
                .filter(|path| path.is_file())
                .map(|path| read_native_lock(path, plan.installer))
                .transpose()?;
            inject_native_metadata(
                &mut resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                native.as_ref(),
            );
        }
        runtime = Some(prepared_runtime);
        version = Some(resolved);
    }

    let runtime = runtime.expect("global npm runtime is available after preparation or reuse");
    let version = version.expect("global npm version is available after preparation or reuse");
    let final_layout =
        final_layout.expect("global npm layout is available after preparation or reuse");
    let _mutation_lock =
        mutation_lock.expect("global npm mutation lock is held through publication");

    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    with_global_npm_state_lock(&app.ctx.dirs, || {
        crate::commands::recover_interrupted_global_npm_uninstalls(app)?;
        let new_bin_names = manifest_bin_names(
            staged_install
                .as_ref()
                .map(StagedInstall::root)
                .unwrap_or(&final_layout.root),
        )?;
        let old_bin_names = selected_global_bin_names(&app.ctx, &backend, &final_layout.root)?;
        let publication = PublicationSnapshot::capture(app)?;
        let mut replacement = if let Some(staged) = staged_install {
            let mut promoted = staged.promote()?;
            let promoted_native_lock = native_lock_path(&final_layout, plan.installer);
            if !completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &version,
                plan.installer,
                &runtime.node_version.version,
                &final_layout,
                promoted_native_lock.as_deref(),
            )? {
                return rollback_replacement_error(
                    &mut promoted,
                    anyhow!(
                        "promoted global npm install failed validation at {}",
                        final_layout.root.display()
                    ),
                );
            }
            Some(promoted)
        } else {
            None
        };
        // `NewPromoted` is the roll-forward boundary: an abrupt exit keeps the
        // fully validated new root and a rerun repairs any remaining metadata
        // or shims. Do not mark the publication activated until config, lock,
        // and every affected shim directory entry have been durably published.
        // A normal error still rolls every publication step back below.
        let publish_result = publish_then_activate(replacement.as_ref(), || {
            crate::commands::generate_shims_for(app, &backend, &version)?;
            persist_global_config(app, &request, &persisted_spec, plan.installer)?;
            persist_global_lock(app, &request, &version, &runtime)?;
            let refreshed = refreshed_app(app)?;
            crate::commands::remove_stale_shims_without_other_owners(
                &refreshed,
                backend.id(),
                &old_bin_names,
                &new_bin_names,
            )?;
            // This full pass makes the roll-forward path idempotent even when
            // a prior process crashed after publishing config/lock but before
            // it could remove a bin name from the previously selected version.
            crate::commands::reconcile_managed_shims(&refreshed)?;
            let mut affected_bin_names = old_bin_names.clone();
            affected_bin_names.extend(new_bin_names.iter().cloned());
            affected_bin_names.sort();
            affected_bin_names.dedup();
            sync_shim_publication(&app.ctx.dirs, &affected_bin_names)?;
            Ok(())
        });
        if let Err(error) = publish_result {
            let publication_error = publication.restore().err();
            let replacement_error = replacement
                .as_mut()
                .map(PromotedInstall::rollback)
                .transpose()
                .err();
            let shim_error = refreshed_app(app)
                .and_then(|refreshed| crate::commands::reconcile_managed_shims(&refreshed))
                .err();
            return Err(with_rollback_context(
                error,
                combine_rollback_errors(publication_error, shim_error),
                replacement_error,
            ));
        }
        if let Some(mut replacement) = replacement {
            replacement.finish();
        }
        if let Err(error) = backend.remove_legacy_global_install(&app.ctx, &version) {
            tracing::warn!(error = %error, "failed to remove legacy global npm install after publication");
        }
        Ok(())
    })?;
    println!(
        "{}",
        osdk_core::t!(
            "msg.pinned_global",
            tool = request.backend,
            ver = persisted_spec
        )
    );

    Ok(())
}

pub(crate) fn with_global_npm_state_lock<T>(
    dirs: &osdk_core::dirs::Dirs,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = osdk_core::lock::FileLock::acquire(dirs.data.join("locks/global-npm-state.lock"))?;
    operation()
}

pub(crate) fn acquire_global_version_lock(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
) -> Result<osdk_core::lock::FileLock> {
    let locator = backend.install_locator(&app.ctx, version, ToolScope::Global)?;
    osdk_core::lock::FileLock::acquire(locator.lock_path()).map_err(anyhow::Error::new)
}

pub(crate) fn refreshed_app(app: &App) -> Result<App> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    Ok(App::from_parts(
        ctx,
        osdk_core::Registry::load(&app.ctx.dirs)?,
        app.prompt.clone(),
        app.source_override.clone(),
        app.refresh_sources,
    ))
}

pub(crate) async fn ensure_managed_runtime(
    app: &mut App,
    installer: NpmInstaller,
) -> Result<ManagedRuntime> {
    let node_request = configured_request(app, "node");
    let (node_backend, node_version) = install_backend(app, &node_request).await?;
    let node_bin = find_bin_dir(&app.ctx, node_backend.as_ref(), &node_version, "node")?;
    let manager_id = match installer {
        NpmInstaller::Npm => Some("npm"),
        NpmInstaller::Pnpm => Some("pnpm"),
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    };
    let manager = if let Some(manager_id) = manager_id {
        let request = configured_request(app, manager_id);
        let (backend, version) = install_backend(app, &request).await?;
        let executable = find_executable(&backend.bin_paths(&app.ctx, &version)?, manager_id)
            .ok_or_else(|| anyhow!("managed {manager_id} executable was not installed"))?;
        Some((exact_request(request, &version), version, executable))
    } else {
        None
    };
    Ok(ManagedRuntime {
        node_request: exact_request(node_request, &node_version),
        node_version,
        node_bin,
        manager,
    })
}

pub(crate) fn set_runtime_request_options(
    request: &mut ToolRequest,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
) {
    request.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_NODE_VERSION_OPTION.into(),
        runtime.node_version.version.clone(),
    );
}

/// Reconstruct a runtime strictly from authoritative local state. The npm
/// package manifest is deliberately not used to choose Node: it is only
/// compared against this independently selected installed runtime later.
pub(crate) fn load_existing_managed_runtime(
    app: &App,
    package_backend: &str,
    package_version: &str,
    installer: NpmInstaller,
) -> Result<Option<ManagedRuntime>> {
    let locked = existing_global_lock_requests(app, package_backend, package_version)?;
    let node_request = locked
        .as_ref()
        .and_then(|requests| exact_locked_request(requests, "node"))
        .cloned()
        .or_else(|| exact_configured_request(app, "node"));
    let Some(node_request) = node_request else {
        return Ok(None);
    };
    let Some((node_version, node_bin)) = load_exact_runtime_component(app, &node_request, "node")?
    else {
        return Ok(None);
    };

    let manager_id = match installer {
        NpmInstaller::Npm => Some("npm"),
        NpmInstaller::Pnpm => Some("pnpm"),
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    };
    let manager = if let Some(manager_id) = manager_id {
        let manager_request = locked
            .as_ref()
            .and_then(|requests| exact_locked_request(requests, manager_id))
            .cloned()
            .or_else(|| exact_configured_request(app, manager_id));
        let Some(manager_request) = manager_request else {
            return Ok(None);
        };
        let Some((manager_version, executable)) =
            load_exact_runtime_component(app, &manager_request, manager_id)?
        else {
            return Ok(None);
        };
        Some((manager_request, manager_version, executable))
    } else {
        None
    };

    Ok(Some(ManagedRuntime {
        node_request,
        node_version,
        node_bin,
        manager,
    }))
}

pub(crate) fn existing_global_lock_requests(
    app: &App,
    package_backend: &str,
    package_version: &str,
) -> Result<Option<Vec<ToolRequest>>> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(None);
    }
    let Some(requests) = crate::lockfile::locked_requests(&path, app.ctx.platform)? else {
        return Ok(None);
    };
    let matches_package = requests.iter().any(|request| {
        request.backend == package_backend
            && matches!(&request.spec, VersionSpec::Exact(version) if version == package_version)
            && request
                .options
                .get(LOCKED_NPM_SCOPE_OPTION)
                .map(String::as_str)
                == Some(ToolScope::Global.as_str())
    });
    Ok(matches_package.then_some(requests))
}

pub(crate) fn exact_locked_request<'a>(
    requests: &'a [ToolRequest],
    backend: &str,
) -> Option<&'a ToolRequest> {
    requests
        .iter()
        .find(|request| request.backend == backend && matches!(request.spec, VersionSpec::Exact(_)))
}

pub(crate) fn exact_configured_request(app: &App, backend: &str) -> Option<ToolRequest> {
    let request = configured_request(app, backend);
    matches!(request.spec, VersionSpec::Exact(_)).then_some(request)
}

pub(crate) fn load_exact_runtime_component(
    app: &App,
    request: &ToolRequest,
    executable: &str,
) -> Result<Option<(ToolVersion, PathBuf)>> {
    let VersionSpec::Exact(version) = &request.spec else {
        return Ok(None);
    };
    let backend = app.registry.get(&request.backend)?;
    let installed = ToolVersion::new(backend.id(), version.clone());
    if !backend
        .list_installed(&app.ctx)?
        .iter()
        .any(|candidate| candidate == version)
    {
        return Ok(None);
    }
    let Some(path) = find_executable(&backend.bin_paths(&app.ctx, &installed)?, executable) else {
        return Ok(None);
    };
    let location = if executable == "node" {
        path.parent()
            .ok_or_else(|| anyhow!("managed node executable has no parent directory"))?
            .to_path_buf()
    } else {
        path
    };
    Ok(Some((installed, location)))
}

pub(crate) fn exact_request(mut request: ToolRequest, version: &ToolVersion) -> ToolRequest {
    request.spec = VersionSpec::Exact(version.version.clone());
    request
}

pub(crate) fn configured_request(app: &App, backend: &str) -> ToolRequest {
    let entry = app.ctx.config.global_tool_configs.get(backend);
    ToolRequest {
        backend: backend.into(),
        spec: entry
            .map(|entry| VersionSpec::parse(entry.version()))
            .unwrap_or(VersionSpec::Latest),
        options: entry
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    }
}

pub(crate) async fn install_backend(
    app: &mut App,
    request: &ToolRequest,
) -> Result<(std::sync::Arc<dyn Backend>, ToolVersion)> {
    apply_source_override(app, &request.backend);
    let backend = app.registry.get(&request.backend)?;
    if app.refresh_sources {
        select::refresh(&app.ctx, backend.as_ref()).await?;
    }
    let effective = expand_alias(app, request)?;
    let version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    if pipeline::is_installed(&app.ctx.dirs, backend.id(), &version.version) {
        backend.ensure_post_install(&app.ctx, &version)?;
    } else {
        backend
            .install(&InstallCtx { ctx: &app.ctx }, &version)
            .await
            .with_context(|| format!("installing {}", version))?;
        app.invalidate_dynamic_scan();
    }
    Ok((backend, version))
}

pub(crate) async fn run_global_install(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
    selected_registry: Option<&str>,
) -> Result<()> {
    match installer {
        NpmInstaller::Npm | NpmInstaller::Pnpm => {
            run_native_installer(
                app,
                backend,
                version,
                installer,
                runtime,
                layout,
                selected_registry,
            )
            .await
        }
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    }
}

pub(crate) fn normalize_global_bins(
    backend: &NpmPackageBackend,
    _version: &ToolVersion,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Result<()> {
    // Resolve all package-manager launchers to relocatable osdk-owned
    // launchers. Native npm/pnpm wrappers commonly embed the staging prefix.
    let package_json = package_manifest_path(backend, installer, layout).ok_or_else(|| {
        anyhow!(
            "cannot locate {} under staged global install {}",
            backend.package(),
            layout.root.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&package_json)?)?;
    let entries =
        osdk_core::backend::npm_package::package_bin_entries(&manifest, backend.package())?;
    let package_dir = package_json
        .parent()
        .ok_or_else(|| anyhow!("package manifest has no parent"))?;
    let mut targets = Vec::with_capacity(entries.len());
    for (name, relative_target) in entries {
        let canonical_target = dunce::canonicalize(package_dir.join(relative_target))?;
        targets.push((name, canonical_target));
    }
    reset_global_bin_dir(layout)?;
    for (name, canonical_target) in targets {
        let target_relative = path_relative_to(&canonical_target, &layout.bin)?;
        #[cfg(unix)]
        {
            let destination = layout.bin.join(&name);
            let _ = std::fs::remove_file(&destination);
            std::os::unix::fs::symlink(target_relative, destination)?;
        }
        #[cfg(windows)]
        {
            if name.contains(['%', '!', '^', '&', '|', '<', '>', '(', ')']) {
                return Err(anyhow!("unsafe npm bin name `{name}` for cmd wrapper"));
            }
            let destination = layout.bin.join(format!("{name}.cmd"));
            std::fs::write(destination, render_windows_node_wrapper(&target_relative)?)?;
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_node_wrapper(target_relative: &Path) -> Result<String> {
    let relative = target_relative.to_string_lossy().replace('/', "\\");
    if relative.contains(['%', '!', '\r', '\n', '\0']) {
        return Err(anyhow!(
            "unsafe Windows npm launcher target `{relative}` contains cmd expansion characters"
        ));
    }
    Ok(format!(
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\nnode \"%~dp0{relative}\" %*\r\n"
    ))
}

pub(crate) fn reset_global_bin_dir(layout: &GlobalInstallLayout) -> Result<()> {
    if layout.bin == layout.root {
        for entry in std::fs::read_dir(&layout.root)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_file() || metadata.file_type().is_symlink() {
                remove_path(&entry.path())?;
            }
        }
    } else {
        remove_path(&layout.bin)?;
        std::fs::create_dir_all(&layout.bin)?;
    }
    Ok(())
}

pub(crate) fn path_relative_to(target: &Path, base: &Path) -> Result<PathBuf> {
    let target = target.components().collect::<Vec<_>>();
    let base = dunce::canonicalize(base)?;
    let base = base.components().collect::<Vec<_>>();
    let shared = target
        .iter()
        .zip(&base)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == 0 {
        return Err(anyhow!("cannot create cross-volume relative launcher"));
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    Ok(relative)
}

pub(crate) async fn run_native_installer(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
    selected_registry: Option<&str>,
) -> Result<()> {
    let (_, manager_version, executable) = runtime
        .manager
        .as_ref()
        .ok_or_else(|| anyhow!("managed {installer} was not prepared"))?;
    let manager = match installer {
        NpmInstaller::Npm => PackageManager::Npm,
        NpmInstaller::Pnpm => PackageManager::Pnpm,
        _ => unreachable!(),
    };
    let package_spec = format!("{}@{}", backend.package(), version.version);
    let args = native_args(
        installer,
        &layout.root,
        &layout.bin,
        &package_spec,
        version,
        &app.ctx.dirs,
        app.ctx.config.settings.offline,
    )?;
    let mut env = isolated_native_env(
        app,
        &layout.root,
        &layout.bin,
        installer,
        &runtime.node_bin,
        manager_version,
    )?;
    if let Some(url) = selected_registry {
        env.insert(package_registry::registry_env(manager).into(), url.into());
    }
    run_managed_command(executable, &args, &env, &layout.root)
}

pub(crate) fn global_registry_ctx(app: &App) -> Result<osdk_core::backend::Ctx> {
    let mut config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    // Command-line offline mode is already folded into the active context and
    // must remain authoritative even though project configuration is excluded.
    config.settings.offline = app.ctx.config.settings.offline;
    Ok(osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    })
}

pub(crate) async fn plan_isolated_global_registry(
    app: &App,
    cwd: &Path,
    manager: PackageManager,
    executable_alias: &str,
    logical_args: &[String],
) -> Result<Option<String>> {
    let context = global_registry_ctx(app)?;
    match package_registry::plan(
        &context,
        cwd,
        manager,
        executable_alias,
        logical_args,
        |key| std::env::var(key).ok(),
    )
    .await?
    {
        RegistryPlan::Selected { url, .. } => Ok(Some(url)),
        RegistryPlan::Unavailable { probes } => Err(unavailable_registry_error(manager, &probes)),
        RegistryPlan::PassThrough { .. } if context.config.settings.offline => Ok(None),
        RegistryPlan::PassThrough { reason } => Err(anyhow!(osdk_core::t!(
            "err.npm_global_registry_isolation",
            manager = manager,
            reason = reason
        ))),
    }
}

pub(crate) fn native_args(
    installer: NpmInstaller,
    install_root: &Path,
    bin_dir: &Path,
    package_spec: &str,
    version: &ToolVersion,
    dirs: &osdk_core::dirs::Dirs,
    offline: bool,
) -> Result<Vec<String>> {
    let build_policy = npm_build_policy(version)?;
    match installer {
        NpmInstaller::Npm => {
            let mut args = vec![
                "install".into(),
                "--global".into(),
                "--prefix".into(),
                install_root.display().to_string(),
                "--audit=false".into(),
                "--fund=false".into(),
            ];
            match build_policy {
                NpmBuildPolicy::Deny => args.push("--ignore-scripts".into()),
                NpmBuildPolicy::AllowAll => {}
                NpmBuildPolicy::Packages(_) => {
                    return Err(anyhow!(
                        "installer `npm` cannot enforce a package allowlist; use allow_builds=false or true"
                    ))
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        NpmInstaller::Pnpm => {
            let mut args = vec![
                "add".into(),
                "--global".into(),
                "--global-dir".into(),
                install_root.join("pnpm-global").display().to_string(),
                "--global-bin-dir".into(),
                bin_dir.display().to_string(),
                "--store-dir".into(),
                dirs.store.join("pnpm-store").display().to_string(),
            ];
            match build_policy {
                NpmBuildPolicy::Deny => args.push("--ignore-scripts".into()),
                NpmBuildPolicy::AllowAll => args.push("--dangerously-allow-all-builds".into()),
                NpmBuildPolicy::Packages(packages) => {
                    for package in packages {
                        args.push(format!("--allow-build={package}"));
                    }
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
pub(crate) fn native_preflight_args(
    installer: NpmInstaller,
    package_spec: &str,
    offline: bool,
) -> Vec<String> {
    let mut args = vec![
        if installer == NpmInstaller::Npm {
            "install".into()
        } else {
            "add".into()
        },
        package_spec.into(),
    ];
    if offline {
        args.push("--offline".into());
    }
    args
}

pub(crate) fn isolated_native_env(
    app: &App,
    install_root: &Path,
    bin_dir: &Path,
    installer: NpmInstaller,
    node_bin: &Path,
    manager_version: &ToolVersion,
) -> Result<BTreeMap<String, String>> {
    let native = install_root.join(NATIVE_CONFIG_DIR);
    std::fs::create_dir_all(&native)?;
    let user_config = native.join("user.npmrc");
    let global_config = native.join("global.npmrc");
    for path in [&user_config, &global_config] {
        if !path.exists() {
            std::fs::write(path, b"")?;
        }
    }
    let manager_dir = runtime_manager_dir(app, installer, manager_version)?;
    let mut path_entries = vec![bin_dir.to_path_buf(), manager_dir, node_bin.to_path_buf()];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries)?;
    let cache_root = osdk_core::cache::downstream_root(&app.ctx.dirs.cache);
    let mut env = BTreeMap::from([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        ("HOME".into(), native.join("home").display().to_string()),
        (
            "npm_config_userconfig".into(),
            user_config.display().to_string(),
        ),
        (
            "npm_config_globalconfig".into(),
            global_config.display().to_string(),
        ),
        ("npm_config_update_notifier".into(), "false".into()),
        ("npm_config_audit".into(), "false".into()),
        ("npm_config_fund".into(), "false".into()),
        ("COREPACK_ENABLE_PROJECT_SPEC".into(), "0".into()),
    ]);
    std::fs::create_dir_all(native.join("home"))?;
    match installer {
        NpmInstaller::Npm => {
            env.insert(
                "npm_config_prefix".into(),
                install_root.display().to_string(),
            );
            env.insert(
                "npm_config_cache".into(),
                cache_root.join("npm").display().to_string(),
            );
        }
        NpmInstaller::Pnpm => {
            env.insert("PNPM_HOME".into(), bin_dir.display().to_string());
            env.insert(
                "pnpm_config_cache_dir".into(),
                cache_root.join("pnpm").display().to_string(),
            );
            env.insert(
                "pnpm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "npm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "pnpm_config_state_dir".into(),
                app.ctx
                    .dirs
                    .data
                    .join("npm-native-state/pnpm")
                    .display()
                    .to_string(),
            );
        }
        _ => unreachable!(),
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    Ok(env)
}

pub(crate) fn runtime_manager_dir(
    app: &App,
    installer: NpmInstaller,
    manager_version: &ToolVersion,
) -> Result<PathBuf> {
    let backend = app.registry.get(installer.as_str())?;
    backend
        .bin_paths(&app.ctx, manager_version)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("managed {installer} has no bin directory"))
}

pub(crate) fn run_managed_command(
    executable: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<()> {
    let mut command = if cfg!(windows)
        && executable
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            }) {
        let mut command = Command::new(
            std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
        );
        command.args(["/D", "/S", "/C", "call"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let output = command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running managed installer {}", executable.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
        let stdout = &output.stdout[..output.stdout.len().min(MAX_DIAGNOSTIC_BYTES)];
        let stderr = &output.stderr[..output.stderr.len().min(MAX_DIAGNOSTIC_BYTES)];
        Err(anyhow!(
            "managed installer {} failed with {}:\nstdout:\n{}\nstderr:\n{}",
            executable.display(),
            output.status,
            String::from_utf8_lossy(stdout),
            String::from_utf8_lossy(stderr)
        ))
    }
}

pub(crate) fn parse_npm_build_policy(raw: Option<&str>) -> Result<NpmBuildPolicy> {
    let Some(raw) = raw else {
        return Ok(NpmBuildPolicy::Deny);
    };
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.is_empty() || matches!(lower.as_str(), "false" | "0" | "no" | "off") {
        return Ok(NpmBuildPolicy::Deny);
    }
    if matches!(lower.as_str(), "true" | "1" | "yes" | "on") {
        return Ok(NpmBuildPolicy::AllowAll);
    }
    let mut packages = raw
        .split(',')
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Err(anyhow!("allow_builds contains no package names"));
    }
    Ok(NpmBuildPolicy::Packages(packages))
}

pub(crate) fn npm_build_policy(version: &ToolVersion) -> Result<NpmBuildPolicy> {
    parse_npm_build_policy(version.options.get("allow_builds").map(String::as_str))
}

pub(crate) fn npm_build_policy_identity(version: &ToolVersion) -> Result<String> {
    Ok(npm_build_policy(version)?.identity())
}

pub(crate) fn native_lock_path(
    layout: &GlobalInstallLayout,
    installer: NpmInstaller,
) -> Option<PathBuf> {
    match installer {
        // `npm install --global` never writes a lockfile.
        NpmInstaller::Npm => None,
        NpmInstaller::Pnpm => find_lockfile(&layout.root.join("pnpm-global"), "pnpm-lock.yaml"),
        NpmInstaller::Auto => unreachable!(),
    }
}

pub(crate) fn find_lockfile(root: &Path, file_name: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
        .map(|entry| entry.into_path())
        .min()
}

pub(crate) fn validate_global_package_identity(
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Result<()> {
    let package_json = package_manifest_path(backend, installer, layout).ok_or_else(|| {
        anyhow!(
            "global installer `{installer}` did not contain {}@{} under {}",
            backend.package(),
            version.version,
            layout.root.display()
        )
    })?;
    let bytes = std::fs::read(&package_json)
        .with_context(|| format!("reading installed package {}", package_json.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing installed package {}", package_json.display()))?;
    let actual_name = manifest.get("name").and_then(serde_json::Value::as_str);
    let actual_version = manifest.get("version").and_then(serde_json::Value::as_str);
    if actual_name != Some(backend.package()) || actual_version != Some(version.version.as_str()) {
        return Err(anyhow!(
            "installed global package identity mismatch: expected {}@{}, found {}@{}",
            backend.package(),
            version.version,
            actual_name.unwrap_or("<missing>"),
            actual_version.unwrap_or("<missing>")
        ));
    }
    validate_declared_package_bins(backend.package(), &package_json, &manifest, &layout.bin)?;
    Ok(())
}

pub(crate) fn package_manifest_path(
    backend: &NpmPackageBackend,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Option<PathBuf> {
    Some(match installer {
        NpmInstaller::Npm => {
            #[cfg(windows)]
            let modules = layout.root.join("node_modules");
            #[cfg(not(windows))]
            let modules = layout.root.join("lib/node_modules");
            modules.join(backend.package()).join("package.json")
        }
        NpmInstaller::Pnpm => {
            find_package_json(&layout.root.join("pnpm-global"), backend.package())?
        }
        NpmInstaller::Auto => unreachable!(),
    })
}

pub(crate) fn validate_declared_package_bins(
    package: &str,
    manifest_path: &Path,
    manifest: &serde_json::Value,
    bin_dir: &Path,
) -> Result<()> {
    let package_dir = manifest_path.parent().ok_or_else(|| {
        anyhow!(
            "package manifest has no parent: {}",
            manifest_path.display()
        )
    })?;
    let entries = osdk_core::backend::npm_package::package_bin_entries(manifest, package)?;
    let canonical_package = dunce::canonicalize(package_dir)?;
    for (name, target) in entries {
        if target.is_absolute()
            || target.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(anyhow!(
                "npm package {package} declares unsafe bin path {}",
                target.display()
            ));
        }
        let target = package_dir.join(target);
        let canonical_target = dunce::canonicalize(&target)?;
        if !canonical_target.is_file() || !canonical_target.starts_with(&canonical_package) {
            return Err(anyhow!(
                "npm package {package} bin `{name}` escapes {}",
                package_dir.display()
            ));
        }
        let _launcher = osdk_core::backend::npm_package::global_bin_entry(bin_dir, &name)
            .ok_or_else(|| {
                anyhow!(
                    "global npm package {package} did not publish declared bin `{name}` under {}",
                    bin_dir.display()
                )
            })?;
        #[cfg(unix)]
        {
            let metadata = std::fs::symlink_metadata(&_launcher)?;
            if !metadata.file_type().is_symlink()
                || dunce::canonicalize(&_launcher)? != canonical_target
            {
                return Err(anyhow!(
                    "global launcher `{name}` does not resolve to the target declared by {package}"
                ));
            }
        }
        #[cfg(windows)]
        {
            let extension = _launcher
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_ascii_lowercase);
            if !matches!(extension.as_deref(), Some("cmd")) {
                return Err(anyhow!(
                    "global launcher `{name}` for {package} is an opaque Windows executable"
                ));
            }
            for shadow in [
                bin_dir.join(format!("{name}.exe")),
                bin_dir.join(name.as_str()),
            ] {
                if shadow.exists() {
                    return Err(anyhow!(
                        "global launcher `{name}` for {package} has an opaque Windows shadow"
                    ));
                }
            }
            let actual = parse_windows_node_wrapper(&_launcher)?;
            if dunce::canonicalize(&actual)? != canonical_target {
                return Err(anyhow!(
                    "global launcher `{name}` does not execute the target declared by {package}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn parse_windows_node_wrapper(path: &Path) -> Result<PathBuf> {
    const MAX_WRAPPER_BYTES: u64 = 64 * 1024;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_WRAPPER_BYTES {
        return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
    }
    let text = std::fs::read_to_string(path)?;
    let mut target = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.eq_ignore_ascii_case("@echo off")
            || line.starts_with("::")
            || line.to_ascii_lowercase().starts_with("rem ")
            || line.to_ascii_lowercase().starts_with("setlocal")
            || line.to_ascii_lowercase().starts_with("endlocal")
        {
            continue;
        }
        if line.contains(['&', '|', '>', '<', '`', '!']) {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let line = line.strip_prefix('@').unwrap_or(line).trim_start();
        let lower = line.to_ascii_lowercase();
        let command_end = if lower.starts_with("node.exe ") {
            8
        } else if lower.starts_with("node ") {
            4
        } else {
            return Err(anyhow!(
                "unrecognized Windows npm wrapper {}",
                path.display()
            ));
        };
        let arguments = line[command_end..].trim_start();
        let raw_target = arguments
            .strip_prefix('\"')
            .and_then(|quoted| quoted.split_once('\"').map(|(target, _)| target))
            .ok_or_else(|| anyhow!("unquoted Windows npm target in {}", path.display()))?;
        if raw_target
            .strip_prefix("%~dp0")
            .is_some_and(|relative| relative.contains('%'))
        {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let relative = raw_target
            .strip_prefix("%~dp0")
            .ok_or_else(|| anyhow!("non-relative Windows npm target in {}", path.display()))?;
        if relative.contains(['\r', '\n', '\0']) {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let candidate = path
            .parent()
            .unwrap_or(Path::new(""))
            .join(relative.trim_start_matches(['/', '\\']));
        if target.replace(candidate).is_some() {
            return Err(anyhow!(
                "multiple commands in Windows npm wrapper {}",
                path.display()
            ));
        }
    }
    target.ok_or_else(|| anyhow!("missing command in Windows npm wrapper {}", path.display()))
}
pub(crate) fn find_package_json(root: &Path, package: &str) -> Option<PathBuf> {
    let suffix = format!("node_modules/{package}/package.json").replace('\\', "/");
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "package.json")
        .map(|entry| entry.into_path())
        .filter(|path| path.to_string_lossy().replace('\\', "/").ends_with(&suffix))
        .min()
}

pub(crate) fn manifest_bin_names(root: &Path) -> Result<Vec<String>> {
    let manifest =
        osdk_core::inventory::DynamicToolManifest::load(root).map_err(anyhow::Error::new)?;
    Ok(manifest.bins.into_iter().map(|bin| bin.name).collect())
}

pub(crate) fn manifest_bin_names_best_effort(root: &Path) -> Vec<String> {
    if !osdk_core::inventory::DynamicToolManifest::manifest_path(root).is_file() {
        return Vec::new();
    }
    manifest_bin_names(root).unwrap_or_default()
}

pub(crate) fn selected_global_bin_names(
    ctx: &osdk_core::backend::Ctx,
    backend: &NpmPackageBackend,
    incoming_root: &Path,
) -> Result<Vec<String>> {
    let mut names = manifest_bin_names_best_effort(incoming_root);
    let config = osdk_core::config::Config::load_user(&ctx.dirs.user_config_file())
        .context("reloading global config before npm shim publication")?;
    let selected = match config.global_tool_configs.get(backend.id()) {
        Some(entry) => selected_global_version(ctx, backend, entry)?,
        None => None,
    };
    if let Some(version) = selected {
        let root = backend.global_install_root_for(ctx, &version)?;
        names.extend(manifest_bin_names_best_effort(&root));
        if let Some(legacy_root) = backend.legacy_global_install_root(ctx, &version)? {
            names.extend(manifest_bin_names_best_effort(&legacy_root));
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(crate) fn selected_global_version(
    ctx: &osdk_core::backend::Ctx,
    backend: &NpmPackageBackend,
    configured: &osdk_core::config::ToolConfigEntry,
) -> Result<Option<ToolVersion>> {
    let spec = VersionSpec::parse(configured.version());
    let lock_path = ctx.dirs.user_lock_file();
    if lock_path.is_file() {
        if let Some(request) = crate::lockfile::locked_requests(&lock_path, ctx.platform)?
            .into_iter()
            .flatten()
            .find(|request| {
                request.backend == backend.id()
                    && request
                        .options
                        .get(LOCKED_NPM_SCOPE_OPTION)
                        .map(String::as_str)
                        == Some(ToolScope::Global.as_str())
            })
        {
            if let VersionSpec::Exact(version) = request.spec {
                let mut selected = ToolVersion::new(backend.id(), version);
                selected.options = request.options;
                return Ok(Some(selected));
            }
        }
    }
    let mut hint = ToolVersion::new(backend.id(), "scope-selection");
    hint.options = configured.to_request_options();
    hint.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    if let VersionSpec::Exact(version) = &spec {
        hint.version.clone_from(version);
        return Ok(Some(hint));
    }
    let installed = backend.list_installed_for(ctx, &hint)?;
    let candidates = installed
        .iter()
        .map(osdk_core::version::VersionInfo::stable)
        .collect::<Vec<_>>();
    Ok(
        osdk_core::version::select_version(&spec, &candidates).map(|version| {
            hint.version = version.version.clone();
            hint
        }),
    )
}

pub(crate) fn read_native_lock(path: &Path, installer: NpmInstaller) -> Result<NativeLockIdentity> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading native lock {}", path.display()))?;
    let (kind, format) = match installer {
        NpmInstaller::Pnpm => {
            let value: serde_yaml::Value = serde_yaml::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = yaml_lock_major(
                value
                    .get("lockfileVersion")
                    .ok_or_else(|| anyhow!("{} is missing lockfileVersion", path.display()))?,
            )?;
            ("pnpm", format!("pnpm-v{major}"))
        }
        NpmInstaller::Npm => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = value
                .get("lockfileVersion")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("{} is missing numeric lockfileVersion", path.display()))?;
            ("npm", format!("package-lock-v{major}"))
        }
        NpmInstaller::Auto => unreachable!(),
    };
    let supported = match installer {
        NpmInstaller::Pnpm => format.ends_with("-v9"),
        NpmInstaller::Npm => matches!(format.as_str(), "package-lock-v2" | "package-lock-v3"),
        NpmInstaller::Auto => unreachable!(),
    };
    if !supported {
        anyhow::bail!(
            "unsupported native lock format `{format}` produced by global installer `{installer}`"
        );
    }
    Ok(NativeLockIdentity {
        kind,
        format,
        sha256: osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256),
    })
}

pub(crate) fn yaml_lock_major(value: &serde_yaml::Value) -> Result<u64> {
    let raw = match value {
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => return Err(anyhow!("native lockfileVersion must be a string or number")),
    };
    raw.trim()
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("native lockfileVersion is malformed"))
}

pub(crate) fn inject_native_metadata(
    version: &mut ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    native: Option<&NativeLockIdentity>,
) {
    version.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    version
        .options
        .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.into());
    for key in [
        LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
        LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION,
        LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION,
    ] {
        version.options.remove(key);
    }
    if let Some(native) = native {
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            native.kind.into(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            native.format.clone(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
            native.sha256.clone(),
        );
    }
}

pub(crate) fn completed_install_matches_at(
    platform: osdk_core::platform::Platform,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    layout: &GlobalInstallLayout,
    lock_path: Option<&Path>,
) -> Result<bool> {
    let root = &layout.root;
    if !root.join(".osdk-complete").is_file() {
        return Ok(false);
    }
    if installer != NpmInstaller::Npm && lock_path.is_none() {
        return Ok(false);
    }
    let manifest = match osdk_core::inventory::DynamicToolManifest::load(root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let receipt = match osdk_core::backend::npm_package::load_npm_receipt(root) {
        Ok(receipt) => receipt,
        Err(_) => return Ok(false),
    };
    if validate_global_package_identity(backend, version, installer, layout).is_err() {
        return Ok(false);
    }
    if manifest.bins.is_empty()
        || manifest.bins.iter().any(|bin| {
            let path = root.join(&bin.path);
            let entry_is_file = path.is_file();
            dunce::canonicalize(&path)
                .ok()
                .zip(dunce::canonicalize(root).ok())
                .is_none_or(|(target, root)| {
                    !entry_is_file || !target.is_file() || !target.starts_with(root)
                })
        })
    {
        return Ok(false);
    }
    let expected_native = match lock_path {
        Some(path) if path.is_file() => match read_native_lock(path, installer) {
            Ok(native) => Some(native),
            Err(_) => return Ok(false),
        },
        Some(_) if installer != NpmInstaller::Npm => return Ok(false),
        _ => None,
    };
    let native_matches = match expected_native {
        Some(native) => {
            receipt.native_lock_format.as_ref() == Some(&native.format)
                && receipt.native_lock_sha256.as_ref() == Some(&native.sha256)
        }
        None => receipt.native_lock_format.is_none() && receipt.native_lock_sha256.is_none(),
    };
    let expected_build_policy = npm_build_policy_identity(version)?;
    let build_policy_matches = receipt.build_policy == expected_build_policy;
    let expected_identity = osdk_core::tool::InstallIdentity::new(
        &version.backend,
        &version.version,
        platform.to_string(),
        osdk_core::tool::InstallScope::Global,
        &version.options,
        vec![osdk_core::tool::InstallDependency {
            kind: osdk_core::tool::InstallDependencyKind::Runtime,
            id: "node".into(),
            version: node_version.into(),
            identity: None,
        }],
        BTreeMap::new(),
    )?;
    let bin_dir = &layout.bin;
    Ok(manifest.matches_identity(&expected_identity)
        && receipt.installer == installer.as_str()
        && receipt.node_version == node_version
        && build_policy_matches
        && native_matches
        && bin_dir.is_dir())
}

pub(crate) fn persist_global_lock(
    app: &App,
    request: &ToolRequest,
    version: &ToolVersion,
    runtime: &ManagedRuntime,
) -> Result<()> {
    let path = app.ctx.dirs.user_lock_file();
    let mut resolved = vec![(runtime.node_request.clone(), runtime.node_version.clone())];
    if let Some((manager_request, manager_version, _)) = &runtime.manager {
        resolved.push((manager_request.clone(), manager_version.clone()));
    }
    let locked_request = exact_request(request.clone(), version);
    resolved.push((locked_request, version.clone()));
    crate::lockfile::upsert_resolved_many_with_scope(
        &path,
        app.ctx.platform,
        &app.ctx.dirs,
        &resolved,
        crate::lockfile::LockScope::Global,
    )?;
    Ok(())
}

pub(crate) fn persist_global_config(
    app: &App,
    request: &ToolRequest,
    spec: &str,
    installer: NpmInstaller,
) -> Result<()> {
    let mut options = request
        .options
        .iter()
        .filter(|(key, _)| !key.starts_with("__osdk_"))
        .map(|(key, value)| Ok((key.clone(), option_value(key, value)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    options.insert(
        INSTALLER_OPTION.into(),
        osdk_core::config::ToolConfigValue::String(installer.as_str().into()),
    );
    crate::config_edit::set_global_tool_config_unlocked(
        &app.ctx,
        &request.backend,
        &osdk_core::config::StructuredToolConfig {
            version: spec.into(),
            when: None,
            options,
        },
    )
}

pub(crate) fn option_value(key: &str, value: &str) -> Result<osdk_core::config::ToolConfigValue> {
    if key == "allow_builds" {
        return Ok(match parse_npm_build_policy(Some(value))? {
            NpmBuildPolicy::Deny => osdk_core::config::ToolConfigValue::Bool(false),
            NpmBuildPolicy::AllowAll => osdk_core::config::ToolConfigValue::Bool(true),
            NpmBuildPolicy::Packages(packages) => {
                osdk_core::config::ToolConfigValue::Array(packages)
            }
        });
    }
    Ok(osdk_core::config::ToolConfigValue::String(value.into()))
}

pub(crate) fn apply_source_override(app: &mut App, tool: &str) {
    if let Some(id) = app.source_override.clone() {
        app.ctx
            .config
            .sources
            .per_tool
            .entry(tool.into())
            .or_default()
            .pin = Some(id);
    }
}

pub(crate) fn expand_alias(app: &App, request: &ToolRequest) -> Result<ToolRequest> {
    let mut effective = request.clone();
    effective.spec = VersionSpec::parse(
        &app.ctx
            .config
            .expand_alias(&request.backend, &request.spec.to_string())?,
    );
    Ok(effective)
}

pub(crate) fn find_bin_dir(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
    executable: &str,
) -> Result<PathBuf> {
    backend
        .bin_paths(ctx, version)?
        .into_iter()
        .find(|path| find_executable(std::slice::from_ref(path), executable).is_some())
        .ok_or_else(|| anyhow!("managed {executable} executable was not installed"))
}

pub(crate) fn find_executable(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.into(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    paths.iter().find_map(|directory| {
        candidates
            .iter()
            .map(|candidate| directory.join(candidate))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn unavailable_registry_error(
    manager: PackageManager,
    probes: &[RegistryProbe],
) -> anyhow::Error {
    let details = probes
        .iter()
        .map(|probe| {
            format!(
                "{} ({})",
                probe.url,
                probe.error.as_deref().unwrap_or("unreachable")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!("no usable {manager} registry; manager was not started: {details}")
}
