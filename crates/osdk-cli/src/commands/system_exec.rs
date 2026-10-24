//! The opt-in execution path keeps external tools out of installation records.
use super::*;
use osdk_core::execution::{self, ExecutionTarget, SystemTarget};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn try_exec(
    app: &mut App,
    operands: &[String],
    command: &[String],
) -> Result<bool> {
    if !execution::enabled(&app.ctx) {
        return Ok(false);
    }
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("exec requires a command"))?;
    let cwd = std::env::current_dir()?;
    let mut requests = gather_requests_unchecked(app, operands.to_vec())?;
    // `exec -- node` may have no [tools] declaration at all. Only infer known
    // backend names here, never turn an arbitrary command into an install.
    if operands.is_empty() {
        if let Ok(backend) = app.registry.get(program) {
            if !requests.iter().any(|r| r.backend == backend.id()) {
                requests.push(ToolRequest::parse(backend.id())?);
            }
        }
    }
    let explicit: BTreeSet<_> = operands
        .iter()
        .filter(|s| requested_spec_literal(s).is_some())
        .map(|s| {
            resolve_explicit_request(app, s, &app.ctx.config.tool_configs, &app.ctx.config.tools)
                .map(|r| r.backend)
        })
        .collect::<Result<_>>()?;
    let mut systems = Vec::<SystemTarget>::new();
    let mut managed = Vec::new();
    for request in requests {
        let backend = app.registry.get(&request.backend)?;
        if !explicit.contains(&request.backend) {
            if let Some(ExecutionTarget::System(target)) =
                execution::resolve(&app.ctx, backend.as_ref(), &cwd)?.target
            {
                systems.push(target);
                continue;
            }
        }
        managed.push(request);
    }
    // A managed JS manager can execute with an external Node. Runtime identity
    // is selected before installation; no placeholder managed Node is invented.
    let needs_node = systems
        .iter()
        .any(|t| matches!(t.tool.as_str(), "npm" | "pnpm" | "yarn"))
        || managed
            .iter()
            .any(|r| matches!(r.backend.as_str(), "npm" | "pnpm" | "yarn"));
    if needs_node
        && !systems.iter().any(|t| t.tool == "node")
        && !managed.iter().any(|r| r.backend == "node")
    {
        let backend = app.registry.get("node")?;
        match execution::resolve(&app.ctx, backend.as_ref(), &cwd)?.target {
            Some(ExecutionTarget::System(target)) => systems.push(target),
            Some(ExecutionTarget::Managed(active)) => {
                managed.push(ToolRequest::parse(&format!("node@{}", active.spec))?)
            }
            None => managed.push(ToolRequest::parse("node")?),
        }
    }
    if systems.is_empty() {
        return Ok(false);
    }
    if managed.iter().any(|r| r.backend.contains(':')) {
        anyhow::bail!("mixed system execution with dynamic package tools is unsupported; use explicit managed runtime versions for their recorded installation contracts");
    }
    // Verify standalone runtimes before installation. Manager version probes
    // run below once their selected Node runtime is available.
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        execution::path_with(
            &app.ctx,
            systems
                .iter()
                .filter(|t| t.tool == "node")
                .chain(systems.iter().filter(|t| t.tool != "node"))
                .map(|t| t.bin_dir().to_path_buf()),
        )?,
    );
    for target in &mut systems {
        if !matches!(target.tool.as_str(), "npm" | "pnpm" | "yarn") {
            target.verify(&app.ctx, &env)?;
        }
    }
    let mut resolved = Vec::new();
    for request in managed {
        let (backend, version) = install_one_without_shims(app, &request, false).await?;
        generate_shims_including_dependencies(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    let mut bins = Vec::new();
    for (request, version) in &resolved {
        let backend = app.registry.get(&version.backend)?;
        bins.extend(managed_bin_paths(
            &app.ctx,
            backend.as_ref(),
            version,
            Some(request),
        )?);
        env.extend(backend.exec_env(&app.ctx, version)?);
    }
    if resolved
        .iter()
        .any(|(_, version)| osdk_core::shim::requires_external_jdk(&version.backend))
        && !env.contains_key("JAVA_HOME")
        && !osdk_core::shim::process_java_home_is_user_owned()
    {
        if let Some((jdk_bins, jdk_env)) =
            execution::jdk_environment(&app.ctx, &app.registry, &cwd)?
        {
            bins.extend(jdk_bins);
            env.extend(jdk_env);
        }
    }
    // Managed directories still precede ambient directories; system Node is
    // the only Node here unless an explicit CLI Node request took authority.
    bins.extend(
        systems
            .iter()
            .filter(|t| t.tool == "node")
            .chain(systems.iter().filter(|t| t.tool != "node"))
            .map(|t| t.bin_dir().to_path_buf()),
    );
    env.insert("PATH".into(), execution::path_with(&app.ctx, bins.clone())?);
    for target in &mut systems {
        if matches!(target.tool.as_str(), "npm" | "pnpm" | "yarn") {
            target.verify(&app.ctx, &env)?;
        }
    }
    let name = executable_basename(program);
    let mut target_program = None;
    let owner = osdk_core::shim::package_manager_backend_for_command(&name).unwrap_or(&name);
    let mut system_args = None;
    if std::path::Path::new(program).components().count() == 1
        && !resolved.iter().any(|(_, version)| version.backend == owner)
    {
        // Match only explicitly requested tool/companion names, not arbitrary
        // executables that happen to share the system directory.
        let mut candidates: Vec<_> = systems.iter().collect();
        candidates.sort_by_key(|target| target.tool != owner);
        for target in candidates {
            if target.command_names().contains(&name.as_str()) {
                let (command_name, prefix) = match (target.tool.as_str(), name.as_str()) {
                    ("pnpm", "pnpx") => ("pnpm", Some("dlx")),
                    ("bun", "bunx") => ("bun", Some("x")),
                    _ => (name.as_str(), None),
                };
                target_program = Some(target.command(&app.ctx, command_name)?);
                system_args = Some(
                    prefix
                        .into_iter()
                        .map(str::to_string)
                        .chain(args.iter().cloned())
                        .collect::<Vec<_>>(),
                );
                break;
            }
        }
    }
    let (executable, final_args) = if let Some(path) = target_program {
        (path, system_args.expect("system command arguments"))
    } else {
        let (managed_program, managed_args) =
            resolve_managed_launcher_alias(app, &resolved, program, args)?;
        let owner_dirs = resolved
            .iter()
            .find(|(_, version)| version.backend == owner)
            .map(|(request, version)| {
                let backend = app.registry.get(owner)?;
                managed_bin_paths(&app.ctx, backend.as_ref(), version, Some(request))
            })
            .transpose()?;
        let path = if managed_program.components().count() == 1 {
            owner_dirs
                .as_deref()
                .and_then(|dirs| find_managed_executable(dirs, &name))
                .unwrap_or_else(|| resolve_program_in_dirs(&managed_program, &bins))
        } else {
            managed_program
        };
        (path, managed_args)
    };
    let yarn_version = systems
        .iter()
        .find(|t| t.tool == "yarn")
        .and_then(|t| t.version.as_deref())
        .or_else(|| {
            resolved
                .iter()
                .find(|(_, t)| t.backend == "yarn")
                .map(|(_, t)| t.version.as_str())
        });
    osdk_core::package_sources::apply(&app.ctx, &cwd, &name, yarn_version, args, &mut env).await?;
    // Each adapter restores its own ambient locator, then explicit target env
    // wins. Never call a managed backend's exec_env for an external target.
    let mut child_env: BTreeMap<_, _> = std::env::vars_os().collect();
    for target in &systems {
        target.restore_environment(&mut child_env);
    }
    child_env.extend(
        env.into_iter()
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))),
    );
    for target in &systems {
        child_env.extend(
            target
                .env
                .iter()
                .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))),
        );
    }
    let status = command_for_program(&executable)
        .args(final_args)
        .env_clear()
        .envs(child_env)
        .status()
        .with_context(|| format!("running {}", executable.display()))?;
    if !status.success() {
        anyhow::bail!("command exited with {status}");
    }
    Ok(true)
}
