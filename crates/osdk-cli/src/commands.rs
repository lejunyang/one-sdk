//! Command handlers.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::inventory::ScanReport;
use osdk_core::package_registry::{self, PackageManager, RegistryPlan, RegistryProbe};
use osdk_core::source::select;
use osdk_core::t;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};

use crate::app::App;
use crate::cli::{
    AliasCommand, ConfigCommand, ModelCommand, ModelEnvCommand, NodeCommand, PythonCommand,
    RegistryCommand, RustCommand, RustItemCommand, RustOverrideCommand, RustToolchainCommand,
    SourceCommand, TrustCommand,
};

/// Apply a one-shot `--source` override into the config for this run.
fn apply_source_override(app: &mut App, tool: &str) {
    if let Some(id) = app.source_override.clone() {
        let entry = app
            .ctx
            .config
            .sources
            .per_tool
            .entry(tool.to_string())
            .or_default();
        entry.pin = Some(id);
    }
}

pub async fn install(app: &mut App, tools: Vec<String>, opts: Vec<String>) -> Result<()> {
    let explicit = !tools.is_empty();
    let use_lock = !explicit && opts.is_empty();
    let requests = if use_lock {
        requests_from_lock(app)?.unwrap_or(gather_requests(app, tools)?)
    } else {
        gather_requests(app, tools)?
    };
    install_requests(app, requests, opts).await?;
    Ok(())
}

pub async fn lock(app: &mut App, tools: Vec<String>, opts: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let mut resolved = resolve_requests(app, requests, opts).await?;
    // A reproducible npm tool lock includes aube's exact transitive graph.
    // Ensure managed Node is present first, then ask each npm package backend
    // to generate its lockfile-only graph before serializing osdk.lock.
    if resolved
        .iter()
        .any(|(_, version)| version.backend.starts_with("npm:"))
    {
        let node_request = inject_node_dependency(
            app,
            resolved
                .iter()
                .map(|(request, _)| request.clone())
                .collect(),
        )?
        .into_iter()
        .find(|request| request.backend == "node");
        if !resolved
            .iter()
            .any(|(request, _)| request.backend == "node")
        {
            let node_request = node_request
                .ok_or_else(|| anyhow!(t!("err.npm_managed_node_dependency_required")))?;
            let (backend, version) = install_one_without_shims(app, &node_request).await?;
            generate_shims_for(app, backend.as_ref(), &version)?;
            resolved.push((node_request, version));
        } else {
            let node = resolved
                .iter()
                .find(|(request, _)| request.backend == "node")
                .map(|(request, _)| request.clone())
                .expect("checked above");
            let (backend, version) = install_one_without_shims(app, &node).await?;
            generate_shims_for(app, backend.as_ref(), &version)?;
        }
        bind_resolved_node_version(&mut resolved);
        for (_, version) in &resolved {
            if let Some(npm) = version.backend.strip_prefix("npm:") {
                let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(
                    &version.backend,
                )
                .ok_or_else(|| anyhow!(t!("err.npm_package_backend_invalid", package = npm)))?;
                backend.prepare_lock_graph(&app.ctx, version).await?;
            }
        }
        resolved.sort_by(|left, right| left.0.backend.cmp(&right.0.backend));
    }
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    let target_platform = crate::lockfile::platform_for_resolved(app.ctx.platform, &resolved);
    crate::lockfile::merge_resolved(&path, target_platform, &app.ctx.dirs, &resolved)?;
    println!("wrote {}", path.display());
    Ok(())
}

pub async fn outdated(app: &mut App, tools: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let resolved = resolve_requests(app, requests, Vec::new()).await?;
    let mut any = false;
    for (request, latest) in resolved {
        let backend = app.registry.get(&request.backend)?;
        let installed = backend.list_installed(&app.ctx)?;
        let current = installed
            .iter()
            .max_by(|left, right| {
                osdk_core::backend::python::cmp_versions(left, right).then_with(|| left.cmp(right))
            })
            .map(String::as_str)
            .unwrap_or("-");
        if !installed.iter().any(|version| version == &latest.version) {
            any = true;
            println!("{} {} -> {}", request.backend, current, latest.version);
        }
    }
    if !any {
        println!("all requested tools are up to date");
    }
    Ok(())
}

pub async fn upgrade(app: &mut App, tools: Vec<String>, opts: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let resolved = install_requests(app, requests, opts).await?;
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    crate::lockfile::merge_resolved(&path, app.ctx.platform, &app.ctx.dirs, &resolved)?;
    println!("updated {}", path.display());
    Ok(())
}

pub async fn exec_cmd(app: &mut App, tools: Vec<String>, command: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let resolved = install_requests(app, requests, Vec::new()).await?;
    let mut paths = Vec::new();
    let mut env = std::collections::BTreeMap::new();
    for (_, version) in &resolved {
        let backend = app.registry.get(&version.backend)?;
        paths.extend(managed_bin_paths(&app.ctx, backend.as_ref(), version)?);
        env.extend(backend.exec_env(&app.ctx, version)?);
    }
    paths.sort_by_key(|path| managed_runtime_path_priority(path));
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    paths.extend(std::env::split_paths(&existing_path));
    env.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );

    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("exec requires a command"))?;
    let (managed_program, managed_args) =
        resolve_managed_launcher_alias(app, &resolved, program, args)?;
    apply_package_registry_plan(app, &resolved, program, args, &mut env).await?;
    let status = command_for_program(&managed_program)
        .args(&managed_args)
        .envs(env)
        .status()
        .with_context(|| format!("running {}", managed_program.display()))?;
    if !status.success() {
        return Err(anyhow!("command exited with {status}"));
    }
    Ok(())
}

fn resolve_managed_launcher_alias(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
) -> Result<(std::path::PathBuf, Vec<String>)> {
    let alias = executable_basename(program);
    let (backend_id, canonical, subcommand) = match alias.as_str() {
        "pnpx" => ("pnpm", "pnpm", "dlx"),
        "bunx" => ("bun", "bun", "x"),
        _ => return Ok((program.into(), args.to_vec())),
    };
    let version = resolved
        .iter()
        .find_map(|(_, version)| (version.backend == backend_id).then_some(version))
        .ok_or_else(|| {
            anyhow!("`{alias}` requires a managed {backend_id} tool in this `osdk exec` invocation")
        })?;
    let backend = app.registry.get(backend_id)?;
    let executable = find_managed_executable(
        &managed_bin_paths(&app.ctx, backend.as_ref(), version)?,
        canonical,
    )
    .ok_or_else(|| {
        anyhow!(
            "managed {backend_id} executable `{canonical}` not found for {}@{}",
            version.backend,
            version.version
        )
    })?;
    let rewritten = std::iter::once(subcommand.to_string())
        .chain(args.iter().cloned())
        .collect();
    Ok((executable, rewritten))
}

fn find_managed_executable(
    directories: &[std::path::PathBuf],
    name: &str,
) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.to_string(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];

    directories.iter().find_map(|directory| {
        candidates
            .iter()
            .map(|candidate| directory.join(candidate))
            .find(|candidate| candidate.is_file())
    })
}

fn command_for_program(program: &std::path::Path) -> std::process::Command {
    #[cfg(windows)]
    if program
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    {
        let mut command = std::process::Command::new(
            std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
        );
        command.args(["/D", "/S", "/C", "call"]).arg(program);
        return command;
    }
    std::process::Command::new(program)
}

async fn apply_package_registry_plan(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
    env: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current dir for registry preflight")?;
    apply_package_registry_plan_at(app, resolved, program, args, env, &cwd).await
}

async fn apply_package_registry_plan_at(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
    env: &mut std::collections::BTreeMap<String, String>,
    cwd: &std::path::Path,
) -> Result<()> {
    let executable_alias = executable_basename(program);
    let project_yarn_version = project_yarn_version(&cwd);
    let yarn_version = resolved
        .iter()
        .find(|(_, version)| version.backend == "yarn")
        .map(|(_, version)| version.version.as_str())
        .or(project_yarn_version.as_deref());
    let Some(manager) = package_registry::manager_for_command(&executable_alias, yarn_version)
    else {
        if matches!(executable_alias.as_str(), "yarn" | "yarnpkg") {
            tracing::info!(
                executable = %executable_alias,
                "dependency registry pass-through: Yarn major is unknown; use --tool yarn@<version> or declare packageManager"
            );
        }
        return Ok(());
    };
    if !package_registry::should_plan(manager, &executable_alias, args) {
        return Ok(());
    }
    let registry_env = package_registry::registry_env(manager);
    match package_registry::plan(&app.ctx, cwd, manager, &executable_alias, args, |key| {
        std::env::var(key).ok()
    })
    .await?
    {
        RegistryPlan::PassThrough { reason } => {
            tracing::info!(manager = %manager, %reason, "dependency registry pass-through");
        }
        RegistryPlan::Selected { url, .. } => {
            env.insert(registry_env.to_string(), url);
        }
        RegistryPlan::Unavailable { probes } => {
            return Err(unavailable_registry_error(manager, &probes));
        }
    }
    Ok(())
}

fn executable_basename(program: &str) -> String {
    let basename = std::path::Path::new(program)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(program);
    let basename = basename.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if let Some(stem) = basename.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    basename
}

fn project_yarn_version(cwd: &std::path::Path) -> Option<String> {
    osdk_core::version::resolver::resolve_package_manager(cwd)
        .ok()
        .flatten()
        .filter(|request| request.manager == "yarn")
        .map(|request| request.version)
}

fn unavailable_registry_error(manager: PackageManager, probes: &[RegistryProbe]) -> anyhow::Error {
    let details = probes
        .iter()
        .map(|probe| {
            let reason = probe.error.as_deref().unwrap_or("unreachable");
            format!("{} ({reason})", probe.url)
        })
        .collect::<Vec<_>>()
        .join(", ");
    if details.is_empty() {
        anyhow!(t!("err.registry_command_not_started", manager = manager))
    } else {
        anyhow!(t!(
            "err.registry_command_not_started_details",
            manager = manager,
            details = details
        ))
    }
}

fn managed_runtime_path_priority(path: &std::path::Path) -> u8 {
    let components: std::collections::BTreeSet<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if ["npm", "pnpm", "yarn"]
        .iter()
        .any(|backend| components.contains(backend))
    {
        0
    } else if components.contains("node") {
        1
    } else {
        2
    }
}

pub fn completions(shell: clap_complete::Shell) -> Result<()> {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command();
    clap_complete::generate(shell, &mut command, "osdk", &mut std::io::stdout());
    Ok(())
}

pub async fn registry(app: &mut App, command: RegistryCommand) -> Result<()> {
    match command {
        RegistryCommand::Test { manager } => {
            let cwd = std::env::current_dir().context("getting current dir for registry test")?;
            let managers = registry_test_managers(manager.as_deref(), &cwd)?;
            let mut unavailable = Vec::new();
            for manager in managers {
                let (executable, args) = registry_test_invocation(manager);
                let plan =
                    package_registry::plan(&app.ctx, &cwd, manager, executable, &args, |key| {
                        std::env::var(key).ok()
                    })
                    .await?;
                print_registry_plan(manager, &plan);
                if matches!(plan, RegistryPlan::Unavailable { .. }) {
                    unavailable.push(manager.to_string());
                }
            }
            if unavailable.is_empty() {
                Ok(())
            } else {
                Err(anyhow!(t!(
                    "err.registry_test_unavailable",
                    managers = unavailable.join(", ")
                )))
            }
        }
    }
}

fn registry_test_managers(
    requested: Option<&str>,
    cwd: &std::path::Path,
) -> Result<Vec<PackageManager>> {
    let project_yarn = project_yarn_version(cwd);
    let yarn_manager = project_yarn
        .as_deref()
        .and_then(|version| package_registry::manager_for_command("yarn", Some(version)));
    match requested.map(|value| value.to_ascii_lowercase()) {
        None => {
            let mut managers = vec![PackageManager::Npm, PackageManager::Pnpm];
            if let Some(manager) = yarn_manager {
                managers.push(manager);
            } else {
                managers.extend([PackageManager::YarnClassic, PackageManager::YarnBerry]);
            }
            managers.extend([PackageManager::Bun, PackageManager::Deno]);
            Ok(managers)
        }
        Some(value) if matches!(value.as_str(), "yarn" | "yarnpkg") => Ok(yarn_manager
            .map_or_else(
                || vec![PackageManager::YarnClassic, PackageManager::YarnBerry],
                |manager| vec![manager],
            )),
        Some(value) => value
            .parse::<PackageManager>()
            .map(|manager| vec![manager])
            .map_err(|error| anyhow!(error)),
    }
}

fn registry_test_invocation(manager: PackageManager) -> (&'static str, Vec<String>) {
    match manager {
        PackageManager::Npm => ("npm", vec!["install".into()]),
        PackageManager::Pnpm => ("pnpm", vec!["install".into()]),
        PackageManager::YarnClassic | PackageManager::YarnBerry => ("yarn", vec!["install".into()]),
        PackageManager::Bun => ("bun", vec!["install".into()]),
        PackageManager::Deno => ("deno", vec!["add".into(), "npm:probe".into()]),
    }
}

fn print_registry_plan(manager: PackageManager, plan: &RegistryPlan) {
    println!("{}", t!("msg.registry_manager_header", manager = manager));
    let probes = match plan {
        RegistryPlan::PassThrough { reason } => {
            println!("  {}: {reason}", t!("label.registry_pass_through"));
            return;
        }
        RegistryPlan::Selected { probes, .. } | RegistryPlan::Unavailable { probes } => probes,
    };
    for probe in probes {
        if probe.ok {
            let latency = probe
                .latency_ms
                .map(|latency| format!("{latency} ms"))
                .unwrap_or_else(|| t!("label.registry_ok"));
            println!(
                "  {:<12} {latency:>8}  {}",
                t!("label.registry_healthy"),
                probe.url
            );
        } else {
            println!(
                "  {:<12}          {}{}",
                t!("label.registry_unavailable"),
                probe.url,
                probe
                    .error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            );
        }
    }
    match plan {
        RegistryPlan::Selected { url, .. } => {
            println!(
                "  {:<12} {url} ({})",
                t!("label.registry_selected"),
                package_registry::registry_env(manager)
            );
        }
        RegistryPlan::Unavailable { .. } => println!(
            "  {:<12} {}",
            t!("label.registry_unavailable"),
            t!("msg.registry_no_healthy_candidate")
        ),
        RegistryPlan::PassThrough { .. } => unreachable!(),
    }
}

async fn install_requests(
    app: &mut App,
    requests: Vec<ToolRequest>,
    opts: Vec<String>,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    let mut requests = inject_node_dependency(app, requests)?;
    if requests.is_empty() {
        println!("{}", t!("msg.nothing_to_install"));
        return Ok(Vec::new());
    }
    for req in &mut requests {
        for (k, v) in &parsed_opts {
            req.options.insert(k.clone(), v.clone());
        }
        apply_source_override(app, &req.backend);
    }
    // Package-backed JavaScript tools must never race their managed Node
    // dependency. Resolve and install Node before scheduling the remaining
    // independent requests concurrently.
    let mut node_requests = Vec::new();
    let mut remaining_requests = Vec::new();
    for request in requests {
        if request.backend == "node" {
            node_requests.push(request);
        } else {
            remaining_requests.push(request);
        }
    }
    let mut resolved = Vec::new();
    for request in node_requests {
        let (backend, version) = install_one_without_shims(app, &request).await?;
        generate_shims_for(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    bind_request_node_version(&mut remaining_requests, &resolved);
    let jobs = app.ctx.config.settings.jobs.max(1);
    let installed = stream::iter(remaining_requests.into_iter().map(|req| {
        let app_ref: &App = app;
        async move {
            let installed = install_one_without_shims(app_ref, &req).await?;
            Ok::<_, anyhow::Error>((req, installed))
        }
    }))
    .buffer_unordered(jobs)
    .try_collect::<Vec<_>>()
    .await?;
    for (request, (backend, version)) in installed {
        generate_shims_for(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    resolved.sort_by(|a, b| a.0.backend.cmp(&b.0.backend));
    Ok(resolved)
}

fn resolved_node_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "node").then_some(version.version.clone()))
}

fn bind_request_node_version(
    requests: &mut [ToolRequest],
    resolved: &[(ToolRequest, ToolVersion)],
) {
    let Some(node_version) = resolved_node_version(resolved) else {
        return;
    };
    for request in requests {
        if request.backend.starts_with("npm:") {
            request.options.insert(
                osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
                node_version.clone(),
            );
        }
    }
}

fn bind_resolved_node_version(resolved: &mut [(ToolRequest, ToolVersion)]) {
    let Some(node_version) = resolved_node_version(resolved) else {
        return;
    };
    for (_, version) in resolved {
        if version.backend.starts_with("npm:") {
            version.options.insert(
                osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
                node_version.clone(),
            );
        }
    }
}

async fn resolve_requests(
    app: &mut App,
    requests: Vec<ToolRequest>,
    opts: Vec<String>,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    let mut resolved = Vec::new();
    for mut request in requests {
        for (key, value) in &parsed_opts {
            request.options.insert(key.clone(), value.clone());
        }
        apply_source_override(app, &request.backend);
        let backend = app.registry.get(&request.backend)?;
        let effective = expand_request_alias(app, backend.as_ref(), &request)?;
        let version = backend.resolve_version(&app.ctx, &effective).await?;
        resolved.push((request, version));
    }
    resolved.sort_by(|a, b| a.0.backend.cmp(&b.0.backend));
    Ok(resolved)
}

fn requests_from_lock(app: &App) -> Result<Option<Vec<ToolRequest>>> {
    let cwd = std::env::current_dir()?;
    let Some(path) = crate::lockfile::find(&cwd) else {
        return Ok(None);
    };
    crate::lockfile::locked_requests(&path, app.ctx.platform)
}

fn project_lock_path(app: &App, cwd: &std::path::Path) -> std::path::PathBuf {
    app.ctx
        .config
        .project_config_path
        .as_ref()
        .and_then(|path| path.parent())
        .map(|directory| directory.join(crate::lockfile::LOCKFILE_NAME))
        .unwrap_or_else(|| crate::lockfile::default_path(cwd))
}

/// Parse repeated `key=value` option strings into pairs.
fn parse_opts(opts: &[String]) -> Result<Vec<(String, String)>> {
    opts.iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| anyhow!(t!("err.invalid_opt", val = s)))
        })
        .collect()
}

/// Resolve, install, and shim a single request.
async fn install_one(app: &mut App, req: &ToolRequest) -> Result<ToolVersion> {
    apply_source_override(app, &req.backend);
    install_requests(app, vec![req.clone()], Vec::new())
        .await?
        .into_iter()
        .find_map(|(request, version)| (request.backend == req.backend).then_some(version))
        .ok_or_else(|| anyhow!("requested tool was not installed"))
}

async fn install_one_without_shims(
    app: &App,
    req: &ToolRequest,
) -> Result<(std::sync::Arc<dyn Backend>, ToolVersion)> {
    if app.refresh_sources {
        let backend = app.registry.get(&req.backend)?;
        select::refresh(&app.ctx, backend.as_ref()).await?;
    }
    let backend = app.registry.get(&req.backend)?;
    let effective = expand_request_alias(app, backend.as_ref(), req)?;
    let tv = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", req.backend, req.spec))?;

    if osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version)
        && !backend.id().starts_with("npm:")
    {
        backend.ensure_post_install(&app.ctx, &tv)?;
        println!("{}", t!("msg.already_installed", tool = tv));
    } else {
        println!("{}", t!("msg.installing", tool = tv));
        let ictx = InstallCtx { ctx: &app.ctx };
        backend
            .install(&ictx, &tv)
            .await
            .with_context(|| format!("installing {}", tv))?;
        println!("{}", t!("msg.installed", tool = tv));
    }
    Ok((backend, tv))
}

fn expand_request_alias(
    app: &App,
    backend: &dyn Backend,
    request: &ToolRequest,
) -> Result<ToolRequest> {
    let expanded = app
        .ctx
        .config
        .expand_alias(backend.id(), &request.spec.to_string())?;
    let mut effective = request.clone();
    effective.backend = backend.id().to_string();
    effective.spec = VersionSpec::parse(&expanded);
    Ok(effective)
}

pub fn alias(app: &App, command: AliasCommand) -> Result<()> {
    match command {
        AliasCommand::Set { tool, name, target } => {
            let backend = app.registry.get(&tool)?;
            osdk_core::config::validate_alias_name(&name)?;
            let mut aliases = app
                .ctx
                .config
                .aliases
                .get(backend.id())
                .cloned()
                .unwrap_or_default();
            aliases.insert(name.clone(), target.clone());
            osdk_core::config::expand_alias(&aliases, &name)?;
            crate::config_edit::set_version_alias(&app.ctx, backend.id(), &name, &target)?;
            println!("{} {} = {}", backend.id(), name, target);
        }
        AliasCommand::List { tool } => {
            if let Some(tool) = tool {
                let backend = app.registry.get(&tool)?;
                if let Some(aliases) = app.ctx.config.aliases.get(backend.id()) {
                    for (name, version) in aliases {
                        println!("{} {} = {}", backend.id(), name, version);
                    }
                }
            } else {
                for (tool, aliases) in &app.ctx.config.aliases {
                    for (name, version) in aliases {
                        println!("{tool} {name} = {version}");
                    }
                }
            }
        }
        AliasCommand::Unset { tool, name } => {
            let backend = app.registry.get(&tool)?;
            crate::config_edit::remove_version_alias(&app.ctx, backend.id(), &name)?;
            println!("removed {} {}", backend.id(), name);
        }
    }
    Ok(())
}

fn gather_requests(app: &App, tools: Vec<String>) -> Result<Vec<ToolRequest>> {
    if !tools.is_empty() {
        let mut requests = tools
            .iter()
            .map(|s| ToolRequest::parse(s).map_err(|e| anyhow!("{e}")))
            .collect::<Result<Vec<_>>>()?;
        for request in &mut requests {
            inherit_configured_options(app, request);
        }
        return inject_node_dependency(app, requests);
    }
    // From config pins.
    let mut out = Vec::new();
    for (tool, spec) in &app.ctx.config.tools {
        if tool == "node" {
            continue;
        }
        if app.registry.get(tool).is_ok() {
            out.push(ToolRequest {
                backend: tool.clone(),
                spec: VersionSpec::parse(spec),
                options: app
                    .ctx
                    .config
                    .tool_configs
                    .get(tool)
                    .map(|entry| entry.to_request_options())
                    .unwrap_or_default(),
            });
        } else if let Ok(mut request) = ToolRequest::parse(spec) {
            if !request.backend.contains(':') || app.registry.get(&request.backend).is_err() {
                continue;
            }
            if let Some(entry) = app.ctx.config.tool_configs.get(tool) {
                request.options.extend(entry.to_request_options());
            }
            out.push(request);
        }
    }
    let cwd = std::env::current_dir()?;
    if let Some(package_manager) =
        osdk_core::version::resolver::resolve_package_manager(&cwd).map_err(anyhow::Error::msg)?
    {
        if out
            .iter()
            .all(|request| request.backend != package_manager.manager)
        {
            out.push(ToolRequest {
                backend: package_manager.manager.clone(),
                spec: VersionSpec::Exact(package_manager.version),
                options: app
                    .ctx
                    .config
                    .tool_configs
                    .get(&package_manager.manager)
                    .map(|entry| entry.to_request_options())
                    .unwrap_or_default(),
            });
        }
    }
    let backend = app.registry.get("node")?;
    if let Some(active) = osdk_core::version::resolver::resolve_active(
        backend.id(),
        &cwd,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    ) {
        let spec = if active.is_range {
            VersionSpec::parse_range(&active.spec)?
        } else {
            VersionSpec::parse(&active.spec)
        };
        out.push(ToolRequest {
            backend: backend.id().to_string(),
            spec,
            options: app
                .ctx
                .config
                .tool_configs
                .get(backend.id())
                .map(|entry| entry.to_request_options())
                .unwrap_or_default(),
        });
    }
    inject_node_dependency(app, out)
}

fn inherit_configured_options(app: &App, request: &mut ToolRequest) {
    let configured = app
        .ctx
        .config
        .tool_configs
        .get(&request.backend)
        .or_else(|| {
            app.ctx.config.tools.iter().find_map(|(key, value)| {
                ToolRequest::parse(value)
                    .ok()
                    .filter(|configured| configured.backend == request.backend)
                    .and_then(|_| app.ctx.config.tool_configs.get(key))
            })
        })
        .map(|entry| entry.to_request_options())
        .unwrap_or_default();
    let explicit = std::mem::take(&mut request.options);
    request.options = configured;
    request.options.extend(explicit);
}

fn inject_node_dependency(app: &App, mut requests: Vec<ToolRequest>) -> Result<Vec<ToolRequest>> {
    let has_package_manager = requests.iter().any(|request| {
        matches!(request.backend.as_str(), "npm" | "pnpm" | "yarn")
            || request.backend.starts_with("npm:")
    });
    if !has_package_manager || requests.iter().any(|request| request.backend == "node") {
        return Ok(requests);
    }
    let cwd = std::env::current_dir()?;
    let backend = app.registry.get("node")?;
    let spec = osdk_core::version::resolver::resolve_active(
        "node",
        &cwd,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .map(|active| {
        if active.is_range {
            VersionSpec::parse_range(&active.spec)
        } else {
            Ok(VersionSpec::parse(&active.spec))
        }
    })
    .transpose()?
    .unwrap_or(VersionSpec::Latest);
    requests.push(ToolRequest {
        backend: "node".into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get("node")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    });
    Ok(requests)
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
fn all_display_backends(app: &App) -> Result<Vec<std::sync::Arc<dyn Backend>>> {
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
        println!("{}{}", v.version, lts);
        count += 1;
    }
    if count == 0 {
        println!("{}", t!("msg.no_matching_versions"));
    }
    Ok(())
}

pub async fn use_cmd(app: &mut App, tool: String, global: bool, opts: Vec<String>) -> Result<()> {
    let requested_spec = requested_spec_literal(&tool);
    let mut req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    inherit_configured_options(app, &mut req);
    for (k, v) in parse_opts(&opts)? {
        req.options.insert(k, v);
    }
    if global && req.backend.starts_with("npm:") {
        return crate::global_npm_use::install(app, req, requested_spec).await;
    }
    if !global && req.backend.starts_with("npm:") {
        let cwd = std::env::current_dir()?;
        let requested_installer =
            osdk_core::npm_tools::installer_from_request_options(&req.options)?;
        let plan = osdk_core::npm_tools::plan_npm_installer(
            &cwd,
            requested_installer,
            osdk_core::npm_tools::ToolScope::Project,
        )?;
        if let Some(project) = plan.project {
            return use_project_npm(app, req, requested_spec, project, plan.installer).await;
        }
    }
    use_legacy_cmd(app, req, requested_spec, global).await
}

async fn use_legacy_cmd(
    app: &mut App,
    req: ToolRequest,
    requested_spec: Option<String>,
    global: bool,
) -> Result<()> {
    let persisted_options = req.options.clone();
    let tv = install_one(app, &req).await?;
    // Pin the exact spec string the user typed (verbatim after `@`), so
    // channels like `stable` or `temurin-17` are preserved rather than being
    // normalized to `latest`. Bare `tool` (no `@`) pins the resolved version.
    let spec = requested_spec.unwrap_or_else(|| tv.version.clone());
    if global {
        if persisted_options.is_empty() {
            crate::config_edit::set_global_tool(&app.ctx, &tv.backend, &spec)?;
        } else {
            crate::config_edit::set_global_tool_config(
                &app.ctx,
                &tv.backend,
                &structured_tool_config(&spec, &persisted_options),
            )?;
        }
        println!("{}", t!("msg.pinned_global", tool = tv.backend, ver = spec));
    } else {
        let path = if persisted_options.is_empty() {
            crate::config_edit::set_project_tool(&tv.backend, &spec)?
        } else {
            crate::config_edit::set_project_tool_config(
                &tv.backend,
                &structured_tool_config(&spec, &persisted_options),
            )?
        };
        println!(
            "{}",
            t!(
                "msg.pinned_project",
                tool = tv.backend,
                ver = spec,
                path = path.display()
            )
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectDependencySection {
    Dependencies,
    DevDependencies,
    OptionalDependencies,
    PeerDependencies { also_dev: bool },
}

async fn use_project_npm(
    app: &mut App,
    request: ToolRequest,
    requested_spec: Option<String>,
    project: osdk_core::npm_tools::NpmProject,
    installer: osdk_core::npm_tools::NpmInstaller,
) -> Result<()> {
    let package = request
        .backend
        .strip_prefix("npm:")
        .ok_or_else(|| anyhow!("project npm install requires an npm: package request"))?
        .to_string();
    let section = project_dependency_section(&project.package_json, &package)?;
    let node_request = project_node_request(app, &project.root)?;
    let (node_backend, node_version) = install_one_without_shims(app, &node_request).await?;
    generate_shims_for(app, node_backend.as_ref(), &node_version)?;
    let node_bin_dir = managed_bin_paths(&app.ctx, node_backend.as_ref(), &node_version)?
        .into_iter()
        .find(|path| path.join(node_executable_name()).is_file())
        .ok_or_else(|| {
            anyhow!(t!(
                "err.managed_node_bin_dir_missing",
                version = node_version.version
            ))
        })?;

    apply_source_override(app, &request.backend);
    let backend = app.registry.get(&request.backend)?;
    let mut effective = expand_request_alias(app, backend.as_ref(), &request)?;
    effective.options.insert(
        osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
        node_version.version.clone(),
    );
    let mut version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    let package_spec = project_package_spec(&package, requested_spec.as_deref(), &version.version);

    match installer {
        osdk_core::npm_tools::NpmInstaller::Aube => {
            osdk_core::backend::aube_host::add_to_project(
                osdk_core::backend::aube_host::EmbeddedProjectAddRequest {
                    project_dir: &project.root,
                    packages: std::slice::from_ref(&package_spec),
                    cache_dir: osdk_core::backend::npm_package::NpmPackageBackend::aube_cache_dir(
                        &app.ctx,
                    ),
                    store_dir: osdk_core::backend::npm_package::NpmPackageBackend::aube_store_dir(
                        &app.ctx,
                    ),
                    node_bin_dir: node_bin_dir.clone(),
                    save_dev: matches!(
                        section,
                        ProjectDependencySection::DevDependencies
                            | ProjectDependencySection::PeerDependencies { also_dev: true }
                    ),
                    save_optional: matches!(
                        section,
                        ProjectDependencySection::OptionalDependencies
                    ),
                    save_peer: matches!(section, ProjectDependencySection::PeerDependencies { .. }),
                    offline: app.ctx.config.settings.offline,
                },
            )
            .await?;
        }
        osdk_core::npm_tools::NpmInstaller::Npm | osdk_core::npm_tools::NpmInstaller::Pnpm => {
            run_project_native_installer(
                app,
                installer,
                &project.root,
                &package_spec,
                section,
                &node_bin_dir,
            )
            .await?;
        }
        osdk_core::npm_tools::NpmInstaller::Auto => {
            unreachable!("npm installer planning always returns a concrete installer")
        }
    }

    osdk_core::backend::npm_package::NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?
        .validate_project_package_bins(&project.root)?;
    let installed_project = osdk_core::npm_tools::inspect_npm_project(&project.root)?
        .ok_or_else(|| anyhow!("project package.json disappeared during npm install"))?;
    let native_lock = installed_project.native_lock.ok_or_else(|| {
        anyhow!(
            "installer `{installer}` did not write a recognized native lockfile in {}",
            project.root.display()
        )
    })?;
    if installer != osdk_core::npm_tools::NpmInstaller::Aube && native_lock.installer() != installer
    {
        anyhow::bail!(
            "installer `{installer}` wrote {} owned by `{}`",
            native_lock.path.display(),
            native_lock.installer()
        );
    }
    record_project_npm_metadata(&mut version, installer, &native_lock, &node_version.version)?;

    let lock_path = project.root.join(crate::lockfile::LOCKFILE_NAME);
    crate::lockfile::upsert_resolved_many_with_scope(
        &lock_path,
        app.ctx.platform,
        &app.ctx.dirs,
        &[
            (node_request.clone(), node_version.clone()),
            (request.clone(), version.clone()),
        ],
        crate::lockfile::LockScope::Project,
    )?;

    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    let config_path = project_config_path(app, &project.root);
    let mut config_options = request.options.clone();
    config_options.insert("installer".into(), installer.as_str().into());
    crate::config_edit::set_project_npm_tool_at(
        &config_path,
        &node_version.version,
        &request.backend,
        &structured_tool_config(&persisted_spec, &config_options),
    )?;
    osdk_core::trust::trust(&app.ctx.dirs.config, &config_path)?;
    println!(
        "{}",
        t!(
            "msg.pinned_project",
            tool = request.backend,
            ver = persisted_spec,
            path = config_path.display()
        )
    );
    Ok(())
}

fn project_dependency_section(
    path: &std::path::Path,
    package: &str,
) -> Result<ProjectDependencySection> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let contains = |section: &str| {
        manifest
            .get(section)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|dependencies| dependencies.contains_key(package))
    };
    Ok(if contains("dependencies") {
        ProjectDependencySection::Dependencies
    } else if contains("optionalDependencies") {
        ProjectDependencySection::OptionalDependencies
    } else if contains("peerDependencies") {
        ProjectDependencySection::PeerDependencies {
            also_dev: contains("devDependencies"),
        }
    } else if contains("devDependencies") {
        ProjectDependencySection::DevDependencies
    } else {
        ProjectDependencySection::DevDependencies
    })
}

fn project_package_spec(package: &str, requested: Option<&str>, resolved: &str) -> String {
    format!("{package}@{}", requested.unwrap_or(resolved))
}

fn node_executable_name() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

fn project_node_request(app: &App, project_root: &std::path::Path) -> Result<ToolRequest> {
    let backend = app.registry.get("node")?;
    let spec = osdk_core::version::resolver::resolve_active(
        "node",
        project_root,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .map(|active| {
        if active.is_range {
            VersionSpec::parse_range(&active.spec)
        } else {
            Ok(VersionSpec::parse(&active.spec))
        }
    })
    .transpose()?
    .unwrap_or(VersionSpec::Latest);
    Ok(ToolRequest {
        backend: "node".into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get("node")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

async fn run_project_native_installer(
    app: &mut App,
    installer: osdk_core::npm_tools::NpmInstaller,
    project_root: &std::path::Path,
    package_spec: &str,
    section: ProjectDependencySection,
    node_bin_dir: &std::path::Path,
) -> Result<()> {
    let manager_id = installer
        .executable()
        .filter(|manager| matches!(*manager, "npm" | "pnpm"))
        .ok_or_else(|| anyhow!("project native installer must be npm or pnpm"))?;
    let manager_request = project_manager_request(app, manager_id, project_root)?;
    let (manager_backend, manager_version) =
        install_one_without_shims(app, &manager_request).await?;
    generate_shims_for(app, manager_backend.as_ref(), &manager_version)?;
    let manager = find_managed_executable(
        &managed_bin_paths(&app.ctx, manager_backend.as_ref(), &manager_version)?,
        manager_id,
    )
    .ok_or_else(|| {
        anyhow!(
            "managed {manager_id} executable not found for {}@{}",
            manager_version.backend,
            manager_version.version
        )
    })?;
    let args = project_manager_args(installer, package_spec, section);
    let mut env = manager_backend.exec_env(&app.ctx, &manager_version)?;
    let mut paths = vec![manager.parent().unwrap_or(project_root).to_path_buf()];
    paths.push(node_bin_dir.to_path_buf());
    if let Some(existing_path) = std::env::var_os("PATH").filter(|path| !path.is_empty()) {
        paths.extend(std::env::split_paths(&existing_path));
    }
    env.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    let resolved = vec![(manager_request, manager_version)];
    apply_package_registry_plan_at(app, &resolved, manager_id, &args, &mut env, project_root)
        .await?;
    let status = command_for_program(&manager)
        .args(&args)
        .current_dir(project_root)
        .envs(env)
        .status()
        .with_context(|| format!("running {}", manager.display()))?;
    if !status.success() {
        anyhow::bail!("command exited with {status}");
    }
    Ok(())
}

fn project_manager_request(
    app: &App,
    manager: &str,
    project_root: &std::path::Path,
) -> Result<ToolRequest> {
    let backend = app.registry.get(manager)?;
    let spec = osdk_core::version::resolver::resolve_active(
        manager,
        project_root,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .map(|active| {
        if active.is_range {
            VersionSpec::parse_range(&active.spec)
                .unwrap_or_else(|_| VersionSpec::parse(&active.spec))
        } else {
            VersionSpec::parse(&active.spec)
        }
    })
    .unwrap_or(VersionSpec::Latest);
    Ok(ToolRequest {
        backend: manager.into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get(manager)
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

fn project_manager_args(
    installer: osdk_core::npm_tools::NpmInstaller,
    package_spec: &str,
    section: ProjectDependencySection,
) -> Vec<String> {
    let mut args = match installer {
        osdk_core::npm_tools::NpmInstaller::Npm => vec!["install".into()],
        osdk_core::npm_tools::NpmInstaller::Pnpm => vec!["add".into()],
        _ => unreachable!("only native installers have command arguments"),
    };
    match (installer, section) {
        (osdk_core::npm_tools::NpmInstaller::Npm, ProjectDependencySection::Dependencies) => {
            args.push("--save-prod".into())
        }
        (osdk_core::npm_tools::NpmInstaller::Npm, ProjectDependencySection::DevDependencies) => {
            args.push("--save-dev".into())
        }
        (
            osdk_core::npm_tools::NpmInstaller::Npm,
            ProjectDependencySection::OptionalDependencies,
        ) => args.push("--save-optional".into()),
        (
            osdk_core::npm_tools::NpmInstaller::Npm,
            ProjectDependencySection::PeerDependencies { also_dev },
        ) => {
            args.push("--save-peer".into());
            if also_dev {
                args.push("--save-dev".into());
            }
        }
        (osdk_core::npm_tools::NpmInstaller::Pnpm, ProjectDependencySection::Dependencies) => {
            args.push("-P".into())
        }
        (osdk_core::npm_tools::NpmInstaller::Pnpm, ProjectDependencySection::DevDependencies) => {
            args.push("-D".into())
        }
        (
            osdk_core::npm_tools::NpmInstaller::Pnpm,
            ProjectDependencySection::OptionalDependencies,
        ) => args.push("-O".into()),
        (
            osdk_core::npm_tools::NpmInstaller::Pnpm,
            ProjectDependencySection::PeerDependencies { also_dev },
        ) => {
            args.push("--save-peer".into());
            if also_dev {
                args.push("-D".into());
            }
        }
        _ => unreachable!("only native installers have command arguments"),
    }
    args.push("--ignore-scripts".into());
    args.push(package_spec.into());
    args
}

fn record_project_npm_metadata(
    version: &mut ToolVersion,
    installer: osdk_core::npm_tools::NpmInstaller,
    native_lock: &osdk_core::npm_tools::NativeLock,
    node_version: &str,
) -> Result<()> {
    let sha256 = osdk_core::pipeline::verify::hash_file(
        &native_lock.path,
        osdk_core::pipeline::HashAlgo::Sha256,
    )?;
    version.options.insert(
        osdk_core::npm_tools::INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
        osdk_core::npm_tools::ToolScope::Project.as_str().into(),
    );
    version.options.insert(
        osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
        node_version.into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
        native_lock.installer_name().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
        native_lock.format.clone(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
        sha256,
    );
    Ok(())
}

fn project_config_path(app: &App, project_root: &std::path::Path) -> std::path::PathBuf {
    app.ctx
        .config
        .project_config_path
        .as_ref()
        .filter(|path| {
            path.parent()
                .is_some_and(|parent| same_config_path(parent, project_root))
        })
        .cloned()
        .unwrap_or_else(|| project_root.join("osdk.toml"))
}

fn same_config_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
        || match (dunce::canonicalize(left), dunce::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn structured_tool_config(
    version: &str,
    options: &std::collections::BTreeMap<String, String>,
) -> osdk_core::config::StructuredToolConfig {
    osdk_core::config::StructuredToolConfig {
        version: version.to_string(),
        options: options
            .iter()
            .map(|(key, value)| (key.clone(), structured_tool_option(key, value)))
            .collect(),
    }
}

fn structured_tool_option(key: &str, value: &str) -> osdk_core::config::ToolConfigValue {
    if key == "allow_builds" {
        let lower = value.to_ascii_lowercase();
        if matches!(lower.as_str(), "true" | "false") {
            return osdk_core::config::ToolConfigValue::Bool(lower == "true");
        }
        return osdk_core::config::ToolConfigValue::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    osdk_core::config::ToolConfigValue::String(value.to_string())
}

pub async fn uninstall(app: &App, tool: String) -> Result<()> {
    let req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    let backend = app.registry.get(&req.backend)?;
    let version = match &req.spec {
        VersionSpec::Exact(v) => v.clone(),
        VersionSpec::Latest if backend.id() == "rust" => "stable".to_string(),
        VersionSpec::Prefix(p) => {
            // pick the installed version matching the prefix
            let installed = backend.list_installed(&app.ctx)?;
            installed
                .into_iter()
                .rfind(|v| v.starts_with(p.as_str()))
                .ok_or_else(|| {
                    anyhow!(t!("err.no_installed_match", tool = req.backend, spec = p))
                })?
        }
        other => return Err(anyhow!(t!("err.specify_exact", spec = other))),
    };
    let tv = ToolVersion::new(&req.backend, &version);
    let question = t!("prompt.uninstall", tool = tv);
    if !app.prompt.confirm(&question)? {
        println!("{}", t!("msg.cancelled"));
        return Ok(());
    }
    backend.uninstall(&app.ctx, &tv).await?;
    reconcile_managed_shims(app)?;
    println!("{}", t!("msg.uninstalled", tool = tv));
    // Reclaim now-unreferenced store objects.
    let models = app.ctx.dirs.models();
    let (removed, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
    if removed > 0 {
        println!(
            "{}",
            t!(
                "msg.pruned_store",
                count = removed,
                size = human_bytes(bytes)
            )
        );
    }
    Ok(())
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
    if !any {
        println!("{}", t!("msg.no_active"));
    }
    Ok(())
}

pub fn where_cmd(app: &App, tool: String) -> Result<()> {
    let req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    let backend = app.registry.get(&req.backend)?;
    let version = match &req.spec {
        VersionSpec::Exact(v) => v.clone(),
        _ => {
            let cwd = std::env::current_dir()?;
            let installed = backend.list_installed(&app.ctx)?;
            let dynamic_request =
                osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id());
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
                        VersionSpec::parse_range(&spec)
                            .unwrap_or_else(|_| VersionSpec::parse(&spec))
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
        }
    };
    let dir = app.ctx.dirs.install_path(backend.id(), &version);
    if !dir.exists() {
        return Err(anyhow!("{}@{} is not installed", backend.id(), version));
    }
    println!("{}", dir.display());
    Ok(())
}

pub fn reshim(app: &App) -> Result<()> {
    let mut total = 0;
    for backend in all_display_backends(app)? {
        for version in backend.list_installed(&app.ctx)? {
            let tv = ToolVersion::new(backend.id(), &version);
            total += generate_shims_for(app, backend.as_ref(), &tv)?;
        }
    }
    reconcile_managed_shims(app)?;
    println!("{}", t!("msg.reshimmed", count = total));
    Ok(())
}

fn reconcile_managed_shims(app: &App) -> Result<()> {
    let owners = installed_shim_owners(app)?;
    let expected = owners
        .into_iter()
        .filter_map(|(name, owner_ids)| (!is_real_shim_conflict(&name, &owner_ids)).then_some(name))
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

/// Generate shims for all bin names a version exposes. Returns count.
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
    let names = routed_bin_names_for_version(&app.ctx, backend, tv)?;
    ensure_no_shim_conflicts(app, backend.id(), &names)?;
    let mut count = 0;
    for name in names {
        osdk_core::shim::generate_shim(&app.ctx.dirs, &name, &shim_bin)?;
        count += 1;
    }
    Ok(count)
}

pub async fn source(app: &mut App, command: SourceCommand) -> Result<()> {
    match command {
        SourceCommand::List { tool } => {
            let tool = canonical_source_tool(app, &tool)?;
            let sources = if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
                osdk_core::model::source::effective_sources(&app.ctx, provider)
            } else {
                let backend = app.registry.get(&tool)?;
                select::effective_sources(&app.ctx, backend.as_ref())
            };
            let pin = app
                .ctx
                .config
                .tool_sources(&tool)
                .and_then(|t| t.pin.clone());
            println!(
                "{}",
                t!(
                    "msg.sources_header",
                    tool = tool,
                    mode = format!("{:?}", app.ctx.config.sources.selection)
                )
            );
            for s in sources {
                let marker = if Some(&s.id) == pin.as_ref() {
                    format!(" {}", t!("label.pinned"))
                } else {
                    String::new()
                };
                println!(
                    "  {:12} {:8} {}{}",
                    s.id,
                    select::kind_label(s.kind),
                    s.download_url,
                    marker
                );
            }
        }
        SourceCommand::Test { tool, model } => {
            let tool = canonical_source_tool(app, &tool)?;
            println!("{}", t!("msg.probing", tool = tool));
            let mut ranked = if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
                let model = model.ok_or_else(|| {
                    anyhow!("`osdk source test {tool}` requires --model owner/repo@revision")
                })?;
                let reference = model_reference(provider, &model)?;
                osdk_core::model::source::refresh(&app.ctx, &reference).await?
            } else {
                if model.is_some() {
                    return Err(anyhow!("--model is only valid for model providers"));
                }
                let backend = app.registry.get(&tool)?;
                select::refresh(&app.ctx, backend.as_ref()).await?
            };
            ranked.sort_by(|a, b| b.score().total_cmp(&a.score()));
            for (i, r) in ranked.iter().enumerate() {
                if r.ok {
                    println!(
                        "  {}. {:12} {:>10}/s  ttfb {}ms",
                        i + 1,
                        r.source_id,
                        human_bytes(r.throughput as u64),
                        r.ttfb_ms
                    );
                } else {
                    println!("  -. {:12} {}", r.source_id, t!("msg.unreachable"));
                }
            }
        }
        SourceCommand::Add {
            tool,
            id,
            download_url,
            index_url,
            forward_credentials,
        } => {
            let tool = canonical_source_tool(app, &tool)?;
            crate::config_edit::add_custom_source(
                &app.ctx,
                &tool,
                &id,
                &download_url,
                index_url.as_deref(),
                forward_credentials,
            )?;
            println!("{}", t!("msg.source_added", id = id, tool = tool));
        }
        SourceCommand::Remove { tool, id } => {
            let tool = canonical_source_tool(app, &tool)?;
            let removed = crate::config_edit::remove_custom_source(&app.ctx, &tool, &id)?;
            if removed {
                println!("{}", t!("msg.source_removed", id = id, tool = tool));
            } else {
                println!("{}", t!("msg.source_not_found", id = id, tool = tool));
            }
        }
        SourceCommand::Pin { tool, id } => {
            let tool = canonical_source_tool(app, &tool)?;
            let known = if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
                osdk_core::model::source::effective_sources(&app.ctx, provider)
                    .iter()
                    .any(|source| source.id == id)
            } else {
                let backend = app.registry.get(&tool)?;
                select::effective_sources(&app.ctx, backend.as_ref())
                    .iter()
                    .any(|source| source.id == id)
            };
            if !known {
                return Err(anyhow!(t!("err.unknown_source", id = id, tool = tool)));
            }
            crate::config_edit::set_source_pin(&app.ctx, &tool, Some(&id))?;
            println!("{}", t!("msg.source_pinned", tool = tool, id = id));
        }
        SourceCommand::Unpin { tool } => {
            let tool = canonical_source_tool(app, &tool)?;
            crate::config_edit::set_source_pin(&app.ctx, &tool, None)?;
            println!("{}", t!("msg.source_unpinned", tool = tool));
        }
    }
    Ok(())
}

fn canonical_source_tool(app: &App, tool: &str) -> Result<String> {
    match tool.parse::<osdk_core::model::ProviderId>() {
        Ok(provider) => Ok(provider.as_str().to_string()),
        Err(_) => Ok(app.registry.get(tool)?.id().to_string()),
    }
}

pub fn activate(app: &App, shell: String) -> Result<()> {
    let sh: osdk_core::activate::Shell = shell.parse().map_err(|e| anyhow!("{e}"))?;
    // Reference the osdk binary by its current path so the snippet is portable.
    let bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "osdk".to_string());
    print!("{}", osdk_core::activate::activation_script(sh, &bin));
    let _ = app; // app unused beyond validation
    Ok(())
}

pub fn deactivate(shell: String) -> Result<()> {
    let shell: osdk_core::activate::Shell = shell.parse().map_err(|e| anyhow!("{e}"))?;
    print!("{}", osdk_core::activate::deactivation_script(shell));
    Ok(())
}

pub fn hook_env(app: &App, shell: String) -> Result<()> {
    let sh: osdk_core::activate::Shell = shell.parse().map_err(|e| anyhow!("{e}"))?;
    let cwd = std::env::current_dir()?;
    let mut delta = osdk_core::activate::compute_env_delta(&app.ctx, &app.registry, &cwd);
    // Layer 2: downstream package caches (only vars the user hasn't set).
    let cache_vars = osdk_core::cache::cache_env(&app.ctx.dirs.cache, |k| std::env::var(k).ok());
    delta.set_vars.extend(cache_vars);
    // Layer 3: globally-enabled model provider adapters.
    let model_vars = osdk_core::model::env::configured_env(&app.ctx, |key| std::env::var(key).ok());
    delta.set_vars.extend(model_vars);
    delta
        .unset_vars
        .retain(|key| !delta.set_vars.contains_key(key));
    print!("{}", osdk_core::activate::render_hook_env(sh, &delta));
    Ok(())
}

pub fn cache(app: &App, command: crate::cli::CacheCommand) -> Result<()> {
    use crate::cli::CacheCommand;
    match command {
        CacheCommand::Dir => {
            println!("cache_dir     = {}", app.ctx.dirs.cache.display());
            println!("downloads     = {}", app.ctx.dirs.downloads().display());
            println!("store (CAS)   = {}", app.ctx.dirs.store.display());
            println!(
                "downstream    = {}",
                osdk_core::cache::downstream_root(&app.ctx.dirs.cache).display()
            );
        }
        CacheCommand::Env => {
            for (k, v) in osdk_core::cache::describe(&app.ctx.dirs.cache) {
                println!("{k}={v}");
            }
        }
        CacheCommand::Clean => {
            if !app.prompt.confirm(&t!("prompt.cache_clean"))? {
                println!("{}", t!("msg.cancelled"));
                return Ok(());
            }
            let downloads = app.ctx.dirs.downloads();
            if downloads.exists() {
                std::fs::remove_dir_all(&downloads)
                    .with_context(|| format!("removing {}", downloads.display()))?;
                std::fs::create_dir_all(&downloads).ok();
            }
            println!("{}", t!("msg.cache_cleared"));
        }
    }
    Ok(())
}

pub fn config(app: &App, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            println!("config dir:  {}", app.ctx.dirs.config.display());
            println!("config file: {}", app.ctx.dirs.user_config_file().display());
            if let Some(p) = &app.ctx.config.project_config_path {
                println!("project:     {}", p.display());
            }
        }
        ConfigCommand::List => {
            let s = &app.ctx.config.settings;
            println!("data_dir     = {}", app.ctx.dirs.data.display());
            println!("store_dir    = {}", app.ctx.dirs.store.display());
            println!("install_dir  = {}", app.ctx.dirs.installs.display());
            println!("cache_dir    = {}", app.ctx.dirs.cache.display());
            println!("link_mode    = {}", s.link_mode);
            println!("jobs         = {}", s.jobs);
            println!("offline      = {}", s.offline);
            println!("verify_signatures = {}", s.verify_signatures);
            println!("require_checksums = {}", s.require_checksums);
            println!("attestations = {}", s.attestations);
            println!("prerelease  = {}", s.prerelease);
            println!(
                "python_catalog = {}",
                s.python.catalog_url.as_deref().unwrap_or("built-in")
            );
            println!("selection    = {:?}", app.ctx.config.sources.selection);
            let npm_registries = &app.ctx.config.registries().npm;
            println!(
                "registries.npm.urls = {}",
                if npm_registries.urls.is_empty() {
                    "built-in (npmjs + npmmirror)".to_string()
                } else {
                    npm_registries.urls.join(", ")
                }
            );
            println!(
                "registries.npm.probe_timeout_ms = {}",
                npm_registries.probe_timeout_ms
            );
            for provider in [
                osdk_core::model::ProviderId::HuggingFace,
                osdk_core::model::ProviderId::ModelScope,
            ] {
                if let Some(config) = app.ctx.config.tool_sources(provider.as_str()) {
                    println!(
                        "model_env.{} = {}{}",
                        provider,
                        config.env,
                        if config.env_force { " (force)" } else { "" }
                    );
                }
            }
            if !app.ctx.config.tools.is_empty() {
                println!("tools:");
                for (k, v) in &app.ctx.config.tools {
                    println!("  {k} = {v}");
                }
            }
            if !app.ctx.config.aliases.is_empty() {
                println!("aliases:");
                for (tool, aliases) in &app.ctx.config.aliases {
                    for (name, version) in aliases {
                        println!("  {tool} {name} = {version}");
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn trust(
    app: &App,
    path: Option<std::path::PathBuf>,
    command: Option<TrustCommand>,
) -> Result<()> {
    if matches!(command, Some(TrustCommand::List)) {
        for record in osdk_core::trust::list(&app.ctx.dirs.config)? {
            let state = if record.path.is_file()
                && osdk_core::trust::is_trusted(&app.ctx.dirs.config, &record.path, None)?
            {
                t!("label.trusted")
            } else {
                t!("label.stale")
            };
            println!("{}  {}  {}", state, record.hash, record.path.display());
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let config = osdk_core::trust::resolve_config(path.as_deref(), &cwd)?;
    let question = t!("prompt.trust_config", path = config.display());
    if !app.prompt.confirm(&question)? {
        println!("{}", t!("msg.cancelled"));
        return Ok(());
    }
    let record = osdk_core::trust::trust(&app.ctx.dirs.config, &config)?;
    println!(
        "{}",
        t!(
            "msg.config_trusted",
            path = record.path.display(),
            hash = record.hash
        )
    );
    Ok(())
}

pub fn untrust(app: &App, path: Option<std::path::PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config = osdk_core::trust::resolve_config(path.as_deref(), &cwd)?;
    if osdk_core::trust::untrust(&app.ctx.dirs.config, &config)? {
        println!("{}", t!("msg.config_untrusted", path = config.display()));
    } else {
        println!(
            "{}",
            t!("msg.config_was_not_trusted", path = config.display())
        );
    }
    Ok(())
}

pub fn node(app: &App, command: NodeCommand) -> Result<()> {
    match command {
        NodeCommand::MigratePackages { from, to, apply } => {
            migrate_node_packages(app, &from, &to, apply)
        }
    }
}

pub fn python(app: &App, command: PythonCommand) -> Result<()> {
    match command {
        PythonCommand::Find { request } => find_python(app, request.as_deref()),
    }
}

pub async fn model(app: &App, command: ModelCommand) -> Result<()> {
    let store = osdk_core::model::ModelStore::new(
        app.ctx.dirs.clone(),
        app.ctx.cas.clone(),
        app.ctx.config.settings.link_mode,
    );
    match command {
        ModelCommand::Pull {
            name,
            reference,
            endpoint,
            forward_credentials,
            include,
            exclude,
            variant,
            no_lock,
        } => {
            let reference = osdk_core::model::ModelRef::parse(&reference)?;
            let explicit_endpoint = endpoint.or_else(|| provider_endpoint_env(reference.provider));
            let sources = if let Some(endpoint) = explicit_endpoint {
                let mut source = osdk_core::source::Source::mirror("explicit", &endpoint, i32::MIN);
                source.kind = osdk_core::source::SourceKind::Custom;
                source.forward_credentials =
                    forward_credentials || official_model_endpoint(reference.provider, &endpoint);
                vec![source]
            } else {
                let mut sources = osdk_core::model::source::ranked_sources(
                    &app.ctx,
                    &reference,
                    app.refresh_sources,
                )
                .await?;
                if let Some(id) = app.source_override.as_deref() {
                    let index = sources
                        .iter()
                        .position(|source| source.id == id)
                        .ok_or_else(|| {
                            anyhow!(t!("err.unknown_source", id = id, tool = reference.provider))
                        })?;
                    let selected = sources.remove(index);
                    sources.insert(0, selected);
                }
                sources
            };
            let options = osdk_core::model::pull::PullOptions {
                include,
                exclude,
                variant,
            };
            let mut installed = None;
            let mut last_error = None;
            for source in sources {
                let provider = osdk_core::model::source::provider(
                    reference.provider,
                    source.forward_credentials,
                );
                match osdk_core::model::pull::pull(
                    &app.ctx,
                    provider.as_ref(),
                    &name,
                    &reference,
                    &source.download_url,
                    &options,
                )
                .await
                {
                    Ok(model) => {
                        installed = Some(model);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            source = %source.id,
                            endpoint = %source.download_url,
                            error = %error,
                            "model source failed, trying next endpoint"
                        );
                        last_error = Some(error);
                    }
                }
            }
            let installed = installed.ok_or_else(|| {
                anyhow!(
                    "{}",
                    last_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "no model source candidates".into())
                )
            })?;
            if !no_lock {
                let cwd = std::env::current_dir()?;
                let path = project_lock_path(app, &cwd);
                crate::lockfile::merge_model(&path, &installed.manifest)?;
                println!("updated {}", path.display());
            }
            println!(
                "{} {}@{} -> {}",
                installed.manifest.name,
                installed.manifest.repository,
                installed.manifest.revision,
                installed.path.display()
            );
        }
        ModelCommand::List => {
            for installed in store.list()? {
                println!(
                    "{}  {}:{}@{}  {}",
                    installed.manifest.name,
                    installed.manifest.provider,
                    installed.manifest.repository,
                    installed.manifest.revision,
                    installed.path.display()
                );
            }
        }
        ModelCommand::Path { name } => println!("{}", store.current(&name)?.path.display()),
        ModelCommand::Verify { name } => {
            let manifest = store.verify(&name)?;
            println!(
                "verified {}: {} file(s), revision {}",
                manifest.name,
                manifest.files.len(),
                manifest.revision
            );
        }
        ModelCommand::Remove { name } => {
            if store.remove(&name)? {
                let models = app.ctx.dirs.models();
                let (removed, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
                println!(
                    "removed model {name}; pruned {} object(s), {} freed",
                    removed,
                    human_bytes(bytes)
                );
            } else {
                println!("model {name} is not installed");
            }
        }
        ModelCommand::Env { command } => model_env(app, command)?,
    }
    Ok(())
}

fn model_env(app: &App, command: ModelEnvCommand) -> Result<()> {
    match command {
        ModelEnvCommand::Enable { provider, force } => {
            for provider in selected_providers(provider) {
                crate::config_edit::set_model_env(&app.ctx, provider, true, force)?;
                println!(
                    "{}",
                    t!(
                        "msg.model_env_enabled",
                        provider = provider,
                        mode = if force {
                            t!("label.force")
                        } else {
                            String::new()
                        }
                    )
                );
            }
            println!("{}", t!("msg.model_env_refresh"));
        }
        ModelEnvCommand::Disable { provider } => {
            for provider in selected_providers(provider) {
                crate::config_edit::set_model_env(&app.ctx, provider, false, false)?;
                println!("{}", t!("msg.model_env_disabled", provider = provider));
            }
            println!("{}", t!("msg.model_env_refresh"));
        }
        ModelEnvCommand::List => {
            for provider in [
                osdk_core::model::ProviderId::HuggingFace,
                osdk_core::model::ProviderId::ModelScope,
            ] {
                let config = app.ctx.config.tool_sources(provider.as_str());
                let enabled = config.is_some_and(|config| config.env);
                let force = config.is_some_and(|config| config.env_force);
                println!(
                    "{}: {}{}",
                    provider,
                    if enabled {
                        t!("label.enabled")
                    } else {
                        t!("label.disabled")
                    },
                    if force {
                        t!("label.force")
                    } else {
                        String::new()
                    }
                );
            }
            let environment =
                osdk_core::model::env::configured_env(&app.ctx, |key| std::env::var(key).ok());
            for (key, value) in environment {
                let display = if key.contains("TOKEN") && value.is_empty() {
                    "<disabled>"
                } else {
                    &value
                };
                println!("  {key}={display}");
            }
        }
    }
    Ok(())
}

fn selected_providers(
    provider: Option<osdk_core::model::ProviderId>,
) -> Vec<osdk_core::model::ProviderId> {
    provider.map_or_else(
        || {
            vec![
                osdk_core::model::ProviderId::HuggingFace,
                osdk_core::model::ProviderId::ModelScope,
            ]
        },
        |provider| vec![provider],
    )
}

fn model_reference(
    provider: osdk_core::model::ProviderId,
    value: &str,
) -> Result<osdk_core::model::ModelRef> {
    if value.contains(':') {
        let reference = osdk_core::model::ModelRef::parse(value)?;
        if reference.provider != provider {
            return Err(anyhow!(
                "model reference provider {} does not match {}",
                reference.provider,
                provider
            ));
        }
        Ok(reference)
    } else {
        osdk_core::model::ModelRef::parse(&format!("{provider}:{value}")).map_err(Into::into)
    }
}

fn provider_endpoint_env(provider: osdk_core::model::ProviderId) -> Option<String> {
    match provider {
        osdk_core::model::ProviderId::HuggingFace => std::env::var("HF_ENDPOINT").ok(),
        osdk_core::model::ProviderId::ModelScope => std::env::var("MODELSCOPE_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("MODELSCOPE_DOMAIN").ok()),
    }
}

fn official_model_endpoint(provider: osdk_core::model::ProviderId, endpoint: &str) -> bool {
    let endpoint = endpoint.trim().trim_end_matches('/').to_ascii_lowercase();
    match provider {
        osdk_core::model::ProviderId::HuggingFace => endpoint == "https://huggingface.co",
        osdk_core::model::ProviderId::ModelScope => {
            matches!(
                endpoint.as_str(),
                "https://modelscope.cn" | "https://www.modelscope.ai"
            )
        }
    }
}

pub fn rust(app: &App, command: RustCommand) -> Result<()> {
    match command {
        RustCommand::Component { command } => rust_item(app, "component", command),
        RustCommand::Target { command } => rust_item(app, "target", command),
        RustCommand::Check { repair } => rust_check(app, repair),
        RustCommand::Override { command } => rust_override(app, command),
        RustCommand::Toolchain { command } => rust_toolchain(app, command),
    }
}

fn rust_item(app: &App, kind: &str, command: RustItemCommand) -> Result<()> {
    let (operation, name, toolchain) = match command {
        RustItemCommand::Add { name, toolchain } => ("add", Some(name), toolchain),
        RustItemCommand::Remove { name, toolchain } => ("remove", Some(name), toolchain),
        RustItemCommand::List { toolchain } => ("list", None, toolchain),
    };
    let mut args = vec![kind, operation];
    if let Some(name) = name.as_deref() {
        args.push(name);
    }
    args.extend(["--toolchain", &toolchain]);
    let output = osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &args, None)?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn rust_check(app: &App, repair: bool) -> Result<()> {
    let output = osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &["check"], None)?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if repair {
        let (created, removed) =
            osdk_core::backend::rust::RustBackend::reconcile_markers(&app.ctx)?;
        println!(
            "{}",
            t!(
                "msg.rust_markers_repaired",
                created = created,
                removed = removed
            )
        );
    }
    Ok(())
}

fn rust_override(app: &App, command: RustOverrideCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match command {
        RustOverrideCommand::Import { path } => {
            let directory = path.unwrap_or(cwd);
            let output = osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["override", "list"],
                None,
            )?;
            let text = String::from_utf8_lossy(&output.stdout);
            let canonical = dunce_path(&directory)?;
            let toolchain = parse_rustup_override(&text, &canonical).ok_or_else(|| {
                anyhow!(
                    "no isolated rustup override found for {}",
                    canonical.display()
                )
            })?;
            let path = crate::config_edit::set_project_tool("rust", &toolchain)?;
            println!(
                "{}",
                t!(
                    "msg.rust_override_imported",
                    toolchain = toolchain,
                    path = path.display()
                )
            );
        }
        RustOverrideCommand::Export { path } => {
            let directory = path.unwrap_or(cwd);
            let active = osdk_core::version::resolver::resolve_active(
                "rust",
                &directory,
                &app.ctx.config.tools,
                &["rust-toolchain.toml", "rust-toolchain"],
            )
            .ok_or_else(|| anyhow!("no active osdk Rust version for {}", directory.display()))?;
            let canonical = dunce_path(&directory)?;
            let path_arg = canonical.display().to_string();
            osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["override", "set", &active.spec, "--path", &path_arg],
                None,
            )?;
            println!(
                "{}",
                t!(
                    "msg.rust_override_exported",
                    toolchain = active.spec,
                    path = canonical.display()
                )
            );
        }
    }
    Ok(())
}

fn rust_toolchain(app: &App, command: RustToolchainCommand) -> Result<()> {
    match command {
        RustToolchainCommand::Link { name, path } => {
            validate_rust_link_name(&name)?;
            let canonical = dunce_path(&path)?;
            if !canonical.join("bin").is_dir() {
                return Err(anyhow!(
                    "linked Rust toolchain must contain bin/: {}",
                    canonical.display()
                ));
            }
            let path_arg = canonical.display().to_string();
            osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["toolchain", "link", &name, &path_arg],
                None,
            )?;
            osdk_core::backend::rust::RustBackend::record_linked_toolchain(
                &app.ctx, &name, &canonical,
            )?;
            println!(
                "{}",
                t!(
                    "msg.rust_toolchain_linked",
                    name = name,
                    path = canonical.display()
                )
            );
        }
    }
    Ok(())
}

fn dunce_path(path: &std::path::Path) -> Result<std::path::PathBuf> {
    dunce::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))
}

fn parse_rustup_override(text: &str, path: &std::path::Path) -> Option<String> {
    text.lines().find_map(|line| {
        let (directory, toolchain) = line.rsplit_once(char::is_whitespace)?;
        (dunce::canonicalize(directory).ok().as_deref() == Some(path))
            .then(|| toolchain.trim().to_string())
    })
}

fn validate_rust_link_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
        || name.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("invalid linked Rust toolchain name `{name}`"));
    }
    Ok(())
}

fn find_python(app: &App, request: Option<&str>) -> Result<()> {
    let backend = app.registry.get("python")?;
    let installed = backend.list_installed(&app.ctx)?;
    let selected = request
        .map(|request| osdk_core::backend::python::select_installed(request, &installed))
        .unwrap_or(None);
    let mut seen = std::collections::BTreeSet::new();
    let mut found = false;

    for identity in installed {
        if selected
            .as_deref()
            .is_some_and(|selected| selected != identity)
        {
            continue;
        }
        let version = ToolVersion::new("python", &identity);
        for directory in backend.bin_paths(&app.ctx, &version)? {
            for name in python_executable_names() {
                let path = directory.join(name);
                if path.is_file() && seen.insert(canonical_or_original(&path)) {
                    println!("managed\t{}\t{}", identity, path.display());
                    found = true;
                }
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in python_executable_names() {
                let candidate = directory.join(name);
                if candidate.is_file() && seen.insert(canonical_or_original(&candidate)) {
                    println!("path\t-\t{}", candidate.display());
                    found = true;
                }
            }
        }
    }

    for candidate in system_python_candidates() {
        if candidate.is_file() && seen.insert(canonical_or_original(&candidate)) {
            println!("system\t-\t{}", candidate.display());
            found = true;
        }
    }
    if !found {
        return Err(anyhow!(t!("err.python_not_found")));
    }
    Ok(())
}

fn python_executable_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "python.exe",
            "python3.exe",
            "pypy.exe",
            "pypy3.exe",
            "graalpy.exe",
        ]
    }
    #[cfg(not(windows))]
    {
        &["python", "python3", "pypy", "pypy3", "graalpy"]
    }
}

fn canonical_or_original(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn system_python_candidates() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Some(directory) = std::env::var_os("SystemRoot") {
            candidates.push(std::path::PathBuf::from(directory).join("py.exe"));
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![
            "/usr/bin/python3".into(),
            "/usr/local/bin/python3".into(),
            "/opt/homebrew/bin/python3".into(),
        ]
    }
}

fn migrate_node_packages(app: &App, from: &str, to: &str, apply: bool) -> Result<()> {
    let source = managed_node_tools(app, from)?;
    let target = managed_node_tools(app, to)?;
    let source_packages = list_global_npm_packages(&source)?;
    let target_packages = list_global_npm_packages(&target)?;
    let target_names: std::collections::BTreeSet<_> = target_packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let mut planned = Vec::new();
    for package in source_packages {
        if package.name == "npm" {
            println!("{}", t!("msg.node_migrate_skip_npm"));
            continue;
        }
        if package.native {
            println!(
                "{}",
                t!("msg.node_migrate_skip_native", package = package.spec())
            );
            continue;
        }
        if !target_names.contains(package.name.as_str()) {
            planned.push(package);
        }
    }

    if planned.is_empty() {
        println!("{}", t!("msg.node_migrate_nothing"));
        return Ok(());
    }
    for package in &planned {
        println!("{}", t!("msg.node_migrate_plan", package = package.spec()));
    }
    if !apply {
        println!("{}", t!("msg.node_migrate_dry_run"));
        return Ok(());
    }

    let before = target_packages;
    let specs: Vec<String> = planned.iter().map(NpmPackage::spec).collect();
    if let Err(error) = npm_install_global(&target, &specs) {
        return match restore_global_npm_packages(&target, &before) {
            Ok(()) => Err(error.context(t!("err.node_migrate_rolled_back"))),
            Err(rollback) => Err(error.context(format!(
                "{}: {rollback:#}",
                t!("err.node_migrate_rollback_failed")
            ))),
        };
    }
    println!(
        "{}",
        t!(
            "msg.node_migrate_applied",
            count = planned.len(),
            version = to
        )
    );
    Ok(())
}

#[derive(Debug)]
struct ManagedNodeTools {
    bin: std::path::PathBuf,
    npm: std::path::PathBuf,
}

fn managed_node_tools(app: &App, version: &str) -> Result<ManagedNodeTools> {
    let install = app.ctx.dirs.install_path("node", version);
    if !osdk_core::pipeline::is_installed(&app.ctx.dirs, "node", version) {
        return Err(anyhow!(t!(
            "err.not_installed",
            tool = "node",
            ver = version
        )));
    }
    let bin = match app.ctx.platform.os {
        osdk_core::platform::Os::Windows => install,
        _ => install.join("bin"),
    };
    let npm = if matches!(app.ctx.platform.os, osdk_core::platform::Os::Windows) {
        bin.join("npm.cmd")
    } else {
        bin.join("npm")
    };
    if !npm.is_file() {
        return Err(anyhow!(
            "managed npm executable not found at {}",
            npm.display()
        ));
    }
    Ok(ManagedNodeTools { bin, npm })
}

#[derive(Debug, Clone)]
struct NpmPackage {
    name: String,
    version: String,
    native: bool,
}

impl NpmPackage {
    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

fn list_global_npm_packages(tools: &ManagedNodeTools) -> Result<Vec<NpmPackage>> {
    let output = npm_command(tools, &["ls", "-g", "--depth=0", "--json", "--long"])?;
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing managed npm package list")?;
    let mut packages = Vec::new();
    if let Some(dependencies) = value
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
    {
        for (name, metadata) in dependencies {
            let Some(version) = metadata.get("version").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let native = metadata
                .get("hasInstallScript")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || metadata
                    .get("gypfile")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            packages.push(NpmPackage {
                name: name.clone(),
                version: version.to_string(),
                native,
            });
        }
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

fn npm_install_global(tools: &ManagedNodeTools, specs: &[String]) -> Result<()> {
    let mut args = vec!["install", "-g"];
    args.extend(specs.iter().map(String::as_str));
    npm_command(tools, &args).map(|_| ())
}

fn restore_global_npm_packages(tools: &ManagedNodeTools, packages: &[NpmPackage]) -> Result<()> {
    let current = list_global_npm_packages(tools)?;
    let removable: Vec<String> = current
        .iter()
        .filter(|package| package.name != "npm")
        .map(|package| package.name.clone())
        .collect();
    if !removable.is_empty() {
        let mut args = vec!["uninstall", "-g"];
        args.extend(removable.iter().map(String::as_str));
        npm_command(tools, &args)?;
    }
    let desired: Vec<String> = packages
        .iter()
        .filter(|package| package.name != "npm")
        .map(NpmPackage::spec)
        .collect();
    if !desired.is_empty() {
        npm_install_global(tools, &desired)?;
    }
    Ok(())
}

fn npm_command(tools: &ManagedNodeTools, args: &[&str]) -> Result<std::process::Output> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![tools.bin.clone()];
    paths.extend(std::env::split_paths(&inherited));
    let path = std::env::join_paths(paths)?;
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C"]).arg(&tools.npm);
        command
    } else {
        std::process::Command::new(&tools.npm)
    };
    let output = command
        .args(args)
        .env("PATH", path)
        .output()
        .with_context(|| format!("running managed npm at {}", tools.npm.display()))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(anyhow!(
            "managed npm {} failed with {}:\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

pub fn prune(app: &App, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("{}", t!("msg.prune_dry_run"));
        return Ok(());
    }
    if !app.prompt.confirm(&t!("prompt.prune"))? {
        println!("{}", t!("msg.cancelled"));
        return Ok(());
    }
    let models = app.ctx.dirs.models();
    let (removed, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
    println!(
        "{}",
        t!("msg.pruned", count = removed, size = human_bytes(bytes))
    );
    Ok(())
}

pub fn doctor(app: &App) -> Result<()> {
    use osdk_core::store::link::same_filesystem;
    println!("{}", t!("doctor.title"));
    println!("  platform     : {}", app.ctx.platform);
    println!("  data_dir     : {}", app.ctx.dirs.data.display());
    println!("  store_dir    : {}", app.ctx.dirs.store.display());
    println!("  install_dir  : {}", app.ctx.dirs.installs.display());
    let same = same_filesystem(&app.ctx.dirs.store, &app.ctx.dirs.installs);
    println!(
        "  store/install same filesystem: {} ({})",
        same,
        if same {
            t!("doctor.same_fs_ok")
        } else {
            t!("doctor.same_fs_no")
        }
    );
    let shims = app.ctx.dirs.shims();
    let on_path = std::env::var("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == shims))
        .unwrap_or(false);
    println!(
        "  shims dir    : {} (on PATH: {})",
        shims.display(),
        on_path
    );
    println!("  backends     : {}", app.registry.ids().join(", "));
    Ok(())
}

fn describe_origin(origin: &osdk_core::version::resolver::VersionOrigin) -> String {
    use osdk_core::version::resolver::VersionOrigin::*;
    match origin {
        ProjectConfig(p) => format!("project {}", p.display()),
        ToolVersions(p) => format!(".tool-versions {}", p.display()),
        IdiomaticFile(p) => format!("{}", p.display()),
        ProjectMetadata(p) => format!("{}", p.display()),
        GlobalConfig => "global config".to_string(),
    }
}

fn dynamic_scan_report(app: &App) -> Result<ScanReport> {
    Ok(osdk_core::shim::scan_dynamic_installs(&app.ctx)?)
}

fn routed_bin_names_for_version(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
) -> Result<Vec<String>> {
    let mut names = osdk_core::shim::routed_bin_names(ctx, backend, version)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if version.backend.contains(':') {
        names.extend(osdk_core::shim::dynamic_manifest_bin_names(
            ctx,
            &version.backend,
            &version.version,
        )?);
    }
    Ok(names.into_iter().collect())
}

fn managed_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = backend.bin_paths(ctx, version)?;
    if version.backend.contains(':') {
        paths.extend(osdk_core::shim::dynamic_manifest_bin_paths(
            ctx,
            &version.backend,
            &version.version,
        )?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn requested_spec_literal(tool: &str) -> Option<String> {
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

fn installed_shim_owners(
    app: &App,
) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>> {
    let mut owners =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    let cwd = std::env::current_dir()?;
    for backend in all_display_backends(app)? {
        let mut selected_versions = std::collections::BTreeSet::new();
        let dynamic_request = osdk_core::shim::dynamic_request_from_config(&app.ctx, backend.id());
        if let Some((active_spec, active_is_range)) = osdk_core::version::resolver::resolve_active(
            backend.id(),
            &cwd,
            &app.ctx.config.tools,
            backend.idiomatic_files(),
        )
        .map(|active| (active.spec, active.is_range))
        .or_else(|| dynamic_request.map(|request| (request.spec.to_string(), false)))
        {
            let expanded = app
                .ctx
                .config
                .expand_alias(backend.id(), &active_spec)
                .unwrap_or(active_spec);
            let installed = backend.list_installed(&app.ctx)?;
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
            selected_versions.extend(backend.list_installed(&app.ctx)?);
        }
        for version in selected_versions {
            let version = ToolVersion::new(backend.id(), version);
            for name in routed_bin_names_for_version(&app.ctx, backend.as_ref(), &version)? {
                owners
                    .entry(name)
                    .or_default()
                    .insert(backend.id().to_string());
            }
        }
    }
    Ok(owners)
}

fn ensure_no_shim_conflicts(app: &App, backend_id: &str, names: &[String]) -> Result<()> {
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

fn is_real_shim_conflict(name: &str, owner_ids: &std::collections::BTreeSet<String>) -> bool {
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

#[cfg(test)]
mod command_flow_tests {
    use super::*;

    #[test]
    fn project_dependency_section_preserves_existing_section_and_defaults_to_dev() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("package.json");
        for (manifest, expected) in [
            (
                r#"{"dependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::Dependencies,
            ),
            (
                r#"{"devDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::DevDependencies,
            ),
            (
                r#"{"optionalDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::OptionalDependencies,
            ),
            (
                r#"{"peerDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::PeerDependencies { also_dev: false },
            ),
            (
                r#"{"peerDependencies":{"prettier":"^2"},"devDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::PeerDependencies { also_dev: true },
            ),
            (
                r#"{"dependencies":{}}"#,
                ProjectDependencySection::DevDependencies,
            ),
        ] {
            std::fs::write(&path, manifest).unwrap();
            assert_eq!(
                project_dependency_section(&path, "prettier").unwrap(),
                expected
            );
        }
    }

    #[test]
    fn native_project_args_preserve_sections_and_disable_scripts() {
        use osdk_core::npm_tools::NpmInstaller;

        let cases = [
            (
                NpmInstaller::Npm,
                ProjectDependencySection::Dependencies,
                vec!["install", "--save-prod", "--ignore-scripts", "prettier@3"],
            ),
            (
                NpmInstaller::Npm,
                ProjectDependencySection::DevDependencies,
                vec!["install", "--save-dev", "--ignore-scripts", "prettier@3"],
            ),
            (
                NpmInstaller::Npm,
                ProjectDependencySection::OptionalDependencies,
                vec![
                    "install",
                    "--save-optional",
                    "--ignore-scripts",
                    "prettier@3",
                ],
            ),
            (
                NpmInstaller::Pnpm,
                ProjectDependencySection::PeerDependencies { also_dev: true },
                vec!["add", "--save-peer", "-D", "--ignore-scripts", "prettier@3"],
            ),
        ];
        for (installer, section, expected) in cases {
            assert_eq!(
                project_manager_args(installer, "prettier@3", section),
                expected
            );
        }
    }

    #[test]
    fn project_package_spec_preserves_user_request_and_defaults_to_exact() {
        assert_eq!(
            project_package_spec("prettier", Some("3"), "3.6.2"),
            "prettier@3"
        );
        assert_eq!(
            project_package_spec("@antfu/ni", None, "0.21.12"),
            "@antfu/ni@0.21.12"
        );
    }

    #[test]
    fn exact_resolved_node_is_bound_to_npm_requests() {
        let resolved = vec![(
            ToolRequest::parse("node@20.10.0").unwrap(),
            ToolVersion::new("node", "20.10.0"),
        )];
        let mut requests = vec![
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolRequest::parse("python@3.12.0").unwrap(),
        ];

        bind_request_node_version(&mut requests, &resolved);

        assert_eq!(
            requests[0].options[osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION],
            "20.10.0"
        );
        assert!(requests[1].options.is_empty());
    }

    #[test]
    fn lock_tuple_uses_the_same_exact_node_as_graph_generation() {
        let mut npm = ToolVersion::new("npm:prettier", "3.6.2");
        npm.options.insert(
            osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "24.0.0".into(),
        );
        let mut resolved = vec![
            (ToolRequest::parse("npm:prettier@3.6.2").unwrap(), npm),
            (
                ToolRequest::parse("node@20.10.0").unwrap(),
                ToolVersion::new("node", "20.10.0"),
            ),
        ];

        bind_resolved_node_version(&mut resolved);

        assert_eq!(
            resolved[0].1.options[osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION],
            "20.10.0"
        );
    }
}
