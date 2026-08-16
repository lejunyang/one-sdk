//! Layered configuration.
//!
//! Precedence (highest wins): CLI flags → env (`OSDK_*`) → project config
//! (`osdk.toml`, discovered by walking up) → user global config
//! (`$OSDK_CONFIG_DIR/config.toml`) → built-in defaults.
//!
//! This module owns the persisted settings shape. CLI-flag overlay is applied
//! by the caller (osdk-cli) on top of [`Config::load`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::source::{Selection, Source};
use crate::store::link::LinkMode;

pub const PROJECT_CONFIG_NAMES: &[&str] = &["osdk.toml", ".osdk.toml"];

/// Fully-resolved settings after merging all layers.
#[derive(Debug, Clone)]
pub struct Config {
    pub settings: Settings,
    pub sources: SourcesConfig,
    /// Tool pins gathered from config files (backend id -> version spec string).
    pub tools: BTreeMap<String, String>,
    /// User-defined version aliases: tool -> alias -> version spec.
    pub aliases: BTreeMap<String, BTreeMap<String, String>>,
    /// Path of the project config that contributed pins, if any.
    pub project_config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// How store blobs are materialized into install dirs.
    pub link_mode: LinkMode,
    /// Max concurrent downloads / installs.
    pub jobs: usize,
    /// Assume-yes for prompts.
    pub yes: bool,
    /// Whether to verify signatures when a backend provides them.
    pub verify_signatures: bool,
    /// Reject artifacts when no checksum is available.
    pub require_checksums: bool,
    /// GitHub artifact attestation verification policy.
    pub attestations: AttestationPolicy,
    /// Never make network requests; use cached metadata and archives only.
    pub offline: bool,
    /// Output language override (`en`/`zh`). None = auto-detect from locale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Node-specific installation behavior.
    pub node: NodeSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeSettings {
    /// Run the installed Node's own `corepack enable` after installation.
    pub corepack: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            link_mode: LinkMode::Auto,
            jobs: default_jobs(),
            yes: false,
            verify_signatures: true,
            require_checksums: false,
            attestations: AttestationPolicy::Off,
            offline: false,
            lang: None,
            node: NodeSettings::default(),
        }
    }
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub selection: Selection,
    pub probe_timeout_ms: u64,
    /// TTL for cached probe results, as a human string like "6h".
    pub cache_ttl: String,
    /// Per-tool source overrides.
    #[serde(flatten)]
    pub per_tool: BTreeMap<String, ToolSources>,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        SourcesConfig {
            selection: Selection::Auto,
            probe_timeout_ms: 1500,
            cache_ttl: "6h".to_string(),
            per_tool: BTreeMap::new(),
        }
    }
}

impl SourcesConfig {
    /// Parse the cache TTL string into seconds. Defaults to 6h on parse error.
    pub fn cache_ttl_secs(&self) -> u64 {
        parse_duration_secs(&self.cache_ttl).unwrap_or(6 * 3600)
    }
}

/// Per-tool source config: an optional pin and any user-added custom sources.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSources {
    /// Pin to a specific source id (overrides auto/ordered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// Disabled built-in source ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    /// User-added custom sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<Source>,
}

/// On-disk config file shape (a subset that users edit).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ConfigFile {
    settings: Option<Settings>,
    sources: Option<SourcesConfig>,
    tools: BTreeMap<String, String>,
    aliases: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    /// Load config by merging user global + project files, then env overrides.
    /// `start_dir` is where project-config discovery begins (usually cwd).
    pub fn load(user_config_file: &Path, start_dir: &Path) -> Result<Config> {
        Self::load_layers(user_config_file, Some(start_dir))
    }

    /// Load only user-global configuration and environment overrides. Trust
    /// management uses this so an untrusted project cannot influence the
    /// decision to trust itself.
    pub fn load_user(user_config_file: &Path) -> Result<Config> {
        Self::load_layers(user_config_file, None)
    }

    fn load_layers(user_config_file: &Path, start_dir: Option<&Path>) -> Result<Config> {
        let mut cfg = Config {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            project_config_path: None,
        };

        // 1. user global config
        if user_config_file.exists() {
            let file = read_config_file(user_config_file)?;
            cfg.apply_file(file);
        }

        // 2. project config (nearest ancestor). Also read .tool-versions pins.
        if let Some(start_dir) = start_dir {
            if let Some((path, file)) = find_project_config(start_dir)? {
                cfg.apply_file(file);
                cfg.project_config_path = Some(path);
            }
            if let Some(tv) = find_tool_versions(start_dir)? {
                for (k, v) in tv {
                    cfg.tools.entry(k).or_insert(v);
                }
            }
        }

        // 3. env overrides
        cfg.apply_env(|k| std::env::var(k).ok());

        Ok(cfg)
    }

    fn apply_file(&mut self, file: ConfigFile) {
        if let Some(s) = file.settings {
            self.settings = s;
        }
        if let Some(src) = file.sources {
            // merge: file replaces top-level knobs, per-tool maps merge
            let mut merged = self.sources.per_tool.clone();
            for (k, v) in src.per_tool {
                merged.insert(k, v);
            }
            self.sources = SourcesConfig {
                selection: src.selection,
                probe_timeout_ms: src.probe_timeout_ms,
                cache_ttl: src.cache_ttl,
                per_tool: merged,
            };
        }
        for (k, v) in file.tools {
            self.tools.insert(k, v);
        }
        for (tool, aliases) in file.aliases {
            self.aliases.entry(tool).or_default().extend(aliases);
        }
    }

    /// Apply `OSDK_*` env overrides. Exposed for testing.
    pub fn apply_env(&mut self, getenv: impl Fn(&str) -> Option<String>) {
        if let Some(v) = getenv("OSDK_LINK_MODE") {
            if let Ok(m) = v.parse::<LinkMode>() {
                self.settings.link_mode = m;
            }
        }
        if let Some(v) = getenv("OSDK_JOBS") {
            if let Ok(n) = v.parse::<usize>() {
                if n > 0 {
                    self.settings.jobs = n;
                }
            }
        }
        if let Some(v) = getenv("OSDK_YES") {
            self.settings.yes = truthy(&v);
        }
        if let Some(v) = getenv("OSDK_VERIFY_SIGNATURES") {
            self.settings.verify_signatures = truthy(&v);
        }
        if let Some(v) = getenv("OSDK_REQUIRE_CHECKSUMS") {
            self.settings.require_checksums = truthy(&v);
        }
        if let Some(v) = getenv("OSDK_ATTESTATIONS") {
            if let Ok(policy) = v.parse() {
                self.settings.attestations = policy;
            }
        }
        if let Some(v) = getenv("OSDK_OFFLINE") {
            self.settings.offline = truthy(&v);
        }
        if let Some(v) = getenv("OSDK_SELECTION") {
            self.sources.selection = match v.to_ascii_lowercase().as_str() {
                "pinned" => Selection::Pinned,
                "ordered" => Selection::Ordered,
                _ => Selection::Auto,
            };
        }
    }

    pub fn tool_sources(&self, tool: &str) -> Option<&ToolSources> {
        self.sources.per_tool.get(tool)
    }

    pub fn expand_alias(&self, tool: &str, spec: &str) -> Result<String> {
        let Some(aliases) = self.aliases.get(tool) else {
            return Ok(spec.to_string());
        };
        expand_alias(aliases, spec)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AttestationPolicy {
    #[default]
    Off,
    IfAvailable,
    Required,
}

impl std::str::FromStr for AttestationPolicy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Ok(Self::Off),
            "if-available" | "available" | "auto" => Ok(Self::IfAvailable),
            "required" | "require" | "true" | "1" => Ok(Self::Required),
            other => Err(Error::config(format!(
                "invalid attestation policy `{other}` (expected off|if-available|required)"
            ))),
        }
    }
}

impl std::fmt::Display for AttestationPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::IfAvailable => "if-available",
            Self::Required => "required",
        })
    }
}

pub fn validate_alias_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty()
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "latest" | "current" | "stable" | "system" | "lts" | "lts/*" | "lts-latest"
        )
        || name.starts_with("lts/")
        || name.starts_with("lts-")
    {
        return Err(Error::config(format!(
            "`{name}` is reserved and cannot be used as a version alias"
        )));
    }
    if name.contains(char::is_whitespace) || name.contains('@') {
        return Err(Error::config(format!("invalid version alias `{name}`")));
    }
    Ok(())
}

pub fn expand_alias(aliases: &BTreeMap<String, String>, spec: &str) -> Result<String> {
    let mut current = spec.to_string();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(next) = aliases.get(&current) {
        if !seen.insert(current.clone()) {
            let mut chain = seen.into_iter().collect::<Vec<_>>();
            chain.push(current);
            return Err(Error::config(format!(
                "version alias cycle: {}",
                chain.join(" -> ")
            )));
        }
        current = next.clone();
    }
    Ok(current)
}

fn truthy(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn read_config_file(path: &Path) -> Result<ConfigFile> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let file: ConfigFile = toml::from_str(&text)?;
    Ok(file)
}

/// Walk up from `start_dir` looking for a project config file.
fn find_project_config(start_dir: &Path) -> Result<Option<(PathBuf, ConfigFile)>> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        for name in PROJECT_CONFIG_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                let file = read_config_file(&candidate)?;
                return Ok(Some((candidate, file)));
            }
        }
        cur = dir.parent();
    }
    Ok(None)
}

/// Walk up looking for a `.tool-versions` file (asdf-compatible). Each line is
/// `<tool> <version>`; comments start with `#`.
fn find_tool_versions(start_dir: &Path) -> Result<Option<BTreeMap<String, String>>> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(".tool-versions");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).map_err(|e| Error::io(&candidate, e))?;
            return Ok(Some(parse_tool_versions(&text)));
        }
        cur = dir.parent();
    }
    Ok(None)
}

pub fn parse_tool_versions(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        if let (Some(tool), Some(ver)) = (it.next(), it.next()) {
            map.insert(tool.to_string(), ver.to_string());
        }
    }
    map
}

/// Parse a duration like "6h", "30m", "90s", "1d" into seconds.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: u64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn env_overrides_beat_file() {
        let mut cfg = Config {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            project_config_path: None,
        };
        cfg.settings.link_mode = LinkMode::Hardlink;
        cfg.apply_env(|k| match k {
            "OSDK_LINK_MODE" => Some("copy".to_string()),
            "OSDK_JOBS" => Some("3".to_string()),
            "OSDK_YES" => Some("true".to_string()),
            "OSDK_VERIFY_SIGNATURES" => Some("false".to_string()),
            "OSDK_REQUIRE_CHECKSUMS" => Some("true".to_string()),
            "OSDK_ATTESTATIONS" => Some("required".to_string()),
            "OSDK_OFFLINE" => Some("true".to_string()),
            _ => None,
        });
        assert_eq!(cfg.settings.link_mode, LinkMode::Copy);
        assert_eq!(cfg.settings.jobs, 3);
        assert!(cfg.settings.yes);
        assert!(!cfg.settings.verify_signatures);
        assert!(cfg.settings.require_checksums);
        assert_eq!(cfg.settings.attestations, AttestationPolicy::Required);
        assert!(cfg.settings.offline);
    }

    #[test]
    fn project_config_found_by_walkup() {
        let td = tempfile::tempdir().unwrap();
        let nested = td.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let cfg_path = td.path().join("a/osdk.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(f, "[tools]\nnode = \"20\"\n").unwrap();

        let found = find_project_config(&nested).unwrap();
        assert!(found.is_some());
        let (path, file) = found.unwrap();
        assert_eq!(path, cfg_path);
        assert_eq!(file.tools.get("node").map(|s| s.as_str()), Some("20"));
    }

    #[test]
    fn tool_versions_parse() {
        let m = parse_tool_versions(
            "# comment\nnode 20.11.1\npython 3.12.4 # trailing\n\ngo   1.22.5\n",
        );
        assert_eq!(m.get("node").unwrap(), "20.11.1");
        assert_eq!(m.get("python").unwrap(), "3.12.4");
        assert_eq!(m.get("go").unwrap(), "1.22.5");
    }

    #[test]
    fn duration_parse() {
        assert_eq!(parse_duration_secs("6h"), Some(6 * 3600));
        assert_eq!(parse_duration_secs("30m"), Some(1800));
        assert_eq!(parse_duration_secs("45"), Some(45));
        assert_eq!(parse_duration_secs("1d"), Some(86400));
        assert_eq!(parse_duration_secs("bad"), None);
    }

    #[test]
    fn aliases_expand_and_reject_cycles() {
        let aliases = BTreeMap::from([
            ("default".to_string(), "maintenance".to_string()),
            ("maintenance".to_string(), "20".to_string()),
        ]);
        assert_eq!(expand_alias(&aliases, "default").unwrap(), "20");
        assert_eq!(expand_alias(&aliases, "21").unwrap(), "21");

        let cycle = BTreeMap::from([
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        assert!(expand_alias(&cycle, "a")
            .unwrap_err()
            .to_string()
            .contains("cycle"));
        assert!(validate_alias_name("latest").is_err());
        assert!(validate_alias_name("default").is_ok());
    }

    #[test]
    fn attestation_policy_parses() {
        assert_eq!(
            "if-available".parse::<AttestationPolicy>().unwrap(),
            AttestationPolicy::IfAvailable
        );
        assert_eq!(
            "required".parse::<AttestationPolicy>().unwrap(),
            AttestationPolicy::Required
        );
        assert!("sometimes".parse::<AttestationPolicy>().is_err());
    }
}
