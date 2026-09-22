//! `osdk deps` command implementation.
//!
//! The split from `install` is the whole point: `install` brings *tools* into
//! osdk's isolated directories, while this brings a project's *own* dependency
//! closure into the project using the project's own package manager. osdk
//! contributes the parts it is better placed to own -- picking the installer,
//! deciding frozen vs not, registry policy, reading the result back, recording
//! the identity -- and leaves resolution and installation to the native tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use osdk_core::deps::{
    self, state, DepsProviderSchema, DetectedProject, InstallerChoice, ProviderConfig, RunPlan,
};

use crate::App;

/// Flags from the CLI, resolved once.
pub struct DepsOptions {
    pub providers: Vec<String>,
    pub list: bool,
    pub dry_run: bool,
    pub force: bool,
    pub explain: bool,
    pub skip: Vec<String>,
    pub no_install_tools: bool,
    pub frozen: bool,
}

/// One provider ready to be reported on or run.
struct Resolved {
    schema: &'static DepsProviderSchema,
    project: DetectedProject,
    choice: InstallerChoice,
    plan: RunPlan,
    decision: state::Decision,
}

pub async fn deps(app: &mut App, options: DepsOptions) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current dir")?;
    let configured = configured_providers(app);
    if configured.is_empty() {
        // Detection without configuration only *reports*: silently running
        // `npm ci` because a package.json exists would be exactly the kind of
        // implicit, large side effect osdk avoids elsewhere (a bare `install`
        // does not fetch models either).
        report_undeclared(&cwd)?;
        return Ok(());
    }

    let enabled: Vec<&'static DepsProviderSchema> = configured
        .keys()
        .filter(|id| !options.skip.iter().any(|skip| skip == *id))
        .filter(|id| options.providers.is_empty() || options.providers.iter().any(|p| p == *id))
        .filter_map(|id| deps::provider_schema(id))
        .collect();
    if enabled.is_empty() {
        println!("no matching deps providers are configured");
        return Ok(());
    }

    let ceiling = app
        .ctx
        .config
        .project_config_path
        .as_deref()
        .and_then(Path::parent);
    let detected = deps::discover(&cwd, ceiling, &enabled)?;
    if detected.is_empty() {
        println!("no dependency manifests found for the configured providers");
        return Ok(());
    }

    let tool_versions = resolved_tool_versions(app);
    let mut resolved = Vec::new();
    for project in &detected {
        let config = configured
            .get(project.provider)
            .cloned()
            .unwrap_or_default();
        let schema = deps::provider_schema(project.provider)
            .ok_or_else(|| anyhow!("unknown deps provider `{}`", project.provider))?;
        let choice = deps::select_installer(project, &config, &detected)?;
        let plan = deps::plan(project, &choice, &config, &tool_versions)?;
        if options.frozen && !plan.frozen {
            return Err(anyhow!(
                "`--frozen` requires a native lockfile for `{}`: {}",
                project.provider,
                plan.downgraded_reason
                    .clone()
                    .unwrap_or_else(|| "no lockfile found".into())
            ));
        }
        let decision = decide(app, schema, project, &config, &plan)?;
        resolved.push(Resolved {
            schema,
            project: project.clone(),
            choice,
            plan,
            decision,
        });
    }

    if options.list {
        for item in &resolved {
            println!(
                "{}  {}  {}",
                item.project.provider,
                if item.decision.is_fresh() {
                    "fresh"
                } else {
                    "stale"
                },
                item.project.root.display()
            );
            if options.explain {
                if let Some(reason) = item.decision.reason() {
                    println!("    reason: {reason}");
                }
                println!(
                    "    installer: {} ({:?})",
                    item.choice.provider, item.choice.origin
                );
                println!("    command:   {}", command_line(&item.plan));
            }
        }
        return Ok(());
    }

    for item in &resolved {
        if let Some(reason) = &item.plan.downgraded_reason {
            // Reported, never silent: a flag that does nothing is worse than an
            // error, because the run looks successful.
            println!("warning: {reason}");
        }
        if item.decision.is_fresh() && !options.force {
            println!("{} is up to date", item.project.provider);
            continue;
        }
        if options.explain {
            if let Some(reason) = item.decision.reason() {
                println!("{}: {reason}", item.project.provider);
            }
        }
        if options.dry_run {
            println!(
                "would run in {}: {}",
                item.project.root.display(),
                command_line(&item.plan)
            );
            continue;
        }
        let _ = &item.schema;
        let _ = &options.no_install_tools;
        return Err(anyhow!(
            "running `{}` is not wired up yet: installing a missing package manager \
             and executing the plan land in the next step (D2). \
             Use `--dry-run` or `--list` for now.",
            command_line(&item.plan)
        ));
    }
    Ok(())
}

/// Providers configured in `[deps]`, minus any the project disabled.
fn configured_providers(app: &App) -> BTreeMap<String, ProviderConfig> {
    let deps_config = &app.ctx.config.deps;
    let mut out = BTreeMap::new();
    for (id, entry) in &deps_config.providers {
        if deps_config.disable.iter().any(|name| name == id) {
            continue;
        }
        out.insert(
            id.clone(),
            ProviderConfig {
                auto: entry.auto,
                sources: entry.sources.clone(),
                outputs: entry.outputs.clone().unwrap_or_default(),
                run: entry.run.clone(),
                env: entry.env.clone(),
                dir: entry.dir.clone(),
                depends: entry.depends.clone(),
                installer: entry.installer.clone(),
                index: entry.index.clone().or_else(|| entry.extra_index.clone()),
                allow_build_from_source: entry.allow_build_from_source,
            },
        );
    }
    out
}

/// Tool versions osdk already resolved, so a provider that dispatches on the
/// major (yarn) sees the same version that will actually run.
fn resolved_tool_versions(app: &App) -> BTreeMap<String, String> {
    app.ctx
        .config
        .tools
        .iter()
        .map(|(tool, spec)| (tool.clone(), spec.clone()))
        .collect()
}

fn decide(
    app: &App,
    schema: &'static DepsProviderSchema,
    project: &DetectedProject,
    config: &ProviderConfig,
    plan: &RunPlan,
) -> anyhow::Result<state::Decision> {
    let sources = deps::effective_sources(schema, config);
    let existing: Vec<PathBuf> = sources
        .iter()
        .map(|source| project.root.join(source))
        .filter(|path| path.is_file())
        .collect();
    let outputs: Vec<(PathBuf, bool)> =
        deps::effective_outputs(schema, config, config_outputs_set(config))
            .into_iter()
            .map(|spec| (project.root.join(&spec.path), spec.required))
            .collect();
    let path = state::state_path(&app.ctx.dirs.cache, &project.root);
    let persisted = state::DepsState::load(&path)?;
    let command = command_line(plan);
    let inputs = state::Inputs {
        sources: &existing,
        declared_sources: !sources.is_empty(),
        command: &command,
        outputs: &outputs,
    };
    Ok(state::decide(
        &inputs,
        persisted.providers.get(project.provider),
    )?)
}

fn config_outputs_set(config: &ProviderConfig) -> bool {
    !config.outputs.is_empty()
}

/// The effective command, including env, because env is part of *what runs*:
/// yarn berry disables build scripts through `YARN_ENABLE_SCRIPTS`, so a command
/// string without env would hash two materially different runs the same.
fn command_line(plan: &RunPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in &plan.env {
        parts.push(format!("{key}={value}"));
    }
    parts.push(
        plan.program_candidates
            .first()
            .cloned()
            .unwrap_or_else(|| plan.tool.to_string()),
    );
    parts.extend(plan.args.iter().cloned());
    parts.join(" ")
}

/// With no `[deps]` section, say what was found and how to opt in -- do not act.
fn report_undeclared(cwd: &Path) -> anyhow::Result<()> {
    let all: Vec<&'static DepsProviderSchema> = deps::PROVIDERS.to_vec();
    let detected = deps::discover(cwd, None, &all)?;
    if detected.is_empty() {
        println!("no `[deps]` section, and no known dependency manifests nearby");
        return Ok(());
    }
    // Group by manifest, not by provider. Four Node providers all read
    // `package.json`, so listing one line each would claim four findings where
    // there is one project and make the reader think a choice had been made for
    // them. The choice is theirs, so the candidates are shown as candidates.
    let mut by_manifest: Vec<(PathBuf, Vec<&str>)> = Vec::new();
    for project in &detected {
        match by_manifest
            .iter_mut()
            .find(|(path, _)| path == &project.manifest)
        {
            Some((_, providers)) => {
                if !providers.contains(&project.provider) {
                    providers.push(project.provider);
                }
            }
            None => by_manifest.push((project.manifest.clone(), vec![project.provider])),
        }
    }

    println!("no `[deps]` section; found manifests osdk could manage:");
    for (manifest, providers) in &by_manifest {
        println!("  {}", manifest.display());
        println!("    candidates: {}", providers.join(", "));
    }
    println!();
    println!("enable one in osdk.toml, for example:");
    let suggestion = by_manifest
        .first()
        .and_then(|(_, providers)| providers.first().copied())
        .unwrap_or("pnpm");
    println!("  [deps.{suggestion}]");
    Ok(())
}
