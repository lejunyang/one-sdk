//! `osdk deps` command implementation.
//!
//! The split from `install` is the whole point: `install` brings *tools* into
//! osdk's isolated directories, while this brings a project's *own* dependency
//! closure into the project using the project's own package manager. osdk
//! contributes the parts it is better placed to own -- picking the installer,
//! deciding frozen vs not, registry policy, reading the result back, recording
//! the identity -- and leaves resolution and installation to the native tool.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use osdk_core::deps::{
    self, state, DepsProviderSchema, DetectedProject, InstallerChoice, ProviderConfig, RunPlan,
    ToolRole,
};
use osdk_core::version::{ToolRequest, ToolVersion};

use crate::App;

/// Flags from the CLI, resolved once.
pub struct DepsOptions {
    pub providers: Vec<String>,
    pub list: bool,
    /// With `list`, widen it to every sub-project from `[deps].roots`.
    ///
    /// An operand-free materialization covers every declared root. Supplying a
    /// provider name narrows it to the nearest root; rooted ids are exact.
    pub list_all: bool,
    pub dry_run: bool,
    pub force: bool,
    pub explain: bool,
    pub skip: Vec<String>,
    /// Path patterns restricting which sub-projects take part.
    ///
    /// Orthogonal to `providers`, which selects by kind: `--filter 'apps/*'` means
    /// "every declared sub-project under apps/", whatever package manager each uses.
    /// Empty means no restriction.
    pub filter: Vec<String>,
    pub no_install_tools: bool,
    pub frozen: bool,
    pub verify: bool,
    /// Restrict the run to providers that opted into automatic materialization.
    ///
    /// Set only by [\materialize_auto\]. An explicit \osdk deps\ ignores \uto    /// entirely: asking for it by name is itself the opt-in.
    pub auto_only: bool,
}

/// One provider ready to be reported on or run.
struct Resolved {
    /// `None` for a custom provider: not having a built-in schema is precisely
    /// what makes one custom, so this is the distinction rather than an error.
    schema: Option<&'static DepsProviderSchema>,
    /// Set when this came from a `[deps].roots` pattern: the addressable id
    /// (`//apps/api:uv`) and the pattern that produced it. Reported so `--list`
    /// says where a sub-project came from instead of printing the same bare
    /// provider name once per package.
    rooted: Option<(String, String)>,
    project: DetectedProject,
    choice: InstallerChoice,
    config: ProviderConfig,
    plan: RunPlan,
    decision: state::Decision,
    unit: Option<deps::workspace::WorkspaceUnit>,
    lock_path: PathBuf,
}

pub async fn deps(app: &mut App, options: DepsOptions) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()
        .context("getting current dir")?
        .canonicalize()
        .context("canonicalizing dependency cwd")?;
    let Some(config_path) = app.ctx.config.project_config_path.clone() else {
        report_undeclared(&cwd)?;
        return Ok(());
    };
    let rel_base = app
        .ctx
        .config
        .project
        .as_ref()
        .filter(|p| p.is_external())
        .map(|p| p.root.as_path());
    let leaf_config = config_path;
    let config_path = osdk_core::config::inheritance::deps_anchor(&leaf_config, rel_base)?;
    let anchor_is_leaf = dunce::canonicalize(&leaf_config)? == dunce::canonicalize(&config_path)?;
    // Only the external leaf uses the bound project as its relative base.
    // An inherited catalog belongs to the ancestor that declared it, just as
    // it does for local projects, including that ancestor's own extends chain.
    let root = if anchor_is_leaf {
        app.ctx
            .config
            .project_root
            .as_deref()
            .unwrap_or_else(|| config_path.parent().unwrap_or(&cwd))
            .to_path_buf()
    } else {
        config_path.parent().unwrap_or(&cwd).to_path_buf()
    };
    let anchor_rel_base = if anchor_is_leaf { rel_base } else { None };
    let deps_config = osdk_core::config::inheritance::deps_file(&config_path, anchor_rel_base)?;
    let deps_config = if deps_config.providers.is_empty() && deps_config.projects.is_empty() {
        app.ctx.config.deps.clone()
    } else {
        deps_config
    };
    if deps_config.providers.is_empty() && deps_config.projects.is_empty() {
        report_undeclared(&cwd)?;
        return Ok(());
    }
    // Local dependency units keep their own locks. External units must stay
    // in the selected bundle and validate its one-manifest-per-provider limit.
    let external_lock = app
        .ctx
        .config
        .project
        .as_ref()
        .filter(|project| project.is_external())
        .map(|project| project.lock_path.clone());
    let mut entries = deps::catalog::catalog(&root, &config_path, &deps_config)?;
    let mut tool_evidence: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for entry in &mut entries {
        let mut effective = app.config_for_project(
            entry
                .config
                .project_config
                .as_deref()
                .unwrap_or(&config_path),
        )?;
        if let Some(registries) = &entry.config.registries {
            effective.sources.registries = registries.clone();
        }
        entry.config.registries = Some(effective.registries().clone());
        // Resolve only the tools this provider can execute. Native files for
        // unrelated ecosystems must not make an explicit deps provider fail.
        let ids: Vec<String> = match deps::provider_schema(&entry.project.provider) {
            Some(schema) => schema
                .required_tools
                .iter()
                .map(|tool| tool.id.to_string())
                .chain(std::iter::once(schema.id.to_string()))
                .collect(),
            None => entry.config.project_tools.keys().cloned().collect(),
        };
        for id in &ids {
            if let Ok(backend) = app.registry.get(id) {
                let selection = osdk_core::version::resolver::resolve_selection(
                    id,
                    &entry.project.root,
                    &effective,
                    backend.idiomatic_files(),
                )?;
                tool_evidence
                    .entry(entry.id())
                    .or_default()
                    .insert(id.clone(), selection.explain());
                if let Some(active) = selection.winner {
                    entry.config.project_tools.insert(id.clone(), active.spec);
                }
            }
        }
        entry.config.offline = effective.settings.offline;
        let tools: BTreeMap<_, _> = effective
            .tool_configs
            .iter()
            .map(|(id, tool)| (id, (tool.version(), tool.to_request_options())))
            .collect();
        entry.config.project_context = serde_json::to_string(&(
            &effective.settings,
            &effective.tool_aliases,
            &effective.sources,
            effective.registries(),
            &tools,
        ))?;
    }
    for entry in &entries {
        if let Some(unit) = &entry.unit {
            if entries.iter().any(|other| {
                other.unit.as_ref() == Some(unit)
                    && other.config.project_context != entry.config.project_context
            }) {
                return Err(anyhow!(
                    "{}: shared workspace has conflicting effective tool settings or sources",
                    unit.root.display()
                ));
            }
        }
    }
    let selected = select_projects(&entries, &cwd, &options);
    if !options.filter.is_empty() && selected.is_empty() {
        return Err(anyhow!(
            "`--filter` matched no project: {}",
            options.filter.join(", ")
        ));
    }
    for operand in &options.providers {
        if !selected
            .iter()
            .any(|entry| operand == &entry.id() || operand == &*entry.project.provider)
        {
            return Err(anyhow!("no matching dependency project for `{operand}`"));
        }
    }
    if !options.list && !options.dry_run && !options.verify {
        for entry in &selected {
            for source in &entry.config.config_sources {
                app.ensure_config_trusted(source, &[osdk_core::trust::Scope::Deps])?;
            }
        }
    }
    let mut resolved = Vec::new();
    for entry in selected {
        let unit = entry.unit.clone();
        let mut project = entry.project.clone();
        let mut rooted = (entry.id(), entry.source.clone());
        if let Some(unit) = &unit {
            if project.root != unit.root {
                println!("{} selects shared installation in {}; coverage expands to the native workspace", entry.id(), unit.root.display());
            }
            let owner = entries
                .iter()
                .find(|candidate| {
                    candidate.project.root == unit.root
                        && candidate.project.provider == unit.provider
                })
                .ok_or_else(|| anyhow!("workspace installation root is not enabled"))?;
            project = owner.project.clone();
            rooted = (owner.id(), owner.source.clone());
            if resolved.iter().any(|prior: &Resolved| {
                prior.project.root == project.root && prior.project.provider == project.provider
            }) {
                continue;
            }
        }
        let config = entry.config.clone();
        let schema = deps::provider_schema(&project.provider);
        let mut choice = deps::select_installer(&project, &[])?;
        if let Some(version) = config.project_tools.get(&*project.provider) {
            choice.version = Some(version.clone());
        }
        let mut plan = deps::plan(
            &project,
            &choice,
            &config,
            &resolved_tool_versions(app, &config),
        )?;
        if unit.as_ref().is_some_and(|unit| unit.provider == "uv") && config.run.is_none() {
            plan.args.push("--all-packages".into());
        }
        if config.offline {
            if config.run.is_some() {
                return Err(anyhow!(
                    "offline mode cannot be guaranteed for a custom deps command"
                ));
            }
            match &*project.provider {
                "npm" | "pnpm" | "uv" | "pip-requirements" | "cargo" => {
                    plan.args.push("--offline".into())
                }
                other => return Err(anyhow!("offline deps is not supported for `{other}`")),
            }
        }
        if options.frozen && !plan.frozen {
            return Err(anyhow!(
                "`--frozen` requires a native lockfile and native frozen guarantees for `{}`: {}",
                entry.id(),
                plan.downgraded_reason
                    .as_deref()
                    .unwrap_or("no native lockfile")
            ));
        }
        if config.run.is_none() {
            let mut context = source_context(app, &config)?;
            // Dry-run/freshness must not probe; explicit mappings still apply.
            context.config.settings.offline = true;
            let evidence = osdk_core::package_sources::apply(
                &context,
                &plan.cwd,
                &plan.program,
                resolved_tool_versions(app, &config)
                    .get("yarn")
                    .map(String::as_str),
                &plan.args,
                &mut plan.env,
            )
            .await?;
            if options.explain {
                println!("    sources: {evidence}");
                let candidates: Vec<_> =
                    osdk_core::package_sources::native_files(&plan.cwd, &plan.program, &|key| {
                        plan.env
                            .get(key)
                            .cloned()
                            .or_else(|| std::env::var(key).ok())
                    })
                    .into_iter()
                    .filter(|path| path.is_file())
                    .collect();
                println!("    fingerprint inputs: {candidates:?}; not an effective discovery chain; native winner and interpolation delegated to manager");
                if let Some(evidence) = tool_evidence.get(&rooted.0) {
                    for (id, selection) in evidence {
                        let pinned_installer = id == &plan.tool && choice.version.is_some();
                        if !plan.frozen
                            && !pinned_installer
                            && osdk_core::execution::enabled(&context)
                        {
                            if let Ok(backend) = app.registry.get(id) {
                                let actual = osdk_core::execution::resolve_without_probes(
                                    &context,
                                    backend.as_ref(),
                                    &plan.cwd,
                                )?;
                                println!(
                                    "    {id}: {}; verification deferred until execution",
                                    serde_json::to_string(&actual.evidence)?
                                );
                                continue;
                            }
                        }
                        println!(
                            "    {id}: {selection}; managed authority for frozen/pinned plans"
                        );
                    }
                }
            }
        }
        let decision = if osdk_core::execution::enabled(&source_context(app, &config)?) {
            state::Decision::Stale(
                "system tool policy: host execution is revalidated on every run".into(),
            )
        } else {
            decide(app, schema, &project, &config, &plan, unit.as_ref())?
        };
        let lock_path = external_lock
            .clone()
            .unwrap_or_else(|| project.root.join(crate::lockfile::LOCKFILE_NAME));
        resolved.push(Resolved {
            schema,
            rooted: Some(rooted),
            project,
            choice,
            config,
            plan,
            decision,
            unit,
            lock_path,
        });
    }
    if resolved.is_empty() {
        println!("no matching deps providers are configured");
        return Ok(());
    }
    order_by_depends(&mut resolved)?;

    if options.verify {
        validate_lock_ownership(&resolved)?;
        return verify(app, &resolved);
    }

    if options.list {
        for item in &resolved {
            println!(
                "{}  {}  {}",
                item.label(),
                if item.decision.is_fresh() {
                    "fresh"
                } else {
                    "stale"
                },
                item.project.root.display()
            );
            if options.explain {
                if let Some((_, pattern)) = &item.rooted {
                    println!("    from root: {pattern}");
                }
                if let Some(reason) = item.decision.reason() {
                    println!("    reason: {reason}");
                }
                println!("    config sources: {:?}", item.config.config_sources);
                println!(
                    "    installer: {} ({:?})",
                    item.choice.provider, item.choice.origin
                );
                println!(
                    "    effective: auto={}, timeout={:?}, sources={:?}, outputs={:?}",
                    item.config.auto,
                    item.config.timeout,
                    effective_sources(item.schema, &item.config),
                    effective_outputs(item.schema, &item.config)
                );
                if let Some(unit) = &item.unit {
                    println!(
                        "    coverage: {} {:?}",
                        if item.config.run.is_none() {
                            "native workspace"
                        } else {
                            "unverified custom run; declared members"
                        },
                        unit.coverage()
                    );
                }
                println!("    command:   {}", display_command(item));
            }
        }
        return Ok(());
    }

    validate_lock_ownership(&resolved)?;
    for item in &mut resolved {
        let system_policy = osdk_core::execution::enabled(&source_context(app, &item.config)?);
        if let Some(reason) = &item.plan.downgraded_reason {
            // Reported, never silent: a flag that does nothing is worse than an
            // error, because the run looks successful.
            println!("warning: {reason}");
        }
        if item.decision.is_fresh() && !options.force && !system_policy {
            println!("{} is up to date", item.label());
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
                display_command(item)
            );
            continue;
        }
        let state_path = state::state_path(&app.ctx.dirs.cache, &item.project.root);
        let lock_path = app
            .ctx
            .dirs
            .cache
            .join("deps-install-locks")
            .join(state_path.file_name().unwrap())
            .with_extension("lock");
        let _installation_lock = osdk_core::lock::FileLock::acquire(&lock_path)?;
        if !options.force
            && decide(
                app,
                item.schema,
                &item.project,
                &item.config,
                &item.plan,
                item.unit.as_ref(),
            )?
            .is_fresh()
            && !system_policy
        {
            println!(
                "{} is up to date after waiting for shared installation",
                item.label()
            );
            continue;
        }
        let tools = ensure_tools(app, item, options.no_install_tools).await?;
        let source_env = item.plan.env.clone();
        if item.config.run.is_none() {
            let context = source_context(app, &item.config)?;
            let evidence = osdk_core::package_sources::apply(
                &context,
                &item.plan.cwd,
                &item.plan.program,
                tools
                    .iter()
                    .find(|tool| tool.id == "yarn")
                    .map(|tool| tool.version.as_str()),
                &item.plan.args,
                &mut item.plan.env,
            )
            .await?;
            if options.explain {
                println!("    sources: {evidence}");
            }
        }
        run_plan(app, item, &tools)?;
        item.plan.env = source_env;
        record(app, item, &tools)?;
    }
    Ok(())
}

impl Resolved {
    /// How this provider is named in output: the rooted id when it came from a
    /// `roots` pattern, otherwise the plain provider name.
    ///
    /// Without it a monorepo with four `uv` packages prints `uv` four times with
    /// no way to tell the lines apart.
    fn label(&self) -> String {
        match &self.rooted {
            Some((id, _)) => id.clone(),
            None => self.project.provider.to_string(),
        }
    }
}

fn source_context(app: &App, config: &ProviderConfig) -> anyhow::Result<osdk_core::backend::Ctx> {
    let mut effective = match config.project_config.as_deref() {
        Some(path) => app.config_for_project(path)?,
        None => app.ctx.config.clone(),
    };
    if let Some(registries) = &config.registries {
        effective.sources.registries = registries.clone();
    }
    Ok(osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: effective,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: false,
    })
}

/// Sort providers so that everything a provider `depends` on runs before it.
///
/// A cycle is an error rather than a silently broken order: picking some order
/// anyway would make one of the steps run before its input existed, and the
/// failure would point at the wrong provider. Dependencies naming a provider that
/// is not configured are ignored -- that is a no-op, not a contradiction, since a
/// disabled provider has nothing to wait for.
fn order_by_depends(resolved: &mut Vec<Resolved>) -> anyhow::Result<()> {
    let present: Vec<_> = resolved
        .iter()
        .map(|item| {
            (
                item.label(),
                item.project.provider.to_string(),
                item.project.root.clone(),
                item.unit.clone(),
            )
        })
        .collect();

    let mut ordered: Vec<Resolved> = Vec::with_capacity(resolved.len());
    let mut done: Vec<String> = Vec::new();
    let mut remaining: Vec<Resolved> = std::mem::take(resolved);

    while !remaining.is_empty() {
        let ready = remaining.iter().position(|item| {
            item.config.depends.iter().all(|need| {
                present
                    .iter()
                    .filter(|(label, provider, root, unit)| {
                        if need.starts_with("//") {
                            need == label
                        } else {
                            need == provider
                                && (root == &item.project.root
                                    || unit.as_ref().is_some_and(|unit| {
                                        unit.members.contains(&item.project.root)
                                    })
                                    || item
                                        .unit
                                        .as_ref()
                                        .is_some_and(|unit| unit.members.contains(root)))
                        }
                    })
                    .all(|(label, _, _, _)| done.contains(label))
            })
        });
        match ready {
            Some(index) => {
                let item = remaining.remove(index);
                done.push(item.label());
                ordered.push(item);
            }
            None => {
                let stuck: Vec<&str> = remaining
                    .iter()
                    .map(|item| &*item.project.provider)
                    .collect();
                return Err(anyhow!(
                    "`depends` forms a cycle among: {}; break it in osdk.toml",
                    stuck.join(", ")
                ));
            }
        }
    }
    *resolved = ordered;
    Ok(())
}

/// Reject shared provider records before running installers or verification.
fn validate_lock_ownership(resolved: &[Resolved]) -> anyhow::Result<()> {
    let mut owners = BTreeMap::new();
    for item in resolved {
        let key = (&item.lock_path, &item.project.provider);
        if let Some(previous) = owners.insert(key, &item.project.manifest) {
            if previous != &item.project.manifest {
                anyhow::bail!("shared dependency lock cannot represent distinct manifests for provider {}; use separate external bindings for independent projects", item.project.provider);
            }
        }
        crate::lockfile::validate_deps_manifest(
            &item.lock_path,
            &item.project.provider,
            &item.project.manifest,
        )?;
    }
    Ok(())
}

/// Check installed environments against the package managers' own receipts.
///
/// Two layers, cheapest first:
///
/// * **L1** -- is the native lockfile still the one osdk installed from? This
///   catches a drift freshness structurally cannot: the lock is untouched, so the
///   sources hash matches, but the environment was rebuilt by something else.
/// * **L2** -- does every entry in the receipt still exist, at the recorded size
///   and version?
///
/// The exit code is non-zero when anything is wrong, so this is usable as a CI
/// gate. `checked` is reported alongside, because "0 problems" and "nothing was
/// examined" must not read the same -- a verification that inspected nothing is
/// not a pass.
fn verify(app: &App, resolved: &[Resolved]) -> anyhow::Result<()> {
    use osdk_core::deps::verify as deep;

    let mut problems = 0usize;
    for item in resolved {
        println!("{}  {}", item.project.provider, item.project.root.display());

        // L1 first: it is one file read, and it explains an L2 failure when both
        // fire.
        let lock_path = &item.lock_path;
        if lock_path.is_file() {
            let locked = crate::lockfile::load(lock_path)?;
            if let Some(entry) = locked.deps.get(&*item.project.provider) {
                if let Some(native) = &entry.native_lock {
                    match deep::verify_native_lock(
                        lock_path.parent().unwrap_or(&item.project.root),
                        &native.path,
                        &native.sha256,
                    )? {
                        Some(finding) => {
                            problems += 1;
                            println!("    L1 {}", finding.describe());
                        }
                        None => println!("    L1 {} matches osdk.lock", native.path),
                    }
                }
            }
        }

        let report = match item.project.ecosystem {
            osdk_core::deps::Ecosystem::Node => deep::verify_node(&item.project.root)?,
            osdk_core::deps::Ecosystem::Python => deep::verify_python(&item.project.root, None)?,
            other => {
                println!("    L2 not implemented for {} yet", other.as_str());
                continue;
            }
        };
        if report.is_clean() {
            println!("    L2 {} entries verified", report.checked);
        } else {
            problems += report.findings.len();
            println!(
                "    L2 {} entries checked, {} problem(s):",
                report.checked,
                report.findings.len()
            );
            for finding in &report.findings {
                println!("       {}", finding.describe());
            }
        }
    }
    let _ = app;

    if problems > 0 {
        return Err(anyhow!(
            "{problems} problem(s) found; the installed environment does not match \
             what was installed. Note that re-running the installer may not fix it: \
             `uv pip sync` was measured not to repair a modified file, and a tampered \
             file can already be in the tool's global cache. Clear the cache and \
             reinstall."
        ));
    }
    Ok(())
}

/// A tool `deps` resolved, and the bin directories it contributes.
struct ReadyTool {
    id: String,
    version: String,
    role: ToolRole,
    bin_dirs: Vec<PathBuf>,
    system: Option<osdk_core::execution::SystemTarget>,
    /// The backend's own process environment -- the managed package caches,
    /// such as pnpm's store. Empty for a system tool, which keeps the user's.
    env: BTreeMap<String, String>,
}

/// Resolve every tool a provider needs, installing the missing ones through
/// osdk's existing install path.
///
/// Installing is delegated, not reimplemented. `install_one_without_shims`
/// already carries source selection, verification, attestation and the CAS; a
/// second installer here would have to duplicate all of it to be equally honest,
/// and the copy would be the one that rots. The tools land where every other
/// osdk install lands -- an isolated install root -- never in the project:
/// `deps` puts *dependencies* in the project, while the package manager that
/// installs them is a tool.
async fn ensure_tools(
    app: &mut App,
    item: &Resolved,
    no_install_tools: bool,
) -> anyhow::Result<Vec<ReadyTool>> {
    let Some(path) = item.config.project_config.as_deref() else {
        return ensure_tools_in_context(app, item, no_install_tools).await;
    };
    let config = app.config_for_project(path)?;
    let mut project_app = app.with_config(config)?;
    let result = Box::pin(ensure_tools_in_context(
        &mut project_app,
        item,
        no_install_tools,
    ))
    .await;
    app.invalidate_dynamic_scan();
    result
}

async fn ensure_tools_in_context(
    app: &mut App,
    item: &Resolved,
    no_install_tools: bool,
) -> anyhow::Result<Vec<ReadyTool>> {
    let mut ready = Vec::new();
    // A custom provider declares no tools of its own: its `run` line invokes
    // whatever is already available, which in practice is what `depends` brought
    // in. Inventing a tool requirement for it would be guessing at the command.
    let Some(schema) = item.schema else {
        return Ok(ready);
    };
    for required in schema.required_tools {
        // An installer's version can be pinned by the manifest's
        // `packageManager` field. A runtime's cannot, so it comes from `[tools]`
        // or, failing that, whatever is already installed.
        let pinned = match required.role {
            ToolRole::Installer => item.choice.version.clone(),
            ToolRole::Runtime => None,
        };
        let spec = match &pinned {
            Some(version) => format!("{}@{version}", required.id),
            None => match item
                .config
                .project_tools
                .get(required.id)
                .or_else(|| app.ctx.config.global_tools.get(required.id))
            {
                Some(configured) => format!("{}@{configured}", required.id),
                None => required.id.to_string(),
            },
        };
        let mut request = ToolRequest::parse(&spec)
            .with_context(|| format!("parsing deps tool request `{spec}`"))?;
        crate::commands::inherit_configured_options_from(
            &mut request,
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
            None,
        );
        let backend = app.registry.get(&request.backend)?;
        // Frozen native plans and pinned installer choices retain managed
        // replay authority. A host executable is never a portable lock entry.
        if !item.plan.frozen && pinned.is_none() && osdk_core::execution::enabled(&app.ctx) {
            if let Some(osdk_core::execution::ExecutionTarget::System(target)) =
                osdk_core::execution::resolve(&app.ctx, backend.as_ref(), &item.project.root)?
                    .target
            {
                ready.push(ReadyTool {
                    id: required.id.into(),
                    version: String::new(),
                    role: required.role,
                    bin_dirs: vec![target.bin_dir().to_path_buf()],
                    system: Some(target),
                    env: BTreeMap::new(),
                });
                continue;
            }
        }
        if required.role == ToolRole::Runtime {
            if let Some(active) = osdk_core::version::resolver::resolve_active_config(
                required.id,
                &item.project.root,
                &app.ctx.config,
                backend.idiomatic_files(),
            )? {
                request.spec = if active.is_range {
                    osdk_core::version::VersionSpec::parse_range(&active.spec)?
                } else {
                    osdk_core::version::VersionSpec::parse(&active.spec)
                };
            }
        }
        let request = crate::commands::expand_request_alias(app, backend.as_ref(), &request)?;

        // Already installed means nothing is acquired, which is the distinction
        // `--no-install-tools` draws: it forbids *acquiring* a tool, not using
        // one the user installed themselves.
        let resolved = if request.backend.contains(':') {
            crate::commands::installed_dynamic_match(app, backend.as_ref(), &request)
        } else {
            let installed = backend.list_installed(&app.ctx)?;
            crate::commands::select_installed_version(&request.backend, &request.spec, installed)
                .ok()
                .map(|version| {
                    let mut version = ToolVersion::new(&request.backend, &version);
                    version.options.extend(request.options.clone());
                    version
                })
        };

        let version = match resolved {
            Some(version) => {
                backend.ensure_post_install(&app.ctx, &version)?;
                version
            }
            None => {
                if no_install_tools {
                    return Err(anyhow!(
                        "deps provider `{}` needs `{}`, which is not installed, and \
                         `--no-install-tools` forbids installing it; \
                         run `osdk install {spec}` first",
                        schema.id,
                        required.id
                    ));
                }
                for source in &item.config.config_sources {
                    app.ensure_config_trusted(source, &[osdk_core::trust::Scope::Install])?;
                }
                println!("installing {spec} for deps provider `{}`", schema.id);
                let (_, installed) =
                    crate::commands::install_one_without_shims(app, &request, false).await?;
                installed
            }
        };

        let bin_dirs = backend
            .bin_paths(&app.ctx, &version)
            .with_context(|| format!("resolving bin directories for {}", version.backend))?;
        let env = backend
            .exec_env(&app.ctx, &version)
            .with_context(|| format!("resolving environment for {}", version.backend))?;
        ready.push(ReadyTool {
            id: required.id.to_string(),
            version: version.version.clone(),
            role: required.role,
            bin_dirs,
            system: None,
            env,
        });
    }
    let mut env = item.plan.env.clone();
    env.insert(
        "PATH".into(),
        osdk_core::execution::path_with(
            &app.ctx,
            ready
                .iter()
                .filter(|t| t.role == ToolRole::Runtime)
                .chain(ready.iter().filter(|t| t.role != ToolRole::Runtime))
                .flat_map(|t| t.bin_dirs.clone()),
        )?,
    );
    for tool in &mut ready {
        if let Some(target) = &mut tool.system {
            target.verify(&app.ctx, &env)?;
            tool.version = target.version.clone().expect("verified");
        }
    }
    Ok(ready)
}

/// Run the provider's command with the resolved tools on PATH.
///
/// The program is resolved out of the install's own bin directory rather than
/// through an osdk shim. A shim refuses when no version is selected for the
/// current directory (`osdk-shim: no version of 'pnpm' selected`), and the
/// situation `deps` exists for is exactly "just installed, never `use`d".
///
/// Runtime bin directories are *prepended* to PATH rather than appended: npm and
/// pnpm are node scripts and dependency lifecycle hooks invoke `node` directly,
/// so an unrelated node earlier on PATH would win and the install would run
/// under a runtime osdk did not choose.
fn run_plan(app: &App, item: &Resolved, tools: &[ReadyTool]) -> anyhow::Result<()> {
    for tool in tools {
        if let Some(target) = &tool.system {
            target.command(&app.ctx, &target.tool)?;
        }
    }
    let program = resolve_program(&item.plan, tools).ok_or_else(|| {
        anyhow!(
            "could not find `{}` in the install for `{}`",
            item.plan.program_candidates.join("` or `"),
            item.plan.tool
        )
    })?;

    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut path = OsString::new();
    for dir in tools
        .iter()
        .filter(|t| t.role == ToolRole::Runtime)
        .chain(tools.iter().filter(|t| t.role != ToolRole::Runtime))
        .flat_map(|tool| &tool.bin_dirs)
    {
        if !path.is_empty() {
            path.push(separator);
        }
        path.push(dir);
    }
    if let Some(inherited) = std::env::var_os("PATH") {
        if !inherited.is_empty() {
            path.push(separator);
            path.push(inherited);
        }
    }

    if tools.iter().any(|t| t.system.is_some()) {
        path = osdk_core::execution::path_with(
            &app.ctx,
            tools
                .iter()
                .filter(|t| t.role == ToolRole::Runtime)
                .chain(tools.iter().filter(|t| t.role != ToolRole::Runtime))
                .flat_map(|t| t.bin_dirs.clone()),
        )?
        .into();
    }

    // A bare name is left to the OS, but Windows `CreateProcess` only tries
    // `.exe`: a custom `run = "pnpm ..."` missed the managed `pnpm.cmd` on the
    // PATH built above and failed with "program not found". Resolve it against
    // that same PATH first; explicit paths and Unix are left untouched.
    let program = crate::commands::resolve_program_in_dirs(
        &program,
        &std::env::split_paths(&path).collect::<Vec<_>>(),
    );

    // Already absolute: the provider resolved \[deps.<p>].dir\ against the project
    // root. Joining again would be a no-op for an absolute path but would quietly
    // change meaning if a provider ever returned a relative one.
    let cwd = &item.plan.cwd;
    let mut command = std::process::Command::new(&program);
    let mut external_env: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
    for tool in tools {
        if let Some(target) = &tool.system {
            target.restore_environment(&mut external_env);
        }
    }
    if tools.iter().any(|t| t.system.is_some()) {
        command.env_clear().envs(&external_env);
    }
    let child_env = child_env(&item.plan.env, tools);
    command
        .args(&item.plan.args)
        .current_dir(cwd)
        .env("PATH", &path)
        .envs(&child_env);

    println!("{}", display_command(item));

    // Prelude steps run first, with the same program, cwd and environment. A
    // failure here stops the run: continuing would hand the main command an
    // environment the tool already said it could not build.
    for step in &item.plan.prelude {
        let mut prelude = std::process::Command::new(&program);
        if tools.iter().any(|t| t.system.is_some()) {
            prelude.env_clear().envs(&external_env);
        }
        prelude
            .args(step)
            .current_dir(cwd)
            .env("PATH", &path)
            .envs(&child_env);
        run_bounded(prelude, item.config.timeout)?;
    }
    run_bounded(command, item.config.timeout)?;
    Ok(())
}

/// The environment a deps command adds on top of what it inherits.
///
/// Each managed tool contributes what `osdk exec` would give it -- above all
/// the shared package caches. Without them the install depended on whether the
/// calling shell happened to be activated: from an IDE task, CI or
/// `pwsh -NoProfile`, pnpm picked its default store and refused a
/// `node_modules` that osdk had linked from the managed one
/// (`ERR_PNPM_UNEXPECTED_STORE`). The project's `[deps].env` is applied last so
/// an explicit setting still wins, and `PATH` stays the one `run_plan` built.
fn child_env(plan_env: &BTreeMap<String, String>, tools: &[ReadyTool]) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = tools
        .iter()
        .flat_map(|tool| &tool.env)
        .filter(|(key, _)| !key.eq_ignore_ascii_case("PATH"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(
        plan_env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    env
}

/// Locate the installer program among the resolved bin directories.
///
/// Candidates are tried in order on Windows. Unix must skip Windows launchers:
/// distributions such as Yarn Classic ship both `yarn.cmd` and `yarn`. The
/// installer's own directories are searched first, then all of them, because npm
/// ships inside node's bin directory rather than its own.
fn resolve_program(plan: &RunPlan, tools: &[ReadyTool]) -> Option<PathBuf> {
    // A custom provider brings no tools of its own, so there are no install
    // directories to search: its `run` line names whatever `depends` made
    // available, or something already on PATH. Returning the name unresolved lets
    // the OS do the lookup against the PATH the child is given -- which is the
    // resolved tools first, then the inherited one.
    if tools.is_empty() {
        return plan.program_candidates.first().map(PathBuf::from);
    }
    let installer_first = tools
        .iter()
        .filter(|tool| tool.role == ToolRole::Installer || tool.id == plan.tool);
    for tool in installer_first.chain(tools.iter()) {
        if let Some(target) = &tool.system {
            if tool.id == plan.tool {
                return Some(target.executable.clone());
            }
            continue;
        }
        for candidate in &plan.program_candidates {
            if !cfg!(windows)
                && [".exe", ".cmd", ".bat"]
                    .iter()
                    .any(|suffix| candidate.ends_with(suffix))
            {
                continue;
            }
            for dir in &tool.bin_dirs {
                let path = dir.join(candidate);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    plan.program_candidates
        .first()
        .filter(|candidate| {
            **candidate == plan.program
                && (cfg!(windows)
                    || ![".exe", ".cmd", ".bat"]
                        .iter()
                        .any(|suffix| candidate.ends_with(suffix)))
        })
        .map(PathBuf::from)
}

/// Persist what happened: freshness state, then the lock entry.
///
/// Both only after a successful run. Recording freshness for a failed install
/// would make the next run report "up to date" for a tree that was never
/// populated, turning one visible failure into a silently broken working tree.
fn record(app: &App, item: &Resolved, tools: &[ReadyTool]) -> anyhow::Result<()> {
    // External tools are host state, not managed receipts or portable lock
    // entries. Conservatively rerun system-enabled deps instead of recording
    // unverifiable freshness for a mutable host toolchain.
    if tools.iter().any(|tool| tool.system.is_some()) {
        return Ok(());
    }
    let command = freshness_command(
        app,
        &item.plan,
        &item.config,
        &resolved_tool_versions(app, &item.config),
    )?;
    let sources = tracked_sources(item.schema, &item.config, item.unit.as_ref());
    let existing: Vec<PathBuf> = sources
        .iter()
        .map(|source| item.project.root.join(source))
        .filter(|path| path.is_file())
        .collect();
    let outputs: Vec<(PathBuf, bool)> = effective_outputs(item.schema, &item.config)
        .into_iter()
        .map(|(path, required)| (item.project.root.join(path), required))
        .collect();

    let state_path = state::state_path(&app.ctx.dirs.cache, &item.project.root);
    let mut persisted = state::DepsState::load(&state_path)?;
    persisted.record(
        &item.project.provider,
        &state::Inputs {
            sources: &existing,
            declared_sources: !sources.is_empty(),
            command: &command,
            outputs: &outputs,
        },
    )?;
    if item.config.run.is_none() {
        if let Some(unit) = &item.unit {
            persisted
                .providers
                .get_mut(&*item.project.provider)
                .unwrap()
                .coverage = unit.coverage();
        }
    }
    persisted.save(&state_path)?;

    let installer_version = tools
        .iter()
        .find(|tool| tool.role == ToolRole::Installer)
        .map(|tool| tool.version.clone());
    let runtime = tools
        .iter()
        .find(|tool| tool.role == ToolRole::Runtime)
        .map(|tool| format!("{}@{}", tool.id, tool.version));

    let lock_path = &item.lock_path;
    crate::lockfile::merge_deps(
        lock_path,
        &item.project.provider,
        crate::lockfile::DepsRecord {
            installer: item.choice.provider.to_string(),
            installer_version,
            runtime,
            project_root: &item.project.root,
            manifest: &item.project.manifest,
            native_lock: item.project.native_lock.as_deref(),
            run: display_command(item),
            index: None,
            allow_build_from_source: item.config.allow_build_from_source,
        },
    )?;
    Ok(())
}

/// Materialize declared dependencies ahead of another command, when stale.
///
/// Called by a **bare** `install`, `run` and `exec`. Three properties are
/// deliberate, each with a test that fails without it:
///
/// * **Only freshness is checked, never `--verify`.** The hash comparison is a
///   cache hit in the common case and costs well under a millisecond (measured:
///   0.16ms for a 20KiB lock, 2.06ms for a 2MiB monorepo lock). The deep receipt
///   scan is seconds, and seconds in front of every `osdk run` would make users
///   disable the whole mechanism. `--verify` stays explicit.
/// * **A fresh provider costs nothing beyond that check.** No installer is
///   spawned, so a warm project pays the millisecond and nothing else.
/// * **Never for a command with explicit operands.** `osdk install node` asks for
///   one tool; also rewriting the project's dependency tree would be a side
///   effect nobody asked for. Same rule as explicit operands skipping lock replay.
///
/// Errors propagate: if dependencies were meant to be ready and could not be made
/// ready, running the real command against a known-wrong environment is worse.
pub async fn materialize_auto(app: &mut App) -> anyhow::Result<()> {
    // Cheapest possible exit, before touching the filesystem: a project with no
    // `[deps]` section pays nothing for this feature existing.
    if app.ctx.config.deps.providers.is_empty() && app.ctx.config.deps.projects.is_empty() {
        return Ok(());
    }

    // Boxed on this edge, and the reason is worth keeping. `deps` transitively
    // awaits the entire install pipeline, so giving it a *second* async caller
    // makes the compiler inline a second copy of that future -- and the sum
    // overflowed the main thread's 1MB stack. The symptom was not subtle but was
    // deeply misleading: `osdk --version` died too, because a frame is reserved on
    // entry regardless of which branch runs. Boxing `materialize_auto` itself does
    // nothing here; the large part is what this function *contains*, not what
    // contains it.
    Box::pin(deps(app, auto_options())).await
}

/// The flag set an automatic run uses.
///
/// Split out from [`materialize_auto`] so the promises it makes are a value a test
/// can inspect. Two of them are load-bearing and neither is observable from the
/// outside once an install has run.
fn auto_options() -> DepsOptions {
    DepsOptions {
        providers: Vec::new(),
        list: false,
        dry_run: false,
        force: false,
        explain: false,
        skip: Vec::new(),
        // Acquiring a package manager is part of making dependencies ready, so
        // this path allows it. `--no-deps` opts out of the whole step rather than
        // half of it.
        no_install_tools: false,
        frozen: false,
        // An automatic run covers every declared root; narrowing it by path is an
        // explicit request, never a default.
        filter: Vec::new(),
        // Load-bearing: freshness only, never the deep receipt scan. Freshness is
        // sub-millisecond; the scan is seconds, and seconds in front of every
        // `osdk run` would get the whole mechanism switched off.
        verify: false,
        // An auto run covers only providers that asked for it.
        auto_only: true,
        // Not listing, so this is irrelevant; set explicitly rather than relying
        // on a default that could change.
        list_all: false,
    }
}

fn select_projects<'a>(
    entries: &'a [deps::catalog::ProjectEntry],
    cwd: &Path,
    options: &DepsOptions,
) -> Vec<&'a deps::catalog::ProjectEntry> {
    let wide = options.list_all
        || !options.filter.is_empty()
        || (options.providers.is_empty() && !options.list);
    entries
        .iter()
        .filter(|entry| {
            let provider = &*entry.project.provider;
            let id = entry.id();
            let nearest = !entries.iter().any(|other| {
                other.project.provider == entry.project.provider
                    && cwd.starts_with(&other.project.root)
                    && other.project.root.starts_with(&entry.project.root)
                    && other.project.root != entry.project.root
            });
            let intervening_manifest = deps::provider_schema(provider).is_some_and(|schema| {
                cwd.ancestors()
                    .take_while(|directory| *directory != entry.project.root)
                    .any(|directory| directory.join(schema.primary_manifest()).is_file())
            });
            let local = cwd.starts_with(&entry.project.root) && nearest && !intervening_manifest;
            let selected = if options.providers.is_empty() {
                wide || local
            } else {
                options
                    .providers
                    .iter()
                    .any(|operand| operand == &id || (operand == provider && (wide || local)))
            };
            selected
                && (!options.auto_only || entry.config.auto)
                && !options
                    .skip
                    .iter()
                    .any(|skip| skip == provider || skip == &id)
                && (options.filter.is_empty()
                    || options
                        .filter
                        .iter()
                        .any(|pattern| deps::path_matches(pattern, &entry.relative)))
        })
        .collect()
}

/// Tool versions osdk already resolved, so a provider that dispatches on the
/// major (yarn) sees the same version that will actually run.
fn resolved_tool_versions(app: &App, config: &ProviderConfig) -> BTreeMap<String, String> {
    let mut versions = app.ctx.config.global_tools.clone();
    versions.extend(config.project_tools.clone());
    let ids: std::collections::BTreeSet<_> = deps::PROVIDERS
        .iter()
        .flat_map(|schema| schema.required_tools.iter().map(|tool| tool.id))
        .collect();
    for id in ids {
        if let Ok(backend) = app.registry.get(id) {
            if let Ok(installed) = backend.list_installed(&app.ctx) {
                let mut installed: Vec<_> = installed;
                installed.sort();
                versions.insert(format!("{id}:installed"), installed.join(","));
            }
        }
    }
    versions
}

fn decide(
    app: &App,
    schema: Option<&'static DepsProviderSchema>,
    project: &DetectedProject,
    config: &ProviderConfig,
    plan: &RunPlan,
    unit: Option<&deps::workspace::WorkspaceUnit>,
) -> anyhow::Result<state::Decision> {
    let sources = tracked_sources(schema, config, unit);
    let existing: Vec<PathBuf> = sources
        .iter()
        .map(|source| project.root.join(source))
        .filter(|path| path.is_file())
        .collect();
    let outputs: Vec<(PathBuf, bool)> = effective_outputs(schema, config)
        .into_iter()
        .map(|(path, required)| (project.root.join(path), required))
        .collect();
    let path = state::state_path(&app.ctx.dirs.cache, &project.root);
    let persisted = state::DepsState::load(&path)?;
    let command = freshness_command(app, plan, config, &resolved_tool_versions(app, config))?;
    let inputs = state::Inputs {
        sources: &existing,
        declared_sources: !sources.is_empty(),
        command: &command,
        outputs: &outputs,
    };
    Ok(state::decide(
        &inputs,
        persisted.providers.get(&*project.provider),
    )?)
}

fn tracked_sources(
    schema: Option<&'static DepsProviderSchema>,
    config: &ProviderConfig,
    unit: Option<&deps::workspace::WorkspaceUnit>,
) -> Vec<String> {
    let mut sources = effective_sources(schema, config);
    if let Some(unit) = unit {
        sources.extend(
            unit.sources()
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
    }
    sources.sort();
    sources.dedup();
    sources
}

/// Freshness sources for a provider, whether or not it has a built-in schema.
///
/// A custom provider has no defaults to fall back to: what the project declared
/// is all there is. Declaring nothing therefore means freshness cannot be
/// established, so it runs every time -- deliberately, rather than being treated
/// as "always fresh", which would be the vacuous-truth trap.
fn effective_sources(
    schema: Option<&'static DepsProviderSchema>,
    config: &ProviderConfig,
) -> Vec<String> {
    match schema {
        Some(schema) => deps::effective_sources(schema, config),
        None => config.sources.clone(),
    }
}

/// Outputs for a provider, as (path, required) pairs.
///
/// A custom provider's declared outputs are `Required`: the user named them as the
/// result of the step, so their absence means the step has not produced what it
/// promised. Built-in providers keep their own mix of required and
/// optional-once-seen.
fn effective_outputs(
    schema: Option<&'static DepsProviderSchema>,
    config: &ProviderConfig,
) -> Vec<(String, bool)> {
    match schema {
        Some(schema) => deps::effective_outputs(
            schema,
            config,
            config.outputs_explicit || !config.outputs.is_empty(),
        )
        .into_iter()
        .map(|spec| (spec.path, spec.required))
        .collect(),
        None => config
            .outputs
            .iter()
            .map(|path| (path.clone(), true))
            .collect(),
    }
}

fn freshness_command(
    app: &App,
    plan: &RunPlan,
    config: &ProviderConfig,
    versions: &BTreeMap<String, String>,
) -> anyhow::Result<String> {
    let mut paths = osdk_core::package_registry::managed_npm_config_files(&app.ctx);
    paths.extend(osdk_core::package_sources::invocation_files(
        &plan.cwd,
        &plan.program,
        &plan.args,
    ));
    paths.extend(app.ctx.config.global_config_path.iter().cloned());
    Ok(osdk_core::package_sources::fingerprint(
        &plan.cwd,
        &plan.program,
        &format!(
            "{plan:?}|{config:?}|{versions:?}|{:?}",
            app.ctx.config.settings.precedence
        ),
        &paths,
    )?)
}

fn run_bounded(
    mut command: std::process::Command,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<()> {
    use osdk_core::tasks::tree;
    let (mut child, mut handle) = tree::spawn_in_tree(&mut command)?;
    match tree::wait_with_timeout(&mut child, &mut handle, timeout)? {
        Some(0) => Ok(()),
        Some(code) => Err(anyhow!("deps command failed with exit code {code}")),
        None => Err(anyhow!("deps command timed out; process tree terminated")),
    }
}

/// Safe diagnostic command. Freshness hashes execution material separately.
fn display_command(item: &Resolved) -> String {
    if item.config.run.is_some() {
        return format!(
            "custom command {} [arguments and environment redacted]",
            item.project.provider
        );
    }
    command_line(&item.plan)
}

fn command_line(plan: &RunPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in &plan.env {
        let safe = matches!(
            (key.as_str(), value.as_str()),
            ("UV_PYTHON_DOWNLOADS", "never")
                | ("GOTOOLCHAIN", "local")
                | ("YARN_ENABLE_SCRIPTS", "false" | "true")
                | ("UV_INDEX_STRATEGY", "first-index")
        );
        parts.push(format!(
            "{key}={}",
            if safe { value.as_str() } else { "[redacted]" }
        ));
    }
    let program = plan.program.clone();
    // Show prelude structure without exposing credential-bearing values.
    for step in &plan.prelude {
        parts.push(format!(
            "{program} {}",
            step.iter()
                .map(|arg| osdk_core::package_sources::display_value(arg))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        parts.push("&&".to_string());
    }
    parts.push(program);
    parts.extend(plan.args.iter().map(|arg| {
        let arg = osdk_core::package_sources::display_value(arg);
        if arg.chars().any(char::is_whitespace) {
            serde_json::to_string(&arg).unwrap()
        } else {
            arg.clone()
        }
    }));
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
                if !providers.iter().any(|name| *name == &*project.provider) {
                    providers.push(&*project.provider);
                }
            }
            None => by_manifest.push((project.manifest.clone(), vec![&*project.provider])),
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

#[cfg(test)]
mod tests {
    use super::{auto_options, child_env, command_line, resolve_program, ReadyTool, ToolRole};
    use osdk_core::deps::{
        node, DetectedProject, Ecosystem, InstallerChoice, InstallerOrigin, ProviderConfig,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn dependency_order_waits_for_the_matching_project() {
        fn item(
            root: &std::path::Path,
            relative: &str,
            provider: &str,
            needs: &[&str],
        ) -> super::Resolved {
            let project = osdk_core::deps::detect_custom(
                provider,
                &root.join(relative),
                &root.join("osdk.toml"),
            );
            let config = ProviderConfig {
                run: Some("command".into()),
                depends: needs.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };
            let choice = osdk_core::deps::select_installer(&project, &[]).unwrap();
            let plan = osdk_core::deps::plan_custom(&project, &config).unwrap();
            super::Resolved {
                schema: None,
                rooted: Some((format!("//{relative}:{provider}"), "test".into())),
                project,
                choice,
                config,
                plan,
                decision: osdk_core::deps::state::Decision::Fresh,
                unit: None,
                lock_path: std::env::temp_dir().join("test-osdk.lock"),
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut items = vec![
            item(root, "a", "consume", &["produce"]),
            item(root, "b", "produce", &[]),
            item(root, "a", "produce", &["prepare"]),
            item(root, "a", "prepare", &[]),
        ];
        super::order_by_depends(&mut items).unwrap();
        assert_eq!(
            items.iter().map(super::Resolved::label).collect::<Vec<_>>(),
            ["//b:produce", "//a:prepare", "//a:produce", "//a:consume"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn deps_timeout_stops_a_real_command() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let start = std::time::Instant::now();
        let error =
            super::run_bounded(command, Some(std::time::Duration::from_millis(100))).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn explicit_empty_sources_and_outputs_do_not_fall_back() {
        let config = ProviderConfig {
            sources_explicit: true,
            outputs_explicit: true,
            ..Default::default()
        };
        assert!(super::effective_sources(Some(&node::NPM), &config).is_empty());
        assert!(super::effective_outputs(Some(&node::NPM), &config).is_empty());
    }

    /// Windows launcher suffixes are execution details and must never enter the
    /// command identity persisted in a cross-platform project lock.
    #[test]
    fn dependency_command_identity_is_platform_neutral() {
        let project = DetectedProject {
            provider: "bun".into(),
            ecosystem: Ecosystem::Node,
            root: PathBuf::from("project"),
            manifest: PathBuf::from("project/package.json"),
            native_lock: Some(PathBuf::from("project/bun.lock")),
            declared_manager: None,
        };
        let choice = InstallerChoice {
            provider: "bun".into(),
            origin: InstallerOrigin::Default,
            version: Some("1.4.0".into()),
        };
        let plan = node::plan(
            &project,
            &choice,
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(plan.program_candidates[0], "bun.exe");
        assert_eq!(
            command_line(&plan),
            "bun install --frozen-lockfile --ignore-scripts"
        );
    }

    #[test]
    fn deps_child_env_carries_tool_caches_under_the_project_env() {
        let tool = |id: &str, env: &[(&str, &str)]| ReadyTool {
            id: id.into(),
            version: "1".into(),
            role: ToolRole::Installer,
            bin_dirs: Vec::new(),
            system: None,
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        };
        let tools = [
            tool(
                "pnpm",
                &[
                    ("pnpm_config_store_dir", "/managed/pnpm-store"),
                    ("PNPM_HOME", "/managed/pnpm"),
                    ("PATH", "/must/not/replace/the/built/path"),
                ],
            ),
            tool("node", &[("npm_config_cache", "/managed/npm")]),
        ];
        let plan_env = BTreeMap::from([("PNPM_HOME".to_string(), "/project/choice".to_string())]);

        let env = child_env(&plan_env, &tools);
        assert_eq!(env["pnpm_config_store_dir"], "/managed/pnpm-store");
        assert_eq!(env["npm_config_cache"], "/managed/npm");
        assert_eq!(env["PNPM_HOME"], "/project/choice");
        assert!(!env.contains_key("PATH"), "{env:?}");
    }

    #[test]
    fn dependency_program_selection_uses_host_launchers() {
        let temp = tempfile::tempdir().unwrap();
        let project = DetectedProject {
            provider: "yarn".into(),
            ecosystem: Ecosystem::Node,
            root: temp.path().to_path_buf(),
            manifest: temp.path().join("package.json"),
            native_lock: None,
            declared_manager: None,
        };
        let choice = InstallerChoice {
            provider: "yarn".into(),
            origin: InstallerOrigin::Default,
            version: Some("1.22.22".into()),
        };
        let plan = node::plan(
            &project,
            &choice,
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        for name in ["yarn", "yarn.cmd"] {
            std::fs::write(temp.path().join(name), "fixture").unwrap();
        }
        let tools = vec![ReadyTool {
            id: "yarn".into(),
            version: "1.22.22".into(),
            role: ToolRole::Installer,
            bin_dirs: vec![temp.path().to_path_buf()],
            system: None,
            env: BTreeMap::new(),
        }];
        let expected = if cfg!(windows) { "yarn.cmd" } else { "yarn" };
        assert_eq!(
            resolve_program(&plan, &tools),
            Some(temp.path().join(expected))
        );
        #[cfg(not(windows))]
        {
            std::fs::remove_file(temp.path().join("yarn")).unwrap();
            assert_eq!(resolve_program(&plan, &tools), None);
        }
    }

    /// The automatic path must never trigger the deep verification scan.
    ///
    /// This test exists because its absence was caught by mutation: setting
    /// `verify: true` in the auto path left the entire suite green, which means the
    /// most expensive promise in the feature had nothing holding it. `--verify`
    /// stays an explicit request.
    #[test]
    fn the_auto_path_checks_freshness_and_never_scans_deeply() {
        let options = auto_options();
        assert!(
            !options.verify,
            "an automatic run must not pay for the receipt scan"
        );
        assert!(
            !options.force,
            "an automatic run must respect freshness rather than override it"
        );
    }

    /// Automatic runs are narrowed to providers that opted in; explicit ones are not.
    #[test]
    fn only_the_auto_path_filters_on_the_auto_flag() {
        assert!(
            auto_options().auto_only,
            "`auto = false` must exclude a provider from automatic runs"
        );
    }

    /// An automatic run may acquire a missing package manager.
    ///
    /// Pinned because the opposite is a defensible-looking choice that would break
    /// the feature's whole point: a project declaring pnpm would then fail on a
    /// machine without it, in the one code path meant to make things just work.
    #[test]
    fn an_automatic_run_may_acquire_a_missing_package_manager() {
        assert!(!auto_options().no_install_tools);
    }
}
