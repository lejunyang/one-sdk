use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PROJECT_CONFIG_NAMES;
use crate::error::{Error, Result};
use crate::lock::FileLock;

const TRUST_FILE_NAME: &str = "trusted-configs.toml";
const TRUST_LOCK_FILE_NAME: &str = "trusted-configs.lock";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrustStore {
    #[serde(default = "schema")]
    schema: u32,
    #[serde(default)]
    configs: Vec<TrustRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustRecord {
    pub path: PathBuf,
    pub hash: String,
}

fn schema() -> u32 {
    1
}

pub fn project_config(start: &Path) -> Result<Option<PathBuf>> {
    let start = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    for directory in start.ancestors() {
        for name in PROJECT_CONFIG_NAMES {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

pub fn resolve_config(path: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    let candidate = path.unwrap_or(cwd);
    if candidate.is_file() {
        return canonical_file(candidate);
    }
    let Some(config) = project_config(candidate)? else {
        return Err(Error::config(format!(
            "no osdk project config found from {}",
            candidate.display()
        )));
    };
    canonical_file(&config)
}

/// One reason a project config needs review, naming the exact key.
///
/// Carrying the key rather than a bare `true` is what lets the refusal say
/// *what* to look at. A person told only "this config is untrusted" has to
/// diff it against nothing; a person told that `settings.verify_signatures`
/// disables signature verification can decide in one glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRequirement {
    /// Dotted path to the offending key, e.g. `settings.verify_signatures`.
    pub key: String,
    /// Why this key is execution-affecting.
    pub reason: TrustReason,
}

/// The capability a key grants. These are the only two things trust gates.
///
/// Declaring *which package* to install is deliberately not here: npm installs
/// pass `--ignore-scripts`, `http:` artifacts require a pinned sha256, and
/// `go:` builds run with `CGO_ENABLED=0`, so a dependency declaration on its
/// own executes nothing the tool's publisher did not already ship. Treating it
/// as dangerous made every added package demand re-approval while teaching
/// nothing -- the gate cried wolf, which is how a real warning gets ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustReason {
    /// Runs arbitrary code on this machine during install.
    ExecutesCode,
    /// Silently changes *how* a command the user asked for gets executed.
    ///
    /// Distinct from [`Self::ExecutesCode`], which is about code running at a
    /// moment the user did not choose. This one is about a command the user
    /// *did* choose being routed somewhere else: `task_config.shell` picks the
    /// interpreter for every task, so the text of a task stops determining what
    /// actually runs. Reusing the install-time wording here would have stated
    /// something plainly untrue -- tasks never run during install.
    RedirectsExecution,
    /// Weakens verification of what is installed, or redirects where it comes
    /// from. Dangerous in combination: an unverified mirror is both at once.
    WeakensVerification,
}

impl TrustReason {
    /// The localized explanation shown next to the key.
    pub fn describe(self) -> String {
        match self {
            Self::ExecutesCode => crate::t!("trust.reason.executes_code"),
            Self::RedirectsExecution => crate::t!("trust.reason.redirects_execution"),
            Self::WeakensVerification => crate::t!("trust.reason.weakens_verification"),
        }
    }
}

/// `[settings]` keys that can neither execute code nor weaken verification.
///
/// This is an allowlist, and an unlisted key defaults to requiring trust. A
/// newly added setting is therefore fail-closed: forgetting to classify it
/// makes configs ask for review needlessly (visible, mildly annoying, safe)
/// rather than letting a dangerous key through silently.
/// `settings_allowlist_covers_every_field` fails when a field is added without
/// a decision being recorded on either list.
///
/// `offline` is safe in the only direction it can move: it forbids network
/// access, never grants it.
///
/// `node` and `npm` are here after checking what they can actually express.
/// `node` holds a single bool, `corepack`, and `npm` a single two-way choice
/// between the npm and pnpm that osdk itself manages -- and that choice is only
/// the lowest-priority fallback, so it cannot override a project that declares
/// its own installer. See `corepack_and_installer_choice_are_safe` for why the
/// obvious reading of "runs corepack" does not make this a gate.
const SAFE_SETTINGS_KEYS: &[&str] = &[
    "jobs",
    "lang",
    "link_mode",
    "node",
    "npm",
    "offline",
    "prerelease",
    "shims",
    "yes",
];

/// `[settings]` keys that require trust, each with the reason it does.
///
/// Kept explicit rather than derived from "absent from the allowlist" so the
/// refusal can explain itself, and so the coverage test can tell a deliberate
/// classification apart from an omission.
const TRUST_REQUIRING_SETTINGS: &[(&str, TrustReason)] = &[
    ("verify_signatures", TrustReason::WeakensVerification),
    ("require_checksums", TrustReason::WeakensVerification),
    ("attestations", TrustReason::WeakensVerification),
    // Both carry `catalog_url`, a free-form URL that decides which interpreter
    // or runtime bytes get installed, and `python` also carries the
    // `catalog_sha256` that would otherwise pin them.
    ("python", TrustReason::WeakensVerification),
    ("java", TrustReason::WeakensVerification),
];

/// Top-level tables that require trust as a whole, with the reason.
///
/// `syspkg` installs into the machine outside the managed root, may prompt for
/// elevation, and is deliberately not covered by `osdk.lock`. `sources` and
/// `registries` change where subprocesses fetch from.
/// `tasks` is deliberately **absent**. Nothing in osdk ever runs a task on its
/// own: there is no postinstall, no lifecycle hook, no automatic invocation --
/// `[tasks]` is read by `osdk run` and `osdk task` and nowhere else. Typing
/// `osdk run build` *is* the authorization, so demanding a trust record first
/// asks the same question twice. That is exactly the wolf-crying this module
/// warns about above: a gate that fires on something the user just asked for
/// teaches nothing and trains people to approve without reading.
///
/// The contrast with `syspkg` is the whole point -- but state it accurately:
/// `syspkg` is read only by the `osdk pkg` subcommands, and only
/// `osdk pkg apply --yes` installs anything. What makes it different from
/// `[tasks]` is not *when* it is read but *what one approval covers*: `osdk run
/// build` names a single task whose text is right there, whereas `osdk pkg
/// apply` accepts the whole package list at once, each entry able to run a
/// distribution's install scripts as root. Review before the fact is what makes
/// that list reviewable at all.
///
/// Note this table says nothing about which *commands* must enforce the gate --
/// see [`affects_tool_dispatch`]. `syspkg` requires review before installing,
/// not before every `cargo --version`.
///
/// `task_config` is different again, and does stay gated: it is not a command
/// the user names but an ambient setting, and its `shell` field decides which
/// interpreter *every* task in scope runs under. A config that quietly sets
/// `shell = "evil --run"` turns every later `osdk run` into something other
/// than what the task text says, with nothing at the call site to reveal it.
const TRUST_REQUIRING_TABLES: &[(&str, TrustReason)] = &[
    ("syspkg", TrustReason::ExecutesCode),
    ("sources", TrustReason::WeakensVerification),
    ("registries", TrustReason::WeakensVerification),
    ("task_config", TrustReason::RedirectsExecution),
];

/// Top-level tables inspected key by key instead of judged as a whole.
///
/// `tools` needs this because a single tool option (`allow_builds`) can still
/// opt into script execution even though the surrounding table is safe.
const INSPECTED_TABLES: &[&str] = &[
    "tools", "aliases", "settings", "tasks", "models", "deps", "skills",
];

/// Keys under one `[models.<name>]` entry that decide where model bytes come
/// from (or weaken how they are checked). Everything else is a harmless
/// declaration, like declaring `npm:prettier` (research §6.4).
const MODEL_SOURCE_KEYS: &[&str] = &["endpoint", "insecure", "url", "mirror"];
/// Keys under one `[skills.<name>]` entry that change where the skill's bytes
/// come from. A bare `source = "github:owner/repo"` declaration is not one of
/// them: like declaring `npm:prettier`, saying *which* skill to install is not a
/// gate. Only a custom `endpoint` (or an insecure/mirror override) is.
const SKILL_SOURCE_KEYS: &[&str] = &["endpoint", "insecure", "mirror"];

/// Keys under one `[deps.<provider>]` entry that redirect where bytes come from.
///
/// Note the deliberate narrowness. `UV_INDEX_`-style prefix matching taught this
/// project that matching too widely is as harmful as matching too narrowly, and
/// harder to notice: a key like `index_strategy` is not a source override, and
/// treating it as one would demand approval for a harmless setting. These are
/// exact names.
const DEPS_SOURCE_KEYS: &[&str] = &["index", "extra_index", "registry", "insecure"];

/// Keys that opt into running build or lifecycle scripts, i.e. executing code
/// the user did not otherwise ask to run. Same class as `tools.<name>.allow_builds`.
const DEPS_BUILD_KEYS: &[&str] = &["allow_build_from_source"];

/// Does this `[deps.<provider>.env]` variable *name* redirect where packages are
/// fetched from?
///
/// This check exists because omitting it left a hole: `index` was gated while
/// `env = { NPM_CONFIG_REGISTRY = "https://…" }` achieved exactly the same
/// redirect with no approval at all. A gate that one spelling walks around is
/// not a gate.
///
/// The matching is by suffix rather than prefix, and that direction is
/// deliberate. `UV_INDEX_` taught this project that a prefix sweeps in unrelated
/// settings (`index_strategy`, `NPM_CONFIG_FUND`) and the damage is invisible --
/// a needless prompt, or a feature quietly switched off. A suffix like
/// `_REGISTRY` names the thing itself. Both directions are asserted in the
/// tests: names that redirect, and names that merely sound like they do.
fn env_name_redirects_source(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    // Scoped registries: `NPM_CONFIG_@SCOPE:REGISTRY`.
    if upper.contains(":REGISTRY") {
        return true;
    }
    upper.ends_with("_REGISTRY")
        || upper.ends_with("REGISTRY_SERVER")
        || upper.ends_with("_INDEX_URL")
        || upper.ends_with("_DEFAULT_INDEX")
}

/// The npm tool option that turns lifecycle scripts back on.
const ALLOW_BUILDS_OPTION: &str = "allow_builds";

/// Does this requirement affect how `osdk-shim` dispatches an already-installed
/// tool?
///
/// The shim runs on **every command invocation**, so whatever it refuses makes
/// that command unusable in the directory. It must therefore gate only what it
/// can itself act on, and nothing else.
///
/// It can act on `sources` and `registries`: the shim performs a registry
/// preflight before running a package manager, so those genuinely decide where
/// a subprocess it starts will fetch from. It cannot act on `syspkg` (read only
/// by `osdk pkg`, and installing needs `osdk pkg apply --yes`) or on
/// `task_config` (read only by `osdk run` / `osdk task`).
///
/// Gating those two here bought no safety and cost a great deal: adding a
/// `[syspkg]` block to a project made `cargo --version` fail in that directory
/// with "project config is not trusted" -- a refusal about installing system
/// packages, raised by a command that installs nothing. And because trust is
/// bound to the file's hash, every later edit of `osdk.toml` re-locked every
/// tool again. That is the wolf-crying this module warns about, in the one place
/// where it also breaks the build.
///
/// `osdk install`, `osdk pkg` and `osdk run` still evaluate the full set: the
/// narrowing is the shim's alone, and each of those paths reaches keys the shim
/// never does.
pub fn affects_tool_dispatch(requirement: &TrustRequirement) -> bool {
    // Match on the top-level table: a requirement key is either a bare table
    // name or `table.key`.
    let table = requirement
        .key
        .split_once('.')
        .map_or(requirement.key.as_str(), |(table, _)| table);
    match table {
        // Never reached by the shim.
        "syspkg" | "task_config" | "models" | "deps" | "skills" => false,
        // Everything else is treated as dispatch-affecting. Fail-closed on
        // purpose: `settings`, `tools`, `sources`, `registries` and any table a
        // future build does not recognize all stay gated, so adding a new
        // execution-affecting key cannot silently escape the shim's check by
        // being forgotten here.
        _ => true,
    }
}

/// Collect every key in this config that requires review, in reporting order.
///
/// An empty result means the config is safe to load with no trust record.
pub fn trust_requirements(path: &Path) -> Result<Vec<TrustRequirement>> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    Ok(collect_requirements(&value))
}

/// Render the requirements as indented `key -- reason` lines for a message.
pub fn describe_requirements(requirements: &[TrustRequirement]) -> String {
    requirements
        .iter()
        .map(|requirement| {
            format!(
                "\n  {} -- {}",
                requirement.key,
                requirement.reason.describe()
            )
        })
        .collect()
}

fn collect_requirements(value: &toml::Value) -> Vec<TrustRequirement> {
    let Some(table) = value.as_table() else {
        return Vec::new();
    };
    let mut found = Vec::new();

    for (key, value) in table {
        match key.as_str() {
            "settings" => collect_settings_requirements(value, &mut found),
            "tools" => collect_tools_requirements(value, &mut found),
            "models" => collect_models_requirements(value, &mut found),
            "deps" => collect_deps_requirements(value, &mut found),
            "skills" => collect_skills_requirements(value, &mut found),
            "aliases" => {}
            // Declaring a task is not running one; see TRUST_REQUIRING_TABLES.
            "tasks" => {}
            other => {
                debug_assert!(!INSPECTED_TABLES.contains(&other));
                let reason = TRUST_REQUIRING_TABLES
                    .iter()
                    .find(|(name, _)| *name == other)
                    .map(|(_, reason)| *reason)
                    // An unrecognized top-level table is fail-closed: a table
                    // this build cannot interpret cannot be shown harmless.
                    .unwrap_or(TrustReason::ExecutesCode);
                found.push(TrustRequirement {
                    key: other.to_string(),
                    reason,
                });
            }
        }
    }
    found
}

fn collect_settings_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(settings) = value.as_table() else {
        // A `settings` key that is not a table is malformed, not safe.
        found.push(TrustRequirement {
            key: "settings".into(),
            reason: TrustReason::WeakensVerification,
        });
        return;
    };
    for key in settings.keys() {
        if SAFE_SETTINGS_KEYS.contains(&key.as_str()) {
            continue;
        }
        let reason = TRUST_REQUIRING_SETTINGS
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, reason)| *reason)
            // Unlisted means unclassified, which is treated as dangerous.
            .unwrap_or(TrustReason::WeakensVerification);
        found.push(TrustRequirement {
            key: format!("settings.{key}"),
            reason,
        });
    }
}

/// Inspect `[tools]` for the one option that grants code execution.
///
/// Declaring `npm:prettier` or `github:cli/cli` is not itself a reason: those
/// installs execute no package-authored code. `allow_builds` is, and it can
/// arrive either as a table field or inline in the request string.
fn collect_tools_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(tools) = value.as_table() else {
        return;
    };
    let inline_allows_builds = |spec: &str| {
        crate::version::ToolRequest::parse(spec).is_ok_and(|request| {
            request
                .options
                .get(ALLOW_BUILDS_OPTION)
                .is_some_and(|value| allow_builds_enabled(value))
        })
    };
    for (name, entry) in tools {
        let allows_builds = match entry {
            toml::Value::String(spec) => inline_allows_builds(spec),
            toml::Value::Table(fields) => {
                let declared = fields.get(ALLOW_BUILDS_OPTION).is_some_and(|value| {
                    value
                        .as_str()
                        .map(allow_builds_enabled)
                        // `allow_builds = true` is the natural TOML spelling.
                        .or_else(|| value.as_bool())
                        .unwrap_or(false)
                });
                declared
                    || fields
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .is_some_and(inline_allows_builds)
            }
            _ => false,
        };
        if allows_builds {
            found.push(TrustRequirement {
                key: format!("tools.{name}.{ALLOW_BUILDS_OPTION}"),
                reason: TrustReason::ExecutesCode,
            });
        }
    }
}

/// Inspect `[models]` entries for keys that redirect or weaken the byte source.
///
/// Declaring what to fetch (`source`, `include`, `exclude`, `variant`, `when`)
/// and which consumer views to render (`views`) is never a reason to ask for
/// trust -- like declaring an npm dependency, it runs nothing. An explicit
/// `endpoint`/custom URL/`insecure` flag is, and is reported per model so the
/// message names the offending entry.
fn collect_models_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(models) = value.as_table() else {
        // A malformed `models` table cannot be shown harmless.
        found.push(TrustRequirement {
            key: "models".into(),
            reason: TrustReason::WeakensVerification,
        });
        return;
    };
    for (name, entry) in models {
        let Some(fields) = entry.as_table() else {
            continue;
        };
        for source_key in fields
            .keys()
            .filter(|k| MODEL_SOURCE_KEYS.contains(&k.as_str()) || k.as_str().contains("endpoint"))
        {
            found.push(TrustRequirement {
                key: format!("models.{name}.{source_key}"),
                reason: TrustReason::WeakensVerification,
            });
        }
    }
}

/// Inspect `[skills]` entries for keys that redirect the byte source.
///
/// The `[skills]` table flattens two kinds of key: top-level defaults
/// (`default_agents`, `scope`, `link_mode`), which are scalars/arrays and never
/// require trust, and named `[skills.<name>]` sub-tables. A bare
/// `source = "github:owner/repo"` is a harmless declaration, exactly like
/// declaring `npm:prettier`; only a custom `endpoint` (or an insecure/mirror
/// override) changes where the bytes come from and is reported per skill.
fn collect_skills_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(skills) = value.as_table() else {
        // A malformed `skills` table cannot be shown harmless.
        found.push(TrustRequirement {
            key: "skills".into(),
            reason: TrustReason::WeakensVerification,
        });
        return;
    };
    for (name, entry) in skills {
        // Only named sub-tables are skills; a scalar/array is a top-level
        // default (default_agents/scope/link_mode) and needs no trust.
        let Some(fields) = entry.as_table() else {
            continue;
        };
        for source_key in fields
            .keys()
            .filter(|k| SKILL_SOURCE_KEYS.contains(&k.as_str()) || k.as_str().contains("endpoint"))
        {
            found.push(TrustRequirement {
                key: format!("skills.{name}.{source_key}"),
                reason: TrustReason::WeakensVerification,
            });
        }
    }
}

/// Inspect `[deps]` for keys that redirect the byte source or enable builds.
///
/// The default is that a provider entry needs **no** trust. That is not leniency
/// but consistency: `TrustReason`'s own documentation says declaring which
/// package to install is deliberately not a gate, because installs pass
/// `--ignore-scripts` and artifacts are pinned. The deps layer keeps that
/// promise -- it passes `--ignore-scripts` (or yarn berry's
/// `YARN_ENABLE_SCRIPTS=false`) by default -- so enabling `[deps.pnpm]` executes
/// nothing the package publisher did not already ship as plain files.
///
/// Two things do change it, and they are reported per entry so the message names
/// the provider:
/// - a custom `index`/`registry` redirects where bytes come from
///   (`WeakensVerification`, same class as `sources`);
/// - `allow_build_from_source` runs build and lifecycle scripts on this machine
///   (`ExecutesCode`, same class as `tools.<name>.allow_builds`).
///
/// A custom provider is different again: its `run` *is* an arbitrary command,
/// and unlike a task it can be triggered ahead of `osdk run` by `auto`, so the
/// user does not point at it each time. Those always require trust.
fn collect_deps_requirements(value: &toml::Value, found: &mut Vec<TrustRequirement>) {
    let Some(table) = value.as_table() else {
        // A malformed `deps` table cannot be shown harmless.
        found.push(TrustRequirement {
            key: "deps".into(),
            reason: TrustReason::WeakensVerification,
        });
        return;
    };
    for (name, entry) in table {
        // `disable` is a list of provider names; turning a provider off cannot
        // add capability.
        if name == "disable" {
            continue;
        }
        let Some(fields) = entry.as_table() else {
            continue;
        };
        // A provider that is not compiled in is a custom one: `run` is an
        // arbitrary command.
        if fields.contains_key("run") {
            found.push(TrustRequirement {
                key: format!("deps.{name}.run"),
                reason: TrustReason::ExecutesCode,
            });
        }
        // An env table can redirect the source just as effectively as `index`,
        // so it is inspected by variable name rather than trusted wholesale.
        if let Some(env) = fields.get("env").and_then(toml::Value::as_table) {
            for variable in env.keys() {
                if env_name_redirects_source(variable) {
                    found.push(TrustRequirement {
                        key: format!("deps.{name}.env.{variable}", variable = variable),
                        reason: TrustReason::WeakensVerification,
                    });
                }
            }
        }
        for key in fields.keys() {
            if DEPS_SOURCE_KEYS.contains(&key.as_str()) {
                found.push(TrustRequirement {
                    key: format!("deps.{name}.{key}"),
                    reason: TrustReason::WeakensVerification,
                });
            } else if DEPS_BUILD_KEYS.contains(&key.as_str())
                && build_from_source_enabled(&fields[key])
            {
                found.push(TrustRequirement {
                    key: format!("deps.{name}.{key}"),
                    reason: TrustReason::ExecutesCode,
                });
            }
        }
    }
}

/// `allow_build_from_source = false` leaves scripts denied, so it is not a
/// reason. Mirrors `allow_builds_enabled`: writing the key to turn the thing
/// *off* must not demand approval.
fn build_from_source_enabled(value: &toml::Value) -> bool {
    match value {
        toml::Value::Boolean(flag) => *flag,
        toml::Value::String(raw) => allow_builds_enabled(raw),
        _ => false,
    }
}

/// Whether an `allow_builds` value actually asks for scripts to run.
///
/// Mirrors the npm backend's own parsing. A package list still ends up denied
/// there (npm has no per-package allowlist), but it states the intent to build,
/// so it is reported rather than silently treated as harmless.
fn allow_builds_enabled(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    !matches!(
        raw.to_ascii_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

/// Hash only the keys that trust actually governs.
///
/// Hashing the whole file made trust much stricter than its own gate: once a
/// config contained a single trust-requiring key, *every* later edit
/// invalidated the record, so bumping a version in `[tools]` -- a change that
/// needs no trust on its own -- demanded re-approval. Both gates now read the
/// same subset, so a record survives exactly the edits that were never gated.
pub fn normalized_hash(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let value: toml::Value = toml::from_str(&text)?;
    let governed = governed_subset(&value);
    let normalized = toml::to_string(&governed)
        .map_err(|error| Error::config(format!("normalizing {}: {error}", path.display())))?;
    Ok(blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

/// Project the config down to the keys `trust_requirements` reports on.
///
/// Driven by the same requirement list as the gate, so the two cannot drift: a
/// key that is not a reason to ask for trust is also not a reason to
/// invalidate it.
fn governed_subset(value: &toml::Value) -> toml::Value {
    let mut subset = toml::value::Table::new();
    let Some(table) = value.as_table() else {
        return toml::Value::Table(subset);
    };
    for requirement in collect_requirements(value) {
        let mut segments = requirement.key.split('.');
        let Some(head) = segments.next() else {
            continue;
        };
        let Some(head_value) = table.get(head) else {
            continue;
        };
        match segments.next() {
            // A whole-table reason (`syspkg`, `sources`, an unknown table)
            // pins that table's entire content.
            None => {
                subset.insert(head.to_string(), head_value.clone());
            }
            // A key-level reason pins just that key, leaving unrelated
            // siblings editable.
            Some(field) => {
                let Some(field_value) = head_value.as_table().and_then(|table| table.get(field))
                else {
                    continue;
                };
                let nested = subset
                    .entry(head.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
                if let Some(nested) = nested.as_table_mut() {
                    // For `tools.<name>.allow_builds` this records the whole
                    // entry: the package identity is what approval covered.
                    nested.insert(field.to_string(), field_value.clone());
                }
            }
        }
    }
    toml::Value::Table(subset)
}

pub fn requires_trust(path: &Path) -> Result<bool> {
    Ok(!trust_requirements(path)?.is_empty())
}

pub fn is_trusted(
    config_dir: &Path,
    path: &Path,
    trusted_paths: Option<&OsString>,
) -> Result<bool> {
    let canonical = canonical_file(path)?;
    if trusted_paths
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter_map(|entry| canonical_existing(&entry).ok())
        .any(|entry| canonical == entry || canonical.starts_with(&entry))
    {
        return Ok(true);
    }

    let hash = normalized_hash(&canonical)?;
    Ok(read_store(config_dir)?
        .configs
        .iter()
        .any(|record| record.path == canonical && record.hash == hash))
}

pub fn trust(config_dir: &Path, path: &Path) -> Result<TrustRecord> {
    let path = canonical_file(path)?;
    let record = TrustRecord {
        hash: normalized_hash(&path)?,
        path,
    };
    update_store(config_dir, |store| {
        store
            .configs
            .retain(|existing| existing.path != record.path);
        store.configs.push(record.clone());
        store
            .configs
            .sort_by(|left, right| left.path.cmp(&right.path));
        (record, true)
    })
}

pub fn untrust(config_dir: &Path, path: &Path) -> Result<bool> {
    let path = canonical_file(path)?;
    update_store(config_dir, |store| {
        let original = store.configs.len();
        store.configs.retain(|record| record.path != path);
        let removed = store.configs.len() != original;
        (removed, removed)
    })
}

pub fn list(config_dir: &Path) -> Result<Vec<TrustRecord>> {
    Ok(read_store(config_dir)?.configs)
}

/// What a stored trust record is currently worth.
///
/// `list` and `prune` must agree on this, so it is derived once here rather than
/// re-implemented per command: a `prune` that classified records differently from
/// what `list` showed would delete something the user had just been told was
/// still needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState {
    /// The file is present and its governed keys still hash to the record.
    Active,
    /// The file is present but its governed keys changed. The project is still
    /// there; it needs reviewing and trusting again. **Never prunable** -- this is
    /// a live project mid-edit, and dropping the record would silently turn into
    /// "untrusted" with nothing explaining why.
    Changed,
    /// The path no longer resolves to a file, while its parent directory does
    /// exist. The record cannot apply again unless the file comes back.
    Missing,
    /// Neither the file nor its parent directory resolves.
    ///
    /// Kept apart from `Missing` because on Windows this is what an unmounted
    /// drive looks like -- a USB disk, a network share, a WSL mount. Treating it
    /// as prunable garbage would delete valid approvals whenever a volume happened
    /// to be detached, and the user would only discover it later as an
    /// unexplained "untrusted".
    Unreachable,
}

impl RecordState {
    /// Whether dropping this record loses nothing.
    ///
    /// Only `Missing` qualifies. In particular `Changed` does not: the judgement
    /// has to be "this record can never apply again", not "this record does not
    /// apply right now".
    pub fn is_prunable(self) -> bool {
        matches!(self, RecordState::Missing)
    }

    /// Catalog key for the label shown to the user.
    pub fn label_key(self) -> &'static str {
        match self {
            RecordState::Active => "label.trust.active",
            RecordState::Changed => "label.trust.changed",
            RecordState::Missing => "label.trust.missing",
            RecordState::Unreachable => "label.trust.unreachable",
        }
    }
}

/// Classify one stored record against the filesystem.
pub fn record_state(config_dir: &Path, record: &TrustRecord) -> RecordState {
    if record.path.is_file() {
        return match is_trusted(config_dir, &record.path, None) {
            Ok(true) => RecordState::Active,
            // A hash that cannot be computed is reported as changed rather than
            // as missing: the file is right there, so this is not a record to
            // throw away.
            Ok(false) | Err(_) => RecordState::Changed,
        };
    }
    match record.path.parent() {
        // The parent is readable and the file is genuinely gone.
        Some(parent) if parent.is_dir() => RecordState::Missing,
        _ => RecordState::Unreachable,
    }
}

/// Every stored record with its current state.
pub fn list_with_state(config_dir: &Path) -> Result<Vec<(TrustRecord, RecordState)>> {
    Ok(list(config_dir)?
        .into_iter()
        .map(|record| {
            let state = record_state(config_dir, &record);
            (record, state)
        })
        .collect())
}

/// Drop every record whose file can never apply again, returning what was removed.
///
/// Deliberately keyed on [`RecordState::is_prunable`] rather than on
/// `is_trusted`: the latter is also false for a config that is merely mid-edit,
/// so pruning by it would revoke approvals for projects still in use.
pub fn prune(config_dir: &Path) -> Result<Vec<TrustRecord>> {
    let _lock = FileLock::acquire(store_lock_path(config_dir))?;
    let mut store = read_store(config_dir)?;
    let mut removed = Vec::new();
    store.configs.retain(|record| {
        if record_state(config_dir, record).is_prunable() {
            removed.push(record.clone());
            false
        } else {
            true
        }
    });
    if !removed.is_empty() {
        write_store(config_dir, &store)?;
    }
    Ok(removed)
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    dunce::canonicalize(path).map_err(|error| Error::io(path, error))
}

fn canonical_file(path: &Path) -> Result<PathBuf> {
    let canonical = canonical_existing(path)?;
    if !canonical.is_file() {
        return Err(Error::config(format!(
            "trusted config path is not a file: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TRUST_FILE_NAME)
}

fn store_lock_path(config_dir: &Path) -> PathBuf {
    config_dir.join(TRUST_LOCK_FILE_NAME)
}

fn update_store<T>(
    config_dir: &Path,
    update: impl FnOnce(&mut TrustStore) -> (T, bool),
) -> Result<T> {
    let _lock = FileLock::acquire(store_lock_path(config_dir))?;
    let mut store = read_store(config_dir)?;
    let (result, changed) = update(&mut store);
    if changed {
        write_store(config_dir, &store)?;
    }
    Ok(result)
}

fn read_store(config_dir: &Path) -> Result<TrustStore> {
    let path = store_path(config_dir);
    if !path.is_file() {
        return Ok(TrustStore {
            schema: schema(),
            configs: Vec::new(),
        });
    }
    let text = std::fs::read_to_string(&path).map_err(|error| Error::io(&path, error))?;
    let store: TrustStore = toml::from_str(&text)?;
    if store.schema != schema() {
        return Err(Error::config(format!(
            "unsupported trust store schema {}",
            store.schema
        )));
    }
    Ok(store)
}

fn write_store(config_dir: &Path, store: &TrustStore) -> Result<()> {
    std::fs::create_dir_all(config_dir).map_err(|error| Error::io(config_dir, error))?;
    let path = store_path(config_dir);
    let text = toml::to_string_pretty(store)
        .map_err(|error| Error::config(format!("serializing trust store: {error}")))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".trusted-configs.")
        .suffix(".tmp")
        .tempfile_in(config_dir)
        .map_err(|error| Error::io(config_dir, error))?;
    let temporary_path = temporary.path().to_path_buf();
    temporary
        .write_all(text.as_bytes())
        .map_err(|error| Error::io(&temporary_path, error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| Error::io(&temporary_path, error))?;
    let (temporary_file, temporary_path) = temporary
        .keep()
        .map_err(|error| Error::io(&temporary_path, error.error))?;
    drop(temporary_file);
    if let Err(error) = crate::fs::atomic_replace(&temporary_path, &path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(Error::io(&path, error));
    }
    crate::fs::sync_parent(config_dir).map_err(|error| Error::io(config_dir, error))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    use super::*;

    #[test]
    fn normalized_content_and_canonical_path_define_identity() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");
        std::fs::write(&config, "[sources]\nselection = \"ordered\"\n").unwrap();

        let traversal = repo.join("nested/../osdk.toml");
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        let record = trust(&config_dir, &traversal).unwrap();
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        assert_eq!(record.path, dunce::canonicalize(&config).unwrap());

        std::fs::write(&config, "[sources]\nselection = \"auto\"\n").unwrap();
        assert!(!is_trusted(&config_dir, &config, None).unwrap());
    }

    #[test]
    fn repeated_updates_replace_store_without_using_fixed_temporary_path() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = temp.path().join("osdk.toml");
        let legacy_temporary = config_dir.join("trusted-configs.toml.tmp");
        std::fs::write(&legacy_temporary, "do not overwrite").unwrap();

        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();
        let original = trust(&config_dir, &config).unwrap();
        std::fs::write(&config, "[settings]\nvalue = 2\n").unwrap();
        let updated = trust(&config_dir, &config).unwrap();

        assert_ne!(original.hash, updated.hash);
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        assert_eq!(
            std::fs::read_to_string(&legacy_temporary).unwrap(),
            "do not overwrite"
        );
        assert!(untrust(&config_dir, &config).unwrap());
        assert!(list(&config_dir).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_replaces_an_existing_store() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let config = temp.path().join("osdk.toml");

        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();
        trust(&config_dir, &config).unwrap();
        std::fs::write(&config, "[settings]\nvalue = 2\n").unwrap();
        trust(&config_dir, &config).unwrap();

        assert_eq!(list(&config_dir).unwrap().len(), 1);
        assert!(is_trusted(&config_dir, &config, None).unwrap());
    }

    #[test]
    fn concurrent_trust_updates_preserve_every_record() {
        const WRITERS: usize = 16;

        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let mut configs = Vec::with_capacity(WRITERS);
        for index in 0..WRITERS {
            let config = temp.path().join(format!("osdk-{index}.toml"));
            std::fs::write(&config, format!("[settings]\nvalue = {index}\n")).unwrap();
            configs.push(config);
        }

        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = configs
            .iter()
            .cloned()
            .map(|config| {
                let barrier = Arc::clone(&barrier);
                let config_dir = config_dir.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    trust(&config_dir, &config).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut expected: Vec<_> = configs
            .iter()
            .map(|config| dunce::canonicalize(config).unwrap())
            .collect();
        expected.sort();
        let actual: Vec<_> = list(&config_dir)
            .unwrap()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn trust_and_untrust_wait_for_the_store_lock() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let config = temp.path().join("osdk.toml");
        std::fs::write(&config, "[settings]\nvalue = 1\n").unwrap();

        let lock = FileLock::acquire(store_lock_path(&config_dir)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let trust_config_dir = config_dir.clone();
        let trust_config = config.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(trust(&trust_config_dir, &trust_config).map(|_| ()))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(250)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lock);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        handle.join().unwrap();

        let lock = FileLock::acquire(store_lock_path(&config_dir)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let untrust_config_dir = config_dir.clone();
        let untrust_config = config.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(untrust(&untrust_config_dir, &untrust_config))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(250)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lock);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap());
        handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolves_to_target_but_repository_move_invalidates_trust() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");
        std::fs::write(&config, "[settings]\nyes = true\n").unwrap();
        trust(&config_dir, &config).unwrap();

        let link = temp.path().join("linked.toml");
        symlink(&config, &link).unwrap();
        assert!(is_trusted(&config_dir, &link, None).unwrap());

        let moved = temp.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        assert!(!is_trusted(&config_dir, &moved.join("osdk.toml"), None).unwrap());
    }

    /// The whole point of the revision: declaring a dependency is not a reason
    /// to ask for approval. Every namespace here installs without running
    /// package-authored code, so a project may add and bump tools freely.
    #[test]
    fn declaring_tools_and_aliases_never_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n[aliases.node]\ndefault = \"20\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());

        for tool in [
            "npm:prettier",
            "github:cli/cli",
            "go:github.com/user/cmd/tool",
            "cargo:ripgrep",
            "pypi:ruff",
            "conda:nasm",
            "http:https://downloads.example.test/tool-{version}",
        ] {
            std::fs::write(&path, format!("[tools]\n{tool:?} = \"1.2.3\"\n")).unwrap();
            assert!(!requires_trust(&path).unwrap(), "{tool}");
        }

        // The table form, including a chosen installer, is equally harmless.
        std::fs::write(
            &path,
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"pnpm\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
    }

    /// `allow_builds` is the one thing inside `[tools]` that grants execution,
    /// in either spelling, and it must be reported against its own key.
    #[test]
    fn allow_builds_requires_trust_in_every_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for body in [
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = true\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"yes\"\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"esbuild\"\n",
            "[tools]\nbundler = \"npm:esbuild[allow_builds=true]@0.21\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{body}");
            assert!(found[0].key.ends_with(".allow_builds"), "{body}");
            assert_eq!(found[0].reason, TrustReason::ExecutesCode, "{body}");
        }

        // A false-ish value leaves scripts denied, so it is not a reason.
        for body in [
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = false\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"off\"\n",
            "[tools.\"npm:esbuild\"]\nversion = \"0.21\"\nallow_builds = \"\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(!requires_trust(&path).unwrap(), "{body}");
        }
    }

    /// `settings.node` and `settings.npm` do not require trust.
    ///
    /// Both look alarming and are not. `node` holds one bool, `corepack`, which
    /// runs the corepack shipped inside that very Node install -- refusing when
    /// absent rather than fetching it -- and `enable` only writes shims into the
    /// install directory. Turning it off changes which shims exist, not which
    /// bytes are on the machine. Corepack does download a package manager later,
    /// but that is triggered at run time by `packageManager` in `package.json`,
    /// which trust has never governed; gating the bool would not prevent it.
    ///
    /// `npm` holds one choice between the npm and pnpm osdk already manages, and
    /// only as the lowest-priority fallback.
    ///
    /// A gate that cannot stop the thing it names is worse than no gate: it
    /// teaches people to accept trust prompts without reading them, so the one
    /// that really matters gets waved through too.
    #[test]
    fn corepack_and_installer_choice_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for body in [
            "[settings.node]\ncorepack = true\n",
            "[settings.npm]\ndefault_installer = \"pnpm\"\n",
            "[settings]\njobs = 4\n[settings.node]\ncorepack = true\n[settings.npm]\ndefault_installer = \"npm\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(
                !requires_trust(&path).unwrap(),
                "should need no trust: {body}"
            );
        }

        // Editing them must not invalidate an existing record either.
        let config_dir = temp.path().join("state");
        std::fs::write(
            &path,
            "[settings]\nverify_signatures = false\n[settings.node]\ncorepack = false\n",
        )
        .unwrap();
        trust(&config_dir, &path).unwrap();
        std::fs::write(
            &path,
            "[settings]\nverify_signatures = false\n[settings.node]\ncorepack = true\n[settings.npm]\ndefault_installer = \"pnpm\"\n",
        )
        .unwrap();
        assert!(is_trusted(&config_dir, &path, None).unwrap());
    }

    /// Each reported key must carry the reason a person needs to judge it, and
    /// safe siblings in the same table must not be dragged in.
    #[test]
    fn dangerous_keys_are_reported_individually_with_a_reason() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            "[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nlang = \"zh\"\nverify_signatures = false\nrequire_checksums = false\n",
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        let keys: Vec<_> = found.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            ["settings.require_checksums", "settings.verify_signatures"]
        );
        assert!(found
            .iter()
            .all(|item| item.reason == TrustReason::WeakensVerification));

        // The rendered message names each key and explains it.
        let described = describe_requirements(&found);
        assert!(described.contains("settings.verify_signatures"));
        assert!(described.contains(&TrustReason::WeakensVerification.describe()));
        assert!(!described.contains("settings.jobs"));
    }

    #[test]
    fn syspkg_sources_and_registries_require_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        std::fs::write(&path, "[syspkg.packages]\n\"winget:Foo\" = \"latest\"\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "syspkg");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);

        for (body, key) in [
            ("[sources]\nselection = \"ordered\"\n", "sources"),
            (
                "[registries.npm]\nurls = [\"https://registry.npmjs.org/\"]\n",
                "registries",
            ),
        ] {
            std::fs::write(&path, body).unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{body}");
            assert_eq!(found[0].key, key);
            assert_eq!(found[0].reason, TrustReason::WeakensVerification, "{body}");
        }
    }

    /// The shim gates only what it can itself act on.
    ///
    /// It runs on every command invocation, so a refusal it cannot act on simply
    /// makes the directory unusable: a `[syspkg]` block used to make
    /// `cargo --version` fail with "project config is not trusted" -- a message
    /// about installing system packages, from a command that installs nothing --
    /// and since trust is bound to the file hash, every later edit of
    /// `osdk.toml` re-locked every tool again.
    ///
    /// Both directions are asserted. Narrowing this predicate too far is the
    /// more dangerous mistake and would not show up as a failure anywhere else:
    /// the shim would dispatch tools under a config whose `sources` or
    /// `registries` nobody reviewed.
    #[test]
    fn the_shim_gates_dispatch_affecting_keys_only() {
        let dispatch_affecting = |key: &str| {
            affects_tool_dispatch(&TrustRequirement {
                key: key.to_string(),
                reason: TrustReason::ExecutesCode,
            })
        };

        // Reached by the shim: it runs a registry preflight before starting a
        // package manager, and settings/tools decide what it resolves and runs.
        for key in [
            "sources",
            "registries",
            "settings.verify_signatures",
            "tools.npm.allow_builds",
        ] {
            assert!(dispatch_affecting(key), "{key} must still gate the shim");
        }

        // Never reached by the shim. `syspkg` is read only by `osdk pkg` (and
        // only `apply --yes` installs); `task_config` only by `osdk run` /
        // `osdk task`. Those commands evaluate the full requirement set
        // themselves, which is where the review belongs.
        for key in ["syspkg", "task_config"] {
            assert!(
                !dispatch_affecting(key),
                "{key} must not block an unrelated tool invocation"
            );
        }

        // An unrecognized table stays gated: a key this build cannot interpret
        // must not escape the shim's check by having been forgotten here.
        assert!(dispatch_affecting("something_new_from_the_future"));
    }

    /// Declaring a model is like declaring a dependency: it runs nothing and
    /// fetches nothing on its own, so `source`/`include`/`variant`/`when`/`views`
    /// must not demand trust (research §6.4). Only keys that actually choose the
    /// byte source (`endpoint`, a custom URL, an `insecure` toggle) do, and they
    /// are reported per model rather than gating the whole table.
    #[test]
    fn declaring_a_model_is_safe_but_an_endpoint_override_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        // A full declaration with no source-redirecting key needs no trust.
        let full = concat!(
            "[models.sd]\n",
            "source = \"hf:runwayml/stable-diffusion-v1-5@main\"\n",
            "include = [\"*.safetensors\"]\n",
            "variant = \"fp16\"\n",
            "[models.sd.views.comfyui.map]\n",
            "unet = \"diffusion_models\"\n",
            "vae = \"vae\"\n",
        );
        std::fs::write(&path, full).unwrap();
        assert!(
            !requires_trust(&path).unwrap(),
            "a model declaration with no endpoint must need no trust"
        );

        // An endpoint override is the one thing that does.
        std::fs::write(
            &path,
            concat!(
                "[models.sd]\n",
                "source = \"hf:runwayml/stable-diffusion-v1-5@main\"\n",
                "endpoint = \"https://mirror.example.com\"\n",
            ),
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "models.sd.endpoint");
        assert_eq!(found[0].reason, TrustReason::WeakensVerification);

        // insecure is reported under its own key, not the whole table.
        std::fs::write(
            &path,
            "[models.sd]
source = \"hf:o/r@main\"
insecure = true
",
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "models.sd.insecure");
    }

    /// Even when a model *does* require trust (an endpoint override), that
    /// requirement must never block the shim: the shim cannot reach model
    /// sources, and gating it would make `cargo --version` fail in a project
    /// that merely declares models. The `install`/`sync` paths still see it.
    #[test]
    fn model_requirements_never_affect_tool_dispatch() {
        let dispatch_affecting = |key: &str| {
            affects_tool_dispatch(&TrustRequirement {
                key: key.to_string(),
                reason: TrustReason::WeakensVerification,
            })
        };
        assert!(!dispatch_affecting("models"));
        assert!(!dispatch_affecting("models.sd.endpoint"));
        assert!(!dispatch_affecting("models.sd.insecure"));
    }

    /// A skill declaration is harmless; only an endpoint override needs trust,
    /// and the top-level `[skills]` defaults never do.
    #[test]
    fn declaring_a_skill_is_safe_but_an_endpoint_override_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        // Defaults plus a bare source declaration: no trust.
        std::fs::write(
            &path,
            concat!(
                "[skills]\n",
                "default_agents = [\"claude-code\"]\n",
                "scope = \"project\"\n",
                "[skills.web-design]\n",
                "source = \"github:vercel-labs/agent-skills\"\n",
                "agents = [\"claude-code\"]\n",
            ),
        )
        .unwrap();
        assert!(
            !requires_trust(&path).unwrap(),
            "a skill declaration with no endpoint must need no trust"
        );

        // An endpoint override is reported per skill, not as the whole table.
        std::fs::write(
            &path,
            concat!(
                "[skills.web-design]\n",
                "source = \"github:vercel-labs/agent-skills\"\n",
                "endpoint = \"https://mirror.example.com\"\n",
            ),
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "skills.web-design.endpoint");
        assert_eq!(found[0].reason, TrustReason::WeakensVerification);
    }

    /// A skill requirement must never block the shim, exactly like a model one:
    /// the shim cannot reach a skill source, and gating it would make an ordinary
    /// tool command fail in a project that merely declares skills.
    #[test]
    fn skill_requirements_never_affect_tool_dispatch() {
        let dispatch_affecting = |key: &str| {
            affects_tool_dispatch(&TrustRequirement {
                key: key.to_string(),
                reason: TrustReason::WeakensVerification,
            })
        };
        assert!(!dispatch_affecting("skills"));
        assert!(!dispatch_affecting("skills.web-design.endpoint"));
    }

    /// Editing a harmless declaration field must not invalidate a trust record
    /// that was granted for a sibling endpoint key. The governed subset keeps
    /// only the keys trust actually governs.
    /// An entry that carries an endpoint pins that whole model entry (the same
    /// whole-entry granularity `tools.<name>.allow_builds` uses: the reviewed
    /// identity is the entry, not one leaf). So editing a sibling field *inside
    /// that same entry* re-prompts, but editing a different, endpoint-free model
    /// does not. The governed subset projects by entry, not by the whole
    /// `[models]` table.
    #[test]
    fn an_endpoint_pins_its_own_model_entry_but_not_sibling_models() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        let config_dir = temp.path().join("state");

        let body = |sd_include: &str, other_include: &str| {
            format!(
                "[models.sd]\nsource = \"hf:a/b@main\"\nendpoint = \"https://m.example.com\"\ninclude = [{sd_include}]\n\n                 [models.plain]\nsource = \"hf:c/d@main\"\ninclude = [{other_include}]\n"
            )
        };

        std::fs::write(&path, body("\"a\"", "\"x\"")).unwrap();
        trust(&config_dir, &path).unwrap();

        // Editing a sibling model that has no endpoint keeps the record: the
        // governed subset does not include `models.plain` at all.
        std::fs::write(&path, body("\"a\"", "\"x\", \"y\"")).unwrap();
        assert!(
            is_trusted(&config_dir, &path, None).unwrap(),
            "editing a different endpoint-free model must not invalidate trust"
        );

        // Editing the reviewed entry -- even a harmless field -- re-prompts,
        // because that entry is the reviewed unit.
        std::fs::write(&path, body("\"a\", \"b\"", "\"x\", \"y\"")).unwrap();
        assert!(!is_trusted(&config_dir, &path, None).unwrap());

        // Changing the endpoint re-prompts as well (the direct case).
        std::fs::write(
            &path,
            "[models.sd]\nsource = \"hf:a/b@main\"\nendpoint = \"https://other.example.com\"\n             \n[models.plain]\nsource = \"hf:c/d@main\"\n",
        )
        .unwrap();
        assert!(!is_trusted(&config_dir, &path, None).unwrap());
    }

    /// Enabling a built-in deps provider must need no trust: osdk passes
    /// `--ignore-scripts` (or yarn berry's env equivalent) by default, so nothing
    /// the publisher did not ship as plain files runs. This is the same judgement
    /// `TrustReason`'s own docs make about declaring a package.
    #[test]
    fn enabling_a_builtin_deps_provider_needs_no_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for body in [
            "[deps.pnpm]\n",
            "[deps.npm]\nauto = true\n",
            "[deps.pnpm]\nsources = [\"package.json\"]\noutputs = [\"node_modules\"]\n",
            "[deps.pnpm]\ndir = \"apps/api\"\ndepends = [\"npm\"]\n",
            // Writing the build key to turn it *off* must not demand approval.
            "[deps.pnpm]\nallow_build_from_source = false\n",
            "[deps]\ndisable = [\"npm\"]\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(
                !requires_trust(&path).unwrap(),
                "should need no trust: {body}"
            );
        }
    }

    /// The two things that do change it, reported per provider and with the right
    /// reason. A custom registry is `WeakensVerification` because it redirects
    /// where bytes come from -- not because it executes anything.
    #[test]
    fn a_custom_registry_weakens_verification_and_builds_execute_code() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for (body, key) in [
            (
                "[deps.pnpm]\nindex = \"https://registry.example.com/\"\n",
                "deps.pnpm.index",
            ),
            (
                "[deps.npm]\nextra_index = \"https://other.example.com/\"\n",
                "deps.npm.extra_index",
            ),
            (
                "[deps.npm]\nregistry = \"https://r.example.com/\"\n",
                "deps.npm.registry",
            ),
            ("[deps.npm]\ninsecure = true\n", "deps.npm.insecure"),
        ] {
            std::fs::write(&path, body).unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{body}");
            assert_eq!(found[0].key, key, "{body}");
            assert_eq!(
                found[0].reason,
                TrustReason::WeakensVerification,
                "a redirected source is not code execution: {body}"
            );
        }

        std::fs::write(&path, "[deps.pnpm]\nallow_build_from_source = true\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "deps.pnpm.allow_build_from_source");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);
    }

    /// An env table can redirect the source as effectively as `index`, so it is
    /// gated too -- otherwise `index` demands approval while
    /// `env = { NPM_CONFIG_REGISTRY = ... }` achieves the same thing for free.
    /// (That hole existed in the first draft of this classifier and is the reason
    /// the check is here.)
    #[test]
    fn an_env_variable_that_redirects_the_registry_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for (variable, key) in [
            ("NPM_CONFIG_REGISTRY", "deps.pnpm.env.NPM_CONFIG_REGISTRY"),
            (
                "YARN_NPM_REGISTRY_SERVER",
                "deps.pnpm.env.YARN_NPM_REGISTRY_SERVER",
            ),
            ("PIP_INDEX_URL", "deps.pnpm.env.PIP_INDEX_URL"),
            ("UV_DEFAULT_INDEX", "deps.pnpm.env.UV_DEFAULT_INDEX"),
            (
                "NPM_CONFIG_@ACME:REGISTRY",
                "deps.pnpm.env.NPM_CONFIG_@ACME:REGISTRY",
            ),
        ] {
            std::fs::write(
                &path,
                format!("[deps.pnpm.env]\n\"{variable}\" = \"https://r.example.com/\"\n"),
            )
            .unwrap();
            let found = trust_requirements(&path).unwrap();
            assert_eq!(found.len(), 1, "{variable}");
            assert_eq!(found[0].key, key, "{variable}");
            assert_eq!(
                found[0].reason,
                TrustReason::WeakensVerification,
                "{variable}"
            );
        }
    }

    /// The other direction, which is the one that hides.
    ///
    /// AGENTS.md records the `UV_INDEX_` lesson: a *prefix* match sweeps in
    /// settings that are not credentials or sources at all, and the damage is
    /// invisible -- a needless approval prompt, or a feature quietly switched
    /// off. So names that merely resemble a source override must be asserted to
    /// stay trust-free. Every entry below shares a word or prefix with a real
    /// redirect and is not one.
    #[test]
    fn env_names_that_merely_resemble_a_redirect_do_not_require_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        for variable in [
            // Same `UV_INDEX_`/`*_INDEX*` neighbourhood, not a source.
            "UV_INDEX_STRATEGY",
            "NPM_CONFIG_INDEX_HINT",
            // Contains "REGISTRY" but does not name one.
            "NPM_CONFIG_REGISTRY_TIMEOUT_MS",
            // Ordinary settings.
            "NPM_CONFIG_FUND",
            "NODE_ENV",
            "CI",
        ] {
            std::fs::write(&path, format!("[deps.pnpm.env]\n\"{variable}\" = \"1\"\n")).unwrap();
            assert!(
                !requires_trust(&path).unwrap(),
                "`{variable}` is not a source override and must not demand approval"
            );
        }

        // Ordinary provider settings, likewise.
        std::fs::write(
            &path,
            concat!(
                "[deps.pnpm]\n",
                "sources = [\"package.json\"]\n",
                "outputs = [\"node_modules\"]\n",
                "auto = true\n",
                "installer = \"pnpm\"\n",
                "timeout = \"5m\"\n",
                "depends = [\"npm\"]\n",
                "dir = \"apps/api\"\n",
            ),
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
    }

    /// A custom provider's `run` is an arbitrary command, and `auto` can trigger
    /// it ahead of `osdk run`, so unlike a task the user does not point at it
    /// each time. Always `ExecutesCode`.
    #[test]
    fn a_custom_deps_provider_always_requires_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(
            &path,
            concat!(
                "[deps.codegen]\n",
                "sources = [\"schema.graphql\"]\n",
                "outputs = [\"src/generated\"]\n",
                "run = \"pnpm run codegen\"\n",
            ),
        )
        .unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "deps.codegen.run");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);
    }

    /// Even when a deps entry *does* require trust, that must never block the
    /// shim: the shim never materializes a dependency closure, and gating it
    /// would make `cargo --version` fail in a project that merely declares
    /// providers -- the `[syspkg]` accident all over again.
    #[test]
    fn deps_requirements_never_affect_tool_dispatch() {
        let dispatch_affecting = |key: &str| {
            affects_tool_dispatch(&TrustRequirement {
                key: key.to_string(),
                reason: TrustReason::ExecutesCode,
            })
        };
        assert!(!dispatch_affecting("deps"));
        assert!(!dispatch_affecting("deps.pnpm.index"));
        assert!(!dispatch_affecting("deps.codegen.run"));
        assert!(!dispatch_affecting("deps.pnpm.allow_build_from_source"));
    }

    /// An unknown top-level table, and an unknown `[settings]` key, must both
    /// fail closed. A build that cannot interpret a key cannot clear it.
    #[test]
    fn unrecognized_keys_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");

        std::fs::write(&path, "[future_capability]\nvalue = 1\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "future_capability");
        assert_eq!(found[0].reason, TrustReason::ExecutesCode);

        std::fs::write(&path, "[settings]\nfuture_switch = true\n").unwrap();
        let found = trust_requirements(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "settings.future_switch");
    }

    /// Every `Settings` field must appear on exactly one of the two lists.
    ///
    /// Without this, adding a field is silently classified by the fallback.
    /// That fallback is fail-closed so a new field cannot become a hole, but an
    /// unclassified field also cannot explain itself in the refusal, and a
    /// field that *is* safe would needlessly demand approval forever. The field
    /// names come from serializing a default `Settings`, so this test tracks the
    /// struct rather than a hand-copied list that would drift.
    /// Declaring a task must not demand a trust record.
    ///
    /// Nothing runs a task implicitly -- no postinstall, no lifecycle hook --
    /// so the user's `osdk run <name>` is itself the authorization. Gating it
    /// would ask the same question twice, which is how a gate teaches people to
    /// approve without reading. This test exists because re-adding `tasks` to
    /// the trust list looks like a safety improvement and is not.
    #[test]
    fn declaring_a_task_does_not_require_trust() {
        let value: toml::Value = toml::from_str(
            r#"
[tasks]
build = "cargo build --release"

[tasks.ci]
run = ["cargo test", { cmd = "cargo clippy", ignore_error = true }]
depends = ["build"]
"#,
        )
        .unwrap();
        assert_eq!(
            collect_requirements(&value),
            Vec::new(),
            "a config that only declares tasks must load without a trust record"
        );
    }

    /// `task_config` is an ambient setting, not a command the user names.
    ///
    /// Its `shell` decides the interpreter for every task in scope, so a config
    /// can redirect what `osdk run` executes without changing any task's text.
    #[test]
    fn task_config_still_requires_trust_because_it_picks_the_interpreter() {
        let value: toml::Value = toml::from_str("[task_config]\nshell = \"evil --run\"\n").unwrap();
        let found = collect_requirements(&value);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "task_config");
        // Not ExecutesCode: nothing here runs during install, and saying so
        // would be false.
        assert_eq!(found[0].reason, TrustReason::RedirectsExecution);
    }

    /// Tasks alongside a genuinely dangerous table must not mask it.
    #[test]
    fn tasks_do_not_suppress_another_tables_requirement() {
        let value: toml::Value = toml::from_str(
            r#"
[tasks]
build = "cargo build"

[sources]
mode = "env"
"#,
        )
        .unwrap();
        let keys: Vec<String> = collect_requirements(&value)
            .into_iter()
            .map(|requirement| requirement.key)
            .collect();
        assert_eq!(
            keys,
            vec!["sources".to_string()],
            "tasks must neither add nor remove"
        );
    }
    #[test]
    fn settings_allowlist_covers_every_field() {
        let settings = crate::config::Settings::default();
        let serialized = toml::Value::try_from(&settings).unwrap();
        let table = serialized
            .as_table()
            .expect("settings serialize to a table");

        let mut unclassified = Vec::new();
        for key in table.keys() {
            let safe = SAFE_SETTINGS_KEYS.contains(&key.as_str());
            let dangerous = TRUST_REQUIRING_SETTINGS.iter().any(|(name, _)| name == key);
            if safe == dangerous {
                // Either on neither list, or contradictorily on both.
                unclassified.push(key.clone());
            }
        }
        assert!(
            unclassified.is_empty(),
            "these `Settings` fields are not classified for trust: {unclassified:?}. \
             Add each to SAFE_SETTINGS_KEYS or TRUST_REQUIRING_SETTINGS."
        );
    }

    /// Trust identity must follow the gate: edits to keys that never needed
    /// approval must not invalidate a record, and edits to keys that did must.
    #[test]
    fn only_governed_keys_invalidate_a_trust_record() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");

        let write = |body: &str| std::fs::write(&config, body).unwrap();
        write("[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n");
        trust(&config_dir, &config).unwrap();
        assert!(is_trusted(&config_dir, &config, None).unwrap());

        // Bumping a version, adding a dependency, adding an alias and changing
        // a safe setting are all ungated, so trust survives all of them.
        for body in [
            "[tools]\nnode = \"22\"\n\n[settings]\njobs = 4\nverify_signatures = false\n",
            "[tools]\nnode = \"22\"\nformatter = \"npm:prettier@3\"\n\n[settings]\njobs = 4\nverify_signatures = false\n",
            "[tools]\nnode = \"22\"\nformatter = \"npm:prettier@3\"\n[aliases.node]\ndefault = \"22\"\n\n[settings]\njobs = 12\nverify_signatures = false\n",
            "# a comment\n[settings]\nverify_signatures = false\njobs = 12\n[tools]\nnode = '22'\nformatter = \"npm:prettier@3\"\n[aliases.node]\ndefault = \"22\"\n",
        ] {
            write(body);
            assert!(
                is_trusted(&config_dir, &config, None).unwrap(),
                "trust should survive: {body}"
            );
        }

        // Restoring signature verification changes a governed key, so the
        // record no longer matches. Fail-closed applies in both directions:
        // identity is "what was approved", not "is this safer now".
        write("[tools]\nnode = \"22\"\n\n[settings]\njobs = 12\nverify_signatures = true\n");
        assert!(!is_trusted(&config_dir, &config, None).unwrap());

        // Introducing a governed key that was absent at approval time must
        // also invalidate it.
        write("[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n");
        assert!(is_trusted(&config_dir, &config, None).unwrap());
        write(
            "[tools]\nnode = \"20\"\n\n[settings]\njobs = 4\nverify_signatures = false\n\n[syspkg.packages]\n\"winget:Foo\" = \"latest\"\n",
        );
        assert!(!is_trusted(&config_dir, &config, None).unwrap());
    }

    /// A config with nothing governed has no trust record to invalidate, so it
    /// must never be refused however much it changes.
    #[test]
    fn ungoverned_configs_are_never_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.toml");
        std::fs::write(&path, "[tools]\nnode = \"20\"\n").unwrap();
        let before = normalized_hash(&path).unwrap();
        std::fs::write(
            &path,
            "[tools]\nnode = \"24\"\nformatter = \"npm:prettier@3\"\ncli = \"github:cli/cli@2\"\n[aliases.node]\ndefault = \"24\"\n",
        )
        .unwrap();
        assert!(!requires_trust(&path).unwrap());
        // Identical because neither version contains a governed key.
        assert_eq!(before, normalized_hash(&path).unwrap());
    }

    /// The four states must be told apart, and only one of them prunable.
    ///
    /// `list` previously labelled both "the file changed" and "the file is gone"
    /// as `stale`, which left no way to tell whether the right move was `trust`
    /// or `untrust`.
    #[test]
    fn record_states_are_distinguished_and_only_missing_is_prunable() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");

        let active = temp.path().join("active");
        let changed = temp.path().join("changed");
        let gone = temp.path().join("gone");
        for directory in [&active, &changed, &gone] {
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(
                directory.join("osdk.toml"),
                "[sources]\nselection = \"ordered\"\n",
            )
            .unwrap();
            trust(&config_dir, &directory.join("osdk.toml")).unwrap();
        }

        // Still exactly as approved.
        // Same path, different governed content.
        std::fs::write(
            changed.join("osdk.toml"),
            "[sources]\nselection = \"auto\"\n",
        )
        .unwrap();
        // File removed, its directory still readable.
        std::fs::remove_file(gone.join("osdk.toml")).unwrap();

        let states: std::collections::BTreeMap<_, _> = list_with_state(&config_dir)
            .unwrap()
            .into_iter()
            .map(|(record, state)| (record.path, state))
            .collect();

        assert_eq!(
            states[&dunce::canonicalize(active.join("osdk.toml")).unwrap()],
            RecordState::Active
        );
        // The canonical path of a deleted file cannot be recomputed, so these two
        // are located by their directory name instead.
        let by_dir = |needle: &str| {
            *states
                .iter()
                .find(|(path, _)| path.to_string_lossy().contains(needle))
                .map(|(_, state)| state)
                .unwrap_or_else(|| panic!("no record under {needle}: {states:?}"))
        };
        assert_eq!(by_dir("changed"), RecordState::Changed);
        assert_eq!(by_dir("gone"), RecordState::Missing);

        // Only the removed file is prunable. `Changed` in particular is not: that
        // project is still there and only needs approving again, so dropping its
        // record would turn into an unexplained "untrusted" later.
        assert!(!RecordState::Active.is_prunable());
        assert!(!RecordState::Changed.is_prunable());
        assert!(RecordState::Missing.is_prunable());
        assert!(!RecordState::Unreachable.is_prunable());

        let removed = prune(&config_dir).unwrap();
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(removed[0].path.to_string_lossy().contains("gone"));

        // The other two survive, including the changed one.
        let after: Vec<_> = list(&config_dir)
            .unwrap()
            .into_iter()
            .map(|record| record.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(after.len(), 2, "{after:?}");
        assert!(
            after.iter().any(|path| path.contains("active")),
            "{after:?}"
        );
        assert!(
            after.iter().any(|path| path.contains("changed")),
            "a changed config must keep its record: {after:?}"
        );

        // Pruning again is a no-op rather than an error.
        assert!(prune(&config_dir).unwrap().is_empty());
    }

    /// A path whose parent directory is also gone is left alone.
    ///
    /// On Windows that is indistinguishable from a detached volume -- a USB disk,
    /// a network share, a WSL mount -- and pruning it would revoke valid
    /// approvals whenever a drive happened to be unplugged, surfacing much later
    /// as an unexplained "untrusted".
    #[test]
    fn an_unreachable_path_is_not_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("state");
        let repo = temp.path().join("detachable/repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = repo.join("osdk.toml");
        std::fs::write(&config, "[sources]\nselection = \"ordered\"\n").unwrap();
        trust(&config_dir, &config).unwrap();

        // Remove the whole tree, so neither the file nor its parent resolves.
        std::fs::remove_dir_all(temp.path().join("detachable")).unwrap();

        let states = list_with_state(&config_dir).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, RecordState::Unreachable);

        assert!(
            prune(&config_dir).unwrap().is_empty(),
            "an unreachable path must not be pruned"
        );
        assert_eq!(list(&config_dir).unwrap().len(), 1);
    }
}
