//! `list` command handlers (split from commands.rs).

use super::*;

pub fn completions(shell: clap_complete::Shell) -> Result<()> {
    const COMPLETION_STACK_SIZE: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("osdk-completions".into())
        .stack_size(COMPLETION_STACK_SIZE)
        .spawn(move || {
            use clap::CommandFactory;
            let mut command = crate::cli::Cli::command();
            clap_complete::generate(shell, &mut command, "osdk", &mut std::io::stdout());
        })
        .context("spawning shell completion generator")?
        .join()
        .map_err(|_| anyhow!("shell completion generator panicked"))
}

pub fn list(app: &App, tool: Option<String>) -> Result<()> {
    let backends: Vec<_> = match tool {
        Some(t) => vec![app.registry.get(&t)?],
        None => all_display_backends(app)?,
    };
    let mut any = false;
    for backend in backends {
        let installed = backend.list_installed(&app.ctx)?;
        if installed.is_empty() {
            continue;
        }
        any = true;
        println!("{}:", backend.id());
        for v in installed {
            println!("  {v}");
        }
    }
    if !any {
        println!("{}", t!("msg.no_tools_installed"));
    }
    Ok(())
}

/// All backends to display in list/current: compiled-in backends plus any
/// dynamically-installed inventory-backed backends found on disk or in the
/// merged config.
pub(crate) fn all_display_backends(app: &App) -> Result<Vec<std::sync::Arc<dyn Backend>>> {
    let mut out: Vec<std::sync::Arc<dyn Backend>> = app.registry.all().to_vec();
    let report = dynamic_scan_report(app)?;
    for id in osdk_core::shim::configured_and_installed_dynamic_ids(&app.ctx, &report) {
        out.push(app.registry.get(&id)?);
    }
    out.sort_by(|left, right| left.id().cmp(right.id()));
    out.dedup_by(|left, right| left.id() == right.id());
    Ok(out)
}

pub async fn list_remote(app: &mut App, tool: String, filter: Option<String>) -> Result<()> {
    apply_source_override(app, &tool);
    let backend = app.registry.get(&tool)?;
    let versions = backend.list_remote_versions(&app.ctx).await?;
    // Android platform-ish families publish an API level that the package name
    // does not always reveal, and the mismatch is a real trap: `android-36.1`
    // is API 36.1, not API 36, so a project needing exactly 36 that reads this
    // list has no way to tell the two apart. Annotate them.
    let api_levels = android_api_levels(app, backend.id()).await;
    let mut count = 0;
    for v in &versions {
        if !v.stable {
            continue;
        }
        if let Some(f) = &filter {
            if !v.version.starts_with(f.as_str()) {
                continue;
            }
        }
        let lts = v
            .lts
            .as_deref()
            .map(|l| format!(" (LTS: {l})"))
            .unwrap_or_default();
        let api = api_levels
            .get(&v.version)
            .map(|level| format!(" (API {level})"))
            .unwrap_or_default();
        println!("{}{}{}", v.version, api, lts);
        count += 1;
    }
    if count == 0 {
        println!("{}", t!("msg.no_matching_versions"));
    }
    Ok(())
}

/// Map each Android package version to the API level its manifest declares.
///
/// Empty for every non-Android backend and for the Android families that publish
/// no `<type-details>` (`build-tools`, `platform-tools`, ...), so callers can
/// annotate unconditionally.
///
/// Constructs `AndroidBackend` directly rather than widening the `Backend`
/// trait: a new trait method enters the vtable of all backends and is therefore
/// retained in `osdk-shim`, whose binary size is a guarded budget. This is the
/// same approach the android doctor, licence and preflight paths take.
///
/// Best-effort: a manifest that cannot be fetched yields no annotations rather
/// than failing the listing, which still has the versions the user asked for.
pub(crate) async fn android_api_levels(
    app: &App,
    backend_id: &str,
) -> std::collections::BTreeMap<String, String> {
    use osdk_core::backend::android::{AndroidBackend, ID_PREFIX, SUPPORTED_FAMILIES};

    let Some(family) = backend_id.strip_prefix(ID_PREFIX) else {
        return Default::default();
    };
    let Some(family) = SUPPORTED_FAMILIES
        .iter()
        .find(|candidate| **candidate == family)
    else {
        return Default::default();
    };
    let android = AndroidBackend::new(family);
    let Ok(manifest) = android.manifest(&app.ctx).await else {
        return Default::default();
    };
    manifest
        .family(family)
        .into_iter()
        .filter_map(|package| {
            package
                .api
                .api_level
                .as_ref()
                .map(|level| (package.version(), level.clone()))
        })
        .collect()
}

pub fn current(app: &App, tool: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backends: Vec<_> = match tool {
        Some(t) => vec![app.registry.get(&t)?],
        None => all_display_backends(app)?,
    };
    let mut any = false;
    for backend in backends {
        if let Some(av) = osdk_core::version::resolver::resolve_active(
            backend.id(),
            &cwd,
            &app.ctx.config.tools,
            backend.idiomatic_files(),
        )
        .map(|active| (active.spec, Some(active.source)))
        .or_else(|| {
            osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id())
                .map(|request| (request.spec.to_string(), None))
        }) {
            any = true;
            println!(
                "{} {} ({})",
                backend.id(),
                av.0,
                av.1.as_ref()
                    .map(describe_origin)
                    .unwrap_or_else(|| "config value".to_string())
            );
        }
    }
    // Excluded entries are reported here rather than omitted. A tool that is
    // in the config yet absent from `current` otherwise looks like a mistake in
    // the config; naming the restriction shows the filter did its job.
    for (tool, restriction) in &app.ctx.config.excluded_tools {
        any = true;
        println!(
            "{} {}",
            tool,
            t!("msg.excluded_by_platform", restriction = restriction)
        );
    }
    if !any {
        println!("{}", t!("msg.no_active"));
    }
    Ok(())
}

pub fn where_cmd(app: &App, tool: String, global: bool, bins: bool) -> Result<()> {
    let explicit_spec = requested_spec_literal(&tool).is_some();
    let req = if global {
        resolve_explicit_request(
            app,
            &tool,
            &app.ctx.config.global_tool_configs,
            &app.ctx.config.global_tools,
        )?
    } else {
        resolve_explicit_request(
            app,
            &tool,
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )?
    };
    let backend = app.registry.get(&req.backend)?;
    if global && !req.backend.starts_with("npm:") {
        anyhow::bail!("--global is only supported for npm:<package> where requests");
    }
    let npm_backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(backend.id());
    let mut selected_npm = None;
    let version = if let Some(npm) = npm_backend.as_ref() {
        if global {
            let selected = select_global_npm_version(app, &req, explicit_spec)?;
            let version = selected.version.clone();
            selected_npm = Some(selected);
            version
        } else {
            let hint = npm_scope_hint(&req, false);
            let installed = npm.list_installed_identities_for(&app.ctx, &hint)?;
            let requested = if !matches!(req.spec, VersionSpec::Exact(_)) {
                osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id())
                    .map(|request| request.spec)
                    .unwrap_or(req.spec.clone())
            } else {
                req.spec.clone()
            };
            let selected = select_installed_npm_identity(backend.id(), &requested, installed)?;
            let version = selected.version.clone();
            selected_npm = Some(selected);
            version
        }
    } else if explicit_spec && !matches!(req.spec, VersionSpec::Latest | VersionSpec::System) {
        // An explicit `tool@<selector>` asks about that selector among the
        // installed versions; it must never silently answer with the active
        // project/global version (previously `where java@21.0.12.1+1` printed
        // the path of the globally configured java@26).
        let installed = backend.list_installed(&app.ctx)?;
        let literal =
            requested_spec_literal(&tool).expect("explicit_spec is derived from the same operand");
        let selected = if backend.id() == "python" {
            osdk_core::backend::python::select_installed(&literal, &installed)
        } else {
            let infos: Vec<_> = installed
                .iter()
                .map(osdk_core::version::VersionInfo::stable)
                .collect();
            osdk_core::version::select_version(&req.spec, &infos).map(|info| info.version.clone())
        };
        selected.ok_or_else(|| anyhow!("{}@{} is not installed", req.backend, literal))?
    } else {
        let cwd = std::env::current_dir()?;
        let installed = backend.list_installed(&app.ctx)?;
        let dynamic_request = osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id());
        let resolved = osdk_core::version::resolver::resolve_active(
            backend.id(),
            &cwd,
            &app.ctx.config.tools,
            backend.idiomatic_files(),
        )
        .map(|active| (active.spec, active.is_range))
        .or_else(|| dynamic_request.map(|request| (request.spec.to_string(), false)));
        match resolved {
            Some((spec, _is_range)) if backend.id() == "python" => {
                osdk_core::backend::python::select_installed(&spec, &installed)
                    .ok_or_else(|| anyhow!("{} is not installed", req.backend))?
            }
            Some((spec, is_range)) => {
                let parsed = if is_range {
                    VersionSpec::parse_range(&spec).unwrap_or_else(|_| VersionSpec::parse(&spec))
                } else {
                    VersionSpec::parse(&spec)
                };
                match &parsed {
                    VersionSpec::Exact(version)
                        if installed.iter().any(|installed| installed == version) =>
                    {
                        version.clone()
                    }
                    _ => {
                        let infos: Vec<_> = installed
                            .iter()
                            .map(osdk_core::version::VersionInfo::stable)
                            .collect();
                        osdk_core::version::select_version(&parsed, &infos)
                            .map(|version| version.version.clone())
                            .ok_or_else(|| anyhow!("{} is not installed", req.backend))?
                    }
                }
            }
            None => installed
                .into_iter()
                .last()
                .ok_or_else(|| anyhow!("{} is not installed", req.backend))?,
        }
    };
    let dir = if let Some(npm) = npm_backend {
        let selected = selected_npm
            .take()
            .expect("npm selection retains its complete install identity");
        npm.where_install_root_for(&app.ctx, &selected)?
    } else if backend.id().contains(':') {
        let mut selected = ToolVersion::new(backend.id(), &version);
        selected.options = req.options.clone();
        let request = exact_request_for_version(&selected);
        let report = app.dynamic_scan_report()?;
        Some(
            osdk_core::shim::validated_dynamic_install(&app.ctx, &report, &request, &version)?
                .install_root()
                .to_path_buf(),
        )
    } else {
        let path = app.ctx.dirs.install_path(backend.id(), &version);
        path.exists().then_some(path)
    };
    let Some(dir) = dir else {
        return Err(anyhow!("{}@{} is not installed", backend.id(), version));
    };
    println!("{}", dir.display());
    if bins {
        let mut selected = ToolVersion::new(backend.id(), &version);
        selected.options = req.options.clone();
        // Report the decision that shim generation will actually make, config
        // included -- a preview that ignored `include` would contradict the
        // shims the very next `reshim` produces.
        let present =
            osdk_core::backend::bin_names_in_dirs(&backend.bin_paths(&app.ctx, &selected)?);
        let published = app
            .dynamic_scan_report()
            .and_then(|report| {
                routed_bin_names_for_version(&app.ctx, &report, backend.as_ref(), &selected)
            })
            .unwrap_or_else(|_| backend.bin_names(&app.ctx, &selected).unwrap_or_default());
        let withheld: Vec<&String> = present
            .iter()
            .filter(|name| !published.contains(name))
            .collect();

        println!("published ({}): {}", published.len(), published.join(", "));
        if withheld.is_empty() {
            println!("withheld (0):");
        } else {
            let names: Vec<&str> = withheld.iter().map(|name| name.as_str()).collect();
            println!("withheld ({}): {}", names.len(), names.join(", "));
            // Point at the per-tool `expose`, never the global `include`.
            // `include` is an allowlist over every tool, so the old advice
            // recovered one command by withholding all the others -- 646 shims
            // down to zero (docs/bugs/008).
            println!(
                "  expose one with `osdk config set shims.{}.expose \"{}\"`",
                backend.id(),
                names[0]
            );
        }
    }
    Ok(())
}
