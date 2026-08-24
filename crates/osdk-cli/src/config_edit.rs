//! Format-preserving edits to config files via `toml_edit`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::PROJECT_CONFIG_NAMES;
use osdk_core::config::{StructuredToolConfig, ToolConfigValue};

/// Write a `[tools] <tool> = <spec>` pin to the user global config.
pub fn set_global_tool(ctx: &Ctx, tool: &str, spec: &str) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    edit_tool_version(&path, tool, spec)?;
    Ok(())
}

/// Write a `[tools]` pin to the nearest project config, creating `osdk.toml` in
/// the current dir if none exists. Returns the file path written.
pub fn set_project_tool(tool: &str, spec: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = find_project_config(&cwd).unwrap_or_else(|| cwd.join("osdk.toml"));
    edit_tool_version(&path, tool, spec)?;
    Ok(path)
}

/// Write a structured `[tools] <tool> = { version = ..., ... }` entry to the
/// user global config.
#[allow(dead_code)]
pub fn set_global_tool_config(ctx: &Ctx, tool: &str, config: &StructuredToolConfig) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    edit_tool_config(&path, tool, config)?;
    Ok(())
}

/// Write a structured `[tools]` entry to the nearest project config, creating
/// `osdk.toml` in the current dir if none exists. Returns the file path written.
#[allow(dead_code)]
pub fn set_project_tool_config(tool: &str, config: &StructuredToolConfig) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = find_project_config(&cwd).unwrap_or_else(|| cwd.join("osdk.toml"));
    edit_tool_config(&path, tool, config)?;
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

pub fn set_model_env(
    ctx: &Ctx,
    provider: osdk_core::model::ProviderId,
    enabled: bool,
    force: bool,
) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;
    let sources = doc
        .entry("sources")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let sources = sources
        .as_table_mut()
        .context("`sources` is not a table in config")?;
    sources.set_implicit(true);
    let provider_item = sources
        .entry(provider.as_str())
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let provider_table = provider_item
        .as_table_mut()
        .context("`sources.<provider>` is not a table")?;
    if enabled {
        provider_table.insert("env", toml_edit::value(true));
        if force {
            provider_table.insert("env_force", toml_edit::value(true));
        } else {
            provider_table.remove("env_force");
        }
    } else {
        provider_table.remove("env");
        provider_table.remove("env_force");
    }
    save_doc(&path, &doc)
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

fn edit_tool_version(path: &Path, tool: &str, spec: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    let tools = doc
        .entry("tools")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tools_tbl = tools
        .as_table_mut()
        .context("`tools` is not a table in config")?;

    match tools_tbl.get_mut(tool) {
        Some(item) if item.is_inline_table() => {
            let inline = item
                .as_inline_table_mut()
                .context("`tools.<tool>` is not an inline table")?;
            inline.insert("version", toml_edit::Value::from(spec));
        }
        Some(item) if item.is_table() => {
            let table = item
                .as_table_mut()
                .context("`tools.<tool>` is not a table")?;
            table.insert("version", toml_edit::value(spec));
        }
        _ => {
            tools_tbl.insert(tool, toml_edit::value(spec));
        }
    }
    save_doc(path, &doc)?;
    Ok(())
}

fn edit_tool_config(path: &Path, tool: &str, config: &StructuredToolConfig) -> Result<()> {
    let mut doc = load_doc(path)?;
    let tools = doc
        .entry("tools")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tools_tbl = tools
        .as_table_mut()
        .context("`tools` is not a table in config")?;

    let mut inline = toml_edit::InlineTable::default();
    inline.insert("version", toml_edit::Value::from(config.version.as_str()));
    for (key, value) in &config.options {
        inline.insert(key, to_toml_value(value)?);
    }
    inline.fmt();
    tools_tbl.insert(
        tool,
        toml_edit::Item::Value(toml_edit::Value::InlineTable(inline)),
    );
    save_doc(path, &doc)?;
    Ok(())
}

fn to_toml_value(value: &ToolConfigValue) -> Result<toml_edit::Value> {
    Ok(match value {
        ToolConfigValue::String(value) => toml_edit::Value::from(value.as_str()),
        ToolConfigValue::Bool(value) => toml_edit::Value::from(*value),
        ToolConfigValue::Array(values) => {
            let mut array = toml_edit::Array::default();
            for value in values {
                array.push(toml_edit::Value::from(value.as_str()));
            }
            array.fmt();
            toml_edit::Value::Array(array)
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn structured(
        version: &str,
        options: impl IntoIterator<Item = (&'static str, ToolConfigValue)>,
    ) -> StructuredToolConfig {
        StructuredToolConfig {
            version: version.to_string(),
            options: options
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn legacy_string_update_preserves_inline_table_options() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(
            &path,
            "[tools]\nnpm = { version = \"11.5.1\", allow_builds = [\"esbuild\"], engine = \"node\" }\n",
        )
        .unwrap();

        edit_tool_version(&path, "npm", "11.5.2").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(
            "npm = { version = \"11.5.2\", allow_builds = [\"esbuild\"], engine = \"node\" }"
        ));
    }

    #[test]
    fn legacy_string_update_preserves_table_options() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(
            &path,
            "[tools.npm]\nversion = \"11.5.1\"\nallow_builds = [\"esbuild\"]\nengine = \"node\"\n",
        )
        .unwrap();

        edit_tool_version(&path, "npm", "11.5.2").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[tools.npm]"));
        assert!(text.contains("version = \"11.5.2\""));
        assert!(text.contains("allow_builds = [\"esbuild\"]"));
        assert!(text.contains("engine = \"node\""));
    }

    #[test]
    fn structured_write_round_trips_options_and_scoped_keys() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");

        edit_tool_config(
            &path,
            "@scope/tool",
            &structured(
                "1.2.3",
                [
                    (
                        "allow_builds",
                        ToolConfigValue::Array(vec!["esbuild".to_string(), "sharp".to_string()]),
                    ),
                    ("engine", ToolConfigValue::String("node".to_string())),
                    ("frozen", ToolConfigValue::Bool(true)),
                ],
            ),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"@scope/tool\" = {"));
        assert!(text.contains("version = \"1.2.3\""));
        assert!(text.contains("allow_builds = [\"esbuild\", \"sharp\"]"));
        assert!(text.contains("engine = \"node\""));
        assert!(text.contains("frozen = true"));

        let parsed: toml::Value = text.parse().unwrap();
        let tool = &parsed["tools"]["@scope/tool"];
        assert_eq!(tool["version"].as_str(), Some("1.2.3"));
        assert_eq!(tool["engine"].as_str(), Some("node"));
        assert_eq!(tool["frozen"].as_bool(), Some(true));
        assert_eq!(tool["allow_builds"][0].as_str(), Some("esbuild"));
        assert_eq!(tool["allow_builds"][1].as_str(), Some("sharp"));
    }

    #[test]
    fn writing_new_structured_entry_preserves_other_sections() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(&path, "[settings]\nyes = true\n").unwrap();

        edit_tool_config(
            &path,
            "npm",
            &structured(
                "11.5.2",
                [(
                    "allow_builds",
                    ToolConfigValue::Array(vec!["sharp".to_string()]),
                )],
            ),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[settings]"));
        assert!(text.contains("yes = true"));
        assert!(text.contains("[tools]"));
        assert!(text.contains("npm = { version = \"11.5.2\", allow_builds = [\"sharp\"] }"));
    }
}
