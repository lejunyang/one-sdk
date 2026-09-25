//! `shims` command handlers (split from commands.rs).

use super::*;

pub fn reshim(app: &App) -> Result<()> {
    let mut total = 0;
    for backend in all_display_backends(app)? {
        let dynamic_request = osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id());
        let installed = match backend.list_installed(&app.ctx) {
            Ok(installed) => installed,
            Err(error) if dynamic_request.is_none() && backend.id().contains(':') => {
                tracing::warn!(
                    backend = backend.id(),
                    error = %error,
                    "skipping unconfigured dynamic backend during reshim"
                );
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        // A configured dynamic backend with nothing installed is the one case
        // worth saying out loud. It is not an error -- the install may simply be
        // absent -- but it is also what a rejected install looks like from here,
        // and those two are indistinguishable in silence. `osdk doctor` can then
        // be pointed at the tool to get the actual reason.
        if installed.is_empty() && dynamic_request.is_some() {
            tracing::info!(
                backend = backend.id(),
                "configured dynamic backend has no usable install; no shim generated"
            );
        }
        for version in installed {
            let mut tv = ToolVersion::new(backend.id(), &version);
            if backend.id().contains(':') {
                let Some(request) = dynamic_request.as_ref() else {
                    continue;
                };
                if !request_selects_installed_version(app, backend.as_ref(), request, &version)? {
                    continue;
                }
                tv.options = request.options.clone();
            }
            total += generate_shims_for(app, backend.as_ref(), &tv)?;
        }
    }
    reconcile_managed_shims(app)?;
    println!("{}", t!("msg.reshimmed", count = total));
    Ok(())
}

pub(crate) fn reconcile_managed_shims(app: &App) -> Result<()> {
    let owners = installed_shim_owners(app)?;
    let expected = owners
        .iter()
        .filter(|(name, owner_ids)| !is_real_shim_conflict(name, owner_ids))
        .map(|(name, _)| name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let shims = app.ctx.dirs.shims();
    let read_dir = match std::fs::read_dir(&shims) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow::Error::new(osdk_core::Error::io(&shims, error))),
    };
    let mut names = std::collections::BTreeSet::new();
    for entry in read_dir {
        let entry =
            entry.map_err(|error| anyhow::Error::new(osdk_core::Error::io(&shims, error)))?;
        if !entry.path().is_file() && !entry.path().symlink_metadata()?.file_type().is_symlink() {
            continue;
        }
        names.insert(executable_basename(&entry.file_name().to_string_lossy()));
    }
    for name in names {
        if !expected.contains(&name) {
            osdk_core::shim::remove_managed_shim(&app.ctx.dirs, &name)?;
        }
    }
    Ok(())
}

/// Remove old bin shims from one backend only when no different installed
/// backend still owns that bin name. This deliberately ignores the current
/// backend's old versions and does not consult its possibly stale config pin.
pub(crate) fn remove_stale_shims_without_other_owners(
    app: &App,
    current_backend: &str,
    old_names: &[String],
    new_names: &[String],
) -> Result<()> {
    let owners = installed_shim_owners(app)?;
    for name in old_names.iter().filter(|name| !new_names.contains(name)) {
        if !has_other_shim_owner(&owners, name, current_backend) {
            osdk_core::shim::remove_managed_shim(&app.ctx.dirs, name)?;
        }
    }
    Ok(())
}

pub(crate) fn has_other_shim_owner(
    owners: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    name: &str,
    current_backend: &str,
) -> bool {
    owners
        .get(name)
        .is_some_and(|owner_ids| owner_ids.iter().any(|owner| owner != current_backend))
}

/// Generate shims for all bin names a version exposes. Returns count.
/// Generate shims for `tv`, plus for anything `backend` installed on its own
/// behalf while installing it (declared dependencies).
///
/// Dependencies are shimmed first: when a name is shared, the precedence rules
/// decide the owner regardless of order, but doing the requested tool last keeps
/// its diagnostics the ones the user sees.
pub(crate) fn generate_shims_including_dependencies(
    app: &App,
    backend: &dyn Backend,
    tv: &ToolVersion,
) -> Result<usize> {
    let mut total = 0;
    for side in backend.take_side_installed() {
        let side_backend = match app.registry.get(&side.backend) {
            Ok(b) => b,
            Err(_) => continue,
        };
        total += generate_shims_for(app, side_backend.as_ref(), &side)?;
    }
    total += generate_shims_for(app, backend, tv)?;
    Ok(total)
}

pub(crate) fn generate_shims_for(
    app: &App,
    backend: &dyn Backend,
    tv: &ToolVersion,
) -> Result<usize> {
    let shim_bin = match osdk_core::shim::find_shim_binary(&app.ctx.dirs) {
        Some(b) => b,
        None => {
            // Not fatal: warn once. Activation via PATH still works.
            eprintln!("{}", t!("msg.shim_bin_missing"));
            return Ok(0);
        }
    };
    let dynamic_report = app.dynamic_scan_report()?;
    let names = routed_bin_names_for_version(&app.ctx, &dynamic_report, backend, tv)?;
    ensure_no_shim_conflicts(app, backend.id(), &names)?;
    let owners = installed_shim_owners(app)?;
    let mut count = 0;
    for name in names {
        // When a name is shared by several tools of one ecosystem, only the
        // curated winner may own the shim; the other family would otherwise
        // overwrite it depending on install order.
        if let Some(owner_ids) = owners.get(&name) {
            if let Some(winner) = osdk_core::shim::precedence_winner(&name, owner_ids) {
                if winner != backend.id() {
                    continue;
                }
            }
        }
        osdk_core::shim::generate_shim(&app.ctx.dirs, &name, &shim_bin)?;
        count += 1;
    }
    Ok(count)
}

pub(crate) fn describe_origin(origin: &osdk_core::version::resolver::VersionOrigin) -> String {
    use osdk_core::version::resolver::VersionOrigin::*;
    match origin {
        ProjectConfig(p) => format!("project {}", p.display()),
        ToolVersions(p) => format!(".tool-versions {}", p.display()),
        IdiomaticFile(p) => format!("{}", p.display()),
        ProjectMetadata(p) => format!("{}", p.display()),
        GlobalConfig => "global config".to_string(),
    }
}

pub(crate) fn dynamic_scan_report(app: &App) -> Result<std::sync::Arc<ScanReport>> {
    app.dynamic_scan_report()
}

pub(crate) fn routed_bin_names_for_version(
    ctx: &osdk_core::backend::Ctx,
    dynamic_report: &ScanReport,
    backend: &dyn Backend,
    version: &ToolVersion,
) -> Result<Vec<String>> {
    // Both shim generation and the reconciliation pass read names through
    // here, so filtering once keeps them from disagreeing.
    let keep = |names: Vec<String>| {
        names
            .into_iter()
            .filter(|name| {
                osdk_core::shim::shim_is_enabled(&ctx.config.settings.shims, &version.backend, name)
            })
            .collect::<Vec<_>>()
    };
    if version.backend.contains(':') {
        let request = exact_request_for_version(version);
        let install = osdk_core::shim::validated_dynamic_install(
            ctx,
            dynamic_report,
            &request,
            &version.version,
        )?;
        // Ownership travels with the name: a prefix shared with a dependency
        // closure withholds the closure's commands unless they are included.
        return Ok(install
            .bin_names()
            .into_iter()
            .filter(|name| {
                osdk_core::shim::shim_is_enabled_for(
                    &ctx.config.settings.shims,
                    &version.backend,
                    name,
                    install.owns(name),
                )
            })
            .collect());
    }
    let names = osdk_core::shim::routed_bin_names(ctx, backend, version)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    Ok(keep(names.into_iter().collect()))
}

pub(crate) fn managed_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
    request: Option<&ToolRequest>,
) -> Result<Vec<std::path::PathBuf>> {
    if version.backend.contains(':') {
        let fallback_request;
        let request = match request {
            Some(request) => request,
            None => {
                fallback_request = exact_request_for_version(version);
                &fallback_request
            }
        };
        let report = osdk_core::shim::scan_dynamic_installs(ctx)?;
        return Ok(osdk_core::shim::validated_dynamic_install(
            ctx,
            &report,
            request,
            &version.version,
        )?
        .bin_paths());
    }
    let mut paths = backend.bin_paths(ctx, version)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn exact_request_for_version(version: &ToolVersion) -> ToolRequest {
    ToolRequest {
        backend: version.backend.clone(),
        spec: VersionSpec::Exact(version.version.clone()),
        options: version.options.clone(),
    }
}

pub(crate) fn request_selects_installed_version(
    app: &App,
    backend: &dyn Backend,
    request: &ToolRequest,
    version: &str,
) -> Result<bool> {
    let expanded = app
        .ctx
        .config
        .expand_alias(backend.id(), &request.spec.to_string())?;
    let spec = request_spec_after_alias(&request.spec, &expanded);
    if let VersionSpec::Exact(selected) = spec {
        return Ok(selected == version);
    }
    request_selects_version_from_candidates(backend.id(), &spec, version, || {
        Ok(backend.list_installed(&app.ctx)?)
    })
}

pub(crate) fn request_selects_version_from_candidates(
    backend_id: &str,
    spec: &VersionSpec,
    version: &str,
    installed: impl FnOnce() -> Result<Vec<String>>,
) -> Result<bool> {
    if let VersionSpec::Exact(selected) = spec {
        return Ok(selected == version);
    }
    let installed = installed()?;
    let selected = if backend_id == "python" {
        osdk_core::backend::python::select_installed(&spec.to_string(), &installed)
    } else {
        let infos = installed
            .iter()
            .map(osdk_core::version::VersionInfo::stable)
            .collect::<Vec<_>>();
        osdk_core::version::select_version(spec, &infos).map(|version| version.version.clone())
    };
    Ok(selected.as_deref() == Some(version))
}

pub(crate) fn request_spec_after_alias(original: &VersionSpec, expanded: &str) -> VersionSpec {
    if matches!(original, VersionSpec::Range(_)) {
        VersionSpec::parse_range(expanded).unwrap_or_else(|_| VersionSpec::parse(expanded))
    } else {
        VersionSpec::parse(expanded)
    }
}

pub(crate) fn requested_spec_literal(tool: &str) -> Option<String> {
    let raw = tool.trim();
    if let Some(rest) = raw.strip_prefix("npm:") {
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        if let Some(package) = rest.strip_prefix('@') {
            let (_scope, package_and_version) = package.split_once('/')?;
            let (_name, version) = package_and_version.split_once('@')?;
            return (!version.trim().is_empty()).then(|| version.trim().to_string());
        }
        let (_name, version) = rest.split_once('@')?;
        return (!version.trim().is_empty()).then(|| version.trim().to_string());
    }
    match raw.split_once('@') {
        Some((_, version)) if !version.trim().is_empty() => Some(version.trim().to_string()),
        _ => None,
    }
}

pub(crate) fn installed_shim_owners(
    app: &App,
) -> Result<std::sync::Arc<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>>
{
    if let Some(cached) = app.cached_shim_owners() {
        return Ok(cached);
    }
    let mut owners =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    let dynamic_report = dynamic_scan_report(app)?;
    // `dynamic_bin_ownership` yields one entry per published command, and a conda
    // prefix publishes hundreds of them (m2-gawk alone accounts for over 800
    // scans here). The selection answer only depends on the tool and version, so
    // resolve each pair once instead of once per name.
    let mut selection_memo = std::collections::HashMap::<(String, String), bool>::new();
    // Same predicate as generation, so reconciliation cannot delete a shim that
    // generation just created for an explicitly exposed command (docs/bugs/008).
    for (name, candidates) in osdk_core::shim::dynamic_bin_ownership_with_settings(
        &dynamic_report,
        &app.ctx.config.settings.shims,
    ) {
        let configured_owners = candidates
            .into_iter()
            .filter_map(|candidate| {
                let request = osdk_core::shim::dynamic_request_from_config(
                    &app.ctx,
                    &candidate.canonical_id,
                )?;
                let install = dynamic_report.installs.iter().find(|install| {
                    install.install_root == candidate.install_root
                        && install.canonical_id == candidate.canonical_id
                })?;
                let version = install.manifest.identity.version.as_str();
                let memo_key = (candidate.canonical_id.clone(), version.to_string());
                let selected = match selection_memo.get(&memo_key) {
                    Some(known) => *known,
                    None => {
                        let resolved = app
                            .registry
                            .get(&candidate.canonical_id)
                            .ok()
                            .and_then(|backend| {
                                request_selects_installed_version(
                                    app,
                                    backend.as_ref(),
                                    &request,
                                    version,
                                )
                                .ok()
                            })
                            .unwrap_or(false);
                        selection_memo.insert(memo_key, resolved);
                        resolved
                    }
                };
                let mut selected_version = ToolVersion::new(&request.backend, version);
                selected_version.options = request.options.clone();
                let selected_root = osdk_core::shim::validated_dynamic_install(
                    &app.ctx,
                    &dynamic_report,
                    &request,
                    version,
                )
                .ok()
                .map(|install| install.install_root().to_path_buf());
                // Expected shims must be filtered exactly as generation filters
                // them, or reconciliation deletes what generation just wrote --
                // or keeps a shim the user has since excluded.
                let owned = install
                    .manifest
                    .bins
                    .iter()
                    .find(|bin| bin.name == name)
                    .is_none_or(|bin| bin.owned);
                if !osdk_core::shim::shim_is_enabled_for(
                    &app.ctx.config.settings.shims,
                    &candidate.canonical_id,
                    &name,
                    owned,
                ) {
                    return None;
                }
                (selected && selected_root.as_ref() == Some(&candidate.install_root))
                    .then_some(candidate.canonical_id)
            })
            .collect::<std::collections::BTreeSet<_>>();
        if !configured_owners.is_empty() {
            owners.insert(name, configured_owners);
        }
    }
    let cwd = std::env::current_dir()?;
    for backend in all_display_backends(app)? {
        if backend.id().contains(':') {
            continue;
        }
        let mut selected_versions = std::collections::BTreeSet::new();
        let installed = backend.list_installed(&app.ctx)?;
        if let Some((active_spec, active_is_range)) = osdk_core::version::resolver::resolve_active(
            backend.id(),
            &cwd,
            &app.ctx.config.tools,
            backend.idiomatic_files(),
        )
        .map(|active| (active.spec, active.is_range))
        {
            let expanded = app
                .ctx
                .config
                .expand_alias(backend.id(), &active_spec)
                .unwrap_or(active_spec);
            let selected = if backend.id() == "python" {
                osdk_core::backend::python::select_installed(&expanded, &installed)
            } else {
                let spec = if active_is_range {
                    VersionSpec::parse_range(&expanded)
                        .unwrap_or_else(|_| VersionSpec::parse(&expanded))
                } else {
                    VersionSpec::parse(&expanded)
                };
                match &spec {
                    VersionSpec::Exact(version)
                        if installed.iter().any(|installed| installed == version) =>
                    {
                        Some(version.clone())
                    }
                    _ => {
                        let infos: Vec<_> = installed
                            .iter()
                            .map(osdk_core::version::VersionInfo::stable)
                            .collect();
                        osdk_core::version::select_version(&spec, &infos)
                            .map(|version| version.version.clone())
                    }
                }
            };
            if let Some(version) = selected {
                selected_versions.insert(version);
            }
        } else {
            selected_versions.extend(installed);
        }
        for version in selected_versions {
            let version = ToolVersion::new(backend.id(), version);
            for name in
                routed_bin_names_for_version(&app.ctx, &dynamic_report, backend.as_ref(), &version)?
            {
                owners
                    .entry(name)
                    .or_default()
                    .insert(backend.id().to_string());
            }
        }
    }
    Ok(app.cache_shim_owners(owners))
}

pub(crate) fn ensure_no_shim_conflicts(
    app: &App,
    backend_id: &str,
    names: &[String],
) -> Result<()> {
    let owners = installed_shim_owners(app)?;
    for name in names {
        let owner_ids = owners.get(name).cloned().unwrap_or_else(|| {
            let mut owner_ids = std::collections::BTreeSet::new();
            owner_ids.insert(backend_id.to_string());
            owner_ids
        });
        if is_real_shim_conflict(name, &owner_ids) {
            osdk_core::shim::remove_managed_shim(&app.ctx.dirs, name)?;
            return Err(anyhow!(t!(
                "err.shim_generation_conflict",
                name = name,
                owners = owner_ids.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

pub(crate) fn is_real_shim_conflict(
    name: &str,
    owner_ids: &std::collections::BTreeSet<String>,
) -> bool {
    if owner_ids.len() <= 1 {
        return false;
    }
    if matches!(name, "npm" | "npx")
        && owner_ids
            .iter()
            .all(|owner_id| matches!(owner_id.as_str(), "node" | "npm"))
    {
        return false;
    }
    if osdk_core::shim::precedence_winner(name, owner_ids).is_some() {
        return false;
    }
    true
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", U[0])
    } else {
        format!("{f:.1} {}", U[i])
    }
}
