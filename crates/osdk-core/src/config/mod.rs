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
    /// Merged tool config entries with structured options preserved.
    pub tool_configs: BTreeMap<String, ToolConfigEntry>,
    /// Tool pins contributed by the user-global config before project merging.
    pub global_tools: BTreeMap<String, String>,
    /// Structured user-global tool entries before project merging.
    pub global_tool_configs: BTreeMap<String, ToolConfigEntry>,
    /// Origin of each winning entry in [`Config::tools`].
    pub tool_origins: BTreeMap<String, ToolConfigOrigin>,
    /// User-defined version aliases: tool -> alias -> version spec.
    pub aliases: BTreeMap<String, BTreeMap<String, String>>,
    /// Path of the nearest discovered project config, if any.
    pub project_config_path: Option<PathBuf>,
}

/// Source layer that contributed an effective tool entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolConfigOrigin {
    GlobalConfig(PathBuf),
    ProjectConfig(PathBuf),
    ToolVersions(PathBuf),
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
    /// Python catalog refresh and verification.
    pub python: PythonSettings,
    /// Java runtime metadata endpoint.
    pub java: JavaSettings,
    /// Pre-release resolution policy shared by supporting backends.
    pub prerelease: PrereleasePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeSettings {
    /// Run the installed Node's own `corepack enable` after installation.
    pub corepack: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PythonSettings {
    /// Optional JSON catalog URL or local path.
    pub catalog_url: Option<String>,
    /// Required SHA-256 for the exact catalog bytes.
    pub catalog_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JavaSettings {
    /// Foojay-compatible `/packages` endpoint or static mirror.
    pub catalog_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PrereleasePolicy {
    #[default]
    IfExplicit,
    Never,
    Allow,
}

impl std::str::FromStr for PrereleasePolicy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "never" => Ok(Self::Never),
            "if-explicit" | "explicit" | "auto" => Ok(Self::IfExplicit),
            "allow" | "always" => Ok(Self::Allow),
            other => Err(Error::config(format!(
                "invalid prerelease policy `{other}` (expected never|if-explicit|allow)"
            ))),
        }
    }
}

impl std::fmt::Display for PrereleasePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Never => "never",
            Self::IfExplicit => "if-explicit",
            Self::Allow => "allow",
        })
    }
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
            python: PythonSettings::default(),
            java: JavaSettings::default(),
            prerelease: PrereleasePolicy::default(),
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
    /// Package-registry configuration is persisted under the top-level
    /// `[registries]` table. It lives here internally so adding it does not
    /// break callers that construct [`Config`] directly.
    #[doc(hidden)]
    #[serde(skip)]
    pub registries: RegistriesConfig,
    /// Native-container configuration is persisted under the separate
    /// top-level `[containers]` table. It lives here internally so adding it
    /// does not break callers that construct [`Config`] directly.
    #[doc(hidden)]
    #[serde(skip)]
    pub containers: ContainersConfig,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        SourcesConfig {
            selection: Selection::Auto,
            probe_timeout_ms: 1500,
            cache_ttl: "6h".to_string(),
            per_tool: BTreeMap::new(),
            registries: RegistriesConfig::default(),
            containers: ContainersConfig::default(),
        }
    }
}

impl SourcesConfig {
    /// Parse the cache TTL string into seconds. Defaults to 6h on parse error.
    pub fn cache_ttl_secs(&self) -> u64 {
        parse_duration_secs(&self.cache_ttl).unwrap_or(6 * 3600)
    }
}

/// Registry preflight settings. Package registry URLs are deliberately kept
/// separate from SDK download sources because they affect delegated package
/// manager commands rather than osdk's own downloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RegistriesConfig {
    pub npm: NpmRegistryConfig,
}

/// Candidate registries for npm-compatible package managers. An empty list
/// means to use osdk's built-in public candidates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NpmRegistryConfig {
    pub urls: Vec<String>,
    pub probe_timeout_ms: u64,
}

impl Default for NpmRegistryConfig {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            probe_timeout_ms: 1500,
        }
    }
}

/// Native-only container integration settings. Higher-precedence configuration
/// files replace this section as a unit instead of merging registry policies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContainersConfig {
    pub runtime: ContainerRuntime,
    /// `auto` or an explicit native builder name.
    pub builder: crate::container::BuildxBuilderSelector,
    /// `runtime` or a strict OCI `OS/ARCH[/VARIANT]` selector.
    pub platform: ContainerPlatform,
    pub probe_timeout_ms: u64,
    pub registries: BTreeMap<String, ContainerRegistryConfig>,
}

impl Default for ContainersConfig {
    fn default() -> Self {
        Self {
            runtime: ContainerRuntime::Auto,
            builder: crate::container::BuildxBuilderSelector::Auto,
            platform: ContainerPlatform::Runtime,
            probe_timeout_ms: 1500,
            registries: BTreeMap::new(),
        }
    }
}

/// Native runtime selected for container inspection and operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerRuntime {
    #[default]
    Auto,
    Docker,
    Containerd,
}

impl std::str::FromStr for ContainerRuntime {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "docker" => Ok(Self::Docker),
            "containerd" => Ok(Self::Containerd),
            _ => Err(Error::config(
                "invalid container runtime (expected auto|docker|containerd)",
            )),
        }
    }
}

impl std::fmt::Display for ContainerRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Docker => "docker",
            Self::Containerd => "containerd",
        })
    }
}

impl<'de> Deserialize<'de> for ContainerRuntime {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(|_| {
            serde::de::Error::custom("invalid container runtime (expected auto|docker|containerd)")
        })
    }
}

/// Target platform chosen from the native runtime or an explicit OCI tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerPlatform {
    Runtime,
    Explicit {
        os: String,
        arch: String,
        variant: Option<String>,
    },
}

impl Default for ContainerPlatform {
    fn default() -> Self {
        Self::Runtime
    }
}

impl std::str::FromStr for ContainerPlatform {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value == "runtime" {
            return Ok(Self::Runtime);
        }

        let components = value.split('/').collect::<Vec<_>>();
        if !(2..=3).contains(&components.len())
            || components
                .iter()
                .any(|component| !is_oci_platform_component(component))
        {
            return Err(Error::config(
                "invalid container platform (expected runtime or OS/ARCH[/VARIANT])",
            ));
        }

        Ok(Self::Explicit {
            os: components[0].to_string(),
            arch: components[1].to_string(),
            variant: components.get(2).map(|value| (*value).to_string()),
        })
    }
}

impl std::fmt::Display for ContainerPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime => formatter.write_str("runtime"),
            Self::Explicit { os, arch, variant } => {
                write!(formatter, "{os}/{arch}")?;
                if let Some(variant) = variant {
                    write!(formatter, "/{variant}")?;
                }
                Ok(())
            }
        }
    }
}

impl Serialize for ContainerPlatform {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContainerPlatform {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn is_oci_platform_component(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

/// Mirror policy for one upstream OCI registry namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerRegistryConfig {
    pub mirrors: Vec<String>,
    pub anonymous_only: bool,
    pub resolve: ContainerResolve,
}

impl Default for ContainerRegistryConfig {
    fn default() -> Self {
        Self {
            mirrors: Vec::new(),
            anonymous_only: true,
            resolve: ContainerResolve::Upstream,
        }
    }
}

/// Whether tags are resolved by the origin registry or by a configured mirror.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerResolve {
    #[default]
    Upstream,
    Mirror,
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
    /// Export this model provider's endpoint/cache through shell activation.
    #[serde(default)]
    pub env: bool,
    /// Override pre-existing provider environment variables.
    #[serde(default)]
    pub env_force: bool,
}

/// Persisted `[tools]` entry. Legacy strings remain supported, while structured
/// objects can carry extra backend-specific options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolConfigEntry {
    Legacy(String),
    Structured(StructuredToolConfig),
}

impl ToolConfigEntry {
    pub fn legacy(version: impl Into<String>) -> Self {
        Self::Legacy(version.into())
    }

    pub fn structured(
        version: impl Into<String>,
        options: BTreeMap<String, ToolConfigValue>,
    ) -> Self {
        Self::Structured(StructuredToolConfig {
            version: version.into(),
            options,
        })
    }

    pub fn version(&self) -> &str {
        match self {
            Self::Legacy(version) => version,
            Self::Structured(config) => &config.version,
        }
    }

    pub fn options(&self) -> Option<&BTreeMap<String, ToolConfigValue>> {
        match self {
            Self::Legacy(_) => None,
            Self::Structured(config) => Some(&config.options),
        }
    }

    pub fn structured_config(&self) -> Option<&StructuredToolConfig> {
        match self {
            Self::Legacy(_) => None,
            Self::Structured(config) => Some(config),
        }
    }

    pub fn to_cli_option_strings(&self) -> Vec<String> {
        match self {
            Self::Legacy(_) => Vec::new(),
            Self::Structured(config) => config.to_cli_option_strings(),
        }
    }

    pub fn to_request_options(&self) -> BTreeMap<String, String> {
        match self {
            Self::Legacy(_) => BTreeMap::new(),
            Self::Structured(config) => config.to_request_options(),
        }
    }
}

/// Structured `[tools.<tool>]` object with a required version plus arbitrary
/// backend-specific options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredToolConfig {
    pub version: String,
    #[serde(flatten)]
    pub options: BTreeMap<String, ToolConfigValue>,
}

impl StructuredToolConfig {
    pub fn to_cli_option_strings(&self) -> Vec<String> {
        self.options
            .iter()
            .map(|(key, value)| format!("{key}={}", value.as_cli_value()))
            .collect()
    }

    pub fn to_request_options(&self) -> BTreeMap<String, String> {
        self.options
            .iter()
            .map(|(key, value)| (key.clone(), value.as_cli_value()))
            .collect()
    }
}

/// Arbitrary structured tool option value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolConfigValue {
    String(String),
    Bool(bool),
    Array(Vec<String>),
}

impl ToolConfigValue {
    pub fn as_cli_value(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Bool(value) => value.to_string(),
            Self::Array(values) => values.join(","),
        }
    }
}

/// On-disk config file shape (a subset that users edit).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ConfigFile {
    settings: Option<Settings>,
    sources: Option<SourcesConfig>,
    registries: Option<RegistriesConfig>,
    containers: Option<ContainersConfig>,
    tools: BTreeMap<String, ToolConfigEntry>,
    aliases: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    /// Load config by merging user global + project files, then env overrides.
    /// `start_dir` is where project-config discovery begins (usually cwd).
    pub fn load(user_config_file: &Path, start_dir: &Path) -> Result<Config> {
        load_layers_internal(user_config_file, Some(start_dir))
    }

    /// Load only user-global configuration and environment overrides. Trust
    /// management uses this so an untrusted project cannot influence the
    /// decision to trust itself.
    pub fn load_user(user_config_file: &Path) -> Result<Config> {
        load_layers_internal(user_config_file, None)
    }

    fn apply_file(&mut self, file: ConfigFile, allow_model_env: bool) {
        if let Some(s) = file.settings {
            self.settings = s;
        }
        if let Some(src) = file.sources {
            // merge: file replaces top-level knobs, per-tool maps merge
            let mut merged = self.sources.per_tool.clone();
            for (k, mut v) in src.per_tool {
                if !allow_model_env {
                    let global = merged.get(&k);
                    v.env = global.is_some_and(|config| config.env);
                    v.env_force = global.is_some_and(|config| config.env_force);
                }
                merged.insert(k, v);
            }
            self.sources = SourcesConfig {
                selection: src.selection,
                probe_timeout_ms: src.probe_timeout_ms,
                cache_ttl: src.cache_ttl,
                per_tool: merged,
                registries: self.sources.registries.clone(),
                containers: self.sources.containers.clone(),
            };
        }
        if let Some(registries) = file.registries {
            // Registry sections replace the lower-precedence layer as a unit.
            self.sources.registries = registries;
        }
        if let Some(containers) = file.containers {
            // Container sections replace the lower-precedence layer as a unit.
            self.sources.containers = containers;
        }
        self.apply_tool_configs(&file.tools);
        for (tool, aliases) in file.aliases {
            self.aliases.entry(tool).or_default().extend(aliases);
        }
    }

    fn apply_tool_configs(&mut self, tools: &BTreeMap<String, ToolConfigEntry>) {
        for (tool, entry) in tools {
            self.tools.insert(tool.clone(), entry.version().to_string());
            self.tool_configs.insert(tool.clone(), entry.clone());
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
        if let Some(v) = getenv("OSDK_PRERELEASE") {
            if let Ok(policy) = v.parse() {
                self.settings.prerelease = policy;
            }
        }
        if let Some(v) = getenv("OSDK_PYTHON_CATALOG_URL") {
            self.settings.python.catalog_url = Some(v);
        }
        if let Some(v) = getenv("OSDK_PYTHON_CATALOG_SHA256") {
            self.settings.python.catalog_sha256 = Some(v);
        }
        if let Some(v) = getenv("OSDK_JAVA_CATALOG_URL") {
            self.settings.java.catalog_url = Some(v);
        }
        if let Some(v) = getenv("OSDK_SELECTION") {
            self.sources.selection = match v.to_ascii_lowercase().as_str() {
                "pinned" => Selection::Pinned,
                "ordered" => Selection::Ordered,
                _ => Selection::Auto,
            };
        }
        if let Some(v) = getenv("OSDK_CONTAINER_RUNTIME") {
            if let Ok(runtime) = v.parse() {
                self.sources.containers.runtime = runtime;
            }
        }
        if let Some(v) = getenv("OSDK_CONTAINER_BUILDER") {
            if let Ok(builder) = v.parse() {
                self.sources.containers.builder = builder;
            }
        }
        if let Some(v) = getenv("OSDK_CONTAINER_PLATFORM") {
            if let Ok(platform) = v.parse() {
                self.sources.containers.platform = platform;
            }
        }
    }

    pub fn tool_sources(&self, tool: &str) -> Option<&ToolSources> {
        self.sources.per_tool.get(tool)
    }

    /// Provenance of the effective merged tool entry.
    pub fn tool_origin(&self, tool: &str) -> Option<&ToolConfigOrigin> {
        self.tool_origins.get(tool)
    }

    /// Effective package-registry configuration after user/project layering.
    pub fn registries(&self) -> &RegistriesConfig {
        &self.sources.registries
    }

    /// Effective native-container configuration after user/project layering.
    pub fn containers(&self) -> &ContainersConfig {
        &self.sources.containers
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
    let mut file: ConfigFile = toml::from_str(&text).map_err(sanitize_config_parse_error)?;
    if let Some(registries) = &mut file.registries {
        normalize_registry_urls(&mut registries.npm.urls)?;
    }
    if let Some(containers) = &mut file.containers {
        validate_containers_config(containers)?;
    }
    Ok(file)
}

fn sanitize_config_parse_error(error: toml::de::Error) -> Error {
    let message = error.message();
    if message.contains("invalid container runtime") {
        Error::config("invalid container runtime (expected auto|docker|containerd)")
    } else if message.contains("invalid Buildx builder selector") {
        Error::config("invalid container builder (expected auto or a safe ASCII name)")
    } else if message.contains("invalid container platform") {
        Error::config("invalid container platform (expected runtime or OS/ARCH[/VARIANT])")
    } else {
        Error::TomlDe(error)
    }
}

fn validate_containers_config(config: &mut ContainersConfig) -> Result<()> {
    if config.probe_timeout_ms == 0 {
        return Err(Error::config(
            "container probe_timeout_ms must be greater than zero",
        ));
    }

    let registries = std::mem::take(&mut config.registries);
    let mut canonical_registries = BTreeMap::new();
    for (registry, mut policy) in registries {
        let registry = canonical_container_registry_name(&registry)?;
        let mut normalized = Vec::with_capacity(policy.mirrors.len());
        for mirror in &policy.mirrors {
            let mirror = normalize_container_mirror_url(mirror)?;
            if !normalized.contains(&mirror) {
                normalized.push(mirror);
            }
        }
        policy.mirrors = normalized;
        if canonical_registries.insert(registry, policy).is_some() {
            return Err(Error::config(
                "container registry keys collide after canonicalization",
            ));
        }
    }
    config.registries = canonical_registries;
    Ok(())
}

fn canonical_container_registry_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.contains("://")
        || value.contains(['/', '?', '#', '@'])
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(Error::config(
            "invalid container registry (expected a host name with optional port)",
        ));
    }

    // URL parsing gives us strict host and port validation while the fixed
    // scheme prevents registry keys from embedding credentials or paths.
    let parsed = reqwest::Url::parse(&format!("https://{value}/"))
        .map_err(|_| Error::config("invalid container registry"))?;
    if parsed.host_str().is_none() || parsed.port().is_none() && value.ends_with(':') {
        return Err(Error::config(
            "invalid container registry (expected a host name with optional port)",
        ));
    }
    let host = parsed
        .host_str()
        .expect("host checked above")
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err(Error::config("invalid container registry"));
    }
    Ok(match parsed.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None if host.contains(':') => format!("[{host}]"),
        None => host,
    })
}

/// Validate and canonicalize a persisted OCI mirror URL. Persisted mirror
/// policy is HTTPS-only and never carries credentials, query, or fragment.
pub fn normalize_container_mirror_url(value: &str) -> Result<String> {
    let original = value.trim();
    let mut url =
        reqwest::Url::parse(original).map_err(|_| Error::config("invalid container mirror URL"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(Error::config(
            "container mirror URL must use https and include a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::config(
            "container mirror URL must not contain credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::config(
            "container mirror URL must not contain a query string or fragment",
        ));
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{path}/"));
    Ok(url.to_string())
}

fn load_layers_internal(user_config_file: &Path, start_dir: Option<&Path>) -> Result<Config> {
    let mut cfg = Config {
        settings: Settings::default(),
        sources: SourcesConfig::default(),
        tools: BTreeMap::new(),
        tool_configs: BTreeMap::new(),
        global_tools: BTreeMap::new(),
        global_tool_configs: BTreeMap::new(),
        tool_origins: BTreeMap::new(),
        aliases: BTreeMap::new(),
        project_config_path: None,
    };

    if user_config_file.exists() {
        let file = read_config_file(user_config_file)?;
        cfg.global_tool_configs = file.tools.clone();
        cfg.global_tools = file
            .tools
            .iter()
            .map(|(tool, entry)| (tool.clone(), entry.version().to_string()))
            .collect();
        cfg.tool_origins.extend(file.tools.keys().map(|tool| {
            (
                tool.clone(),
                ToolConfigOrigin::GlobalConfig(user_config_file.to_path_buf()),
            )
        }));
        cfg.apply_file(file, true);
    }

    if let Some(start_dir) = start_dir {
        if let Some((path, file)) = find_project_config(start_dir)? {
            cfg.tool_origins.extend(
                file.tools
                    .keys()
                    .map(|tool| (tool.clone(), ToolConfigOrigin::ProjectConfig(path.clone()))),
            );
            cfg.apply_file(file, false);
            cfg.project_config_path = Some(path);
        }
        if let Some((path, tv)) = find_tool_versions(start_dir)? {
            for (tool, version) in tv {
                if !cfg.tools.contains_key(&tool) {
                    cfg.tools.insert(tool.clone(), version.clone());
                    cfg.tool_configs
                        .insert(tool.clone(), ToolConfigEntry::legacy(version));
                    cfg.tool_origins
                        .insert(tool, ToolConfigOrigin::ToolVersions(path.clone()));
                }
            }
        }
    }

    cfg.apply_env(|k| std::env::var(k).ok());

    Ok(cfg)
}

fn normalize_registry_urls(urls: &mut Vec<String>) -> Result<()> {
    let mut normalized = Vec::with_capacity(urls.len());
    for value in urls.iter() {
        let value = normalize_registry_url(value)?;
        if !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    *urls = normalized;
    Ok(())
}

/// Validate and canonicalize an npm-compatible registry base URL. Credentials,
/// query strings, and fragments are rejected.
pub fn normalize_registry_url(value: &str) -> Result<String> {
    let original = value.trim();
    let mut url =
        reqwest::Url::parse(original).map_err(|_| Error::config("invalid registry URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::config(
            "registry URL must use http or https and include a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::config("registry URL must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::config(
            "registry URL must not contain a query string or fragment",
        ));
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{path}/"));
    Ok(url.to_string())
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
fn find_tool_versions(start_dir: &Path) -> Result<Option<(PathBuf, BTreeMap<String, String>)>> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(".tool-versions");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).map_err(|e| Error::io(&candidate, e))?;
            return Ok(Some((candidate, parse_tool_versions(&text))));
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
            tool_configs: BTreeMap::new(),
            global_tools: BTreeMap::new(),
            global_tool_configs: BTreeMap::new(),
            tool_origins: BTreeMap::new(),
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
    fn container_config_parses_strict_native_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        std::fs::write(
            &config_file,
            r#"
[containers]
runtime = "containerd"
builder = "remote-builder_1"
platform = "linux/arm64/v8"
probe_timeout_ms = 750

[containers.registries."docker.io"]
mirrors = ["https://mirror.example/cache", "https://mirror.example/cache/"]
anonymous_only = false
resolve = "mirror"
"#,
        )
        .unwrap();

        let config = Config::load_user(&config_file).unwrap();
        assert_eq!(config.containers().runtime, ContainerRuntime::Containerd);
        assert_eq!(
            config.containers().builder.as_name(),
            Some("remote-builder_1")
        );
        assert_eq!(
            config.containers().platform,
            ContainerPlatform::Explicit {
                os: "linux".to_string(),
                arch: "arm64".to_string(),
                variant: Some("v8".to_string()),
            }
        );
        assert_eq!(config.containers().probe_timeout_ms, 750);
        assert_eq!(
            config.containers().registries["docker.io"].mirrors,
            ["https://mirror.example/cache/"]
        );
        assert!(!config.containers().registries["docker.io"].anonymous_only);
        assert_eq!(
            config.containers().registries["docker.io"].resolve,
            ContainerResolve::Mirror
        );
    }

    #[test]
    fn project_container_config_replaces_user_section_as_a_unit() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[containers]
runtime = "docker"
builder = "global-builder"
platform = "linux/amd64"
probe_timeout_ms = 900

[containers.registries."docker.io"]
mirrors = ["https://global.example"]
anonymous_only = false
resolve = "mirror"
"#,
        )
        .unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            r#"
[containers]
runtime = "containerd"

[containers.registries."ghcr.io"]
mirrors = ["https://project.example"]
"#,
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        let containers = config.containers();
        assert_eq!(containers.runtime, ContainerRuntime::Containerd);
        assert_eq!(
            containers.builder,
            crate::container::BuildxBuilderSelector::Auto
        );
        assert_eq!(containers.platform, ContainerPlatform::Runtime);
        assert_eq!(containers.probe_timeout_ms, 1500);
        assert!(!containers.registries.contains_key("docker.io"));
        assert_eq!(
            containers.registries["ghcr.io"].mirrors,
            ["https://project.example/"]
        );
        assert!(containers.registries["ghcr.io"].anonymous_only);
        assert_eq!(
            containers.registries["ghcr.io"].resolve,
            ContainerResolve::Upstream
        );
    }

    #[test]
    fn project_container_config_requires_trust() {
        let temporary = tempfile::tempdir().unwrap();
        let project_config = temporary.path().join("osdk.toml");
        std::fs::write(&project_config, "[containers]\nruntime = \"auto\"\n").unwrap();

        assert!(crate::trust::requires_trust(&project_config).unwrap());
    }

    #[test]
    fn container_env_overrides_only_native_selectors() {
        let mut cfg = Config {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: BTreeMap::new(),
            tool_configs: BTreeMap::new(),
            global_tools: BTreeMap::new(),
            global_tool_configs: BTreeMap::new(),
            tool_origins: BTreeMap::new(),
            aliases: BTreeMap::new(),
            project_config_path: None,
        };
        cfg.sources.containers.registries.insert(
            "docker.io".to_string(),
            ContainerRegistryConfig {
                mirrors: vec!["https://mirror.example/".to_string()],
                anonymous_only: true,
                resolve: ContainerResolve::Upstream,
            },
        );

        cfg.apply_env(|key| match key {
            "OSDK_CONTAINER_RUNTIME" => Some("docker".to_string()),
            "OSDK_CONTAINER_BUILDER" => Some("ci-builder".to_string()),
            "OSDK_CONTAINER_PLATFORM" => Some("linux/amd64".to_string()),
            // These deliberately have no supported environment surface.
            "OSDK_CONTAINER_MIRRORS" => Some("https://evil.example".to_string()),
            "OSDK_CONTAINER_RESOLVE" => Some("mirror".to_string()),
            _ => None,
        });

        assert_eq!(cfg.containers().runtime, ContainerRuntime::Docker);
        assert_eq!(cfg.containers().builder.as_name(), Some("ci-builder"));
        assert_eq!(cfg.containers().platform.to_string(), "linux/amd64");
        assert_eq!(
            cfg.containers().registries["docker.io"].mirrors,
            ["https://mirror.example/"]
        );
        assert_eq!(
            cfg.containers().registries["docker.io"].resolve,
            ContainerResolve::Upstream
        );
    }

    #[test]
    fn invalid_container_env_selectors_leave_config_unchanged() {
        let mut cfg = Config {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: BTreeMap::new(),
            tool_configs: BTreeMap::new(),
            global_tools: BTreeMap::new(),
            global_tool_configs: BTreeMap::new(),
            tool_origins: BTreeMap::new(),
            aliases: BTreeMap::new(),
            project_config_path: None,
        };

        cfg.apply_env(|key| match key {
            "OSDK_CONTAINER_RUNTIME" => Some("podman".to_string()),
            "OSDK_CONTAINER_BUILDER" => Some("name with spaces".to_string()),
            "OSDK_CONTAINER_PLATFORM" => Some("linux".to_string()),
            _ => None,
        });

        assert_eq!(cfg.containers(), &ContainersConfig::default());
    }

    #[test]
    fn container_config_rejects_invalid_values_and_unsafe_mirrors() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        let invalid = [
            "[containers]\nruntime = \"podman\"\n",
            "[containers]\nbuilder = \"name with spaces\"\n",
            "[containers]\nplatform = \"linux\"\n",
            "[containers]\nplatform = \"Linux/amd64\"\n",
            "[containers]\nplatform = \"linux/amd64/v8/extra\"\n",
            "[containers]\nprobe_timeout_ms = 0\n",
            "[containers]\nunknown = true\n",
            "[containers.registries.\"docker.io\"]\nresolve = \"fastest\"\n",
            "[containers.registries.\"docker.io\"]\nmirrors = [\"http://127.0.0.1:5000\"]\n",
            "[containers.registries.\"docker.io\"]\nmirrors = [\"https://user:secret@example.test\"]\n",
            "[containers.registries.\"docker.io\"]\nmirrors = [\"https://example.test?token=secret\"]\n",
            "[containers.registries.\"docker.io\"]\nmirrors = [\"https://example.test#fragment\"]\n",
            "[containers.registries.\"https://docker.io/path\"]\nmirrors = [\"https://example.test\"]\n",
        ];

        for contents in invalid {
            std::fs::write(&config_file, contents).unwrap();
            assert!(
                Config::load_user(&config_file).is_err(),
                "accepted invalid container config: {contents}"
            );
        }
    }

    #[test]
    fn container_registry_keys_are_canonicalized_without_losing_ports() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        std::fs::write(
            &config_file,
            r#"
[containers.registries."EXAMPLE.COM."]
[containers.registries."Registry.Example:5443"]
[containers.registries."[2001:DB8::1]:5000"]
"#,
        )
        .unwrap();

        let config = Config::load_user(&config_file).unwrap();
        let registries = &config.containers().registries;
        assert!(registries.contains_key("example.com"));
        assert!(registries.contains_key("registry.example:5443"));
        assert!(registries.contains_key("[2001:db8::1]:5000"));
    }

    #[test]
    fn canonical_container_registry_collisions_are_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        std::fs::write(
            &config_file,
            r#"
[containers.registries."docker.io"]
[containers.registries."DOCKER.IO."]
"#,
        )
        .unwrap();

        let error = Config::load_user(&config_file).unwrap_err();
        assert_eq!(
            error.to_string(),
            "config error: container registry keys collide after canonicalization"
        );
    }

    #[test]
    fn container_validation_errors_do_not_echo_rejected_values() {
        let cases = [
            ("runtime", "token-runtime-7d9"),
            ("builder", "token builder 7d9"),
            ("platform", "token-platform-7d9"),
        ];
        for (field, secret) in cases {
            let temporary = tempfile::tempdir().unwrap();
            let config_file = temporary.path().join("config.toml");
            std::fs::write(
                &config_file,
                format!("[containers]\n{field} = {secret:?}\n"),
            )
            .unwrap();
            let error = Config::load_user(&config_file).unwrap_err();
            assert!(!error.to_string().contains(secret));
            assert!(!format!("{error:?}").contains(secret));
        }

        for (contents, secret) in [
            (
                "[containers.registries.\"user:registry-secret-7d9@example.test\"]\n",
                "registry-secret-7d9",
            ),
            (
                "[containers.registries.\"docker.io\"]\nmirrors = [\"https://user:mirror-secret-7d9@example.test\"]\n",
                "mirror-secret-7d9",
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let config_file = temporary.path().join("config.toml");
            std::fs::write(&config_file, contents).unwrap();
            let error = Config::load_user(&config_file).unwrap_err();
            assert!(!error.to_string().contains(secret));
            assert!(!format!("{error:?}").contains(secret));
        }
    }

    #[test]
    fn containers_default_to_runtime_platform() {
        assert_eq!(
            ContainersConfig::default().platform,
            ContainerPlatform::Runtime
        );
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        std::fs::write(&config_file, "[containers]\nruntime = \"docker\"\n").unwrap();
        assert_eq!(
            Config::load_user(&config_file)
                .unwrap()
                .containers()
                .platform,
            ContainerPlatform::Runtime
        );
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
        assert_eq!(
            file.tools.get("node").map(ToolConfigEntry::version),
            Some("20")
        );
    }

    #[test]
    fn structured_tool_configs_support_legacy_inline_table_and_scoped_keys() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/nested");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
node = "20"
npm = { version = "11.5.2", allow_builds = ["esbuild", "sharp"], engine = "node", frozen = true }
"@scope/tool" = { version = "1.2.3", allow_builds = ["pkg-a"] }
"#,
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        let tools = &config.tool_configs;
        assert_eq!(tools["node"].version(), "20");
        assert_eq!(tools["npm"].version(), "11.5.2");
        assert_eq!(
            tools["@scope/tool"]
                .structured_config()
                .unwrap()
                .options
                .get("allow_builds"),
            Some(&ToolConfigValue::Array(vec!["pkg-a".to_string()]))
        );
        assert_eq!(
            tools["npm"]
                .structured_config()
                .unwrap()
                .options
                .get("engine"),
            Some(&ToolConfigValue::String("node".to_string()))
        );
        assert_eq!(
            tools["npm"]
                .structured_config()
                .unwrap()
                .options
                .get("frozen"),
            Some(&ToolConfigValue::Bool(true))
        );
        assert_eq!(
            tools["@scope/tool"].to_cli_option_strings(),
            vec!["allow_builds=pkg-a".to_string()]
        );
        assert_eq!(
            tools["npm"].to_request_options().get("allow_builds"),
            Some(&"esbuild,sharp".to_string())
        );
    }

    #[test]
    fn project_tool_configs_override_global_and_tool_versions_only_fill_missing() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/nested");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
node = "20"
npm = { version = "11.5.1", allow_builds = ["esbuild"] }
"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("project/osdk.toml"),
            r#"
[tools]
npm = { version = "11.5.2", allow_builds = ["sharp"], engine = "node" }
pnpm = "9.0.0"
"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("project/.tool-versions"),
            "node 22.0.0\nbun 1.1.0\n",
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        let tools = &config.tool_configs;
        assert_eq!(tools["node"].version(), "20");
        assert_eq!(tools["npm"].version(), "11.5.2");
        assert_eq!(tools["pnpm"].version(), "9.0.0");
        assert_eq!(tools["bun"].version(), "1.1.0");
        assert_eq!(
            tools["npm"]
                .structured_config()
                .unwrap()
                .options
                .get("allow_builds"),
            Some(&ToolConfigValue::Array(vec!["sharp".to_string()]))
        );
        assert_eq!(
            tools["npm"]
                .structured_config()
                .unwrap()
                .options
                .get("engine"),
            Some(&ToolConfigValue::String("node".to_string()))
        );
        assert_eq!(config.global_tools["node"], "20");
        assert_eq!(config.global_tools["npm"], "11.5.1");
        assert_eq!(config.global_tool_configs["npm"].version(), "11.5.1");
        assert_eq!(
            config.tool_origins["node"],
            ToolConfigOrigin::GlobalConfig(user_config.clone())
        );
        assert_eq!(
            config.tool_origins["npm"],
            ToolConfigOrigin::ProjectConfig(temporary.path().join("project/osdk.toml"))
        );
        assert_eq!(
            config.tool_origins["pnpm"],
            ToolConfigOrigin::ProjectConfig(temporary.path().join("project/osdk.toml"))
        );
        assert_eq!(
            config.tool_origins["bun"],
            ToolConfigOrigin::ToolVersions(temporary.path().join("project/.tool-versions"))
        );
    }

    #[test]
    fn load_user_preserves_global_tool_provenance_without_project_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            "[tools]\nnode = \"20\"\nnpm = { version = \"11\", installer = \"npm\" }\n",
        )
        .unwrap();

        let config = Config::load_user(&user_config).unwrap();
        assert_eq!(config.tools, config.global_tools);
        assert_eq!(config.tool_configs, config.global_tool_configs);
        assert_eq!(
            config.tool_origins["npm"],
            ToolConfigOrigin::GlobalConfig(user_config)
        );
        assert!(config.project_config_path.is_none());
    }

    #[test]
    fn structured_tool_config_requires_version_and_rejects_non_scalar_options() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");

        std::fs::write(
            &config_file,
            r#"
[tools.npm]
allow_builds = ["esbuild"]
"#,
        )
        .unwrap();
        assert!(Config::load_user(&config_file).is_err());

        std::fs::write(
            &config_file,
            r#"
[tools.npm]
version = "11.5.2"
nested = { enabled = true }
"#,
        )
        .unwrap();
        assert!(Config::load_user(&config_file).is_err());
    }

    #[test]
    fn project_sources_cannot_override_global_model_env_switches() {
        let temporary = tempfile::tempdir().unwrap();
        let config_dir = temporary.path().join("config");
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let user_config = config_dir.join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[sources.huggingface]
env = true
env_force = true
pin = "global"
"#,
        )
        .unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            r#"
[sources.huggingface]
env = false
env_force = false
pin = "project"
"#,
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        let huggingface = config.tool_sources("huggingface").unwrap();
        assert!(huggingface.env);
        assert!(huggingface.env_force);
        assert_eq!(huggingface.pin.as_deref(), Some("project"));
    }

    #[test]
    fn project_registry_replaces_user_registry_and_load_user_excludes_it() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/nested");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[registries.npm]
urls = ["https://registry.npmjs.org"]
probe_timeout_ms = 900
"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("project/osdk.toml"),
            r#"
[registries.npm]
urls = ["https://registry.npmmirror.com/path/"]
probe_timeout_ms = 125
"#,
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        assert_eq!(
            config.registries().npm.urls,
            ["https://registry.npmmirror.com/path/"]
        );
        assert_eq!(config.registries().npm.probe_timeout_ms, 125);

        let user = Config::load_user(&user_config).unwrap();
        assert_eq!(user.registries().npm.urls, ["https://registry.npmjs.org/"]);
        assert_eq!(user.registries().npm.probe_timeout_ms, 900);
    }

    #[test]
    fn registry_urls_normalize_and_reject_unsafe_values() {
        assert_eq!(
            normalize_registry_url("https://example.test/team").unwrap(),
            "https://example.test/team/"
        );
        let query_error = normalize_registry_url("https://example.test/team?x=1").unwrap_err();
        assert_eq!(
            query_error.to_string(),
            "config error: registry URL must not contain a query string or fragment"
        );
        let fragment_error =
            normalize_registry_url("https://example.test/team#fragment").unwrap_err();
        assert_eq!(
            fragment_error.to_string(),
            "config error: registry URL must not contain a query string or fragment"
        );
        assert!(normalize_registry_url("file:///tmp/registry").is_err());
        assert!(normalize_registry_url("https://token@example.test/").is_err());
        assert!(normalize_registry_url("relative/path").is_err());
    }

    #[test]
    fn registry_config_rejects_query_and_fragment() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");

        for url in [
            "https://registry.npmjs.org/?write=true",
            "https://registry.npmjs.org/#scope",
        ] {
            std::fs::write(
                &config_file,
                format!("[registries.npm]\nurls = [{url:?}]\n"),
            )
            .unwrap();

            let error = Config::load_user(&config_file).unwrap_err();
            assert_eq!(
                error.to_string(),
                "config error: registry URL must not contain a query string or fragment"
            );
        }
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
