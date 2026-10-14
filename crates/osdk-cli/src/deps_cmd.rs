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
}

pub async fn deps(app: &mut App, options: DepsOptions) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current dir")?;
    let Some(config_path) = app.ctx.config.project_config_path.clone() else {
        report_undeclared(&cwd)?;
        return Ok(());
    };
    let root = config_path.parent().unwrap_or(&cwd);
    if app.ctx.config.deps.providers.is_empty() && app.ctx.config.deps.projects.is_empty() {
        report_undeclared(&cwd)?;
        return Ok(());
    }
    let entries = deps::catalog::catalog(root, &config_path, &app.ctx.config.deps)?;
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
    let tool_versions = resolved_tool_versions(app);
    let mut resolved = Vec::new();
    for entry in selected {
        let unit = entry.unit.clone();
        let mut project = entry.project.clone();
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
            if resolved.iter().any(|prior: &Resolved| {
                prior.project.root == project.root && prior.project.provider == project.provider
            }) {
                continue;
            }
        }
        let config = entry.config.clone();
        let schema = deps::provider_schema(&project.provider);
        let choice = deps::select_installer(&project, &config, &[])?;
        let mut plan = deps::plan(&project, &choice, &config, &tool_versions)?;
        if unit.as_ref().is_some_and(|unit| unit.provider == "uv") && config.run.is_none() {
            plan.args.push("--all-packages".into());
        }
        if options.frozen && !plan.frozen {
            return Err(anyhow!(
                "`--frozen` cannot be guaranteed for `{}`: {}",
                entry.id(),
                plan.downgraded_reason
                    .as_deref()
                    .unwrap_or("no native lockfile")
            ));
        }
        let decision = decide(app, schema, &project, &config, &plan, unit.as_ref())?;
        resolved.push(Resolved {
            schema,
            rooted: Some((entry.id(), entry.source.clone())),
            project,
            choice,
            config,
            plan,
            decision,
            unit,
        });
    }
    if resolved.is_empty() {
        println!("no matching deps providers are configured");
        return Ok(());
    }
    order_by_depends(&mut resolved)?;

    if options.verify {
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
        let lock_path =
            state::state_path(&app.ctx.dirs.cache, &item.project.root).with_extension("lock");
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
        {
            println!(
                "{} is up to date after waiting for shared installation",
                item.label()
            );
            continue;
        }
        let tools = ensure_tools(app, item, options.no_install_tools).await?;
        run_plan(item, &tools)?;
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

/// Sort providers so that everything a provider `depends` on runs before it.
///
/// A cycle is an error rather than a silently broken order: picking some order
/// anyway would make one of the steps run before its input existed, and the
/// failure would point at the wrong provider. Dependencies naming a provider that
/// is not configured are ignored -- that is a no-op, not a contradiction, since a
/// disabled provider has nothing to wait for.
fn order_by_depends(resolved: &mut Vec<Resolved>) -> anyhow::Result<()> {
    let present: Vec<String> = resolved
        .iter()
        .map(|item| item.project.provider.to_string())
        .collect();

    let mut ordered: Vec<Resolved> = Vec::with_capacity(resolved.len());
    let mut done: Vec<String> = Vec::new();
    let mut remaining: Vec<Resolved> = std::mem::take(resolved);

    while !remaining.is_empty() {
        let ready = remaining.iter().position(|item| {
            item.config.depends.iter().all(|need| {
                !present.iter().any(|name| name == need) || done.iter().any(|name| name == need)
            })
        });
        match ready {
            Some(index) => {
                let item = remaining.remove(index);
                done.push(item.project.provider.to_string());
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
        let lock_path = crate::lockfile::default_path(&item.project.root);
        if lock_path.is_file() {
            let locked = crate::lockfile::load(&lock_path)?;
            if let Some(entry) = locked.deps.get(&*item.project.provider) {
                if let Some(native) = &entry.native_lock {
                    match deep::verify_native_lock(
                        &item.project.root,
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
            None => match app.ctx.config.tools.get(required.id) {
                Some(configured) => format!("{}@{configured}", required.id),
                None => required.id.to_string(),
            },
        };
        let request = ToolRequest::parse(&spec)
            .with_context(|| format!("parsing deps tool request `{spec}`"))?;
        let backend = app.registry.get(&request.backend)?;

        // Already installed means nothing is acquired, which is the distinction
        // `--no-install-tools` draws: it forbids *acquiring* a tool, not using
        // one the user installed themselves.
        let installed = backend.list_installed(&app.ctx)?;
        let resolved =
            crate::commands::select_installed_version(&request.backend, &request.spec, installed)
                .ok();

        let version = match resolved {
            Some(version) => ToolVersion::new(&request.backend, &version),
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
                println!("installing {spec} for deps provider `{}`", schema.id);
                let (_, installed) =
                    crate::commands::install_one_without_shims(app, &request, false).await?;
                installed
            }
        };

        let bin_dirs = backend
            .bin_paths(&app.ctx, &version)
            .with_context(|| format!("resolving bin directories for {}", version.backend))?;
        ready.push(ReadyTool {
            id: required.id.to_string(),
            version: version.version.clone(),
            role: required.role,
            bin_dirs,
        });
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
fn run_plan(item: &Resolved, tools: &[ReadyTool]) -> anyhow::Result<()> {
    let program = resolve_program(&item.plan, tools).ok_or_else(|| {
        anyhow!(
            "could not find `{}` in the install for `{}`",
            item.plan.program_candidates.join("` or `"),
            item.plan.tool
        )
    })?;

    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut path = OsString::new();
    for dir in tools.iter().flat_map(|tool| &tool.bin_dirs) {
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

    // Already absolute: the provider resolved \[deps.<p>].dir\ against the project
    // root. Joining again would be a no-op for an absolute path but would quietly
    // change meaning if a provider ever returned a relative one.
    let cwd = &item.plan.cwd;
    let mut command = std::process::Command::new(&program);
    command
        .args(&item.plan.args)
        .current_dir(cwd)
        .env("PATH", &path);
    for (key, value) in &item.plan.env {
        command.env(key, value);
    }

    println!("{}", command_line(&item.plan));

    // Prelude steps run first, with the same program, cwd and environment. A
    // failure here stops the run: continuing would hand the main command an
    // environment the tool already said it could not build.
    for step in &item.plan.prelude {
        let mut prelude = std::process::Command::new(&program);
        prelude.args(step).current_dir(cwd).env("PATH", &path);
        for (key, value) in &item.plan.env {
            prelude.env(key, value);
        }
        run_bounded(prelude, item.config.timeout)?;
    }
    run_bounded(command, item.config.timeout)?;
    Ok(())
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
    let command = freshness_command(&item.plan, &item.config, &resolved_tool_versions(app));
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

    let lock_path = crate::lockfile::default_path(&item.project.root);
    crate::lockfile::merge_deps(
        &lock_path,
        &item.project.provider,
        crate::lockfile::DepsRecord {
            installer: item.choice.provider.to_string(),
            installer_version,
            runtime,
            project_root: &item.project.root,
            manifest: &item.project.manifest,
            native_lock: item.project.native_lock.as_deref(),
            run: command_line(&item.plan),
            index: item.config.index.clone(),
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
            let local = cwd.starts_with(&entry.project.root) && nearest;
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
fn resolved_tool_versions(app: &App) -> BTreeMap<String, String> {
    let mut versions = app.ctx.config.tools.clone();
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
    let command = freshness_command(plan, config, &resolved_tool_versions(app));
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
    plan: &RunPlan,
    config: &ProviderConfig,
    versions: &BTreeMap<String, String>,
) -> String {
    format!("{plan:?}|{config:?}|{versions:?}")
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

/// The effective command, including env, because env is part of *what runs*:
/// yarn berry disables build scripts through `YARN_ENABLE_SCRIPTS`, so a command
/// string without env would hash two materially different runs the same.
fn command_line(plan: &RunPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in &plan.env {
        parts.push(format!("{key}={value}"));
    }
    let program = plan.program.clone();
    // Prelude steps are part of *what runs*, so they belong in the string that
    // gets hashed. Leaving them out would hash "create the environment, then
    // sync" and "sync into whatever is already there" identically.
    for step in &plan.prelude {
        parts.push(format!("{program} {}", step.join(" ")));
        parts.push("&&".to_string());
    }
    parts.push(program);
    parts.extend(plan.args.iter().map(|arg| {
        if arg.chars().any(char::is_whitespace) {
            serde_json::to_string(arg).unwrap()
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
    use super::{auto_options, command_line, resolve_program, ReadyTool, ToolRole};
    use osdk_core::deps::{
        node, DetectedProject, Ecosystem, InstallerChoice, InstallerOrigin, ProviderConfig,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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
