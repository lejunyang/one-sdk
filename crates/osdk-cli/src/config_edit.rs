//! Format-preserving edits to config files via `toml_edit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::PROJECT_CONFIG_NAMES;
use osdk_core::config::{StructuredToolConfig, ToolConfigValue};

static NEXT_CONFIG_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

/// Write a `[tools] <tool> = <spec>` pin to the user global config.
pub fn set_global_tool(ctx: &Ctx, tool: &str, spec: &str) -> Result<()> {
    with_global_config_lock(ctx, || set_global_tool_unlocked(ctx, tool, spec))
}

/// Update a global tool version when the caller already holds the shared
/// global state lock.
pub(crate) fn set_global_tool_unlocked(ctx: &Ctx, tool: &str, spec: &str) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    edit_tool_version(&path, tool, spec)
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
    with_global_config_lock(ctx, || set_global_tool_config_unlocked(ctx, tool, config))
}

/// Update a global tool entry when the caller already holds the shared global
/// state lock. This avoids recursively acquiring the process-wide file lock.
pub(crate) fn set_global_tool_config_unlocked(
    ctx: &Ctx,
    tool: &str,
    config: &StructuredToolConfig,
) -> Result<()> {
    let path = ctx.dirs.user_config_file();
    edit_tool_config(&path, tool, config)
}

/// Remove one user-global tool selection. Callers that already hold the
/// shared global state lock must use this unlocked form.
pub(crate) fn remove_global_tool_unlocked(ctx: &Ctx, tool: &str) -> Result<bool> {
    let path = ctx.dirs.user_config_file();
    let mut doc = load_doc(&path)?;
    let removed = doc
        .get_mut("tools")
        .and_then(toml_edit::Item::as_table_mut)
        .map(|tools| tools.remove(tool).is_some())
        .unwrap_or(false);
    if removed {
        save_doc(&path, &doc)?;
    }
    Ok(removed)
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

/// Atomically update the exact Node runtime and a structured npm tool entry in
/// one project config publication. Existing Node options are retained.
pub fn set_project_npm_tool_at(
    path: &Path,
    node_version: &str,
    tool: &str,
    config: &StructuredToolConfig,
) -> Result<()> {
    let mut doc = load_doc(path)?;
    set_tool_version_in_doc(&mut doc, "node", node_version)?;
    set_tool_config_in_doc(&mut doc, tool, config)?;
    save_doc(path, &doc)
}

/// Set or clear a per-tool source pin in the user global config.
pub fn set_source_pin(ctx: &Ctx, tool: &str, id: Option<&str>) -> Result<()> {
    with_global_config_lock(ctx, || set_source_pin_unlocked(ctx, tool, id))
}

fn set_source_pin_unlocked(ctx: &Ctx, tool: &str, id: Option<&str>) -> Result<()> {
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
    with_global_config_lock(ctx, || {
        set_model_env_unlocked(ctx, provider, enabled, force)
    })
}

fn set_model_env_unlocked(
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
    with_global_config_lock(ctx, || set_version_alias_unlocked(ctx, tool, name, version))
}

fn set_version_alias_unlocked(ctx: &Ctx, tool: &str, name: &str, version: &str) -> Result<()> {
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
    with_global_config_lock(ctx, || remove_version_alias_unlocked(ctx, tool, name))
}

fn remove_version_alias_unlocked(ctx: &Ctx, tool: &str, name: &str) -> Result<bool> {
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
    with_global_config_lock(ctx, || {
        add_custom_source_unlocked(ctx, tool, id, download_url, index_url, forward_credentials)
    })
}

fn add_custom_source_unlocked(
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
    with_global_config_lock(ctx, || remove_custom_source_unlocked(ctx, tool, id))
}

fn remove_custom_source_unlocked(ctx: &Ctx, tool: &str, id: &str) -> Result<bool> {
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

fn with_global_config_lock<T>(ctx: &Ctx, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::global_npm_use::with_global_npm_state_lock(&ctx.dirs, operation)
}

fn edit_tool_version(path: &Path, tool: &str, spec: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    set_tool_version_in_doc(&mut doc, tool, spec)?;
    save_doc(path, &doc)?;
    Ok(())
}

fn set_tool_version_in_doc(doc: &mut toml_edit::DocumentMut, tool: &str, spec: &str) -> Result<()> {
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
    Ok(())
}

fn edit_tool_config(path: &Path, tool: &str, config: &StructuredToolConfig) -> Result<()> {
    let mut doc = load_doc(path)?;
    set_tool_config_in_doc(&mut doc, tool, config)?;
    save_doc(path, &doc)?;
    Ok(())
}

fn set_tool_config_in_doc(
    doc: &mut toml_edit::DocumentMut,
    tool: &str,
    config: &StructuredToolConfig,
) -> Result<()> {
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
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let file_name = path
        .file_name()
        .context("configuration path has no file name")?
        .to_string_lossy();
    let (temporary, mut file) = loop {
        let serial = NEXT_CONFIG_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.tmp-{}-{serial}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", temporary.display()))
            }
        }
    };
    use std::io::Write as _;
    let result = (|| -> Result<()> {
        file.write_all(doc.to_string().as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        drop(file);
        replace_file(&temporary, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| format!("replacing {}", destination.display()))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            REPLACE_EXISTING | WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| "replacing configuration file".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing configuration directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
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

    #[test]
    fn removing_global_tool_preserves_other_user_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            osdk_core::dirs::env_keys::DATA_DIR => {
                Some(temporary.path().join("data").display().to_string())
            }
            osdk_core::dirs::env_keys::CACHE_DIR => {
                Some(temporary.path().join("cache").display().to_string())
            }
            osdk_core::dirs::env_keys::CONFIG_DIR => {
                Some(temporary.path().join("config").display().to_string())
            }
            osdk_core::dirs::env_keys::STORE_DIR => {
                Some(temporary.path().join("store").display().to_string())
            }
            osdk_core::dirs::env_keys::INSTALL_DIR => {
                Some(temporary.path().join("installs").display().to_string())
            }
            _ => None,
        })
        .unwrap();
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            dirs.user_config_file(),
            "[tools]\nnode = \"22\"\n\"npm:prettier\" = { version = \"3\", installer = \"aube\" }\n\n[aliases.node]\nlts = \"22\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs,
            platform: osdk_core::platform::Platform::current(),
            config,
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(temporary.path().join("store"))),
            show_progress: false,
        };

        assert!(remove_global_tool_unlocked(&ctx, "npm:prettier").unwrap());
        assert!(!remove_global_tool_unlocked(&ctx, "npm:prettier").unwrap());
        let text = std::fs::read_to_string(ctx.dirs.user_config_file()).unwrap();
        assert!(!text.contains("npm:prettier"));
        assert!(text.contains("node = \"22\""));
        assert!(text.contains("lts = \"22\""));
    }
}
