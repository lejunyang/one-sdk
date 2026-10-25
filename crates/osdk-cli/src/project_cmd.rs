//! `osdk project`: manage external project configuration bindings.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use osdk_core::config::PROJECT_CONFIG_NAMES;
use osdk_core::dirs::Dirs;
use osdk_core::lock::FileLock;
use osdk_core::project::{
    self, find_binding_for_root, list_bindings, Binding, Manifest, ProjectSource,
};

use crate::app::App;

#[derive(Debug, Serialize)]
struct ShowJson {
    root: String,
    source: &'static str,
    config_path: String,
    lock_path: String,
    binding_dir: Option<String>,
    shadowed_local_config: Option<String>,
    missing: bool,
}

#[derive(Debug, Serialize)]
struct BindingRow {
    root: String,
    binding_dir: String,
    config_path: String,
    lock_path: String,
    root_exists: bool,
    config_exists: bool,
    shadowed_by_local: bool,
}

pub(crate) fn run(app: &mut App, command: crate::cli::ProjectCommand) -> Result<()> {
    let dirs = &app.ctx.dirs;
    match command {
        crate::cli::ProjectCommand::Add {
            path,
            dry_run,
            json,
        } => add(dirs, path.as_deref(), dry_run, json),
        crate::cli::ProjectCommand::Show { path, json } => show(dirs, path.as_deref(), json),
        crate::cli::ProjectCommand::List { json } => list(dirs, json),
        crate::cli::ProjectCommand::Relocate {
            old,
            new,
            dry_run,
            json,
        } => relocate(dirs, &old, &new, dry_run, json),
        crate::cli::ProjectCommand::Remove {
            path,
            dry_run,
            json,
        } => remove(dirs, &path, dry_run, json),
    }
}

fn canonical_root(path: &Path) -> Result<PathBuf> {
    let root = dunce::canonicalize(path)
        .with_context(|| format!("project directory does not exist: {}", path.display()))?;
    if !root.is_dir() {
        bail!("project path is not a directory: {}", path.display());
    }
    Ok(root)
}

fn existing_local_config(root: &Path) -> Option<PathBuf> {
    PROJECT_CONFIG_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
}

fn lock_path_for(config_dir: &Path) -> PathBuf {
    osdk_core::project::projects_dir(config_dir).join(".lock")
}

pub(crate) fn add(dirs: &Dirs, path: Option<&Path>, dry_run: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let root = canonical_root(path.unwrap_or(&cwd))?;
    let binding_dir = project::binding_dir_for_root(&dirs.config, &root);

    // Re-entrant: the store edit is the only thing serialized, and only inside
    // the binding directory once created.
    let _guard = FileLock::acquire(lock_path_for(&dirs.config))?;

    if let Some(existing) = find_binding_for_root(&dirs.config, &root)? {
        if json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "root": root,
                    "binding_dir": existing.dir,
                    "status": "already-bound",
                }))?
            );
        } else {
            println!(
                "{} is already bound at {}",
                root.display(),
                existing.dir.display()
            );
        }
        return Ok(());
    }

    let shadowed = existing_local_config(&root);
    if !dry_run {
        std::fs::create_dir_all(&binding_dir)
            .with_context(|| format!("creating {}", binding_dir.display()))?;
        // Write the manifest via a temp file + rename so a crashed add never
        // leaves a directory discoverable with a half-written manifest.
        write_atomic(
            &binding_dir,
            "project.toml.tmp",
            "project.toml",
            &toml::to_string(&Manifest::new(root.clone()))?,
        )?;
        write_atomic(
            &binding_dir,
            "osdk.toml.tmp",
            "osdk.toml",
            "# external project config: edit me\n",
        )?;
        // The bundle owns its lock from birth: discovery must never find an
        // empty selection and then borrow a lock from somewhere else.
        write_atomic(
            &binding_dir,
            "osdk.lock.tmp",
            "osdk.lock",
            &crate::lockfile::empty_lock_contents(),
        )?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "root": root,
                "binding_dir": binding_dir,
                "created": !dry_run,
                "shadowed_by_local": shadowed,
            }))?
        );
    } else {
        println!("bound {} at {}", root.display(), binding_dir.display());
        if let Some(local) = &shadowed {
            eprintln!(
                "note: local {} exists and will take precedence over this binding",
                local.display()
            );
        }
        if dry_run {
            println!("(dry run: nothing written)");
        }
    }
    Ok(())
}

fn write_atomic(dir: &Path, tmp_name: &str, final_name: &str, contents: &str) -> Result<()> {
    let tmp = dir.join(tmp_name);
    let final_path = dir.join(final_name);
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &final_path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), final_path.display()))?;
    Ok(())
}

pub(crate) fn show(dirs: &Dirs, path: Option<&Path>, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let from = path.unwrap_or(&cwd);
    let selected = project::discover(from, &dirs.config)?;
    let root = selected
        .as_ref()
        .map(|ctx| ctx.root.clone())
        .unwrap_or_else(|| dunce::canonicalize(from).unwrap_or_else(|_| from.to_path_buf()));
    let binding = find_binding_for_root(&dirs.config, &root)?;

    if json {
        let out = ShowJson {
            root: root.to_string_lossy().into_owned(),
            source: match selected.as_ref() {
                Some(ctx) => match ctx.source {
                    ProjectSource::Local => "local",
                    ProjectSource::External => "external",
                },
                None => "none",
            },
            config_path: selected
                .as_ref()
                .map(|c| c.config_path.display().to_string())
                .unwrap_or_default(),
            lock_path: selected
                .as_ref()
                .map(|c| c.lock_path.display().to_string())
                .unwrap_or_default(),
            binding_dir: binding.as_ref().map(|b| b.dir.display().to_string()),
            shadowed_local_config: existing_local_config(&root).map(|p| p.display().to_string()),
            missing: binding.as_ref().is_some_and(|b| !b.root_exists()),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("project root: {}", root.display());
    match &selected {
        Some(ctx) => {
            let source = match ctx.source {
                ProjectSource::Local => "local",
                ProjectSource::External => "external binding",
            };
            println!("selected:     {source}");
            println!("config:       {}", ctx.config_path.display());
            println!("lock:         {}", ctx.lock_path.display());
        }
        None => println!("selected:     none"),
    }
    if let Some(binding) = &binding {
        println!("binding dir:  {}", binding.dir.display());
        if !binding.root_exists() {
            println!("status:       WARNING recorded root no longer exists");
        }
        if !binding.config_exists() {
            println!("status:       WARNING binding config missing");
        }
        if existing_local_config(&root).is_some() {
            println!("status:       shadowed by a local osdk.toml/.osdk.toml");
        }
    }
    Ok(())
}

pub(crate) fn list(dirs: &Dirs, json: bool) -> Result<()> {
    let bindings = list_bindings(&dirs.config)?;
    if json {
        let rows: Vec<BindingRow> = bindings
            .iter()
            .map(|b| BindingRow {
                root: b.root.display().to_string(),
                binding_dir: b.dir.display().to_string(),
                config_path: b.config_path.display().to_string(),
                lock_path: b.lock_path.display().to_string(),
                root_exists: b.root_exists(),
                config_exists: b.config_exists(),
                shadowed_by_local: existing_local_config(&b.root).is_some(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if bindings.is_empty() {
        println!("no external project bindings");
        return Ok(());
    }
    for b in &bindings {
        let mut flags = Vec::new();
        if !b.root_exists() {
            flags.push("project root missing");
        }
        if !b.config_exists() {
            flags.push("config missing");
        }
        if existing_local_config(&b.root).is_some() {
            flags.push("shadowed by local osdk.toml");
        }
        let status = if flags.is_empty() {
            "active".to_string()
        } else {
            flags.join("; ")
        };
        println!("{}", b.root.display());
        println!("  source: external binding");
        println!("  status: {status}");
        println!("  config: {}", b.config_path.display());
        println!("  lock:   {}", b.lock_path.display());
    }
    Ok(())
}

fn find_binding_by_path(dirs: &Dirs, path: &Path) -> Result<Binding> {
    let canonical = dunce::canonicalize(path).ok();
    if let Some(c) = &canonical {
        if let Some(b) = find_binding_for_root(&dirs.config, c)? {
            return Ok(b);
        }
    }
    // Missing roots: match the recorded root, so relocate/remove still works
    // after the project directory went away.
    list_bindings(&dirs.config)?
        .into_iter()
        .find(|b| canonical.as_ref() == Some(&b.root) || b.root.as_os_str() == path.as_os_str())
        .with_context(|| format!("no external project binding for {}", path.display()))
}

pub(crate) fn relocate(
    dirs: &Dirs,
    old: &Path,
    new: &Path,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let _guard = FileLock::acquire(lock_path_for(&dirs.config))?;
    let binding = find_binding_by_path(dirs, old)?;
    let new_root = canonical_root(new)?;

    if let Some(existing) = find_binding_for_root(&dirs.config, &new_root)? {
        bail!(
            "{} is already bound at {}; unbind it first",
            new_root.display(),
            existing.dir.display()
        );
    }
    let new_dir = project::binding_dir_for_root(&dirs.config, &new_root);

    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "from": binding.dir,
                "to": new_dir,
                "new_root": new_root,
                "applied": !dry_run,
            }))?
        );
    } else {
        println!(
            "relocating {} -> {}",
            binding.root.display(),
            new_root.display()
        );
        println!(
            "  binding: {} -> {}",
            binding.dir.display(),
            new_dir.display()
        );
    }
    if dry_run {
        if !json {
            println!("(dry run: nothing changed)");
        }
        return Ok(());
    }

    std::fs::create_dir_all(&new_dir)?;
    // Move each bundle file individually; leave the old directory removed.
    for name in ["project.toml", "osdk.toml"] {
        let src = binding.dir.join(name);
        if src.exists() {
            std::fs::rename(&src, new_dir.join(name))
                .with_context(|| format!("moving {}", src.display()))?;
        }
    }
    let lock = binding.lock_path.clone();
    if lock.exists() {
        std::fs::rename(&lock, new_dir.join("osdk.lock"))?;
    }
    // Rewrite the manifest with the new recorded root.
    write_atomic(
        &new_dir,
        "project.toml.tmp",
        "project.toml",
        &toml::to_string(&Manifest::new(new_root.clone()))?,
    )?;
    // Remove the now-empty old bundle directory.
    let _ = std::fs::remove_dir(&binding.dir);

    // The config file moved; its old trust record no longer points at anything.
    eprintln!(
        "note: if {} needed trust, run `osdk trust {}` again",
        new_dir.join("osdk.toml").display(),
        new_dir.join("osdk.toml").display()
    );
    Ok(())
}

pub(crate) fn remove(dirs: &Dirs, path: &Path, dry_run: bool, json: bool) -> Result<()> {
    let _guard = FileLock::acquire(lock_path_for(&dirs.config))?;
    let binding = find_binding_by_path(dirs, path)?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "binding_dir": binding.dir,
                "root": binding.root,
                "deleted": !dry_run,
            }))?
        );
    } else {
        println!("removing binding for {}", binding.root.display());
        println!("  deleting {}", binding.dir.display());
    }
    if dry_run {
        if !json {
            println!("(dry run: nothing deleted)");
        }
        return Ok(());
    }
    std::fs::remove_dir_all(&binding.dir)
        .with_context(|| format!("deleting {}", binding.dir.display()))?;
    Ok(())
}
