//! `osdk model view` command implementation.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context};
use osdk_core::model::view::{ViewEntry, ViewEntrySpec, ViewKind, ViewState, ViewStore};

/// Reconcile one model's consumer views from a lock/config declaration into the
/// persisted [`ViewState`] and (re)render them. Idempotent: declaring the same
/// views again is a no-op render; changing a mapping replaces the entry.
///
/// Unknown consumer names are skipped with a warning rather than failing the
/// whole sync: a newer osdk may declare a consumer this build cannot render, and
/// ignoring it is forward-compatible with how the lock tolerates unknown fields.
pub(crate) fn reconcile_declared_views(
    app: &crate::App,
    model: &str,
    declared: &std::collections::BTreeMap<String, crate::lockfile::LockedModelView>,
) -> anyhow::Result<()> {
    if declared.is_empty() {
        return Ok(());
    }
    let views = view_store(app);
    let mut state = ViewState::load(&app.ctx.dirs)?;
    for (consumer, view) in declared {
        let Ok(kind) = consumer.parse::<ViewKind>() else {
            eprintln!(
                "warning: model `{model}` declares unknown view consumer `{consumer}`; \
                 this build supports only comfyui|hf-cache, skipping"
            );
            continue;
        };
        let profile = if view.profile.is_empty() {
            "default"
        } else {
            view.profile.as_str()
        };
        state.add(
            kind,
            profile,
            ViewEntrySpec {
                model: model.to_string(),
                map: view.map.clone(),
            },
        );
        state.save(&app.ctx.dirs)?;
        let entries = entries_for(&state, kind, profile);
        let reports = views.render(kind, profile, &entries)?;
        print_render_report(model, reports.get(model));
    }
    Ok(())
}

use crate::cli::ModelViewCommand;
use crate::App;

fn view_store(app: &App) -> ViewStore {
    ViewStore::new(
        app.ctx.dirs.clone(),
        app.ctx.cas.clone(),
        app.ctx.config.settings.link_mode,
    )
}

fn parse_mappings(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for item in raw {
        let (prefix, category) = item
            .split_once('=')
            .with_context(|| format!("--map expects PREFIX=CATEGORY, got `{item}`"))?;
        let prefix = prefix.trim();
        let category = category.trim();
        if prefix.is_empty() || category.is_empty() {
            return Err(anyhow!(
                "--map PREFIX and CATEGORY must be non-empty: `{item}`"
            ));
        }
        // Normalize separators in the prefix to `/`, the on-disk/lock convention.
        map.insert(prefix.replace('\\', "/"), category.to_string());
    }
    Ok(map)
}

fn entries_for(state: &ViewState, kind: ViewKind, profile: &str) -> Vec<ViewEntry> {
    state
        .profiles(kind)
        .get(profile)
        .map(|specs| {
            specs
                .iter()
                .map(|s| ViewEntry {
                    model: s.model.clone(),
                    map: s.map.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn model_view(app: &App, command: ModelViewCommand) -> anyhow::Result<()> {
    let views = view_store(app);
    let mut state = ViewState::load(&app.ctx.dirs)?;

    match command {
        ModelViewCommand::Add {
            kind,
            model,
            profile,
            map,
        } => {
            osdk_core::model::validate_model_name(&model)?;
            let map = parse_mappings(&map)?;
            // Fail fast if the model is not pulled: a view over a missing
            // snapshot renders nothing, and silently accepting would make
            // `add` look successful with an empty category.
            if !views.model_exists(&model)? {
                return Err(anyhow!(
                    "model `{model}` is not pulled; run `osdk model pull {model} …` first"
                ));
            }
            state.add(
                kind,
                &profile,
                ViewEntrySpec {
                    model: model.clone(),
                    map,
                },
            );
            state.save(&app.ctx.dirs)?;
            let reports = views.render(kind, &profile, &entries_for(&state, kind, &profile))?;
            print_render_report(&model, reports.get(&model));
            println!("view root: {}", views.view_root(kind, &profile)?.display());
        }
        ModelViewCommand::List => {
            if state.consumers.is_empty() {
                println!("no model views configured");
                return Ok(());
            }
            for (consumer, view_consumer) in &state.consumers {
                for (profile, specs) in &view_consumer.profiles {
                    let models: Vec<&str> = specs.iter().map(|s| s.model.as_str()).collect();
                    println!("{consumer}/{profile}: {}", models.join(", "));
                }
            }
        }
        ModelViewCommand::Path { kind, profile } => {
            println!("{}", views.view_root(kind, &profile)?.display());
        }
        ModelViewCommand::Rebuild { kind } => {
            let kinds: Vec<ViewKind> = match kind {
                Some(k) => vec![k],
                None => ViewKind::all().to_vec(),
            };
            for k in kinds {
                for profile in state.profiles(k).keys().cloned().collect::<Vec<_>>() {
                    let entries = entries_for(&state, k, &profile);
                    let reports = views.render(k, &profile, &entries)?;
                    println!("[{k}/{profile}]");
                    for (model, report) in &reports {
                        print_render_report(model, Some(report));
                    }
                }
            }
        }
        ModelViewCommand::Remove {
            kind,
            profile,
            model,
        } => {
            let present = state.remove(kind, &profile, model.as_deref());
            state.save(&app.ctx.dirs)?;
            // Remove the rendered subtree (or the whole profile dir when no
            // specific model was given).
            let removed = views.remove(kind, &profile, model.as_deref())?;
            if present || removed {
                println!("removed from view");
            } else {
                println!("nothing to remove");
            }
        }
        ModelViewCommand::Export { kind, profile, to } => {
            let fragment = export_fragment(kind, &profile, &views)?;
            match to {
                Some(path) => merge_yaml(&path, kind, &profile, &fragment)?,
                None => {
                    // Printed path: the one-time action for Desktop.
                    println!("{fragment}");
                    println!("# source edition: save this as extra_model_paths.yaml in the ComfyUI directory");
                    println!("# Desktop: add this root once in Settings -> Storage -> Add Shared Directory:");
                    println!("#   {}", views.view_root(kind, &profile)?.display());
                }
            }
        }
        ModelViewCommand::Doctor { kind, profile } => {
            let reports = views.render(kind, &profile, &entries_for(&state, kind, &profile))?;
            let mut problems = 0usize;
            for (model, report) in &reports {
                if !report.unclassified.is_empty() {
                    problems += report.unclassified.len();
                    println!(
                        "{model}: {} unclassified (not placed):",
                        report.unclassified.len()
                    );
                    for file in &report.unclassified {
                        println!("  {file}");
                    }
                }
                if report.copies > 0 {
                    println!(
                        "{model}: {} file(s) fell back to a byte copy (different volume); \
                         links were not possible",
                        report.copies
                    );
                }
            }
            if problems == 0 {
                println!("all rendered files classified; no problems");
            }
        }
    }
    Ok(())
}

fn print_render_report(model: &str, report: Option<&osdk_core::model::view::RenderReport>) {
    let Some(report) = report else {
        println!("{model}: not pulled yet, skipped");
        return;
    };
    println!(
        "{model}: {} placed, {} unclassified{}",
        report.placed.len(),
        report.unclassified.len(),
        if report.copies > 0 {
            format!(" ({} byte-copied: different volume)", report.copies)
        } else {
            String::new()
        }
    );
    for file in &report.unclassified {
        println!("  unclassified: {file}");
    }
}

/// Build the consumer fragment. ComfyUI maps category names 1:1 to its model
/// folders; hf-cache needs no per-category fragment (the hub client discovers
/// the layout by itself), so exporting it just points at the view root.
fn export_fragment(kind: ViewKind, profile: &str, views: &ViewStore) -> anyhow::Result<String> {
    let root = views.view_root(kind, profile)?;
    let key = format!("osdk-{}-{}", kind.as_str(), profile);
    let root_str = root.display().to_string().replace('\\', "/");
    match kind {
        ViewKind::Comfyui => {
            // One base_path + every category that exists in the view. The
            // categories are ComfyUI's own folder names, each on its own line.
            // Deliberately NO is_default (research §5.11).
            let cats = osdk_core::model::view::comfyui::categories();
            let mut body = String::new();
            body.push_str(&format!("{key}:\n"));
            body.push_str(&format!("  base_path: {root_str}\n"));
            for cat in cats {
                body.push_str(&format!("  {cat}: {cat}\n"));
            }
            Ok(body)
        }
        ViewKind::HfCache => Ok(format!(
            "# hf-cache layout is read directly by the hub client; set HF_HOME to this root.\n\
             # {key}: {root_str}\n"
        )),
    }
}

/// Merge (or create) a ComfyUI `extra_model_paths.yaml` fragment idempotently.
///
/// We append/replace the uniquely-keyed section. Parsing the existing file with
/// a minimal line-based approach would risk corrupting user comments; instead
/// we only ever manage our own keyed block, delimited by markers, so repeated
/// exports are safe and user content elsewhere is untouched.
fn merge_yaml(path: &str, kind: ViewKind, profile: &str, fragment: &str) -> anyhow::Result<()> {
    let path = std::path::Path::new(path);
    let key = format!("osdk-{}-{}", kind.as_str(), profile);
    let begin = format!("# >>> osdk view {key} (managed, do not edit) >>>");
    let end = format!("# <<< osdk view {key} <<<");

    let existing = if path.is_file() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };

    // Strip any prior managed block for this key.
    let mut out = String::with_capacity(existing.len() + fragment.len() + 64);
    let mut skipping = false;
    for line in existing.lines() {
        if line.trim() == begin {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim() == end {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&begin);
    out.push('\n');
    out.push_str(fragment.trim_end());
    out.push('\n');
    out.push_str(&end);
    out.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    println!("merged view fragment into {} (key: {key})", path.display());
    Ok(())
}
