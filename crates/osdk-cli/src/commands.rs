//! Command handlers.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::source::select;
use osdk_core::t;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};

use crate::app::App;
use crate::cli::{ConfigCommand, SourceCommand};

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
    crate::lockfile::merge_resolved(&path, app.ctx.platform, &app.ctx.dirs, &resolved)?;
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
        let version = backend.resolve_version(&app.ctx, &request).await?;
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
    let tv = backend
        .resolve_version(&app.ctx, req)
        .await
        .with_context(|| format!("resolving {}@{}", req.backend, req.spec))?;

    if osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version) {
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

fn gather_requests(app: &App, tools: Vec<String>) -> Result<Vec<ToolRequest>> {
    if !tools.is_empty() {
        return tools
            .iter()
            .map(|s| ToolRequest::parse(s).map_err(|e| anyhow!("{e}")))
            .collect();
    }
    // From config pins.
    let mut out = Vec::new();
    for (tool, spec) in &app.ctx.config.tools {
        if app.registry.get(tool).is_ok() {
            out.push(ToolRequest {
                backend: tool.clone(),
                spec: VersionSpec::parse(spec),
                options: Default::default(),
            });
        }
    }
    Ok(out)
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
    backend.uninstall(&app.ctx, &tv).await?;
    println!("{}", t!("msg.uninstalled", tool = tv));
    // Reclaim now-unreferenced store objects.
    let (removed, bytes) = app.ctx.cas.gc(&app.ctx.dirs.installs)?;
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
            println!("selection    = {:?}", app.ctx.config.sources.selection);
            if !app.ctx.config.tools.is_empty() {
                println!("tools:");
                for (k, v) in &app.ctx.config.tools {
                    println!("  {k} = {v}");
                }
            }
        }
    }
    Ok(())
}

pub fn prune(app: &App, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("{}", t!("msg.prune_dry_run"));
        return Ok(());
    }
    let (removed, bytes) = app.ctx.cas.gc(&app.ctx.dirs.installs)?;
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
