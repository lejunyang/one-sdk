//! Command handlers.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::source::select;
use osdk_core::t;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};

use crate::app::App;
use crate::cli::{
    AliasCommand, ConfigCommand, NodeCommand, PythonCommand, RustCommand, RustItemCommand,
    RustOverrideCommand, RustToolchainCommand, SourceCommand, TrustCommand,
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
    let resolved = resolve_requests(app, requests, opts).await?;
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
        paths.extend(backend.bin_paths(&app.ctx, version)?);
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
    let status = std::process::Command::new(program)
        .args(args)
        .envs(env)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        return Err(anyhow!("command exited with {status}"));
    }
    Ok(())
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

async fn install_requests(
    app: &mut App,
    requests: Vec<ToolRequest>,
    opts: Vec<String>,
) -> Result<Vec<(ToolRequest, ToolVersion)>> {
    let parsed_opts = parse_opts(&opts)?;
    let mut requests = requests;
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
    let jobs = app.ctx.config.settings.jobs.max(1);
    let installed = stream::iter(requests.into_iter().map(|req| {
        let app_ref: &App = app;
        async move {
            let installed = install_one_without_shims(app_ref, &req).await?;
            Ok::<_, anyhow::Error>((req, installed))
        }
    }))
    .buffer_unordered(jobs)
    .try_collect::<Vec<_>>()
    .await?;
    let mut resolved = Vec::with_capacity(installed.len());
    for (request, (backend, version)) in installed {
        generate_shims_for(app, backend.as_ref(), &version)?;
        resolved.push((request, version));
    }
    resolved.sort_by(|a, b| a.0.backend.cmp(&b.0.backend));
    Ok(resolved)
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
    let (backend, version) = install_one_without_shims(app, req).await?;
    generate_shims_for(app, backend.as_ref(), &version)?;
    Ok(version)
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

    if osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version) {
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
        let requests = tools
            .iter()
            .map(|s| ToolRequest::parse(s).map_err(|e| anyhow!("{e}")))
            .collect::<Result<Vec<_>>>()?;
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
                options: Default::default(),
            });
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
                backend: package_manager.manager,
                spec: VersionSpec::Exact(package_manager.version),
                options: Default::default(),
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
            options: Default::default(),
        });
    }
    inject_node_dependency(app, out)
}

fn inject_node_dependency(app: &App, mut requests: Vec<ToolRequest>) -> Result<Vec<ToolRequest>> {
    let has_package_manager = requests
        .iter()
        .any(|request| matches!(request.backend.as_str(), "npm" | "pnpm" | "yarn"));
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
        options: Default::default(),
    });
    Ok(requests)
}

pub fn list(app: &App, tool: Option<String>) -> Result<()> {
    let backends: Vec<_> = match tool {
        Some(t) => vec![app.registry.get(&t)?],
        None => all_display_backends(app),
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
/// dynamically-installed `github:owner/repo` backends found on disk.
fn all_display_backends(app: &App) -> Vec<std::sync::Arc<dyn Backend>> {
    let mut out: Vec<std::sync::Arc<dyn Backend>> = app.registry.all().to_vec();
    let base = app.ctx.dirs.installs.join("github");
    if let Ok(owners) = std::fs::read_dir(&base) {
        for owner in owners.flatten() {
            if !owner.path().is_dir() {
                continue;
            }
            let owner_name = owner.file_name().to_string_lossy().to_string();
            if let Ok(repos) = std::fs::read_dir(owner.path()) {
                for repo in repos.flatten() {
                    if repo.path().is_dir() {
                        let repo_name = repo.file_name().to_string_lossy().to_string();
                        let id = format!("github:{owner_name}/{repo_name}");
                        if let Ok(b) = app.registry.get(&id) {
                            out.push(b);
                        }
                    }
                }
            }
        }
    }
    out
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
    let mut req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    for (k, v) in parse_opts(&opts)? {
        req.options.insert(k, v);
    }
    let tv = install_one(app, &req).await?;
    // Pin the exact spec string the user typed (verbatim after `@`), so
    // channels like `stable` or `temurin-17` are preserved rather than being
    // normalized to `latest`. Bare `tool` (no `@`) pins the resolved version.
    let spec = match tool.split_once('@') {
        Some((_, v)) if !v.trim().is_empty() => v.trim().to_string(),
        _ => tv.version.clone(),
    };
    if global {
        crate::config_edit::set_global_tool(&app.ctx, &tv.backend, &spec)?;
        println!("{}", t!("msg.pinned_global", tool = tv.backend, ver = spec));
    } else {
        let path = crate::config_edit::set_project_tool(&tv.backend, &spec)?;
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
        None => all_display_backends(app),
    };
    let mut any = false;
    for backend in backends {
        if let Some(av) = osdk_core::version::resolver::resolve_active(
            backend.id(),
            &cwd,
            &app.ctx.config.tools,
            backend.idiomatic_files(),
        ) {
            any = true;
            println!(
                "{} {} ({})",
                backend.id(),
                av.spec,
                describe_origin(&av.source)
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
            let installed = backend.list_installed(&app.ctx)?;
            installed
                .into_iter()
                .last()
                .ok_or_else(|| anyhow!("{} is not installed", req.backend))?
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
    for backend in app.registry.all() {
        for version in backend.list_installed(&app.ctx)? {
            let tv = ToolVersion::new(backend.id(), &version);
            total += generate_shims_for(app, backend.as_ref(), &tv)?;
        }
    }
    println!("{}", t!("msg.reshimmed", count = total));
    Ok(())
}

/// Generate shims for all bin names a version exposes. Returns count.
fn generate_shims_for(app: &App, backend: &dyn Backend, tv: &ToolVersion) -> Result<usize> {
    let shim_bin = match osdk_core::shim::find_shim_binary(&app.ctx.dirs) {
        Some(b) => b,
        None => {
            // Not fatal: warn once. Activation via PATH still works.
            eprintln!("{}", t!("msg.shim_bin_missing"));
            return Ok(0);
        }
    };
    let names = backend.bin_names(&app.ctx, tv)?;
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
            let backend = app.registry.get(&tool)?;
            let sources = select::effective_sources(&app.ctx, backend.as_ref());
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
        SourceCommand::Test { tool } => {
            let backend = app.registry.get(&tool)?;
            println!("{}", t!("msg.probing", tool = tool));
            let mut ranked = select::refresh(&app.ctx, backend.as_ref()).await?;
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
        } => {
            app.registry.get(&tool)?; // validate known backend
            crate::config_edit::add_custom_source(
                &app.ctx,
                &tool,
                &id,
                &download_url,
                index_url.as_deref(),
            )?;
            println!("{}", t!("msg.source_added", id = id, tool = tool));
        }
        SourceCommand::Remove { tool, id } => {
            let removed = crate::config_edit::remove_custom_source(&app.ctx, &tool, &id)?;
            if removed {
                println!("{}", t!("msg.source_removed", id = id, tool = tool));
            } else {
                println!("{}", t!("msg.source_not_found", id = id, tool = tool));
            }
        }
        SourceCommand::Pin { tool, id } => {
            let backend = app.registry.get(&tool)?;
            let known = select::effective_sources(&app.ctx, backend.as_ref())
                .iter()
                .any(|s| s.id == id);
            if !known {
                return Err(anyhow!(t!("err.unknown_source", id = id, tool = tool)));
            }
            crate::config_edit::set_source_pin(&app.ctx, &tool, Some(&id))?;
            println!("{}", t!("msg.source_pinned", tool = tool, id = id));
        }
        SourceCommand::Unpin { tool } => {
            crate::config_edit::set_source_pin(&app.ctx, &tool, None)?;
            println!("{}", t!("msg.source_unpinned", tool = tool));
        }
    }
    Ok(())
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
