//! Command handlers.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use osdk_core::backend::native_tool::{
    LOCKED_NATIVE_RUNTIME_OPTION, LOCKED_NATIVE_RUNTIME_VERSION_OPTION,
};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::inventory::ScanReport;
use osdk_core::package_registry::{self, PackageManager, RegistryPlan, RegistryProbe};
use osdk_core::source::select;
use osdk_core::t;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::app::App;
use crate::cli::{
    AliasCommand, AndroidAvdCommand, AndroidCommand, AndroidLicensesCommand, AndroidSdkRootCommand,
    ConfigCommand, ModelCommand, ModelEnvCommand, NodeCommand, PythonCommand, RegistryCommand,
    RustCommand, RustItemCommand, RustOverrideCommand, RustToolchainCommand, SourceCommand,
    TrustCommand,
};

const GLOBAL_NPM_UNINSTALL_JOURNAL_DIR: &str = "transactions/global-npm-uninstall";
static NEXT_GLOBAL_NPM_UNINSTALL_FILE: AtomicU64 = AtomicU64::new(0);

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

/// Whether the supplied options convey nothing but license consent. An empty
/// list qualifies, so the plain `osdk install` path is unchanged.
fn opts_are_only_consent(opts: &[String]) -> bool {
    opts.iter().all(|opt| {
        opt.split_once('=')
            .is_some_and(|(key, _)| crate::lockfile::CONSENT_OPTIONS.contains(&key.trim()))
    })
}

pub async fn install(
    app: &mut App,
    tools: Vec<String>,
    opts: Vec<String>,
    force: bool,
) -> Result<()> {
    let explicit = !tools.is_empty();
    // Options normally mean the caller wants something the lock file does not
    // describe, so replay is skipped. Consent is the exception: it records a
    // decision rather than selecting an artifact, and is deliberately absent
    // from the lock, so requiring it must not make a committed lock file
    // impossible to install from.
    let use_lock = !explicit && opts_are_only_consent(&opts);
    let (requests, trusted_replay) = if use_lock {
        match requests_from_lock(app)? {
            Some(requests) => (requests, true),
            None => (gather_requests(app, tools)?, false),
        }
    } else {
        (gather_requests(app, tools)?, false)
    };
    install_requests(app, requests, opts, trusted_replay, force).await?;
    Ok(())
}

pub async fn lock(app: &mut App, tools: Vec<String>, opts: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let mut resolved = resolve_requests(app, requests, opts).await?;
    // A reproducible npm tool lock includes npm's exact transitive graph.
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
            let (backend, version) = install_one_without_shims(app, &node_request, false).await?;
            generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
            resolved.push((node_request, version));
        } else {
            let node = resolved
                .iter()
                .find(|(request, _)| request.backend == "node")
                .map(|(request, _)| request.clone())
                .expect("checked above");
            let (backend, version) = install_one_without_shims(app, &node, false).await?;
            generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
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
    let resolved = install_requests(app, requests, opts, false, false).await?;
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    crate::lockfile::merge_resolved(&path, app.ctx.platform, &app.ctx.dirs, &resolved)?;
    println!("updated {}", path.display());
    Ok(())
}

pub async fn exec_cmd(app: &mut App, tools: Vec<String>, command: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    let resolved = install_requests(app, requests, Vec::new(), false, false).await?;
    let mut paths = Vec::new();
    let mut env = std::collections::BTreeMap::new();
    for (request, version) in &resolved {
        let backend = app.registry.get(&version.backend)?;
        paths.extend(managed_bin_paths(
            &app.ctx,
            backend.as_ref(),
            version,
            Some(request),
        )?);
        env.extend(backend.exec_env(&app.ctx, version)?);
    }
    // JVM tools that bundle no runtime abort unless a JDK is visible, and no
    // backend can describe another backend's install. Respect an existing
    // JAVA_HOME, whether it came from activation or from the user.
    if resolved
        .iter()
        .any(|(_, version)| osdk_core::shim::requires_external_jdk(&version.backend))
        && !env.contains_key("JAVA_HOME")
        && std::env::var_os("JAVA_HOME").is_none()
    {
        let cwd = std::env::current_dir()?;
        if let Some((jdk_env, jdk_paths)) =
            osdk_core::shim::managed_jdk_env(&app.ctx, &app.registry, &cwd)
        {
            env.extend(jdk_env);
            paths.extend(jdk_paths);
        }
    }
    paths.sort_by_key(|path| managed_runtime_path_priority(path));
    let managed_paths = paths.clone();
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
    // A bare name is normally left to the OS, but Windows `CreateProcess` does
    // not apply PATHEXT, so a tool shipped only as a `.cmd`/`.bat` launcher
    // (the NDK's per-API clang wrappers, for one) would not be found even
    // though it is on the PATH we just built. Resolve it ourselves first.
    let managed_program = resolve_program_in_dirs(&managed_program, &managed_paths);
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
        &managed_bin_paths(&app.ctx, backend.as_ref(), version, None)?,
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

/// Resolve a bare program name against managed directories.
///
/// Only bare names are resolved: an explicit path is the caller's choice and is
/// returned untouched. On Windows the executable extensions are tried in
/// PATHEXT-like order so a `.cmd` wrapper is reachable by its plain name; on
/// Unix the OS already handles this, so the input is returned unchanged.
fn resolve_program_in_dirs(
    program: &std::path::Path,
    directories: &[std::path::PathBuf],
) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if program.components().count() != 1 {
            return program.to_path_buf();
        }
        let name = program.to_string_lossy();
        for directory in directories {
            for candidate in [
                format!("{name}.exe"),
                format!("{name}.cmd"),
                format!("{name}.bat"),
                name.to_string(),
            ] {
                let path = directory.join(&candidate);
                if path.is_file() {
                    return path;
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = directories;
    program.to_path_buf()
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
    let project_yarn_version = project_yarn_version(cwd);
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
    mut requests: Vec<ToolRequest>,
    opts: Vec<String>,
    trusted_replay: bool,
    force: bool,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    for request in &mut requests {
        if !trusted_replay {
            reject_public_internal_options(&request.options)?;
        }
        for (key, value) in &parsed_opts {
            request.options.insert(key.clone(), value.clone());
        }
    }
    let mut requests = inject_managed_dependencies(app, requests)?;
    if requests.is_empty() {
        println!("{}", t!("msg.nothing_to_install"));
        return Ok(Vec::new());
    }
    for req in &mut requests {
        apply_source_override(app, &req.backend);
    }
    // The compatibility install/exec/lock paths always target the isolated
    // npm package root, even when the merged config has a global selection for
    // the same package and version.
    mark_isolated_npm_scope(&mut requests);
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
        let (backend, version) = install_one_without_shims(app, &request, force).await?;
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    let (rust_requests, remaining_requests) =
        partition_runtime_dependency(remaining_requests, "rust", "cargo:");
    for request in rust_requests {
        let (backend, version) = install_one_without_shims(app, &request, force).await?;
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    let (go_requests, mut remaining_requests) =
        partition_runtime_dependency(remaining_requests, "go", "go:");
    for request in go_requests {
        let (backend, version) = install_one_without_shims(app, &request, force).await?;
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    bind_request_node_version(&mut remaining_requests, &resolved);
    bind_request_rust_version(&mut remaining_requests, &resolved)?;
    bind_request_go_version(&mut remaining_requests, &resolved)?;
    let jobs = app.ctx.config.settings.jobs.max(1);
    let installed = stream::iter(remaining_requests.into_iter().map(|req| {
        let app_ref: &App = app;
        async move {
            let installed = install_one_without_shims(app_ref, &req, force).await?;
            Ok::<_, anyhow::Error>((req, installed))
        }
    }))
    .buffer_unordered(jobs)
    .try_collect::<Vec<_>>()
    .await?;
    for (request, (backend, version)) in installed {
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
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

fn resolved_rust_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "rust").then_some(version.version.clone()))
}

fn resolved_go_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "go").then_some(version.version.clone()))
}

fn exact_rust_version(version: &str) -> bool {
    matches!(VersionSpec::parse(version), VersionSpec::Exact(exact) if exact == version)
}

fn require_exact_rust_spec(spec: &VersionSpec) -> Result<()> {
    if matches!(spec, VersionSpec::Exact(version) if exact_rust_version(version)) {
        return Ok(());
    }
    anyhow::bail!(
        "Cargo tools require one exact managed Rust version; configure `rust = \"1.91.1\"` or include `rust@1.91.1`"
    )
}

fn exact_go_version(version: &str) -> bool {
    matches!(VersionSpec::parse(version), VersionSpec::Exact(exact) if exact == version)
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

fn bind_resolved_rust_version(resolved: &mut [(ToolRequest, ToolVersion)]) -> Result<()> {
    if !resolved
        .iter()
        .any(|(_, version)| version.backend.starts_with("cargo:"))
    {
        return Ok(());
    }
    let rust_version = resolved_rust_version(resolved)
        .ok_or_else(|| anyhow!("Cargo tools require exactly one managed Rust dependency"))?;
    if !exact_rust_version(&rust_version) {
        anyhow::bail!("Cargo tools require an exact resolved Rust version, got `{rust_version}`");
    }
    for (_, version) in resolved {
        if version.backend.starts_with("cargo:") {
            version
                .options
                .insert(LOCKED_NATIVE_RUNTIME_OPTION.into(), "rust".into());
            version.options.insert(
                LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(),
                rust_version.clone(),
            );
        }
    }
    Ok(())
}

fn bind_resolved_go_version(resolved: &mut [(ToolRequest, ToolVersion)]) -> Result<()> {
    if !resolved
        .iter()
        .any(|(_, version)| version.backend.starts_with("go:"))
    {
        return Ok(());
    }
    let go_version = resolved_go_version(resolved)
        .ok_or_else(|| anyhow!("Go tools require exactly one managed Go dependency"))?;
    if !exact_go_version(&go_version) {
        anyhow::bail!("Go tools require an exact resolved Go version, got `{go_version}`");
    }
    for (_, version) in resolved {
        if version.backend.starts_with("go:") {
            version
                .options
                .insert(LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into());
            version.options.insert(
                LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(),
                go_version.clone(),
            );
        }
    }
    Ok(())
}

fn bind_request_rust_version(
    requests: &mut [ToolRequest],
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    if !requests
        .iter()
        .any(|request| request.backend.starts_with("cargo:"))
    {
        return Ok(());
    }
    let rust_version = resolved_rust_version(resolved)
        .ok_or_else(|| anyhow!("Cargo tools require exactly one managed Rust dependency"))?;
    if !exact_rust_version(&rust_version) {
        anyhow::bail!("Cargo tools require an exact resolved Rust version, got `{rust_version}`");
    }
    for request in requests {
        if request.backend.starts_with("cargo:") {
            request
                .options
                .insert(LOCKED_NATIVE_RUNTIME_OPTION.into(), "rust".into());
            request.options.insert(
                LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(),
                rust_version.clone(),
            );
        }
    }
    Ok(())
}

fn bind_request_go_version(
    requests: &mut [ToolRequest],
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    if !requests
        .iter()
        .any(|request| request.backend.starts_with("go:"))
    {
        return Ok(());
    }
    let go_version = resolved_go_version(resolved)
        .ok_or_else(|| anyhow!("Go tools require exactly one managed Go dependency"))?;
    if !exact_go_version(&go_version) {
        anyhow::bail!("Go tools require an exact resolved Go version, got `{go_version}`");
    }
    for request in requests {
        if request.backend.starts_with("go:") {
            request
                .options
                .insert(LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into());
            request.options.insert(
                LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(),
                go_version.clone(),
            );
        }
    }
    Ok(())
}

fn partition_runtime_dependency(
    requests: Vec<ToolRequest>,
    runtime: &str,
    dependent_prefix: &str,
) -> (Vec<ToolRequest>, Vec<ToolRequest>) {
    if !requests
        .iter()
        .any(|request| request.backend.starts_with(dependent_prefix))
    {
        return (Vec::new(), requests);
    }
    requests
        .into_iter()
        .partition(|request| request.backend == runtime)
}

fn mark_isolated_npm_scope(requests: &mut [ToolRequest]) {
    for request in requests {
        if request.backend.starts_with("npm:") {
            request.options.insert(
                osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
                osdk_core::npm_tools::ToolScope::Project.as_str().into(),
            );
        }
    }
}

async fn resolve_requests(
    app: &mut App,
    mut requests: Vec<ToolRequest>,
    opts: Vec<String>,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    for request in &mut requests {
        reject_public_internal_options(&request.options)?;
        for (key, value) in &parsed_opts {
            request.options.insert(key.clone(), value.clone());
        }
    }
    let requests = inject_managed_dependencies(app, requests)?;
    let (rust_requests, remaining_requests) =
        partition_runtime_dependency(requests, "rust", "cargo:");
    let (go_requests, remaining_requests) =
        partition_runtime_dependency(remaining_requests, "go", "go:");
    let requests = rust_requests
        .into_iter()
        .chain(go_requests)
        .chain(remaining_requests);
    let mut resolved = Vec::new();
    for mut request in requests {
        bind_request_rust_version(std::slice::from_mut(&mut request), &resolved)?;
        bind_request_go_version(std::slice::from_mut(&mut request), &resolved)?;
        mark_isolated_npm_scope(std::slice::from_mut(&mut request));
        apply_source_override(app, &request.backend);
        let backend = app.registry.get(&request.backend)?;
        let effective = expand_request_alias(app, backend.as_ref(), &request)?;
        let mut version = backend.resolve_version(&app.ctx, &effective).await?;
        bind_dynamic_request_options(&effective, &mut version);
        resolved.push((request, version));
    }
    bind_resolved_rust_version(&mut resolved)?;
    bind_resolved_go_version(&mut resolved)?;
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
            let (key, value) = s
                .split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| anyhow!(t!("err.invalid_opt", val = s)))?;
            if key.starts_with("__osdk_") {
                anyhow::bail!("internal option `{key}` cannot be set by the user");
            }
            Ok((key, value))
        })
        .collect()
}

fn reject_public_internal_options(
    options: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    if let Some(key) = options.keys().find(|key| key.starts_with("__osdk_")) {
        anyhow::bail!("internal option `{key}` cannot be set by the user");
    }
    Ok(())
}

async fn install_one_without_shims(
    app: &App,
    req: &ToolRequest,
    force: bool,
) -> Result<(std::sync::Arc<dyn Backend>, ToolVersion)> {
    if app.refresh_sources {
        let backend = app.registry.get(&req.backend)?;
        select::refresh(&app.ctx, backend.as_ref()).await?;
    }
    let backend = app.registry.get(&req.backend)?;
    let effective = expand_request_alias(app, backend.as_ref(), req)?;
    let mut tv = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", req.backend, req.spec))?;
    bind_dynamic_request_options(&effective, &mut tv);

    if !force
        && osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version)
        && !backend.id().contains(':')
    {
        backend.ensure_post_install(&app.ctx, &tv)?;
        println!("{}", t!("msg.already_installed", tool = tv));
    } else {
        if force {
            // The marker is what every install path trusts instead of
            // looking at the bytes, so clearing it is what turns this
            // into a real reinstall rather than a no-op that prints
            // success without repairing anything.
            osdk_core::pipeline::clear_complete_marker(&app.ctx.dirs, backend.id(), &tv.version)?;
        }
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

fn bind_dynamic_request_options(request: &ToolRequest, version: &mut ToolVersion) {
    if version.backend.contains(':') {
        for (name, value) in &request.options {
            if !name.starts_with("__osdk_") || !version.options.contains_key(name) {
                version.options.insert(name.clone(), value.clone());
            }
        }
    }
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
        let requests = tools
            .iter()
            .map(|s| {
                resolve_explicit_request(
                    app,
                    s,
                    &app.ctx.config.tool_configs,
                    &app.ctx.config.tools,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(requests);
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
    Ok(out)
}

fn inherit_configured_options_from(
    request: &mut ToolRequest,
    tool_configs: &std::collections::BTreeMap<String, osdk_core::config::ToolConfigEntry>,
    tools: &std::collections::BTreeMap<String, String>,
    configured_key: Option<&str>,
) {
    let configured = configured_key
        .and_then(|key| tool_configs.get(key))
        .or_else(|| tool_configs.get(&request.backend))
        .or_else(|| {
            tools.iter().find_map(|(key, value)| {
                ToolRequest::parse(value)
                    .ok()
                    .filter(|configured| configured.backend == request.backend)
                    .and_then(|_| tool_configs.get(key))
            })
        })
        .map(|entry| entry.to_request_options())
        .unwrap_or_default();
    let explicit = std::mem::take(&mut request.options);
    request.options = configured;
    request.options.extend(explicit);
}

fn resolve_explicit_request(
    app: &App,
    operand: &str,
    tool_configs: &std::collections::BTreeMap<String, osdk_core::config::ToolConfigEntry>,
    tools: &std::collections::BTreeMap<String, String>,
) -> Result<ToolRequest> {
    let (mut request, configured_key) =
        resolve_explicit_request_target(app, operand, tool_configs, tools)?;
    inherit_configured_options_from(&mut request, tool_configs, tools, configured_key.as_deref());
    Ok(request)
}

fn resolve_explicit_request_target(
    app: &App,
    operand: &str,
    tool_configs: &std::collections::BTreeMap<String, osdk_core::config::ToolConfigEntry>,
    _tools: &std::collections::BTreeMap<String, String>,
) -> Result<(ToolRequest, Option<String>)> {
    let parsed = ToolRequest::parse(operand).map_err(|e| anyhow!("{e}"))?;
    if app.registry.get(&parsed.backend).is_ok() {
        return Ok((parsed, None));
    }
    let Some(entry) = tool_configs.get(&parsed.backend) else {
        return Ok((parsed, None));
    };
    let configured_key = parsed.backend.clone();
    let mut resolved = ToolRequest::parse(entry.version()).map_err(|e| anyhow!("{e}"))?;
    if app.registry.get(&resolved.backend).is_err() {
        return Ok((parsed, None));
    }
    if requested_spec_literal(operand).is_some() {
        resolved.spec = parsed.spec;
    }
    Ok((resolved, Some(configured_key)))
}

fn apply_use_options(
    config: &osdk_core::config::Config,
    request: &mut ToolRequest,
    global: bool,
    opts: &[String],
    configured_key: Option<&str>,
) -> Result<()> {
    if global && (request.backend.starts_with("npm:") || request.backend.starts_with("go:")) {
        inherit_configured_options_from(
            request,
            &config.global_tool_configs,
            &config.global_tools,
            configured_key,
        );
    } else {
        inherit_configured_options_from(
            request,
            &config.tool_configs,
            &config.tools,
            configured_key,
        );
    }
    for (key, value) in parse_opts(opts)? {
        request.options.insert(key, value);
    }
    reject_public_internal_options(&request.options)?;
    Ok(())
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

fn inject_managed_dependencies(app: &App, requests: Vec<ToolRequest>) -> Result<Vec<ToolRequest>> {
    let requests = inject_node_dependency(app, requests)?;
    let requests = inject_rust_dependency(app, requests)?;
    inject_go_dependency(app, requests)
}

fn inject_go_dependency(app: &App, requests: Vec<ToolRequest>) -> Result<Vec<ToolRequest>> {
    let cwd = std::env::current_dir()?;
    inject_go_dependency_at(app, requests, &cwd)
}

fn inject_go_dependency_at(
    app: &App,
    mut requests: Vec<ToolRequest>,
    cwd: &std::path::Path,
) -> Result<Vec<ToolRequest>> {
    if !requests
        .iter()
        .any(|request| request.backend.starts_with("go:"))
    {
        return Ok(requests);
    }
    for request in &mut requests {
        if request.backend == "golang" {
            request.backend = "go".into();
        }
    }
    let go_requests = requests
        .iter()
        .filter(|request| request.backend == "go")
        .count();
    if go_requests > 1 {
        anyhow::bail!("Go tools require exactly one managed Go request");
    }
    if go_requests == 1 {
        return Ok(requests);
    }

    let backend = app.registry.get("go")?;
    let active = osdk_core::version::resolver::resolve_active(
        "go",
        cwd,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .ok_or_else(|| {
        anyhow!(
            "Go tools require a managed Go selection; configure `go = \"1.24\"` or include `go@1.24`"
        )
    })?;
    let spec = if active.is_range {
        VersionSpec::parse_range(&active.spec)?
    } else {
        VersionSpec::parse(&active.spec)
    };
    requests.push(ToolRequest {
        backend: "go".into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get("go")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    });
    Ok(requests)
}

fn inject_rust_dependency(app: &App, requests: Vec<ToolRequest>) -> Result<Vec<ToolRequest>> {
    let cwd = std::env::current_dir()?;
    inject_rust_dependency_at(app, requests, &cwd)
}

fn inject_rust_dependency_at(
    app: &App,
    mut requests: Vec<ToolRequest>,
    cwd: &std::path::Path,
) -> Result<Vec<ToolRequest>> {
    if !requests
        .iter()
        .any(|request| request.backend.starts_with("cargo:"))
    {
        return Ok(requests);
    }
    let rust_requests = requests
        .iter()
        .filter(|request| request.backend == "rust")
        .count();
    if rust_requests > 1 {
        anyhow::bail!("Cargo tools require exactly one managed Rust request");
    }
    if rust_requests == 1 {
        let rust = requests
            .iter()
            .find(|request| request.backend == "rust")
            .expect("counted one Rust request");
        require_exact_rust_spec(&rust.spec)?;
        return Ok(requests);
    }

    let backend = app.registry.get("rust")?;
    let active = osdk_core::version::resolver::resolve_active(
        "rust",
        cwd,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .ok_or_else(|| {
        anyhow!(
            "Cargo tools require one exact managed Rust version; configure `rust = \"1.91.1\"` or include `rust@1.91.1`"
        )
    })?;
    let spec = if active.is_range {
        VersionSpec::parse_range(&active.spec)?
    } else {
        VersionSpec::parse(&active.spec)
    };
    require_exact_rust_spec(&spec)?;
    requests.push(ToolRequest {
        backend: "rust".into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get("rust")
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
    let (mut req, configured_key) = if global {
        resolve_explicit_request_target(
            app,
            &tool,
            &app.ctx.config.global_tool_configs,
            &app.ctx.config.global_tools,
        )?
    } else {
        let (request, key) = resolve_explicit_request_target(
            app,
            &tool,
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )?;
        (request, key)
    };
    apply_use_options(
        &app.ctx.config,
        &mut req,
        global,
        &opts,
        configured_key.as_deref(),
    )?;
    if global && req.backend.starts_with("npm:") {
        if configured_key.is_some() {
            anyhow::bail!(
                "global npm package aliases are not supported; use the canonical npm:<package> request"
            );
        }
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
            app.ctx.config.settings.npm.default_installer,
        )?;
        if let Some(project) = plan.project {
            return use_project_npm(
                app,
                req,
                requested_spec,
                project,
                requested_installer,
                configured_key,
            )
            .await;
        }
    }
    use_legacy_cmd(app, req, requested_spec, global, configured_key).await
}

async fn use_legacy_cmd(
    app: &mut App,
    req: ToolRequest,
    requested_spec: Option<String>,
    global: bool,
    configured_key: Option<String>,
) -> Result<()> {
    let runtime_override = if global && req.backend.starts_with("go:") {
        Some(global_go_dependency_request(app)?)
    } else {
        None
    };
    let mut install_input = vec![req.clone()];
    if let Some(runtime) = runtime_override {
        install_input.push(runtime);
    }
    let installed = install_requests(app, install_input, Vec::new(), false, false).await?;
    let tv = installed
        .iter()
        .find_map(|(request, version)| (request.backend == req.backend).then_some(version.clone()))
        .ok_or_else(|| anyhow!("requested tool was not installed"))?;
    let persisted_options = if req.backend.starts_with("go:") {
        osdk_core::backend::dynamic::identity_options(&req.backend, &tv.options)?
    } else {
        req.options.clone()
    };
    // Pin the exact spec string the user typed (verbatim after `@`), so
    // channels like `stable` or `temurin-17` are preserved rather than being
    // normalized to `latest`. Bare `tool` (no `@`) pins the resolved version.
    let spec = requested_spec.unwrap_or_else(|| tv.version.clone());
    let persist_target = select_use_persist_target(app, &tv.backend, global, configured_key)?;
    let persisted_version = persist_version_for_target(&persist_target, &tv.backend, &spec);
    if global {
        if persisted_options.is_empty() {
            crate::config_edit::set_global_tool(&app.ctx, &persist_target.key, &persisted_version)?;
        } else {
            crate::config_edit::set_global_tool_config(
                &app.ctx,
                &persist_target.key,
                &structured_tool_config(&persisted_version, &persisted_options),
            )?;
        }
        println!(
            "{}",
            t!("msg.pinned_global", tool = persist_target.key, ver = spec)
        );
    } else {
        let path = if persisted_options.is_empty() {
            crate::config_edit::set_project_tool(&persist_target.key, &persisted_version)?
        } else {
            crate::config_edit::set_project_tool_config(
                &persist_target.key,
                &structured_tool_config(&persisted_version, &persisted_options),
            )?
        };
        println!(
            "{}",
            t!(
                "msg.pinned_project",
                tool = persist_target.key,
                ver = spec,
                path = path.display()
            )
        );
    }
    if req.backend.starts_with("go:") {
        let lock_path = if global {
            app.ctx.dirs.user_lock_file()
        } else {
            project_lock_path(app, &std::env::current_dir()?)
        };
        crate::lockfile::upsert_resolved_many_with_scope(
            &lock_path,
            app.ctx.platform,
            &app.ctx.dirs,
            &installed,
            if global {
                crate::lockfile::LockScope::Global
            } else {
                crate::lockfile::LockScope::Project
            },
        )?;
    }
    Ok(())
}

fn global_go_dependency_request(app: &App) -> Result<ToolRequest> {
    let spec = app
        .ctx
        .config
        .global_tools
        .get("go")
        .ok_or_else(|| {
            anyhow!(
                "global Go tools require a global managed Go selection; configure `go = \"1.24\"` globally"
            )
        })?;
    Ok(ToolRequest {
        backend: "go".into(),
        spec: VersionSpec::parse(spec),
        options: app
            .ctx
            .config
            .global_tool_configs
            .get("go")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

#[derive(Debug)]
struct UsePersistTarget {
    key: String,
    indirect: bool,
}

fn select_use_persist_target(
    app: &App,
    backend: &str,
    global: bool,
    configured_key: Option<String>,
) -> Result<UsePersistTarget> {
    if let Some(key) = configured_key {
        return Ok(UsePersistTarget {
            indirect: key != backend,
            key,
        });
    }

    let tools = if global {
        &app.ctx.config.global_tools
    } else {
        &app.ctx.config.tools
    };
    let matches = tools
        .iter()
        .filter_map(|(key, value)| {
            (key != backend)
                .then(|| ToolRequest::parse(value).ok())
                .flatten()
                .filter(|request| request.backend == backend)
                .map(|_| key.clone())
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(UsePersistTarget {
            key: backend.to_string(),
            indirect: false,
        }),
        [key] => Ok(UsePersistTarget {
            key: key.clone(),
            indirect: true,
        }),
        _ => anyhow::bail!(
            "multiple configured tool keys resolve to `{backend}`; use the desired key explicitly"
        ),
    }
}

fn persist_version_for_target(target: &UsePersistTarget, backend: &str, spec: &str) -> String {
    if target.indirect {
        format!("{backend}@{spec}")
    } else {
        spec.to_string()
    }
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
    requested_installer: osdk_core::npm_tools::NpmInstaller,
    configured_key: Option<String>,
) -> Result<()> {
    let package = request
        .backend
        .strip_prefix("npm:")
        .ok_or_else(|| anyhow!("project npm install requires an npm: package request"))?
        .to_string();
    let node_request = project_node_request(app, &project.root)?;
    let (node_backend, node_version) = install_one_without_shims(app, &node_request, false).await?;
    generate_shims_including_dependencies(app, node_backend.as_ref(), &node_version)?;
    let node_bin_dir = managed_bin_paths(&app.ctx, node_backend.as_ref(), &node_version, None)?
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
    bind_dynamic_request_options(&effective, &mut version);
    let package_spec = project_package_spec(&package, requested_spec.as_deref(), &version.version);
    // Re-open and re-plan after taking the project lock below. Another osdk
    // process may have changed the manifest or incumbent native lock while
    // Node/package resolution was running.
    let config_path = project_config_path(app, &project.root);
    let metadata_rollback = ProjectNpmMetadataRollback::begin(
        &app.ctx.dirs,
        &project.root,
        &project.package_json,
        &config_path,
    )?;
    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    let configured_spec = persisted_spec.clone();
    let config_key = configured_key.as_deref().unwrap_or(&request.backend);
    let configured_specs = project_npm_configured_specs(
        app,
        &config_path,
        Some((config_key, &request.backend, &configured_spec)),
    )?;
    let project = osdk_core::npm_tools::inspect_npm_project(&project.root)?
        .ok_or_else(|| anyhow!("project package.json disappeared before npm install"))?;
    let installer = osdk_core::npm_tools::plan_npm_installer(
        &project.root,
        requested_installer,
        osdk_core::npm_tools::ToolScope::Project,
        app.ctx.config.settings.npm.default_installer,
    )?
    .installer;
    let section = project_dependency_section(&project.package_json, &package)?;
    let expected_native_lock = expected_project_native_lock(&project, installer);

    let result: Result<(String, std::path::PathBuf)> = async {
        match installer {
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
            .validate_project_package_bins(&project.root, &version.version)?;
        let installed_project = osdk_core::npm_tools::inspect_npm_project(&project.root)?
            .ok_or_else(|| anyhow!("project package.json disappeared during npm install"))?;
        let native_lock = installed_project.native_lock.ok_or_else(|| {
            anyhow!(
                "installer `{installer}` did not write a recognized native lockfile in {}",
                project.root.display()
            )
        })?;
        validate_installed_project_native_lock(installer, &expected_native_lock, &native_lock)?;
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

        osdk_core::backend::npm_package::publish_project_bin_generation(
            &project.root,
            &osdk_core::backend::npm_package::ProjectNpmBinSelection {
                backend: request.backend.clone(),
                configured_spec: configured_spec.clone(),
                version: version.version.clone(),
            },
            &configured_specs,
        )?;
        let mut config_options = request.options.clone();
        config_options.insert("installer".into(), installer.as_str().into());
        let config_version = configured_key
            .as_ref()
            .map(|_| format!("{}@{}", request.backend, persisted_spec))
            .unwrap_or_else(|| persisted_spec.clone());
        crate::config_edit::set_project_npm_tool_at(
            &config_path,
            &node_version.version,
            config_key,
            &structured_tool_config(&config_version, &config_options),
        )?;
        osdk_core::trust::trust(&app.ctx.dirs.config, &config_path)?;
        Ok((persisted_spec.clone(), config_path.clone()))
    }
    .await;

    let (persisted_spec, config_path) = match result {
        Ok(result) => result,
        Err(error) => return Err(metadata_rollback.rollback(error)),
    };
    metadata_rollback.commit();
    println!(
        "{}",
        t!(
            "msg.pinned_project",
            tool = configured_key.as_deref().unwrap_or(&request.backend),
            ver = persisted_spec,
            path = config_path.display()
        )
    );
    Ok(())
}

fn project_npm_configured_specs(
    app: &App,
    config_path: &std::path::Path,
    replacement: Option<(&str, &str, &str)>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let project_root = config_path
        .parent()
        .ok_or_else(|| anyhow!("project config has no parent: {}", config_path.display()))?;
    let config = osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), project_root)?;
    let mut entries = Vec::new();
    for (key, value) in &config.tools {
        if !matches!(
            config.tool_origins.get(key),
            Some(osdk_core::config::ToolConfigOrigin::ProjectConfig(path))
                if same_config_path(path, config_path)
        ) {
            continue;
        }
        if replacement.is_some_and(|(replacement_key, _, _)| key == replacement_key) {
            continue;
        }
        entries.push((key.clone(), value.clone()));
    }
    if let Some((key, backend, spec)) = replacement {
        let value = if key == backend {
            spec.to_string()
        } else {
            format!("{backend}@{spec}")
        };
        entries.push((key.to_string(), value));
    }
    osdk_core::activate::canonical_project_npm_specs(entries).map_err(anyhow::Error::from)
}

fn expected_project_native_lock(
    project: &osdk_core::npm_tools::NpmProject,
    installer: osdk_core::npm_tools::NpmInstaller,
) -> (osdk_core::npm_tools::NativeLockKind, std::path::PathBuf) {
    if let Some(lock) = &project.native_lock {
        return (lock.kind, lock.path.clone());
    }
    let kind = match installer {
        osdk_core::npm_tools::NpmInstaller::Npm => {
            osdk_core::npm_tools::NativeLockKind::PackageLock
        }
        osdk_core::npm_tools::NpmInstaller::Pnpm => osdk_core::npm_tools::NativeLockKind::Pnpm,
        osdk_core::npm_tools::NpmInstaller::Auto => {
            unreachable!("npm installer planning always returns a concrete installer")
        }
    };
    (kind, project.root.join(kind.file_name()))
}

fn validate_installed_project_native_lock(
    installer: osdk_core::npm_tools::NpmInstaller,
    expected: &(osdk_core::npm_tools::NativeLockKind, std::path::PathBuf),
    actual: &osdk_core::npm_tools::NativeLock,
) -> Result<()> {
    if actual.kind != expected.0 || actual.path != expected.1 {
        anyhow::bail!(
            "installer `{installer}` changed native lock identity: expected {}, got {}",
            expected.1.display(),
            actual.path.display()
        );
    }
    Ok(())
}

/// Roll back user-authored manifests and osdk publication metadata. Package
/// manager materialization is intentionally outside this bounded snapshot;
/// activation never exposes it unless a validated curated generation commits.
struct ProjectNpmMetadataRollback {
    _lock: osdk_core::lock::FileLock,
    snapshots: Vec<ProjectFileSnapshot>,
}

struct ProjectFileSnapshot {
    path: std::path::PathBuf,
    bytes: Option<Vec<u8>>,
}

impl ProjectNpmMetadataRollback {
    fn begin(
        dirs: &osdk_core::dirs::Dirs,
        project_root: &std::path::Path,
        package_json: &std::path::Path,
        config_path: &std::path::Path,
    ) -> Result<Self> {
        let canonical_root = dunce::canonicalize(project_root)
            .with_context(|| format!("canonicalizing {}", project_root.display()))?;
        let project_key = osdk_core::pipeline::verify::hash_bytes(
            canonical_root.to_string_lossy().as_bytes(),
            osdk_core::pipeline::HashAlgo::Sha256,
        );
        let lock = osdk_core::lock::FileLock::acquire(
            dirs.data
                .join("locks/project-npm")
                .join(format!("{project_key}.lock")),
        )?;
        let mut paths = vec![
            package_json.to_path_buf(),
            project_root.join("pnpm-lock.yaml"),
            project_root.join("package-lock.json"),
            project_root.join("npm-shrinkwrap.json"),
            project_root.join(crate::lockfile::LOCKFILE_NAME),
            config_path.to_path_buf(),
            osdk_core::backend::npm_package::project_bin_current_path(project_root),
        ];
        paths.sort();
        paths.dedup();
        let snapshots = paths
            .into_iter()
            .map(ProjectFileSnapshot::capture)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            _lock: lock,
            snapshots,
        })
    }

    fn rollback(&self, error: anyhow::Error) -> anyhow::Error {
        let failures = self
            .snapshots
            .iter()
            .rev()
            .filter_map(|snapshot| snapshot.restore().err())
            .map(|failure| failure.to_string())
            .collect::<Vec<_>>();
        if failures.is_empty() {
            error
        } else {
            error.context(format!("project rollback failed: {}", failures.join("; ")))
        }
    }

    fn commit(self) {}
}

impl ProjectFileSnapshot {
    fn capture(path: std::path::PathBuf) -> Result<Self> {
        let bytes = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Some(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
            }
            Ok(_) => anyhow::bail!(
                "project transaction path must be a regular file: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()))
            }
        };
        Ok(Self { path, bytes })
    }

    fn restore(&self) -> Result<()> {
        match &self.bytes {
            Some(bytes) => atomic_restore_project_file(&self.path, bytes),
            None => match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(&self.path)
                    .with_context(|| format!("removing {} during rollback", self.path.display())),
                Ok(_) => anyhow::bail!(
                    "refusing to remove non-file rollback target {}",
                    self.path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error)
                    .with_context(|| format!("inspecting {} during rollback", self.path.display())),
            },
        }
    }
}

fn atomic_restore_project_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("rollback path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating rollback directory {}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("rollback path has no file name: {}", path.display()))?
        .to_string_lossy();
    let temporary = (0..1024u32)
        .map(|attempt| {
            parent.join(format!(
                ".{name}.osdk-rollback-{}-{attempt}",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| anyhow!("could not allocate rollback path for {}", path.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating rollback file {}", temporary.display()))?;
    use std::io::Write as _;
    file.write_all(bytes)
        .with_context(|| format!("writing rollback file {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing rollback file {}", temporary.display()))?;
    drop(file);
    if let Err(error) = replace_project_file(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
fn snapshot_project_file(path: &std::path::Path) -> Result<ProjectFileSnapshot> {
    ProjectFileSnapshot::capture(path.to_path_buf())
}

#[cfg(not(windows))]
fn replace_project_file(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| format!("restoring {}", destination.display()))
}

#[cfg(windows)]
fn replace_project_file(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;
    let source = source
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
            source.as_ptr(),
            destination_wide.as_ptr(),
            REPLACE_EXISTING | WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("restoring {}", destination.display()));
    }
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
    Ok(if contains("optionalDependencies") {
        ProjectDependencySection::OptionalDependencies
    } else if contains("dependencies") {
        ProjectDependencySection::Dependencies
    } else if contains("peerDependencies") {
        ProjectDependencySection::PeerDependencies {
            also_dev: contains("devDependencies"),
        }
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
        install_one_without_shims(app, &manager_request, false).await?;
    generate_shims_including_dependencies(app, manager_backend.as_ref(), &manager_version)?;
    let manager = find_managed_executable(
        &managed_bin_paths(&app.ctx, manager_backend.as_ref(), &manager_version, None)?,
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

pub async fn uninstall(app: &App, tool: String, global: bool) -> Result<()> {
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
        anyhow::bail!("--global is only supported for npm:<package> uninstall requests");
    }
    if global {
        crate::global_npm_use::with_global_npm_state_lock(&app.ctx.dirs, || {
            recover_interrupted_global_npm_uninstalls(app)
        })?;
    }
    let npm_backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&req.backend);
    let selected_npm = if global {
        Some(select_global_npm_version(
            app,
            &req,
            requested_spec_literal(&tool).is_some(),
        )?)
    } else if let Some(npm) = npm_backend.as_ref() {
        let hint = npm_scope_hint(&req, false);
        let installed = npm.list_installed_identities_for(&app.ctx, &hint)?;
        let spec = match &req.spec {
            VersionSpec::Exact(_) | VersionSpec::Prefix(_) => &req.spec,
            other => return Err(anyhow!(t!("err.specify_exact", spec = other))),
        };
        Some(select_installed_npm_identity(
            &req.backend,
            spec,
            installed,
        )?)
    } else {
        None
    };
    let version = if let Some(selected) = selected_npm.as_ref() {
        selected.version.clone()
    } else {
        match &req.spec {
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
        }
    };
    let mut tv = selected_npm.unwrap_or_else(|| ToolVersion::new(&req.backend, &version));
    if tv.options.is_empty() {
        tv.options = req.options.clone();
    }
    let question = t!("prompt.uninstall", tool = tv);
    if !app.prompt.confirm(&question)? {
        println!("{}", t!("msg.cancelled"));
        return Ok(());
    }
    if global {
        uninstall_global_npm(app, &tv)?;
    } else {
        backend.uninstall(&app.ctx, &tv).await?;
        reconcile_managed_shims(app)?;
    }
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

fn npm_scope_hint(request: &ToolRequest, global: bool) -> ToolVersion {
    let mut hint = ToolVersion::new(&request.backend, "scope-selection");
    hint.options = request.options.clone();
    if global {
        hint.options.insert(
            osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
            osdk_core::npm_tools::ToolScope::Global.as_str().into(),
        );
    }
    hint
}

fn select_global_npm_version(
    app: &App,
    request: &ToolRequest,
    explicit_spec: bool,
) -> Result<ToolVersion> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    let hint = npm_scope_hint(request, true);
    let installed = backend.list_installed_identities_for(&app.ctx, &hint)?;
    let spec = global_npm_selection_spec(app, request, explicit_spec)?;
    let selected_version = select_installed_version(
        &request.backend,
        &spec,
        installed
            .iter()
            .map(|candidate| candidate.version.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    )?;
    if !explicit_spec {
        let lock_path = app.ctx.dirs.user_lock_file();
        if lock_path.is_file() {
            if let Some(locked) = crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
                .into_iter()
                .flatten()
                .find(|locked| {
                    locked.backend == request.backend
                        && locked.spec == VersionSpec::Exact(selected_version.clone())
                        && locked
                            .options
                            .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                            .map(String::as_str)
                            == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
                })
            {
                let mut version = ToolVersion::new(&request.backend, &selected_version);
                version.options = locked.options;
                if backend
                    .where_install_root_for(&app.ctx, &version)?
                    .is_some()
                {
                    return Ok(version);
                }
            }
        }
    }
    let mut version = select_installed_npm_identity(
        &request.backend,
        &VersionSpec::Exact(selected_version),
        installed,
    )?;
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
        osdk_core::npm_tools::ToolScope::Global.as_str().into(),
    );
    Ok(version)
}

fn global_npm_selection_spec(
    app: &App,
    request: &ToolRequest,
    explicit_spec: bool,
) -> Result<VersionSpec> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let spec = if explicit_spec {
        request.spec.clone()
    } else {
        let configured = config
            .global_tool_configs
            .get(&request.backend)
            .map(|entry| VersionSpec::parse(entry.version()));
        match configured {
            Some(VersionSpec::Exact(version)) => VersionSpec::Exact(version),
            Some(spec) => {
                let lock_path = app.ctx.dirs.user_lock_file();
                if lock_path.is_file() {
                    if let Some(locked) =
                        crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
                            .unwrap_or_default()
                            .into_iter()
                            .find(|locked| {
                                locked.backend == request.backend
                                    && locked
                                        .options
                                        .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                                        .map(String::as_str)
                                        == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
                            })
                    {
                        locked.spec
                    } else {
                        spec
                    }
                } else {
                    spec
                }
            }
            None => request.spec.clone(),
        }
    };
    let expanded = config.expand_alias(&request.backend, &spec.to_string())?;
    Ok(VersionSpec::parse(&expanded))
}

fn select_installed_version(
    backend: &str,
    spec: &VersionSpec,
    installed: Vec<String>,
) -> Result<String> {
    if let VersionSpec::Exact(version) = spec {
        if installed.iter().any(|candidate| candidate == version) {
            return Ok(version.clone());
        }
        anyhow::bail!("{backend}@{version} is not installed");
    }
    let infos = installed
        .iter()
        .map(osdk_core::version::VersionInfo::stable)
        .collect::<Vec<_>>();
    osdk_core::version::select_version(spec, &infos)
        .map(|version| version.version.clone())
        .ok_or_else(|| anyhow!("{backend} is not installed"))
}

fn select_installed_npm_identity(
    backend: &str,
    spec: &VersionSpec,
    installed: Vec<ToolVersion>,
) -> Result<ToolVersion> {
    let versions = installed
        .iter()
        .map(|candidate| candidate.version.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let selected = select_installed_version(backend, spec, versions)?;
    let mut matches = installed
        .into_iter()
        .filter(|candidate| candidate.version == selected);
    let candidate = matches
        .next()
        .ok_or_else(|| anyhow!("{backend}@{selected} is not installed"))?;
    if matches.next().is_some() {
        anyhow::bail!(
            "{backend}@{selected} has multiple installed identities; use a lockfile-backed selection or remove an obsolete variant"
        );
    }
    Ok(candidate)
}

fn uninstall_global_npm(app: &App, version: &ToolVersion) -> Result<()> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", version.backend))?;
    crate::global_npm_use::with_global_npm_state_lock(&app.ctx.dirs, || {
        recover_interrupted_global_npm_uninstalls(app)?;
        let roots = global_npm_install_roots(&app.ctx, &backend, version)?;
        if roots.is_empty() {
            anyhow::bail!("{} is not installed in global scope", version);
        }
        let bin_names = roots
            .iter()
            .map(|root| osdk_core::inventory::DynamicToolManifest::load(root))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(|manifest| manifest.bins.into_iter().map(|bin| bin.name))
            .collect::<std::collections::BTreeSet<_>>();
        let installed_before = backend.list_installed_for(
            &app.ctx,
            &npm_scope_hint(&exact_request_for_version(version), true),
        )?;
        let remove_config = global_config_selects_version(app, version, &installed_before)?;
        let remove_lock = global_lock_selects_version(app, version)?;
        let snapshots = global_npm_metadata_snapshots(app, &bin_names)?;
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            app,
            version,
            &roots,
            &bin_names,
            remove_config,
            remove_lock,
        )?;
        for index in 0..transaction.journal.roots.len() {
            let original = transaction.journal.roots[index].original.clone();
            let backup = transaction.journal.roots[index].backup.clone();
            if let Err(error) = rename_global_npm_uninstall_path(&original, &backup) {
                let rollback = transaction.rollback_roots();
                return Err(with_uninstall_rollback(
                    anyhow!(error).context(format!("staging removal of {}", original.display())),
                    rollback.err(),
                ));
            }
        }
        // Every root is now recoverable from a durable backup. Crossing this
        // write-ahead commit makes a crash complete the uninstall; normal
        // returned errors still use the in-process snapshots to roll back.
        transaction.mark_committed()?;
        let metadata_result = (|| {
            if remove_config {
                crate::config_edit::remove_global_tool_unlocked(&app.ctx, &version.backend)?;
            }
            if remove_lock {
                crate::lockfile::remove_tool(
                    &app.ctx.dirs.user_lock_file(),
                    app.ctx.platform,
                    &version.backend,
                )?;
            }
            remove_unowned_global_npm_shims(app, &bin_names)
        })();
        if let Err(error) = metadata_result {
            let metadata_rollback = restore_global_npm_metadata(&snapshots);
            let install_rollback = transaction.rollback_roots();
            let shim_rollback = refreshed_global_npm_app(app)
                .and_then(|app| generate_global_npm_shims_for(&app, version, &bin_names));
            let rollback = combine_rollback_errors(
                combine_rollback_errors(metadata_rollback.err(), install_rollback.err()),
                shim_rollback.err(),
            );
            return Err(with_uninstall_rollback(error, rollback));
        }
        transaction.finish();
        Ok(())
    })
}

fn global_npm_install_roots(
    ctx: &osdk_core::backend::Ctx,
    backend: &osdk_core::backend::npm_package::NpmPackageBackend,
    version: &ToolVersion,
) -> Result<Vec<std::path::PathBuf>> {
    let mut roots = Vec::new();
    if let Some(root) = backend.existing_global_install_root(ctx, version)? {
        roots.push(root);
    }
    if let Some(legacy) = backend.legacy_global_install_root(ctx, version)? {
        if !roots.contains(&legacy) {
            roots.push(legacy);
        }
    }
    Ok(roots)
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct GlobalNpmUninstallJournal {
    backend: String,
    version: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, String>,
    roots: Vec<GlobalNpmUninstallRoot>,
    bin_names: Vec<String>,
    config_entry: Option<osdk_core::config::ToolConfigEntry>,
    lock_entry: Option<GlobalNpmUninstallLockEntry>,
    committed: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct GlobalNpmUninstallRoot {
    original: std::path::PathBuf,
    backup: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct GlobalNpmUninstallLockEntry {
    version: String,
    options: std::collections::BTreeMap<String, String>,
}

struct GlobalNpmUninstallTransaction {
    path: std::path::PathBuf,
    journal: GlobalNpmUninstallJournal,
    completed: bool,
}

impl GlobalNpmUninstallTransaction {
    fn prepare(
        app: &App,
        version: &ToolVersion,
        roots: &[std::path::PathBuf],
        bin_names: &std::collections::BTreeSet<String>,
        remove_config: bool,
        remove_lock: bool,
    ) -> Result<Self> {
        let roots = roots
            .iter()
            .map(|original| {
                Ok(GlobalNpmUninstallRoot {
                    original: original.clone(),
                    backup: unused_sibling_path(original, "uninstall-backup")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let path = global_npm_uninstall_journal_path(&app.ctx.dirs, version);
        if path.exists() {
            anyhow::bail!(
                "global npm uninstall journal already exists for {}",
                version
            );
        }
        let transaction = Self {
            path,
            journal: GlobalNpmUninstallJournal {
                backend: version.backend.clone(),
                version: version.version.clone(),
                options: version.options.clone(),
                roots,
                bin_names: bin_names.iter().cloned().collect(),
                config_entry: remove_config
                    .then(|| capture_global_npm_uninstall_config_entry(app, version))
                    .transpose()?
                    .flatten(),
                lock_entry: remove_lock
                    .then(|| capture_global_npm_uninstall_lock_entry(app, version))
                    .transpose()?
                    .flatten(),
                committed: false,
            },
            completed: false,
        };
        transaction.persist()?;
        Ok(transaction)
    }

    fn persist(&self) -> Result<()> {
        write_global_npm_uninstall_journal(&self.path, &self.journal)
    }

    fn mark_committed(&mut self) -> Result<()> {
        let mut committed = self.journal.clone();
        committed.committed = true;
        write_global_npm_uninstall_journal(&self.path, &committed)?;
        self.journal = committed;
        Ok(())
    }

    fn rollback_roots(&mut self) -> Result<()> {
        let result = restore_moved_global_npm_roots(&self.journal.roots);
        if result.is_ok() {
            remove_global_npm_uninstall_path(&self.path)?;
            self.completed = true;
        }
        result
    }

    fn finish(&mut self) {
        let mut cleanup_failed = false;
        for root in &self.journal.roots {
            if let Err(error) = remove_global_npm_uninstall_path(&root.backup) {
                cleanup_failed = true;
                tracing::warn!(
                    error = %error,
                    path = %root.backup.display(),
                    "failed to remove committed global npm uninstall backup"
                );
            }
        }
        if !cleanup_failed {
            if let Err(error) = remove_global_npm_uninstall_path(&self.path) {
                tracing::warn!(
                    error = %error,
                    path = %self.path.display(),
                    "failed to remove committed global npm uninstall journal"
                );
            } else {
                self.completed = true;
            }
        }
    }
}

impl Drop for GlobalNpmUninstallTransaction {
    fn drop(&mut self) {
        // A normal error before the commit marker restores availability. A
        // committed transaction is deliberately left for roll-forward unless
        // the caller explicitly rolls it back with its metadata snapshots.
        // Panics model abrupt termination and likewise leave the journal for
        // deterministic recovery on the next locked operation.
        if !self.completed && !self.journal.committed && !std::thread::panicking() {
            let _ = self.rollback_roots();
        }
    }
}

fn global_npm_uninstall_journal_dir(dirs: &osdk_core::dirs::Dirs) -> std::path::PathBuf {
    dirs.data.join(GLOBAL_NPM_UNINSTALL_JOURNAL_DIR)
}

fn global_npm_uninstall_journal_path(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
) -> std::path::PathBuf {
    let backend = osdk_core::pipeline::verify::hash_bytes(
        version.backend.as_bytes(),
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    let version = osdk_core::pipeline::verify::hash_bytes(
        version.version.as_bytes(),
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    global_npm_uninstall_journal_dir(dirs).join(format!("{backend}-{version}.json"))
}

fn capture_global_npm_uninstall_config_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<osdk_core::config::ToolConfigEntry>> {
    Ok(
        osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?
            .global_tool_configs
            .get(&version.backend)
            .cloned(),
    )
}

fn capture_global_npm_uninstall_lock_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<GlobalNpmUninstallLockEntry>> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(None);
    }
    Ok(crate::lockfile::locked_requests(&path, app.ctx.platform)?
        .unwrap_or_default()
        .into_iter()
        .find(|request| {
            request.backend == version.backend
                && request.spec == VersionSpec::Exact(version.version.clone())
                && request
                    .options
                    .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                    .map(String::as_str)
                    == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
        })
        .map(|request| GlobalNpmUninstallLockEntry {
            version: version.version.clone(),
            options: request.options,
        }))
}

fn write_global_npm_uninstall_journal(
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("uninstall journal has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating uninstall journal directory {}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("uninstall journal has no file name: {}", path.display()))?
        .to_string_lossy();
    let temporary = loop {
        let nonce = NEXT_GLOBAL_NPM_UNINSTALL_FILE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.write-{}-{nonce}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "creating global npm uninstall journal {}",
                        candidate.display()
                    )
                })
            }
        }
    };
    let (temporary_path, mut temporary_file) = temporary;
    let result = (|| -> Result<()> {
        use std::io::Write as _;
        temporary_file.write_all(&serde_json::to_vec(journal)?)?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        replace_project_file(&temporary_path, path)?;
        sync_global_npm_uninstall_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result.with_context(|| format!("writing global npm uninstall journal {}", path.display()))
}

#[cfg(unix)]
fn sync_global_npm_uninstall_directory(path: &std::path::Path) -> Result<()> {
    std::fs::File::open(path)?
        .sync_all()
        .with_context(|| format!("syncing uninstall journal directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_global_npm_uninstall_directory(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn read_global_npm_uninstall_journal(path: &std::path::Path) -> Result<GlobalNpmUninstallJournal> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading global npm uninstall journal {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing global npm uninstall journal {}", path.display()))
}

/// Complete or roll back global npm uninstalls left by a terminated process.
/// Callers must hold the shared global npm state lock.
pub(crate) fn recover_interrupted_global_npm_uninstalls(app: &App) -> Result<()> {
    let directory = global_npm_uninstall_journal_dir(&app.ctx.dirs);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading global npm uninstall journals {}",
                    directory.display()
                )
            })
        }
    };
    let mut journals = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    journals.sort();
    for path in journals {
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        recover_global_npm_uninstall(app, &path)?;
    }
    Ok(())
}

fn recover_global_npm_uninstall(app: &App, path: &std::path::Path) -> Result<()> {
    let journal = read_global_npm_uninstall_journal(path)?;
    validate_global_npm_uninstall_journal(app, path, &journal)?;
    let mut version = ToolVersion::new(&journal.backend, &journal.version);
    version.options = journal.options.clone();
    let bin_names = journal.bin_names.iter().cloned().collect();
    if journal.committed {
        finish_recovered_global_npm_uninstall(app, path, &journal, &version, &bin_names)
    } else {
        rollback_recovered_global_npm_uninstall(app, path, &journal, &version, &bin_names)
    }
}

fn validate_global_npm_uninstall_journal(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
) -> Result<()> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&journal.backend)
        .filter(|backend| backend.id() == journal.backend)
        .ok_or_else(|| {
            anyhow!(
                "invalid backend `{}` in global npm uninstall journal {}",
                journal.backend,
                path.display()
            )
        })?;
    if journal.version.is_empty() || journal.roots.is_empty() {
        anyhow::bail!("incomplete global npm uninstall journal {}", path.display());
    }
    let mut version = ToolVersion::new(&journal.backend, &journal.version);
    version.options = journal.options.clone();
    let expected_path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);
    if path != expected_path {
        anyhow::bail!(
            "unsafe global npm uninstall journal path {} (expected {})",
            path.display(),
            expected_path.display()
        );
    }
    let expected_roots = [
        backend.global_install_root_for(&app.ctx, &version)?,
        backend.legacy_global_install_root_path(&app.ctx, &journal.version),
        backend.legacy_isolated_install_root(&app.ctx, &journal.version),
    ];
    let mut originals = std::collections::BTreeSet::new();
    let mut backups = std::collections::BTreeSet::new();
    for root in &journal.roots {
        if !expected_roots.contains(&root.original) || !originals.insert(root.original.clone()) {
            anyhow::bail!(
                "unsafe original path {} in global npm uninstall journal {}",
                root.original.display(),
                path.display()
            );
        }
        let parent = root
            .original
            .parent()
            .ok_or_else(|| anyhow!("global npm uninstall root has no parent"))?;
        let name = root
            .original
            .file_name()
            .ok_or_else(|| anyhow!("global npm uninstall root has no file name"))?
            .to_string_lossy();
        let prefix = format!(".{name}.osdk-uninstall-backup-");
        if root.backup.parent() != Some(parent)
            || !root
                .backup
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            || !backups.insert(root.backup.clone())
        {
            anyhow::bail!(
                "unsafe backup path {} in global npm uninstall journal {}",
                root.backup.display(),
                path.display()
            );
        }
    }
    Ok(())
}

fn finish_recovered_global_npm_uninstall(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let config_matches = match &journal.config_entry {
        Some(expected) => {
            global_npm_uninstall_config_entry(app, version)?.as_ref() == Some(expected)
        }
        None => false,
    };
    if config_matches {
        crate::config_edit::remove_global_tool_unlocked(&app.ctx, &version.backend)?;
    }
    let config_is_safe = match &journal.config_entry {
        Some(expected) => {
            global_npm_uninstall_config_entry(app, version)?.as_ref() != Some(expected)
        }
        None => true,
    };
    let lock_matches = match &journal.lock_entry {
        Some(expected) => global_npm_uninstall_lock_entry(app, version)?.as_ref() == Some(expected),
        None => false,
    };
    if lock_matches {
        crate::lockfile::remove_tool(
            &app.ctx.dirs.user_lock_file(),
            app.ctx.platform,
            &version.backend,
        )?;
    }
    let lock_is_safe = match &journal.lock_entry {
        Some(expected) => global_npm_uninstall_lock_entry(app, version)?.as_ref() != Some(expected),
        None => true,
    };
    if !config_is_safe || !lock_is_safe {
        anyhow::bail!(
            "cannot finish interrupted global npm uninstall for {}: active metadata still matches the removed install",
            version
        );
    }
    remove_unowned_global_npm_shims(app, bin_names)?;
    for root in &journal.roots {
        remove_global_npm_uninstall_path(&root.backup)?;
    }
    remove_global_npm_uninstall_path(path)
}

fn rollback_recovered_global_npm_uninstall(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    restore_moved_global_npm_roots(&journal.roots)?;
    if global_config_still_selects_version(app, version)? {
        let refreshed = refreshed_global_npm_app(app)?;
        generate_global_npm_shims_for(&refreshed, version, bin_names)?;
    }
    remove_global_npm_uninstall_path(path)
}

fn global_config_still_selects_version(app: &App, version: &ToolVersion) -> Result<bool> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let Some(entry) = config.global_tool_configs.get(&version.backend) else {
        return Ok(false);
    };
    let spec = VersionSpec::parse(&config.expand_alias(&version.backend, entry.version())?);
    match spec {
        VersionSpec::Exact(selected) => Ok(selected == version.version),
        _ => Ok(global_lock_selects_version(app, version)?),
    }
}

fn global_npm_uninstall_config_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<osdk_core::config::ToolConfigEntry>> {
    capture_global_npm_uninstall_config_entry(app, version)
}

fn global_npm_uninstall_lock_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<GlobalNpmUninstallLockEntry>> {
    capture_global_npm_uninstall_lock_entry(app, version)
}

fn refreshed_global_npm_app(app: &App) -> Result<App> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    Ok(App {
        ctx,
        registry: osdk_core::Registry::load(&app.ctx.dirs)?,
        prompt: app.prompt.clone(),
        source_override: app.source_override.clone(),
        refresh_sources: app.refresh_sources,
    })
}

fn generate_global_npm_shims_for(
    app: &App,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if global_config_still_selects_version(app, version)? {
        if let Some(shim_binary) = osdk_core::shim::find_shim_binary(&app.ctx.dirs) {
            for name in bin_names {
                osdk_core::shim::generate_shim(&app.ctx.dirs, name, &shim_binary)?;
            }
        }
    }
    Ok(())
}

fn remove_global_npm_uninstall_path(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn rename_global_npm_uninstall_path(
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..4 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
    }
    Err(last.expect("rename was attempted"))
}

fn global_config_selects_version(
    app: &App,
    version: &ToolVersion,
    installed: &[String],
) -> Result<bool> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let Some(entry) = config.global_tool_configs.get(&version.backend) else {
        return Ok(false);
    };
    let spec = VersionSpec::parse(&config.expand_alias(&version.backend, entry.version())?);
    if let VersionSpec::Exact(selected) = spec {
        return Ok(selected == version.version);
    }
    let lock_path = app.ctx.dirs.user_lock_file();
    if lock_path.is_file() {
        if let Some(selected) = crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
            .unwrap_or_default()
            .into_iter()
            .find(|request| {
                request.backend == version.backend
                    && request
                        .options
                        .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                        .map(String::as_str)
                        == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
            })
        {
            return Ok(selected.spec == VersionSpec::Exact(version.version.clone()));
        }
    }
    Ok(
        select_installed_version(&version.backend, &spec, installed.to_vec())
            .is_ok_and(|selected| selected == version.version),
    )
}

fn global_lock_selects_version(app: &App, version: &ToolVersion) -> Result<bool> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(false);
    }
    Ok(crate::lockfile::locked_requests(&path, app.ctx.platform)?
        .unwrap_or_default()
        .into_iter()
        .any(|request| {
            request.backend == version.backend
                && request.spec == VersionSpec::Exact(version.version.clone())
                && request
                    .options
                    .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                    .map(String::as_str)
                    == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
        }))
}

fn remove_unowned_global_npm_shims(
    app: &App,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    let refreshed = App {
        ctx,
        registry: osdk_core::Registry::load(&app.ctx.dirs)?,
        prompt: app.prompt.clone(),
        source_override: app.source_override.clone(),
        refresh_sources: app.refresh_sources,
    };
    let owners = installed_shim_owners(&refreshed)?;
    for name in bin_names {
        if !owners
            .get(name)
            .is_some_and(|owner_ids| !owner_ids.is_empty())
        {
            osdk_core::shim::remove_managed_shim(&app.ctx.dirs, name)?;
        }
    }
    Ok(())
}

fn global_npm_metadata_snapshots(
    app: &App,
    _bin_names: &std::collections::BTreeSet<String>,
) -> Result<Vec<GlobalNpmPathSnapshot>> {
    let paths = vec![
        app.ctx.dirs.user_config_file(),
        app.ctx.dirs.user_lock_file(),
    ];
    paths
        .into_iter()
        .map(GlobalNpmPathSnapshot::capture)
        .collect()
}

fn restore_global_npm_metadata(snapshots: &[GlobalNpmPathSnapshot]) -> Result<()> {
    let failures = snapshots
        .iter()
        .rev()
        .filter_map(|snapshot| snapshot.restore().err())
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", failures.join("; "))
    }
}

struct GlobalNpmPathSnapshot {
    path: std::path::PathBuf,
    state: GlobalNpmPathState,
}

enum GlobalNpmPathState {
    Absent,
    File {
        bytes: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    Symlink(std::path::PathBuf),
}

impl GlobalNpmPathSnapshot {
    fn capture(path: std::path::PathBuf) -> Result<Self> {
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                GlobalNpmPathState::Symlink(std::fs::read_link(&path)?)
            }
            Ok(metadata) if metadata.is_file() => GlobalNpmPathState::File {
                bytes: std::fs::read(&path)?,
                permissions: metadata.permissions(),
            },
            Ok(_) => anyhow::bail!("cannot snapshot non-file path {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                GlobalNpmPathState::Absent
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()))
            }
        };
        Ok(Self { path, state })
    }

    fn restore(&self) -> Result<()> {
        match &self.state {
            GlobalNpmPathState::Absent => remove_global_npm_snapshot_path(&self.path),
            GlobalNpmPathState::File { bytes, permissions } => {
                atomic_restore_project_file(&self.path, bytes)?;
                std::fs::set_permissions(&self.path, permissions.clone())
                    .with_context(|| format!("restoring permissions for {}", self.path.display()))
            }
            GlobalNpmPathState::Symlink(target) => {
                remove_global_npm_snapshot_path(&self.path)?;
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &self.path)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(target, &self.path)?;
                Ok(())
            }
        }
    }
}

fn remove_global_npm_snapshot_path(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => anyhow::bail!("refusing to remove non-file path {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn restore_moved_global_npm_roots(moved: &[GlobalNpmUninstallRoot]) -> Result<()> {
    let failures = moved
        .iter()
        .rev()
        .filter_map(|root| {
            if !root.backup.exists() {
                return None;
            }
            if root.original.exists() {
                return Some(anyhow!(
                    "refusing to overwrite existing global npm install {} while backup {} also exists",
                    root.original.display(),
                    root.backup.display()
                ));
            }
            rename_global_npm_uninstall_path(&root.backup, &root.original)
                .with_context(|| {
                    format!(
                        "restoring global npm install {}",
                        root.original.display()
                    )
                })
                .err()
        })
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", failures.join("; "))
    }
}

fn unused_sibling_path(path: &std::path::Path, kind: &str) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?
        .to_string_lossy();
    (0..1024u32)
        .map(|attempt| {
            parent.join(format!(
                ".{name}.osdk-{kind}-{}-{attempt}",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| anyhow!("could not allocate transaction path for {}", path.display()))
}

fn combine_rollback_errors(
    first: Option<anyhow::Error>,
    second: Option<anyhow::Error>,
) -> Option<anyhow::Error> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.context(second.to_string())),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

fn with_uninstall_rollback(error: anyhow::Error, rollback: Option<anyhow::Error>) -> anyhow::Error {
    match rollback {
        Some(rollback) => error.context(format!(
            "global npm uninstall rollback failed: {rollback:#}"
        )),
        None => error,
    }
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

pub fn where_cmd(app: &App, tool: String, global: bool) -> Result<()> {
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
        let report = osdk_core::shim::scan_dynamic_installs(&app.ctx)?;
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
    Ok(())
}

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

fn has_other_shim_owner(
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
    let names = routed_bin_names_for_version(&app.ctx, backend, tv)?;
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

pub async fn source(app: &mut App, command: SourceCommand) -> Result<()> {
    match command {
        SourceCommand::List { tool } => {
            let tool = canonical_source_tool(app, &tool)?;
            let sources = if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
                osdk_core::model::source::effective_sources(&app.ctx, provider)
            } else {
                let backend = app.registry.get(&tool)?;
                // Show the ambient candidate too: `source list` is where a user
                // looks to understand why some mirror is being used.
                select::effective_sources_with_env(&app.ctx, backend.as_ref())?
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
                // Show the ambient candidate too: `source list` is where a user
                // looks to understand why some mirror is being used.
                select::effective_sources_with_env(&app.ctx, backend.as_ref())?
                    .iter()
                    .any(|source| source.id == id)
            };
            if !known {
                return Err(anyhow!(t!("err.unknown_source", id = id, tool = tool)));
            }
            crate::config_edit::set_source_pin(&app.ctx, &tool, Some(&id))?;
            println!("{}", t!("msg.source_pinned", tool = tool, id = id));
            if tool == "rust"
                && !osdk_core::backend::rust::RustBackend::isolated_rustup_present(&app.ctx)
            {
                println!("{}", t!("msg.rust_pin_needs_managed_toolchain"));
            }
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
    let mut delta = osdk_core::activate::compute_env_delta(&app.ctx, &app.registry, &cwd)?;
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
                "shims.include = {}",
                if s.shims.include.is_empty() {
                    "all".to_string()
                } else {
                    s.shims.include.join(", ")
                }
            );
            println!(
                "shims.exclude = {}",
                if s.shims.exclude.is_empty() {
                    "none".to_string()
                } else {
                    s.shims.exclude.join(", ")
                }
            );
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

pub async fn android(app: &App, command: AndroidCommand) -> Result<()> {
    match command {
        AndroidCommand::Licenses { command } => android_licenses(app, command).await,
        AndroidCommand::SdkRoot { command } => android_sdk_root(app, command),
        AndroidCommand::Avd { command } => android_avd(app, command),
    }
}

/// Inspect or rebuild the shared SDK root.
fn android_sdk_root(app: &App, command: AndroidSdkRootCommand) -> Result<()> {
    use osdk_core::backend::android::{AndroidBackend, ID_PREFIX, SUPPORTED_FAMILIES};

    let root = AndroidBackend::sdk_root(&app.ctx);
    match command {
        AndroidSdkRootCommand::Show => {
            println!("sdk root: {}", root.display());
            // Reported first because the emulator rejects the whole root without
            // it, whatever else is installed.
            let marker = root.join("platform-tools");
            println!(
                "valid for the emulator: {}",
                if marker.exists() {
                    "yes"
                } else {
                    "no (platform-tools missing)"
                }
            );
            let mut listed = 0usize;
            for family in SUPPORTED_FAMILIES {
                let backend = AndroidBackend::new(family);
                let installed = backend.list_installed(&app.ctx).unwrap_or_default();
                for version in installed {
                    let Some(relative) = AndroidBackend::sdk_root_relative_path(family, &version)
                    else {
                        continue;
                    };
                    let link = root.join(&relative);
                    let real = app
                        .ctx
                        .dirs
                        .install_path(&format!("{ID_PREFIX}{family}"), &version);
                    // Whether the bridge is in place, and whether the index
                    // Google's tools read is present: both are needed for
                    // avdmanager to see the package at all.
                    let bridged = std::fs::canonicalize(&link)
                        .ok()
                        .zip(std::fs::canonicalize(&real).ok())
                        .map(|(a, b)| a == b)
                        .unwrap_or(false);
                    let indexed = real
                        .join(osdk_core::android::package_xml::PACKAGE_XML)
                        .is_file();
                    println!(
                        "  {:<22} {:<34} bridged={} indexed={}",
                        family,
                        version,
                        if bridged { "yes" } else { "no " },
                        if indexed { "yes" } else { "no" }
                    );
                    listed += 1;
                }
            }
            if listed == 0 {
                println!("  (no Android packages installed)");
            }
            // Links whose package is gone are invisible to the loop above, which
            // only walks what is installed. They matter: a dangling entry still
            // passes the existence checks the emulator and Gradle make, so the
            // root looks healthy and fails deeper in.
            let dangling = AndroidBackend::dangling_sdk_root_links(&app.ctx);
            if !dangling.is_empty() {
                println!("dangling links (target no longer installed):");
                for path in &dangling {
                    println!("  {}", path.display());
                }
                println!("remove them with `osdk android sdk-root repair`");
            }
            Ok(())
        }
        AndroidSdkRootCommand::Repair => {
            let mut written = 0usize;
            let mut linked = 0usize;
            for family in SUPPORTED_FAMILIES {
                let backend = AndroidBackend::new(family);
                for version in backend.list_installed(&app.ctx).unwrap_or_default() {
                    if AndroidBackend::repair_package_index(&app.ctx, family, &version) {
                        written += 1;
                    }
                    if AndroidBackend::link_into_sdk_root(&app.ctx, family, &version).is_ok() {
                        linked += 1;
                    }
                }
            }
            // Relinking only covers packages that are still installed, so it
            // cannot see a link whose package is gone. Sweep the root itself.
            let pruned = AndroidBackend::prune_dangling_sdk_root_links(&app.ctx);
            println!(
                "wrote {written} package index file(s) and checked {linked} SDK root link(s) under {}",
                root.display()
            );
            if written > 0 {
                // Worth saying explicitly: this is the failure the repair fixes.
                println!(
                    "`avdmanager` and `sdkmanager` can now see these packages; \
                     without the index avdmanager reports `Package path is not valid`"
                );
            }
            if pruned.is_empty() {
                println!("no dangling links to remove");
            } else {
                println!(
                    "removed {} dangling link(s) left by an uninstall:",
                    pruned.len()
                );
                for path in &pruned {
                    println!("  {}", path.display());
                }
            }
            Ok(())
        }
    }
}

/// Create, list and delete AVDs without avdmanager.
fn android_avd(app: &App, command: AndroidAvdCommand) -> Result<()> {
    use osdk_core::android::avd::{self, CreateOptions, ImageId};
    use osdk_core::backend::android::{AndroidBackend, SYSTEM_IMAGES_FAMILY};

    match command {
        AndroidAvdCommand::List => {
            let devices = avd::list(&app.ctx.dirs);
            if devices.is_empty() {
                println!("no AVDs under {}", avd::avd_home(&app.ctx.dirs).display());
                return Ok(());
            }
            for device in devices {
                // The image is reported present or missing rather than just
                // echoed: an AVD whose image was uninstalled looks fine here but
                // dies inside the emulator.
                let present = device.image_present();
                println!(
                    "{:<24} {:<12} image={}",
                    device.name,
                    device.target.as_deref().unwrap_or("-"),
                    if present { "ok" } else { "MISSING" }
                );
                println!("  {}", device.path.display());
            }
            Ok(())
        }
        AndroidAvdCommand::Create {
            name,
            image,
            force,
            data_size,
            sdcard_size,
        } => {
            let id = ImageId::parse(&image)?;
            let backend = AndroidBackend::new(SYSTEM_IMAGES_FAMILY);
            let version = id.as_version();
            let installed = backend.list_installed(&app.ctx).unwrap_or_default();
            if !installed.iter().any(|candidate| candidate == &version) {
                return Err(anyhow!(
                    "system image `{version}` is not installed; install it with \
                     `osdk install \"android-system-images@{version}\"`{}",
                    if installed.is_empty() {
                        String::new()
                    } else {
                        format!("\ninstalled images: {}", installed.join(", "))
                    }
                ));
            }
            // Two paths reach the same payload: the versioned install directory,
            // and the bridged one under the SDK root. The emulator expands
            // `%VAR%` in `image.sysdir.1`, and osdk's versioned directory names
            // are percent-encoded, so the bridged path is the only one it can
            // read. Verified: the install path yields `Broken AVD system path`.
            let install_dir = app.ctx.dirs.install_path("android-system-images", &version);
            let bridged = AndroidBackend::sdk_root_relative_path(SYSTEM_IMAGES_FAMILY, &version)
                .map(|relative| AndroidBackend::sdk_root(&app.ctx).join(relative))
                .filter(|path| path.join("system.img").is_file());
            let image_dir = match bridged {
                Some(path) => path,
                None => {
                    // Without the bridge there is no `%`-free path to the image,
                    // so say what to do rather than writing a config that fails
                    // later inside the emulator.
                    return Err(anyhow!(
                        "the system image is installed at {} but is not exposed \
                         under the SDK root, and the emulator cannot read that \
                         path directly; run `osdk android sdk-root repair` first",
                        install_dir.display()
                    ));
                }
            };
            // The display name shown by the emulator, read from the image's own
            // metadata so it matches what Google's tools would show.
            let tag_display = osdk_core::android::package_xml::read_source_properties(&install_dir)
                .and_then(|fields| fields.get("SystemImage.TagDisplay").cloned())
                .unwrap_or_else(|| id.tag.clone());
            let options = CreateOptions {
                force,
                data_size,
                sdcard_size,
            };
            let path = avd::create(
                &app.ctx.dirs,
                &name,
                &id,
                &image_dir,
                &tag_display,
                &options,
            )?;
            println!("created AVD `{name}` at {}", path.display());
            // Both are required to actually boot, and neither is implied by
            // installing the image.
            let root = AndroidBackend::sdk_root(&app.ctx);
            if !root.join("platform-tools").exists() {
                println!(
                    "note: the emulator will reject this SDK root until \
                     platform-tools is installed"
                );
            }
            println!("start it with: emulator -avd {name}");
            Ok(())
        }
        AndroidAvdCommand::Delete { name } => {
            if avd::delete(&app.ctx.dirs, &name)? {
                println!("deleted AVD `{name}`");
            } else {
                println!("no AVD named `{name}`");
            }
            Ok(())
        }
    }
}

async fn android_licenses(app: &App, command: AndroidLicensesCommand) -> Result<()> {
    use osdk_core::android::license;
    use osdk_core::backend::android::AndroidBackend;

    let sdk_root = AndroidBackend::sdk_root(&app.ctx);
    match command {
        AndroidLicensesCommand::Show { tool, digest_only } => {
            let request = ToolRequest::parse(&tool)?;
            if !AndroidBackend::owns_id(&request.backend) {
                return Err(anyhow!(
                    "`{}` is not an Android SDK tool; expected e.g. `android-ndk@29.0.14206865`",
                    request.backend
                ));
            }
            let backend = app.registry.get(&request.backend)?;
            let version = backend.resolve_version(&app.ctx, &request).await?;
            // Reach the concrete backend for its manifest helpers.
            let family = request
                .backend
                .strip_prefix(osdk_core::backend::android::ID_PREFIX)
                .unwrap_or_default();
            let android = AndroidBackend::all()
                .into_iter()
                .find(|candidate| candidate.family() == family)
                .ok_or_else(|| anyhow!("unknown Android family `{family}`"))?;
            let manifest = android.manifest(&app.ctx).await?;
            let package = android.package(&manifest, &version.version)?;
            let Some(license) = manifest.license_for(package) else {
                println!("{} requires no license agreement", package.path);
                return Ok(());
            };
            let recorded = license::is_recorded(&sdk_root, license);
            println!("package:  {}", package.path);
            println!("license:  {}", license.id);
            println!("digest:   {}", license.hash());
            println!("accepted: {}", if recorded { "yes" } else { "no" });
            if !digest_only {
                println!();
                println!("{}", license.text);
            }
            if !recorded {
                println!();
                println!(
                    "to accept: osdk install {tool} -o accept-license={}",
                    license.id
                );
            }
            Ok(())
        }
        AndroidLicensesCommand::Status => {
            let dir = sdk_root.join(license::LICENSES_DIR);
            let mut found = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        found.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
            found.sort();
            if found.is_empty() {
                println!("no Android licenses recorded under {}", dir.display());
                return Ok(());
            }
            println!("recorded under {}:", dir.display());
            for id in found {
                let body = std::fs::read_to_string(dir.join(&id)).unwrap_or_default();
                let digests: Vec<&str> = body
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                println!("  {id}  ({})", digests.join(", "));
            }
            Ok(())
        }
        AndroidLicensesCommand::Export { sdk_root: dest } => {
            let source = sdk_root.join(license::LICENSES_DIR);
            let target = dest.join(license::LICENSES_DIR);
            let entries = std::fs::read_dir(&source).map_err(|error| {
                anyhow!("no recorded licenses at {}: {error}", source.display())
            })?;
            std::fs::create_dir_all(&target)?;
            let mut copied = 0usize;
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let to = target.join(entry.file_name());
                std::fs::copy(&path, &to)?;
                copied += 1;
            }
            println!(
                "exported {copied} license record(s) to {}",
                target.display()
            );
            Ok(())
        }
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

pub async fn rust(app: &mut App, command: RustCommand) -> Result<()> {
    match command {
        RustCommand::Component { command } => rust_item(app, "component", command).await,
        RustCommand::Target { command } => rust_item(app, "target", command).await,
        RustCommand::Check { repair } => rust_check(app, repair),
        RustCommand::Override { command } => rust_override(app, command),
        RustCommand::Toolchain { command } => rust_toolchain(app, command),
    }
}

/// The source `rust` operations should drive, honoring `--source` and the
/// configured pin. Adding a component or target downloads from the dist server,
/// so it must use the same selection the install path uses instead of whatever
/// the ambient environment happens to hold.
async fn selected_rust_source(app: &mut App) -> Result<Option<osdk_core::source::Source>> {
    apply_source_override(app, "rust");
    let backend = app.registry.get("rust")?;
    match osdk_core::source::select::active_source(&app.ctx, backend.as_ref()).await {
        Ok(source) => Ok(Some(source)),
        // Selection needs no network when a pin resolves, but probing can fail
        // offline. rustup still has its own default host, so a failed selection
        // must not block a local operation.
        Err(error) => {
            tracing::debug!(%error, "falling back to rustup's default dist server");
            Ok(None)
        }
    }
}

async fn rust_item(app: &mut App, kind: &str, command: RustItemCommand) -> Result<()> {
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
    // Only `add` downloads; the others are local and must not pay for a probe.
    let source = if operation == "add" {
        selected_rust_source(app).await?
    } else {
        None
    };
    if let Some(source) = &source {
        tracing::info!(
            source = %source.id,
            dist = %source.download_url,
            "{}",
            osdk_core::i18n::tr("log.rustup_dist_server")
        );
    }
    let output =
        osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &args, None, source.as_ref())?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn rust_check(app: &App, repair: bool) -> Result<()> {
    let output =
        osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &["check"], None, None)?;
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

pub fn doctor(app: &App, verify: bool, tool: Option<String>) -> Result<()> {
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
    if verify {
        doctor_verify(app, tool.as_deref())?;
    }
    Ok(())
}

/// Re-hash every installed file and report drift.
///
/// Downloads are verified on the way in, but nothing looked at the bytes again
/// afterwards, so a tool that rewrites itself in place, a manual edit or a
/// half-restored backup left osdk reporting the version it installed while a
/// different one actually ran. Reads every file, so it sits behind
/// `--verify` rather than in the default diagnostics.
fn doctor_verify(app: &App, only: Option<&str>) -> Result<()> {
    use osdk_core::store::manifest::Manifest;

    println!("{}", t!("doctor.verify_title"));
    let mut checked = 0usize;
    let mut drifted: Vec<(String, String, Vec<osdk_core::store::manifest::Drift>)> = Vec::new();
    let mut unverifiable: Vec<String> = Vec::new();

    // Reading every file of every SDK takes minutes, so honour a request
    // to check just the tool the user actually suspects.
    let backends = match only {
        Some(tool) => vec![app.registry.get(tool)?],
        None => all_display_backends(app)?,
    };
    for backend in backends {
        for version in backend.list_installed(&app.ctx)? {
            let root = app.ctx.dirs.install_path(backend.id(), &version);
            let label = format!("{}@{}", backend.id(), version);
            // An install predating the manifest, or one whose manifest was
            // itself removed, cannot be checked. Say so instead of passing it.
            let manifest = match Manifest::load(&root) {
                Ok(manifest) => manifest,
                Err(_) => {
                    unverifiable.push(label);
                    continue;
                }
            };
            checked += 1;
            // A large SDK takes tens of seconds on its own, so name it before
            // reading it rather than leaving the terminal silent for minutes.
            print!("    {label} ... ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let drift = manifest.verify(&root)?;
            println!(
                "{}",
                if drift.is_empty() {
                    t!("doctor.verify_item_ok")
                } else {
                    t!("doctor.verify_item_drift", count = drift.len())
                }
            );
            if !drift.is_empty() {
                drifted.push((label, root.display().to_string(), drift));
            }
        }
    }

    for (label, root, drift) in &drifted {
        println!("  {label} : {}", t!("doctor.verify_drifted"));
        println!("    {root}");
        // Cap the per-install listing: a rewritten NDK would otherwise bury
        // every other finding under thousands of lines.
        for item in drift.iter().take(10) {
            println!("      {item}");
        }
        if drift.len() > 10 {
            println!(
                "      {}",
                t!("doctor.verify_more", count = drift.len() - 10)
            );
        }
    }
    for label in &unverifiable {
        println!("  {label} : {}", t!("doctor.verify_no_manifest"));
    }

    println!(
        "  {}",
        t!(
            "doctor.verify_summary",
            checked = checked,
            drifted = drifted.len()
        )
    );
    if !drifted.is_empty() {
        // Name the exact command, since the fix is not discoverable: a plain
        // reinstall skips anything already present.
        println!("  {}", t!("doctor.verify_hint"));
        for (label, _, _) in &drifted {
            println!("    osdk install --force {label}");
        }
    }
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
        let report = osdk_core::shim::scan_dynamic_installs(ctx)?;
        return Ok(keep(
            osdk_core::shim::validated_dynamic_install(ctx, &report, &request, &version.version)?
                .bin_names(),
        ));
    }
    let names = osdk_core::shim::routed_bin_names(ctx, backend, version)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    Ok(keep(names.into_iter().collect()))
}

fn managed_bin_paths(
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

fn exact_request_for_version(version: &ToolVersion) -> ToolRequest {
    ToolRequest {
        backend: version.backend.clone(),
        spec: VersionSpec::Exact(version.version.clone()),
        options: version.options.clone(),
    }
}

fn request_selects_installed_version(
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

fn request_selects_version_from_candidates(
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

fn request_spec_after_alias(original: &VersionSpec, expanded: &str) -> VersionSpec {
    if matches!(original, VersionSpec::Range(_)) {
        VersionSpec::parse_range(expanded).unwrap_or_else(|_| VersionSpec::parse(expanded))
    } else {
        VersionSpec::parse(expanded)
    }
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
    let dynamic_report = dynamic_scan_report(app)?;
    for (name, candidates) in osdk_core::shim::dynamic_bin_ownership(&dynamic_report) {
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
                let selected = app
                    .registry
                    .get(&candidate.canonical_id)
                    .ok()
                    .and_then(|backend| {
                        request_selects_installed_version(app, backend.as_ref(), &request, version)
                            .ok()
                    })
                    .unwrap_or(false);
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

#[cfg(test)]
mod command_flow_tests {
    use super::*;
    use std::sync::Arc;

    use crate::prompt::TerminalPrompt;

    fn layered_npm_tool_config() -> (tempfile::TempDir, osdk_core::config::Config) {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/nested");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"npm:fixture-cli" = { version = "1", installer = "pnpm", allow_builds = ["global-build"] }
"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("project/osdk.toml"),
            r#"
[tools]
"npm:fixture-cli" = { version = "2", installer = "npm", allow_builds = ["project-build"] }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        (temporary, config)
    }

    fn app_with_config(
        temporary: &tempfile::TempDir,
        config: osdk_core::config::Config,
    ) -> crate::app::App {
        let dirs = osdk_core::dirs::Dirs {
            data: temporary.path().join("data"),
            cache: temporary.path().join("cache"),
            config: temporary.path().join("config"),
            store: temporary.path().join("store"),
            installs: temporary.path().join("installs"),
        };
        for directory in [
            &dirs.data,
            &dirs.cache,
            &dirs.config,
            &dirs.store,
            &dirs.installs,
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let client = osdk_core::http::client().unwrap();
        let cas = Arc::new(osdk_core::store::Cas::new(dirs.store.clone()));
        let registry = osdk_core::Registry::load(&dirs).unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs,
            platform: osdk_core::platform::Platform::current(),
            config,
            client,
            cas,
            show_progress: false,
        };
        crate::app::App {
            ctx,
            registry,
            prompt: Arc::new(TerminalPrompt::new(false)),
            source_override: None,
            refresh_sources: false,
        }
    }

    #[test]
    fn android_r8_tools_resolve_to_build_tools_instead_of_conflicting() {
        // Google ships the same R8 launchers in two families. Without a
        // precedence rule this refused every shim of whichever family was
        // installed second, including sdkmanager, which nothing else provides.
        let both = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
        ]);
        for name in ["d8", "r8", "retrace", "resourceshrinker"] {
            assert!(!is_real_shim_conflict(name, &both), "{name}");
            assert_eq!(
                osdk_core::shim::precedence_winner(name, &both),
                Some("android-build-tools"),
                "{name}"
            );
        }
    }

    #[test]
    fn names_owned_by_one_android_family_have_no_precedence_winner() {
        // `sdkmanager` and `aapt2` are single-owner; they must stay ordinary.
        let cmdline = std::collections::BTreeSet::from(["android-cmdline-tools".to_string()]);
        assert!(!is_real_shim_conflict("sdkmanager", &cmdline));
        assert_eq!(
            osdk_core::shim::precedence_winner("sdkmanager", &cmdline),
            None
        );
        // A name outside the curated set stays a real conflict.
        let unrelated = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "node".to_string(),
        ]);
        assert!(is_real_shim_conflict("aapt2", &unrelated));
    }

    #[test]
    fn an_unexpected_third_owner_is_still_a_real_conflict() {
        // Precedence only decides among the known Android families; anything
        // else must still be surfaced to the user.
        let with_outsider = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
            "npm:some-d8-clone".to_string(),
        ]);
        assert_eq!(
            osdk_core::shim::precedence_winner("d8", &with_outsider),
            None
        );
        assert!(is_real_shim_conflict("d8", &with_outsider));
    }
    #[test]
    fn global_npm_use_options_ignore_project_scope_and_apply_cli_last() {
        let (_temporary, config) = layered_npm_tool_config();
        let mut configured = ToolRequest::parse("npm:fixture-cli@3").unwrap();

        apply_use_options(&config, &mut configured, true, &[], None).unwrap();

        assert_eq!(configured.options["installer"], "pnpm");
        assert_eq!(configured.options["allow_builds"], "global-build");

        let mut overridden = ToolRequest::parse("npm:fixture-cli@3").unwrap();
        apply_use_options(
            &config,
            &mut overridden,
            true,
            &["installer=pnpm".into(), "allow_builds=cli-build".into()],
            None,
        )
        .unwrap();

        assert_eq!(overridden.options["installer"], "pnpm");
        assert_eq!(overridden.options["allow_builds"], "cli-build");
    }

    #[test]
    fn local_npm_use_options_keep_project_merged_values() {
        let (_temporary, config) = layered_npm_tool_config();
        let mut request = ToolRequest::parse("npm:fixture-cli@3").unwrap();

        apply_use_options(&config, &mut request, false, &[], None).unwrap();

        assert_eq!(request.options["installer"], "npm");
        assert_eq!(request.options["allow_builds"], "project-build");
    }

    #[test]
    fn npm_lifecycle_hint_preserves_request_identity_options() {
        let mut request =
            ToolRequest::parse("npm:fixture-cli[installer=pnpm,allow_builds='sharp,esbuild']@3")
                .unwrap();
        request
            .options
            .insert("__osdk_npm_node_version".into(), "22.1.0".into());

        let project = npm_scope_hint(&request, false);
        assert_eq!(project.options, request.options);

        let global = npm_scope_hint(&request, true);
        assert_eq!(global.options["installer"], "pnpm");
        assert_eq!(global.options["allow_builds"], "esbuild,sharp");
        assert_eq!(global.options["__osdk_npm_node_version"], "22.1.0");
        assert_eq!(
            global.options[osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION],
            osdk_core::npm_tools::ToolScope::Global.as_str()
        );
    }

    #[test]
    fn explicit_operand_resolves_through_configured_indirect_key() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = { version = "node@20.10.0", corepack = true }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let request = resolve_explicit_request(
            &app,
            "tool.node",
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )
        .unwrap();

        assert_eq!(request.backend, "node");
        assert_eq!(request.spec, VersionSpec::Exact("20.10.0".into()));
        assert_eq!(request.options["corepack"], "true");
    }

    #[test]
    fn explicit_operand_keeps_explicit_spec_when_resolving_indirect_key() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = { version = "node@20.10.0", corepack = true }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let request = resolve_explicit_request(
            &app,
            "tool.node@22.0.0",
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )
        .unwrap();

        assert_eq!(request.backend, "node");
        assert_eq!(request.spec, VersionSpec::Exact("22.0.0".into()));
        assert_eq!(request.options["corepack"], "true");
    }

    #[test]
    fn select_use_persist_target_reports_ambiguous_indirect_backend() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = "node@20.10.0"
"tool.node.lts" = "node@22.0.0"
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let error = select_use_persist_target(&app, "node", false, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple configured tool keys resolve to `node`"),
            "{error}"
        );
    }

    #[test]
    fn project_npm_specs_reject_conflicting_aliases_and_accept_identical_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let error = project_npm_configured_specs(&app, &config_path, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("npm:fixture-cli"), "{message}");
        assert!(message.contains("tool.alpha"), "{message}");
        assert!(message.contains("1.2.3"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");
        assert!(message.contains("2.0.0"), "{message}");

        std::fs::write(
            &config_path,
            "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let error = project_npm_configured_specs(&app, &config_path, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("npm:fixture-cli"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");

        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@1.2.3\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        assert_eq!(
            project_npm_configured_specs(&app, &config_path, None).unwrap(),
            std::collections::BTreeMap::from([("npm:fixture-cli".into(), "1.2.3".into())])
        );
    }

    #[test]
    fn project_npm_specs_replace_the_current_alias_before_conflict_checking() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.0.0\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        assert_eq!(
            project_npm_configured_specs(
                &app,
                &config_path,
                Some(("tool.alpha", "npm:fixture-cli", "2.0.0")),
            )
            .unwrap(),
            std::collections::BTreeMap::from([("npm:fixture-cli".into(), "2.0.0".into())])
        );

        let error = project_npm_configured_specs(
            &app,
            &config_path,
            Some(("tool.alpha", "npm:fixture-cli", "3.0.0")),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("tool.alpha"), "{message}");
        assert!(message.contains("3.0.0"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");
        assert!(message.contains("2.0.0"), "{message}");
    }

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
                r#"{"dependencies":{"prettier":"^2"},"optionalDependencies":{"prettier":"^2"}}"#,
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
    fn project_file_snapshot_restores_old_bytes_and_removes_new_files() {
        let temporary = tempfile::tempdir().unwrap();
        let existing = temporary.path().join("package.json");
        let created = temporary.path().join("package-lock.json");
        std::fs::write(&existing, b"old manifest").unwrap();
        let existing_snapshot = snapshot_project_file(&existing).unwrap();
        let created_snapshot = snapshot_project_file(&created).unwrap();

        std::fs::write(&existing, b"new manifest").unwrap();
        std::fs::write(&created, b"new lock").unwrap();
        existing_snapshot.restore().unwrap();
        created_snapshot.restore().unwrap();

        assert_eq!(std::fs::read(&existing).unwrap(), b"old manifest");
        assert!(!created.exists());
    }

    #[test]
    fn expected_project_native_lock_preserves_incumbent_and_defaults_to_installer() {
        use osdk_core::npm_tools::{NativeLock, NativeLockKind, NpmInstaller, NpmProject};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let project = NpmProject {
            root: root.to_path_buf(),
            package_json: root.join("package.json"),
            declared_manager: None,
            native_lock: Some(NativeLock {
                kind: NativeLockKind::Pnpm,
                path: root.join("pnpm-lock.yaml"),
                version: 9,
                format: "pnpm-v9".into(),
                supported: true,
            }),
        };
        // An incumbent lock wins over what the installer would pick.
        assert_eq!(
            expected_project_native_lock(&project, NpmInstaller::Npm),
            (NativeLockKind::Pnpm, root.join("pnpm-lock.yaml"))
        );

        let without_lock = NpmProject {
            native_lock: None,
            ..project
        };
        assert_eq!(
            expected_project_native_lock(&without_lock, NpmInstaller::Npm),
            (NativeLockKind::PackageLock, root.join("package-lock.json"))
        );
        assert_eq!(
            expected_project_native_lock(&without_lock, NpmInstaller::Pnpm),
            (NativeLockKind::Pnpm, root.join("pnpm-lock.yaml"))
        );
        let switched = NativeLock {
            kind: NativeLockKind::Pnpm,
            path: root.join("pnpm-lock.yaml"),
            version: 9,
            format: "pnpm-v9".into(),
            supported: true,
        };
        let error = validate_installed_project_native_lock(
            NpmInstaller::Npm,
            &(NativeLockKind::PackageLock, root.join("package-lock.json")),
            &switched,
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed native lock identity"));
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

    fn request(backend: &str, spec: VersionSpec) -> ToolRequest {
        ToolRequest {
            backend: backend.into(),
            spec,
            options: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn cargo_requests_inject_exactly_one_configured_rust_request() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            "[tools]\nrust = { version = \"1.91.1\", profile = \"minimal\" }\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request(
            "cargo:https://github.com/BurntSushi/ripgrep.git",
            VersionSpec::Exact("rev:0123456789abcdef0123456789abcdef01234567".into()),
        );

        let requests = inject_rust_dependency_at(&app, vec![cargo], &project).unwrap();
        let rust = requests
            .iter()
            .filter(|request| request.backend == "rust")
            .collect::<Vec<_>>();

        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].spec, VersionSpec::Exact("1.91.1".into()));
        assert_eq!(rust[0].options["profile"], "minimal");
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn cargo_requests_preserve_one_explicit_rust_and_reject_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));
        let rust = ToolRequest::parse("rust@1.91.1").unwrap();

        let requests = inject_rust_dependency(&app, vec![cargo.clone(), rust.clone()]).unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.backend == "rust")
                .count(),
            1
        );
        let error = inject_rust_dependency(&app, vec![cargo, rust.clone(), rust]).unwrap_err();
        assert!(error.to_string().contains("exactly one managed Rust"));
    }

    #[test]
    fn cargo_requests_reject_missing_or_floating_rust_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));

        let missing = inject_rust_dependency_at(&app, vec![cargo.clone()], &project).unwrap_err();
        assert!(missing.to_string().contains("configure `rust"), "{missing}");

        for rust in [
            ToolRequest::parse("rust@stable").unwrap(),
            request("rust", VersionSpec::Latest),
        ] {
            let floating =
                inject_rust_dependency_at(&app, vec![cargo.clone(), rust], &project).unwrap_err();
            assert!(
                floating.to_string().contains("exact managed Rust version"),
                "{floating}"
            );
        }
    }

    #[test]
    fn non_cargo_requests_do_not_inject_or_require_rust() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let npm = ToolRequest::parse("npm:prettier@3.6.2").unwrap();

        let requests = inject_rust_dependency_at(&app, vec![npm.clone()], &project).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].backend, npm.backend);
        assert!(requests[0].options.is_empty());
    }

    #[test]
    fn cargo_runtime_partition_and_binding_are_exact_and_npm_neutral() {
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));
        let npm = ToolRequest::parse("npm:prettier@3.6.2").unwrap();
        let rust = ToolRequest::parse("rust@1.91.1").unwrap();
        let node = ToolRequest::parse("node@20.10.0").unwrap();
        let (runtime, mut remaining) =
            partition_runtime_dependency(vec![cargo, npm, rust, node], "rust", "cargo:");
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].backend, "rust");
        assert!(remaining.iter().all(|request| request.backend != "rust"));

        let resolved = vec![(runtime[0].clone(), ToolVersion::new("rust", "1.91.1"))];
        bind_request_rust_version(&mut remaining, &resolved).unwrap();
        let cargo = remaining
            .iter()
            .find(|request| request.backend.starts_with("cargo:"))
            .unwrap();
        assert_eq!(cargo.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert_eq!(
            cargo.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        let npm = remaining
            .iter()
            .find(|request| request.backend.starts_with("npm:"))
            .unwrap();
        assert!(npm.options.is_empty());
    }

    #[test]
    fn resolved_cargo_lock_metadata_uses_the_same_exact_rust_version() {
        let mut resolved = vec![
            (
                request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into())),
                ToolVersion::new("cargo:ripgrep", "14.1.1"),
            ),
            (
                ToolRequest::parse("rust@1.91.1").unwrap(),
                ToolVersion::new("rust", "1.91.1"),
            ),
        ];

        bind_resolved_rust_version(&mut resolved).unwrap();

        assert_eq!(
            resolved[0].1.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        assert_eq!(resolved[0].1.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert!(resolved[1].1.options.is_empty());
    }

    #[test]
    fn go_requests_inject_configured_fuzzy_runtime_and_bind_exact_resolution() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\ngo = \"1.24\"\n").unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();

        let requests = inject_go_dependency_at(&app, vec![tool], &project).unwrap();
        let runtime = requests
            .iter()
            .find(|request| request.backend == "go")
            .unwrap();
        assert_eq!(runtime.spec, VersionSpec::Prefix("1.24".into()));

        let (runtime, mut remaining) = partition_runtime_dependency(requests, "go", "go:");
        let resolved = vec![(runtime[0].clone(), ToolVersion::new("go", "1.24.6"))];
        bind_request_go_version(&mut remaining, &resolved).unwrap();
        assert_eq!(remaining[0].options[LOCKED_NATIVE_RUNTIME_OPTION], "go");
        assert_eq!(
            remaining[0].options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.24.6"
        );
    }

    #[test]
    fn consent_only_opts_still_allow_lockfile_replay() {
        // Consent is absent from the lock by design, so demanding it must not
        // make a committed lock file unusable.
        assert!(opts_are_only_consent(&[]));
        assert!(opts_are_only_consent(&["accept-licenses=true".into()]));
        assert!(opts_are_only_consent(&[
            "accept-licenses=true".into(),
            "accept-license=android-sdk-license".into(),
        ]));

        // Anything that actually selects an artifact keeps the old behaviour of
        // bypassing the lock.
        assert!(!opts_are_only_consent(&["channel=beta".into()]));
        assert!(!opts_are_only_consent(&[
            "accept-licenses=true".into(),
            "profile=minimal".into(),
        ]));
        // A malformed option is not consent either.
        assert!(!opts_are_only_consent(&["accept-licenses".into()]));
    }

    #[test]
    fn generic_opts_do_not_pollute_injected_go_runtime() {
        let mut tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();
        for (key, value) in parse_opts(&["tags=netgo".into()]).unwrap() {
            tool.options.insert(key, value);
        }
        let requests = [tool, ToolRequest::parse("go@1.24").unwrap()];
        let runtime = requests
            .iter()
            .find(|request| request.backend == "go")
            .unwrap();
        let tool = requests
            .iter()
            .find(|request| request.backend.starts_with("go:"))
            .unwrap();
        assert!(runtime.options.is_empty());
        assert_eq!(tool.options["tags"], "netgo");
    }

    #[test]
    fn go_requests_preserve_one_explicit_runtime_and_reject_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();
        let runtime = ToolRequest::parse("go@latest").unwrap();

        let requests =
            inject_go_dependency_at(&app, vec![tool.clone(), runtime.clone()], &project).unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.backend == "go")
                .count(),
            1
        );
        let error = inject_go_dependency_at(&app, vec![tool, runtime.clone(), runtime], &project)
            .unwrap_err();
        assert!(error.to_string().contains("exactly one managed Go"));
    }

    #[test]
    fn resolved_go_lock_metadata_requires_an_exact_runtime_result() {
        let mut resolved = vec![
            (
                ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap(),
                ToolVersion::new("go:example.com/acme/tool", "1.2.3"),
            ),
            (
                ToolRequest::parse("go@1.24").unwrap(),
                ToolVersion::new("go", "1.24.6"),
            ),
        ];
        bind_resolved_go_version(&mut resolved).unwrap();
        assert_eq!(resolved[0].1.options[LOCKED_NATIVE_RUNTIME_OPTION], "go");
        assert_eq!(
            resolved[0].1.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.24.6"
        );

        resolved[1].1.version = "latest".into();
        assert!(bind_resolved_go_version(&mut resolved).is_err());
    }

    #[test]
    fn user_options_cannot_inject_private_go_replay_metadata() {
        for key in [
            "__osdk_native_replay",
            "__osdk_native_runtime",
            "__osdk_native_runtime_version",
            "__osdk_go_proxy",
            "__osdk_go_module",
        ] {
            let option = format!("{key}=value");
            let error = parse_opts(&[option]).unwrap_err();
            assert!(error.to_string().contains("internal option"), "{key}");
            let error = reject_public_internal_options(&std::collections::BTreeMap::from([(
                key.to_string(),
                "value".into(),
            )]))
            .unwrap_err();
            assert!(error.to_string().contains("internal option"), "{key}");
        }
    }

    #[test]
    fn global_go_dependency_uses_global_selection_not_project_override() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\ngo = \"1.24\"\n").unwrap();
        std::fs::write(project.join("osdk.toml"), "[tools]\ngo = \"1.23\"\n").unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        assert_eq!(app.ctx.config.tools["go"], "1.23");
        let request = global_go_dependency_request(&app).unwrap();
        assert_eq!(request.backend, "go");
        assert_eq!(request.spec, VersionSpec::Prefix("1.24".into()));
    }

    #[test]
    fn go_use_persists_canonical_public_options_only() {
        let request = ToolRequest {
            backend: "go:example.com/acme/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([
                ("tags".into(), "sqlite,netgo,sqlite".into()),
                ("env".into(), "GOAMD64=v3;CGO_ENABLED=0".into()),
            ]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved.options.extend(std::collections::BTreeMap::from([
            ("tags".into(), "netgo,sqlite".into()),
            ("env".into(), "CGO_ENABLED=0;GOAMD64=v3".into()),
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), "1.24.6".into()),
            (
                osdk_core::backend::native_tool::LOCKED_NATIVE_REPLAY_OPTION.into(),
                "version-only".into(),
            ),
            (
                osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
                "https://proxy.golang.org".into(),
            ),
            (
                osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION.into(),
                "example.com/acme/tool".into(),
            ),
        ]));
        bind_dynamic_request_options(&request, &mut resolved);
        let canonical =
            osdk_core::backend::dynamic::identity_options(&request.backend, &resolved.options)
                .unwrap();
        assert_eq!(canonical["tags"], "netgo,sqlite");
        assert_eq!(canonical["env"], "CGO_ENABLED=0;GOAMD64=v3");
        assert!(canonical.keys().all(|key| !key.starts_with("__osdk_")));
    }

    #[test]
    fn compatibility_requests_are_explicitly_bound_to_isolated_scope() {
        let mut requests = vec![
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolRequest::parse("node@20.10.0").unwrap(),
        ];
        mark_isolated_npm_scope(&mut requests);

        assert_eq!(
            requests[0].options[osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION],
            osdk_core::npm_tools::ToolScope::Project.as_str()
        );
        assert!(!requests[1]
            .options
            .contains_key(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION));
    }

    #[test]
    fn dynamic_request_options_are_bound_to_resolved_version() {
        let request = ToolRequest {
            backend: "github:example/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([(
                "rename".into(),
                "configured-name".into(),
            )]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved
            .options
            .insert("rename".into(), "backend-name".into());
        resolved.options.insert(
            "__osdk_artifact_url".into(),
            "https://example.invalid/tool".into(),
        );

        bind_dynamic_request_options(&request, &mut resolved);

        assert_eq!(resolved.options["rename"], "configured-name");
        assert_eq!(
            resolved.options["__osdk_artifact_url"],
            "https://example.invalid/tool"
        );
    }

    #[test]
    fn dynamic_request_cannot_overwrite_backend_locked_metadata() {
        let request = ToolRequest {
            backend: "go:example.com/acme/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([(
                osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
                "https://attacker.example".into(),
            )]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved.options.insert(
            osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
            "https://proxy.golang.org".into(),
        );
        bind_dynamic_request_options(&request, &mut resolved);
        assert_eq!(
            resolved.options[osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION],
            "https://proxy.golang.org"
        );
    }

    #[test]
    fn reshim_selects_only_the_configured_dynamic_version() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            "[tools]\n\"github:example/tool\" = { version = \"1.2.3\", rename = \"configured\" }\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let backend = app.registry.get("github:example/tool").unwrap();
        let request =
            osdk_core::shim::dynamic_request_from_config(&app.ctx, "github:example/tool").unwrap();

        assert!(
            request_selects_installed_version(&app, backend.as_ref(), &request, "1.2.3").unwrap()
        );
        assert!(
            !request_selects_installed_version(&app, backend.as_ref(), &request, "1.2.4").unwrap()
        );
        assert_eq!(request.options["rename"], "configured");
    }

    #[test]
    fn exact_reshim_selection_does_not_require_a_validated_installed_listing() {
        let spec = VersionSpec::Exact("1.2.3".into());
        let selected =
            request_selects_version_from_candidates("github:example/tool", &spec, "1.2.3", || {
                anyhow::bail!("inventory listing rejected legacy identity")
            })
            .unwrap();

        assert!(selected);
    }

    fn write_dynamic_install(
        app: &App,
        backend: &str,
        version: &str,
        bin_name: &str,
        options: &std::collections::BTreeMap<String, String>,
    ) {
        let checksum = format!("sha256:{}", "a".repeat(64));
        let materials = if backend.starts_with("github:") {
            std::collections::BTreeMap::from([
                ("artifact-file".into(), "fixture.bin".into()),
                ("artifact-checksum".into(), checksum.clone()),
            ])
        } else {
            std::collections::BTreeMap::new()
        };
        let identity = osdk_core::tool::InstallIdentity::new(
            backend,
            version,
            app.ctx.platform.to_string(),
            osdk_core::tool::InstallScope::Isolated,
            options,
            Vec::new(),
            materials,
        )
        .unwrap();
        let root = osdk_core::dirs::InstallLocator::new(&app.ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let bin = root.join("bin").join(bin_name);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        if backend.starts_with("github:") {
            let receipt = osdk_core::pipeline::ArtifactReceipt {
                url: "https://example.test/fixture.bin".into(),
                file_name: "fixture.bin".into(),
                checksum: Some(checksum),
                evidence: Vec::new(),
            };
            std::fs::write(
                root.join(".osdk-artifact.json"),
                serde_json::to_vec_pretty(&receipt).unwrap(),
            )
            .unwrap();
        }
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
        }];
        manifest.write_atomic(&root).unwrap();
    }

    #[test]
    fn managed_dynamic_paths_require_matching_request_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let backend = app.registry.get("github:example/tool").unwrap();
        let installed_options =
            std::collections::BTreeMap::from([("rename".into(), "installed".into())]);
        write_dynamic_install(&app, backend.id(), "1.2.3", "installed", &installed_options);
        let version = ToolVersion::new(backend.id(), "1.2.3");
        let request = ToolRequest {
            backend: backend.id().into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([("rename".into(), "configured".into())]),
        };

        let error =
            managed_bin_paths(&app.ctx, backend.as_ref(), &version, Some(&request)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no complete install matching its unlocked request"),
            "{error}"
        );
    }

    #[test]
    fn unconfigured_dynamic_inventory_is_not_a_runtime_shim_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        write_dynamic_install(
            &app,
            "github:example/tool",
            "1.2.3",
            "example-tool",
            &std::collections::BTreeMap::new(),
        );

        let owners = installed_shim_owners(&app).unwrap();

        assert!(!owners.contains_key("example-tool"));
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

    #[test]
    fn stale_shim_cleanup_preserves_names_owned_by_another_backend() {
        let owners = std::collections::BTreeMap::from([
            (
                "shared".to_string(),
                std::collections::BTreeSet::from(["npm:old".to_string(), "npm:other".to_string()]),
            ),
            (
                "only-old".to_string(),
                std::collections::BTreeSet::from(["npm:old".to_string()]),
            ),
        ]);
        assert!(has_other_shim_owner(&owners, "shared", "npm:old"));
        assert!(!has_other_shim_owner(&owners, "only-old", "npm:old"));
    }

    #[test]
    fn installed_version_selection_is_scope_candidate_bounded() {
        let global = vec!["2.0.0".to_string(), "3.0.0".to_string()];
        assert_eq!(
            select_installed_version(
                "npm:fixture",
                &VersionSpec::Prefix("3".into()),
                global.clone(),
            )
            .unwrap(),
            "3.0.0"
        );
        assert!(select_installed_version(
            "npm:fixture",
            &VersionSpec::Exact("1.0.0".into()),
            global,
        )
        .is_err());
    }

    #[test]
    fn global_snapshot_restores_files_symlinks_and_absence() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("config.toml");
        let absent = temporary.path().join("new-shim");
        std::fs::write(&file, b"old").unwrap();
        let file_snapshot = GlobalNpmPathSnapshot::capture(file.clone()).unwrap();
        let absent_snapshot = GlobalNpmPathSnapshot::capture(absent.clone()).unwrap();

        std::fs::write(&file, b"new").unwrap();
        std::fs::write(&absent, b"generated").unwrap();
        file_snapshot.restore().unwrap();
        absent_snapshot.restore().unwrap();

        assert_eq!(std::fs::read(file).unwrap(), b"old");
        assert!(!absent.exists());
    }

    fn write_global_npm_uninstall_fixture(
        app: &App,
        version: &mut ToolVersion,
        bin_name: &str,
    ) -> std::path::PathBuf {
        let backend =
            osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend).unwrap();
        version
            .options
            .entry(osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into())
            .or_insert_with(|| "22.1.0".into());
        let root = backend.global_install_root_for(&app.ctx, version).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin").join(bin_name), b"fixture").unwrap();
        let identity = backend
            .install_identity(&app.ctx, version, osdk_core::npm_tools::ToolScope::Global)
            .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    #[test]
    fn interrupted_global_npm_uninstall_before_commit_restores_install() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let journal_path = transaction.path.clone();
        let backup = transaction.journal.roots[0].backup.clone();
        std::mem::forget(transaction);
        std::fs::rename(&root, &backup).unwrap();

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(root.is_dir());
        assert!(!backup.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert_eq!(config.global_tools["npm:fixture-cli"], "1.2.3");
    }

    #[test]
    fn committed_global_npm_uninstall_recovery_preserves_newer_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let journal_path = transaction.path.clone();
        let backup = transaction.journal.roots[0].backup.clone();
        std::fs::rename(&root, &backup).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);
        crate::config_edit::set_global_tool_unlocked(&app.ctx, &version.backend, "2.0.0").unwrap();

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(!root.exists());
        assert!(!backup.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert_eq!(config.global_tools["npm:fixture-cli"], "2.0.0");
    }

    #[test]
    fn global_npm_uninstall_journal_rejects_unsafe_backup_path() {
        let temporary = tempfile::tempdir().unwrap();
        let config =
            osdk_core::config::Config::load_user(&temporary.path().join("config/config.toml"))
                .unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        version.options.insert(
            osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "22.1.0".into(),
        );
        let backend =
            osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend).unwrap();
        let path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);
        let journal = GlobalNpmUninstallJournal {
            backend: version.backend.clone(),
            version: version.version.clone(),
            options: version.options.clone(),
            roots: vec![GlobalNpmUninstallRoot {
                original: backend.global_install_root_for(&app.ctx, &version).unwrap(),
                backup: temporary.path().join("outside"),
            }],
            bin_names: vec!["fixture-cli".into()],
            config_entry: None,
            lock_entry: None,
            committed: false,
        };
        write_global_npm_uninstall_journal(&path, &journal).unwrap();

        let error = recover_interrupted_global_npm_uninstalls(&app).unwrap_err();

        assert!(
            error.to_string().contains("unsafe backup path"),
            "{error:#}"
        );
        assert!(path.exists());
    }

    #[test]
    fn committed_global_npm_uninstall_recovery_removes_matching_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[tools]\nnode = \"20.0.0\"\n\"npm:fixture-cli\" = \"1.2.3\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let backup = transaction.journal.roots[0].backup.clone();
        std::fs::rename(&root, &backup).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert!(!config.global_tools.contains_key("npm:fixture-cli"));
        assert_eq!(config.global_tools["node"], "20.0.0");
        assert!(!root.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn panicking_after_global_npm_uninstall_commit_leaves_recoverable_journal() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let journal_path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut transaction = GlobalNpmUninstallTransaction::prepare(
                &app,
                &version,
                std::slice::from_ref(&root),
                &bins,
                true,
                false,
            )
            .unwrap();
            let backup = transaction.journal.roots[0].backup.clone();
            std::fs::rename(&root, backup).unwrap();
            transaction.mark_committed().unwrap();
            panic!("simulated abrupt termination");
        }));
        assert!(panic.is_err());
        assert!(!root.exists());
        assert!(journal_path.exists());

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(!root.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert!(!config.global_tools.contains_key("npm:fixture-cli"));
    }
}
