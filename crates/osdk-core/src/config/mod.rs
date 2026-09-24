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
use crate::source::{Selection, Source, SourceMode};
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
    /// Project tasks, merged and platform-filtered.
    ///
    /// Behind `install` for the same reason as the module: the shim dispatches
    /// tools and never runs a task, so its build should not carry the parsing.
    #[cfg(feature = "install")]
    pub tasks: crate::tasks::TaskSet,
    /// Declared project models (`[models.<name>]`). Like tasks, install-gated:
    /// the shim never reads model declarations.
    #[cfg(feature = "install")]
    pub models: BTreeMap<String, ModelDeclaration>,
    /// Declared agent skills (`[skills.<name>]`). Install-gated like models: the
    /// shim never installs a skill.
    #[cfg(feature = "install")]
    pub skills: BTreeMap<String, SkillDeclaration>,
    /// Top-level `[skills]` defaults (default agents, scope, link mode).
    #[cfg(feature = "install")]
    pub skills_defaults: SkillsDefaults,
    /// Application dependency providers (`[deps.<provider>]`). Install-gated for
    /// the same reason: the shim never materializes a dependency closure.
    #[cfg(feature = "install")]
    pub deps: DepsConfig,
    /// Tools present in configuration but excluded by their platform filter,
    /// mapped to the restriction that excluded them.
    ///
    /// Kept so a command that names such a tool can explain the absence. Without
    /// it the tool would look simply unknown, sending the user to check their
    /// spelling rather than the os/rch line that is doing its job.
    pub excluded_tools: BTreeMap<String, String>,
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
    /// Installer selection for `npm:` tools.
    pub npm: NpmSettings,
    /// Python catalog refresh and verification.
    pub python: PythonSettings,
    /// Java runtime metadata endpoint.
    pub java: JavaSettings,
    /// Pre-release resolution policy shared by supporting backends.
    pub prerelease: PrereleasePolicy,
    /// Which tools get a shim.
    pub shims: ShimSettings,
}

/// Which of an installed tool's executables get a shim.
///
/// Both lists are empty by default, which shims everything a tool exposes.
/// Some SDKs are legitimately large -- an Android NDK ships 172 executables,
/// one clang wrapper per API level -- and hiding them by default would break
/// the ordinary way of selecting a compiler, so narrowing is opt-in.
///
/// Patterns accept `*` and `?`, are matched case-insensitively, and may be
/// qualified with an owning backend (`android-ndk:*`) to narrow one tool
/// without naming each executable.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ShimSettings {
    /// When non-empty, only matching names are shimmed.
    ///
    /// This is an allowlist over **everything**, not a way to add one command
    /// back: setting it to a single name withholds every other tool on the
    /// machine. Reaching for it to recover one withheld command took a working
    /// setup from 646 shims to zero (see docs/bugs/008); `expose` is the
    /// additive setting for that job.
    pub include: Vec<String>,
    /// Names to skip. Applied after `include` and `expose`, so it always wins.
    pub exclude: Vec<String>,
    /// Names to shim *in addition to* the default decision.
    ///
    /// Additive and therefore safe: it never withholds anything, it only lifts
    /// a name that the ownership rules withheld on their own. A conda
    /// metapackage owns none of the commands in its prefix, so `make` from
    /// `conda:m2-base` needs asking for by name -- and asking must not imply
    /// "and nothing else".
    ///
    /// `exclude` still wins, so a broad `expose` remains trimmable.
    pub expose: Vec<String>,
    /// Per-tool overrides, keyed by backend id (`conda:m2-base`, `go`, ...).
    ///
    /// A tool's own lists are evaluated instead of the global ones, so narrowing
    /// one noisy SDK cannot silence unrelated tools -- the failure mode that
    /// made the global `include` dangerous. Keys are matched exactly (no globs)
    /// against the owning backend id; use the global lists for cross-tool
    /// patterns.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolShimSettings>,
}

/// One tool's shim policy, overriding the global lists for that tool only.
///
/// Every field is optional so a tool can adjust one dimension and inherit the
/// rest: `Some(vec![])` ("explicitly empty") is meaningfully different from
/// `None` ("not specified, use the global list").
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolShimSettings {
    /// Allowlist for this tool alone. Scoped, so it cannot affect other tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Names to skip for this tool alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// Additional names to shim for this tool alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeSettings {
    /// Run the installed Node's own `corepack enable` after installation.
    pub corepack: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NpmSettings {
    /// Installer used for `npm:` tools when nothing else decides.
    ///
    /// Auto-selection consults, in order, an explicit `-o installer=`, the
    /// project's declared `packageManager`, and the installer that owns an
    /// incumbent lockfile. This setting only supplies the final fallback, so
    /// changing it never overrides a project that states its own installer.
    /// Only concrete installers are meaningful here; `auto` would be circular
    /// and is rejected when the value is resolved.
    pub default_installer: NpmDefaultInstaller,
}

/// Concrete installer usable as the auto-selection fallback.
///
/// This is deliberately separate from [`crate::npm_tools::NpmInstaller`], which
/// also carries `Auto`. Keeping the configurable set in its own enum means a new
/// backend (pnpm, bun, ...) is added in one place and cannot accidentally make
/// `auto` its own fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NpmDefaultInstaller {
    #[default]
    Npm,
    Pnpm,
}

impl NpmDefaultInstaller {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
        }
    }
}

impl std::fmt::Display for NpmDefaultInstaller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for NpmDefaultInstaller {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "npm" => Ok(Self::Npm),
            "pnpm" => Ok(Self::Pnpm),
            other => Err(Error::config(crate::t!(
                "err.npm_default_installer_invalid",
                installer = other
            ))),
        }
    }
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
            npm: NpmSettings::default(),
            python: PythonSettings::default(),
            java: JavaSettings::default(),
            prerelease: PrereleasePolicy::default(),
            shims: ShimSettings::default(),
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
    /// Whether an ambient mirror environment variable is ranked alongside the
    /// built-in sources (`auto`) or obeyed on its own (`env`).
    pub mode: SourceMode,
    pub probe_timeout_ms: u64,
    /// Per-stage budget for model metadata, response headers, and sample reads.
    /// Model repositories are much heavier than SDK version indexes, so they
    /// need a separate default rather than inheriting the 1500 ms tool budget.
    pub model_probe_timeout_ms: u64,
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
    /// System-package configuration, persisted under the separate top-level
    /// `[syspkg]` table. It lives here internally so adding it does not break
    /// callers that construct [`Config`] directly.
    #[doc(hidden)]
    #[serde(skip)]
    #[cfg(feature = "install")]
    pub syspkg: crate::syspkg::SyspkgConfig,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        SourcesConfig {
            selection: Selection::Auto,
            mode: SourceMode::default(),
            probe_timeout_ms: 1500,
            model_probe_timeout_ms: 8000,
            cache_ttl: "6h".to_string(),
            per_tool: BTreeMap::new(),
            registries: RegistriesConfig::default(),
            containers: ContainersConfig::default(),
            #[cfg(feature = "install")]
            syspkg: crate::syspkg::SyspkgConfig::default(),
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
    pub python: PythonIndexConfig,
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

/// Candidate PyPI-compatible indexes, ranked by probe like SDK sources are.
///
/// These are mirrors of the *default* index only. A mirror is a full copy of
/// PyPI, so it necessarily carries the same package names as upstream --
/// including malicious ones. Ranking it above the default index (uv's
/// `--index` / `--extra-index-url`, pip's `--extra-index-url`) would turn a
/// convenience into a dependency-confusion vector, so osdk only ever maps a
/// mirror onto the default index. Private indexes that genuinely need higher
/// precedence are out of scope here and stay with the package manager's own
/// configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PythonIndexConfig {
    /// Ranked candidates. Empty means "use the built-in public default".
    pub urls: Vec<String>,
    pub probe_timeout_ms: u64,
}

impl Default for PythonIndexConfig {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            // Higher than the npm default of 1500 ms, which was copied here at
            // first and proved too tight: a project listing is a far larger
            // response than an npm ping. Measured from Beijing, the TUNA mirror
            // needed about 1.2 s for `/simple/numpy/` and pypi.org about 1.8 s,
            // so at 1500 ms a perfectly usable mirror was reported unreachable --
            // which then failed `latest` outright instead of merely being slow.
            probe_timeout_ms: 8000,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ContainerPlatform {
    #[default]
    Runtime,
    Explicit {
        os: String,
        arch: String,
        variant: Option<String>,
    },
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ToolConfigEntry {
    Legacy(String),
    Structured(StructuredToolConfig),
}

/// Deserialize by hand rather than with `#[serde(untagged)]`.
///
/// `untagged` discards the error from every variant it tried and reports only
/// "data did not match any variant", so a mistake inside the table -- a
/// misspelled `when.os`, an unsupported dimension -- surfaced as a parse error
/// pointing at the `[tools.<name>]` header with no mention of the real cause.
/// Deciding the variant from the value's own shape lets the inner error through.
impl<'de> Deserialize<'de> for ToolConfigEntry {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(version) => Ok(Self::Legacy(version)),
            other => StructuredToolConfig::deserialize(other)
                .map(Self::Structured)
                .map_err(serde::de::Error::custom),
        }
    }
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
            when: None,
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
    /// Platform restriction for this entry. `None` means "every platform".
    ///
    /// A typed field rather than one of the flattened `options`, so that it is
    /// validated when the config is read and cannot be mistaken for a backend
    /// option. `deny_unknown_fields` on the inner table makes a not-yet-supported
    /// dimension fail loudly instead of widening the filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<crate::platform::PlatformFilter>,
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

/// The option key holding a platform filter rather than backend options.
///
/// It is removed from `options` before anything reaches a backend: dynamic
/// backends reject unknown options outright, so leaving it in place would turn
/// every filtered entry into `unsupported option `when`` instead of a filter.
///
/// Nested under one key rather than flat `os`/`arch` because both of those are
/// already backend options meaning "which artifact to download"; see
/// [`crate::platform::PlatformFilter`] for what went wrong when they were
/// reused.
pub const PLATFORM_FILTER_KEY: &str = "when";

impl StructuredToolConfig {
    /// Split this entry's `when` filter out from its backend options.
    ///
    /// Returns the filter plus the options with `when` removed. An unrecognized
    /// token is an error rather than a filter that matches nothing, for the same
    /// reason as in `[syspkg.packages]`: silently never matching would remove the
    /// tool on every machine, and the symptom ("the tool is missing") points
    /// nowhere near the misspelled line.
    pub fn split_platform_filter(
        &self,
    ) -> Result<(
        crate::platform::PlatformFilter,
        BTreeMap<String, ToolConfigValue>,
    )> {
        let filter = match self.when.as_ref() {
            Some(filter) => filter.clone(),
            None => crate::platform::PlatformFilter::default(),
        };
        let options = self
            .options
            .iter()
            .filter(|(key, _)| key.as_str() != PLATFORM_FILTER_KEY)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok((filter, options))
    }
}

impl ToolConfigEntry {
    /// This entry's platform filter, and the entry with the filter keys removed.
    ///
    /// A legacy string entry carries no filter and is returned unchanged, so the
    /// `fd = "npm:fd@10"` spelling keeps working exactly as before.
    pub fn split_platform_filter(&self) -> Result<(crate::platform::PlatformFilter, Self)> {
        match self {
            Self::Legacy(_) => Ok((crate::platform::PlatformFilter::default(), self.clone())),
            Self::Structured(config) => {
                let (filter, options) = config.split_platform_filter()?;
                Ok((
                    filter,
                    Self::Structured(StructuredToolConfig {
                        version: config.version.clone(),
                        // The filter has been extracted; the stripped entry
                        // must not carry it again or a second split would
                        // re-apply it.
                        when: None,
                        options,
                    }),
                ))
            }
        }
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

/// `[deps]`: which application dependency providers this project uses.
///
/// Deliberately separate from `[tools]`: `[tools]` installs the *package
/// manager*, `[deps]` installs the *project's packages*. Mixing them is how a
/// design ends up treating "install pnpm" and "install this project's
/// dependencies" as one action, which they are not -- they have different
/// targets (isolated install dir vs the project), different sources of truth
/// (osdk.lock vs the native lockfile) and different triggers.
///
/// Note the absence of `deny_unknown_fields` on *this* table: it is incompatible
/// with `#[serde(flatten)]`, because the deny check runs before flatten can
/// absorb the key, so every provider name would be rejected as unknown. The
/// flattened map is the intended catch-all here; the protection against typos
/// lives one level down, on [`DepsProviderEntry`], where a misspelled field is a
/// real hazard.
#[cfg(feature = "install")]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DepsConfig {
    /// Providers disabled here even if a broader layer enabled them. Disabling
    /// stops the provider from running; it does not uninstall anything.
    #[serde(default)]
    pub disable: Vec<String>,
    /// Sub-project directories to look inside, as patterns relative to this
    /// config: `roots = ["apps/*", "packages/*"]`.
    ///
    /// Nothing is discovered below the config root without an entry here. That is
    /// the whole point of the field: the set of sub-projects osdk may act on has
    /// to be the set the project declared, not whatever a subtree walk turns up.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Provider entries, keyed by provider id (`npm`, `pnpm`, ...). An empty
    /// table is enough to select the built-in provider: `auto` defaults to true,
    /// so declaring the provider is also opting into automatic materialization.
    #[serde(flatten)]
    pub providers: BTreeMap<String, DepsProviderEntry>,
}

// Gated with the type it implements: \DepsProviderEntry\ only exists behind
// \install\, and an ungated impl makes the shim build fail to compile rather than
// merely grow. Caught by building the shim separately for the size measurement.
#[cfg(feature = "install")]
impl Default for DepsProviderEntry {
    /// Written out rather than derived, because `derive` would give
    /// `auto: false` while the field's serde default is `true` -- and then a
    /// programmatically built entry would not mean what a parsed one means. That
    /// divergence is invisible until a test constructs an entry and concludes
    /// something false about real config.
    fn default() -> Self {
        Self {
            auto: default_true(),
            sources: Vec::new(),
            outputs: None,
            run: None,
            env: BTreeMap::new(),
            dir: None,
            depends: Vec::new(),
            timeout: None,
            installer: None,
            index: None,
            extra_index: None,
            allow_build_from_source: false,
        }
    }
}

#[cfg(feature = "install")]
fn default_true() -> bool {
    true
}

/// One `[deps.<provider>]` entry.
///
/// `deny_unknown_fields` is the point: a misspelled `output` or `allow_builds`
/// must fail loudly. Silently ignoring `allow_build_from_source` would be the
/// worst case -- the user would believe build scripts are enabled (or disabled)
/// while the opposite holds.
#[cfg(feature = "install")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepsProviderEntry {
    /// Materialize this provider's dependencies before a bare `install`, `run`
    /// or `exec`.
    ///
    /// Defaults to **true**. The freshness check in front of it is a cache hit in
    /// the common case and costs well under a millisecond for an ordinary lock
    /// (measured: 0.16ms at 20KiB, 0.39ms at 200KiB, 2.06ms for a 2MiB monorepo
    /// lock), so there is nothing meaningful to save by making the user opt in --
    /// while a project whose dependencies are quietly out of date is exactly the
    /// failure this prevents.
    ///
    /// Two ways out: `--no-deps` for a single command, `auto = false` to declare
    /// it for a provider.
    #[serde(default = "default_true")]
    pub auto: bool,
    /// Freshness inputs. Replaces the provider's built-in list rather than
    /// adding to it.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Tracked outputs. Replaces the built-in list; an explicit empty list
    /// disables output tracking on purpose.
    pub outputs: Option<Vec<String>>,
    /// Override the command entirely.
    pub run: Option<String>,
    /// Extra environment for the install command.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory relative to the config root (for a nested project).
    pub dir: Option<String>,
    /// Providers that must finish first. Orders configured providers; it does
    /// not declare a provider that is not configured.
    #[serde(default)]
    pub depends: Vec<String>,
    /// Timeout for the install command, e.g. "5m".
    pub timeout: Option<String>,
    /// Pin the installer explicitly, overriding automatic selection.
    pub installer: Option<String>,
    /// Registry/index override. Its presence is what makes this entry
    /// trust-requiring as `WeakensVerification` -- it redirects where bytes come
    /// from (see `trust::collect_deps_requirements`).
    pub index: Option<String>,
    /// Additional index. Same trust class as `index`, and never ranked above the
    /// default one.
    pub extra_index: Option<String>,
    /// Allow building from source / running lifecycle scripts. `ExecutesCode`:
    /// default denied, must be asked for.
    pub allow_build_from_source: bool,
}

/// One declared project model (`[models.<name>]`, research §6.2).
///
/// `deny_unknown_fields` is deliberate: a typo in `source`/`endpoint` must fail
/// loudly rather than being silently ignored while the model never materializes.
/// View sub-tables are free-form category maps (repo prefix -> consumer
/// category), validated by the view renderer rather than the config parser.
#[cfg(feature = "install")]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDeclaration {
    /// Provider reference, e.g. `hf:owner/repo@main`.
    pub source: String,
    /// Glob include patterns applied at pull time.
    #[serde(default)]
    pub include: Vec<String>,
    /// Glob exclude patterns.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Format/quantization label recorded into the snapshot identity.
    pub variant: Option<String>,
    /// Optional platform filter (same `when` shape as tools).
    pub when: Option<crate::platform::PlatformFilter>,
    /// Consumer views: consumer (e.g. "comfyui") -> its declaration.
    #[serde(default)]
    pub views: BTreeMap<String, ModelViewDeclaration>,
    /// Explicit endpoint override. Its presence is what makes this entry
    /// trust-requiring (see `trust::collect_models_requirements`).
    pub endpoint: Option<String>,
}

/// One consumer view attached to a `[models.<name>]` entry (research §6.2).
///
/// ```toml
/// [models.flux.views.comfyui]
/// profile = "default"
/// map = { "unet/" = "diffusion_models", "vae/" = "vae" }
/// ```
///
/// `profile` defaults to "default"; `map` is a repo-relative path prefix ->
/// consumer category table, normalized to `/` by the consumer layer.
#[cfg(feature = "install")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelViewDeclaration {
    pub profile: String,
    #[serde(default)]
    pub map: BTreeMap<String, String>,
}

#[cfg(feature = "install")]
impl Default for ModelViewDeclaration {
    fn default() -> Self {
        Self {
            profile: "default".to_string(),
            map: BTreeMap::new(),
        }
    }
}

/// One declared agent skill (`[skills.<name>]`).
///
/// Shaped like [`ModelDeclaration`]: it states *what* skill to install and from
/// *where*, and only the byte-source key (`endpoint`) makes the entry
/// trust-requiring (see `trust::collect_skills_requirements`). `deny_unknown_fields`
/// makes a typo fail loudly rather than silently skip a skill.
#[cfg(feature = "install")]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillDeclaration {
    /// Source reference, e.g. `github:vercel-labs/agent-skills`.
    pub source: String,
    /// When the repository holds several skills, which one to install.
    pub skill: Option<String>,
    /// Version selector for the source, e.g. `branch:main` or `rev:<sha>`.
    /// Resolved to an immutable commit that is pinned into `osdk.lock`.
    pub r#ref: Option<String>,
    /// Which agents to install into; empty falls back to `[skills].default_agents`.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Optional platform filter (same `when` shape as tools and models).
    pub when: Option<crate::platform::PlatformFilter>,
    /// Custom source endpoint. Its presence is what makes this entry
    /// trust-requiring (`WeakensVerification`), matching `[models].endpoint`.
    pub endpoint: Option<String>,
}

/// Top-level `[skills]` defaults, beside the per-skill `[skills.<name>]` entries
/// the way `[deps]` top-level keys sit beside `[deps.<provider>]`.
///
/// Every field is optional: an empty `[skills]` table means "use built-in
/// defaults", and declaring only `[skills.<name>]` entries never needs this block.
#[cfg(feature = "install")]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsDefaults {
    /// Agents an `add`/`sync` targets when a skill names none of its own.
    #[serde(default)]
    pub default_agents: Vec<String>,
    /// Install scope: `project` (default) or `global`.
    pub scope: Option<String>,
    /// Link-mode override for staged skills only, e.g. `symlink` / `copy`.
    pub link_mode: Option<String>,
}

/// The `[skills]` table as written in a file: optional top-level defaults plus a
/// flattened map of named skill entries.
///
/// Split from [`SkillsDefaults`] so the top-level keys stay `deny_unknown_fields`
/// while the named entries flatten in beside them, exactly as `[deps]` combines
/// `disable`/`roots` with `[deps.<provider>]`.
#[cfg(feature = "install")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SkillsFile {
    #[serde(flatten)]
    defaults: SkillsDefaults,
    #[serde(flatten)]
    entries: BTreeMap<String, SkillDeclaration>,
}
/// On-disk config file shape (a subset that users edit).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ConfigFile {
    settings: Option<Settings>,
    sources: Option<SourcesConfig>,
    registries: Option<RegistriesConfig>,
    containers: Option<ContainersConfig>,
    #[cfg(feature = "install")]
    syspkg: Option<crate::syspkg::SyspkgConfig>,
    tools: BTreeMap<String, ToolConfigEntry>,
    aliases: BTreeMap<String, BTreeMap<String, String>>,
    #[cfg(feature = "install")]
    tasks: BTreeMap<String, crate::tasks::TaskEntry>,
    #[cfg(feature = "install")]
    models: BTreeMap<String, ModelDeclaration>,
    /// The `[skills]` table: top-level defaults flattened with named entries.
    #[cfg(feature = "install")]
    skills: SkillsFile,
    #[cfg(feature = "install")]
    deps: Option<DepsConfig>,
    #[cfg(feature = "install")]
    task_config: Option<crate::tasks::TaskConfig>,
}

/// An empty configuration: built-in defaults, nothing declared.
///
/// Hand-written rather than derived because `Config` is assembled field by
/// field in `load_layers_internal`, and dozens of tests construct one to stand
/// in for "no config". Without this they each spell out every field, so adding
/// one field breaks all of them at once and the fix is 40 identical diffs.
impl Default for Config {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: BTreeMap::new(),
            tool_configs: BTreeMap::new(),
            global_tools: BTreeMap::new(),
            global_tool_configs: BTreeMap::new(),
            tool_origins: BTreeMap::new(),
            aliases: BTreeMap::new(),
            #[cfg(feature = "install")]
            tasks: crate::tasks::TaskSet::default(),
            #[cfg(feature = "install")]
            models: BTreeMap::new(),
            #[cfg(feature = "install")]
            skills: BTreeMap::new(),
            #[cfg(feature = "install")]
            skills_defaults: SkillsDefaults::default(),
            #[cfg(feature = "install")]
            deps: DepsConfig::default(),
            project_config_path: None,
            excluded_tools: BTreeMap::new(),
        }
    }
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

    fn apply_file(
        &mut self,
        file: ConfigFile,
        allow_model_env: bool,
        origin: Option<&Path>,
    ) -> Result<()> {
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
                mode: src.mode,
                probe_timeout_ms: src.probe_timeout_ms,
                model_probe_timeout_ms: src.model_probe_timeout_ms,
                cache_ttl: src.cache_ttl,
                per_tool: merged,
                registries: self.sources.registries.clone(),
                containers: self.sources.containers.clone(),
                #[cfg(feature = "install")]
                syspkg: self.sources.syspkg.clone(),
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
        #[cfg(feature = "install")]
        if let Some(syspkg) = file.syspkg {
            // Replaced as a unit for the same reason: a project that lists its
            // managers means exactly that list, not that list added to whatever
            // a broader layer happened to allow.
            self.sources.syspkg = syspkg;
        }
        #[cfg(feature = "install")]
        if let Some(deps) = file.deps {
            // Replaced as a unit, like `[registries]` and `[syspkg]`: a project
            // that lists its providers means exactly that list, not that list
            // added to whatever a broader layer happened to enable.
            self.deps = deps;
        }
        #[cfg(feature = "install")]
        if !file.models.is_empty() {
            // Project declarations replace the lower-precedence layer as a
            // unit, like tools: the project is the more specific statement of
            // which models it needs.
            self.models.extend(file.models);
        }
        #[cfg(feature = "install")]
        {
            // Named skills extend/replace by name, like models. Top-level
            // `[skills]` defaults replace as a unit only when the layer states
            // them, so a project block does not silently blank a user default.
            self.skills.extend(file.skills.entries);
            let defaults = file.skills.defaults;
            if !defaults.default_agents.is_empty() {
                self.skills_defaults.default_agents = defaults.default_agents;
            }
            if defaults.scope.is_some() {
                self.skills_defaults.scope = defaults.scope;
            }
            if defaults.link_mode.is_some() {
                self.skills_defaults.link_mode = defaults.link_mode;
            }
        }
        self.apply_tool_configs(&file.tools)?;
        for (tool, aliases) in file.aliases {
            self.aliases.entry(tool).or_default().extend(aliases);
        }
        #[cfg(feature = "install")]
        {
            // Same-name tasks replace whole; `[task_config]` replaces as a unit,
            // matching `[registries]`. See `tasks::TaskSet::apply`.
            let includes = file
                .task_config
                .as_ref()
                .map(|config| config.includes.clone())
                .unwrap_or_default();
            if let Some(config) = file.task_config {
                self.tasks.apply_config(config);
            }
            let base = origin.and_then(Path::parent);
            // Scripts first, so a `[tasks]` entry of the same name wins: the
            // TOML is the more specific statement, and a file appearing in the
            // directory should not quietly shadow it.
            if let Some(base) = base {
                self.tasks
                    .apply_files(&crate::tasks::files::discover(base, &includes)?);
            }
            self.tasks.apply_from(file.tasks, base)?;

            // Sub-projects named by `[task_config].roots`. After the layer's own
            // `[task_config]`, because that is what states the roots.
            if let Some(base) = base {
                self.apply_rooted_tasks(base)?;
            }
        }
        Ok(())
    }

    /// Merge the tasks of every sub-project `[task_config].roots` names.
    ///
    /// Each sub-project's tasks enter the shared set as `//<relative>:<name>`, the
    /// same addressing `[deps].roots` uses for providers. Three properties are
    /// deliberate:
    ///
    /// * **Only declared roots are read.** `deps::expand_roots` is shared rather than
    ///   reimplemented, so "nothing outside the declared set is reached" stays one
    ///   guarantee instead of two that can drift. The stake is higher here than for
    ///   deps: a `run` line is an arbitrary command, so discovering one by accident
    ///   is discovering code to execute by accident.
    /// * **A sub-project without an `osdk.toml` is not an error.** A root pattern
    ///   says where projects live, not that every one of them configures osdk.
    /// * **A malformed one fails the whole load.** Same fail-closed rule as a broken
    ///   dependency manifest inside a root: skipping it would mean running without
    ///   tasks the project declared, and saying nothing about it.
    #[cfg(feature = "install")]
    fn apply_rooted_tasks(&mut self, config_root: &Path) -> Result<()> {
        if self.tasks.config.roots.is_empty() {
            return Ok(());
        }
        let roots = self.tasks.config.roots.clone();
        for expanded in crate::deps::expand_roots(config_root, &roots, "[task_config].roots")? {
            let Some(path) = PROJECT_CONFIG_NAMES
                .iter()
                .map(|name| expanded.directory.join(name))
                .find(|candidate| candidate.is_file())
            else {
                continue;
            };
            // `read_config_file` rather than a second parser: it already attributes
            // errors to the file that caused them, which matters most here -- a parse
            // error blamed on the monorepo root would send the reader to the wrong
            // file entirely.
            let file = read_config_file(&path)?;

            // A sub-project contributes task *definitions* and nothing else.
            //
            // `[task_config]` holds ambient defaults, and `shell` among them decides
            // which interpreter every task in scope runs under -- a
            // `shell = "evil --run"` makes every later `osdk run` do something other
            // than what the task text says, with nothing at the call site to reveal
            // it. That is why the root's `[task_config]` sits in
            // `TRUST_REQUIRING_TABLES` as `RedirectsExecution`.
            //
            // The alternative was to route sub-configs through that same gate. It was
            // rejected: trust is keyed on a file the user approved, so this would need
            // one approval per sub-project -- a gate per package in a monorepo, for a
            // field most projects never set. Removing the capability is both stricter
            // and quieter than gating it.
            //
            // Rejected rather than ignored. A silently ineffective setting is worse
            // than an error: the user wrote it, sees no complaint, and concludes it
            // took effect.
            if file.task_config.is_some() {
                return Err(Error::config(format!(
                    "{}: a sub-project cannot declare `[task_config]`; runner defaults \
                     such as `shell` belong to the config that declares \
                     `[task_config].roots`",
                    path.display()
                )));
            }

            let prefix = format!("//{}:", expanded.relative);
            let prefixed = file
                .tasks
                .into_iter()
                .map(|(name, entry)| (format!("{prefix}{name}"), qualify_entry(entry, &prefix)))
                .collect();
            // The base is the sub-project's own directory, so its `file = "x.sh"`
            // resolves against itself rather than against the monorepo root.
            self.tasks.apply_from(prefixed, path.parent())?;
        }
        Ok(())
    }

    /// Merge one layer's `[tools]`, dropping entries whose platform filter does
    /// not match this host.
    ///
    /// Filtering here rather than at each use site is what makes the semantics
    /// "the entry does not exist": every downstream consumer -- resolution, lock,
    /// shims, activation -- reads `tools`/`tool_configs` and so cannot
    /// accidentally act on an entry meant for another platform.
    ///
    /// A filtered-out name is remembered so a command that names it explicitly
    /// can say why it is absent instead of reporting an unknown tool.
    fn apply_tool_configs(&mut self, tools: &BTreeMap<String, ToolConfigEntry>) -> Result<()> {
        let host = crate::platform::Platform::current();
        for (tool, entry) in tools {
            // The inner error is already a config error, so its own message is
            // reused rather than wrapped: `Error::config` adds the "config
            // error" prefix, and nesting produced it twice.
            let (filter, entry) = entry
                .split_platform_filter()
                .map_err(|error| Error::config(format!("tool `{tool}`: {error}")))?;
            if !filter.matches(&host) {
                // A lower layer may have declared the same tool without a
                // filter; this layer excluding it must not resurrect that one.
                self.tools.remove(tool);
                self.tool_configs.remove(tool);
                self.excluded_tools.insert(tool.clone(), filter.describe());
                continue;
            }
            self.excluded_tools.remove(tool);
            self.tools.insert(tool.clone(), entry.version().to_string());
            self.tool_configs.insert(tool.clone(), entry);
        }
        Ok(())
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
        if let Some(v) = getenv("OSDK_NPM_DEFAULT_INSTALLER") {
            if let Ok(installer) = v.parse() {
                self.settings.npm.default_installer = installer;
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
        if let Some(v) = getenv("OSDK_SOURCE_MODE") {
            if let Some(mode) = SourceMode::parse(&v) {
                self.sources.mode = mode;
            }
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
        normalize_python_index_urls(&mut registries.python.urls)?;
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
        if normalized.len() > 8 {
            return Err(Error::config(
                "container registry policy supports at most 8 mirrors",
            ));
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
    let mut cfg = Config::default();

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
        cfg.apply_file(file, true, Some(user_config_file))?;
    }

    if let Some(start_dir) = start_dir {
        if let Some((path, file)) = find_project_config(start_dir)? {
            cfg.tool_origins.extend(
                file.tools
                    .keys()
                    .map(|tool| (tool.clone(), ToolConfigOrigin::ProjectConfig(path.clone()))),
            );
            cfg.apply_file(file, false, Some(&path))?;
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

fn normalize_python_index_urls(urls: &mut Vec<String>) -> Result<()> {
    let mut normalized = Vec::with_capacity(urls.len());
    for value in urls.iter() {
        let value = normalize_python_index_url(value)?;
        if !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    *urls = normalized;
    Ok(())
}

/// Validate and canonicalize a PEP 503 simple-index base URL.
///
/// Stricter than [`normalize_registry_url`] in one deliberate way: plaintext
/// `http` is refused. An index is where package hashes come from, so a
/// downgradeable transport would let an attacker rewrite both the artifact and
/// the hash that is supposed to detect the rewrite. Credentials are refused
/// too -- they would otherwise reach logs and config files.
pub fn normalize_python_index_url(value: &str) -> Result<String> {
    let original = value.trim();
    let mut url =
        reqwest::Url::parse(original).map_err(|_| Error::config("invalid Python index URL"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(Error::config(
            "Python index URL must use https and include a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::config(
            "Python index URL must not contain credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::config(
            "Python index URL must not contain a query string or fragment",
        ));
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{path}/"));
    Ok(url.to_string())
}

/// Rewrite a sub-project task's references to its own siblings.
///
/// A sub-project's tasks are stored as `//<relative>:<name>`, so a bare
/// `depends = ["prep"]` written inside that file would otherwise be looked up as a
/// global name and fail as unknown -- a config that was correct on its own terms
/// breaking purely because it became a sub-project, with an error pointing at the
/// dependency rather than at the rewrite that lost it.
///
/// So a bare name resolves within the declaring sub-project, which is both what the
/// file appears to say and the convention mise settles on. An entry already starting
/// with `//` is absolute and left untouched, which is how one sub-project depends on
/// another.
#[cfg(feature = "install")]
fn qualify_entry(entry: crate::tasks::TaskEntry, prefix: &str) -> crate::tasks::TaskEntry {
    fn qualify(names: &mut [String], prefix: &str) {
        for name in names.iter_mut() {
            if !name.starts_with("//") {
                *name = format!("{prefix}{name}");
            }
        }
    }

    let mut entry = entry;
    if let crate::tasks::TaskEntry::Full(def) = &mut entry {
        qualify(&mut def.depends, prefix);
        qualify(&mut def.wait_for, prefix);
    }
    entry
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
            excluded_tools: Default::default(),
            ..Default::default()
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
            excluded_tools: Default::default(),
            ..Default::default()
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
            excluded_tools: Default::default(),
            ..Default::default()
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

        let mirrors = (0..9)
            .map(|index| format!("\"https://mirror-{index}.example/\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            &config_file,
            format!("[containers.registries.\"docker.io\"]\nmirrors = [{mirrors}]\n"),
        )
        .unwrap();
        let error = Config::load_user(&config_file).unwrap_err();
        assert!(error.to_string().contains("at most 8 mirrors"));
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
    fn python_index_urls_require_https_and_reject_credentials() {
        assert_eq!(
            normalize_python_index_url("https://pypi.org/simple").unwrap(),
            "https://pypi.org/simple/"
        );
        // Trailing slash is idempotent.
        assert_eq!(
            normalize_python_index_url("https://pypi.org/simple/").unwrap(),
            "https://pypi.org/simple/"
        );

        // An index supplies the hashes used to verify artifacts, so plaintext
        // transport is refused outright -- unlike npm registries, which still
        // accept http.
        let http = normalize_python_index_url("http://pypi.org/simple").unwrap_err();
        assert_eq!(
            http.to_string(),
            "config error: Python index URL must use https and include a host"
        );
        assert!(normalize_registry_url("http://registry.test/").is_ok());

        let credentials =
            normalize_python_index_url("https://user:pass@mirror.test/simple").unwrap_err();
        assert_eq!(
            credentials.to_string(),
            "config error: Python index URL must not contain credentials"
        );

        for rejected in [
            "https://mirror.test/simple?token=1",
            "https://mirror.test/simple#frag",
            "file:///tmp/simple",
            "relative/simple",
        ] {
            assert!(
                normalize_python_index_url(rejected).is_err(),
                "expected `{rejected}` to be refused"
            );
        }
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

    /// `os`/`arch` on a `[tools]` entry filter it rather than reaching the
    /// backend as options.
    ///
    /// The second half matters as much as the first: dynamic backends reject
    /// unknown options, so an `os` key left in `options` would turn every
    /// filtered entry into `unsupported option \`os\`` instead of a filter.
    #[test]
    fn tool_platform_filter_is_split_out_of_backend_options() {
        let entry: ToolConfigEntry = toml::from_str(
            "version = \"1.2.3\"\ninstaller = \"pnpm\"\n\n[when]\nos = \"windows\"\narch = [\"arm64\", \"x64\"]\n",
        )
        .unwrap();
        let (filter, stripped) = entry.split_platform_filter().unwrap();

        assert_eq!(filter.os, vec![crate::platform::Os::Windows]);
        assert_eq!(
            filter.arch,
            vec![crate::platform::Arch::Arm64, crate::platform::Arch::X64]
        );

        // The backend must still see its own option, and must not see ours.
        let options = stripped.to_request_options();
        assert_eq!(options.get("installer").map(String::as_str), Some("pnpm"));
        assert!(!options.contains_key("os"), "{options:?}");
        assert!(!options.contains_key("arch"), "{options:?}");
        assert_eq!(stripped.version(), "1.2.3");
    }

    /// A legacy string entry has no filter and is passed through untouched, so
    /// `fd = "npm:fd@10"` keeps behaving exactly as before.
    #[test]
    fn a_legacy_string_entry_is_never_filtered() {
        let entry = ToolConfigEntry::legacy("npm:fd@10");
        let (filter, stripped) = entry.split_platform_filter().unwrap();
        assert!(filter.is_unrestricted());
        assert_eq!(stripped, entry);
    }

    /// A misspelled token is an error, not a filter that matches nothing.
    #[test]
    fn a_misspelled_tool_platform_token_is_rejected() {
        // Rejection happens while reading the config, not later: `when` is a
        // typed field, so an unusable token never becomes a `PlatformFilter` at
        // all. The message must name both the offending token and the accepted
        // set, or the author is left guessing which of the two keys is wrong.
        for (body, needle) in [
            ("version = \"1\"\n[when]\nos = \"windwos\"\n", "windwos"),
            ("version = \"1\"\n[when]\narch = \"arm65\"\n", "arm65"),
        ] {
            let error = toml::from_str::<ToolConfigEntry>(body)
                .expect_err(&format!("should be rejected: {body}"));
            let message = error.to_string();
            assert!(message.contains(needle), "{message}");
            assert!(
                message.contains("expected one of"),
                "must list accepted tokens: {message}"
            );
        }

        // A dimension no build understands must not be quietly ignored, which
        // would widen the filter to "every libc" -- the opposite of the intent.
        let error = toml::from_str::<ToolConfigEntry>("version = \"1\"\n[when]\nlibc = \"musl\"\n")
            .expect_err("an unknown `when` dimension must be rejected");
        assert!(error.to_string().contains("libc"), "{error}");
    }

    /// The filter decides membership of the merged `tools` map, so every
    /// downstream consumer sees a non-matching entry as simply absent.
    ///
    /// Two entries, not one: with a single entry a bug that keeps everything and
    /// a bug that drops everything are equally consistent with the result.
    #[test]
    fn a_non_matching_entry_is_absent_from_the_merged_config() {
        let host = crate::platform::Platform::current();
        let other_os = match host.os {
            crate::platform::Os::Windows => "linux",
            _ => "windows",
        };

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        std::fs::write(
            project.join("osdk.toml"),
            format!(
                "[tools.kept]\nversion = \"1\"\nwhen = {{ os = \"{}\" }}\n\n[tools.dropped]\nversion = \"2\"\nwhen = {{ os = \"{other_os}\" }}\n",
                host.os.config_token()
            ),
        )
        .unwrap();

        let config = Config::load(&temporary.path().join("missing.toml"), project).unwrap();
        assert!(config.tools.contains_key("kept"), "{:?}", config.tools);
        assert!(!config.tools.contains_key("dropped"), "{:?}", config.tools);
        assert!(!config.tool_configs.contains_key("dropped"));

        // And the exclusion is recorded with its reason, so a command naming it
        // can explain the absence instead of reporting an unknown tool.
        assert!(config.excluded_tools.contains_key("dropped"));
        assert!(config.excluded_tools["dropped"].contains(other_os));
        assert!(!config.excluded_tools.contains_key("kept"));
    }

    /// `os` and `arch` are AND, not OR: matching one is not enough.
    #[test]
    fn os_and_arch_must_both_match_for_a_tool_entry() {
        let host = crate::platform::Platform::current();
        let other_arch = match host.arch {
            crate::platform::Arch::Arm64 => "x64",
            _ => "arm64",
        };

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        std::fs::write(
            project.join("osdk.toml"),
            format!(
                "[tools.matching_os_only]\nversion = \"1\"\nwhen = {{ os = \"{}\", arch = \"{other_arch}\" }}\n",
                host.os.config_token()
            ),
        )
        .unwrap();

        let config = Config::load(&temporary.path().join("missing.toml"), project).unwrap();
        assert!(
            !config.tools.contains_key("matching_os_only"),
            "a matching os must not be enough when arch differs: {:?}",
            config.tools
        );
    }

    /// A project layer excluding a tool must not fall back to a global entry
    /// that had no filter. The narrower layer said "not here", and resurrecting
    /// the broader one would install exactly what was ruled out.
    #[test]
    fn an_excluded_project_entry_does_not_fall_back_to_the_global_layer() {
        let host = crate::platform::Platform::current();
        let other_os = match host.os {
            crate::platform::Os::Windows => "linux",
            _ => "windows",
        };

        let temporary = tempfile::tempdir().unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\nsometool = \"1\"\n").unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            format!("[tools.sometool]\nversion = \"2\"\nwhen = {{ os = \"{other_os}\" }}\n"),
        )
        .unwrap();

        let config = Config::load(&user_config, &project).unwrap();
        assert!(
            !config.tools.contains_key("sometool"),
            "project exclusion must win over the unfiltered global pin: {:?}",
            config.tools
        );
        assert!(config.excluded_tools.contains_key("sometool"));
    }

    /// A backend's own `os`/`arch`/`libc` options must keep meaning "which
    /// artifact to download", not "where this entry applies".
    ///
    /// This is the regression that made `when` a nested table. `github:` and
    /// `node` have had `arch` as a real option for as long as cross-architecture
    /// locking has existed: `osdk lock node@20 -o arch=arm64` produces a lock
    /// section for another machine. A first version of this feature read a flat
    /// `arch` key as the platform filter, so `[tools.node] arch = "arm64"` made
    /// node vanish from the merged config on an x64 host -- silently, with the
    /// tool simply reported as absent.
    #[test]
    fn a_backends_own_arch_option_is_not_a_platform_filter() {
        let host = crate::platform::Platform::current();
        let other_arch = match host.arch {
            crate::platform::Arch::Arm64 => "x64",
            _ => "arm64",
        };

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        std::fs::write(
            project.join("osdk.toml"),
            format!("[tools.node]\nversion = \"20\"\narch = \"{other_arch}\"\n"),
        )
        .unwrap();

        let config = Config::load(&temporary.path().join("missing.toml"), project).unwrap();
        assert!(
            config.tools.contains_key("node"),
            "a backend `arch` option must not filter the entry out: {:?} / excluded {:?}",
            config.tools,
            config.excluded_tools
        );
        assert!(
            config.excluded_tools.is_empty(),
            "{:?}",
            config.excluded_tools
        );
        // And it must still reach the backend as an option.
        let options = config.tool_configs["node"].to_request_options();
        assert_eq!(options.get("arch").map(String::as_str), Some(other_arch));
    }

    /// The two vocabularies must be usable together on one entry: download for
    /// another architecture, yet only when this host matches.
    #[test]
    fn a_when_filter_and_an_arch_option_coexist_on_one_entry() {
        let host = crate::platform::Platform::current();
        let other_arch = match host.arch {
            crate::platform::Arch::Arm64 => "x64",
            _ => "arm64",
        };

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        std::fs::write(
            project.join("osdk.toml"),
            format!(
                "[tools.node]\nversion = \"20\"\narch = \"{other_arch}\"\nwhen = {{ os = \"{}\" }}\n",
                host.os.config_token()
            ),
        )
        .unwrap();

        let config = Config::load(&temporary.path().join("missing.toml"), project).unwrap();
        assert!(config.tools.contains_key("node"), "{:?}", config.tools);
        let options = config.tool_configs["node"].to_request_options();
        // The filter is consumed; the backend option survives untouched.
        assert_eq!(options.get("arch").map(String::as_str), Some(other_arch));
        assert!(!options.contains_key("when"), "{options:?}");
    }

    /// A dimension this build does not implement must fail loudly.
    ///
    /// Accepting and ignoring `libc` would widen the filter to "every libc",
    /// which is the opposite of what the author asked for, and the entry would
    /// install somewhere it was explicitly excluded from.
    #[test]
    fn an_unimplemented_when_dimension_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        std::fs::write(
            project.join("osdk.toml"),
            "[tools.sometool]\nversion = \"1\"\nwhen = { libc = \"musl\" }\n",
        )
        .unwrap();

        let error = Config::load(&temporary.path().join("missing.toml"), project)
            .expect_err("an unknown `when` dimension must be rejected");
        let message = error.localized();
        assert!(message.contains("libc"), "{message}");
    }

    #[test]
    fn models_section_parses_with_views_and_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("config.toml");
        std::fs::write(
            &config_file,
            r#"
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"
include = ["*.safetensors", "*.json"]
exclude = ["*.onnx"]
variant = "fp16"
when = { os = "windows" }

[models.flux.views.comfyui]
profile = "desktop"
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/" = "vae"

[models.plain]
source = "hf:o/r@main"
"#,
        )
        .unwrap();

        let config = Config::load_user(&config_file).unwrap();
        let flux = config.models.get("flux").expect("flux parsed");
        assert_eq!(flux.source, "hf:black-forest-labs/FLUX.1-dev@main");
        assert_eq!(flux.include, vec!["*.safetensors", "*.json"]);
        assert_eq!(flux.exclude, vec!["*.onnx"]);
        assert_eq!(flux.variant.as_deref(), Some("fp16"));
        assert!(flux.endpoint.is_none());
        assert_eq!(flux.when.as_ref().unwrap().os.len(), 1);
        let comfyui = flux.views.get("comfyui").expect("comfyui view parsed");
        assert_eq!(comfyui.profile, "desktop");
        assert_eq!(
            comfyui.map.get("unet/").map(String::as_str),
            Some("diffusion_models")
        );
        // A view block omitted entirely still deserializes as empty; a view with
        // no explicit profile defaults to "default".
        assert!(config.models.get("plain").unwrap().views.is_empty());
    }

    #[test]
    fn models_view_profile_defaults_to_default_and_unknown_key_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();

        // Default profile.
        let with_profile = temporary.path().join("p.toml");
        std::fs::write(
            &with_profile,
            "[models.m]\nsource = \"hf:o/r@main\"\n[models.m.views.comfyui]\n",
        )
        .unwrap();
        let config = Config::load_user(&with_profile).unwrap();
        assert_eq!(config.models["m"].views["comfyui"].profile, "default");

        // Unknown model key is denied loudly (deny_unknown_fields), so a typo in
        // e.g. `endpont` cannot silently make the endpoint override not apply.
        let bad = temporary.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[models.m]\nsource = \"hf:o/r@main\"\nendpont = \"https://x.example\"\n",
        )
        .unwrap();
        let error = Config::load_user(&bad).unwrap_err();
        assert!(
            error.localized().contains("endpont"),
            "{}",
            error.localized()
        );
    }

    #[test]
    fn skills_section_parses_entries_and_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("osdk.toml");
        std::fs::write(
            &config_file,
            "[skills]\n\
             default_agents = [\"claude-code\"]\n\
             scope = \"project\"\n\
             [skills.web-design]\n\
             source = \"github:vercel-labs/agent-skills\"\n\
             skill = \"web-design-guidelines\"\n\
             ref = \"branch:main\"\n\
             agents = [\"claude-code\", \"codex\"]\n\
             when = { os = \"linux\" }\n",
        )
        .unwrap();

        let config = Config::load_user(&config_file).unwrap();
        assert_eq!(config.skills_defaults.default_agents, vec!["claude-code"]);
        assert_eq!(config.skills_defaults.scope.as_deref(), Some("project"));

        let web = config.skills.get("web-design").expect("skill parsed");
        assert_eq!(web.source, "github:vercel-labs/agent-skills");
        assert_eq!(web.skill.as_deref(), Some("web-design-guidelines"));
        assert_eq!(web.r#ref.as_deref(), Some("branch:main"));
        assert_eq!(web.agents, vec!["claude-code", "codex"]);
        assert_eq!(web.when.as_ref().unwrap().os.len(), 1);
        assert!(web.endpoint.is_none());
    }

    #[test]
    fn skills_top_level_defaults_are_not_mistaken_for_a_skill() {
        // The `[skills]` table flattens defaults with named entries; a scalar
        // default (`scope`) must not surface as a skill named "scope".
        let temporary = tempfile::tempdir().unwrap();
        let config_file = temporary.path().join("osdk.toml");
        std::fs::write(
            &config_file,
            "[skills]\nscope = \"global\"\n[skills.only]\nsource = \"github:o/r\"\n",
        )
        .unwrap();
        let config = Config::load_user(&config_file).unwrap();
        assert_eq!(config.skills.len(), 1);
        assert!(config.skills.contains_key("only"));
        assert_eq!(config.skills_defaults.scope.as_deref(), Some("global"));
    }

    #[test]
    fn skills_unknown_entry_key_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let bad = temporary.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[skills.m]\nsource = \"github:o/r\"\nsorce = \"typo\"\n",
        )
        .unwrap();
        let error = Config::load_user(&bad).unwrap_err();
        assert!(error.localized().contains("sorce"), "{}", error.localized());
    }
}
