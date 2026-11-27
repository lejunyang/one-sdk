//! `osdk skills` command handlers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osdk_core::config::SkillDeclaration;
use osdk_core::skills;
use osdk_core::skills::{agent_target, install, AgentTarget, SkillSource, AGENT_TARGETS};
use osdk_core::store::link::LinkMode;

use crate::app::App;
use crate::cli::SkillsCommand;
use crate::lockfile::{self, LockedSkill};

pub async fn run(app: &mut App, command: SkillsCommand) -> Result<()> {
    let default_global = match app.ctx.config.skills_defaults.scope.as_deref() {
        None => None,
        Some("project") => Some(false),
        Some("global") => Some(true),
        Some(value) => anyhow::bail!(
            "invalid `[skills].scope` value `{value}`: expected `project` or `global`"
        ),
    };
    let effective_global = |flag| flag || default_global.unwrap_or(false);
    match command {
        SkillsCommand::Agents => agents(),
        SkillsCommand::List { global } => list(app, effective_global(global)),
        SkillsCommand::Path { name } => path(app, &name, default_global),
        SkillsCommand::Add {
            source,
            skills,
            agents,
            global,
            copy,
            r#ref,
            list,
            no_lock,
        } => {
            add(
                app,
                AddArgs {
                    source,
                    skills,
                    agents,
                    global: effective_global(global),
                    copy,
                    reference: r#ref,
                    list_only: list,
                    no_lock,
                },
            )
            .await
        }
        SkillsCommand::Remove {
            name,
            global,
            agents,
        } => remove(app, &name, effective_global(global), &agents),
        SkillsCommand::Sync { global } => sync(app, effective_global(global)).await,
        SkillsCommand::Update { skills, global } => {
            update(app, &skills, effective_global(global)).await
        }
        SkillsCommand::Use {
            source,
            skill,
            agent,
            r#ref,
        } => {
            use_skill(
                app,
                &source,
                skill.as_deref(),
                agent.as_deref(),
                r#ref.as_deref(),
            )
            .await
        }
        SkillsCommand::Init { name } => init(name.as_deref()),
        SkillsCommand::Find {
            query,
            owner,
            limit,
        } => find(app, &query.join(" "), owner.as_deref(), limit).await,
    }
}

/// Print the agents osdk can install into and where each keeps its skills.
fn agents() -> Result<()> {
    println!("Agents osdk can install skills into:\n");
    for target in AGENT_TARGETS {
        let global = target.global.unwrap_or("(project only)");
        println!(
            "  {:<16} project: {:<20} global: {}",
            target.id, target.project, global
        );
    }
    println!(
        "\nUse -a/--agent to pick; several agents share `.agents/skills`, so one \
         install can serve them all."
    );
    Ok(())
}

/// List locked skills and installations found in the project's Agent directories.
fn list(app: &App, global: bool) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    let skills = if lock_path.is_file() {
        lockfile::load(&lock_path)?.skills
    } else {
        Default::default()
    };
    let agent_dirs = project_agent_dirs(app, global)?;
    let mut unlocked = BTreeMap::<String, Vec<PathBuf>>::new();
    for dir in &agent_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            if !skills.contains_key(&name) && path.join(install::SKILL_MANIFEST).is_file() {
                unlocked.entry(name).or_default().push(path);
            }
        }
    }
    let recovery = legacy_skill_recovery(app, global, None, &skills, &agent_dirs);
    if skills.is_empty() && unlocked.is_empty() && recovery.is_none() {
        println!("No skills installed.");
    }
    for (name, entry) in &skills {
        let agents = if entry.agents.is_empty() {
            "-".to_string()
        } else {
            entry.agents.join(", ")
        };
        println!("{name}  <- {}  [{}]", entry.source, agents);
    }
    for (name, paths) in unlocked {
        println!("{}", unlocked_skill_message(&name, &paths));
    }
    if let Some(hint) = recovery {
        eprintln!("{hint}");
    }
    Ok(())
}

fn project_agent_dirs(app: &App, global: bool) -> Result<Vec<PathBuf>> {
    if global {
        return Ok(Vec::new());
    }
    let root = scope_root(app, false)?;
    Ok(agent_choices(false)
        .into_iter()
        .map(|choice| choice.members[0].project_dir(&root))
        .collect())
}

fn unlocked_skill_message(name: &str, paths: &[PathBuf]) -> String {
    let locations = paths
        .iter()
        .map(|path| format!("`{}`", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("skill `{name}` is not recorded in the lock; found in project Agent directories: {locations}")
}

/// Older releases could record a child's installation in an ancestor lock.
/// Report candidate entries for names absent from the current project.
fn legacy_skill_recovery(
    app: &App,
    global: bool,
    wanted: Option<&str>,
    recorded: &BTreeMap<String, LockedSkill>,
    agent_dirs: &[PathBuf],
) -> Option<String> {
    let project = app.ctx.config.project.as_ref()?;
    if global || project.is_external() {
        return None;
    }
    let repository = project
        .root
        .ancestors()
        .find(|directory| directory.join(".git").exists())?;
    let ancestors = project
        .root
        .ancestors()
        .skip(1)
        .take_while(|directory| directory.starts_with(repository))
        .map(|directory| directory.join(lockfile::LOCKFILE_NAME))
        .filter(|path| path.is_file());
    let mut hints = Vec::new();
    for ancestor in ancestors {
        // A closer lock may have been created later or may be unreadable.
        let Ok(lock) = lockfile::load(&ancestor) else {
            continue;
        };
        let candidates = lock.skills.keys().filter(|name| {
            wanted.is_none_or(|wanted| wanted == *name)
                && !recorded.contains_key(*name)
                && skills::validate_skill_name(name).is_ok()
                && !agent_dirs.iter().any(|dir| dir.join(name).is_dir())
        });
        for name in candidates {
            hints.push(format!(
                "Found [skills.{name:?}] in ancestor lock `{}`. Verify which project owns this entry.",
                ancestor.display()
            ));
        }
    }
    (!hints.is_empty()).then(|| hints.join("\n"))
}

/// Print the staged store path of an installed skill.
fn path(app: &App, name: &str, default_global: Option<bool>) -> Result<()> {
    osdk_core::skills::validate_skill_name(name)?;
    let scopes: &[bool] = match default_global {
        Some(true) => &[true],
        Some(false) => &[false],
        None => &[false, true],
    };
    // Prefer the recorded content hash so the path is the exact staged copy.
    for &global in scopes {
        let lock_path = lock_path_for(app, global);
        if !lock_path.is_file() {
            continue;
        }
        if let Some(entry) = lockfile::load(&lock_path)?.skills.get(name) {
            let staged = install::staged_root(&app.ctx.dirs, &entry.source, &entry.content_hash);
            if staged.exists() {
                println!("{}", staged.display());
                return Ok(());
            }
        }
    }
    anyhow::bail!("skill `{name}` is not installed (no staged copy found)")
}

struct AddArgs {
    source: String,
    skills: Vec<String>,
    agents: Vec<String>,
    global: bool,
    copy: bool,
    reference: Option<String>,
    list_only: bool,
    no_lock: bool,
}

async fn add(app: &mut App, args: AddArgs) -> Result<()> {
    let source = resolve_source(
        app,
        &args.source,
        None,
        &args.skills,
        args.reference.as_deref(),
    )
    .await?;

    if args.list_only {
        println!("Skills offered by {}:", source.canonical);
        for (name, root) in &source.roots {
            let package = install::read_skill_dir(root)?;
            println!("  {name}  — {}", package.description);
        }
        return Ok(());
    }

    let scope_root = scope_root(app, args.global)?;
    let targets = resolve_agents(app, &args.agents, &scope_root, args.global).context(
        "selecting target agents for `skills add`; pass -a/--agent to choose explicitly",
    )?;
    let target_dirs = agent_dirs_by_path(&targets, &scope_root, args.global)?;
    let mode = add_link_mode(app, args.copy, target_dirs.len())?;
    let installation = SkillInstallation::prepare(source, targets, target_dirs, mode, None)?;
    installation.preview();
    if !app.prompt.confirm("Proceed with installation?")? {
        println!("Installation cancelled.");
        return Ok(());
    }

    let lock_entries = installation.install(app)?;
    if !args.no_lock {
        write_lock_entries(&lock_path_for(app, args.global), lock_entries)?;
    }
    Ok(())
}

struct ResolvedSource {
    canonical: String,
    roots: Vec<(String, PathBuf)>,
    requested_ref: Option<String>,
    resolved_commit: Option<String>,
}

async fn resolve_source(
    app: &App,
    input: &str,
    base: Option<&Path>,
    wanted: &[String],
    reference: Option<&str>,
) -> Result<ResolvedSource> {
    let source = SkillSource::parse(input)?;
    let (roots, resolved_commit) = match &source {
        SkillSource::Local { path } => {
            if reference.is_some() {
                anyhow::bail!("--ref applies only to a GitHub source, not a local path");
            }
            let root = match base {
                Some(base) if !path.is_absolute() => {
                    base.join(path.to_string_lossy().replace('\\', "/"))
                }
                _ => path.clone(),
            };
            (local_roots(&root, wanted)?, None)
        }
        SkillSource::GitHub {
            owner,
            repo,
            subdir,
        } => {
            let commit = skills::fetch::resolve_commit(&app.ctx, owner, repo, reference)
                .await
                .with_context(|| format!("resolving {owner}/{repo}"))?;
            println!(
                "Resolved {owner}/{repo} to commit {}",
                &commit[..commit.len().min(12)]
            );
            let tree = skills::fetch::fetch_at_commit(&app.ctx, owner, repo, &commit)
                .await
                .with_context(|| format!("downloading {owner}/{repo}@{commit}"))?;
            let root = skills::fetch::subtree(&tree, subdir.as_deref())?;
            (local_roots(&root, wanted)?, Some(commit))
        }
    };
    Ok(ResolvedSource {
        canonical: source.canonical(),
        roots,
        requested_ref: resolved_commit
            .as_ref()
            .and_then(|_| normalized_requested_ref(reference)),
        resolved_commit,
    })
}

type AgentDirs = BTreeMap<PathBuf, Vec<&'static AgentTarget>>;

struct PreparedSkill {
    name: String,
    selector: String,
    package: install::SkillPackage,
}

struct SkillInstallation {
    source: ResolvedSource,
    skills: Vec<PreparedSkill>,
    targets: Vec<&'static AgentTarget>,
    target_dirs: AgentDirs,
    mode: LinkMode,
}

impl SkillInstallation {
    fn prepare(
        source: ResolvedSource,
        targets: Vec<&'static AgentTarget>,
        target_dirs: AgentDirs,
        mode: LinkMode,
        declaration: Option<(&str, &str)>,
    ) -> Result<Self> {
        let mut skills = Vec::new();
        for (selector, root) in &source.roots {
            let package = install::read_skill_dir(root)?;
            let (name, selector) = declaration.unwrap_or((&package.name, selector));
            for directory in target_dirs.keys() {
                install::validate_source_destination(root, &directory.join(name))?;
            }
            skills.push(PreparedSkill {
                name: name.to_string(),
                selector: selector.to_string(),
                package,
            });
        }
        Ok(Self {
            source,
            skills,
            targets,
            target_dirs,
            mode,
        })
    }

    fn preview(&self) {
        println!("Installation summary:");
        for skill in &self.skills {
            println!("  {}", skill.package.preview());
            for (agent_dir, owners) in &self.target_dirs {
                let destination = agent_dir.join(&skill.name);
                let overwrites = std::fs::symlink_metadata(&destination).is_ok();
                println!(
                    "    {} -> {}{}",
                    target_ids(owners).collect::<Vec<_>>().join(", "),
                    destination.display(),
                    if overwrites {
                        " (overwrites existing)"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    fn replaces_existing_skill(&self) -> bool {
        self.skills.iter().any(|skill| {
            self.target_dirs
                .keys()
                .any(|agent_dir| agent_dir.join(&skill.name).symlink_metadata().is_ok())
        })
    }

    fn install(&self, app: &App) -> Result<Vec<(String, LockedSkill)>> {
        let mut entries = Vec::new();
        for skill in &self.skills {
            let name = &skill.name;
            let staged = install::stage(
                &app.ctx.dirs,
                &self.source.canonical,
                &skill.package,
                self.mode,
            )
            .with_context(|| format!("staging skill `{name}`"))?;
            for (agent_dir, owners) in &self.target_dirs {
                install::link_into(agent_dir, name, &staged, self.mode).with_context(|| {
                    format!(
                        "linking `{name}` into {}",
                        target_ids(owners).collect::<Vec<_>>().join(", ")
                    )
                })?;
                println!("  installed into {}", agent_dir.display());
            }

            entries.push((
                name.clone(),
                LockedSkill {
                    source: self.source.canonical.clone(),
                    content_hash: skill.package.content_hash(),
                    requested_ref: self.source.requested_ref.clone(),
                    resolved_commit: self.source.resolved_commit.clone(),
                    skill: (skill.selector != *name).then(|| skill.selector.clone()),
                    agents: target_ids(&self.targets).map(str::to_string).collect(),
                    install_mode: Some(self.mode),
                },
            ));
        }
        Ok(entries)
    }
}

fn target_ids<'a>(targets: &'a [&AgentTarget]) -> impl Iterator<Item = &'static str> + 'a {
    targets.iter().map(|target| target.id)
}

/// Remove a skill from its agents and drop it from the lock.
fn remove(app: &mut App, name: &str, global: bool, only_agents: &[String]) -> Result<()> {
    osdk_core::skills::validate_skill_name(name)?;
    let lock_path = lock_path_for(app, global);
    let has_lock = lock_path.is_file();
    let mut lock = if has_lock {
        lockfile::load(&lock_path)?
    } else {
        lockfile::Lockfile::default()
    };
    let Some(entry) = lock.skills.get(name).cloned() else {
        let agent_dirs = project_agent_dirs(app, global)?;
        let installed_paths: Vec<_> = agent_dirs
            .iter()
            .map(|dir| dir.join(name))
            .filter(|path| path.is_dir())
            .collect();
        if !installed_paths.is_empty() {
            anyhow::bail!("{}", unlocked_skill_message(name, &installed_paths));
        }
        if let Some(hint) =
            legacy_skill_recovery(app, global, Some(name), &lock.skills, &agent_dirs)
        {
            anyhow::bail!("{hint}");
        }
        if has_lock {
            anyhow::bail!("skill `{name}` is not recorded in the lock");
        }
        anyhow::bail!("no lockfile; nothing to remove");
    };

    let scope_root = scope_root(app, global)?;
    let remove_from: Vec<String> = if only_agents.is_empty() {
        entry.agents.clone()
    } else {
        only_agents.to_vec()
    };
    // Agents that keep the skill after this removal.
    let remaining: Vec<String> = entry
        .agents
        .iter()
        .filter(|a| !remove_from.contains(a))
        .cloned()
        .collect();

    let removed_any = unlink_agents(name, &remove_from, &remaining, &scope_root, global)?;

    // Update the lock: drop the whole entry when no agent still holds it,
    // otherwise keep it with the remaining agents.
    if remaining.is_empty() {
        lock.skills.remove(name);
    } else if let Some(existing) = lock.skills.get_mut(name) {
        existing.agents = remaining;
    }
    lockfile::save(&lock_path, &lock)?;

    if !removed_any {
        println!("`{name}` was not linked into any of the targeted agents");
    }
    Ok(())
}

/// Unlink `name` from `remove_from`, keeping files a `remaining` agent shares.
///
/// Several agents share one skills directory (codex / cursor / opencode all
/// use `.agents/skills`), so a plan decides which removed agents physically
/// unlink and which only drop ownership because a *remaining* agent still
/// points at the same directory. Returns whether anything was dropped.
fn unlink_agents(
    name: &str,
    remove_from: &[String],
    remaining: &[String],
    scope_root: &Path,
    global: bool,
) -> Result<bool> {
    let plan = plan_agent_unlinks(remove_from, remaining, scope_root, global)?;
    let mut removed_any = false;
    for step in &plan {
        if !step.physically_unlink || install::unlink_from(&step.dir(scope_root, global)?, name)? {
            removed_any = true;
            step.report(name);
        }
    }
    Ok(removed_any)
}

fn plan_agent_unlinks(
    remove_from: &[String],
    remaining: &[String],
    scope_root: &Path,
    global: bool,
) -> Result<Vec<UnlinkStep>> {
    let mut dir_of: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for agent_id in remove_from.iter().chain(remaining.iter()) {
        let Some(target) = agent_target(agent_id) else {
            if remove_from.contains(agent_id) {
                anyhow::bail!("unknown agent `{agent_id}`");
            }
            continue;
        };
        let dir = agent_dir(target, scope_root, global)?;
        let canonical = dir.canonicalize().unwrap_or(dir);
        dir_of.insert(agent_id.clone(), canonical);
    }
    Ok(plan_unlinks(remove_from, remaining, &dir_of))
}

/// The agents the project now declares for `name`, when it declares any.
///
/// Same fallback as installing: the entry's own `agents`, else
/// `[skills].default_agents`. `None` means the configuration says nothing, so
/// the lock's record stands.
fn declared_agents(app: &App, name: &str) -> Option<Vec<String>> {
    let declaration = app.ctx.config.skills.get(name)?;
    let agents = if declaration.agents.is_empty() {
        &app.ctx.config.skills_defaults.default_agents
    } else {
        &declaration.agents
    };
    (!agents.is_empty()).then(|| agents.clone())
}

/// Prepare a replacement for the final set of agents, or use the locked
/// bytes for newly declared agents. Dropped agents join the same transaction.
async fn prepare_skill_targets<'a>(
    app: &App,
    name: &'a str,
    entry: &mut LockedSkill,
    scope_root: &Path,
    global: bool,
    mode: LinkMode,
    replacement: Option<PathBuf>,
) -> Result<(install::SkillTargetChanges<'a>, Vec<UnlinkStep>)> {
    let declared = declared_agents(app, name).unwrap_or_else(|| entry.agents.clone());
    let removed: Vec<String> = entry
        .agents
        .iter()
        .filter(|agent| !declared.contains(agent))
        .cloned()
        .collect();
    let added: Vec<String> = declared
        .iter()
        .filter(|agent| !entry.agents.contains(agent))
        .cloned()
        .collect();
    for agent in &added {
        agent_target(agent).with_context(|| {
            format!("unknown agent `{agent}` (run `osdk skills agents` for the list)")
        })?;
    }

    let installation = match replacement {
        Some(staged) => Some((&declared, staged)),
        None if !added.is_empty() => {
            let mut staged =
                install::staged_root(&app.ctx.dirs, &entry.source, &entry.content_hash);
            if !staged.exists() {
                staged = refetch_staged(app, name, entry, mode)
                    .await?
                    .with_context(|| {
                        format!("skill `{name}` has no staged copy to install for the added agents")
                    })?;
            }
            Some((&added, staged))
        }
        None => None,
    };
    let mut changes = install::SkillTargetChanges::new(name)?;
    if let Some((agents, staged)) = installation {
        let targets = agents
            .iter()
            .filter_map(|agent| agent_target(agent))
            .collect::<Vec<_>>();
        for agent_dir in agent_dirs_by_path(&targets, scope_root, global)?.keys() {
            changes.replace(agent_dir, &staged, mode)?;
        }
    }
    let mut unlinks = Vec::new();
    if !removed.is_empty() {
        for step in plan_agent_unlinks(&removed, &declared, scope_root, global)? {
            if !step.physically_unlink || changes.remove(&step.dir(scope_root, global)?)? {
                unlinks.push(step);
            }
        }
    }
    if !removed.is_empty() || !added.is_empty() {
        entry.agents = declared;
    }
    Ok((changes, unlinks))
}

fn platform_exclusion(app: &App, declaration: &SkillDeclaration) -> Option<String> {
    declaration
        .when
        .as_ref()
        .filter(|filter| !filter.matches(&app.ctx.platform))
        .map(|filter| filter.describe())
}

fn skipped_skill_message(subject: &str, name: &str, reason: &str) -> String {
    format!("skipping {subject} `{name}`: its `when` filter excludes this platform ({reason})")
}

fn report_skipped_locked(name: &str, reason: &str) {
    println!(
        "{}; keeping its lock entry",
        skipped_skill_message("locked skill", name, reason)
    );
}

async fn prepare_declared_skill(
    app: &App,
    name: &str,
    declaration: &SkillDeclaration,
    scope_root: &Path,
    global: bool,
) -> Result<SkillInstallation> {
    skills::validate_skill_name(name)?;
    if declaration.endpoint.is_some() {
        anyhow::bail!("`[skills.{name}].endpoint` is not supported yet");
    }
    let targets = resolve_agents(app, &declaration.agents, scope_root, global).with_context(|| {
        format!("selecting target agents for `[skills.{name}]`; set its `agents` to choose explicitly")
    })?;
    let target_dirs = agent_dirs_by_path(&targets, scope_root, global)?;
    let mode = match app.ctx.config.skills_defaults.link_mode.as_deref() {
        Some(configured) => configured.parse()?,
        None if target_dirs.len() == 1 => LinkMode::Copy,
        None => LinkMode::Auto,
    };
    let selector = declaration.skill.as_deref().unwrap_or(name);
    let source = resolve_source(
        app,
        &declaration.source,
        declaration.source_dir.as_deref(),
        &[selector.to_string()],
        declaration.r#ref.as_deref(),
    )
    .await?;
    if source.roots.len() != 1 {
        anyhow::bail!(
            "expected one skill for selector `{selector}` in {}",
            source.canonical
        );
    }
    SkillInstallation::prepare(source, targets, target_dirs, mode, Some((name, selector)))
}

fn report_declaration_changes(
    app: &App,
    name: &str,
    declaration: &SkillDeclaration,
    entry: &LockedSkill,
    flag: &str,
) {
    let source_matches = SkillSource::parse(&declaration.source)
        .is_ok_and(|source| source.canonical() == entry.source);
    if !source_matches
        || declaration.skill.as_deref().unwrap_or(name) != entry.skill.as_deref().unwrap_or(name)
    {
        println!(
            "skill `{name}`: source or skill selection differs from osdk.lock; using the lock. \
             To replace it, run `osdk skills remove {name}{flag}` then `osdk skills sync{flag}`."
        );
    }
    let ref_changed = entry.resolved_commit.is_some()
        && normalized_requested_ref(declaration.r#ref.as_deref()).as_deref()
            != Some(entry.requested_ref.as_deref().unwrap_or("default"));
    let agents_changed = declared_agents(app, name).is_some_and(|agents| {
        agents.iter().any(|agent| !entry.agents.contains(agent))
            || entry.agents.iter().any(|agent| !agents.contains(agent))
    });
    if ref_changed || agents_changed {
        println!(
            "skill `{name}`: ref or agents differ from osdk.lock; using the lock. \
             Run `osdk skills update {name}{flag}` to apply the change."
        );
    }
}

/// Install missing declarations, then reproduce the skills that were already locked.
async fn sync(app: &App, global: bool) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    let has_lock = lock_path.is_file();
    let skills = if has_lock {
        lockfile::load(&lock_path)?.skills
    } else {
        BTreeMap::new()
    };
    let declarations = &app.ctx.config.skills;
    let flag = if global { " -g" } else { "" };
    let mut pending = Vec::new();
    for (name, declaration) in declarations
        .iter()
        .filter(|(name, _)| !skills.contains_key(*name))
    {
        if let Some(reason) = platform_exclusion(app, declaration) {
            println!("{}", skipped_skill_message("declaration", name, &reason));
        } else {
            pending.push((name, declaration));
        }
    }
    if pending.is_empty() && skills.is_empty() {
        if has_lock {
            println!("No skills in the lock; nothing to sync.");
        } else {
            println!("No lockfile; nothing to sync.");
        }
        return Ok(());
    }

    let scope_root = scope_root(app, global)?;
    let mut installations = Vec::new();
    for (name, declaration) in pending {
        let installation = prepare_declared_skill(app, name, declaration, &scope_root, global)
            .await
            .with_context(|| format!("preparing skill `{name}` from project config"))?;
        installations.push(installation);
    }
    for installation in &installations {
        installation.preview();
    }
    if installations
        .iter()
        .any(SkillInstallation::replaces_existing_skill)
        && !app
            .prompt
            .confirm("Replace existing unlocked skill files?")
            .context("move the existing skill directories aside before syncing to keep them")?
    {
        println!("Installation cancelled.");
        return Ok(());
    }
    let mut entries = Vec::new();
    for installation in installations {
        entries.extend(installation.install(app)?);
    }
    if !entries.is_empty() {
        write_lock_entries(&lock_path, entries)?;
    }

    // Reproduce each skill from its staged copy. When the copy is gone, a GitHub
    // source is re-fetched at its recorded commit and re-staged; the content hash
    // is then re-checked so a moved tag or a tampered mirror cannot silently swap
    // the bytes. A local source that has lost its staged copy cannot be replayed
    // (its origin path may be gone), so it is reported rather than guessed at.
    let fallback_mode = link_mode(app, false);
    let mut missing = Vec::new();
    for (name, entry) in &skills {
        match declarations.get(name) {
            Some(declaration) => {
                if let Some(reason) = platform_exclusion(app, declaration) {
                    report_skipped_locked(name, &reason);
                    continue;
                }
                report_declaration_changes(app, name, declaration, entry, flag);
            }
            None if !declarations.is_empty() => println!(
                "skill `{name}` is locked without a declaration in osdk.toml; using the lock. \
                 Run `osdk skills remove {name}{flag}` to remove it."
            ),
            None => {}
        }
        let mode = install_mode_for_entry(entry, name, &scope_root, global, fallback_mode)?;
        let mut staged = install::staged_root(&app.ctx.dirs, &entry.source, &entry.content_hash);
        if !staged.exists() {
            match refetch_staged(app, name, entry, mode).await {
                Ok(Some(path)) => staged = path,
                Ok(None) => {
                    missing.push(name.clone());
                    continue;
                }
                Err(error) => {
                    return Err(error.context(format!("re-fetching skill `{name}` for sync")));
                }
            }
        }
        let targets = entry
            .agents
            .iter()
            .filter_map(|agent_id| agent_target(agent_id))
            .collect::<Vec<_>>();
        for agent_dir in agent_dirs_by_path(&targets, &scope_root, global)?.keys() {
            install::link_into(agent_dir, name, &staged, mode)?;
        }
        println!("synced `{name}` -> {}", entry.agents.join(", "));
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "cannot reproduce {} skill(s) with no staged copy and no re-fetchable source: {}",
            missing.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// Re-stage a skill whose CAS copy is gone, from its recorded source.
///
/// Returns the staged path on success, `None` when the source cannot be
/// re-fetched (a local source, whose origin may no longer exist). The re-staged
/// content hash must match what the lock recorded, or the sync fails closed: a
/// moved ref or a substituted mirror must not quietly change what is installed.
async fn refetch_staged(
    app: &App,
    name: &str,
    entry: &LockedSkill,
    mode: LinkMode,
) -> Result<Option<PathBuf>> {
    let source = SkillSource::parse(&entry.source)?;
    let SkillSource::GitHub {
        owner,
        repo,
        subdir,
    } = &source
    else {
        // A local source is not re-fetchable during sync.
        return Ok(None);
    };
    let commit = entry
        .resolved_commit
        .clone()
        .with_context(|| format!("skill `{name}` has no recorded commit to re-fetch"))?;
    let tree = skills::fetch::fetch_at_commit(&app.ctx, owner, repo, &commit).await?;
    let root = skills::fetch::subtree(&tree, subdir.as_deref())?;
    // Pick the same skill using the same conventional-container discovery as
    // `add`; repositories commonly keep it under `skills/<name>`.
    let wanted = entry.skill.as_deref().unwrap_or(name);
    let skill_root = recorded_skill_root(&root, wanted)?;
    let package = install::read_skill_dir(&skill_root)?;
    let staged = install::stage(&app.ctx.dirs, &source.canonical(), &package, mode)?;
    if package.content_hash() != entry.content_hash {
        anyhow::bail!(
            "re-fetched `{name}` hashes to {} but the lock recorded {}; refusing to install \
             different bytes",
            package.content_hash(),
            entry.content_hash
        );
    }
    Ok(Some(staged))
}

/// Update installed skills to the latest resolution of their source.
///
/// Resolve and stage changes to a GitHub source before changing agent
/// directories. Local skills and GitHub skills that keep their current commit
/// can still update their agent list.
async fn update(app: &mut App, wanted: &[String], global: bool) -> Result<()> {
    let exclusion = |name: &str| {
        app.ctx
            .config
            .skills
            .get(name)
            .and_then(|declaration| platform_exclusion(app, declaration))
    };
    for name in wanted {
        if let Some(reason) = exclusion(name) {
            anyhow::bail!("skill `{name}` is not available on this platform ({reason})");
        }
    }

    let lock_path = lock_path_for(app, global);
    if !lock_path.is_file() {
        println!("No lockfile; nothing to update.");
        return Ok(());
    }
    let mut lock = lockfile::load(&lock_path)?;
    let candidates: Vec<_> = if wanted.is_empty() {
        lock.skills
            .keys()
            .map(|name| (name.clone(), exclusion(name)))
            .collect()
    } else {
        wanted.iter().map(|name| (name.clone(), None)).collect()
    };
    if candidates.is_empty() {
        println!("No skills in the lock to update.");
        return Ok(());
    }

    let scope_root = scope_root(app, global)?;
    let fallback_mode = link_mode(app, false);
    for (name, reason) in &candidates {
        if let Some(reason) = reason {
            report_skipped_locked(name, reason);
            continue;
        }
        let Some(locked) = lock.skills.get(name) else {
            anyhow::bail!("skill `{name}` is not recorded in the lock");
        };
        let mode = install_mode_for_entry(locked, name, &scope_root, global, fallback_mode)?;
        let mut entry = locked.clone();
        let (replacement, message) = resolve_skill_update(app, name, &mut entry, mode).await?;
        let (targets, unlinks) = prepare_skill_targets(
            app,
            name,
            &mut entry,
            &scope_root,
            global,
            mode,
            replacement,
        )
        .await?;
        if &entry != locked {
            let agents_changed = entry.agents != locked.agents;
            lock.skills.insert(name.clone(), entry);
            // Save each skill before discarding its backups. A later skill can
            // fail independently, even when one invocation updates several.
            targets.apply(|| lockfile::save_before_directory_sync(&lock_path, &lock))?;
            // A directory sync error occurs after the lock has been replaced.
            lockfile::sync_directory(&lock_path)?;
            for step in &unlinks {
                step.report(name);
            }
            if agents_changed {
                println!(
                    "`{name}`: agents now {}",
                    lock.skills[name].agents.join(", ")
                );
            }
        }
        println!("`{name}`: {message}");
    }
    Ok(())
}

async fn resolve_skill_update(
    app: &App,
    name: &str,
    entry: &mut LockedSkill,
    mode: LinkMode,
) -> Result<(Option<PathBuf>, String)> {
    let source = SkillSource::parse(&entry.source)?;
    let SkillSource::GitHub {
        owner,
        repo,
        subdir,
    } = &source
    else {
        return Ok((None, "local source, nothing to update".into()));
    };
    // New locks carry the user's request independently from the immutable
    // commit. Legacy locks fall back to project configuration for migration.
    let configured_reference = app
        .ctx
        .config
        .skills
        .get(name)
        .map(|declaration| declaration.r#ref.as_deref());
    let requested_reference = configured_reference.unwrap_or(entry.requested_ref.as_deref());
    let reference = reference_for_update(requested_reference);
    let requested_ref = normalized_requested_ref(requested_reference);
    // An abbreviated pin can reuse a resolution made for that same request.
    let pinned_commit = reference.and_then(|reference| reference.strip_prefix("rev:"));
    let commit = match &entry.resolved_commit {
        Some(commit)
            if pinned_commit.is_some_and(|pin| {
                pin.eq_ignore_ascii_case(commit)
                    || (entry.requested_ref == requested_ref
                        && !pin.is_empty()
                        && commit
                            .get(..pin.len())
                            .is_some_and(|prefix| pin.eq_ignore_ascii_case(prefix)))
            }) =>
        {
            commit.clone()
        }
        _ => skills::fetch::resolve_commit(&app.ctx, owner, repo, reference)
            .await
            .with_context(|| format!("re-resolving {owner}/{repo}"))?,
    };
    let short_commit = &commit[..commit.len().min(12)];
    if Some(&commit) == entry.resolved_commit.as_ref() {
        let mut message = format!("already at {short_commit}");
        if entry.requested_ref != requested_ref || entry.install_mode.is_none() {
            entry.requested_ref = requested_ref;
            entry.install_mode = Some(mode);
            message.push_str(", migrated lock metadata");
        }
        return Ok((None, message));
    }

    let tree = skills::fetch::fetch_at_commit(&app.ctx, owner, repo, &commit).await?;
    let root = skills::fetch::subtree(&tree, subdir.as_deref())?;
    let wanted_dir = entry.skill.as_deref().unwrap_or(name);
    let skill_root = recorded_skill_root(&root, wanted_dir)?;
    let package = install::read_skill_dir(&skill_root)?;
    let content_hash = package.content_hash();
    let (replacement, message) = if content_hash != entry.content_hash {
        let staged = install::stage(&app.ctx.dirs, &source.canonical(), &package, mode)?;
        (Some(staged), format!("updated to {short_commit}"))
    } else {
        (
            None,
            format!("source advanced to {short_commit}, skill content unchanged"),
        )
    };
    entry.requested_ref = requested_ref;
    entry.resolved_commit = Some(commit);
    entry.content_hash = content_hash;
    entry.install_mode = Some(mode);
    Ok((replacement, message))
}

/// Interpret a declaration ref for an explicit `skills update`.
///
/// Existing projects can carry the commit selected at install time as a bare
/// 40-hex declaration value. Treating that snapshot as the next update target
/// makes `update` resolve the same commit forever. Preserve intentional immutable
/// declarations through the unambiguous `rev:` spelling;
/// every other selector is a moving update target, while a bare 40-hex snapshot
/// follows the source's default branch for backward compatibility.
fn reference_for_update(reference: Option<&str>) -> Option<&str> {
    match reference.map(str::trim).filter(|value| !value.is_empty()) {
        Some("default") => None,
        Some(reference)
            if reference.len() == 40 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            None
        }
        reference => reference,
    }
}

fn normalized_requested_ref(reference: Option<&str>) -> Option<String> {
    Some(
        reference
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| {
                !(value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            })
            .unwrap_or("default")
            .to_string(),
    )
}

/// Use a skill without installing it: print its prompt, or start an agent.
async fn use_skill(
    app: &mut App,
    source_str: &str,
    skill: Option<&str>,
    agent: Option<&str>,
    reference: Option<&str>,
) -> Result<()> {
    let source = SkillSource::parse(source_str)?;
    let wanted: Vec<String> = skill.map(|s| vec![s.to_string()]).unwrap_or_default();

    // Resolve to one skill directory (local or GitHub), without staging or lock.
    let root = match &source {
        SkillSource::Local { path } => path.clone(),
        SkillSource::GitHub {
            owner,
            repo,
            subdir,
        } => {
            let commit = skills::fetch::resolve_commit(&app.ctx, owner, repo, reference).await?;
            let tree = skills::fetch::fetch_at_commit(&app.ctx, owner, repo, &commit).await?;
            skills::fetch::subtree(&tree, subdir.as_deref())?
        }
    };
    let roots = local_roots(&root, &wanted)?;
    if roots.len() != 1 {
        anyhow::bail!(
            "`use` needs exactly one skill; {} matched -- pass -s <name> to pick one",
            roots.len()
        );
    }
    let package = install::read_skill_dir(&roots[0].1)?;
    let manifest = package
        .files
        .iter()
        .find(|f| f.path == install::SKILL_MANIFEST)
        .map(|f| String::from_utf8_lossy(&f.bytes).into_owned())
        .unwrap_or_default();
    let prompt = format!(
        "Use the following skill for this session.\n\n# Skill: {}\n\n{}\n",
        package.name, manifest
    );

    match agent {
        None => {
            // Pipe target: only the prompt goes to stdout.
            print!("{prompt}");
            Ok(())
        }
        Some(agent_id) => {
            let target =
                agent_target(agent_id).with_context(|| format!("unknown agent `{agent_id}`"))?;
            let launch = target.launch.with_context(|| {
                format!("agent `{agent_id}` has no launchable CLI osdk can start")
            })?;
            // Start the agent interactively with the prompt as its argument,
            // inheriting stdio so the session is the user's.
            let status = std::process::Command::new(launch)
                .arg(&prompt)
                .status()
                .with_context(|| format!("starting `{launch}` (is it on PATH?)"))?;
            if !status.success() {
                anyhow::bail!("`{launch}` exited with {status}");
            }
            Ok(())
        }
    }
}

/// Create a SKILL.md template to start authoring a skill.
fn init(name: Option<&str>) -> Result<()> {
    let dir = match name {
        Some(n) => {
            osdk_core::skills::validate_skill_name(n)?;
            std::env::current_dir()?.join(n)
        }
        None => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let manifest = dir.join(install::SKILL_MANIFEST);
    if manifest.exists() {
        anyhow::bail!(
            "{} already exists; refusing to overwrite",
            manifest.display()
        );
    }
    let leaf = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "my-skill".to_string());
    let template = format!(
        "---\nname: {leaf}\ndescription: One sentence on what this skill does and when to use it.\n---\n\n# {leaf}\n\nWrite the skill's instructions here.\n"
    );
    std::fs::write(&manifest, template)
        .with_context(|| format!("writing {}", manifest.display()))?;
    println!("Created {}", manifest.display());
    Ok(())
}

/// Search GitHub for installable skills and print `owner/repo` hits.
///
/// Discovery only: it never writes an agent directory or the lock. The network
/// call reuses osdk's GitHub transport (anonymous first, `GITHUB_TOKEN` only as
/// a rate-limit fallback), so a hit list is one `osdk skills add <owner/repo>`
/// away from installing.
async fn find(app: &mut App, query: &str, owner: Option<&str>, limit: u8) -> Result<()> {
    let hits = skills::search::search(&app.ctx, query, owner, limit)
        .await
        .context("searching GitHub for skills")?;
    if hits.is_empty() {
        println!(
            "No skills found. Try broader keywords, or `--owner <OWNER>` to browse one owner."
        );
        return Ok(());
    }
    println!(
        "Found {} skill {} (install with `osdk skills add <owner/repo>`):\n",
        hits.len(),
        if hits.len() == 1 { "repo" } else { "repos" }
    );
    for hit in &hits {
        // Stars give a quick sense of adoption; the description (when present)
        // says what it is. Keep the line scannable: name, stars, then blurb.
        let blurb = if hit.description.is_empty() {
            String::new()
        } else {
            format!("  {}", truncate_blurb(&hit.description, 100))
        };
        println!("  {}  (\u{2605}{}){}", hit.full_name, hit.stars, blurb);
    }
    Ok(())
}

/// Trim a one-line blurb to `max` chars on a char boundary, adding an ellipsis.
fn truncate_blurb(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

// --- helpers ---------------------------------------------------------------
/// Conventional directories that hold a collection of skills inside a repo.
///
/// The wider ecosystem publishes skills under `skills/` (and agents mount them
/// from `.agents/skills` / `.claude/skills`), so a repo root that is not itself a
/// skill is searched in these containers too, not only its immediate children.
const SKILL_CONTAINERS: &[&str] = &["", "skills", ".agents/skills", ".claude/skills"];

/// Enumerate skill roots from a local (or extracted) source, honoring `--skill`.
///
/// Three shapes are handled: a directory that is itself a skill (one root, named
/// by its frontmatter); a directory whose immediate children are skills; and a
/// repo that keeps skills under a conventional container such as `skills/`.
/// `--skill` narrows the multi-skill cases by directory name (`*` selects all).
fn local_roots(path: &Path, wanted: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if !path.is_dir() {
        anyhow::bail!("local skill source `{}` is not a directory", path.display());
    }
    if path.join(install::SKILL_MANIFEST).is_file() {
        // Single skill: its name comes from the frontmatter, read by the caller.
        let package = install::read_skill_dir(path)?;
        return Ok(vec![(package.name, path.to_path_buf())]);
    }

    // A collection of skills: scan each conventional container for immediate
    // subdirectories that hold a SKILL.md. Keyed by name so the same skill found
    // via two containers is not staged twice.
    let mut roots: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for container in SKILL_CONTAINERS {
        let dir = if container.is_empty() {
            path.to_path_buf()
        } else {
            let mut dir = path.to_path_buf();
            for segment in container.split('/') {
                dir.push(segment);
            }
            dir
        };
        if !dir.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let child = entry.path();
            if child.join(install::SKILL_MANIFEST).is_file() {
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if wanted.is_empty() || wanted.iter().any(|w| w == "*" || w == &dir_name) {
                    roots.entry(dir_name).or_insert(child);
                }
            }
        }
    }
    if roots.is_empty() {
        anyhow::bail!(
            "no {} found in {} (looked in the root, its subdirectories, and skills/)",
            install::SKILL_MANIFEST,
            path.display()
        );
    }
    Ok(roots.into_iter().collect())
}

fn recorded_skill_root(root: &Path, wanted: &str) -> Result<PathBuf> {
    let matches = local_roots(root, &[wanted.to_string()])?;
    if matches.len() != 1 {
        anyhow::bail!(
            "expected exactly one skill `{wanted}` in {}, found {}",
            root.display(),
            matches.len()
        );
    }
    Ok(matches.into_iter().next().expect("one match").1)
}

/// Which agents an install targets: explicit flags, then configured defaults,
/// then detected agents or an interactive multi-select.
fn resolve_agents(
    app: &App,
    explicit: &[String],
    scope_root: &Path,
    global: bool,
) -> Result<Vec<&'static AgentTarget>> {
    let home = home_dir().ok();
    let detected = detect_installed_agent_ids(scope_root, home.as_deref());
    let ids = resolve_agent_ids(
        explicit,
        &app.ctx.config.skills_defaults.default_agents,
        &detected,
        app.prompt.as_ref(),
        global,
    )?;
    let mut targets = Vec::new();
    for id in &ids {
        let target = agent_target(id).with_context(|| {
            format!("unknown agent `{id}` (run `osdk skills agents` for the list)")
        })?;
        if !targets.iter().any(|t: &&AgentTarget| t.id == target.id) {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn resolve_agent_ids(
    explicit: &[String],
    defaults: &[String],
    detected: &[String],
    prompt: &dyn crate::prompt::Prompt,
    global: bool,
) -> Result<Vec<String>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    if !defaults.is_empty() {
        return Ok(defaults.to_vec());
    }

    if detected.len() == 1 {
        return Ok(with_universal_agents(detected.to_vec()));
    }
    if prompt.assume_yes() {
        let selected = if detected.is_empty() {
            AGENT_TARGETS
                .iter()
                .map(|target| target.id.to_string())
                .collect()
        } else {
            detected.to_vec()
        };
        return Ok(with_universal_agents(selected));
    }
    if !prompt.is_interactive() {
        anyhow::bail!(
            "no target agent in this non-interactive session: set [skills].default_agents \
             in osdk.toml or pass --yes to accept the recommended targets"
        );
    }

    // One choice per directory, not per agent: in a project the shared
    // `.agents/skills` serves five agents at once, and listing them separately
    // offered five options that all wrote the same place. The picker installs
    // exactly what is checked -- it used to add every shared-directory agent
    // on top, so checking one agent installed for all of them.
    let choices = agent_choices(global);
    let options = choices.iter().map(AgentChoice::label).collect::<Vec<_>>();
    let checked = choices
        .iter()
        .map(|choice| {
            choice
                .members
                .iter()
                .any(|member| detected.iter().any(|id| id == member.id))
        })
        .collect::<Vec<_>>();
    let selected = prompt.select_many(
        "Select the agents that should receive this skill",
        &options,
        &checked,
    )?;
    let mut ids = Vec::new();
    for index in selected {
        let choice = choices
            .get(index)
            .with_context(|| format!("prompt returned invalid agent index {index}"))?;
        if !ids.iter().any(|id| id == choice.id()) {
            ids.push(choice.id().to_string());
        }
    }
    if ids.is_empty() {
        anyhow::bail!("select at least one target agent");
    }
    Ok(ids)
}

/// Agents that install into one directory, offered as a single choice.
struct AgentChoice {
    dir: &'static str,
    members: Vec<&'static AgentTarget>,
}

impl AgentChoice {
    /// The id recorded for this choice: `universal` for the shared directory,
    /// otherwise the agent's own.
    fn id(&self) -> &'static str {
        self.members
            .iter()
            .find(|member| member.id == "universal")
            .unwrap_or(&self.members[0])
            .id
    }

    fn label(&self) -> String {
        match self.members.as_slice() {
            [only] => format!("{} ({})  {}", only.name, only.id, self.dir),
            members => {
                let served = members
                    .iter()
                    .filter(|member| member.id != "universal")
                    .map(|member| member.name)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Universal (universal)  {}, used by {served}", self.dir)
            }
        }
    }
}

/// The directories this scope can install into, each with the agents it serves.
/// Globally every agent has its own home directory, so nothing is grouped.
fn agent_choices(global: bool) -> Vec<AgentChoice> {
    let mut choices: Vec<AgentChoice> = Vec::new();
    for target in AGENT_TARGETS {
        let dir = if global {
            match target.global {
                Some(dir) => dir,
                None => continue,
            }
        } else {
            target.project
        };
        match choices.iter_mut().find(|choice| choice.dir == dir) {
            Some(choice) => choice.members.push(target),
            None => choices.push(AgentChoice {
                dir,
                members: vec![target],
            }),
        }
    }
    choices
}

/// Add the shared `.agents/skills` directory, as the single `universal` id.
///
/// It used to add every agent that reads that directory. In a project those
/// all resolve to the one directory, but globally each has its own home
/// directory, so an unattended default installed into all of them.
fn with_universal_agents(mut ids: Vec<String>) -> Vec<String> {
    if !ids.iter().any(|id| id == "universal") {
        ids.push("universal".to_string());
    }
    ids
}

/// Detect agents from their dedicated project or user configuration roots.
/// A shared `.agents/skills` directory is not an installation signal: it cannot
/// distinguish Codex, Cursor, OpenCode, Gemini, Copilot, or Universal.
fn detect_installed_agent_ids(scope_root: &Path, home: Option<&Path>) -> Vec<String> {
    let mut detected = Vec::new();
    for target in AGENT_TARGETS {
        if target.id == "universal" {
            continue;
        }
        let project_root = target.project_dir(scope_root);
        let project_root = project_root.parent().unwrap_or(&project_root);
        let project_signal = target.project != ".agents/skills" && project_root.is_dir();
        let user_signal = home
            .and_then(|home| target.global_dir(home))
            .and_then(|skills| skills.parent().map(Path::to_path_buf))
            .is_some_and(|root| root.is_dir());
        if project_signal || user_signal {
            detected.push(target.id.to_string());
        }
    }
    detected
}

fn agent_dirs_by_path(
    targets: &[&'static AgentTarget],
    scope_root: &Path,
    global: bool,
) -> Result<AgentDirs> {
    let mut dirs = BTreeMap::new();
    for target in targets {
        dirs.entry(agent_dir(target, scope_root, global)?)
            .or_insert_with(Vec::new)
            .push(*target);
    }
    Ok(dirs)
}

fn install_mode_for_entry(
    entry: &LockedSkill,
    name: &str,
    scope_root: &Path,
    global: bool,
    fallback: LinkMode,
) -> Result<LinkMode> {
    if let Some(mode) = entry.install_mode {
        return Ok(mode);
    }
    let targets = entry
        .agents
        .iter()
        .filter_map(|agent_id| agent_target(agent_id))
        .collect::<Vec<_>>();
    let mut saw_link = false;
    for agent_dir in agent_dirs_by_path(&targets, scope_root, global)?.keys() {
        let destination = agent_dir.join(name);
        if let Ok(metadata) = destination.symlink_metadata() {
            if osdk_core::store::dirlink::is_link(&metadata) {
                saw_link = true;
            } else {
                return Ok(LinkMode::Copy);
            }
        }
    }
    Ok(if saw_link { LinkMode::Auto } else { fallback })
}

/// Pick link vs copy for `skills add`. Explicit flags and skill configuration
/// win. A single physical target is copied directly; multiple targets prompt in
/// a terminal and otherwise retain osdk's configured fallback.
fn add_link_mode(app: &App, copy: bool, unique_dirs: usize) -> Result<LinkMode> {
    resolve_add_link_mode(
        copy,
        app.ctx.config.skills_defaults.link_mode.as_deref(),
        app.ctx.config.settings.link_mode,
        unique_dirs,
        app.prompt.as_ref(),
    )
}

fn resolve_add_link_mode(
    copy: bool,
    configured: Option<&str>,
    fallback: LinkMode,
    unique_dirs: usize,
    prompt: &dyn crate::prompt::Prompt,
) -> Result<LinkMode> {
    if copy {
        return Ok(LinkMode::Copy);
    }
    if let Some(configured) = configured {
        return configured.parse::<LinkMode>().map_err(Into::into);
    }
    if unique_dirs <= 1 {
        return Ok(LinkMode::Copy);
    }
    if prompt.assume_yes() {
        return Ok(LinkMode::Auto);
    }
    if prompt.is_interactive() {
        let options = vec![
            "Link (recommended: one canonical copy for all agents)".to_string(),
            "Copy (independent files in every agent directory)".to_string(),
        ];
        return Ok(
            match prompt.select_one("How should osdk install the skill?", &options, 0)? {
                0 => LinkMode::Auto,
                1 => LinkMode::Copy,
                other => anyhow::bail!("prompt returned invalid install method index {other}"),
            },
        );
    }
    Ok(fallback)
}

/// The link mode for non-add operations: the `[skills].link_mode` override, else
/// the global link mode.
fn link_mode(app: &App, copy: bool) -> LinkMode {
    if copy {
        return LinkMode::Copy;
    }
    if let Some(configured) = app.ctx.config.skills_defaults.link_mode.as_deref() {
        if let Ok(mode) = configured.parse::<LinkMode>() {
            return mode;
        }
    }
    app.ctx.config.settings.link_mode
}

/// The root a project-scoped install links relative to (the project dir), or the
/// home directory for a global install.
fn scope_root(app: &App, global: bool) -> Result<PathBuf> {
    if global {
        home_dir()
    } else if let Some(project) = &app.ctx.config.project {
        // Project root: the real project tree an external binding records, so
        // agent-visible output lands in the repo -- never in the bundle dir.
        Ok(project.root.clone())
    } else {
        // Project root: the directory holding the nearest config, else cwd.
        Ok(app
            .ctx
            .config
            .project_config_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or(std::env::current_dir().context("getting current dir")?))
    }
}

fn agent_dir(target: &AgentTarget, scope_root: &Path, global: bool) -> Result<PathBuf> {
    if global {
        target
            .global_dir(scope_root)
            .with_context(|| format!("agent `{}` has no global skills directory", target.id))
    } else {
        Ok(target.project_dir(scope_root))
    }
}

/// The lockfile path for the chosen scope. With a selected project, use the
/// lock that belongs to its config, even if only an ancestor has a lock.
fn lock_path_for(app: &App, global: bool) -> PathBuf {
    if global {
        app.ctx.dirs.user_lock_file()
    } else if let Some(project) = &app.ctx.config.project {
        project.lock_path.clone()
    } else {
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        lockfile::default_path(&start)
    }
}

fn write_lock_entries(lock_path: &Path, entries: Vec<(String, LockedSkill)>) -> Result<()> {
    let mut lock = if lock_path.is_file() {
        lockfile::load(lock_path)?
    } else {
        lockfile::Lockfile::default()
    };
    for (name, entry) in entries {
        lock.skills.insert(name, entry);
    }
    lockfile::save(lock_path, &lock)?;
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    // Avoid a new dependency: the home directory is `USERPROFILE` on Windows and
    // `HOME` elsewhere. This only feeds a global-scope install path, and the
    // agent-dir join fails loudly later if it is wrong.
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .with_context(|| format!("cannot determine home directory (${key} is unset)"))
}

/// One removed agent's action: unlink its directory, or only drop ownership.
#[derive(Debug, PartialEq, Eq)]
struct UnlinkStep {
    agent: String,
    physically_unlink: bool,
}

impl UnlinkStep {
    fn dir(&self, scope_root: &Path, global: bool) -> Result<PathBuf> {
        agent_dir(
            agent_target(&self.agent).expect("planned agent exists"),
            scope_root,
            global,
        )
    }

    fn report(&self, name: &str) {
        if self.physically_unlink {
            println!("removed `{name}` from {}", self.agent);
        } else {
            println!(
                "unlinked `{name}` from {} (shared dir kept for other agents)",
                self.agent
            );
        }
    }
}

/// Decide, for each agent being removed, whether to physically unlink its skills
/// directory or only drop the lock ownership.
///
/// The hazard this exists for: codex / cursor / opencode all read `.agents/skills`,
/// so removing one must not delete a directory a *remaining* agent still uses, and
/// two removed agents that share a directory must unlink it once. An agent
/// physically unlinks only when no remaining agent maps to its directory and no
/// earlier removed agent already unlinked that same directory.
fn plan_unlinks(
    remove_from: &[String],
    remaining: &[String],
    dir_of: &std::collections::BTreeMap<String, PathBuf>,
) -> Vec<UnlinkStep> {
    let remaining_dirs: std::collections::BTreeSet<&PathBuf> =
        remaining.iter().filter_map(|a| dir_of.get(a)).collect();
    let mut already: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut plan = Vec::new();
    for agent in remove_from {
        let Some(dir) = dir_of.get(agent) else {
            continue;
        };
        let shared_with_remaining = remaining_dirs.contains(dir);
        let already_unlinked = already.contains(dir);
        let physically_unlink = !shared_with_remaining && !already_unlinked;
        if physically_unlink {
            already.insert(dir.clone());
        }
        plan.push(UnlinkStep {
            agent: agent.clone(),
            physically_unlink,
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, PathBuf> {
        pairs
            .iter()
            .map(|(a, d)| (a.to_string(), PathBuf::from(d)))
            .collect()
    }

    /// N=2 shared directory: removing one of two agents that share `.agents/skills`
    /// must NOT physically unlink, or the remaining agent loses files it owns.
    /// This is the regression for the real bug found by probing with two agents.
    #[test]
    fn removing_one_of_two_agents_sharing_a_dir_keeps_the_files() {
        // codex and cursor both map to the same `.agents/skills`.
        let dir_of = dirs(&[
            ("codex", "/p/.agents/skills"),
            ("cursor", "/p/.agents/skills"),
        ]);
        let plan = plan_unlinks(&["codex".into()], &["cursor".into()], &dir_of);
        assert_eq!(
            plan,
            vec![UnlinkStep {
                agent: "codex".into(),
                physically_unlink: false
            }],
            "codex must not delete the dir cursor still uses"
        );
    }

    /// Removing the last owner of a shared dir does physically unlink it.
    #[test]
    fn removing_the_last_owner_unlinks_the_shared_dir() {
        let dir_of = dirs(&[("cursor", "/p/.agents/skills")]);
        let plan = plan_unlinks(&["cursor".into()], &[], &dir_of);
        assert!(plan[0].physically_unlink);
    }

    /// Two removed agents sharing one dir unlink it exactly once.
    #[test]
    fn two_removed_agents_sharing_a_dir_unlink_once() {
        let dir_of = dirs(&[
            ("codex", "/p/.agents/skills"),
            ("cursor", "/p/.agents/skills"),
        ]);
        let plan = plan_unlinks(&["codex".into(), "cursor".into()], &[], &dir_of);
        let physical: Vec<bool> = plan.iter().map(|s| s.physically_unlink).collect();
        assert_eq!(
            physical,
            vec![true, false],
            "the shared dir is unlinked once"
        );
    }

    /// An agent with its own private directory always unlinks.
    #[test]
    fn a_private_dir_always_unlinks() {
        let dir_of = dirs(&[
            ("claude-code", "/p/.claude/skills"),
            ("codex", "/p/.agents/skills"),
        ]);
        let plan = plan_unlinks(&["claude-code".into()], &["codex".into()], &dir_of);
        assert!(plan[0].physically_unlink);
    }

    #[derive(Default)]
    struct SelectionPrompt {
        interactive: bool,
        assume_yes: bool,
        many: Vec<usize>,
        one: usize,
        /// What the last `select_many` offered: labels and preselection.
        offered: std::sync::Mutex<Option<(Vec<String>, Vec<bool>)>>,
    }

    impl crate::prompt::Prompt for SelectionPrompt {
        fn confirm(&self, _question: &str) -> Result<bool> {
            Ok(true)
        }

        fn is_interactive(&self) -> bool {
            self.interactive
        }

        fn assume_yes(&self) -> bool {
            self.assume_yes
        }

        fn select_many(
            &self,
            _question: &str,
            options: &[String],
            defaults: &[bool],
        ) -> Result<Vec<usize>> {
            *self.offered.lock().unwrap() = Some((options.to_vec(), defaults.to_vec()));
            Ok(self.many.clone())
        }

        fn select_one(
            &self,
            _question: &str,
            _options: &[String],
            _default: usize,
        ) -> Result<usize> {
            Ok(self.one)
        }
    }

    fn selection_prompt(interactive: bool) -> SelectionPrompt {
        SelectionPrompt {
            interactive,
            ..SelectionPrompt::default()
        }
    }

    #[test]
    fn agent_selection_prefers_explicit_then_defaults_then_prompt() {
        let prompt = SelectionPrompt {
            interactive: true,
            many: vec![1, 1, 0],
            ..SelectionPrompt::default()
        };
        assert_eq!(
            resolve_agent_ids(
                &["cursor".into()],
                &["claude-code".into()],
                &["codex".into()],
                &prompt,
                false,
            )
            .unwrap(),
            vec!["cursor"]
        );
        assert_eq!(
            resolve_agent_ids(
                &[],
                &["claude-code".into()],
                &["codex".into()],
                &prompt,
                false
            )
            .unwrap(),
            vec!["claude-code"]
        );
        assert_eq!(
            resolve_agent_ids(&[], &[], &[], &prompt, false).unwrap(),
            vec!["universal", "claude-code"]
        );
    }

    #[test]
    fn one_detected_agent_auto_selects_the_universal_group() {
        let ids = resolve_agent_ids(
            &[],
            &[],
            &["claude-code".into()],
            &selection_prompt(false),
            false,
        )
        .unwrap();
        assert_eq!(ids, vec!["claude-code", "universal"]);
    }

    /// Regression: checking one agent installed for every agent. The picker
    /// offers one choice per directory, preselects what was detected, and
    /// returns exactly what was checked -- in both scopes.
    #[test]
    fn interactive_selection_installs_exactly_the_checked_agents() {
        let prompt = SelectionPrompt {
            interactive: true,
            many: vec![0],
            ..SelectionPrompt::default()
        };
        let ids = resolve_agent_ids(
            &[],
            &[],
            &["claude-code".into(), "codex".into()],
            &prompt,
            false,
        )
        .unwrap();
        assert_eq!(ids, vec!["claude-code"]);
        let (options, checked) = prompt.offered.lock().unwrap().clone().unwrap();
        assert_eq!(
            options.len(),
            2,
            "one choice per project directory: {options:?}"
        );
        assert!(
            options[0].starts_with("Claude Code (claude-code)"),
            "{options:?}"
        );
        assert!(options[1].contains(".agents/skills"), "{options:?}");
        assert!(options[1].contains("Codex"), "{options:?}");
        assert_eq!(checked, vec![true, true], "detected agents start checked");

        // Globally every agent has its own directory, so each is offered and
        // picking one still installs only that one.
        let prompt = SelectionPrompt {
            interactive: true,
            many: vec![1],
            ..SelectionPrompt::default()
        };
        let ids = resolve_agent_ids(&[], &[], &[], &prompt, true).unwrap();
        assert_eq!(ids, vec!["codex"]);
        let (options, checked) = prompt.offered.lock().unwrap().clone().unwrap();
        assert_eq!(options.len(), AGENT_TARGETS.len(), "{options:?}");
        assert!(checked.iter().all(|checked| !checked));
    }

    #[test]
    fn yes_selects_detected_agents_or_all_agents() {
        let yes = SelectionPrompt {
            assume_yes: true,
            ..SelectionPrompt::default()
        };
        let detected = resolve_agent_ids(
            &[],
            &[],
            &["claude-code".into(), "codex".into()],
            &yes,
            false,
        )
        .unwrap();
        assert!(detected.contains(&"claude-code".to_string()));
        assert!(detected.contains(&"universal".to_string()));
        assert_eq!(
            resolve_agent_ids(&[], &[], &[], &yes, false).unwrap().len(),
            AGENT_TARGETS.len()
        );
    }

    #[test]
    fn missing_agent_fails_only_when_the_session_is_non_interactive() {
        let error = resolve_agent_ids(&[], &[], &[], &selection_prompt(false), false).unwrap_err();
        assert!(error.to_string().contains("non-interactive"));
    }

    #[test]
    fn interactive_agent_selection_rejects_invalid_prompt_indexes() {
        let prompt = SelectionPrompt {
            interactive: true,
            many: vec![AGENT_TARGETS.len()],
            ..SelectionPrompt::default()
        };
        assert!(resolve_agent_ids(&[], &[], &[], &prompt, false).is_err());
    }

    #[test]
    fn recorded_skill_root_uses_conventional_containers() {
        let repository = tempfile::tempdir().unwrap();
        let skill = repository.path().join("skills/osdk-guide");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: osdk-guide\ndescription: guide\n---\n",
        )
        .unwrap();

        assert_eq!(
            recorded_skill_root(repository.path(), "osdk-guide").unwrap(),
            skill
        );
    }

    #[test]
    fn agent_detection_uses_specific_roots_not_shared_agents_dir() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".agents/skills")).unwrap();
        std::fs::create_dir_all(project.path().join(".claude")).unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::create_dir_all(home.path().join(".config/agents")).unwrap();

        assert_eq!(
            detect_installed_agent_ids(project.path(), Some(home.path())),
            vec!["claude-code", "codex"]
        );
    }

    #[test]
    fn shared_agent_directories_are_grouped_once() {
        let root = tempfile::tempdir().unwrap();
        let targets = vec![
            agent_target("codex").unwrap(),
            agent_target("cursor").unwrap(),
            agent_target("claude-code").unwrap(),
        ];
        let dirs = agent_dirs_by_path(&targets, root.path(), false).unwrap();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.values().any(|owners| owners
            .iter()
            .map(|target| target.id)
            .collect::<Vec<_>>()
            == ["codex", "cursor"]));
    }

    #[test]
    fn legacy_lock_infers_copy_from_an_existing_real_directory() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join(".agents/skills/mine");
        std::fs::create_dir_all(&destination).unwrap();
        let entry = LockedSkill {
            source: "github:owner/repo".into(),
            content_hash: "b3-v2:test".into(),
            resolved_commit: Some("deadbeef".into()),
            agents: vec!["universal".into(), "codex".into()],
            ..LockedSkill::default()
        };
        assert_eq!(
            install_mode_for_entry(&entry, "mine", root.path(), false, LinkMode::Auto).unwrap(),
            LinkMode::Copy
        );
    }

    #[test]
    fn add_link_mode_obeys_flags_config_unique_dirs_and_choice() {
        let link_prompt = SelectionPrompt {
            interactive: true,
            ..SelectionPrompt::default()
        };
        let copy_prompt = SelectionPrompt {
            interactive: true,
            one: 1,
            ..SelectionPrompt::default()
        };
        let noninteractive = selection_prompt(false);
        let yes = SelectionPrompt {
            assume_yes: true,
            one: 1,
            ..SelectionPrompt::default()
        };

        assert_eq!(
            resolve_add_link_mode(true, Some("symlink"), LinkMode::Hardlink, 2, &copy_prompt)
                .unwrap(),
            LinkMode::Copy
        );
        assert_eq!(
            resolve_add_link_mode(false, Some("hardlink"), LinkMode::Auto, 1, &copy_prompt)
                .unwrap(),
            LinkMode::Hardlink
        );
        assert_eq!(
            resolve_add_link_mode(false, None, LinkMode::Hardlink, 1, &link_prompt).unwrap(),
            LinkMode::Copy
        );
        assert_eq!(
            resolve_add_link_mode(false, None, LinkMode::Hardlink, 2, &noninteractive).unwrap(),
            LinkMode::Hardlink
        );
        assert_eq!(
            resolve_add_link_mode(false, None, LinkMode::Hardlink, 2, &yes).unwrap(),
            LinkMode::Auto
        );
        assert_eq!(
            resolve_add_link_mode(false, None, LinkMode::Hardlink, 2, &link_prompt).unwrap(),
            LinkMode::Auto
        );
        assert_eq!(
            resolve_add_link_mode(false, None, LinkMode::Auto, 2, &copy_prompt).unwrap(),
            LinkMode::Copy
        );
    }

    /// A bare commit recorded in a declaration is an installation snapshot, not
    /// a permanent pin: an explicit update must follow the source's default
    /// branch. Users who need an immutable declaration spell it `rev:<value>`.
    #[test]
    fn update_reference_advances_bare_commit_snapshots() {
        let snapshot = "02a2f2b3142ded7d73554aaee629a24966a62313";
        assert_eq!(reference_for_update(Some(snapshot)), None);
        assert_eq!(reference_for_update(None), None);
    }

    #[test]
    fn update_reference_preserves_explicit_pins_and_floating_refs() {
        assert_eq!(
            reference_for_update(Some("rev:02a2f2b3142ded7d73554aaee629a24966a62313")),
            Some("rev:02a2f2b3142ded7d73554aaee629a24966a62313")
        );
        assert_eq!(
            reference_for_update(Some("branch:main")),
            Some("branch:main")
        );
        assert_eq!(reference_for_update(Some("tag:v1")), Some("tag:v1"));
        assert_eq!(reference_for_update(Some("release")), Some("release"));
    }

    #[test]
    fn requested_refs_store_update_intent_not_legacy_snapshots() {
        let snapshot = "02a2f2b3142ded7d73554aaee629a24966a62313";
        assert_eq!(normalized_requested_ref(None).as_deref(), Some("default"));
        assert_eq!(
            normalized_requested_ref(Some("branch:main")).as_deref(),
            Some("branch:main")
        );
        assert_eq!(
            normalized_requested_ref(Some("rev:02a2f2b3142ded7d73554aaee629a24966a62313"))
                .as_deref(),
            Some("rev:02a2f2b3142ded7d73554aaee629a24966a62313")
        );
        assert_eq!(
            normalized_requested_ref(Some(snapshot)).as_deref(),
            Some("default")
        );
    }
}
