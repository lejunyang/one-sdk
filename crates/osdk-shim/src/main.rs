//! `osdk-shim`: the tiny launcher every shim points at.
//!
//! It learns which tool to run from `argv[0]` (the shim's own name), resolves
//! the active version for the current directory (walking up config files), then
//! `exec`s the real binary from that version's install dir. Ordinary tools stay
//! on a synchronous hot path; dependency-fetching package-manager commands
//! create a short-lived current-thread runtime for a fresh registry preflight.

use std::path::PathBuf;
use std::process::Command;

use osdk_core::backend::registry::Registry;
use osdk_core::config::Config;
use osdk_core::dirs::Dirs;
use osdk_core::inventory::ScanReport;
use osdk_core::package_registry::{
    manager_for_command, plan, registry_env, should_plan, PackageManager, RegistryPlan,
};
use osdk_core::platform::Platform;
use osdk_core::version::resolver::resolve_active;
use osdk_core::version::{select_version, ToolVersion, VersionSpec};

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let lang = osdk_core::i18n::detect(None, |key| std::env::var(key).ok());
    osdk_core::i18n::set_lang(lang);
    // The tool name is argv[0]'s basename (e.g. the shim named "node"), unless
    // invoked directly as "osdk-shim <tool> <args...>" (windows .cmd wrapper).
    let (tool_name, forward_args) = parse_invocation(&args);
    let tool_name = match tool_name {
        Some(t) => t,
        None => {
            eprintln!("osdk-shim: could not determine tool name from argv[0]");
            return 1;
        }
    };
    if std::env::var_os("OSDK_SHIM_ACTIVE").is_some() {
        eprintln!("osdk-shim: recursive shim invocation for `{tool_name}`");
        return 126;
    }

    let dirs = match Dirs::resolve() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Err(error) = ensure_project_config_trusted(&dirs, &cwd) {
        eprintln!("osdk-shim: {error}");
        return 1;
    }
    let config = match Config::load(&dirs.user_config_file(), &cwd) {
        Ok(config) => config,
        Err(error) => {
            // A malformed configuration may contain execution-affecting
            // registry policy. Silently replacing it with public defaults
            // would make the shim behave differently from `osdk exec` and
            // could send requests to an unintended endpoint.
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };
    if std::env::var_os("OSDK_LANG").is_none() {
        if let Some(lang) = config
            .settings
            .lang
            .as_deref()
            .and_then(osdk_core::i18n::Lang::parse)
        {
            osdk_core::i18n::set_lang(lang);
        }
    }

    let registry = match Registry::load(&dirs) {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };

    // Build the (sync) context up front; needed to resolve which backend owns
    // this tool name by scanning installed bin names.
    let platform = Platform::current();
    let tools = config.tools.clone();
    let idiomatic_probe_cwd = cwd.clone();
    let ctx = make_ctx(dirs.clone(), platform, config);
    let dynamic_report = match osdk_core::shim::scan_dynamic_installs(&ctx) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };

    // Find which backend owns this tool name (its id, or one of the executables
    // an installed version provides, e.g. pip -> python, npm -> node).
    let backend = match owning_backend(
        &registry,
        &ctx,
        &idiomatic_probe_cwd,
        &tool_name,
        Some(&dynamic_report),
    ) {
        Some(b) => b,
        None => {
            if let Some((request, version)) =
                configured_dynamic_request_for_bin(&registry, &ctx, &tool_name, &dynamic_report)
            {
                if let Err(error) = osdk_core::shim::validated_dynamic_install(
                    &ctx,
                    &dynamic_report,
                    &request,
                    &version,
                ) {
                    eprintln!("osdk-shim: {error}");
                    return 1;
                }
            }
            eprintln!("osdk-shim: no backend provides `{tool_name}`");
            return 127;
        }
    };

    // Resolve the active version spec for this backend.
    let dynamic_request = osdk_core::shim::dynamic_request_from_config(&ctx, backend.id());
    let active = resolve_active(
        backend.id(),
        &idiomatic_probe_cwd,
        &tools,
        backend.idiomatic_files(),
    )
    .map(|active| (active.spec, active.is_range))
    .or_else(|| {
        dynamic_request
            .as_ref()
            .map(|request| (request.spec.to_string(), false))
    });
    let (spec, is_range) = match active {
        Some(active) => active,
        None => {
            eprintln!(
                "osdk-shim: no version of `{}` selected (set one with `osdk use {}@<version>`)",
                backend.id(),
                backend.id()
            );
            return 1;
        }
    };

    // Resolve spec -> concrete installed version (offline: pick from installed).
    let expanded_spec = match ctx.config.expand_alias(backend.id(), &spec) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    let parsed_dynamic_spec = dynamic_request
        .as_ref()
        .map(|_| VersionSpec::parse(strip_distribution_prefix(&expanded_spec)));
    let version = match parsed_dynamic_spec {
        Some(VersionSpec::Exact(version)) => Some(version),
        _ => resolve_installed(&ctx, backend.as_ref(), &expanded_spec, is_range),
    };
    let version = match version {
        Some(v) => v,
        None => {
            eprintln!(
                "osdk-shim: `{}@{}` is not installed (run `osdk install {}@{}`)",
                backend.id(),
                spec,
                backend.id(),
                spec
            );
            return 1;
        }
    };

    let mut tv = ToolVersion::new(backend.id(), &version);
    let dynamic_install = if let Some(request) = dynamic_request.as_ref() {
        tv.options = request.options.clone();
        match osdk_core::shim::validated_dynamic_install(&ctx, &dynamic_report, request, &version) {
            Ok(install) => Some(install),
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    } else {
        None
    };
    let bin_dirs = dynamic_install
        .as_ref()
        .map(osdk_core::shim::ValidatedDynamicInstall::bin_paths)
        .unwrap_or_else(|| managed_bin_paths(&ctx, backend.as_ref(), &tv));

    let (executable_name, alias_subcommand) = routed_launcher(&tool_name, backend.id());
    let exe = dynamic_install
        .as_ref()
        .and_then(|install| install.executable(executable_name))
        .or_else(|| {
            dynamic_install
                .is_none()
                .then(|| managed_executable_path(&ctx, backend.as_ref(), &tv, executable_name))
                .flatten()
        });
    let exe = match exe {
        Some(p) => p,
        None => {
            eprintln!(
                "osdk-shim: `{executable_name}` not found in {}@{}",
                backend.id(),
                version
            );
            return 127;
        }
    };

    let mut exec_env = match backend.exec_env(&ctx, &tv) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    // Activated shells put the shim directory on PATH. Never let lifecycle
    // subprocesses re-enter the shim: expose the owning backend's real bins
    // (plus Node for JavaScript launchers) ahead of the inherited PATH.
    // Remove both lexical and canonical matches so symlinked activation paths
    // cannot retain the shim directory under a different spelling.
    remove_env_path(&mut exec_env, &ctx.dirs.shims());
    if backend.id().starts_with("npm:") {
        let node_backend = registry.get("node").unwrap();
        let node_bin_paths = match manifest_bound_node_bin_paths(
            &ctx,
            backend.id(),
            dynamic_install
                .as_ref()
                .expect("configured npm backend has validated inventory"),
            &*node_backend,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        };
        let mut exact_runtime_path = node_bin_paths;
        exact_runtime_path.extend(bin_dirs.clone());
        prepend_env_path(&mut exec_env, exact_runtime_path);
    } else if backend.id() == "pnpm" || backend.id() == "yarn" {
        let node_backend = registry.get("node").unwrap();
        let active_node = resolve_active(
            "node",
            &idiomatic_probe_cwd,
            &tools,
            &[".nvmrc", ".node-version"],
        );
        let node_version = active_node
            .as_ref()
            .and_then(|active| {
                resolve_installed(&ctx, &*node_backend, &active.spec, active.is_range)
            })
            .or_else(|| node_backend.list_installed(&ctx).ok()?.into_iter().last());
        let Some(node_version) = node_version else {
            eprintln!(
                "osdk-shim: {}",
                osdk_core::t!("err.shim_managed_node_required", tool = backend.id())
            );
            return 1;
        };
        let node = ToolVersion::new("node", node_version);
        prepend_env_path(
            &mut exec_env,
            managed_bin_paths(&ctx, &*node_backend, &node),
        );
        prepend_env_path(&mut exec_env, bin_dirs);
    } else {
        prepend_env_path(&mut exec_env, bin_dirs);
    }

    if let Some(manager) = package_manager_for_backend(backend.id(), &tool_name, &version) {
        if should_plan(manager, &tool_name, forward_args) {
            if let Err(error) = apply_registry_preflight(
                &ctx,
                &cwd,
                manager,
                &tool_name,
                forward_args,
                &mut exec_env,
            ) {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    }

    exec_env.insert("OSDK_SHIM_ACTIVE".into(), tool_name);
    let routed_args;
    let exec_args = if let Some(subcommand) = alias_subcommand {
        routed_args = std::iter::once(subcommand.to_string())
            .chain(forward_args.iter().cloned())
            .collect::<Vec<_>>();
        routed_args.as_slice()
    } else {
        forward_args
    };
    exec(&exe, exec_args, &exec_env)
}

fn manifest_bound_node_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    npm_backend: &str,
    install: &osdk_core::shim::ValidatedDynamicInstall,
    node_backend: &dyn osdk_core::backend::Backend,
) -> Result<Vec<PathBuf>, String> {
    let required = || osdk_core::t!("err.shim_managed_node_required", tool = npm_backend);
    let recorded = install
        .identity()
        .dependencies
        .iter()
        .find(|dependency| {
            dependency.kind == osdk_core::tool::InstallDependencyKind::Runtime
                && dependency.id == "node"
        })
        .map(|dependency| dependency.version.as_str())
        .ok_or_else(&required)?;
    let node_version = match VersionSpec::parse(recorded) {
        // The manifest records the concrete runtime identity selected during
        // installation. Reject aliases, ranges, prefixes, and even textual
        // normalization so execution cannot drift to another installed Node.
        VersionSpec::Exact(version) if version == recorded => version,
        _ => return Err(required()),
    };
    let installed = node_backend.list_installed(ctx).map_err(|_| required())?;
    if !installed.iter().any(|version| version == &node_version) {
        return Err(required());
    }
    let node = ToolVersion::new("node", node_version);
    let bin_paths = managed_bin_paths(ctx, node_backend, &node);
    if find_exe(&bin_paths, "node").is_none() {
        return Err(required());
    }
    Ok(bin_paths)
}

fn routed_launcher<'a>(tool_name: &'a str, backend: &str) -> (&'a str, Option<&'static str>) {
    match (backend, tool_name) {
        ("pnpm", "pnpx") => ("pnpm", Some("dlx")),
        ("bun", "bunx") => ("bun", Some("x")),
        _ => (tool_name, None),
    }
}

fn ensure_project_config_trusted(dirs: &Dirs, cwd: &std::path::Path) -> Result<(), String> {
    let Some(project_config) = osdk_core::trust::project_config(cwd).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    if !osdk_core::trust::requires_trust(&project_config).map_err(|e| e.to_string())? {
        return Ok(());
    }
    let trusted_paths = std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS");
    if osdk_core::trust::is_trusted(&dirs.config, &project_config, trusted_paths.as_ref())
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    Err(osdk_core::t!(
        "err.untrusted_config",
        path = project_config.display()
    ))
}

fn package_manager_for_backend(
    backend: &str,
    executable_alias: &str,
    backend_version: &str,
) -> Option<PackageManager> {
    let belongs_to_manager = match backend {
        // npm/npx may be supplied by the independent npm backend or by a Node
        // installation. npm registry behavior is version-independent.
        "npm" | "node" => matches!(executable_alias, "npm" | "npx"),
        "pnpm" => matches!(executable_alias, "pnpm" | "pnpx"),
        "yarn" => matches!(executable_alias, "yarn" | "yarnpkg"),
        "bun" => matches!(executable_alias, "bun" | "bunx"),
        "deno" => executable_alias == "deno",
        _ => false,
    };
    belongs_to_manager
        .then(|| manager_for_command(executable_alias, Some(backend_version)))
        .flatten()
}

fn apply_registry_preflight(
    ctx: &osdk_core::backend::Ctx,
    cwd: &std::path::Path,
    manager: PackageManager,
    executable_alias: &str,
    args: &[String],
    exec_env: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| osdk_core::t!("err.registry_preflight_runtime", error = error))?;
    let registry_plan = runtime
        .block_on(plan(ctx, cwd, manager, executable_alias, args, |key| {
            std::env::var(key).ok()
        }))
        .map_err(|error| osdk_core::t!("err.registry_preflight", error = error))?;
    match registry_plan {
        RegistryPlan::PassThrough { .. } => Ok(()),
        RegistryPlan::Selected { url, .. } => {
            exec_env.insert(registry_env(manager).into(), url);
            Ok(())
        }
        RegistryPlan::Unavailable { probes } => {
            let details = probes
                .iter()
                .map(|probe| {
                    probe.error.as_deref().map_or_else(
                        || probe.url.clone(),
                        |error| format!("{}: {error}", probe.url),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            if details.is_empty() {
                Err(osdk_core::t!(
                    "err.registry_unavailable",
                    executable = executable_alias
                ))
            } else {
                Err(osdk_core::t!(
                    "err.registry_unavailable_details",
                    executable = executable_alias,
                    details = details
                ))
            }
        }
    }
}

fn prepend_env_path(env: &mut std::collections::BTreeMap<String, String>, paths: Vec<PathBuf>) {
    let existing = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let mut combined = paths;
    combined.extend(std::env::split_paths(&existing));
    if let Ok(value) = std::env::join_paths(combined) {
        env.insert("PATH".into(), value.to_string_lossy().into_owned());
    }
}

fn remove_env_path(env: &mut std::collections::BTreeMap<String, String>, remove: &std::path::Path) {
    let existing = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let remove_canonical = std::fs::canonicalize(remove).ok();
    let retained = std::env::split_paths(&existing)
        .filter(|path| {
            path != remove
                && remove_canonical.as_ref().is_none_or(|canonical| {
                    std::fs::canonicalize(path)
                        .map(|path| path != *canonical)
                        .unwrap_or(true)
                })
        })
        .collect::<Vec<_>>();
    if let Ok(value) = std::env::join_paths(retained) {
        env.insert("PATH".into(), value.to_string_lossy().into_owned());
    }
}

/// Determine the tool name and args to forward.
fn parse_invocation(args: &[String]) -> (Option<String>, &[String]) {
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("");
    let base = basename_no_ext(argv0);
    if base == "osdk-shim" {
        // Direct form: osdk-shim <tool> <args...>
        let tool = args.get(1).map(|s| basename_no_ext(s));
        let rest = if args.len() > 2 { &args[2..] } else { &[] };
        (tool, rest)
    } else if base.is_empty() {
        (None, &[])
    } else {
        (Some(base), &args[1..])
    }
}

fn basename_no_ext(p: &str) -> String {
    let name = std::path::Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    #[cfg(windows)]
    {
        for ext in [".exe", ".cmd", ".bat"] {
            if name.to_ascii_lowercase().ends_with(ext) {
                return name[..name.len() - ext.len()].to_string();
            }
        }
    }
    name
}

/// Find the backend that owns a tool name. First checks backend ids directly,
/// then scans each backend's exposed bin names across installed versions so
/// tools like `pip`, `npm`, `pnpx`, `gofmt`, `cargo` route to the right SDK.
/// Also scans dynamically-installed `github:owner/repo` backends.
fn owning_backend(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    cwd: &std::path::Path,
    tool_name: &str,
    dynamic_report: Option<&ScanReport>,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
    if matches!(tool_name, "npm" | "npx") {
        let npm = registry.get("npm").ok()?;
        // An explicit independent npm selection is authoritative, including
        // when its selected version is missing: do not silently fall back to
        // the bundled copy and hide a broken project pin.
        if resolve_active("npm", cwd, &ctx.config.tools, npm.idiomatic_files()).is_some() {
            return Some(npm);
        }

        // Node intentionally does not claim npm/npx in `bin_names`, because
        // the independent npm backend owns those public tool IDs. A routing
        // shim may still dispatch to the selected Node installation's bundled
        // launcher when no independent npm version is selected.
        let node = registry.get("node").ok()?;
        if let Some(active) = resolve_active("node", cwd, &ctx.config.tools, node.idiomatic_files())
        {
            if let Some(version) =
                resolve_installed(ctx, node.as_ref(), &active.spec, active.is_range)
            {
                let version = ToolVersion::new("node", version);
                if node
                    .bin_paths(ctx, &version)
                    .ok()
                    .and_then(|paths| find_exe(&paths, tool_name))
                    .is_some()
                {
                    return Some(node);
                }
            }
        }

        return (tool_name == "npm").then_some(npm);
    }
    if let Ok(b) = registry.get(tool_name) {
        return Some(b);
    }
    if let Some(report) = dynamic_report {
        if let Some(backend) = dynamic_backend_for_bin(registry, ctx, report, tool_name) {
            return Some(backend);
        }
    }
    // Scan compiled-in backends' installed versions' bin names.
    for backend in registry.all() {
        if let Ok(versions) = backend.list_installed(ctx) {
            for v in versions {
                let tv = ToolVersion::new(backend.id(), &v);
                if let Ok(names) = backend.bin_names(ctx, &tv) {
                    if names.iter().any(|n| n == tool_name) {
                        return Some(backend.clone());
                    }
                }
            }
        }
    }
    None
}

fn dynamic_backend_for_bin(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    report: &ScanReport,
    tool_name: &str,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
    let owners = osdk_core::shim::dynamic_bin_ownership(report);
    let candidates = owners.get(tool_name)?;
    let owner_ids = candidates
        .iter()
        .map(|candidate| candidate.canonical_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let configured = osdk_core::shim::configured_dynamic_ids(ctx, report);
    let matching_configured = owner_ids
        .iter()
        .filter(|owner_id| {
            configured
                .iter()
                .any(|configured_id| configured_id == *owner_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if matching_configured.len() == 1 {
        return registry.get(&matching_configured[0]).ok();
    }
    if owner_ids.len() == 1 {
        return registry.get(owner_ids.iter().next()?).ok();
    }
    let conflicting = if matching_configured.is_empty() {
        owner_ids.into_iter().collect::<Vec<_>>()
    } else {
        matching_configured
    };
    let owners = conflicting.join(", ");
    if !owners.is_empty() {
        eprintln!(
            "osdk-shim: {}",
            osdk_core::t!(
                "err.shim_dynamic_route_conflict",
                tool = tool_name,
                owners = owners
            )
        );
    }
    None
}

fn configured_dynamic_request_for_bin(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    tool_name: &str,
    report: &ScanReport,
) -> Option<(osdk_core::version::ToolRequest, String)> {
    for backend_id in osdk_core::shim::configured_dynamic_ids(ctx, report) {
        let Some(request) = osdk_core::shim::dynamic_request_from_config(ctx, &backend_id) else {
            continue;
        };
        let VersionSpec::Exact(version) = request.spec.clone() else {
            continue;
        };
        if registry.get(&backend_id).is_err() {
            continue;
        }
        let directly_configured = ctx.config.tools.iter().any(|(key, value)| {
            let direct_id = osdk_core::inventory::canonical_dynamic_id(key).ok();
            let targets_backend = direct_id.as_deref() == Some(backend_id.as_str())
                || osdk_core::version::ToolRequest::parse(value)
                    .is_ok_and(|candidate| candidate.backend == backend_id);
            targets_backend
                && (key.strip_prefix("tool.") == Some(tool_name)
                    || direct_id
                        .as_deref()
                        .is_some_and(|id| dynamic_default_bin(id) == Some(tool_name)))
        });
        if directly_configured {
            return Some((request, version));
        }
    }
    None
}

fn dynamic_default_bin(backend_id: &str) -> Option<&str> {
    backend_id.split_once(':')?.1.rsplit('/').next()
}

fn make_ctx(dirs: Dirs, platform: Platform, config: Config) -> osdk_core::backend::Ctx {
    use std::sync::Arc;
    // A minimal client is required by Ctx; the shim never uses it for network.
    let client = osdk_core::http::client().unwrap_or_default();
    let cas = Arc::new(osdk_core::store::Cas::new(dirs.store.clone()));
    osdk_core::backend::Ctx {
        dirs,
        platform,
        config,
        client,
        cas,
        show_progress: false,
    }
}

/// Resolve a spec against locally installed versions (no network).
fn resolve_installed(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    spec: &str,
    is_range: bool,
) -> Option<String> {
    let installed = backend.list_installed(ctx).ok()?;
    if installed.is_empty() {
        return None;
    }
    if backend.id() == "python" {
        return osdk_core::backend::python::select_installed(spec, &installed);
    }
    // Strip a leading distribution prefix like `temurin-` (java) so the version
    // part matches the installed dir names (e.g. `17.0.20+8`).
    let spec = strip_distribution_prefix(spec);
    let parsed = if is_range {
        VersionSpec::parse_range(spec).ok()?
    } else {
        VersionSpec::parse(spec)
    };
    match &parsed {
        VersionSpec::Exact(v) => installed.iter().find(|i| *i == v).cloned(),
        _ => {
            let infos: Vec<_> = installed
                .iter()
                .map(osdk_core::version::VersionInfo::stable)
                .collect();
            select_version(&parsed, &infos).map(|vi| vi.version.clone())
        }
    }
}

fn managed_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    version: &ToolVersion,
) -> Vec<PathBuf> {
    let mut paths = backend.bin_paths(ctx, version).unwrap_or_default();
    paths.sort();
    paths.dedup();
    paths
}

fn managed_executable_path(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    version: &ToolVersion,
    executable_name: &str,
) -> Option<PathBuf> {
    find_exe(&managed_bin_paths(ctx, backend, version), executable_name)
}

/// Strip a leading `<word>-` distribution prefix (e.g. `temurin-17` -> `17`).
/// Only strips when the left side is purely alphabetic, so real versions like
/// `1.22` or prereleases are untouched.
fn strip_distribution_prefix(spec: &str) -> &str {
    if let Some((left, right)) = spec.split_once('-') {
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_alphabetic()) && !right.is_empty() {
            return right;
        }
    }
    spec
}

fn find_exe(bin_dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let candidates = exe_candidates(name);
    for dir in bin_dirs {
        for cand in &candidates {
            let p = dir.join(cand);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(windows)]
fn exe_candidates(name: &str) -> Vec<String> {
    vec![
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.to_string(),
    ]
}

#[cfg(not(windows))]
fn exe_candidates(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[cfg(unix)]
fn exec(exe: &PathBuf, args: &[String], env: &std::collections::BTreeMap<String, String>) -> i32 {
    use std::os::unix::process::CommandExt;
    // Replace the current process so signals/exit codes pass through cleanly.
    let err = Command::new(exe).args(args).envs(env).exec();
    eprintln!("osdk-shim: failed to exec {}: {err}", exe.display());
    126
}

#[cfg(not(unix))]
fn exec(exe: &PathBuf, args: &[String], env: &std::collections::BTreeMap<String, String>) -> i32 {
    let extension = exe
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let mut command =
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let mut command = Command::new(
                std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
            );
            let command_path = match windows_command_path(exe) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("osdk-shim: failed to prepare {}: {error}", exe.display());
                    return 126;
                }
            };
            command.args(["/D", "/S", "/C", "call"]).arg(command_path);
            command
        } else {
            Command::new(exe)
        };
    match command.args(args).envs(env).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("osdk-shim: failed to run {}: {e}", exe.display());
            126
        }
    }
}

#[cfg(windows)]
fn windows_command_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    // `GetShortPathNameW` resolves its argument through the MAX_PATH-limited
    // path API, so a longer path fails with ERROR_PATH_NOT_FOUND unless it
    // carries the extended-length prefix. Query with `\\?\`, then strip it
    // again, because `cmd.exe` cannot consume a verbatim path.
    let wide = verbatim_wide(path);
    let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if written == 0 || written as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error());
    }
    buffer.truncate(written as usize);
    let short: PathBuf = std::ffi::OsString::from_wide(&buffer).into();
    let short = strip_verbatim_prefix(&short);
    // A volume with 8.3 name creation disabled returns the long path unchanged.
    // `cmd.exe` still cannot address it, so fail with an explanatory error
    // instead of returning a path whose invocation dies as "cannot find path".
    if short.as_os_str().encode_wide().count() >= MAX_COMMAND_PATH_WIDE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path needs a short name to stay under {MAX_COMMAND_PATH_WIDE} characters, \
                 but 8.3 short names are unavailable on this volume"
            ),
        ));
    }
    Ok(short)
}

/// `cmd.exe` resolves its target through the MAX_PATH-limited Win32 path API.
#[cfg(windows)]
const MAX_COMMAND_PATH_WIDE: usize = 260;

#[cfg(windows)]
const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Encode `path` for the wide path APIs, adding `\\?\` when that is both needed
/// and valid. A UNC path would require `\\?\UNC\...` and a relative path cannot
/// be prefixed at all, so both are passed through unchanged.
#[cfg(windows)]
fn verbatim_wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let is_verbatim = encoded.starts_with(&VERBATIM_PREFIX);
    let is_unc = encoded.starts_with(&[b'\\' as u16, b'\\' as u16]);
    let mut wide = Vec::with_capacity(encoded.len() + VERBATIM_PREFIX.len() + 1);
    if !is_verbatim && !is_unc && path.is_absolute() {
        wide.extend_from_slice(&VERBATIM_PREFIX);
    }
    wide.extend_from_slice(&encoded);
    wide.push(0);
    wide
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: &std::path::Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    match encoded.strip_prefix(&VERBATIM_PREFIX[..]) {
        Some(rest) => std::ffi::OsString::from_wide(rest).into(),
        None => path.to_path_buf(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    fn batch_targets_run_through_comspec() {
        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fixture with spaces.cmd");
        let output = temporary.path().join("output.txt");
        std::fs::write(
            &script,
            "@echo off\r\n> \"%~dp0output.txt\" echo %~1\r\nexit /b 23\r\n",
        )
        .unwrap();

        let code = exec(
            &script,
            &["forwarded value".into()],
            &std::collections::BTreeMap::new(),
        );

        assert_eq!(code, 23);
        assert_eq!(
            std::fs::read_to_string(output).unwrap().trim(),
            "forwarded value"
        );
    }

    #[test]
    fn batch_targets_forward_standard_streams_and_exit_code() {
        use std::io::Write;
        use std::process::Stdio;

        let temporary = tempfile::tempdir().unwrap();
        let mut long_root = temporary.path().to_path_buf();
        for index in 0..8 {
            long_root.push(format!("segment-{index}-abcdefghijklmnopqrstuvwxyz"));
        }
        std::fs::create_dir_all(&long_root).unwrap();
        let script = long_root.join("stream fixture.cmd");
        assert!(script.as_os_str().encode_wide().count() > 260);
        // `cmd.exe` can only reach a path this long through its 8.3 short name.
        // Volumes created with 8.3 generation disabled cannot produce one, so
        // the contract under test is unobservable there.
        if windows_command_path(&script).is_err() {
            eprintln!(
                "skipping: {} has no 8.3 short name for a >260 character path",
                long_root.display()
            );
            return;
        }
        let stdout_path = temporary.path().join("stdout.txt");
        let stderr_path = temporary.path().join("stderr.txt");
        std::fs::write(
            &script,
            "@echo off\r\nset /p line=\r\necho out:%~1:%line%\r\necho err:%~2 1>&2\r\nexit /b 23\r\n",
        )
        .unwrap();

        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("tests::run_batch_target_fixture")
            .arg("--ignored")
            .env("OSDK_SHIM_TEST_BATCH_TARGET", &script)
            .env("OSDK_SHIM_TEST_BATCH_ARG1", "first arg")
            .env("OSDK_SHIM_TEST_BATCH_ARG2", "second arg")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"input\n").unwrap();
        drop(child.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                panic!("batch target stream forwarding timed out after 10 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(status.code(), Some(23));
        let stdout = std::fs::read_to_string(stdout_path).unwrap();
        let stderr = std::fs::read_to_string(stderr_path).unwrap();
        assert!(
            stdout
                .lines()
                .any(|line| line.trim() == "out:first arg:input"),
            "{stdout}"
        );
        assert!(
            stderr.lines().any(|line| line.trim() == "err:second arg"),
            "{stderr}"
        );
    }

    #[test]
    #[ignore = "helper process for the redirected batch-target contract"]
    fn run_batch_target_fixture() {
        let Some(script) = std::env::var_os("OSDK_SHIM_TEST_BATCH_TARGET") else {
            return;
        };
        let first = std::env::var("OSDK_SHIM_TEST_BATCH_ARG1").unwrap();
        let second = std::env::var("OSDK_SHIM_TEST_BATCH_ARG2").unwrap();
        std::process::exit(exec(
            &PathBuf::from(script),
            &[first, second],
            &std::collections::BTreeMap::new(),
        ));
    }
}
