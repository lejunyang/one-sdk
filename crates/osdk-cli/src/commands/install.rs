//! `install` command handlers (split from commands.rs).

use super::*;

/// Apply a one-shot `--source` override into the config for this run.
pub(crate) fn apply_source_override(app: &mut App, tool: &str) {
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
pub(crate) fn opts_are_only_consent(opts: &[String]) -> bool {
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
    include_lazy: bool,
) -> Result<()> {
    let tools = tools
        .into_iter()
        .map(|tool| bind_explicit_operand_options(&tool, &opts))
        .collect::<Result<Vec<_>>>()?;
    let explicit = !tools.is_empty();
    // A bare tool name that no backend owns is worth explaining before anything
    // else happens: the operand is almost always a real package that simply needs
    // its namespace, and the alternatives are not interchangeable.
    for tool in &tools {
        report_bare_tool_name(app, tool).await?;
    }
    // Options normally mean the caller wants something the lock file does not
    // describe, so replay is skipped. Consent is the exception: it records a
    // decision rather than selecting an artifact, and is deliberately absent
    // from the lock, so requiring it must not make a committed lock file
    // impossible to install from.
    let use_lock = !explicit && opts_are_only_consent(&opts);
    let (mut requests, trusted_replay, supplemented_lock, inherit_cross_platform) = if use_lock {
        match requests_from_lock(app)? {
            Some(mut requests) => {
                warn_about_lock_drift(app, &requests);
                let supplemented = include_lazy
                    && supplement_locked_requests_with_lazy(
                        app,
                        &mut requests,
                        gather_install_requests(app, Vec::new())?,
                    )?;
                (requests, true, supplemented, false)
            }
            None => (gather_install_requests(app, tools)?, false, false, true),
        }
    } else {
        (gather_install_requests(app, tools)?, false, false, false)
    };
    if !explicit {
        requests = select_default_install_requests(app, requests, include_lazy);
    }
    let cwd = std::env::current_dir()?;
    let update_project_lock = !explicit
        && (!trusted_replay || supplemented_lock)
        && has_project_context(app, &requests, &cwd);
    let installed = install_requests_with_inheritance(
        app,
        requests,
        opts,
        trusted_replay,
        force,
        inherit_cross_platform,
    )
    .await?;
    if update_project_lock && !installed.is_empty() {
        let path = project_lock_path(app, &cwd);
        let target_platform = crate::lockfile::platform_for_resolved(app.ctx.platform, &installed);
        crate::lockfile::merge_resolved(&path, target_platform, &app.ctx.dirs, &installed)?;
        println!("updated {}", path.display());
    }
    Ok(())
}

/// Say so when a replayed lock no longer matches what the project declares.
///
/// A bare install replays the lock's tool table as-is and never compares it
/// with osdk.toml (see the lockfile guide); `osdk lock` is how the two are
/// reconciled. Nothing said that, so a tool removed from osdk.toml kept being
/// installed and reported as "already installed" -- it looked like a global
/// tool leaking into the project -- and a newly declared one was silently not
/// installed. The replay itself is unchanged; this only names the drift.
///
/// The expected set is what a fresh `osdk lock` would write: the project's own
/// declarations plus the runtimes osdk injects for them (node for npm tools, uv
/// for uv-installed pypi tools, ...). Lazy declarations are left out of the
/// missing side, because a lock written by a default install omits them.
fn warn_about_lock_drift(app: &App, locked: &[ToolRequest]) {
    let Ok(declared) = gather_install_requests(app, Vec::new()) else {
        return;
    };
    let eager: Vec<String> = declared
        .iter()
        .filter(|request| !configured_request_is_lazy(app, request))
        .map(|request| request.backend.clone())
        .collect();
    let Ok(expected) = inject_managed_dependencies(app, declared) else {
        return;
    };
    let expected: Vec<String> = expected
        .into_iter()
        .map(|request| request.backend)
        .collect();
    let locked: Vec<String> = locked
        .iter()
        .map(|request| request.backend.clone())
        .collect();
    let (stale, missing) = lock_drift(&locked, &expected, &eager);
    if !stale.is_empty() {
        eprintln!(
            "{}",
            t!("warn.lock_tools_not_declared", tools = stale.join(", "))
        );
    }
    if !missing.is_empty() {
        eprintln!(
            "{}",
            t!("warn.lock_tools_missing", tools = missing.join(", "))
        );
    }
}

/// Locked tools nothing expects, and eagerly declared tools the lock lacks.
pub(crate) fn lock_drift(
    locked: &[String],
    expected: &[String],
    eager: &[String],
) -> (Vec<String>, Vec<String>) {
    let stale = locked
        .iter()
        .filter(|backend| !expected.contains(backend))
        .cloned()
        .collect();
    let missing = eager
        .iter()
        .filter(|backend| !locked.contains(backend))
        .cloned()
        .collect();
    (stale, missing)
}

/// Add lazy declarations omitted by an earlier default install to a lock replay.
///
/// A first bare install can create a lock containing only eager tools. A later
/// `--include-lazy` must still be able to install the omitted entries without
/// discarding exact versions already present in that lock. Existing locked
/// backends win; only missing lazy backends are appended from configuration.
pub(crate) fn supplement_locked_requests_with_lazy(
    app: &App,
    locked: &mut Vec<ToolRequest>,
    configured: Vec<ToolRequest>,
) -> Result<bool> {
    let mut supplemented = false;
    for request in configured
        .into_iter()
        .filter(|request| configured_request_is_lazy(app, request))
    {
        if locked
            .iter()
            .any(|existing| existing.backend == request.backend)
        {
            continue;
        }
        reject_public_internal_options(&request.options)?;
        locked.push(request);
        supplemented = true;
    }
    Ok(supplemented)
}

/// Apply the no-argument install policy to an already resolved request list.
/// Explicit operands bypass this helper entirely.
pub(crate) fn select_default_install_requests(
    app: &App,
    requests: Vec<ToolRequest>,
    include_lazy: bool,
) -> Vec<ToolRequest> {
    if include_lazy {
        return requests;
    }
    requests
        .into_iter()
        .filter(|request| !configured_request_is_lazy(app, request))
        .collect()
}

/// Whether the active config entry that produced `request` is lazy.
///
/// Static tools normally use their backend id as the config key. Dynamic tools
/// can instead be declared through an arbitrary key whose value is the real
/// request (`formatter = "npm:prettier@3"`), so the fallback matches the parsed
/// backend rather than assuming the two keys are identical.
pub(crate) fn configured_request_is_lazy(app: &App, request: &ToolRequest) -> bool {
    if app
        .ctx
        .config
        .tool_configs
        .get(&request.backend)
        .is_some_and(osdk_core::config::ToolConfigEntry::is_lazy)
    {
        return true;
    }
    app.ctx.config.tool_configs.iter().any(|(key, entry)| {
        entry.is_lazy()
            && app
                .ctx
                .config
                .tools
                .get(key)
                .and_then(|value| ToolRequest::parse(value).ok())
                .is_some_and(|configured| configured.backend == request.backend)
    })
}

pub async fn lock(app: &mut App, tools: Vec<String>, opts: Vec<String>) -> Result<()> {
    let tools = tools
        .into_iter()
        .map(|tool| bind_explicit_operand_options(&tool, &opts))
        .collect::<Result<Vec<_>>>()?;
    let explicit = !tools.is_empty();
    let requests = gather_requests(app, tools)?;
    // Without operands the lock describes the project, so global pins are
    // dropped here rather than in `gather_requests`, which `install` / `exec` /
    // `outdated` share and where the global layer must keep applying.
    let requests = if explicit {
        requests
    } else {
        project_scoped_requests(app, requests)
    };
    let mut resolved = resolve_requests_with_inheritance(app, requests, opts, !explicit).await?;
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
    let explicit = !tools.is_empty();
    let requests = gather_requests(app, tools)?;
    let resolved = install_requests(app, requests, opts, false, false).await?;
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    // Upgrading installs every configured tool, global pins included -- that is
    // what the user asked for. Recording them is a separate question: the lock
    // belongs to the project, so only its own tools are written.
    let recorded = if explicit {
        resolved
    } else {
        resolved
            .into_iter()
            .filter(|(request, _)| project_owns_request(app, request))
            .collect()
    };
    crate::lockfile::merge_resolved(&path, app.ctx.platform, &app.ctx.dirs, &recorded)?;
    println!("updated {}", path.display());
    Ok(())
}

pub async fn exec_cmd(app: &mut App, tools: Vec<String>, command: Vec<String>) -> Result<()> {
    if tools.is_empty() {
        return exec_in_project_environment(app, &command);
    }
    if super::system_exec::try_exec(app, &tools, &command).await? {
        return Ok(());
    }
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
    // backend can describe another backend's install. A JAVA_HOME the *user*
    // set is a deliberate choice and is respected; one that osdk itself
    // exported from a previous activation is a stale snapshot, not an
    // instruction, so it is recomputed for the current directory.
    if resolved
        .iter()
        .any(|(_, version)| osdk_core::shim::requires_external_jdk(&version.backend))
        && !env.contains_key("JAVA_HOME")
        && !osdk_core::shim::process_java_home_is_user_owned()
    {
        let cwd = std::env::current_dir()?;
        if let Some((jdk_paths, jdk_env)) =
            osdk_core::execution::jdk_environment(&app.ctx, &app.registry, &cwd)?
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
    let hosts: Vec<_> = resolved
        .iter()
        .map(|(_, version)| version.backend.as_str())
        .collect();
    let managed_program =
        resolve_native_global_for_exec(app, &managed_program, &hosts, &managed_paths, &mut env)?
            .unwrap_or(managed_program);
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

/// `osdk exec -- <command>` without `--tool`: run under the directory's own
/// selection, exactly as an activated shell would.
///
/// This is the entry point for callers that have no shell hook -- an IDE task,
/// a CI step, a scheduled job. Nothing is installed: a selected tool that is
/// missing fails the way it does in an activated shell, instead of starting a
/// download the caller never asked for.
fn exec_in_project_environment(app: &App, command: &[String]) -> Result<()> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("exec requires a command"))?;
    let cwd = std::env::current_dir()?;
    let delta = super::maintenance::project_environment(app, &cwd)?;
    // Built on the PATH from before any outer activation, so a shell that
    // activated in another directory cannot leak that directory's tools in.
    let env = osdk_core::activate::env_assignments(&delta, &osdk_core::execution::ambient_path())?;
    let search: Vec<std::path::PathBuf> = std::env::split_paths(&env["PATH"]).collect();
    let program = resolve_program_in_dirs(std::path::Path::new(program), &search);
    let mut child = command_for_program(&program);
    // What an outer activation set and this directory does not: back to the
    // value it captured, as its own hook would restore it on `cd`.
    for key in &delta.unset_vars {
        match osdk_core::execution::original_env(key) {
            Some(value) => child.env(key, value),
            None => child.env_remove(key),
        };
    }
    let status = child
        .args(args)
        .envs(&env)
        .status()
        .with_context(|| format!("running {}", program.display()))?;
    if !status.success() {
        return Err(anyhow!("command exited with {status}"));
    }
    Ok(())
}

pub(crate) fn resolve_managed_launcher_alias(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
) -> Result<(std::path::PathBuf, Vec<String>)> {
    // Inventory names need not match target filenames (for example prettier ->
    // prettier.cjs). Select the validated, published command, not an arbitrary
    // sibling from the target's directory. Explicit paths remain caller-owned.
    if !program.contains(['/', '\\']) {
        let dynamic = resolved
            .iter()
            .filter(|(_, version)| version.backend.contains(':'));
        let mut selected = None;
        let report = if resolved
            .iter()
            .any(|(_, version)| version.backend.contains(':'))
        {
            Some(osdk_core::shim::scan_dynamic_installs(&app.ctx)?)
        } else {
            None
        };
        for (request, version) in dynamic {
            let install = osdk_core::shim::validated_dynamic_install(
                &app.ctx,
                report.as_ref().expect("dynamic scan"),
                request,
                &version.version,
            )?;
            if let Some(path) = install.executable(program) {
                if !osdk_core::shim::shim_is_enabled_for(
                    &app.ctx.config.settings.shims,
                    &version.backend,
                    program,
                    install.owns(program),
                ) {
                    continue;
                }
                if selected.replace(path).is_some() {
                    return Err(anyhow!(
                        "multiple requested tools publish `{program}`; select one tool"
                    ));
                }
            }
        }
        if let Some(path) = selected {
            return Ok((path, args.to_vec()));
        }
    }
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

/// Launch a known native global directly when this exec plan already supplies
/// its host. Re-entering a shim would discard explicit CLI runtime authority.
/// Explicit paths and managed tool/receipt commands retain priority.
pub(crate) fn resolve_native_global_for_exec(
    app: &App,
    program: &std::path::Path,
    hosts: &[&str],
    runtime_paths: &[std::path::PathBuf],
    env: &mut std::collections::BTreeMap<String, String>,
) -> Result<Option<std::path::PathBuf>> {
    let Some(name) = program.to_str().filter(|s| !s.contains(['/', '\\'])) else {
        return Ok(None);
    };
    if app.registry.get(name).is_ok() || find_managed_executable(runtime_paths, name).is_some() {
        return Ok(None);
    }
    for source in osdk_core::shim::global_bin_sources(&app.ctx) {
        if !source.names.iter().any(|n| n == name) {
            continue;
        }
        let Some(launcher) = find_managed_executable(std::slice::from_ref(&source.bin_dir), name)
        else {
            continue;
        };
        if let Some(host) = osdk_core::shim::native_global_runtime(&launcher, source.owner) {
            if !hosts.contains(&host) {
                return Ok(None);
            }
            let available = find_managed_executable(runtime_paths, host).is_some_and(|path| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    true
                }
            });
            if !available {
                anyhow::bail!("selected {host} runtime executable is missing or not executable");
            }
        }
        // Native sibling commands must see the same selected runtime too.
        let mut paths: Vec<_> = env
            .get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        paths.insert(runtime_paths.len().min(paths.len()), source.bin_dir);
        env.insert(
            "PATH".into(),
            std::env::join_paths(paths)?.to_string_lossy().into_owned(),
        );
        return Ok(Some(launcher));
    }
    Ok(None)
}

pub(crate) fn find_managed_executable(
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
pub(crate) fn resolve_program_in_dirs(
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

pub(crate) fn command_for_program(program: &std::path::Path) -> std::process::Command {
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

pub(crate) async fn apply_package_registry_plan(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
    env: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current dir for registry preflight")?;
    apply_package_registry_plan_at(app, resolved, program, args, env, &cwd).await
}

pub(crate) async fn apply_package_registry_plan_at(
    app: &App,
    resolved: &[(ToolRequest, ToolVersion)],
    program: &str,
    args: &[String],
    env: &mut std::collections::BTreeMap<String, String>,
    cwd: &std::path::Path,
) -> Result<()> {
    let executable_alias = executable_basename(program);
    let project_yarn_version = if matches!(executable_alias.as_str(), "yarn" | "yarnpkg")
        && !resolved
            .iter()
            .any(|(_, version)| version.backend == "yarn")
    {
        project_yarn_version(app, cwd)?
    } else {
        None
    };
    let yarn_version = resolved
        .iter()
        .find(|(_, version)| version.backend == "yarn")
        .map(|(_, version)| version.version.as_str())
        .or(project_yarn_version.as_deref());
    let evidence = osdk_core::package_sources::apply(
        &app.ctx,
        cwd,
        &executable_alias,
        yarn_version,
        args,
        env,
    )
    .await?;
    tracing::info!(%evidence, "package source resolution");
    Ok(())
}

pub(crate) fn executable_basename(program: &str) -> String {
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

pub(crate) fn project_yarn_version(app: &App, cwd: &std::path::Path) -> Result<Option<String>> {
    Ok(
        osdk_core::version::resolver::resolve_active_config("yarn", cwd, &app.ctx.config, &[])?
            .map(|request| request.spec),
    )
}

pub(crate) fn managed_runtime_path_priority(path: &std::path::Path) -> u8 {
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

pub(crate) async fn install_requests(
    app: &mut App,
    requests: Vec<ToolRequest>,
    opts: Vec<String>,
    trusted_replay: bool,
    force: bool,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    install_requests_with_inheritance(app, requests, opts, trusted_replay, force, false).await
}

pub(crate) async fn install_requests_with_inheritance(
    app: &mut App,
    mut requests: Vec<ToolRequest>,
    opts: Vec<String>,
    trusted_replay: bool,
    force: bool,
    inherit_cross_platform: bool,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    for request in &mut requests {
        if !trusted_replay {
            reject_public_internal_options(&request.options)?;
        }
        for (key, value) in &parsed_opts {
            merge_operand_option(&request.backend, &mut request.options, key, value)?;
        }
    }
    let mut requests = inject_managed_dependencies(app, requests)?;
    let inherited_specs = if inherit_cross_platform {
        inherit_versions_from_other_platforms(app, &mut requests)?
    } else {
        Default::default()
    };
    if requests.is_empty() {
        println!("{}", t!("msg.nothing_to_install"));
        return Ok(Vec::new());
    }
    // Warn once per distinct runtime tool about persistent env vars that
    // already exist (typically a standalone install left behind). Only tool
    // installs (no namespace prefix) are checked: namespaced packages such as
    // `npm:<pkg>` ride on a runtime whose own install already warned.
    let mut warned_tools = std::collections::BTreeSet::new();
    for req in &requests {
        if req.backend.contains(':') || !warned_tools.insert(req.backend.clone()) {
            continue;
        }
        for warning in
            osdk_core::persistent_env::install_warnings(&app.ctx.config.settings, &req.backend)
        {
            eprintln!("warning: {warning}");
        }
    }
    for req in &mut requests {
        apply_source_override(app, &req.backend);
    }
    // The compatibility install/exec/lock paths always target the isolated
    // npm package root, even when the merged config has a global selection for
    // the same package and version.
    mark_isolated_npm_scope(&mut requests);
    // Refuse the whole batch before installing anything if consent is missing.
    // Without this the requests run concurrently and a licence failure cancels
    // whatever else was in flight, throwing away work that had already
    // succeeded -- for a condition that was knowable before the first byte was
    // fetched.
    preflight_android_licenses(app, &requests).await?;
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
    let (go_requests, remaining_requests) =
        partition_runtime_dependency(remaining_requests, "go", "go:");
    for request in go_requests {
        let (backend, version) = install_one_without_shims(app, &request, force).await?;
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    // uv has to be installed before the pypi tools that were locked to it, for
    // the same reason node precedes npm packages: the tools below run
    // concurrently, so leaving uv in that batch means a tool can start while uv
    // is still being installed and fall back to pip. Measured before this split:
    // uv was injected correctly and the option reached the backend, yet every
    // tool still reported `creator: "stdlib"` -- the injection was right and the
    // ordering was wrong, which looks identical from the outside.
    let (uv_requests, mut remaining_requests) =
        partition_runtime_dependency(remaining_requests, "pypi:uv", "pypi:");
    for request in uv_requests {
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
    restore_inherited_request_specs(&mut resolved, inherited_specs)?;
    Ok(resolved)
}

/// Fail the batch up front when any Android request still needs consent.
///
/// The gate inside the Android backend already refuses to fetch bytes without
/// consent, so this adds no permission of its own; it only moves the *timing* of
/// an unavoidable failure to before the first install starts. That matters
/// because installs run concurrently: a late refusal cancels its siblings
/// mid-flight, so a user who forgot `-o accept-license` watched an unrelated
/// tool get discarded and had to fetch it again.
///
/// Only manifest reads happen here, and only for Android requests, so a batch
/// without Android tools pays nothing.
pub(crate) async fn preflight_android_licenses(app: &App, requests: &[ToolRequest]) -> Result<()> {
    use osdk_core::backend::android::{AndroidBackend, ID_PREFIX, SUPPORTED_FAMILIES};

    let mut blocked = Vec::new();
    for request in requests {
        if !AndroidBackend::owns_id(&request.backend) {
            continue;
        }
        let backend = app.registry.get(&request.backend)?;
        // Construct the concrete backend instead of widening the `Backend`
        // trait: a new trait method would enter the vtable and so be retained in
        // the shim, whose binary size is a budget guarded on purpose. This is the
        // approach the android doctor and licence commands already take.
        let Some(family) = request.backend.strip_prefix(ID_PREFIX) else {
            continue;
        };
        let Some(family) = SUPPORTED_FAMILIES
            .iter()
            .find(|candidate| **candidate == family)
        else {
            continue;
        };
        let android = AndroidBackend::new(family);
        let effective = expand_request_alias(app, backend.as_ref(), request)?;
        let mut tv = backend
            .resolve_version(&app.ctx, &effective)
            .await
            .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
        bind_dynamic_request_options(&effective, &mut tv);
        blocked.extend(android.pending_licenses(&app.ctx, &tv).await?);
    }
    if blocked.is_empty() {
        return Ok(());
    }
    // Merge per-request findings so one licence covering several packages is
    // reported once, the way the in-install gate reports it.
    let mut merged: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for entry in blocked {
        merged
            .entry(entry.license_id)
            .or_default()
            .extend(entry.packages);
    }
    let pending: Vec<osdk_core::android::license::PendingLicense> = merged
        .into_iter()
        .map(
            |(license_id, packages)| osdk_core::android::license::PendingLicense {
                license_id,
                packages: packages.into_iter().collect(),
            },
        )
        .collect();
    Err(osdk_core::android::license::blocked_error(&pending).into())
}

pub(crate) fn resolved_node_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "node").then_some(version.version.clone()))
}

pub(crate) fn resolved_rust_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "rust").then_some(version.version.clone()))
}

pub(crate) fn resolved_go_version(resolved: &[(ToolRequest, ToolVersion)]) -> Option<String> {
    resolved
        .iter()
        .find_map(|(_, version)| (version.backend == "go").then_some(version.version.clone()))
}

pub(crate) fn exact_rust_version(version: &str) -> bool {
    matches!(VersionSpec::parse(version), VersionSpec::Exact(exact) if exact == version)
}

pub(crate) fn require_exact_rust_spec(spec: &VersionSpec) -> Result<()> {
    if matches!(spec, VersionSpec::Exact(version) if exact_rust_version(version)) {
        return Ok(());
    }
    anyhow::bail!(
        "Cargo tools require one exact managed Rust version; configure `rust = \"1.91.1\"` or include `rust@1.91.1`"
    )
}

pub(crate) fn exact_go_version(version: &str) -> bool {
    matches!(VersionSpec::parse(version), VersionSpec::Exact(exact) if exact == version)
}

pub(crate) fn bind_request_node_version(
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

pub(crate) fn bind_resolved_node_version(resolved: &mut [(ToolRequest, ToolVersion)]) {
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

pub(crate) fn bind_resolved_rust_version(
    resolved: &mut [(ToolRequest, ToolVersion)],
) -> Result<()> {
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

pub(crate) fn bind_resolved_go_version(resolved: &mut [(ToolRequest, ToolVersion)]) -> Result<()> {
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

pub(crate) fn bind_request_rust_version(
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

pub(crate) fn bind_request_go_version(
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

pub(crate) fn partition_runtime_dependency(
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

pub(crate) fn mark_isolated_npm_scope(requests: &mut [ToolRequest]) {
    for request in requests {
        if request.backend.starts_with("npm:") {
            request.options.insert(
                osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
                osdk_core::npm_tools::ToolScope::Project.as_str().into(),
            );
        }
    }
}

pub(crate) async fn resolve_requests(
    app: &mut App,
    requests: Vec<ToolRequest>,
    opts: Vec<String>,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    resolve_requests_with_inheritance(app, requests, opts, false).await
}

pub(crate) async fn resolve_requests_with_inheritance(
    app: &mut App,
    mut requests: Vec<ToolRequest>,
    opts: Vec<String>,
    inherit_cross_platform: bool,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    for request in &mut requests {
        reject_public_internal_options(&request.options)?;
        for (key, value) in &parsed_opts {
            merge_operand_option(&request.backend, &mut request.options, key, value)?;
        }
    }
    let mut requests = inject_managed_dependencies(app, requests)?;
    let inherited_specs = if inherit_cross_platform {
        inherit_versions_from_other_platforms(app, &mut requests)?
    } else {
        Default::default()
    };
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
    restore_inherited_request_specs(&mut resolved, inherited_specs)?;
    Ok(resolved)
}

pub(crate) fn inherit_versions_from_other_platforms(
    app: &App,
    requests: &mut [ToolRequest],
) -> Result<std::collections::BTreeMap<String, VersionSpec>> {
    let cwd = std::env::current_dir()?;
    let Some(path) = existing_lock_path(app, &cwd) else {
        return Ok(Default::default());
    };
    let Some(inherited) =
        crate::lockfile::locked_versions_from_other_platforms(&path, app.ctx.platform, requests)?
    else {
        return Ok(Default::default());
    };
    Ok(apply_inherited_versions(requests, &inherited))
}

pub(crate) fn apply_inherited_versions(
    requests: &mut [ToolRequest],
    inherited: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, VersionSpec> {
    let mut original = std::collections::BTreeMap::new();
    for request in requests {
        let Some(version) = inherited.get(&request.backend) else {
            continue;
        };
        if request.spec.matches_version(version) {
            original.insert(request.backend.clone(), request.spec.clone());
            request.spec = VersionSpec::Exact(version.clone());
        }
    }
    original
}

pub(crate) fn restore_inherited_request_specs(
    resolved: &mut [(ToolRequest, ToolVersion)],
    original: std::collections::BTreeMap<String, VersionSpec>,
) -> Result<()> {
    for (request, version) in resolved {
        if let Some(spec) = original.get(&request.backend) {
            let inherited = match &request.spec {
                VersionSpec::Exact(inherited) => inherited,
                _ => unreachable!("cross-platform inheritance always uses an exact request"),
            };
            if version.version != *inherited {
                anyhow::bail!(
                    "cross-platform lock selected {}@{inherited}, but this platform resolved {}@{}",
                    request.backend,
                    request.backend,
                    version.version
                );
            }
            request.spec = spec.clone();
        }
    }
    Ok(())
}

pub(crate) fn requests_from_lock(app: &App) -> Result<Option<Vec<ToolRequest>>> {
    let cwd = std::env::current_dir()?;
    let Some(path) = existing_lock_path(app, &cwd) else {
        return Ok(None);
    };
    crate::lockfile::locked_requests(&path, app.ctx.platform)
}

/// The lock that belongs to the currently selected project configuration, when
/// one already exists. An external binding owns its lock exclusively -- locks
/// are never borrowed across sources; a local project keeps the historical
/// ancestor walk.
pub(crate) fn existing_lock_path(app: &App, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Some(ctx) = &app.ctx.config.project {
        // A selected project owns its lock at its own root/bundle -- never a
        // ancestor-walked lock that belongs to a different project.
        return ctx.lock_path.is_file().then(|| ctx.lock_path.clone());
    }
    crate::lockfile::find(cwd)
}

pub(crate) fn project_lock_path(app: &App, cwd: &std::path::Path) -> std::path::PathBuf {
    if let Some(ctx) = &app.ctx.config.project {
        return ctx.lock_path.clone();
    }
    app.ctx
        .config
        .project_config_path
        .as_ref()
        .and_then(|path| path.parent())
        .map(|directory| directory.join(crate::lockfile::LOCKFILE_NAME))
        .unwrap_or_else(|| crate::lockfile::default_path(cwd))
}

/// Bind CLI options before validating required dynamic options.
pub(crate) fn bind_explicit_operand_options(tool: &str, opts: &[String]) -> Result<String> {
    if opts.is_empty() {
        return Ok(tool.to_string());
    }
    // Required dynamic options must be present before ToolRequest validates the
    // expression. Keep fixed tools and configured aliases on their existing path.
    let mut parts = osdk_core::tool::ToolSpecParts::parse(tool)?;
    let id = osdk_core::tool::ToolId::parse(&parts.id)?;
    if !id.is_dynamic() {
        return Ok(tool.to_string());
    }
    for (key, value) in parse_opts(opts)? {
        merge_operand_option(&parts.id, &mut parts.options, &key, &value)?;
    }
    reject_public_internal_options(&parts.options)?;
    let options = osdk_core::tool::canonicalize_dynamic_options(&id, &parts.options)?;
    let bound = osdk_core::tool::ToolSpec {
        id,
        options,
        selector: parts.selector,
    }
    .to_string();
    ToolRequest::parse(&bound)?;
    Ok(bound)
}

fn merge_operand_option(
    backend: &str,
    options: &mut std::collections::BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<()> {
    let id = osdk_core::tool::ToolId::parse(backend)?;
    if let Some(schema) = id.schema() {
        if let Some(definition) = schema.options.definition(key) {
            options.retain(|name, _| {
                schema
                    .options
                    .definition(name)
                    .is_none_or(|existing| existing.canonical_name != definition.canonical_name)
            });
        }
    }
    options.insert(key.to_string(), value.to_string());
    Ok(())
}

/// Parse repeated `key=value` option strings into pairs.
pub(crate) fn parse_opts(opts: &[String]) -> Result<Vec<(String, String)>> {
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

pub(crate) fn reject_public_internal_options(
    options: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    if let Some(key) = options.keys().find(|key| key.starts_with("__osdk_")) {
        anyhow::bail!("internal option `{key}` cannot be set by the user");
    }
    Ok(())
}

/// A dynamic install that already satisfies the request, found without going
/// to the network.
///
/// `is_installed` cannot answer this for a dynamic tool: it looks for the
/// completion marker under `<tool>/<version>`, while a dynamic install lives
/// one level deeper, under `<tool>/<version>/<install_id>`. That mismatch is
/// why the fast path used to be switched off for every id containing a `:`,
/// which made `exec` reinstall an already-present tool on every single call.
///
/// Matching on the install id itself is not an option either: it is a
/// fingerprint over the resolved `materials` (archive digests and the like),
/// so computing it needs the very network round-trip we are trying to skip.
/// What can be compared locally is the pair that decides *which* install a
/// request wants -- the version, and the canonical identity options projected
/// from the request. Both are available offline.
pub(crate) fn installed_dynamic_match(
    app: &App,
    backend: &dyn Backend,
    request: &ToolRequest,
) -> Option<ToolVersion> {
    let tool_id = osdk_core::tool::ToolId::parse(backend.id()).ok()?;
    if !tool_id.is_dynamic() {
        return None;
    }
    // Tolerant: one damaged manifest elsewhere in the tree must not force a
    // reinstall of a tool that is sitting there intact.
    let report = osdk_core::inventory::scan_installs_for_tool(
        &app.ctx.dirs.installs,
        backend.id(),
        &osdk_core::inventory::ScanOptions::tolerant(),
    )
    .ok()?;
    if report.installs.is_empty() {
        return None;
    }

    let wanted_options = osdk_core::tool::dynamic_identity_options(&tool_id, &request.options)
        .ok()?
        .into_map();
    let platform = app.ctx.platform.to_string();

    let candidates: Vec<&osdk_core::inventory::InstalledDynamicTool> = report
        .installs
        .iter()
        .filter(|install| {
            let identity = &install.manifest.identity;
            identity.tool == backend.id()
                && identity.platform == platform
                && identity.material_options == wanted_options
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let versions: Vec<String> = candidates
        .iter()
        .map(|install| install.manifest.identity.version.clone())
        .collect();
    // Compare against the *expanded* spec: an alias such as `@lts` never
    // matches a directory name, so using the raw spec here would silently
    // fall through to the network path and leave the bug half-fixed.
    let selected = match &request.spec {
        VersionSpec::Exact(exact) => versions.iter().find(|value| *value == exact).cloned(),
        parsed => {
            let infos: Vec<_> = versions
                .iter()
                .map(osdk_core::version::VersionInfo::stable)
                .collect();
            osdk_core::version::select_version(parsed, &infos).map(|info| info.version.clone())
        }
    }?;

    let install = candidates
        .iter()
        .find(|install| install.manifest.identity.version == selected)?;
    // The scan happened a moment ago; confirm the directory and manifest are
    // still the same objects before letting them decide no install is needed.
    install.revalidate().ok()?;

    let mut version = ToolVersion::new(backend.id(), selected);
    bind_dynamic_request_options(request, &mut version);
    Some(version)
}

pub(crate) async fn install_one_without_shims(
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
    // Answer from disk before resolving. `resolve_version` goes to the network,
    // so checking after it would still pay the round-trip -- and would still
    // fail with no network, for a tool that is already installed.
    if !force && !app.refresh_sources {
        if let Some(installed) = installed_dynamic_match(app, backend.as_ref(), &effective) {
            backend.ensure_post_install(&app.ctx, &installed)?;
            println!("{}", t!("msg.already_installed", tool = installed));
            return Ok((backend, installed));
        }
    }
    let mut tv = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", req.backend, req.spec))?;
    bind_dynamic_request_options(&effective, &mut tv);

    // The `:` exclusion stays: `is_installed` looks under `<tool>/<version>`,
    // which is the parent of a dynamic install root, so for a dynamic tool it
    // would report "installed" from a directory that holds no completion
    // marker at all. Dynamic tools take the disk fast path above instead; what
    // reaches here is `--force` or `--refresh`, which must reinstall anyway.
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
            if let Some(identity) = backend.dynamic_install_identity(&app.ctx, &tv)? {
                osdk_core::pipeline::clear_dynamic_complete_marker(&app.ctx.dirs, identity).await?;
            } else {
                osdk_core::pipeline::clear_complete_marker(
                    &app.ctx.dirs,
                    backend.id(),
                    &tv.version,
                )?;
            }
        }
        println!("{}", t!("msg.installing", tool = tv));
        let ictx = InstallCtx { ctx: &app.ctx };
        backend
            .install(&ictx, &tv)
            .await
            .with_context(|| format!("installing {}", tv))?;
        // The tree just changed; a later read in this same command (an install
        // followed by shim generation, say) must not see the pre-install memo.
        app.invalidate_dynamic_scan();
        println!("{}", t!("msg.installed", tool = tv));
    }
    Ok((backend, tv))
}

pub(crate) fn expand_request_alias(
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

pub(crate) fn bind_dynamic_request_options(request: &ToolRequest, version: &mut ToolVersion) {
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
                .tool_aliases
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
                if let Some(aliases) = app.ctx.config.tool_aliases.get(backend.id()) {
                    for (name, version) in aliases {
                        println!("{} {} = {}", backend.id(), name, version);
                    }
                }
            } else {
                for (tool, aliases) in &app.ctx.config.tool_aliases {
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

/// Tool requests belonging to the current project rather than the user-global
/// defaults.
///
/// `osdk.lock` sits next to a project's own config and is committed with it, so
/// it must describe that project -- not whatever the machine that ran `lock`
/// happened to have pinned globally. `gather_requests` deliberately reads the
/// *merged* configuration because commands such as `exec` and `outdated` need
/// global defaults. Callers that materialize or record a project filter that
/// merged set through this function.
///
/// The provenance needed to tell the layers apart is already recorded in
/// `tool_origins`, which shell activation has been consulting all along. Only
/// entries the project itself contributed are kept: its config file and its
/// `.tool-versions`. Explicit operands are never filtered -- naming a tool is an
/// instruction, and `osdk lock java` still locks java.
///
/// Backends injected from project evidence rather than from a config layer
/// (`packageManager` in `package.json`, a discovered Node range) carry no origin
/// entry. They are project facts by construction, so an absent origin is kept.
pub(crate) fn project_scoped_requests(app: &App, requests: Vec<ToolRequest>) -> Vec<ToolRequest> {
    requests
        .into_iter()
        .filter(|request| project_owns_request(app, request))
        .collect()
}

/// Gather requests for `install`, keeping global defaults only when no project
/// context exists.
///
/// Tool installations are shared in the user-level osdk store, so a project
/// request still reuses an identical version already present there. What is
/// excluded here is an unrelated request contributed only by user config: a
/// project install must not enumerate every global pin just to report it already
/// installed.
pub(crate) fn gather_install_requests(app: &App, tools: Vec<String>) -> Result<Vec<ToolRequest>> {
    let explicit = !tools.is_empty();
    let requests = gather_requests(app, tools)?;
    if explicit {
        return Ok(requests);
    }
    let cwd = std::env::current_dir()?;
    Ok(scope_bare_install_requests(app, requests, &cwd))
}

pub(crate) fn scope_bare_install_requests(
    app: &App,
    requests: Vec<ToolRequest>,
    cwd: &std::path::Path,
) -> Vec<ToolRequest> {
    if has_project_context(app, &requests, cwd) {
        project_scoped_requests(app, requests)
    } else {
        requests
    }
}

/// Whether a dependency materialization belongs to a project rather than the
/// user-global defaults. A config or existing lock establishes the scope even
/// when it currently declares no tools; resolver evidence such as
/// `package.json` is represented by a request with no global origin.
pub(crate) fn has_project_context(
    app: &App,
    requests: &[ToolRequest],
    cwd: &std::path::Path,
) -> bool {
    app.ctx.config.project_config_path.is_some()
        || crate::lockfile::find(cwd).is_some()
        || requests
            .iter()
            .any(|request| project_owns_request(app, request))
}

/// Whether the project -- rather than the user-global config -- asked for this
/// tool. Keyed by the config key that produced the entry, which for a dynamic
/// backend may be an indirection alias (`tool.ni = "npm:@antfu/ni"`) rather than
/// the backend id itself.
pub(crate) fn project_owns_request(app: &App, request: &ToolRequest) -> bool {
    let origins = &app.ctx.config.tool_origins;
    let direct = origins.get(&request.backend);
    let via_alias = || {
        app.ctx.config.tools.iter().find_map(|(key, value)| {
            let canonical = osdk_core::inventory::canonical_dynamic_id(key).ok();
            let matches = ToolRequest::parse(value)
                .ok()
                .is_some_and(|parsed| parsed.backend == request.backend)
                || canonical.as_deref() == Some(request.backend.as_str());
            matches.then(|| origins.get(key)).flatten()
        })
    };
    match direct.or_else(via_alias) {
        Some(osdk_core::config::ToolConfigOrigin::GlobalConfig(_)) => false,
        Some(
            osdk_core::config::ToolConfigOrigin::ProjectConfig(_)
            | osdk_core::config::ToolConfigOrigin::ToolVersions(_),
        ) => true,
        // Discovered from project evidence, not from any config layer.
        None => true,
    }
}

/// Detect an operand that a shell split on an unquoted comma, and say so.
///
/// PowerShell treats `,` as an array separator even inside an argument, so
/// `osdk install npm:x[a=1,b=2]@3` without quotes arrives as two operands:
/// `npm:x[a=1` and `b=2]@3`. Each fragment is then a syntactically broken tool
/// expression, and the first fails as "unterminated option block" -- accurate for
/// the fragment, misleading for the user, whose brackets were balanced. bash and
/// zsh pass the comma through, so the same command works there, which makes the
/// failure look arbitrary.
///
/// The signal is specific: one operand opens a bracket it never closes, and a
/// later operand closes one it never opened. A genuinely mistyped single operand
/// produces no such pair and keeps the original message.
pub(crate) fn report_shell_split_operands(tools: &[String]) -> Result<()> {
    let brackets = |operand: &str| (operand.matches('[').count(), operand.matches(']').count());
    for (start, operand) in tools.iter().enumerate() {
        let (opens, closes) = brackets(operand);
        if opens <= closes {
            continue;
        }
        // Search for the closing half rather than assuming it is the very next
        // operand: a value may itself contain commas, so one expression can be
        // split into more than two fragments.
        let end = tools[start + 1..].iter().position(|later| {
            let (opens, closes) = brackets(later);
            closes > opens
        });
        if let Some(offset) = end {
            let end = start + 1 + offset;
            // Rejoin with the comma the shell consumed, so the message can show
            // the command that would have worked.
            let rejoined = tools[start..=end].join(",");
            return Err(anyhow::anyhow!(osdk_core::t!(
                "err.operand_split_by_shell",
                operand = rejoined
            )));
        }
    }
    Ok(())
}

pub(crate) fn gather_requests(app: &App, tools: Vec<String>) -> Result<Vec<ToolRequest>> {
    let requests = gather_requests_unchecked(app, tools.clone())?;
    if osdk_core::execution::enabled(&app.ctx) {
        let cwd = std::env::current_dir()?;
        for request in &requests {
            let explicit = tools.iter().any(|operand| {
                requested_spec_literal(operand).is_some()
                    && resolve_explicit_request(
                        app,
                        operand,
                        &app.ctx.config.tool_configs,
                        &app.ctx.config.tools,
                    )
                    .is_ok_and(|r| r.backend == request.backend)
            });
            if explicit {
                continue;
            }
            let backend = app.registry.get(&request.backend)?;
            if matches!(
                osdk_core::execution::resolve(&app.ctx, backend.as_ref(), &cwd)?.target,
                Some(osdk_core::execution::ExecutionTarget::System(_))
            ) {
                anyhow::bail!("{} selects an external system tool; use exec for that target or specify an explicit managed version for install/lock", request.backend);
            }
        }
    }
    Ok(requests)
}

/// The `[tools]` key an operand names when configuration excluded it here.
///
/// The selector cannot be split off at the first `@`: a scoped npm package
/// starts with one, so `npm:@scope/pkg@1` would be looked up as `npm:` and an
/// excluded entry would fall through to a real install. The tool parser knows
/// where the selector begins; the plain split stays as the fallback for
/// operands it rejects, such as an alias that names no backend.
fn platform_exclusion<'a>(
    excluded: &'a std::collections::BTreeMap<String, String>,
    operand: &str,
) -> Option<(&'a str, &'a str)> {
    let lookup = |name: &str| {
        excluded
            .get_key_value(name)
            .map(|(name, restriction)| (name.as_str(), restriction.as_str()))
    };
    let plain = operand.split_once('@').map_or(operand, |(name, _)| name);
    ToolRequest::parse(operand)
        .ok()
        .and_then(|request| lookup(&request.backend))
        .or_else(|| lookup(plain))
}

pub(crate) fn gather_requests_unchecked(app: &App, tools: Vec<String>) -> Result<Vec<ToolRequest>> {
    if !tools.is_empty() {
        // A shell that split one operand into several is worth naming before
        // anything else: the resulting fragments fail as "unterminated option
        // block", which sends the user to count brackets that were in fact
        // written correctly.
        report_shell_split_operands(&tools)?;

        // Naming a tool that configuration excluded on this platform must say
        // so. Falling through would either resolve it as though the filter were
        // absent -- installing what the config said not to -- or report an
        // unknown tool, sending the user to check their spelling instead of the
        // `os`/`arch` line that is working as intended.
        for operand in &tools {
            if let Some((name, restriction)) =
                platform_exclusion(&app.ctx.config.excluded_tools, operand)
            {
                return Err(anyhow::anyhow!(osdk_core::t!(
                    "err.tool_excluded_by_platform",
                    tool = name,
                    restriction = restriction
                )));
            }
        }
        let requests = tools
            .iter()
            .map(|s| {
                let mut request = resolve_explicit_request(
                    app,
                    s,
                    &app.ctx.config.tool_configs,
                    &app.ctx.config.tools,
                )?;
                // A bare `tool` operand asks for "the version this project
                // selected", not "the newest one published". `ToolRequest::parse`
                // cannot know that -- an absent selector parses to
                // `VersionSpec::Latest`, which is indistinguishable from an
                // explicit `tool@latest` -- so the configured spec is bound here,
                // where the original operand text is still available.
                bind_configured_spec_for_bare_operand(app, s, &mut request)?;
                Ok(request)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(requests);
    }
    // From config pins.
    let cwd = std::env::current_dir()?;
    let mut out = Vec::new();
    for (tool, spec) in &app.ctx.config.tools {
        if tool == "node" {
            continue;
        }
        if let Ok(backend) = app.registry.get(tool) {
            let active = osdk_core::version::resolver::resolve_active_config(
                tool,
                &cwd,
                &app.ctx.config,
                backend.idiomatic_files(),
            )?;
            let selected = active.as_ref().map(|a| a.spec.as_str()).unwrap_or(spec);
            out.push(ToolRequest {
                backend: tool.clone(),
                spec: if active.as_ref().is_some_and(|a| a.is_range) {
                    VersionSpec::parse_range(selected)?
                } else {
                    VersionSpec::parse(selected)
                },
                options: app
                    .ctx
                    .config
                    .tool_configs
                    .get(tool)
                    .map(|entry| entry.to_request_options())
                    .unwrap_or_default(),
            });
        } else if let Some(mut request) = ToolRequest::parse(spec).ok().filter(|request| {
            request.backend.contains(':') && app.registry.get(&request.backend).is_ok()
        }) {
            if let Some(entry) = app.ctx.config.tool_configs.get(tool) {
                request.options.extend(entry.to_request_options());
            }
            out.push(request);
        } else {
            // Neither a backend nor an alias for one: usually a typo in the key.
            // Skipping it without a word made a bare `install` report "no config
            // pins found" for a file that plainly has one.
            tracing::warn!(
                "{}",
                t!("warn.unresolved_tool_entry", tool = tool, spec = spec)
            );
        }
    }
    let cwd = std::env::current_dir()?;
    if let Some(package_manager) =
        osdk_core::version::resolver::resolve_package_manager_config(&cwd, &app.ctx.config)?
    {
        if out
            .iter()
            .all(|request| request.backend != package_manager.manager)
        {
            out.push(ToolRequest {
                backend: package_manager.manager.clone(),
                spec: VersionSpec::parse_range(&package_manager.version)
                    .unwrap_or_else(|_| VersionSpec::parse(&package_manager.version)),
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
    if let Some(active) = osdk_core::version::resolver::resolve_active_config(
        backend.id(),
        &cwd,
        &app.ctx.config,
        backend.idiomatic_files(),
    )? {
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

pub(crate) fn inherit_configured_options_from(
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

/// Bind a bare `tool` operand to the spec the active configuration selected.
///
/// `install` / `exec` / `lock` / `outdated` / `upgrade` all name their tools on
/// the command line, and an operand without `@` used to reach the backend as
/// `VersionSpec::Latest`. That silently ignored the project's own pin: in a
/// project pinning `java = "21.0.12.1+1"`, `osdk exec -t java -- ...` resolved
/// `latest` against the remote index and exported
/// `JAVA_HOME=<installs>/java/26.x`, so a Gradle build asking for
/// `languageVersion = 21` failed with "Cannot find a Java installation". The
/// same path sent `-t android-platforms` to `android-37.2` and
/// `-t android-system-images` to the newest preview image, in a project that
/// pinned neither.
///
/// The rule this restores is the one the resolution order already documents:
/// `tool` means "whatever is active here" and only `tool@<selector>` overrides
/// it. Detection is by the **operand text**, not by the parsed spec, because
/// `VersionSpec::Latest` cannot distinguish an absent selector from a literal
/// `@latest` -- and a user who typed `@latest` is asking for the newest release
/// on purpose.
///
/// Resolution reuses [`resolver::resolve_active`], so `.tool-versions`,
/// idiomatic version files and the global config keep their usual precedence
/// relative to `osdk.toml`; only the "nothing configured" case still falls
/// through to `latest`. A failure to parse a configured range is not fatal
/// either: the pre-existing behaviour is kept rather than turning a malformed
/// config value into a refusal to run.
pub(crate) fn bind_configured_spec_for_bare_operand(
    app: &App,
    operand: &str,
    request: &mut ToolRequest,
) -> Result<()> {
    // Ancestor discovery starts at the working directory, exactly as the shim
    // and `osdk current` do, so all three agree on what is active.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    bind_configured_spec_for_bare_operand_at(app, operand, request, &cwd)
}

/// As [`bind_configured_spec_for_bare_operand`], with the start directory given.
///
/// Split out so the contract can be tested without `set_current_dir`, which is
/// process-global and therefore races the rest of the test binary.
pub(crate) fn bind_configured_spec_for_bare_operand_at(
    app: &App,
    operand: &str,
    request: &mut ToolRequest,
    cwd: &std::path::Path,
) -> Result<()> {
    if requested_spec_literal(operand).is_some() {
        return Ok(());
    }
    let Ok(backend) = app.registry.get(&request.backend) else {
        return Ok(());
    };
    let Some(active) = osdk_core::version::resolver::resolve_active_config(
        backend.id(),
        cwd,
        &app.ctx.config,
        backend.idiomatic_files(),
    )?
    else {
        return Ok(());
    };
    let expanded = app
        .ctx
        .config
        .expand_alias(backend.id(), &active.spec)
        .unwrap_or(active.spec);
    let spec = if active.is_range {
        VersionSpec::parse_range(&expanded)?
    } else {
        VersionSpec::parse(&expanded)
    };
    request.spec = spec;
    Ok(())
}

/// Fail with a list of namespaces when a bare tool name owns no backend.
///
/// Only reached for an operand that names no backend and carries no namespace,
/// so an ordinary request pays nothing. A namespaced or known operand returns
/// immediately, and a name nothing provides falls through to the usual error
/// from the resolver rather than being reported twice.
pub(crate) async fn report_bare_tool_name(app: &App, operand: &str) -> Result<()> {
    // Strip any selector first: `uv@0.12.13` is the same ambiguity as `uv`.
    let name = operand
        .split('@')
        .next()
        .unwrap_or(operand)
        .split('[')
        .next()
        .unwrap_or(operand)
        .trim();
    if name.is_empty() || name.contains(':') {
        return Ok(());
    }
    if app.registry.get(name).is_ok() {
        return Ok(());
    }
    // Already configured under this key: the user has a spec for it, so the
    // request is not ambiguous even though the bare name is not a backend.
    if app.ctx.config.tool_configs.contains_key(name) || app.ctx.config.tools.contains_key(name) {
        return Ok(());
    }
    let candidates = osdk_core::backend_discovery::discover(&app.ctx, name).await;
    if candidates.is_empty() {
        return Ok(());
    }
    Err(unknown_backend_error(name, &candidates))
}

/// Fail with the namespaces that provide a bare tool name.
///
/// Discovery itself lives in `osdk_core::backend_discovery`, which walks the
/// registered dynamic namespaces rather than a list written out here. The first
/// version of this hard-coded PyPI and conda-forge, so a new backend simply did
/// not appear -- the list has to be derived from the backends or it goes stale
/// without anyone noticing.
pub(crate) fn unknown_backend_error(
    name: &str,
    candidates: &[osdk_core::backend_discovery::Candidate],
) -> anyhow::Error {
    use std::fmt::Write as _;

    if candidates.is_empty() {
        return anyhow!(
            "`{name}` is not a known backend, and no namespace was found that provides it"
        );
    }
    let mut message = format!(
        "`{name}` is not a backend on its own. These namespaces publish something by that name:"
    );
    for candidate in candidates {
        // Built line by line rather than with escapes inside one literal: a
        // `\n` written into a generated string once shipped as two literal
        // characters, and a `\` continuation leaked its indentation into the
        // output. Neither fails to compile.
        let _ = write!(message, "\n  osdk install {}", candidate.id);
        if let Some(version) = &candidate.version {
            let _ = write!(message, "    {version}");
        }
        // The description matters more than it looks. Probing `uv` finds npm's
        // unrelated `uv` at 1.4.0 next to the real one at 0.12.14, and
        // `pypi:ripgrep` is not BurntSushi's ripgrep. Printing ids and versions
        // alone would present unrelated programs as interchangeable sources,
        // which is a worse failure than not listing them at all.
        if let Some(summary) = &candidate.summary {
            let _ = write!(message, "\n      {summary}");
        }
        let _ = write!(message, "\n      {}", candidate.provenance.describe());
    }
    // Ordering is by provenance, so the first entry is the one osdk would lean
    // toward -- but it does not choose, and the caveat is the point: a shared
    // name is not evidence of a shared project.
    let _ = write!(
        message,
        "\n\nSame name does not mean same program -- compare the descriptions before choosing. Listed best-provenance first; osdk does not choose for you."
    );
    anyhow!(message)
}

pub(crate) fn resolve_explicit_request(
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

pub(crate) fn resolve_explicit_request_target(
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

pub(crate) fn apply_use_options(
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
        merge_operand_option(&request.backend, &mut request.options, &key, &value)?;
    }
    reject_public_internal_options(&request.options)?;
    Ok(())
}

pub(crate) fn inject_node_dependency(
    app: &App,
    mut requests: Vec<ToolRequest>,
) -> Result<Vec<ToolRequest>> {
    let has_package_manager = requests.iter().any(|request| {
        matches!(request.backend.as_str(), "npm" | "pnpm" | "yarn")
            || request.backend.starts_with("npm:")
    });
    if !has_package_manager || requests.iter().any(|request| request.backend == "node") {
        return Ok(requests);
    }
    let cwd = std::env::current_dir()?;
    let backend = app.registry.get("node")?;
    let spec = osdk_core::version::resolver::resolve_active_config(
        "node",
        &cwd,
        &app.ctx.config,
        backend.idiomatic_files(),
    )?
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

pub(crate) fn inject_managed_dependencies(
    app: &App,
    requests: Vec<ToolRequest>,
) -> Result<Vec<ToolRequest>> {
    let requests = inject_node_dependency(app, requests)?;
    let requests = inject_rust_dependency(app, requests)?;
    let requests = inject_go_dependency(app, requests)?;
    inject_uv_dependency(app, requests)
}

/// Add uv to the batch when a locked entry says uv installed it.
///
/// Without this, replaying a lockfile on a machine that has no uv silently used
/// pip instead: the entry recorded `installer = "uv"`, the install succeeded, and
/// the only sign was a notice suggesting uv might be nice to have. Verified
/// before the change on a fresh data directory -- `pypi:requests` locked with uv
/// replayed through pip and no uv was installed at all.
///
/// That is the failure a lockfile exists to prevent. uv and pip do not resolve
/// identically (uv publishes a list of deliberate deviations), so the same locked
/// version can pull different transitive dependencies depending on which one ran.
///
/// uv is installed as an ordinary `pypi:uv` request rather than fetched inline,
/// so it goes through the same index, checksum and receipt path as anything else,
/// and shows up in `osdk list` where the user can see it.
pub(crate) fn inject_uv_dependency(
    app: &App,
    mut requests: Vec<ToolRequest>,
) -> Result<Vec<ToolRequest>> {
    // Only act on entries that actually recorded uv. A plain `pypi:` request
    // with no locked installer keeps the existing behaviour: use uv when it is
    // there, fall back when it is not.
    let needs_uv = requests.iter().any(|request| {
        request.backend.starts_with("pypi:")
            && request
                .options
                .get(crate::lockfile::LOCKED_PYPI_INSTALLER_OPTION)
                .is_some_and(|installer| installer == "uv")
    });
    if !needs_uv {
        return Ok(requests);
    }
    // uv installing itself would be circular, and a batch that already asks for
    // uv needs no help.
    if requests.iter().any(|request| request.backend == "pypi:uv") {
        return Ok(requests);
    }
    // Already installed: nothing to add, and the backend will find it.
    if uv_is_installed(app) {
        return Ok(requests);
    }

    // Pinned to the recorded version when the lockfile carries one, because a
    // resolver's behaviour changes between releases -- replaying with "whatever
    // uv is newest" would reintroduce the drift on a smaller scale.
    let pinned = requests.iter().find_map(|request| {
        request
            .options
            .get(crate::lockfile::LOCKED_PYPI_UV_VERSION_OPTION)
            .cloned()
    });
    let spec = match pinned {
        Some(version) => format!("pypi:uv@{version}"),
        None => "pypi:uv".to_string(),
    };
    let uv_request = ToolRequest::parse(&spec).map_err(|error| anyhow!("{error}"))?;
    // Prepended so it is resolved before the tools that need it; the installer
    // ordering below keeps dependencies ahead of their dependents.
    requests.insert(0, uv_request);
    Ok(requests)
}

/// Whether osdk manages a usable uv already.
pub(crate) fn uv_is_installed(app: &App) -> bool {
    let root = app.ctx.dirs.data.join("installs").join("pypi").join("uv");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return false;
    };
    // A version directory alone is not proof: the install may have been
    // interrupted. Require a complete install below it.
    entries.flatten().any(|entry| {
        std::fs::read_dir(entry.path()).is_ok_and(|inner| {
            inner.flatten().any(|install| {
                install
                    .path()
                    .join(osdk_core::backend::pypi::ENV_RECEIPT_FILE)
                    .is_file()
            })
        })
    })
}

pub(crate) fn inject_go_dependency(
    app: &App,
    requests: Vec<ToolRequest>,
) -> Result<Vec<ToolRequest>> {
    let cwd = std::env::current_dir()?;
    inject_go_dependency_at(app, requests, &cwd)
}

pub(crate) fn inject_go_dependency_at(
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
    let active = osdk_core::version::resolver::resolve_active_config(
        "go",
        cwd,
        &app.ctx.config,
        backend.idiomatic_files(),
    )?
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

pub(crate) fn inject_rust_dependency(
    app: &App,
    requests: Vec<ToolRequest>,
) -> Result<Vec<ToolRequest>> {
    let cwd = std::env::current_dir()?;
    inject_rust_dependency_at(app, requests, &cwd)
}

pub(crate) fn inject_rust_dependency_at(
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
    let active = osdk_core::version::resolver::resolve_active_config(
        "rust",
        cwd,
        &app.ctx.config,
        backend.idiomatic_files(),
    )?
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

#[cfg(test)]
mod tests {
    use super::{lock_drift, platform_exclusion};

    #[test]
    fn lock_drift_names_removed_and_newly_declared_tools() {
        let strings = |items: &[&str]| {
            items
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        };
        // bun was dropped from osdk.toml; zig was added; node is injected for
        // the npm tool, so it is expected even though nobody declared it.
        let locked = strings(&["bun", "node", "npm:prettier", "pnpm"]);
        let expected = strings(&["node", "npm:prettier", "pnpm", "zig"]);
        let eager = strings(&["npm:prettier", "pnpm", "zig"]);
        let (stale, missing) = lock_drift(&locked, &expected, &eager);
        assert_eq!(stale, ["bun"]);
        assert_eq!(missing, ["zig"]);

        // In step: nothing to say.
        let (stale, missing) = lock_drift(&expected, &expected, &eager);
        assert!(stale.is_empty() && missing.is_empty());
    }
    use std::collections::BTreeMap;

    #[test]
    fn platform_exclusion_finds_scoped_npm_and_alias_operands() {
        let excluded = BTreeMap::from([
            ("npm:@scope/pkg".to_string(), "os=linux".to_string()),
            ("npm:cowsay".to_string(), "os=linux".to_string()),
            ("fd".to_string(), "os=macos".to_string()),
        ]);
        for operand in [
            "npm:@scope/pkg",
            "npm:@scope/pkg@1.0.0",
            "npm:@scope/pkg@latest",
        ] {
            assert_eq!(
                platform_exclusion(&excluded, operand),
                Some(("npm:@scope/pkg", "os=linux")),
                "{operand}"
            );
        }
        assert_eq!(
            platform_exclusion(&excluded, "npm:cowsay@1.6.0"),
            Some(("npm:cowsay", "os=linux"))
        );
        assert_eq!(
            platform_exclusion(&excluded, "fd@10"),
            Some(("fd", "os=macos"))
        );
        // Not excluded: a different scope, and the bare namespace a naive
        // first-`@` split used to produce.
        assert_eq!(platform_exclusion(&excluded, "npm:@other/pkg@1"), None);
        assert_eq!(platform_exclusion(&excluded, "node@20"), None);
    }
}
