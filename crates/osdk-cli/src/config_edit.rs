//! Format-preserving edits to config files via `toml_edit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::PROJECT_CONFIG_NAMES;
use osdk_core::config::{StructuredToolConfig, ToolConfigValue};

static NEXT_CONFIG_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

/// Write a task into the nearest project config, creating `osdk.toml` if there
/// is none. Returns the file written.
///
/// Goes through `toml_edit` for the same reason every other edit here does: a
/// config is something a person wrote, with their comments and their ordering,
/// and a command that reformats the file as a side effect of adding one entry
/// makes the diff unreviewable.
pub fn set_project_task(
    name: &str,
    run: &[String],
    description: Option<&str>,
    depends: &[String],
) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = find_project_config(&cwd).unwrap_or_else(|| cwd.join("osdk.toml"));
    let mut doc = load_doc(&path)?;

    let tasks = doc
        .entry("tasks")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .with_context(|| format!("{}: `tasks` is not a table", path.display()))?;
    // Implicit so `[tasks.build]` renders as its own section rather than
    // forcing a bare `[tasks]` header above it.
    tasks.set_implicit(true);

    // A single command with no metadata stays on one line: the shorthand is
    // what most tasks are, and expanding every one into a table would make the
    // file harder to read than the user's own hand-written entries.
    let simple = run.len() == 1 && description.is_none() && depends.is_empty();
    if simple {
        tasks.insert(name, toml_edit::value(run[0].clone()));
        save_doc(&path, &doc)?;
        return Ok(path);
    }

    let entry = tasks
        .entry(name)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = entry
        .as_table_mut()
        .with_context(|| format!("{}: task `{name}` is not a table", path.display()))?;

    if run.len() == 1 {
        table.insert("run", toml_edit::value(run[0].clone()));
    } else {
        let mut array = toml_edit::Array::new();
        for command in run {
            array.push(command.as_str());
        }
        table.insert("run", toml_edit::value(array));
    }
    if let Some(description) = description {
        table.insert("description", toml_edit::value(description));
    }
    if !depends.is_empty() {
        let mut array = toml_edit::Array::new();
        for dependency in depends {
            array.push(dependency.as_str());
        }
        table.insert("depends", toml_edit::value(array));
    }

    save_doc(&path, &doc)?;
    Ok(path)
}

/// Remove a task from the nearest project config.
///
/// Returns the path and whether anything was removed, so the caller can tell
/// "deleted" from "was not there" instead of reporting success either way.
pub fn remove_project_task(name: &str) -> Result<(PathBuf, bool)> {
    let cwd = std::env::current_dir()?;
    let Some(path) = find_project_config(&cwd) else {
        return Err(anyhow::anyhow!("no osdk project config found"));
    };
    let mut doc = load_doc(&path)?;

    let Some(tasks) = doc.get_mut("tasks").and_then(toml_edit::Item::as_table_mut) else {
        return Ok((path, false));
    };
    let removed = tasks.remove(name).is_some();
    // An empty `[tasks]` left behind is noise the user did not write.
    let now_empty = tasks.is_empty();
    if now_empty {
        doc.remove("tasks");
    }
    if removed {
        save_doc(&path, &doc)?;
    }
    Ok((path, removed))
}

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

/// A setting that `osdk config set` understands.
///
/// Settings are declared rather than derived from `Settings` so the writable
/// surface stays deliberate: `set` must not expose derived state such as
/// resolved directories, and each entry states how its text is validated
/// before anything is written.
pub struct SettingSpec {
    /// Dotted name as the user types it.
    pub key: &'static str,
    /// Path into the TOML document.
    pub path: &'static [&'static str],
    /// How the value is parsed and validated.
    pub kind: SettingKind,
}

#[derive(Clone, Copy)]
pub enum SettingKind {
    Bool,
    /// A positive integer; zero would stall the work it bounds.
    PositiveInt,
    /// One of a fixed set of words.
    Enum(&'static [&'static str]),
    /// Validated by the setting type's own parser.
    ///
    /// A hand-copied list of variants here would be a second source of truth
    /// for the same vocabulary, and it drifted immediately: the first version
    /// accepted `preferred` for `attestations` (the real word is
    /// `if-available`), invented `deny` for `prerelease`, and omitted
    /// `reflink` from `link_mode`. Because validation passed, the bad value was
    /// written, and every later command then failed to load the config it had
    /// just produced. Delegating keeps the vocabulary in one place, and the
    /// aliases each parser already accepts keep working.
    Parsed(ParsedSetting),
    /// A comma-separated list stored as a TOML array.
    List,
}

/// Settings whose vocabulary belongs to a type in `osdk-core`.
#[derive(Clone, Copy)]
pub enum ParsedSetting {
    Attestations,
    Prerelease,
    LinkMode,
}

impl ParsedSetting {
    /// Parse `value`, returning the canonical spelling to store.
    ///
    /// Round-tripping through `Display` normalizes accepted aliases, so
    /// `attestations=auto` is stored as `if-available` -- the form the loader
    /// reads back.
    fn canonical(self, value: &str) -> Result<String> {
        Ok(match self {
            Self::Attestations => value
                .parse::<osdk_core::config::AttestationPolicy>()
                .map(|parsed| parsed.to_string())?,
            Self::Prerelease => value
                .parse::<osdk_core::config::PrereleasePolicy>()
                .map(|parsed| parsed.to_string())?,
            Self::LinkMode => value
                .parse::<osdk_core::store::link::LinkMode>()
                .map(|parsed| parsed.to_string())?,
        })
    }
}

/// Every setting `config set` accepts.
///
/// Deliberately narrower than the full `Settings` struct: tool pins go through
/// `use`, source pins through `source pin`, and aliases through `alias`, each
/// of which validates far more than a scalar assignment could.
pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        key: "jobs",
        path: &["settings", "jobs"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "offline",
        path: &["settings", "offline"],
        kind: SettingKind::Bool,
    },
    SettingSpec {
        key: "yes",
        path: &["settings", "yes"],
        kind: SettingKind::Bool,
    },
    SettingSpec {
        key: "verify_signatures",
        path: &["settings", "verify_signatures"],
        kind: SettingKind::Bool,
    },
    SettingSpec {
        key: "require_checksums",
        path: &["settings", "require_checksums"],
        kind: SettingKind::Bool,
    },
    SettingSpec {
        key: "attestations",
        path: &["settings", "attestations"],
        kind: SettingKind::Parsed(ParsedSetting::Attestations),
    },
    SettingSpec {
        key: "prerelease",
        path: &["settings", "prerelease"],
        kind: SettingKind::Parsed(ParsedSetting::Prerelease),
    },
    SettingSpec {
        key: "link_mode",
        path: &["settings", "link_mode"],
        kind: SettingKind::Parsed(ParsedSetting::LinkMode),
    },
    SettingSpec {
        key: "lang",
        path: &["settings", "lang"],
        kind: SettingKind::Enum(&["en", "zh"]),
    },
    SettingSpec {
        key: "shims.include",
        path: &["settings", "shims", "include"],
        kind: SettingKind::List,
    },
    SettingSpec {
        key: "shims.expose",
        path: &["settings", "shims", "expose"],
        kind: SettingKind::List,
    },
    SettingSpec {
        key: "shims.exclude",
        path: &["settings", "shims", "exclude"],
        kind: SettingKind::List,
    },
    // Registry candidates live in the top-level `[registries]` table rather
    // than under `[settings]`, because they configure delegated package
    // managers instead of osdk's own downloads.
    //
    // Exposing them as settings is what makes "sources and configuration are
    // managed through osdk's own commands" actually true for Python. Until
    // now the only way to point osdk at a mirror was to hand-edit
    // `config.toml` -- which the guide already told readers they would not
    // have to do.
    // Source probing. Persisted under the top-level `[sources]` table rather
    // than `[settings]`, alongside `selection` and `cache_ttl`.
    //
    // Exposing this is what frees a slow-network user from hand-editing
    // config.toml. The model probe spends one budget on both a metadata request
    // and a bounded range fetch, so a 1500 ms default tuned for a small version
    // index can classify a perfectly usable local source as unreachable.
    SettingSpec {
        key: "sources.probe_timeout_ms",
        path: &["sources", "probe_timeout_ms"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "sources.model_probe_timeout_ms",
        path: &["sources", "model_probe_timeout_ms"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "sources.model_download_attempts",
        path: &["sources", "model_download_attempts"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "sources.model_download_retry_base_ms",
        path: &["sources", "model_download_retry_base_ms"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "sources.model_jobs",
        path: &["sources", "model_jobs"],
        kind: SettingKind::PositiveInt,
    },
    SettingSpec {
        key: "registries.python.urls",
        path: &["registries", "python", "urls"],
        kind: SettingKind::List,
    },
    SettingSpec {
        key: "registries.npm.urls",
        path: &["registries", "npm", "urls"],
        kind: SettingKind::List,
    },
];

pub fn find_setting(key: &str) -> Option<&'static SettingSpec> {
    SETTINGS.iter().find(|setting| setting.key == key)
}

/// A setting to write, either from the static table or built for a per-tool key.
///
/// Per-tool keys (`shims.<tool>.expose`) carry a segment taken from the user's
/// argument, so they cannot be `&'static`. Keeping them in one owned type means
/// `set_setting` / `unset_setting` have a single code path and cannot drift
/// between the static and dynamic cases.
pub struct ResolvedSetting {
    pub key: String,
    pub path: Vec<String>,
    pub kind: SettingKind,
}

impl ResolvedSetting {
    fn from_static(setting: &'static SettingSpec) -> Self {
        Self {
            key: setting.key.to_string(),
            path: setting.path.iter().map(|part| part.to_string()).collect(),
            kind: setting.kind,
        }
    }
}

/// The three shim lists that can be scoped to a single tool.
const TOOL_SHIM_FIELDS: [&str; 3] = ["include", "exclude", "expose"];

/// Resolve a key the user typed, accepting per-tool shim keys.
///
/// Shape: `shims.<tool>.<field>`, e.g. `shims.conda:m2-base.expose`. The tool id
/// itself contains a colon, and TOML quotes it on write, so the only ambiguity is
/// with the global `shims.include`; that one is in the static table and is tried
/// first.
pub fn resolve_setting(key: &str) -> Result<ResolvedSetting> {
    if let Some(found) = find_setting(key) {
        return Ok(ResolvedSetting::from_static(found));
    }
    if let Some(rest) = key.strip_prefix("shims.") {
        // Split at the *last* dot: everything before it is the tool id, which may
        // itself contain dots (`conda:python.pkg`), and the field never does.
        if let Some((tool, field)) = rest.rsplit_once('.') {
            if TOOL_SHIM_FIELDS.contains(&field) && !tool.is_empty() {
                return Ok(ResolvedSetting {
                    key: key.to_string(),
                    path: vec![
                        "settings".to_string(),
                        "shims".to_string(),
                        "tools".to_string(),
                        tool.to_string(),
                        field.to_string(),
                    ],
                    kind: SettingKind::List,
                });
            }
        }
    }
    let known: Vec<&str> = SETTINGS.iter().map(|setting| setting.key).collect();
    anyhow::bail!(
        "unknown setting `{key}`\n  known: {}\n  per-tool shim lists: shims.<tool>.{{include|exclude|expose}}",
        known.join(", ")
    )
}

/// Parse and validate `value` for `setting`.
///
/// Validation happens before the file is touched, so a rejected value leaves
/// the config exactly as it was rather than writing something the loader would
/// later refuse to parse.
fn setting_value(setting: &ResolvedSetting, value: &str) -> Result<toml_edit::Item> {
    let value = value.trim();
    Ok(match setting.kind {
        SettingKind::Bool => {
            let parsed = match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => anyhow::bail!("`{}` expects a boolean, got `{value}`", setting.key),
            };
            toml_edit::value(parsed)
        }
        SettingKind::PositiveInt => {
            let parsed: i64 = value
                .parse()
                .with_context(|| format!("`{}` expects a number, got `{value}`", setting.key))?;
            if parsed < 1 {
                anyhow::bail!("`{}` must be at least 1, got {parsed}", setting.key);
            }
            toml_edit::value(parsed)
        }
        SettingKind::Enum(allowed) => {
            let lowered = value.to_ascii_lowercase();
            if !allowed.contains(&lowered.as_str()) {
                anyhow::bail!(
                    "`{}` expects one of {}, got `{value}`",
                    setting.key,
                    allowed.join(", ")
                );
            }
            toml_edit::value(lowered)
        }
        SettingKind::Parsed(parsed) => toml_edit::value(parsed.canonical(value)?),
        SettingKind::List => {
            let mut array = toml_edit::Array::default();
            for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                array.push(entry);
            }
            array.fmt();
            toml_edit::Item::Value(toml_edit::Value::Array(array))
        }
    })
}

/// Which config file a setting is written to.
///
/// Project is the default, matching how `git config` and `npm config` behave:
/// the common case is a setting that belongs to the checkout in front of you,
/// and writing to the user config by default silently edits state shared by
/// every project on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingScope {
    Project,
    Global,
}

/// The file a scope resolves to.
///
/// For the project scope this is the nearest existing `osdk.toml`, or one in
/// the current directory when none exists yet -- the same file `use` writes.
pub fn setting_scope_path(ctx: &Ctx, scope: SettingScope) -> Result<PathBuf> {
    match scope {
        SettingScope::Global => Ok(ctx.dirs.user_config_file()),
        SettingScope::Project => {
            let cwd = std::env::current_dir().context("resolving current directory")?;
            Ok(find_project_config(&cwd).unwrap_or_else(|| cwd.join("osdk.toml")))
        }
    }
}

/// Write one setting into the config for `scope`.
pub fn set_setting(
    ctx: &Ctx,
    setting: &ResolvedSetting,
    value: &str,
    scope: SettingScope,
) -> Result<PathBuf> {
    // Parse first: an invalid value must not leave a half-edited file behind.
    let item = setting_value(setting, value)?;
    let path = setting_scope_path(ctx, scope)?;
    let write = || -> Result<PathBuf> {
        let mut doc = load_doc(&path)?;
        let (last, parents) = setting
            .path
            .split_last()
            .expect("every setting has at least one path segment");

        let mut table = doc.as_table_mut();
        for parent in parents {
            let entry = table
                .entry(parent)
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
            table = entry
                .as_table_mut()
                .with_context(|| format!("`{parent}` is not a table in config"))?;
            // Implicit tables render their header only while they hold keys, so
            // unsetting the last key leaves no empty `[settings]` stanza behind.
            table.set_implicit(true);
        }
        table.insert(last, item);
        save_doc(&path, &doc)?;
        Ok(path.clone())
    };
    match scope {
        // Only the user config is shared between concurrent osdk processes.
        SettingScope::Global => with_global_config_lock(ctx, write),
        SettingScope::Project => write(),
    }
}

/// Remove one setting from the config for `scope`.
///
/// Reports whether the key was present so the caller can distinguish a real
/// unset from a no-op instead of claiming to have changed something.
pub fn unset_setting(
    ctx: &Ctx,
    setting: &ResolvedSetting,
    scope: SettingScope,
) -> Result<Option<PathBuf>> {
    let path = setting_scope_path(ctx, scope)?;
    let remove = || -> Result<Option<PathBuf>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut doc = load_doc(&path)?;
        let (last, parents) = setting
            .path
            .split_last()
            .expect("every setting has at least one path segment");

        let mut table = doc.as_table_mut();
        for parent in parents {
            match table.get_mut(parent).and_then(|item| item.as_table_mut()) {
                Some(child) => table = child,
                // A missing parent means the key was never written.
                None => return Ok(None),
            }
        }
        if table.remove(last).is_none() {
            return Ok(None);
        }
        prune_empty_tables(doc.as_table_mut(), parents);
        save_doc(&path, &doc)?;
        Ok(Some(path.clone()))
    };
    match scope {
        SettingScope::Global => with_global_config_lock(ctx, remove),
        SettingScope::Project => remove(),
    }
}

/// Drop parent tables that the removal just emptied.
///
/// Without this, unsetting the last key under `[settings.shims]` leaves the
/// bare header behind, so a config the user has fully reset still looks edited.
/// Walks outward from the deepest parent, and stops at the first table that
/// still holds something -- an emptied `shims` must not take a populated
/// `settings` with it.
/// Generic over the segment type so both the static `&str` paths and the owned
/// paths built for per-tool keys can be pruned by the same code.
fn prune_empty_tables<S: AsRef<str>>(root: &mut toml_edit::Table, parents: &[S]) {
    for depth in (0..parents.len()).rev() {
        let mut table = &mut *root;
        for parent in &parents[..depth] {
            match table
                .get_mut(parent.as_ref())
                .and_then(|item| item.as_table_mut())
            {
                Some(child) => table = child,
                None => return,
            }
        }
        let name = parents[depth].as_ref();
        let empty = table
            .get(name)
            .and_then(|item| item.as_table())
            .is_some_and(|child| child.is_empty());
        if !empty {
            return;
        }
        table.remove(name);
    }
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
            when: None,
            options: options
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn rejected_values_are_caught_before_any_write() {
        let jobs = resolve_setting("jobs").unwrap();
        assert!(setting_value(&jobs, "0").is_err(), "zero jobs stalls work");
        assert!(setting_value(&jobs, "abc").is_err());
        assert!(setting_value(&jobs, "4").is_ok());

        let offline = resolve_setting("offline").unwrap();
        assert!(setting_value(&offline, "maybe").is_err());
        for truthy in ["true", "1", "yes", "ON"] {
            assert_eq!(
                setting_value(&offline, truthy).unwrap().to_string().trim(),
                "true",
                "{truthy} should parse as true"
            );
        }

        let attestations = resolve_setting("attestations").unwrap();
        assert!(setting_value(&attestations, "sometimes").is_err());
        assert!(
            setting_value(&attestations, "REQUIRED").is_ok(),
            "case-insensitive"
        );
    }

    #[test]
    fn list_settings_split_on_commas_and_drop_blanks() {
        let include = resolve_setting("shims.include").unwrap();
        let item = setting_value(&include, " conda:clang:xmllint , , conda:clang:* ").unwrap();
        let array = item.as_array().expect("list settings store an array");
        let entries: Vec<&str> = array.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(entries, ["conda:clang:xmllint", "conda:clang:*"]);
    }

    #[test]
    fn unsetting_the_last_key_removes_the_empty_table_it_leaves() {
        // A reset config should look untouched, not keep bare `[settings]`
        // headers that suggest something is still configured.
        let mut doc: toml_edit::DocumentMut =
            "[tools]\nrust = \"1.98.0\"\n\n[settings]\njobs = 3\n\n[settings.shims]\ninclude = [\"a\"]\n"
                .parse()
                .unwrap();

        let shims = doc["settings"]["shims"].as_table_mut().unwrap();
        shims.remove("include");
        prune_empty_tables(doc.as_table_mut(), &["settings", "shims"]);
        // `settings` still holds `jobs`, so only `shims` may disappear.
        assert!(!doc.to_string().contains("[settings.shims]"));
        assert!(doc.to_string().contains("jobs = 3"));

        let settings = doc["settings"].as_table_mut().unwrap();
        settings.remove("jobs");
        prune_empty_tables(doc.as_table_mut(), &["settings"]);
        let rendered = doc.to_string();
        assert!(!rendered.contains("[settings]"), "got: {rendered}");
        // Unrelated content must survive the pruning.
        assert!(rendered.contains("rust = \"1.98.0\""), "got: {rendered}");
    }

    #[test]
    fn every_declared_setting_is_readable_by_the_same_key() {
        // `config get` matches on these keys; a typo in either table would make
        // a setting writable but permanently unreadable.
        for setting in SETTINGS {
            assert!(
                find_setting(setting.key).is_some(),
                "{} is not findable",
                setting.key
            );
            assert!(
                !setting.path.is_empty(),
                "{} has no document path",
                setting.key
            );
        }
    }

    #[test]
    fn every_accepted_value_can_be_loaded_back() {
        // The original table hand-copied each enum's variants and got three of
        // them wrong, so `set` accepted a word, wrote it, and every later
        // command failed to parse the file it had just written. Validation that
        // disagrees with the loader is worse than no validation.
        let cases: &[(&str, &str)] = &[
            ("attestations", "off"),
            ("attestations", "if-available"),
            ("attestations", "auto"),
            ("attestations", "required"),
            ("prerelease", "never"),
            ("prerelease", "if-explicit"),
            ("prerelease", "allow"),
            ("link_mode", "auto"),
            ("link_mode", "hardlink"),
            ("link_mode", "reflink"),
            ("link_mode", "copy"),
            ("link_mode", "symlink"),
            ("jobs", "4"),
            ("offline", "true"),
            ("lang", "en"),
            ("shims.include", "conda:clang:xmllint"),
            // Registry lists live outside `[settings]`, and the in-memory
            // field is `#[serde(skip)]` with its own load path, so "the writer
            // and the loader agree" is a real question here rather than a
            // formality.
            ("registries.python.urls", "https://pypi.org/simple/"),
            ("registries.npm.urls", "https://registry.npmjs.org/"),
        ];
        for (key, value) in cases {
            let setting = resolve_setting(key).unwrap();
            let item = setting_value(&setting, value)
                .unwrap_or_else(|error| panic!("`{key} = {value}` was rejected: {error}"));

            // Build the document the writer would produce, then load it the way
            // the CLI does on the next command.
            let mut doc = toml_edit::DocumentMut::new();
            let mut table = doc.as_table_mut();
            let (last, parents) = setting.path.split_last().unwrap();
            for parent in parents {
                let entry = table
                    .entry(parent)
                    .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
                table = entry.as_table_mut().unwrap();
            }
            table.insert(last, item);

            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("config.toml");
            std::fs::write(&path, doc.to_string()).unwrap();
            osdk_core::config::Config::load_user(&path).unwrap_or_else(|error| {
                panic!("`{key} = {value}` was written but does not load: {error}")
            });
        }
    }

    #[test]
    fn a_rejected_value_is_named_with_its_alternatives() {
        for (key, bad) in [
            ("attestations", "preferred"),
            ("prerelease", "deny"),
            ("link_mode", "hardlinkk"),
            ("jobs", "0"),
        ] {
            let setting = resolve_setting(key).unwrap();
            assert!(
                setting_value(&setting, bad).is_err(),
                "`{key} = {bad}` must be rejected before it reaches the file"
            );
        }
    }

    #[test]
    fn project_is_the_default_scope_and_global_is_opt_in() {
        // Writing to the user config by default would edit state shared by
        // every project on the machine, which is not what `set` should mean.
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("osdk.toml");
        std::fs::write(&project, "[tools]\nrust = \"1.98.0\"\n").unwrap();

        // A project config that already exists is the one that gets edited.
        assert_eq!(
            find_project_config(temporary.path()).as_deref(),
            Some(project.as_path())
        );
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
            "[tools]\nnode = \"22\"\n\"npm:prettier\" = { version = \"3\", installer = \"pnpm\" }\n\n[aliases.node]\nlts = \"22\"\n",
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
