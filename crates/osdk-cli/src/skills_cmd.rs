//! `osdk skills` command handlers.
//!
//! P0-4a covers the offline surface: `agents`, `list`, `add` from a local path,
//! `remove`, `path`, and `sync` from `osdk.lock`. GitHub sources and the network
//! path land in P0-4b; `add` of a GitHub source reports that clearly rather than
//! pretending to work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

    // Resolve the source to one or more on-disk skill roots. P0-4a handles local
    // sources; GitHub is wired in P0-4b.
    let (roots, _resolved_commit): (Vec<(String, PathBuf)>, Option<String>) = match &source {
        SkillSource::Local { path } => {
            if args.reference.is_some() {
                anyhow::bail!("--ref applies only to a GitHub source, not a local path");
            }
            (local_roots(path, &args.skills)?, None)
        }
        SkillSource::GitHub { .. } => {
            let selector = args
                .reference
                .as_deref()
                .map(|r| format!(" (requested ref `{r}`)"))
                .unwrap_or_default();
            anyhow::bail!(
                "GitHub sources are not available yet in this build{selector}; install from a \
                 local path for now (this is the P0-4b step)"
            )
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
                resolved_commit: _resolved_commit.clone(),
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
    let mut removed_any = false;
    for agent_id in &remove_from {
        let Some(target) = agent_target(agent_id) else {
            anyhow::bail!("unknown agent `{agent_id}`");
        };
        let agent_dir = agent_dir(target, &scope_root, global)?;
        if install::unlink_from(&agent_dir, name)? {
            removed_any = true;
            println!("removed `{name}` from {agent_id}");
        }
    }

    // Update the lock: drop the whole entry when no agent still holds it,
    // otherwise keep it with the remaining agents.
    let remaining: Vec<String> = entry
        .agents
        .iter()
        .filter(|a| !remove_from.contains(a))
        .cloned()
        .collect();
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

    // The staged copy is content-addressed, so a sync can only replay a skill
    // whose staged content is still present. Re-fetching a missing GitHub source
    // is the P0-4b path; here we replay what is staged and report what is not.
    let scope_root = scope_root(app, global)?;
    let mode = link_mode(app, false);
    let mut missing = Vec::new();
    for (name, entry) in &skills {
        let staged = install::staged_root(&app.ctx.dirs, &entry.source, &entry.content_hash);
        if !staged.exists() {
            missing.push(name.clone());
            continue;
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
            "cannot reproduce {} skill(s) with no staged copy: {} (re-adding from source is the P0-4b path)",
            missing.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

// --- helpers ---------------------------------------------------------------

/// Enumerate skill roots from a local source, honoring `--skill` selection.
///
/// A directory that is itself a skill (has `SKILL.md`) is one root named by its
/// frontmatter. A directory of skills (subdirectories each with `SKILL.md`) is
/// several; `--skill` narrows them by directory name.
fn local_roots(path: &Path, wanted: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if !path.is_dir() {
        anyhow::bail!("local skill source `{}` is not a directory", path.display());
    }
    if path.join(install::SKILL_MANIFEST).is_file() {
        // Single skill: its name comes from the frontmatter, read by the caller.
        let package = install::read_skill_dir(path)?;
        return Ok(vec![(package.name, path.to_path_buf())]);
    }

    // A directory of skills: each immediate subdir that holds a SKILL.md.
    let mut roots = Vec::new();
    for entry in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child = entry.path();
        if child.join(install::SKILL_MANIFEST).is_file() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if wanted.is_empty() || wanted.iter().any(|w| w == "*" || w == &dir_name) {
                roots.push((dir_name, child));
            }
        }
    }
    if roots.is_empty() {
        anyhow::bail!(
            "no {} found in {} or its immediate subdirectories",
            install::SKILL_MANIFEST,
            path.display()
        );
    }
    roots.sort();
    Ok(roots)
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
