//! Command handlers.

use anyhow::{anyhow, Context, Result};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::source::select;
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

pub async fn install(app: &mut App, tools: Vec<String>) -> Result<()> {
    let requests = gather_requests(app, tools)?;
    if requests.is_empty() {
        println!("nothing to install (no tools given and no config pins found)");
        return Ok(());
    }
    for req in requests {
        install_one(app, &req).await?;
    }
    Ok(())
}

/// Resolve, install, and shim a single request.
async fn install_one(app: &mut App, req: &ToolRequest) -> Result<ToolVersion> {
    apply_source_override(app, &req.backend);
    if app.refresh_sources {
        let backend = app.registry.get(&req.backend)?;
        let _ = select::refresh(&app.ctx, backend.as_ref()).await;
    }
    let backend = app.registry.get(&req.backend)?;
    let tv = backend
        .resolve_version(&app.ctx, req)
        .await
        .with_context(|| format!("resolving {}@{}", req.backend, req.spec))?;

    if osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version) {
        println!("{} already installed", tv);
    } else {
        println!("installing {} ...", tv);
        let ictx = InstallCtx { ctx: &app.ctx };
        backend
            .install(&ictx, &tv)
            .await
            .with_context(|| format!("installing {}", tv))?;
        println!("installed {}", tv);
    }
    generate_shims_for(app, backend.as_ref(), &tv)?;
    Ok(tv)
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
        None => app.registry.all().to_vec(),
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
        println!("no tools installed yet");
    }
    Ok(())
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
        println!("(no matching versions)");
    }
    Ok(())
}

pub async fn use_cmd(app: &mut App, tool: String, global: bool) -> Result<()> {
    let req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    let tv = install_one(app, &req).await?;
    // Write the pin.
    let spec = req.spec.to_string();
    if global {
        crate::config_edit::set_global_tool(&app.ctx, &tv.backend, &spec)?;
        println!("pinned {}@{} in user config", tv.backend, spec);
    } else {
        let path = crate::config_edit::set_project_tool(&tv.backend, &spec)?;
        println!("pinned {}@{} in {}", tv.backend, spec, path.display());
    }
    Ok(())
}

pub async fn uninstall(app: &App, tool: String) -> Result<()> {
    let req = ToolRequest::parse(&tool).map_err(|e| anyhow!("{e}"))?;
    let backend = app.registry.get(&req.backend)?;
    let version = match &req.spec {
        VersionSpec::Exact(v) => v.clone(),
        VersionSpec::Prefix(p) => {
            // pick the installed version matching the prefix
            let installed = backend.list_installed(&app.ctx)?;
            installed
                .into_iter()
                .rfind(|v| v.starts_with(p.as_str()))
                .ok_or_else(|| anyhow!("no installed {} version matches `{p}`", req.backend))?
        }
        other => {
            return Err(anyhow!(
                "specify an exact version to uninstall (got `{other}`)"
            ))
        }
    };
    let tv = ToolVersion::new(&req.backend, &version);
    backend.uninstall(&app.ctx, &tv).await?;
    println!("uninstalled {}", tv);
    // Reclaim now-unreferenced store objects.
    let (removed, bytes) = app.ctx.cas.gc(&app.ctx.dirs.installs)?;
    if removed > 0 {
        println!(
            "pruned {removed} store object(s), {} freed",
            human_bytes(bytes)
        );
    }
    Ok(())
}

pub fn current(app: &App, tool: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let backends: Vec<_> = match tool {
        Some(t) => vec![app.registry.get(&t)?],
        None => app.registry.all().to_vec(),
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
        println!("no active versions for this directory");
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
    println!("regenerated {total} shim(s)");
    Ok(())
}

/// Generate shims for all bin names a version exposes. Returns count.
fn generate_shims_for(app: &App, backend: &dyn Backend, tv: &ToolVersion) -> Result<usize> {
    let shim_bin = match osdk_core::shim::find_shim_binary(&app.ctx.dirs) {
        Some(b) => b,
        None => {
            // Not fatal: warn once. Activation via PATH still works.
            eprintln!("warning: osdk-shim binary not found; skipping shim generation");
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
                "sources for {} (selection: {:?}):",
                tool, app.ctx.config.sources.selection
            );
            for s in sources {
                let marker = if Some(&s.id) == pin.as_ref() {
                    " [pinned]"
                } else {
                    ""
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
            println!("probing sources for {} ...", tool);
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
                    println!("  -. {:12} unreachable", r.source_id);
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
            println!("added custom source {} for {}", id, tool);
        }
        SourceCommand::Remove { tool, id } => {
            let removed = crate::config_edit::remove_custom_source(&app.ctx, &tool, &id)?;
            if removed {
                println!("removed custom source {} from {}", id, tool);
            } else {
                println!("no custom source {} found for {}", id, tool);
            }
        }
        SourceCommand::Pin { tool, id } => {
            let backend = app.registry.get(&tool)?;
            let known = select::effective_sources(&app.ctx, backend.as_ref())
                .iter()
                .any(|s| s.id == id);
            if !known {
                return Err(anyhow!(
                    "unknown source {} for {} (see `osdk source list {}`)",
                    id,
                    tool,
                    tool
                ));
            }
            crate::config_edit::set_source_pin(&app.ctx, &tool, Some(&id))?;
            println!("pinned {} to source {}", tool, id);
        }
        SourceCommand::Unpin { tool } => {
            crate::config_edit::set_source_pin(&app.ctx, &tool, None)?;
            println!("unpinned {}", tool);
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
            println!("cleared downloaded archives (CAS store + installs kept)");
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
        println!("(dry-run) prune does not delete; run `osdk prune` to reclaim space");
        return Ok(());
    }
    let (removed, bytes) = app.ctx.cas.gc(&app.ctx.dirs.installs)?;
    println!("pruned {removed} object(s), {} freed", human_bytes(bytes));
    Ok(())
}

pub fn doctor(app: &App) -> Result<()> {
    use osdk_core::store::link::same_filesystem;
    println!("osdk doctor");
    println!("  platform     : {}", app.ctx.platform);
    println!("  data_dir     : {}", app.ctx.dirs.data.display());
    println!("  store_dir    : {}", app.ctx.dirs.store.display());
    println!("  install_dir  : {}", app.ctx.dirs.installs.display());
    let same = same_filesystem(&app.ctx.dirs.store, &app.ctx.dirs.installs);
    println!(
        "  store/install same filesystem: {} ({})",
        same,
        if same {
            "hardlinks OK"
        } else {
            "will fall back to copy"
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
