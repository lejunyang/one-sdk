//! `maintenance` command handlers (split from commands.rs).

use super::*;

/// `osdk self ...`: operations on the osdk installation itself.
pub async fn self_command(app: &mut App, command: crate::cli::SelfCommand) -> Result<()> {
    match command {
        crate::cli::SelfCommand::Upgrade {
            version,
            dry_run,
            force,
        } => self_upgrade(app, version, dry_run, force).await,
    }
}

pub(crate) async fn self_upgrade(
    app: &mut App,
    version: Option<String>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    use osdk_core::self_update;

    // Locate the installation before anything is downloaded: an osdk that
    // cannot find its own directory (a deleted or relocated binary) must say so
    // rather than fetch a release it has nowhere to put.
    let install_dir = self_update::install_dir()?;
    // Windows cannot delete the running program, so a previous upgrade may have
    // left the file it replaced behind. It is no longer running now.
    self_update::clean_replaced_programs(&install_dir);

    apply_source_override(app, self_update::SOURCE_ID);
    if app.refresh_sources {
        self_update::refresh_sources(&app.ctx).await?;
    }
    // The same speed probe tool downloads use, so a user behind a slow route to
    // github.com upgrades through the ranked mirror instead of timing out.
    let sources = self_update::ranked_sources(&app.ctx).await?;
    tracing::info!(
        source = %sources.first().map(|source| source.id.as_str()).unwrap_or("none"),
        "selected source for the osdk upgrade"
    );

    let target = self_update::resolve_target(&app.ctx, version.as_deref(), &sources).await?;
    let current = self_update::CURRENT_VERSION;
    println!(
        "{}",
        t!(
            "msg.self_versions",
            current = current,
            available = target.version
        )
    );
    if let Some(source) = sources.first() {
        println!(
            "{}",
            t!("msg.self_source", id = source.id, url = source.download_url)
        );
    }

    let same_version = target.version == current;
    // An explicit `--version` is a deliberate choice, including a downgrade, so
    // only the implicit "latest" path treats "not newer" as nothing to do.
    let nothing_to_do = if version.is_some() {
        same_version
    } else {
        !self_update::is_newer(&target.version, current)
    };
    if nothing_to_do && !force {
        println!("{}", t!("msg.self_up_to_date", version = current));
        return Ok(());
    }
    if dry_run {
        println!("{}", t!("msg.self_dry_run", version = target.version));
        return Ok(());
    }

    println!("{}", t!("msg.self_upgrading", version = target.version));
    let staged = self_update::stage_release(&app.ctx, &target).await?;
    if !staged.checksum_verified() {
        println!("{}", t!("msg.self_checksum_missing", file = target.asset));
    }
    let replaced = self_update::install(&staged, &install_dir)?;

    println!(
        "{}",
        t!(
            "msg.self_upgraded",
            version = target.version,
            dir = install_dir.display()
        )
    );
    println!(
        "{}",
        t!("msg.self_programs", programs = replaced.join(", "))
    );
    Ok(())
}

pub async fn source(app: &mut App, command: SourceCommand) -> Result<()> {
    match command {
        SourceCommand::List { tool } => {
            let tool = canonical_source_tool(app, &tool)?;
            let sources = if tool == osdk_core::self_update::SOURCE_ID {
                osdk_core::self_update::effective_sources(&app.ctx)
            } else if tool == osdk_core::backend::go::GO_MODULE_PROXY_TOOL {
                osdk_core::backend::go::module_proxy_sources(&app.ctx)
            } else if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
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
            let mut ranked = if tool == osdk_core::self_update::SOURCE_ID {
                if model.is_some() {
                    return Err(anyhow!("--model is only valid for model providers"));
                }
                osdk_core::self_update::refresh_sources(&app.ctx).await?
            } else if tool == osdk_core::backend::go::GO_MODULE_PROXY_TOOL {
                if model.is_some() {
                    return Err(anyhow!("--model is only valid for model providers"));
                }
                osdk_core::backend::go::refresh_module_proxy_sources(&app.ctx).await?
            } else if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
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
            let known = if tool == osdk_core::self_update::SOURCE_ID {
                osdk_core::self_update::effective_sources(&app.ctx)
                    .iter()
                    .any(|source| source.id == id)
            } else if tool == osdk_core::backend::go::GO_MODULE_PROXY_TOOL {
                osdk_core::backend::go::module_proxy_sources(&app.ctx)
                    .iter()
                    .any(|source| source.id == id)
            } else if let Ok(provider) = tool.parse::<osdk_core::model::ProviderId>() {
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

pub(crate) fn canonical_source_tool(app: &App, tool: &str) -> Result<String> {
    // `self` is osdk's own release download. It is not a backend, so the
    // registry cannot canonicalize it, but it does carry per-tool source
    // configuration and must stay reachable from `osdk source ...`.
    if tool == osdk_core::self_update::SOURCE_ID {
        return Ok(tool.to_string());
    }
    // `go-modules` is the GOPROXY the go command uses for module downloads. It
    // is not a backend either -- `[sources.go]` selects the toolchain archive
    // host, which is a different service -- but it carries the same per-tool
    // source configuration and must be reachable from `osdk source ...`.
    if tool == osdk_core::backend::go::GO_MODULE_PROXY_TOOL {
        return Ok(tool.to_string());
    }
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

/// Compute the environment a task runs with.
///
/// This is deliberately the same three layers `hook-env` renders into the shell
/// (activation delta, package caches, model providers) rather than a reduced
/// set. osdk does **not** launch tasks with `-NoProfile` the way mise does: its
/// shims sit on the persistent PATH, but `JAVA_HOME`/`GOROOT`-style exports come
/// from the profile hook, so suppressing the profile would quietly drop them and
/// hide any tool that `ShimSettings` excluded from shim generation. Injecting
/// the environment here gets both halves: the task sees what it declared, and no
/// stale outer activation can shadow it.
pub(crate) fn task_environment(
    app: &App,
    cwd: &std::path::Path,
) -> Result<osdk_core::tasks::runner::TaskEnv> {
    let mut delta = osdk_core::activate::compute_env_delta(&app.ctx, &app.registry, cwd)?;
    let cache_vars = osdk_core::cache::cache_env(&app.ctx.dirs.cache, |k| std::env::var(k).ok());
    delta.set_vars.extend(cache_vars);
    let model_vars = osdk_core::model::env::configured_env(&app.ctx, |key| std::env::var(key).ok());
    delta.set_vars.extend(model_vars);
    Ok(osdk_core::tasks::runner::TaskEnv {
        path_prepend: delta.path_prepend,
        set_vars: delta.set_vars,
    })
}

/// Where per-project freshness state lives.
///
/// Under the managed cache rather than beside `osdk.toml`: it is derived data,
/// not something a user edits or commits, and writing into the project would
/// mean every consumer needs a `.gitignore` entry. Keyed by the config path so
/// two projects cannot collide.
pub(crate) fn freshness_state_path(app: &App) -> std::path::PathBuf {
    osdk_core::tasks::freshness::state_path(
        &app.ctx.dirs.cache,
        app.ctx.config.project_config_path.as_deref(),
    )
}

/// Directory that task-relative paths resolve against.
///
/// The config file's own directory, not the shell's cwd: a task means the same
/// thing wherever it is invoked from.
pub(crate) fn task_config_root(app: &App) -> std::path::PathBuf {
    app.ctx
        .config
        .project_config_path
        .as_ref()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
}

pub fn run_task(
    app: &mut App,
    task: String,
    dry_run: bool,
    args: Vec<String>,
) -> Result<Option<std::process::ExitStatus>> {
    use osdk_core::tasks::runner;

    let set = &app.ctx.config.tasks;
    let plan = runner::plan(set, &task)?;

    // Parse against the named task's declaration, before anything runs: a bad
    // `choices` value or a missing required argument should not surface halfway
    // through a pipeline that already had effects.
    let resolved = set
        .resolve(&task)
        .ok_or_else(|| anyhow!("unknown task `{task}`"))?
        .to_string();
    let spec = set.tasks[&resolved].spec.clone();
    let has_spec = !spec.is_empty();
    let values = osdk_core::tasks::args::parse(&resolved, &spec, &args)?;

    let config_root = task_config_root(app);

    if dry_run {
        // Showing the freshness verdict is the point of --dry-run for an
        // incremental task: "what would run" and "why" are the same question.
        let state = osdk_core::tasks::freshness::FreshnessState::load(&freshness_state_path(app))?;
        // Substitute arguments first. A preview that still shows `{{env}}`
        // describes the template, not the command -- and the whole purpose of
        // --dry-run is seeing what will actually happen.
        let previewed = runner::resolve_for_preview(&plan, &resolved, &values, has_spec)?;
        for planned in previewed.steps.iter().filter(|step| step.standalone) {
            let verdict = osdk_core::tasks::freshness::decide(
                &osdk_core::tasks::freshness::Inputs {
                    root: &config_root,
                    name: &planned.name,
                    sources: &planned.sources,
                    outputs: &planned.outputs,
                    freshness: planned.freshness,
                    definition: &planned.definition,
                },
                &state,
            )?;
            match verdict.reason() {
                Some(reason) => println!("{}:  ({reason})", planned.name),
                None => {
                    println!("{}:  (up to date, would be skipped)", planned.name);
                    continue;
                }
            }
            for step in &planned.commands {
                match step {
                    runner::PlannedStep::Command {
                        command,
                        ignore_error,
                        ..
                    } => {
                        let suffix = if *ignore_error {
                            osdk_core::i18n::tr("task.failure_ignored")
                        } else {
                            String::new()
                        };
                        println!("  $ {command}{suffix}");
                    }
                    runner::PlannedStep::Argv { argv, ignore_error } => {
                        let suffix = if *ignore_error {
                            osdk_core::i18n::tr("task.failure_ignored")
                        } else {
                            String::new()
                        };
                        // Quote entries containing spaces so the preview shows
                        // argument boundaries, which is the point of argv.
                        let rendered: Vec<String> = argv
                            .iter()
                            .map(|entry| {
                                if entry.contains(char::is_whitespace) {
                                    format!("{entry:?}")
                                } else {
                                    entry.clone()
                                }
                            })
                            .collect();
                        println!("  > {}{suffix}", rendered.join(" "));
                    }
                    #[cfg(feature = "scripts")]
                    runner::PlannedStep::Lua { source } => {
                        // Show the first line plus a count: a preview that
                        // hides the script entirely would not say what runs,
                        // and dumping 40 lines would bury the rest of the plan.
                        let lines = source.lines().count();
                        let first = source.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        println!("  lua ({lines} lines): {}", first.trim());
                    }
                    runner::PlannedStep::Parallel { tasks } => {
                        println!("  || {}", tasks.join(", "));
                    }
                }
            }
        }
        return Ok(None);
    }

    let cwd = std::env::current_dir()?;
    let env = task_environment(app, &cwd)?;
    let base_path = std::env::var("PATH").unwrap_or_default();
    let defs = app.ctx.config.tasks.tasks.clone();

    let mut spawner = runner::ProcessSpawner {
        env,
        defs,
        base_path,
        arg_env: values.env_vars(),
        // Set per task by the runner; this is just the initial value.
        timeout: None,
        project_root: config_root.clone(),
    };

    let state_path = freshness_state_path(app);
    let mut state = osdk_core::tasks::freshness::FreshnessState::load(&state_path)?;
    let outcomes = runner::execute_full(
        &plan,
        &config_root,
        &mut spawner,
        &mut Some(&mut state),
        &values,
        has_spec,
    )?;
    // Persist even on failure: the tasks that did succeed earlier in the plan
    // recorded themselves, and discarding that would make the next run redo
    // work that is genuinely up to date.
    state.save(&state_path)?;

    for outcome in &outcomes {
        if outcome.skipped {
            eprintln!(
                "{}: {}",
                outcome.name,
                osdk_core::i18n::tr("task.up_to_date")
            );
        }
    }

    for outcome in &outcomes {
        for tolerated in &outcome.tolerated_failures {
            eprintln!(
                "{}: task `{}` continued past a failing step: {tolerated}",
                osdk_core::i18n::tr("label.warning"),
                outcome.name
            );
        }
    }

    // Propagate the failing task's code so `osdk run` composes in a shell.
    if let Some(failed) = outcomes.iter().find(|outcome| outcome.code != 0) {
        eprintln!(
            "{}: task `{}` failed with exit code {}",
            osdk_core::i18n::tr("label.error"),
            failed.name,
            failed.code
        );
        std::process::exit(failed.code);
    }
    Ok(None)
}

pub fn task(app: &mut App, command: crate::cli::TaskCommand) -> Result<()> {
    use crate::cli::TaskCommand;
    let set = &app.ctx.config.tasks;

    match command {
        TaskCommand::List { hidden } => {
            if set.tasks.is_empty() && set.excluded.is_empty() {
                println!("{}", osdk_core::i18n::tr("task.no_tasks"));
                return Ok(());
            }
            for (name, def) in &set.tasks {
                if def.hide && !hidden {
                    continue;
                }
                let description = def.description.clone().unwrap_or_default();
                let aliases: Vec<&str> = set
                    .aliases
                    .iter()
                    .filter(|(_, target)| *target == name)
                    .map(|(alias, _)| alias.as_str())
                    .collect();
                let alias_note = if aliases.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  ({}{})",
                        osdk_core::i18n::tr("task.alias_prefix"),
                        aliases.join(", ")
                    )
                };
                // A script Windows cannot launch is listed with the reason
                // rather than omitted: silently dropping it reads as a typo at
                // the call site, and the fix (add an extension or a shebang) is
                // not guessable from an absence.
                let blocked = if def.windows_invisible && cfg!(windows) {
                    osdk_core::i18n::tr("task.windows_invisible")
                } else {
                    String::new()
                };
                println!("{name:<24} {description}{alias_note}{blocked}");
            }
            // A task hidden by its platform filter is not missing, and saying so
            // points at the `when` line instead of sending the reader to hunt
            // for a typo.
            for (name, reason) in &set.excluded {
                println!(
                    "{name:<24} ({}{reason})",
                    osdk_core::i18n::tr("task.unavailable_here")
                );
            }
        }
        TaskCommand::Info { task } => {
            let Some(name) = set.resolve(&task) else {
                if let Some(reason) = set.exclusion_reason(&task) {
                    return Err(anyhow!(
                        "task `{task}` is not available on this platform ({reason})"
                    ));
                }
                return Err(anyhow!("unknown task `{task}`"));
            };
            let def = &set.tasks[name];
            println!("{}", toml::to_string_pretty(def).unwrap_or_default());
        }
        TaskCommand::Deps { task } => {
            let order = set.execution_order(&task)?;
            for (index, name) in order.iter().enumerate() {
                println!("{}. {name}", index + 1);
            }
        }
        TaskCommand::Add {
            name,
            run,
            description,
            depends,
        } => {
            // Validate before writing: a config that will not load is worse
            // than a rejected command, because the next osdk invocation fails
            // on something the user did not type.
            let mut probe = osdk_core::tasks::TaskSet::default();
            let mut entries = std::collections::BTreeMap::new();
            let spec = if run.len() == 1 {
                osdk_core::tasks::RunSpec::One(run[0].clone())
            } else {
                osdk_core::tasks::RunSpec::Many(
                    run.iter()
                        .map(|command| osdk_core::tasks::RunStep::Simple(command.clone()))
                        .collect(),
                )
            };
            entries.insert(
                name.clone(),
                osdk_core::tasks::TaskEntry::Full(Box::new(osdk_core::tasks::TaskDef {
                    run: Some(spec),
                    description: description.clone(),
                    depends: depends.clone(),
                    ..Default::default()
                })),
            );
            probe.apply(entries)?;

            let path = crate::config_edit::set_project_task(
                &name,
                &run,
                description.as_deref(),
                &depends,
            )?;
            println!("added task `{name}` to {}", path.display());
        }
        TaskCommand::Rm { name } => {
            let (path, removed) = crate::config_edit::remove_project_task(&name)?;
            if removed {
                println!("removed task `{name}` from {}", path.display());
            } else {
                // Saying "removed" for something that was never there would
                // hide a typo behind a success message.
                return Err(anyhow!("no task `{name}` in {}", path.display()));
            }
        }
        TaskCommand::Edit { name } => {
            let Some(path) = app.ctx.config.project_config_path.clone() else {
                return Err(anyhow!("no osdk project config found"));
            };
            if set.resolve(&name).is_none() {
                return Err(anyhow!("unknown task `{name}`"));
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .map_err(|error| anyhow!("cannot launch `{editor}`: {error}"))?;
            if !status.success() {
                return Err(anyhow!("`{editor}` exited with {status}"));
            }
        }
    }
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
            // The Python installer caches are osdk-managed now, so leaving them
            // out would make `cache clean` quietly incomplete: osdk would be
            // the reason they exist while offering no way to reclaim them.
            //
            // Only these two directories are touched, never `<cache>/pkg`
            // wholesale -- that root also holds cargo, gradle and the Go caches,
            // and cargo's in particular is a shared home containing installed
            // binaries rather than just downloads.
            let downstream = osdk_core::cache::downstream_root(&app.ctx.dirs.cache);
            for name in ["uv", "pip"] {
                let directory = downstream.join(name);
                if directory.exists() {
                    std::fs::remove_dir_all(&directory)
                        .with_context(|| format!("removing {}", directory.display()))?;
                }
            }
            println!("{}", t!("msg.cache_cleared"));
        }
        CacheCommand::Prune => {
            let outcome = osdk_core::backend::pypi::prune_cache(&app.ctx)?;
            if !outcome.uv_available {
                // Said plainly rather than reported as a successful no-op: uv is
                // what decides which entries are dangling, so without it this
                // command has nothing to do at all.
                println!("uv is not installed, so there is nothing to prune; run `osdk install pypi:uv` first");
            } else {
                match &outcome.uv_output {
                    Some(reported) => println!("{reported}"),
                    None => println!("nothing to prune"),
                }
            }
        }
    }
    Ok(())
}

/// The effective value of one setting, as osdk resolved it.
///
/// Read from the resolved config rather than re-parsed from the file so `get`
/// answers "what will osdk do" instead of "what does the file happen to say" --
/// the two differ whenever a default applies or an env var overrides.
pub(crate) fn resolved_setting(app: &App, key: &str) -> Option<String> {
    // Registry lists live in their own top-level table, so they are not
    // reachable from `Settings`. They still have to be readable under the key
    // `config set` accepts: a key that writes but then reports "unknown
    // setting" on read is the drift `every_writable_setting_can_also_be_read_back`
    // exists to catch, and it caught exactly this.
    if let Some(rendered) = registry_setting_display(app.ctx.config.registries(), key) {
        return Some(rendered);
    }
    if let Some(rendered) = sources_setting_display(&app.ctx.config.sources, key) {
        return Some(rendered);
    }
    setting_display(&app.ctx.config.settings, key)
}

/// Render a `sources.*` key, or `None` if the key is not one.
///
/// Like the registry lists, sources live in their own top-level table and are not
/// reachable from `Settings`.
pub(crate) fn sources_setting_display(
    sources: &osdk_core::config::SourcesConfig,
    key: &str,
) -> Option<String> {
    match key {
        "sources.probe_timeout_ms" => Some(sources.probe_timeout_ms.to_string()),
        "sources.model_probe_timeout_ms" => Some(sources.model_probe_timeout_ms.to_string()),
        "sources.model_download_attempts" => Some(sources.model_download_attempts.to_string()),
        "sources.model_download_retry_base_ms" => {
            Some(sources.model_download_retry_base_ms.to_string())
        }
        "sources.model_jobs" => Some(sources.model_jobs.to_string()),
        _ => None,
    }
}

/// Render a `registries.*` key, or `None` if the key is not one.
///
/// An empty list prints as `default` rather than as nothing, because the two
/// mean different things: no configured candidates means osdk uses its built-in
/// public default, which is not the same as having configured an empty set.
pub(crate) fn registry_setting_display(
    registries: &osdk_core::config::RegistriesConfig,
    key: &str,
) -> Option<String> {
    let urls = match key {
        "registries.python.urls" => &registries.python.urls,
        "registries.npm.urls" => &registries.npm.urls,
        _ => return None,
    };
    Some(if urls.is_empty() {
        "default".to_string()
    } else {
        urls.join(", ")
    })
}

/// Render one setting from a resolved [`Settings`].
pub(crate) fn setting_display(s: &osdk_core::config::Settings, key: &str) -> Option<String> {
    Some(match key {
        "jobs" => s.jobs.to_string(),
        "offline" => s.offline.to_string(),
        "yes" => s.yes.to_string(),
        "verify_signatures" => s.verify_signatures.to_string(),
        "require_checksums" => s.require_checksums.to_string(),
        "attestations" => s.attestations.to_string(),
        "prerelease" => s.prerelease.to_string(),
        "link_mode" => s.link_mode.to_string(),
        "lang" => s.lang.clone().unwrap_or_else(|| "auto".to_string()),
        "shims.include" => s.shims.include.join(", "),
        "shims.exclude" => s.shims.exclude.join(", "),
        "shims.expose" => s.shims.expose.join(", "),
        // Per-tool lists: `shims.<tool>.<field>`. Read back the same shape
        // `config set` accepts, so a value that was just written is visible
        // under the key the user typed rather than only inside the TOML.
        other => return tool_shim_display(s, other),
    })
}

/// Render `shims.<tool>.{include,exclude,expose}`, or `None` if not that shape.
///
/// An unset field prints as `inherit` rather than as an empty list: the two mean
/// different things here -- inheriting the global list, versus overriding it with
/// nothing -- and showing both as blank would hide which one is in effect.
pub(crate) fn tool_shim_display(s: &osdk_core::config::Settings, key: &str) -> Option<String> {
    let rest = key.strip_prefix("shims.")?;
    let (tool, field) = rest.rsplit_once('.')?;
    let overrides = s.shims.tools.get(tool);
    let list = match field {
        "include" => overrides.and_then(|tool| tool.include.as_ref()),
        "exclude" => overrides.and_then(|tool| tool.exclude.as_ref()),
        "expose" => overrides.and_then(|tool| tool.expose.as_ref()),
        _ => return None,
    };
    Some(match list {
        Some(values) => values.join(", "),
        None => "inherit".to_string(),
    })
}

/// Error text listing the settings that can be read or written.
pub(crate) fn unknown_setting(key: &str) -> anyhow::Error {
    let known: Vec<&str> = crate::config_edit::SETTINGS
        .iter()
        .map(|setting| setting.key)
        .collect();
    // Mention the per-tool shim keys too: they are not in the static table
    // because they carry a tool id, and a user who mistyped one would otherwise
    // see a list that does not contain the shape they were reaching for.
    anyhow!(
        "unknown setting `{key}`; known settings: {}; per-tool shim lists: \
         shims.<tool>.{{include|exclude|expose}}",
        known.join(", ")
    )
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
        ConfigCommand::Get { key, global } => {
            if global {
                // The global value on its own, not the merged result: with `-g`
                // the question is what the user config holds.
                let user = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
                let value = sources_setting_display(&user.sources, &key)
                    .or_else(|| setting_display(&user.settings, &key))
                    .ok_or_else(|| unknown_setting(&key))?;
                println!("{value}");
            } else {
                let value = resolved_setting(app, &key).ok_or_else(|| unknown_setting(&key))?;
                println!("{value}");
            }
        }
        ConfigCommand::Set { key, value, global } => {
            // `resolve_setting` also accepts the per-tool shim keys, which are
            // not in the static table because they carry a tool id.
            let setting = crate::config_edit::resolve_setting(&key)?;
            let scope = setting_scope(global);
            let path = crate::config_edit::set_setting(&app.ctx, &setting, &value, scope)?;
            // Re-read from disk: printing the argument back would claim success
            // even if the value landed somewhere the loader ignores.
            let written = osdk_core::config::Config::load_user(&path)?;
            // Read back through the same resolvers `config get` uses, so a key
            // written into `[sources]` or `[registries]` is confirmed where the
            // loader actually reads it rather than echoing the argument.
            let effective = sources_setting_display(&written.sources, &key)
                .or_else(|| setting_display(&written.settings, &key))
                .unwrap_or_else(|| value.clone());
            println!("set {key} = {effective} in {}", path.display());
            offer_trust_after_set(app, &path, scope)?;
        }
        ConfigCommand::Unset { key, global } => {
            let setting = crate::config_edit::resolve_setting(&key)?;
            let scope = setting_scope(global);
            match crate::config_edit::unset_setting(&app.ctx, &setting, scope)? {
                Some(path) => {
                    println!("unset {key} in {}", path.display());
                    // Removing a key rewrites the file, so a config that stays
                    // trust-required now hashes differently and has lost its
                    // trust record.
                    offer_trust_after_set(app, &path, scope)?;
                }
                None => {
                    let path = crate::config_edit::setting_scope_path(&app.ctx, scope)?;
                    println!("{key} was not set in {}", path.display());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn setting_scope(global: bool) -> crate::config_edit::SettingScope {
    if global {
        crate::config_edit::SettingScope::Global
    } else {
        crate::config_edit::SettingScope::Project
    }
}

/// Offer to trust a project config that the user just made trust-required.
///
/// Writing a setting into a project `osdk.toml` takes it past the
/// `[tools]`/`[aliases]` whitelist, so every later command in that directory
/// would be refused until it is trusted. Asking at the point of writing keeps
/// the gate meaningful -- it exists to catch configs that arrived with someone
/// else's repository, not the line the user just typed -- while not leaving
/// them to discover the refusal on an unrelated command later.
///
/// Declining is a real answer: the setting stays written but untrusted, which
/// is what someone preparing a config for a teammate to review would want.
pub(crate) fn offer_trust_after_set(
    app: &App,
    path: &std::path::Path,
    scope: crate::config_edit::SettingScope,
) -> Result<()> {
    if scope != crate::config_edit::SettingScope::Project {
        return Ok(());
    }
    if !osdk_core::trust::requires_trust(path)? {
        return Ok(());
    }
    // Trust is content-bound, so a config that is still trusted after this edit
    // needs no new record and no prompt.
    if osdk_core::trust::is_trusted(
        &app.ctx.dirs.config,
        path,
        std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS").as_ref(),
    )? {
        return Ok(());
    }
    let question = t!("prompt.trust_after_set", path = path.display());
    // A non-interactive session cannot answer, and `confirm` reports that as an
    // error. The write already succeeded, so failing the command here would
    // report failure for work that was done; treat "could not ask" exactly like
    // "declined" and say what is still needed.
    match app.prompt.confirm(&question) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            println!(
                "{}",
                t!("msg.trust_declined_after_set", path = path.display())
            );
            return Ok(());
        }
    }
    let record = osdk_core::trust::trust(&app.ctx.dirs.config, path)?;
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

pub fn trust(
    app: &App,
    path: Option<std::path::PathBuf>,
    command: Option<TrustCommand>,
) -> Result<()> {
    if matches!(command, Some(TrustCommand::List)) {
        for (record, state) in osdk_core::trust::list_with_state(&app.ctx.dirs.config)? {
            println!(
                "{}  {}  {}",
                osdk_core::i18n::tr(state.label_key()),
                record.hash,
                record.path.display()
            );
        }
        return Ok(());
    }

    if let Some(TrustCommand::Prune { dry_run }) = command {
        // Print before removing, and print the same list in both modes, so
        // `--dry-run` is a genuine preview of what the real run does rather than
        // a separately computed guess.
        let candidates: Vec<_> = osdk_core::trust::list_with_state(&app.ctx.dirs.config)?
            .into_iter()
            .filter(|(_, state)| state.is_prunable())
            .collect();
        if candidates.is_empty() {
            println!("{}", t!("msg.trust_prune_nothing"));
            return Ok(());
        }
        for (record, _) in &candidates {
            println!("  {}", record.path.display());
        }
        if dry_run {
            println!(
                "{}",
                t!("msg.trust_prune_dry_run", count = candidates.len())
            );
            return Ok(());
        }
        let removed = osdk_core::trust::prune(&app.ctx.dirs.config)?;
        println!("{}", t!("msg.trust_pruned", count = removed.len()));
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
    doctor_proxy_section();
    if verify {
        doctor_verify(app, tool.as_deref())?;
    }
    Ok(())
}

/// Report the proxy situation.
///
/// This is diagnosis, not adoption: osdk's reqwest client reads proxy settings
/// only from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`, never from the Windows
/// desktop (WinINET) proxy. A user who enabled the latter has a working browser
/// and a tool that times out, so when the two disagree we say so and point at the
/// exact variable to set. Credentials embedded in a proxy URL are redacted.
pub(crate) fn doctor_proxy_section() {
    let env = crate::proxy_diag::env_proxy();
    let windows = crate::proxy_diag::windows_proxy_settings();
    match crate::proxy_diag::advise(&env, windows.as_ref()) {
        crate::proxy_diag::ProxyAdvice::EnvConfigured { vars } => {
            println!(
                "  proxy        : {} ({})",
                t!("doctor.proxy_env"),
                vars.join(", ")
            );
        }
        crate::proxy_diag::ProxyAdvice::WindowsSystemProxyIgnored { server, pac } => {
            let mut detail = Vec::new();
            if let Some(server) = server {
                detail.push(crate::proxy_diag::redact_proxy(&server));
            }
            if pac {
                detail.push("PAC".to_string());
            }
            println!(
                "  proxy        : {} ({})",
                t!("doctor.proxy_win_ignored"),
                detail.join(", ")
            );
            println!("                {}", t!("doctor.proxy_hint"));
        }
        crate::proxy_diag::ProxyAdvice::NoneConfigured => {
            println!("  proxy        : {}", t!("doctor.proxy_none"));
        }
    }
}

/// Re-hash every installed file and report drift.
///
/// Downloads are verified on the way in, but nothing looked at the bytes again
/// afterwards, so a tool that rewrites itself in place, a manual edit or a
/// half-restored backup left osdk reporting the version it installed while a
/// different one actually ran. Reads every file, so it sits behind
/// `--verify` rather than in the default diagnostics.
pub(crate) fn doctor_verify(app: &App, only: Option<&str>) -> Result<()> {
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
