//! Format-preserving edits to config files via `toml_edit`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::PROJECT_CONFIG_NAMES;

/// Write a `[tools] <tool> = <spec>` pin to the user global config.
pub fn set_global_tool(ctx: &Ctx, tool: &str, spec: &str) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    edit_tool(&path, tool, spec)?;
    Ok(())
}

/// Write a `[tools]` pin to the nearest project config, creating `osdk.toml` in
/// the current dir if none exists. Returns the file path written.
pub fn set_project_tool(tool: &str, spec: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = find_project_config(&cwd).unwrap_or_else(|| cwd.join("osdk.toml"));
    edit_tool(&path, tool, spec)?;
    Ok(path)
}

/// Set or clear a per-tool source pin in the user global config.
pub fn set_source_pin(ctx: &Ctx, tool: &str, id: Option<&str>) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;

    let sources = doc
        .entry("sources")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let sources_tbl = sources
        .as_table_mut()
        .context("`sources` is not a table in config")?;
    sources_tbl.set_implicit(true);

    let tool_item = sources_tbl
        .entry(tool)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tool_tbl = tool_item
        .as_table_mut()
        .context("`sources.<tool>` is not a table")?;

    match id {
        Some(id) => {
            tool_tbl.insert("pin", toml_edit::value(id));
        }
        None => {
            tool_tbl.remove("pin");
        }
    }
    save_doc(&path, &doc)?;
    Ok(())
}

pub fn set_version_alias(ctx: &Ctx, tool: &str, name: &str, version: &str) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;
    let aliases = doc
        .entry("aliases")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let aliases = aliases
        .as_table_mut()
        .context("`aliases` is not a table in config")?;
    aliases.set_implicit(true);
    let tool_aliases = aliases
        .entry(tool)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tool_aliases = tool_aliases
        .as_table_mut()
        .context("`aliases.<tool>` is not a table")?;
    tool_aliases.insert(name, toml_edit::value(version));
    save_doc(&path, &doc)
}

pub fn remove_version_alias(ctx: &Ctx, tool: &str, name: &str) -> Result<bool> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;
    let removed = doc
        .get_mut("aliases")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|aliases| aliases.get_mut(tool))
        .and_then(toml_edit::Item::as_table_mut)
        .map(|aliases| aliases.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        save_doc(&path, &doc)?;
    }
    Ok(removed)
}

/// Add a custom source to a tool's `[[sources.<tool>.custom]]` array.
pub fn add_custom_source(
    ctx: &Ctx,
    tool: &str,
    id: &str,
    download_url: &str,
    index_url: Option<&str>,
    forward_credentials: bool,
) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;

    let sources = doc
        .entry("sources")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let sources_tbl = sources
        .as_table_mut()
        .context("`sources` is not a table in config")?;
    sources_tbl.set_implicit(true);

    let tool_item = sources_tbl
        .entry(tool)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tool_tbl = tool_item
        .as_table_mut()
        .context("`sources.<tool>` is not a table")?;

    // Get or create the `custom` array-of-tables.
    let custom_item = tool_tbl
        .entry("custom")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ));
    let array = custom_item
        .as_array_of_tables_mut()
        .context("`sources.<tool>.custom` is not an array of tables")?;

    // Replace an existing entry with the same id.
    let existing = array
        .iter()
        .position(|t| t.get("id").and_then(|v| v.as_str()) == Some(id));
    if let Some(pos) = existing {
        array.remove(pos);
    }

    let mut tbl = toml_edit::Table::new();
    tbl.insert("id", toml_edit::value(id));
    tbl.insert("kind", toml_edit::value("custom"));
    tbl.insert("download_url", toml_edit::value(download_url));
    if let Some(idx) = index_url {
        tbl.insert("index_url", toml_edit::value(idx));
    }
    if forward_credentials {
        tbl.insert("forward_credentials", toml_edit::value(true));
    }
    array.push(tbl);

    save_doc(&path, &doc)?;
    Ok(())
}

/// Remove a custom source by id from a tool. Returns whether one was removed.
pub fn remove_custom_source(ctx: &Ctx, tool: &str, id: &str) -> Result<bool> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;
    let removed = (|| {
        let array = doc
            .get_mut("sources")?
            .as_table_mut()?
            .get_mut(tool)?
            .as_table_mut()?
            .get_mut("custom")?
            .as_array_of_tables_mut()?;
        let pos = array
            .iter()
            .position(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))?;
        array.remove(pos);
        Some(())
    })()
    .is_some();
    if removed {
        save_doc(&path, &doc)?;
    }
    Ok(removed)
}

fn edit_tool(path: &Path, tool: &str, spec: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    let tools = doc
        .entry("tools")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tools_tbl = tools
        .as_table_mut()
        .context("`tools` is not a table in config")?;
    tools_tbl.insert(tool, toml_edit::value(spec));
    save_doc(path, &doc)?;
    Ok(())
}

fn load_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        text.parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(toml_edit::DocumentMut::new())
    }
}

fn save_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        for name in PROJECT_CONFIG_NAMES {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        cur = dir.parent();
    }
    None
}
