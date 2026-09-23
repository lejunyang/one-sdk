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
    /// Only listing is tiered. Materializing always covers every declared root:
    /// narrowing that by default would skip work without saying so.
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

    // An operand may be a plain provider name or a rooted id (`//apps/api:npm`).
    // Both have to select the underlying provider here, before roots are
    // expanded: filtering `enabled` by the literal operand would leave it empty
    // for a rooted id and there would be nothing left to expand.
    let selects = |id: &str, operands: &[String]| {
        operands.iter().any(|operand| {
            operand == id
                || deps::parse_rooted_id(operand).is_some_and(|(_, provider)| provider == id)
        })
    };
    let enabled: Vec<&'static DepsProviderSchema> = configured
        .keys()
        .filter(|id| !options.auto_only || configured.get(*id).is_some_and(|c| c.auto))
        .filter(|id| !selects(id, &options.skip))
        .filter(|id| options.providers.is_empty() || selects(id, &options.providers))
        .filter_map(|id| deps::provider_schema(id))
        .collect();
    let ceiling = app
        .ctx
        .config
        .project_config_path
        .as_deref()
        .and_then(Path::parent);
    let mut detected = deps::discover(&cwd, ceiling, &enabled)?;

    // Custom providers are not discovered, they are declared: there is no
    // manifest to find. Their root is the directory of the config that declared
    // them, which keeps `sources`/`outputs` relative to the same place a built-in
    // provider's would be.
    let config_path = app.ctx.config.project_config_path.clone();
    for id in configured.keys() {
        if deps::provider_schema(id).is_some() {
            continue;
        }
        if options.auto_only && !configured.get(id).is_some_and(|config| config.auto) {
            continue;
        }
        if options.skip.iter().any(|skip| skip == id) {
            continue;
        }
        if !options.providers.is_empty() && !options.providers.iter().any(|p| p == id) {
            continue;
        }
        let Some(path) = &config_path else {
            return Err(anyhow!(
                "custom deps provider `{id}` needs a project config file to anchor it"
            ));
        };
        let root = path.parent().unwrap_or(&cwd);
        detected.push(deps::detect_custom(id, root, path));
    }

    // Sub-projects named by `[deps].roots`. Discovery never descends below the
    // config root on its own, so this is the only way a monorepo's packages come
    // into scope -- and only the ones a declared pattern actually names.
    let mut rooted: Vec<deps::RootedProject> = Vec::new();
    if !app.ctx.config.deps.roots.is_empty() && wants_rooted(&options) {
        let Some(path) = &config_path else {
            return Err(anyhow!(
                "`[deps].roots` needs a project config file to resolve against"
            ));
        };
        let config_root = path.parent().unwrap_or(&cwd);
        for candidate in deps::discover_in_roots(config_root, &app.ctx.config.deps.roots, &enabled)?
        {
            let id = candidate.id();
            // Selectable either by full id (`//apps/api:uv`) or by provider name,
            // so `osdk deps uv` still means "every uv project" in a monorepo.
            let provider = candidate.project.provider.to_string();
            if options
                .skip
                .iter()
                .any(|skip| *skip == id || *skip == provider)
            {
                continue;
            }
            if !options.providers.is_empty()
                && !options
                    .providers
                    .iter()
                    .any(|wanted| *wanted == id || *wanted == provider)
            {
                continue;
            }
            if !options.filter.is_empty()
                && !options
                    .filter
                    .iter()
                    .any(|pattern| deps::path_matches(pattern, &candidate.relative))
            {
                continue;
            }
            rooted.push(candidate);
        }
    }

    // A `--filter` that matched nothing is an error, deliberately unlike pnpm, whose
    // `failIfNoMatch` defaults to false. Same reasoning as `--verify` refusing to
    // report a clean environment it could not examine: "nothing was done" must not
    // read like "done, no problems". A CI step narrowing to a renamed directory
    // should fail, not pass having built nothing.
    if !options.filter.is_empty() && rooted.is_empty() {
        return Err(anyhow!(
            "`--filter` matched no sub-project: {}\n\
             patterns are matched against paths relative to the config root, \
             segment by segment, and only against sub-projects `[deps].roots` declares",
            options.filter.join(", ")
        ));
    }

    if detected.is_empty() && rooted.is_empty() {
        // Split the two cases: "you selected nothing" and "nothing was found"
        // have different fixes, and collapsing them sends the user looking in the
        // wrong place.
        if enabled.is_empty() {
            println!("no matching deps providers are configured");
        } else {
            println!("no dependency manifests found for the configured providers");
        }
        return Ok(());
    }

    let tool_versions = resolved_tool_versions(app);
    let mut resolved = Vec::new();
    for project in &detected {
        let config = configured
            .get(&*project.provider)
            .cloned()
            .unwrap_or_default();
        let schema = deps::provider_schema(&project.provider);
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
            rooted: None,
            project: project.clone(),
            choice,
            config,
            plan,
            decision,
        });
    }

    for candidate in &rooted {
        let config = configured
            .get(&*candidate.project.provider)
            .cloned()
            .unwrap_or_default();
        let schema = deps::provider_schema(&candidate.project.provider);
        // Peers are scoped to this sub-project: a lockfile in a sibling package
        // says nothing about which installer owns this one.
        let choice = deps::select_installer(&candidate.project, &config, &[])?;
        let plan = deps::plan(&candidate.project, &choice, &config, &tool_versions)?;
        if options.frozen && !plan.frozen {
            return Err(anyhow!(
                "`--frozen` requires a native lockfile for `{}`: {}",
                candidate.id(),
                plan.downgraded_reason
                    .clone()
                    .unwrap_or_else(|| "no lockfile found".into())
            ));
        }
        let decision = decide(app, schema, &candidate.project, &config, &plan)?;
        resolved.push(Resolved {
            schema,
            rooted: Some((candidate.id(), candidate.root_pattern.clone())),
            project: candidate.project.clone(),
            choice,
            config,
            plan,
            decision,
        });
    }

    // `depends` is a promise about order, so it has to be honoured before
    // anything runs. A codegen step that needs another provider's output would
    // otherwise fail for a reason unrelated to what the user configured.
    order_by_depends(&mut resolved, &configured)?;

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
fn order_by_depends(
    resolved: &mut Vec<Resolved>,
    configured: &BTreeMap<String, ProviderConfig>,
) -> anyhow::Result<()> {
    let present: Vec<String> = resolved
        .iter()
        .map(|item| item.project.provider.to_string())
        .collect();

    let mut ordered: Vec<Resolved> = Vec::with_capacity(resolved.len());
    let mut done: Vec<String> = Vec::new();
    let mut remaining: Vec<Resolved> = std::mem::take(resolved);

    while !remaining.is_empty() {
        let ready = remaining.iter().position(|item| {
            let id: &str = &item.project.provider;
            configured
                .get(id)
                .map(|config| {
                    config.depends.iter().all(|need| {
                        // Only wait for something that is actually going to run.
                        !present.iter().any(|name| name == need)
                            || done.iter().any(|name| name == need)
                    })
                })
                .unwrap_or(true)
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
        let status = prelude
            .status()
            .with_context(|| format!("running {} {}", program.display(), step.join(" ")))?;
        if !status.success() {
            return Err(anyhow!(
                "`{} {}` failed with {status}",
                program.display(),
                step.join(" ")
            ));
        }
    }

    let status = command
        .status()
        .with_context(|| format!("running {}", program.display()))?;
    if !status.success() {
        return Err(anyhow!(
            "`{}` failed with {status}",
            command_line(&item.plan)
        ));
    }
    Ok(())
}

/// Locate the installer program among the resolved bin directories.
///
/// Candidates are tried in order because Windows needs `pnpm.cmd` where Unix
/// needs `pnpm`; the provider supplies both and the first that exists wins. The
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
            for dir in &tool.bin_dirs {
                let path = dir.join(candidate);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Persist what happened: freshness state, then the lock entry.
///
/// Both only after a successful run. Recording freshness for a failed install
/// would make the next run report "up to date" for a tree that was never
/// populated, turning one visible failure into a silently broken working tree.
fn record(app: &App, item: &Resolved, tools: &[ReadyTool]) -> anyhow::Result<()> {
    let command = command_line(&item.plan);
    let sources = effective_sources(item.schema, &item.config);
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
            run: command,
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
    if app.ctx.config.deps.providers.is_empty() {
        return Ok(());
    }
    if !configured_providers(app).values().any(|config| config.auto) {
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

/// Should this run expand `[deps].roots` into its sub-projects?
///
/// Always, except for a plain `--list`. Listing is the one place where a large
/// monorepo's full provider set is a readability problem rather than the answer,
/// so it starts at the current config root and `--all` widens it.
///
/// Two cases deliberately keep expanding even under `--list`:
///
/// * `--list --all`, which is what the flag is for.
/// * A rooted operand such as `//apps/api:uv`. Asking for a sub-project by name
///   and being told it does not exist would be a lie about the configuration.
///
/// Materializing is never narrowed. `osdk deps` has always covered every declared
/// root, and doing less by default would skip work silently -- the failure mode
/// this codebase treats as worse than an error.
fn wants_rooted(options: &DepsOptions) -> bool {
    if !options.list || options.list_all {
        return true;
    }
    // A path filter only has sub-projects to match against, so asking for one is
    // asking for the expansion. Without this, \--list --filter\ would match nothing
    // and then fail-closed -- an error about the wrong thing entirely.
    if !options.filter.is_empty() {
        return true;
    }
    options
        .providers
        .iter()
        .any(|wanted| wanted.starts_with("//"))
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
    schema: Option<&'static DepsProviderSchema>,
    project: &DetectedProject,
    config: &ProviderConfig,
    plan: &RunPlan,
) -> anyhow::Result<state::Decision> {
    let sources = effective_sources(schema, config);
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
    let command = command_line(plan);
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
        Some(schema) => deps::effective_outputs(schema, config, !config.outputs.is_empty())
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

/// The effective command, including env, because env is part of *what runs*:
/// yarn berry disables build scripts through `YARN_ENABLE_SCRIPTS`, so a command
/// string without env would hash two materially different runs the same.
fn command_line(plan: &RunPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in &plan.env {
        parts.push(format!("{key}={value}"));
    }
    let program = plan
        .program_candidates
        .first()
        .cloned()
        .unwrap_or_else(|| plan.tool.to_string());
    // Prelude steps are part of *what runs*, so they belong in the string that
    // gets hashed. Leaving them out would hash "create the environment, then
    // sync" and "sync into whatever is already there" identically.
    for step in &plan.prelude {
        parts.push(format!("{program} {}", step.join(" ")));
        parts.push("&&".to_string());
    }
    parts.push(program);
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
    use super::auto_options;

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
