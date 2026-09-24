//! `osdk skills` command handlers.
//!
//! P0-4a covers the offline surface: `agents`, `list`, `add` from a local path,
//! `remove`, `path`, and `sync` from `osdk.lock`. GitHub sources and the network
//! path land in P0-4b; `add` of a GitHub source reports that clearly rather than
//! pretending to work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osdk_core::skills;
use osdk_core::skills::{agent_target, install, AgentTarget, SkillSource, AGENT_TARGETS};
use osdk_core::store::link::LinkMode;

use crate::app::App;
use crate::cli::SkillsCommand;
use crate::lockfile::{self, LockedSkill};

pub async fn run(app: &mut App, command: SkillsCommand) -> Result<()> {
    match command {
        SkillsCommand::Agents => agents(),
        SkillsCommand::List { global } => list(app, global),
        SkillsCommand::Path { name } => path(app, &name),
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
                    global,
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
        } => remove(app, &name, global, &agents),
        SkillsCommand::Sync { global } => sync(app, global).await,
        SkillsCommand::Update { skills, global } => update(app, &skills, global).await,
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

/// List installed skills recorded in the lock and where they are linked.
fn list(app: &App, global: bool) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    let skills = if lock_path.is_file() {
        lockfile::load(&lock_path)?.skills
    } else {
        Default::default()
    };
    if skills.is_empty() {
        println!("No skills installed.");
        return Ok(());
    }
    for (name, entry) in &skills {
        let agents = if entry.agents.is_empty() {
            "-".to_string()
        } else {
            entry.agents.join(", ")
        };
        println!("{name}  <- {}  [{}]", entry.source, agents);
    }
    Ok(())
}

/// Print the staged store path of an installed skill.
fn path(app: &App, name: &str) -> Result<()> {
    osdk_core::skills::validate_skill_name(name)?;
    // Prefer the recorded content hash so the path is the exact staged copy.
    for global in [false, true] {
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
    let source = SkillSource::parse(&args.source)?;

    // Resolve the source to one or more on-disk skill roots plus, for GitHub, the
    // immutable commit the ref pinned to. Both source kinds converge on
    // `local_roots`, which handles "one skill dir" and "a directory of skills".
    let (roots, resolved_commit): (Vec<(String, PathBuf)>, Option<String>) = match &source {
        SkillSource::Local { path } => {
            if args.reference.is_some() {
                anyhow::bail!("--ref applies only to a GitHub source, not a local path");
            }
            (local_roots(path, &args.skills)?, None)
        }
        SkillSource::GitHub {
            owner,
            repo,
            subdir,
        } => {
            let commit =
                skills::fetch::resolve_commit(&app.ctx, owner, repo, args.reference.as_deref())
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
            (local_roots(&root, &args.skills)?, Some(commit))
        }
    };

    if args.list_only {
        println!("Skills offered by {}:", source.canonical());
        for (name, root) in &roots {
            let package = install::read_skill_dir(root)?;
            println!("  {name}  — {}", package.description);
        }
        return Ok(());
    }

    let targets = resolve_agents(app, &args.agents)?;
    let mode = link_mode(app, args.copy);
    let scope_root = scope_root(app, args.global)?;

    let mut lock_entries: Vec<(String, LockedSkill)> = Vec::new();
    for (name, root) in &roots {
        let package = install::read_skill_dir(root)?;
        // Pre-install preview: show what will be written into an agent directory
        // before doing it. This is the one risk surface skills add over tools.
        println!("Installing skill: {}", package.preview());

        let staged = install::stage(&app.ctx.dirs, &source.canonical(), &package, mode)
            .with_context(|| format!("staging skill `{name}`"))?;

        let mut linked_agents = Vec::new();
        for target in &targets {
            let agent_dir = agent_dir(target, &scope_root, args.global)?;
            install::link_into(&agent_dir, &package.name, &staged, mode)
                .with_context(|| format!("linking `{}` into {}", package.name, target.id))?;
            linked_agents.push(target.id.to_string());
            println!("  linked into {} ({})", target.id, agent_dir.display());
        }

        lock_entries.push((
            package.name.clone(),
            LockedSkill {
                source: source.canonical(),
                content_hash: package.content_hash(),
                resolved_commit: resolved_commit.clone(),
                skill: (name != &package.name).then(|| name.clone()),
                agents: linked_agents,
            },
        ));
    }

    if !args.no_lock {
        write_lock_entries(app, args.global, lock_entries)?;
    }
    Ok(())
}

/// Remove a skill from its agents and drop it from the lock.
fn remove(app: &mut App, name: &str, global: bool, only_agents: &[String]) -> Result<()> {
    osdk_core::skills::validate_skill_name(name)?;
    let lock_path = lock_path_for(app, global);
    if !lock_path.is_file() {
        anyhow::bail!("no lockfile; nothing to remove");
    }
    let mut lock = lockfile::load(&lock_path)?;
    let Some(entry) = lock.skills.get(name).cloned() else {
        anyhow::bail!("skill `{name}` is not recorded in the lock");
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

    // Resolve each agent's on-disk directory. Several agents share one skills
    // directory (codex / cursor / opencode all use `.agents/skills`), so a plan
    // decides which removed agents should physically unlink and which only drop
    // ownership because a *remaining* agent still points at the same directory.
    let mut dir_of: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for agent_id in remove_from.iter().chain(remaining.iter()) {
        let Some(target) = agent_target(agent_id) else {
            if remove_from.contains(agent_id) {
                anyhow::bail!("unknown agent `{agent_id}`");
            }
            continue;
        };
        let dir = agent_dir(target, &scope_root, global)?;
        let canonical = dir.canonicalize().unwrap_or(dir);
        dir_of.insert(agent_id.clone(), canonical);
    }
    let plan = plan_unlinks(&remove_from, &remaining, &dir_of);

    let mut removed_any = false;
    for step in &plan {
        if step.physically_unlink {
            let dir = agent_dir(
                agent_target(&step.agent).expect("planned agent exists"),
                &scope_root,
                global,
            )?;
            if install::unlink_from(&dir, name)? {
                removed_any = true;
                println!("removed `{name}` from {}", step.agent);
            }
        } else {
            // A remaining agent shares this directory, or another removed agent
            // already unlinked it: keep the files, only drop ownership in lock.
            removed_any = true;
            println!(
                "unlinked `{name}` from {} (shared dir kept for other agents)",
                step.agent
            );
        }
    }

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

/// Reproduce every skill the lock declares (the counterpart to `add`).
async fn sync(app: &mut App, global: bool) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    if !lock_path.is_file() {
        println!("No lockfile; nothing to sync.");
        return Ok(());
    }
    let skills = lockfile::load(&lock_path)?.skills;
    if skills.is_empty() {
        println!("No skills in the lock; nothing to sync.");
        return Ok(());
    }

    // Reproduce each skill from its staged copy. When the copy is gone, a GitHub
    // source is re-fetched at its recorded commit and re-staged; the content hash
    // is then re-checked so a moved tag or a tampered mirror cannot silently swap
    // the bytes. A local source that has lost its staged copy cannot be replayed
    // (its origin path may be gone), so it is reported rather than guessed at.
    let scope_root = scope_root(app, global)?;
    let mode = link_mode(app, false);
    let mut missing = Vec::new();
    for (name, entry) in &skills {
        let mut staged = install::staged_root(&app.ctx.dirs, &entry.source, &entry.content_hash);
        if !staged.exists() {
            match refetch_staged(app, name, entry).await {
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
        for agent_id in &entry.agents {
            let Some(target) = agent_target(agent_id) else {
                continue;
            };
            let agent_dir = agent_dir(target, &scope_root, global)?;
            install::link_into(&agent_dir, name, &staged, mode)?;
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
async fn refetch_staged(app: &App, name: &str, entry: &LockedSkill) -> Result<Option<PathBuf>> {
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
    // Pick the same skill within the repo the lock recorded.
    let wanted = entry.skill.clone().unwrap_or_else(|| name.to_string());
    let skill_root = if root.join(install::SKILL_MANIFEST).is_file() {
        root
    } else {
        root.join(&wanted)
    };
    let package = install::read_skill_dir(&skill_root)?;
    let mode = link_mode(app, false);
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
/// For each targeted skill, re-resolve its GitHub source's ref to the current
/// commit; if that differs from the lock, re-download, re-stage, re-link the
/// agents it was in, and rewrite the lock. Local sources and commit-pinned
/// GitHub sources have nothing to re-resolve and are reported as up to date.
async fn update(app: &mut App, wanted: &[String], global: bool) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    if !lock_path.is_file() {
        println!("No lockfile; nothing to update.");
        return Ok(());
    }
    let mut lock = lockfile::load(&lock_path)?;
    let names: Vec<String> = if wanted.is_empty() {
        lock.skills.keys().cloned().collect()
    } else {
        wanted.to_vec()
    };
    if names.is_empty() {
        println!("No skills in the lock to update.");
        return Ok(());
    }

    let scope_root = scope_root(app, global)?;
    let mode = link_mode(app, false);
    let mut changed = false;
    for name in &names {
        let Some(entry) = lock.skills.get(name).cloned() else {
            anyhow::bail!("skill `{name}` is not recorded in the lock");
        };
        let source = SkillSource::parse(&entry.source)?;
        let SkillSource::GitHub {
            owner,
            repo,
            subdir,
        } = &source
        else {
            println!("`{name}`: local source, nothing to update");
            continue;
        };
        // Re-resolve the recorded ref. Without a ref, HEAD of the default branch
        // is the moving target; a commit-pinned entry resolves to itself.
        // The original ref lives in the project config, not the lock, so read it
        // from there; absent, HEAD of the default branch is the moving target.
        let reference = app
            .ctx
            .config
            .skills
            .get(name)
            .and_then(|declaration| declaration.r#ref.clone());
        let commit = skills::fetch::resolve_commit(&app.ctx, owner, repo, reference.as_deref())
            .await
            .with_context(|| format!("re-resolving {owner}/{repo}"))?;
        if Some(&commit) == entry.resolved_commit.as_ref() {
            println!("`{name}`: already at {}", &commit[..commit.len().min(12)]);
            continue;
        }

        let tree = skills::fetch::fetch_at_commit(&app.ctx, owner, repo, &commit).await?;
        let root = skills::fetch::subtree(&tree, subdir.as_deref())?;
        let wanted_dir = entry.skill.clone().unwrap_or_else(|| name.clone());
        let skill_root = if root.join(install::SKILL_MANIFEST).is_file() {
            root
        } else {
            root.join(&wanted_dir)
        };
        let package = install::read_skill_dir(&skill_root)?;
        let staged = install::stage(&app.ctx.dirs, &source.canonical(), &package, mode)?;
        for agent_id in &entry.agents {
            let Some(target) = agent_target(agent_id) else {
                continue;
            };
            let agent_dir = agent_dir(target, &scope_root, global)?;
            install::link_into(&agent_dir, name, &staged, mode)?;
        }
        if let Some(existing) = lock.skills.get_mut(name) {
            existing.resolved_commit = Some(commit.clone());
            existing.content_hash = package.content_hash();
        }
        changed = true;
        println!("`{name}`: updated to {}", &commit[..commit.len().min(12)]);
    }

    if changed {
        lockfile::save(&lock_path, &lock)?;
    }
    Ok(())
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

/// Which agents an install targets: explicit flags, else configured defaults,
/// else an error that tells the user how to choose.
fn resolve_agents(app: &App, explicit: &[String]) -> Result<Vec<&'static AgentTarget>> {
    let ids: Vec<String> = if !explicit.is_empty() {
        explicit.to_vec()
    } else {
        app.ctx.config.skills_defaults.default_agents.clone()
    };
    if ids.is_empty() {
        anyhow::bail!(
            "no target agent: pass -a/--agent (e.g. -a claude-code) or set \
             [skills].default_agents in osdk.toml; `osdk skills agents` lists them"
        );
    }
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

/// The link mode for staging and linking: explicit `--copy`, else the
/// `[skills].link_mode` override, else the global link mode.
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

/// The lockfile path for the chosen scope: the user lock for global, else the
/// nearest project lock (creating its path at the project root when absent).
fn lock_path_for(app: &App, global: bool) -> PathBuf {
    if global {
        app.ctx.dirs.user_lock_file()
    } else {
        let start = app
            .ctx
            .config
            .project_config_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        lockfile::default_path(&start)
    }
}

fn write_lock_entries(app: &App, global: bool, entries: Vec<(String, LockedSkill)>) -> Result<()> {
    let lock_path = lock_path_for(app, global);
    let mut lock = if lock_path.is_file() {
        lockfile::load(&lock_path)?
    } else {
        lockfile::Lockfile::default()
    };
    for (name, entry) in entries {
        lock.skills.insert(name, entry);
    }
    lockfile::save(&lock_path, &lock)?;
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
}
