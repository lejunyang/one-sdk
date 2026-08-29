//! Canonical tool identities and the shared dynamic request grammar.
//!
//! A tool id is either a fixed backend name (`node`) or a namespaced dynamic
//! identity (`npm:prettier`).  Dynamic namespaces own both subject
//! canonicalization and their public option schema so parsing, inventory, and
//! fingerprint callers can share one definition of identity.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A canonical backend identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolId {
    /// A compiled-in or declarative backend with no dynamic subject.
    Fixed(String),
    /// A backend selected by a namespace-specific subject.
    Dynamic { namespace: String, subject: String },
}

impl ToolId {
    /// Parse and canonicalize an identity without options or a selector.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if let Some((namespace, subject)) = value.split_once(':') {
            Self::dynamic(namespace, subject)
        } else {
            Self::fixed(value)
        }
    }

    pub fn fixed(value: impl AsRef<str>) -> Result<Self> {
        Ok(Self::Fixed(canonical_fixed_name(value.as_ref())?))
    }

    pub fn dynamic(namespace: &str, subject: &str) -> Result<Self> {
        let namespace = canonical_namespace(namespace)?;
        let schema = namespace_schema(&namespace)
            .ok_or_else(|| Error::UnknownBackend(format!("{namespace}:{}", subject.trim())))?;
        let subject = schema.canonicalize_subject(subject)?;
        Ok(Self::Dynamic { namespace, subject })
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic { .. })
    }

    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::Fixed(_) => None,
            Self::Dynamic { namespace, .. } => Some(namespace),
        }
    }

    /// The fixed backend name or dynamic namespace subject.
    pub fn subject(&self) -> &str {
        match self {
            Self::Fixed(name) => name,
            Self::Dynamic { subject, .. } => subject,
        }
    }

    pub fn schema(&self) -> Option<&'static NamespaceSchema> {
        self.namespace().and_then(namespace_schema)
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(name) => formatter.write_str(name),
            Self::Dynamic { namespace, subject } => {
                write!(formatter, "{namespace}:{subject}")
            }
        }
    }
}

impl std::str::FromStr for ToolId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

/// Installation scope participating in a dynamic install's durable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallScope {
    Isolated,
    Global,
    ProjectManaged,
}

/// Kind of an exact dependency captured by an installation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallDependencyKind {
    Runtime,
    Installer,
    Tool,
}

/// An exact dependency whose bytes or behavior contribute to an install.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallDependency {
    pub kind: InstallDependencyKind,
    pub id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Complete, reproducible identity of one materialized dynamic installation.
///
/// Collections are canonicalized before hashing and serialized in their stable
/// order. `install_id` is a domain-separated BLAKE3 digest of every preceding
/// field and is therefore suitable for selecting an on-disk install root. Only
/// inputs known before publication belong here: post-install observations and
/// verification evidence belong in receipts, never in locator identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallIdentity {
    pub tool: String,
    pub version: String,
    pub platform: String,
    pub scope: InstallScope,
    #[serde(default)]
    pub material_options: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: Vec<InstallDependency>,
    #[serde(default)]
    pub materials: BTreeMap<String, String>,
    pub install_id: String,
}

impl InstallIdentity {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool: impl AsRef<str>,
        version: impl Into<String>,
        platform: impl Into<String>,
        scope: InstallScope,
        options: &BTreeMap<String, String>,
        mut dependencies: Vec<InstallDependency>,
        materials: BTreeMap<String, String>,
    ) -> Result<Self> {
        let tool = ToolId::parse(tool.as_ref())?;
        if !tool.is_dynamic() {
            return Err(Error::config(format!(
                "install identity requires a dynamic tool id: `{tool}`"
            )));
        }
        let material_options = dynamic_identity_options(&tool, options)?.into_map();
        let version = version.into();
        let platform = platform.into();
        validate_identity_text("version", &version)?;
        validate_identity_text("platform", &platform)?;
        validate_dependencies(&mut dependencies)?;
        validate_materials(&materials)?;
        let mut identity = Self {
            tool: tool.to_string(),
            version,
            platform,
            scope,
            material_options,
            dependencies,
            materials,
            install_id: String::new(),
        };
        identity.install_id = crate::backend::dynamic::install_identity_fingerprint(&identity)?;
        Ok(identity)
    }

    /// Validate canonical persisted fields and the self-authenticating id.
    pub fn validate(&self) -> Result<()> {
        let tool = ToolId::parse(&self.tool)?;
        if !tool.is_dynamic() || tool.to_string() != self.tool {
            return Err(Error::config(
                "install identity contains a non-canonical dynamic tool id",
            ));
        }
        validate_identity_text("version", &self.version)?;
        validate_identity_text("platform", &self.platform)?;
        validate_canonical_identity_options(&tool, &self.material_options)?;
        let mut dependencies = self.dependencies.clone();
        validate_dependencies(&mut dependencies)?;
        if dependencies != self.dependencies {
            return Err(Error::config(
                "install identity dependencies are not canonical",
            ));
        }
        validate_materials(&self.materials)?;
        let expected = crate::backend::dynamic::install_identity_fingerprint(self)?;
        if self.install_id != expected {
            return Err(Error::config(
                "dynamic install identity fingerprint mismatch",
            ));
        }
        Ok(())
    }
}

fn validate_identity_text(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(Error::config(format!(
            "install identity {label} must be non-empty canonical text"
        )));
    }
    Ok(())
}

fn validate_dependencies(dependencies: &mut Vec<InstallDependency>) -> Result<()> {
    for dependency in dependencies.iter_mut() {
        validate_identity_text("dependency id", &dependency.id)?;
        dependency.id = ToolId::parse(&dependency.id)?.to_string();
        validate_identity_text("dependency version", &dependency.version)?;
        if let Some(identity) = &dependency.identity {
            validate_identity_text("dependency identity", identity)?;
        }
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(())
}

fn validate_materials(materials: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in materials {
        validate_identity_text("material key", key)?;
        validate_identity_text("material value", value)?;
    }
    Ok(())
}

/// Which lifecycle boundary an option can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptionEffect {
    Resolution,
    Artifact,
    Layout,
    Execution,
    Secret,
}

/// One accepted spelling in a namespace's option schema.
#[derive(Clone, Copy)]
pub struct OptionDefinition {
    pub name: &'static str,
    pub canonical_name: &'static str,
    pub effect: OptionEffect,
    /// Whether the canonical value contributes to install identity.
    pub identity: bool,
    canonicalizer: fn(&str) -> Result<Option<String>>,
}

impl fmt::Debug for OptionDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OptionDefinition")
            .field("name", &self.name)
            .field("canonical_name", &self.canonical_name)
            .field("effect", &self.effect)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

type OptionSetValidator = fn(&ToolId, &BTreeMap<String, String>, &CanonicalOptions) -> Result<()>;
type SelectorValidator = fn(&ToolId, Option<&str>) -> Result<()>;

/// The accepted options and cross-option validation for one namespace.
#[derive(Debug)]
pub struct OptionSchema {
    definitions: &'static [OptionDefinition],
    validator: OptionSetValidator,
}

impl OptionSchema {
    pub fn definitions(&self) -> &'static [OptionDefinition] {
        self.definitions
    }

    pub fn definition(&self, name: &str) -> Option<&'static OptionDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }

    /// Canonicalize a request option map. Internal lock replay metadata is
    /// ignored deliberately; it is not a public namespace option.
    pub fn canonicalize(
        &self,
        id: &ToolId,
        options: &BTreeMap<String, String>,
    ) -> Result<CanonicalOptions> {
        let mut canonical = BTreeMap::new();
        for (raw_name, raw_value) in options {
            if raw_name.starts_with("__osdk_") {
                continue;
            }
            let definition = self.definition(raw_name).ok_or_else(|| {
                Error::config(format!(
                    "unsupported option `{raw_name}` for dynamic backend `{id}`"
                ))
            })?;
            let Some(value) = (definition.canonicalizer)(raw_value)? else {
                continue;
            };
            if canonical
                .insert(definition.canonical_name.to_string(), value)
                .is_some()
            {
                return Err(Error::config(format!(
                    "options `{raw_name}` and `{}` are mutually exclusive",
                    definition.canonical_name
                )));
            }
        }
        let canonical = CanonicalOptions(canonical);
        (self.validator)(id, options, &canonical)?;
        Ok(canonical)
    }

    /// Validate an already-canonical identity projection without applying a
    /// second normalization. Acquisition-only and internal keys are invalid.
    pub fn validate_canonical_identity(
        &self,
        id: &ToolId,
        options: &BTreeMap<String, String>,
    ) -> Result<CanonicalOptions> {
        for name in options.keys() {
            let definition = self.definition(name).ok_or_else(|| {
                Error::config(format!(
                    "unsupported option `{name}` for dynamic backend `{id}`"
                ))
            })?;
            if name != definition.canonical_name || !definition.identity {
                return Err(Error::config(
                    "dynamic tool inventory contains non-canonical identity options",
                ));
            }
        }
        let canonical = self.identity_options(id, options)?;
        if canonical.as_map() != options {
            return Err(Error::config(
                "dynamic tool inventory contains non-canonical identity options",
            ));
        }
        Ok(canonical)
    }

    /// Canonical public options that contribute to installation identity.
    pub fn identity_options(
        &self,
        id: &ToolId,
        options: &BTreeMap<String, String>,
    ) -> Result<CanonicalOptions> {
        let canonical = self.canonicalize(id, options)?;
        let identity = canonical
            .0
            .into_iter()
            .filter(|(name, _)| {
                self.definitions
                    .iter()
                    .find(|definition| definition.canonical_name == name)
                    .is_some_and(|definition| definition.identity)
            })
            .collect();
        Ok(CanonicalOptions(identity))
    }
}

/// A sorted, namespace-validated option map.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonicalOptions(BTreeMap<String, String>);

impl CanonicalOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    pub fn into_map(self) -> BTreeMap<String, String> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl fmt::Display for CanonicalOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (name, value)) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{name}=")?;
            write_option_value(formatter, value)?;
        }
        Ok(())
    }
}

/// Subject and option rules for a registered dynamic namespace.
#[derive(Debug)]
pub struct NamespaceSchema {
    pub namespace: &'static str,
    subject_canonicalizer: fn(&str) -> Result<String>,
    selector_validator: SelectorValidator,
    pub options: OptionSchema,
}

impl NamespaceSchema {
    pub fn canonicalize_subject(&self, subject: &str) -> Result<String> {
        (self.subject_canonicalizer)(subject)
    }

    pub fn validate_selector(&self, id: &ToolId, selector: Option<&str>) -> Result<()> {
        (self.selector_validator)(id, selector)
    }

    pub fn canonicalize_options(
        &self,
        id: &ToolId,
        options: &BTreeMap<String, String>,
    ) -> Result<CanonicalOptions> {
        self.options.canonicalize(id, options)
    }

    pub fn identity_options(
        &self,
        id: &ToolId,
        options: &BTreeMap<String, String>,
    ) -> Result<CanonicalOptions> {
        self.options.identity_options(id, options)
    }
}

/// The syntactic pieces of `tool[options]@selector`, before namespace schema
/// validation. This is useful to future namespaces whose subjects contain `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpecParts {
    pub id: String,
    pub options: BTreeMap<String, String>,
    pub selector: Option<String>,
}

impl ToolSpecParts {
    pub fn parse(input: &str) -> Result<Self> {
        parse_tool_spec_parts(input)
    }
}

/// A fully canonical parsed tool expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub id: ToolId,
    pub options: CanonicalOptions,
    pub selector: Option<String>,
}

impl ToolSpec {
    pub fn parse(input: &str) -> Result<Self> {
        let parts = ToolSpecParts::parse(input)?;
        let id = ToolId::parse(&parts.id)?;
        let options = match id.schema() {
            Some(schema) => {
                if let Some(private) = parts
                    .options
                    .keys()
                    .find(|name| name.starts_with("__osdk_"))
                {
                    return Err(Error::config(format!(
                        "internal option `{private}` cannot be set in a tool request"
                    )));
                }
                schema.canonicalize_options(&id, &parts.options)?
            }
            None if parts.options.is_empty() => CanonicalOptions::new(),
            None => {
                let name = parts.options.keys().next().expect("map is not empty");
                return Err(Error::config(format!(
                    "unsupported option `{name}` for fixed backend `{id}`"
                )));
            }
        };
        if let Some(schema) = id.schema() {
            schema.validate_selector(&id, parts.selector.as_deref())?;
        }
        let selector = parts.selector.filter(|selector| !selector.is_empty());
        Ok(Self {
            id,
            options,
            selector,
        })
    }

    pub fn selector(&self) -> Option<&str> {
        self.selector.as_deref()
    }
}

impl fmt::Display for ToolSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.id)?;
        if !self.options.is_empty() {
            write!(formatter, "[{}]", self.options)?;
        }
        if let Some(selector) = &self.selector {
            write!(formatter, "@{selector}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for ToolSpec {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

/// Return the schema for a registered namespace. Namespace names are already
/// canonical and intentionally case-sensitive at this boundary.
pub fn namespace_schema(namespace: &str) -> Option<&'static NamespaceSchema> {
    match namespace {
        "npm" => Some(&NPM_SCHEMA),
        "github" => Some(&GITHUB_SCHEMA),
        "http" => Some(&HTTP_SCHEMA),
        "cargo" => Some(&CARGO_SCHEMA),
        _ => None,
    }
}

/// Canonicalize a dynamic id through the registered namespace subject rules.
pub fn canonical_dynamic_id(value: &str) -> Result<String> {
    let id = ToolId::parse(value)?;
    if !id.is_dynamic() {
        return Err(Error::config(format!(
            "dynamic tool id must be namespaced: `{value}`"
        )));
    }
    Ok(id.to_string())
}

/// Validate and canonicalize all public request options for a dynamic id.
pub fn canonicalize_dynamic_options(
    id: &ToolId,
    options: &BTreeMap<String, String>,
) -> Result<CanonicalOptions> {
    id.schema()
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{id}`")))?
        .canonicalize_options(id, options)
}

/// Derive the safe, canonical install-identity projection for a dynamic id.
pub fn dynamic_identity_options(
    id: &ToolId,
    options: &BTreeMap<String, String>,
) -> Result<CanonicalOptions> {
    id.schema()
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{id}`")))?
        .identity_options(id, options)
}

/// Validate an identity projection loaded from durable state.
pub fn validate_canonical_identity_options(
    id: &ToolId,
    options: &BTreeMap<String, String>,
) -> Result<CanonicalOptions> {
    id.schema()
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{id}`")))?
        .options
        .validate_canonical_identity(id, options)
}

/// Validate a selector that has already been split from a canonical dynamic
/// backend id. Durable config and lock readers use this instead of rebuilding
/// an ambiguously delimited request string.
pub fn validate_dynamic_selector(id: &ToolId, selector: Option<&str>) -> Result<()> {
    id.schema()
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{id}`")))?
        .validate_selector(id, selector)
}

const NPM_OPTIONS: &[OptionDefinition] = &[
    OptionDefinition {
        name: "allow_builds",
        canonical_name: "allow_builds",
        effect: OptionEffect::Artifact,
        identity: true,
        canonicalizer: canonical_npm_allow_builds,
    },
    OptionDefinition {
        name: "installer",
        canonical_name: "installer",
        effect: OptionEffect::Artifact,
        identity: true,
        canonicalizer: canonical_npm_installer,
    },
];

const GITHUB_OPTIONS: &[OptionDefinition] = &[
    option(
        "arch",
        "arch",
        OptionEffect::Resolution,
        true,
        canonical_arch,
    ),
    option(
        "asset-regex",
        "asset-regex",
        OptionEffect::Resolution,
        true,
        canonical_regex,
    ),
    option(
        "asset-template",
        "asset-template",
        OptionEffect::Resolution,
        true,
        canonical_exact,
    ),
    option("bin", "bins", OptionEffect::Layout, true, canonical_bins),
    option("bins", "bins", OptionEffect::Layout, true, canonical_bins),
    option(
        "catalog-sha256",
        "catalog-sha256",
        OptionEffect::Artifact,
        true,
        canonical_sha256,
    ),
    option(
        "catalog-subdir",
        "catalog-subdir",
        OptionEffect::Layout,
        true,
        canonical_exact,
    ),
    // The required digest identifies catalog content. Persisting its location
    // would leak acquisition metadata without strengthening reuse identity.
    option(
        "catalog-url",
        "catalog-url",
        OptionEffect::Resolution,
        false,
        canonical_catalog_url,
    ),
    option(
        "libc",
        "libc",
        OptionEffect::Resolution,
        true,
        canonical_libc,
    ),
    option("os", "os", OptionEffect::Resolution, true, canonical_os),
    option(
        "rename",
        "rename",
        OptionEffect::Layout,
        true,
        canonical_exact,
    ),
    option(
        "strip-components",
        "strip-components",
        OptionEffect::Layout,
        true,
        canonical_usize,
    ),
];

const HTTP_OPTIONS: &[OptionDefinition] = &[
    option(
        "sha256",
        "sha256",
        OptionEffect::Artifact,
        true,
        canonical_http_sha256,
    ),
    option(
        "kind",
        "kind",
        OptionEffect::Artifact,
        true,
        canonical_http_kind,
    ),
    option(
        "bin",
        "bins",
        OptionEffect::Layout,
        true,
        canonical_http_bins,
    ),
    option(
        "bins",
        "bins",
        OptionEffect::Layout,
        true,
        canonical_http_bins,
    ),
    option(
        "subdir",
        "subdir",
        OptionEffect::Layout,
        true,
        canonical_http_relative_path,
    ),
    option(
        "rename",
        "rename",
        OptionEffect::Layout,
        true,
        canonical_http_basename,
    ),
    option(
        "strip-components",
        "strip-components",
        OptionEffect::Layout,
        true,
        canonical_http_strip_components,
    ),
];

const CARGO_OPTIONS: &[OptionDefinition] = &[
    option(
        "bin",
        "bin",
        OptionEffect::Layout,
        true,
        canonical_cargo_bin,
    ),
    option(
        "crate",
        "crate",
        OptionEffect::Artifact,
        true,
        canonical_cargo_crate,
    ),
    option(
        "default-features",
        "default-features",
        OptionEffect::Artifact,
        true,
        canonical_cargo_default_features,
    ),
    option(
        "features",
        "features",
        OptionEffect::Artifact,
        true,
        canonical_cargo_features,
    ),
    option(
        "locked",
        "locked",
        OptionEffect::Artifact,
        true,
        canonical_cargo_locked,
    ),
];

const fn option(
    name: &'static str,
    canonical_name: &'static str,
    effect: OptionEffect,
    identity: bool,
    canonicalizer: fn(&str) -> Result<Option<String>>,
) -> OptionDefinition {
    OptionDefinition {
        name,
        canonical_name,
        effect,
        identity,
        canonicalizer,
    }
}

static NPM_SCHEMA: NamespaceSchema = NamespaceSchema {
    namespace: "npm",
    subject_canonicalizer: canonical_npm_subject,
    selector_validator: validate_any_selector,
    options: OptionSchema {
        definitions: NPM_OPTIONS,
        validator: validate_npm_options,
    },
};

static GITHUB_SCHEMA: NamespaceSchema = NamespaceSchema {
    namespace: "github",
    subject_canonicalizer: canonical_github_subject,
    selector_validator: validate_any_selector,
    options: OptionSchema {
        definitions: GITHUB_OPTIONS,
        validator: validate_github_options,
    },
};

static HTTP_SCHEMA: NamespaceSchema = NamespaceSchema {
    namespace: "http",
    subject_canonicalizer: canonical_http_subject,
    selector_validator: validate_http_selector,
    options: OptionSchema {
        definitions: HTTP_OPTIONS,
        validator: validate_http_options,
    },
};

static CARGO_SCHEMA: NamespaceSchema = NamespaceSchema {
    namespace: "cargo",
    subject_canonicalizer: canonical_cargo_subject,
    selector_validator: validate_cargo_selector,
    options: OptionSchema {
        definitions: CARGO_OPTIONS,
        validator: validate_cargo_options,
    },
};

fn canonical_fixed_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value
            .chars()
            .any(|character| matches!(character, ':' | '@' | '[' | ']' | '/' | '\\'))
    {
        return Err(Error::other(format!("invalid tool request `{value}`")));
    }
    Ok(value.to_string())
}

fn canonical_namespace(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(Error::config(format!(
            "invalid dynamic backend namespace `{value}`"
        )));
    }
    Ok(value.to_string())
}

fn canonical_npm_subject(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if let Some(rest) = normalized.strip_prefix('@') {
        let (scope, name) = rest
            .split_once('/')
            .ok_or_else(|| Error::config(format!("invalid npm package id `{normalized}`")))?;
        if !valid_npm_segment(scope) || !valid_npm_segment(name) || name.contains('/') {
            return Err(Error::config(format!(
                "invalid npm package id `{normalized}`"
            )));
        }
        return Ok(format!("@{scope}/{name}"));
    }
    if !valid_npm_segment(&normalized) {
        return Err(Error::config(format!(
            "invalid npm package id `{normalized}`"
        )));
    }
    Ok(normalized)
}

fn valid_npm_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 214
        && value != "."
        && value != ".."
        && !is_windows_reserved_component(value)
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_uppercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_' | '.')
        })
}

fn is_windows_reserved_component(value: &str) -> bool {
    let trimmed = value.trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return true;
    }
    let device = trimmed.split('.').next().unwrap_or(trimmed);
    matches!(
        device.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn canonical_github_subject(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches(".git").to_ascii_lowercase();
    let (owner, repository) = value
        .split_once('/')
        .ok_or_else(|| Error::config(format!("invalid GitHub repository id `{value}`")))?;
    if !valid_github_component(owner)
        || !valid_github_component(repository)
        || repository.contains('/')
    {
        return Err(Error::config(format!(
            "invalid GitHub repository id `{value}`"
        )));
    }
    Ok(format!("{owner}/{repository}"))
}

fn canonical_cargo_subject(value: &str) -> Result<String> {
    let value = value.trim();
    if value.contains("://") || value.starts_with("https:") {
        canonical_cargo_git_subject(value)
    } else {
        canonical_cargo_crate_name(value)
    }
}

fn canonical_cargo_crate_name(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphabetic()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || is_windows_reserved_component(&value)
    {
        return Err(Error::config(format!(
            "invalid Cargo registry crate name `{value}`"
        )));
    }
    Ok(value)
}

fn canonical_cargo_git_subject(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
        || value.contains(['@', '\\'])
    {
        return Err(Error::config(
            "Cargo Git repository must be canonical HTTPS text without whitespace, credentials, or backslashes",
        ));
    }

    let parsed = reqwest::Url::parse(value)
        .map_err(|error| Error::config(format!("invalid Cargo Git repository URL: {error}")))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(Error::config(
            "Cargo Git repository must be an absolute HTTPS URL",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::config(
            "Cargo Git repository must not contain credentials",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::config(
            "Cargo Git repository must not contain a query or fragment",
        ));
    }
    if parsed.as_str() != value {
        return Err(Error::config(
            "Cargo Git repository must use its canonical URL spelling",
        ));
    }
    if parsed.path() == "/" || parsed.path().ends_with('/') {
        return Err(Error::config(
            "Cargo Git repository URL must identify a repository path",
        ));
    }
    if url_path_has_unsafe_component(value, parsed.path()) {
        return Err(Error::config(
            "Cargo Git repository URL must not contain path traversal",
        ));
    }
    Ok(value.to_string())
}

fn url_path_has_unsafe_component(raw_url: &str, parsed_path: &str) -> bool {
    let raw_path = raw_url
        .strip_prefix("https://")
        .and_then(|rest| rest.find('/').map(|offset| &rest[offset..]))
        .unwrap_or_default();
    [raw_path, parsed_path].into_iter().any(|path| {
        let mut decoded = path.as_bytes().to_vec();
        loop {
            if decoded.iter().any(|byte| byte.is_ascii_control())
                || decoded.contains(&b'\\')
                || decoded.windows(2).any(|pair| pair == b"//")
                || decoded
                    .split(|byte| *byte == b'/')
                    .any(|component| component == b"." || component == b"..")
            {
                return true;
            }
            if !decoded.contains(&b'%') {
                return false;
            }
            let (next, changed) = percent_decode_url_path(&decoded);
            if !changed {
                return false;
            }
            decoded = next;
        }
    })
}

fn percent_decode_url_path(path: &[u8]) -> (Vec<u8>, bool) {
    let mut decoded = Vec::with_capacity(path.len());
    let mut offset = 0;
    let mut changed = false;
    while offset < path.len() {
        if path[offset] == b'%' {
            if let (Some(high), Some(low)) = (
                path.get(offset + 1)
                    .and_then(|byte| hexadecimal_nibble(*byte)),
                path.get(offset + 2)
                    .and_then(|byte| hexadecimal_nibble(*byte)),
            ) {
                decoded.push(high << 4 | low);
                offset += 3;
                changed = true;
                continue;
            }
        }
        decoded.push(path[offset]);
        offset += 1;
    }
    (decoded, changed)
}

fn hexadecimal_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn canonical_http_subject(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
        || value.contains(['@', '\\'])
    {
        return Err(Error::config(
            "HTTP artifact URL template must be canonical HTTPS text without whitespace, credentials, or backslashes",
        ));
    }

    let mut rendered = String::with_capacity(value.len());
    let mut rest = value;
    let mut version_placeholders = 0usize;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let close = after_open
            .find('}')
            .ok_or_else(|| Error::config("unterminated HTTP URL template placeholder"))?;
        let placeholder = &after_open[..close];
        if placeholder != "version" {
            return Err(Error::config(format!(
                "unsupported HTTP URL template placeholder `{{{placeholder}}}`"
            )));
        }
        version_placeholders += 1;
        if version_placeholders > 8 {
            return Err(Error::config(
                "HTTP URL template may contain at most 8 version placeholders",
            ));
        }
        rendered.push_str("1.2.3");
        rest = &after_open[close + 1..];
    }
    if rest.contains('}') {
        return Err(Error::config("unmatched HTTP URL template brace"));
    }
    rendered.push_str(rest);
    if version_placeholders == 0 {
        return Err(Error::config(
            "HTTP artifact URL template must contain `{version}`",
        ));
    }

    let parsed = reqwest::Url::parse(&rendered)
        .map_err(|error| Error::config(format!("invalid HTTP artifact URL template: {error}")))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(Error::config(
            "HTTP artifact URL template must be an absolute HTTPS URL",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::config(
            "HTTP artifact URL template must not contain credentials",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::config(
            "HTTP artifact URL template must not contain a query or fragment",
        ));
    }
    if parsed.as_str() != rendered {
        return Err(Error::config(
            "HTTP artifact URL template must use its canonical URL spelling",
        ));
    }
    if parsed
        .host_str()
        .and_then(|host| {
            host.strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host)
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|address| !is_public_ip(address))
    {
        return Err(Error::config(
            "HTTP artifact URL template must not target a non-public IP address",
        ));
    }
    let authority_end = value
        .strip_prefix("https://")
        .and_then(|rest| rest.find('/').map(|offset| "https://".len() + offset))
        .ok_or_else(|| Error::config("HTTP artifact URL template requires a path"))?;
    if value[..authority_end].contains('{') {
        return Err(Error::config(
            "`{version}` is allowed only in the HTTP URL path",
        ));
    }
    let path = parsed.path();
    if !path.contains("1.2.3") {
        return Err(Error::config(
            "`{version}` is allowed only in the HTTP URL path",
        ));
    }
    Ok(value.to_string())
}

/// Conservative public-unicast policy shared by HTTP template validation and
/// the network-time resolver. Rejecting special-use ranges is preferable to
/// letting an artifact URL reach local, link-local, documentation, transition,
/// or metadata-service address space.
pub(crate) fn is_public_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        std::net::IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_ip(std::net::IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_unspecified()
                || address.is_loopback()
                || address.is_multicast()
                || segments[0] & 0xfe00 == 0xfc00
                || segments[0] & 0xffc0 == 0xfe80
                || segments[0] & 0xffc0 == 0xfec0
                || (segments[0] == 0 && segments[1..5] == [0, 0, 0, 0])
                || (segments[0] == 0x0064 && segments[1] == 0xff9b)
                || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
                || segments[0] == 0x2002
                || (segments[0] & 0xfff0 == 0x3ff0)
                || segments[0] == 0x5f00)
        }
    }
}

fn valid_github_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn canonical_npm_allow_builds(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        )
    {
        return Ok(None);
    }
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    ) {
        return Ok(Some("true".into()));
    }
    let mut packages = value
        .split(',')
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if packages
        .iter()
        .any(|package| canonical_npm_subject(package).is_err())
    {
        return Err(Error::config(
            "allow_builds contains an invalid npm package name",
        ));
    }
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Err(Error::config("allow_builds must not be empty"));
    }
    Ok(Some(packages.join(",")))
}

fn canonical_npm_installer(value: &str) -> Result<Option<String>> {
    let installer = crate::npm_tools::installer_from_request_options(&BTreeMap::from([(
        "installer".to_string(),
        value.to_string(),
    )]))?;
    Ok((installer != crate::npm_tools::NpmInstaller::Auto).then(|| installer.as_str().to_string()))
}

fn canonical_cargo_features(value: &str) -> Result<Option<String>> {
    let mut features = Vec::new();
    for raw in value.split(',') {
        let feature = raw.trim();
        if !valid_cargo_feature(feature) {
            return Err(Error::config(format!(
                "invalid Cargo feature name `{feature}`"
            )));
        }
        features.push(feature.to_string());
    }
    features.sort();
    features.dedup();
    Ok(Some(features.join(",")))
}

fn valid_cargo_feature(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
        || value.contains('\\')
    {
        return false;
    }
    if let Some(dependency) = value.strip_prefix("dep:") {
        return valid_cargo_feature_atom(dependency);
    }
    if value.contains(':') {
        return false;
    }
    if let Some((dependency, feature)) = value.split_once('/') {
        let dependency = dependency.strip_suffix('?').unwrap_or(dependency);
        return !feature.contains('/')
            && valid_cargo_feature_atom(dependency)
            && valid_cargo_feature_atom(feature);
    }
    valid_cargo_feature_atom(value)
}

fn valid_cargo_feature_atom(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+' | '.')
        })
        && value != "."
        && value != ".."
}

fn canonical_cargo_default_features(value: &str) -> Result<Option<String>> {
    canonical_cargo_boolean(value, "default-features", true)
}

fn canonical_cargo_locked(value: &str) -> Result<Option<String>> {
    canonical_cargo_boolean(value, "locked", false)
}

fn canonical_cargo_boolean(value: &str, name: &str, default: bool) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    let parsed = match value.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(Error::config(format!(
                "Cargo option `{name}` must be `true` or `false`"
            )));
        }
    };
    Ok((parsed != default).then(|| parsed.to_string()))
}

fn canonical_cargo_bin(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    crate::pipeline::validate_safe_filename("Cargo binary name", value)?;
    if value.len() > 255
        || value.ends_with([' ', '.'])
        || is_windows_reserved_component(value)
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return Err(Error::config(format!(
            "Cargo binary name is not portable: `{value}`"
        )));
    }
    Ok(Some(value.to_string()))
}

fn canonical_cargo_crate(value: &str) -> Result<Option<String>> {
    canonical_cargo_crate_name(value).map(Some)
}

fn canonical_exact(value: &str) -> Result<Option<String>> {
    reject_control_characters(value)?;
    Ok(Some(value.to_string()))
}

fn canonical_regex(value: &str) -> Result<Option<String>> {
    reject_control_characters(value)?;
    Ok(Some(value.to_string()))
}

fn canonical_os(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    let value = match value.as_str() {
        "darwin" => "macos",
        "linux" | "macos" | "windows" => value.as_str(),
        _ => return Err(Error::config("invalid GitHub target os")),
    };
    Ok(Some(value.to_string()))
}

fn canonical_arch(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    let value = match value.as_str() {
        "x86_64" | "amd64" => "x64",
        "aarch64" => "arm64",
        "i686" => "x86",
        "armv7" => "arm",
        "x64" | "arm64" | "x86" | "arm" => value.as_str(),
        _ => return Err(Error::config("invalid GitHub target arch")),
    };
    Ok(Some(value.to_string()))
}

fn canonical_libc(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    if !matches!(value.as_str(), "gnu" | "musl" | "none") {
        return Err(Error::config("invalid GitHub target libc"));
    }
    Ok(Some(value))
}

fn canonical_bins(value: &str) -> Result<Option<String>> {
    let bins = value
        .split(',')
        .map(str::trim)
        .filter(|bin| !bin.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(Some(bins.join(",")))
}

fn canonical_usize(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    Ok(Some(
        value
            .parse::<usize>()
            .map_err(|error| Error::config(format!("invalid strip-components `{value}`: {error}")))?
            .to_string(),
    ))
}

fn canonical_sha256(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::config(
            "catalog-sha256 must be a 64-character hexadecimal SHA-256 digest",
        ));
    }
    Ok(Some(value))
}

fn canonical_http_sha256(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::config(
            "sha256 must be a 64-character hexadecimal SHA-256 digest",
        ));
    }
    Ok(Some(value))
}

fn canonical_http_kind(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    if !matches!(value.as_str(), "tar.gz" | "tar.xz" | "zip" | "file") {
        return Err(Error::config(
            "invalid HTTP artifact kind (expected tar.gz|tar.xz|zip|file)",
        ));
    }
    Ok(Some(value))
}

fn canonical_http_bins(value: &str) -> Result<Option<String>> {
    let mut bins = Vec::new();
    for raw in value.split(',') {
        let path = canonical_safe_relative_path("HTTP bin path", raw)?;
        bins.push(path);
    }
    bins.sort();
    bins.dedup();
    if bins.is_empty() {
        return Err(Error::config("HTTP bins must not be empty"));
    }
    Ok(Some(bins.join(",")))
}

fn canonical_http_relative_path(value: &str) -> Result<Option<String>> {
    canonical_safe_relative_path("HTTP subdir", value).map(Some)
}

pub(crate) fn canonical_safe_relative_path(label: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.contains(['\\', ':']) {
        return Err(Error::config(format!("unsafe {label} `{value}`")));
    }
    let path = std::path::Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || path.components().any(|component| {
            let component = component.as_os_str().to_string_lossy();
            component.ends_with([' ', '.']) || is_windows_reserved_component(&component)
        })
    {
        return Err(Error::config(format!("unsafe {label} `{value}`")));
    }
    Ok(path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn canonical_http_basename(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    crate::pipeline::validate_safe_filename("HTTP executable rename", value)?;
    if value.ends_with([' ', '.']) || is_windows_reserved_component(value) {
        return Err(Error::config(format!(
            "unsafe HTTP executable rename `{value}`"
        )));
    }
    Ok(Some(value.to_string()))
}

fn canonical_http_strip_components(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    Ok(Some(
        value
            .parse::<u32>()
            .map_err(|error| Error::config(format!("invalid strip-components `{value}`: {error}")))?
            .to_string(),
    ))
}

fn canonical_catalog_url(value: &str) -> Result<Option<String>> {
    reject_control_characters(value)?;
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let parsed = reqwest::Url::parse(value)
            .map_err(|error| Error::config(format!("invalid GitHub catalog URL: {error}")))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Error::config(
                "GitHub catalog URL must not contain credentials",
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(Error::config(
                "GitHub catalog URL must not contain a query or fragment",
            ));
        }
    }
    Ok(Some(value.to_string()))
}

fn validate_npm_options(
    _id: &ToolId,
    _raw: &BTreeMap<String, String>,
    _canonical: &CanonicalOptions,
) -> Result<()> {
    Ok(())
}

fn validate_any_selector(_id: &ToolId, _selector: Option<&str>) -> Result<()> {
    Ok(())
}

fn validate_http_selector(_id: &ToolId, selector: Option<&str>) -> Result<()> {
    let Some(selector) = selector else {
        return Err(Error::config(
            "HTTP artifacts require an exact semantic version selector",
        ));
    };
    if selector.len() > 128
        || !matches!(crate::version::VersionSpec::parse(selector), crate::version::VersionSpec::Exact(version) if version == selector.trim_start_matches('v'))
    {
        return Err(Error::config(
            "HTTP artifacts require an exact semantic version selector",
        ));
    }
    Ok(())
}

fn validate_cargo_selector(id: &ToolId, selector: Option<&str>) -> Result<()> {
    let Some(selector) = selector else {
        return Ok(());
    };
    if selector.is_empty() || selector.trim() != selector || selector.len() > 1024 {
        return Err(Error::config("invalid Cargo selector"));
    }
    if id.subject().starts_with("https://") {
        if selector == "latest" {
            return Ok(());
        }
        let valid = selector
            .strip_prefix("tag:")
            .or_else(|| selector.strip_prefix("branch:"))
            .is_some_and(valid_cargo_git_ref)
            || selector.strip_prefix("rev:").is_some_and(|revision| {
                revision.len() == 40
                    && revision
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            });
        if !valid {
            return Err(Error::config(
                "Cargo Git selectors must be latest, tag:<ref>, branch:<ref>, or rev:<40 lowercase hex>",
            ));
        }
        return Ok(());
    }

    let valid = selector == "latest"
        || matches!(
            crate::version::VersionSpec::parse(selector),
            crate::version::VersionSpec::Exact(version)
                if version == selector
        )
        || valid_cargo_semver_prefix(selector);
    if !valid {
        return Err(Error::config(
            "Cargo registry selectors must be latest, an exact semantic version, or a numeric semantic-version prefix",
        ));
    }
    Ok(())
}

fn valid_cargo_semver_prefix(value: &str) -> bool {
    let mut components = value.split('.');
    let first = components.next();
    let second = components.next();
    components.next().is_none()
        && first.is_some_and(valid_cargo_version_component)
        && second.is_none_or(valid_cargo_version_component)
}

fn valid_cargo_version_component(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_cargo_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value != "@"
        && !value.starts_with(['/', '.'])
        && !value.ends_with(['/', '.'])
        && !["..", "@{", "//", "\\"]
            .iter()
            .any(|needle| value.contains(needle))
        && !value.ends_with(".lock")
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
        && !value
            .chars()
            .any(|character| matches!(character, '~' | '^' | ':' | '?' | '*' | '['))
        && value.split('/').all(|component| {
            !component.is_empty()
                && !component.starts_with('.')
                && !component.ends_with('.')
                && !component.ends_with(".lock")
        })
}

fn validate_cargo_options(
    id: &ToolId,
    _raw: &BTreeMap<String, String>,
    canonical: &CanonicalOptions,
) -> Result<()> {
    if canonical.get("crate").is_some() && !id.subject().starts_with("https://") {
        return Err(Error::config(
            "Cargo option `crate` is supported only for Git repositories",
        ));
    }
    Ok(())
}

fn validate_http_options(
    id: &ToolId,
    raw: &BTreeMap<String, String>,
    canonical: &CanonicalOptions,
) -> Result<()> {
    if raw.contains_key("bin") && raw.contains_key("bins") {
        return Err(Error::config("bin and bins are mutually exclusive"));
    }
    if canonical.get("sha256").is_none() {
        return Err(Error::config("sha256 is required for HTTP artifacts"));
    }
    let inferred_kind;
    let kind = if let Some(kind) = canonical.get("kind") {
        kind.as_str()
    } else {
        inferred_kind = match crate::pipeline::ArchiveKind::from_name(id.subject()) {
            Ok(crate::pipeline::ArchiveKind::TarGz) => "tar.gz",
            Ok(crate::pipeline::ArchiveKind::TarXz) => "tar.xz",
            Ok(crate::pipeline::ArchiveKind::Zip) => "zip",
            Ok(crate::pipeline::ArchiveKind::TarZst) => {
                return Err(Error::config(
                    "HTTP artifacts support tar.gz, tar.xz, zip, or file",
                ));
            }
            Err(_) => "file",
        };
        inferred_kind
    };
    let bins = canonical
        .get("bins")
        .map(|value| value.split(',').count())
        .unwrap_or(0);
    if kind != "file" && bins == 0 {
        return Err(Error::config(
            "HTTP archives require at least one bin or bins entry",
        ));
    }
    if canonical.get("rename").is_some() && kind != "file" && bins != 1 {
        return Err(Error::config(
            "rename requires exactly one bin for HTTP archives",
        ));
    }
    if kind == "file"
        && (bins != 0
            || canonical.get("subdir").is_some()
            || canonical.get("strip-components").is_some())
    {
        return Err(Error::config(
            "HTTP file artifacts do not accept bin, bins, subdir, or strip-components",
        ));
    }
    Ok(())
}

fn validate_github_options(
    _id: &ToolId,
    raw: &BTreeMap<String, String>,
    canonical: &CanonicalOptions,
) -> Result<()> {
    if raw.contains_key("bin") && raw.contains_key("bins") {
        return Err(Error::config("bin and bins are mutually exclusive"));
    }
    if raw.contains_key("asset-regex") && raw.contains_key("asset-template") {
        return Err(Error::config(
            "asset-regex and asset-template are mutually exclusive",
        ));
    }
    if canonical.get("catalog-url").is_some() && canonical.get("catalog-sha256").is_none() {
        return Err(Error::config("catalog-sha256 is required with catalog-url"));
    }
    Ok(())
}

fn reject_control_characters(value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(Error::config(
            "option values must not contain control characters",
        ));
    }
    Ok(())
}

fn parse_tool_spec_parts(input: &str) -> Result<ToolSpecParts> {
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid_request(input));
    }

    if let Some((open, close)) = find_option_block(input)? {
        let id = input[..open].trim();
        if id.is_empty() {
            return Err(invalid_request(input));
        }
        let options = parse_options(&input[open + 1..close])?;
        let tail = input[close + 1..].trim();
        let selector = if tail.is_empty() {
            None
        } else if let Some(selector) = tail.strip_prefix('@') {
            Some(selector.trim().to_string())
        } else {
            return Err(invalid_request(input));
        };
        return Ok(ToolSpecParts {
            id: id.to_string(),
            options,
            selector,
        });
    }

    let (id, selector) = split_selector(input);
    if id.trim().is_empty() {
        return Err(invalid_request(input));
    }
    Ok(ToolSpecParts {
        id: id.trim().to_string(),
        options: BTreeMap::new(),
        selector: selector.map(|selector| selector.trim().to_string()),
    })
}

fn find_option_block(input: &str) -> Result<Option<(usize, usize)>> {
    for (open, character) in input.char_indices() {
        if character != '[' {
            continue;
        }
        let close = find_option_close(input, open)?
            .ok_or_else(|| Error::config("unterminated dynamic tool option block"))?;
        let body = &input[open + 1..close];
        if !body.trim().is_empty() && !contains_unquoted_equals(body)? {
            continue;
        }
        let tail = input[close + 1..].trim();
        if tail.is_empty() || tail.starts_with('@') {
            return Ok(Some((open, close)));
        }
    }
    if input.contains(']') {
        return Err(Error::config("unmatched dynamic tool option bracket"));
    }
    Ok(None)
}

fn find_option_close(input: &str, open: usize) -> Result<Option<usize>> {
    let mut quote = None;
    let mut escaped = false;
    for (offset, character) in input[open + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '[' {
            return Err(Error::config("nested option brackets are not supported"));
        } else if character == ']' {
            return Ok(Some(open + 1 + offset));
        }
    }
    Ok(None)
}

fn contains_unquoted_equals(input: &str) -> Result<bool> {
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '=' {
            return Ok(true);
        }
    }
    if quote.is_some() {
        return Err(Error::config("unterminated quoted option value"));
    }
    Ok(false)
}

fn split_selector(input: &str) -> (&str, Option<&str>) {
    let Some((namespace, subject)) = input.split_once(':') else {
        return input
            .split_once('@')
            .map_or((input, None), |(id, selector)| (id, Some(selector)));
    };

    if let Some(schema) = namespace_schema(namespace) {
        if schema.canonicalize_subject(subject).is_ok() {
            return (input, None);
        }
        for (offset, character) in subject.char_indices().rev() {
            if character != '@' || is_url_authority_at(subject, offset) {
                continue;
            }
            let candidate = &subject[..offset];
            let selector = &subject[offset + 1..];
            let candidate_id =
                schema
                    .canonicalize_subject(candidate)
                    .ok()
                    .map(|subject| ToolId::Dynamic {
                        namespace: namespace.to_string(),
                        subject,
                    });
            if candidate_id
                .as_ref()
                .is_some_and(|id| schema.validate_selector(id, Some(selector)).is_ok())
            {
                let delimiter = namespace.len() + 1 + offset;
                return (&input[..delimiter], Some(&input[delimiter + 1..]));
            }
        }
    }

    if subject.contains("://") {
        for (offset, character) in subject.char_indices().rev() {
            if character != '@' || is_url_authority_at(subject, offset) {
                continue;
            }
            let delimiter = namespace.len() + 1 + offset;
            return (&input[..delimiter], Some(&input[delimiter + 1..]));
        }
        return (input, None);
    }

    for (offset, character) in subject.char_indices().rev() {
        if character != '@' || is_url_authority_at(subject, offset) {
            continue;
        }
        if offset == 0 && subject[1..].contains('/') {
            continue;
        }
        let delimiter = namespace.len() + 1 + offset;
        return (&input[..delimiter], Some(&input[delimiter + 1..]));
    }
    (input, None)
}

fn is_url_authority_at(subject: &str, at: usize) -> bool {
    let Some(scheme_end) = subject.find("://") else {
        return false;
    };
    let authority_start = scheme_end + 3;
    let authority_end = subject[authority_start..]
        .find(|character| matches!(character, '/' | '?' | '#'))
        .map_or(subject.len(), |offset| authority_start + offset);
    (authority_start..authority_end).contains(&at)
}

fn parse_options(input: &str) -> Result<BTreeMap<String, String>> {
    if input.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut options = BTreeMap::new();
    for entry in split_option_entries(input)? {
        let (raw_name, raw_value) = split_option_assignment(entry)?;
        let name = raw_name.trim();
        if !valid_option_name(name) {
            return Err(Error::config(format!("invalid option name `{name}`")));
        }
        let value = parse_option_value(raw_value)?;
        if options.insert(name.to_string(), value).is_some() {
            return Err(Error::config(format!("duplicate option `{name}`")));
        }
    }
    Ok(options)
}

fn split_option_entries(input: &str) -> Result<Vec<&str>> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == ',' {
            let entry = input[start..index].trim();
            if entry.is_empty() {
                return Err(Error::config("empty option entry"));
            }
            entries.push(entry);
            start = index + character.len_utf8();
        }
    }
    if quote.is_some() {
        return Err(Error::config("unterminated quoted option value"));
    }
    let entry = input[start..].trim();
    if entry.is_empty() {
        return Err(Error::config("empty option entry"));
    }
    entries.push(entry);
    Ok(entries)
}

fn split_option_assignment(input: &str) -> Result<(&str, &str)> {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '=' {
            return Ok((&input[..index], &input[index + 1..]));
        }
    }
    Err(Error::config(format!(
        "option entry must be `name=value`: `{input}`"
    )))
}

fn valid_option_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
}

fn parse_option_value(input: &str) -> Result<String> {
    let input = input.trim();
    let decoded = if let Some(quote) = input
        .chars()
        .next()
        .filter(|character| matches!(character, '\'' | '"'))
    {
        if input.len() < 2 || !input.ends_with(quote) || escaped_final_quote(input) {
            return Err(Error::config("unterminated quoted option value"));
        }
        decode_escapes(
            &input[quote.len_utf8()..input.len() - quote.len_utf8()],
            Some(quote),
        )?
    } else {
        if input
            .chars()
            .any(|character| matches!(character, '\'' | '"'))
        {
            return Err(Error::config(
                "quotes must surround the complete option value",
            ));
        }
        decode_escapes(input, None)?
    };
    reject_control_characters(&decoded)?;
    Ok(decoded)
}

fn escaped_final_quote(input: &str) -> bool {
    input[..input.len() - 1]
        .chars()
        .rev()
        .take_while(|character| *character == '\\')
        .count()
        % 2
        == 1
}

fn decode_escapes(input: &str, quote: Option<char>) -> Result<String> {
    let mut decoded = String::with_capacity(input.len());
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let Some(escaped) = characters.next() else {
            decoded.push('\\');
            break;
        };
        match escaped {
            '\\' => decoded.push('\\'),
            escaped if quote == Some(escaped) => decoded.push(escaped),
            '\'' | '"' | ',' | '[' | ']' | '=' if quote.is_none() => decoded.push(escaped),
            other => {
                // Regexes commonly use backslash escapes unknown to this
                // grammar. Preserve those two bytes rather than changing the
                // backend-visible expression.
                decoded.push('\\');
                decoded.push(other);
            }
        }
    }
    Ok(decoded)
}

fn write_option_value(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    let needs_quotes = value.is_empty()
        || value.trim() != value
        || value.chars().any(|character| {
            matches!(character, ',' | '[' | ']' | '=' | '\'' | '"' | '\\') || character.is_control()
        });
    if !needs_quotes {
        return formatter.write_str(value);
    }
    formatter.write_str("\"")?;
    for character in value.chars() {
        match character {
            '\\' => formatter.write_str("\\\\")?,
            '"' => formatter.write_str("\\\"")?,
            '\n' => formatter.write_str("\\n")?,
            '\r' => formatter.write_str("\\r")?,
            '\t' => formatter.write_str("\\t")?,
            other => formatter.write_str(&other.to_string())?,
        }
    }
    formatter.write_str("\"")
}

fn invalid_request(input: &str) -> Error {
    Error::other(format!("invalid tool request `{input}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_dynamic_ids_have_one_canonical_form() {
        assert_eq!(ToolId::parse(" node ").unwrap().to_string(), "node");
        assert_eq!(
            ToolId::parse("npm:@Scope/Package_Name")
                .unwrap()
                .to_string(),
            "npm:@scope/package_name"
        );
        assert_eq!(
            ToolId::parse("github:Cli/CLI.git").unwrap().to_string(),
            "github:cli/cli"
        );
    }

    #[test]
    fn npm_subject_is_lowercase_but_fixed_ids_are_not_global_lowercased() {
        assert_eq!(ToolId::parse("npm:Prettier").unwrap().subject(), "prettier");
        assert_eq!(ToolId::parse("CustomTool").unwrap().subject(), "CustomTool");
    }

    #[test]
    fn parses_scoped_npm_and_github_selectors_without_ambiguity() {
        let npm = ToolSpec::parse("npm:@antfu/ni@0.21.12").unwrap();
        assert_eq!(npm.id.to_string(), "npm:@antfu/ni");
        assert_eq!(npm.selector(), Some("0.21.12"));

        let github = ToolSpec::parse("github:Cli/CLI.git@v2.62.0").unwrap();
        assert_eq!(github.id.to_string(), "github:cli/cli");
        assert_eq!(github.selector(), Some("v2.62.0"));
    }

    #[test]
    fn cargo_registry_subjects_and_selectors_are_canonical() {
        assert_eq!(
            ToolId::parse("cargo:Cargo_Edit").unwrap().to_string(),
            "cargo:cargo_edit"
        );
        assert_ne!(
            ToolId::parse("cargo:cargo_edit").unwrap(),
            ToolId::parse("cargo:cargo-edit").unwrap()
        );

        for selector in ["latest", "14", "14.1", "14.1.0", "1.0.0-beta.1"] {
            let request = format!("cargo:ripgrep@{selector}");
            assert_eq!(
                ToolSpec::parse(&request).unwrap().selector(),
                Some(selector),
                "{request}"
            );
        }
        assert_eq!(ToolSpec::parse("cargo:ripgrep").unwrap().selector(), None);

        let overlong = "a".repeat(65);
        for invalid in [
            "cargo:".to_string(),
            "cargo:1crate".to_string(),
            "cargo:-crate".to_string(),
            "cargo:crate-".to_string(),
            "cargo:foo/bar".to_string(),
            "cargo:foo.bar".to_string(),
            "cargo:CON".to_string(),
            format!("cargo:{overlong}"),
        ] {
            assert!(ToolSpec::parse(&invalid).is_err(), "{invalid}");
        }
        for invalid in [
            "cargo:ripgrep@",
            "cargo:ripgrep@v14.1.0",
            "cargo:ripgrep@14.1.0.0",
            "cargo:ripgrep@01",
            "cargo:ripgrep@^14",
            "cargo:ripgrep@14.*",
            "cargo:ripgrep@tag:v14",
        ] {
            assert!(ToolSpec::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn cargo_git_subjects_and_selectors_are_strict_and_unambiguous() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        for selector in [
            "latest",
            "tag:v1.2.3",
            "branch:release/1.x",
            &format!("rev:{revision}"),
        ] {
            let request = format!("cargo:https://git.example.test/Team/Tool.git@{selector}");
            let parsed = ToolSpec::parse(&request).unwrap();
            assert_eq!(
                parsed.id.to_string(),
                "cargo:https://git.example.test/Team/Tool.git"
            );
            assert_eq!(parsed.selector(), Some(selector), "{request}");
        }
        assert_eq!(
            ToolSpec::parse("cargo:https://git.example.test/Team/Tool.git")
                .unwrap()
                .selector(),
            None
        );

        for invalid in [
            "cargo:http://git.example.test/team/tool.git@tag:v1",
            "cargo:git://git.example.test/team/tool.git@tag:v1",
            "cargo:file:///tmp/tool@branch:main",
            "cargo:https://user@git.example.test/team/tool.git",
            "cargo:https://git.example.test/team/tool.git?token=x@tag:v1",
            "cargo:https://git.example.test/team/tool.git#main@tag:v1",
            "cargo:https://git.example.test/team/../tool.git@tag:v1",
            "cargo:https://git.example.test/team/%2e%2e/tool.git@tag:v1",
            "cargo:https://git.example.test/team/%252e%252e/tool.git@tag:v1",
            "cargo:https://git.example.test/@tag:v1",
            "cargo:https://git.example.test/team/tool.git@1.2.3",
            "cargo:https://git.example.test/team/tool.git@tag:",
            "cargo:https://git.example.test/team/tool.git@branch:bad..ref",
            "cargo:https://git.example.test/team/tool.git@branch:bad.lock",
            "cargo:https://git.example.test/team/tool.git@rev:01234567",
            "cargo:https://git.example.test/team/tool.git@rev:0123456789ABCDEF0123456789ABCDEF01234567",
            "cargo:https://git.example.test/team/tool.git@0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(ToolSpec::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn cargo_options_are_canonical_safe_and_identity_bearing() {
        let parsed = ToolSpec::parse(
            "cargo:ripgrep[locked=true,features='simd, pcre2,simd',default-features=false,bin=rg]@14.1",
        )
        .unwrap();
        assert_eq!(
            parsed.to_string(),
            "cargo:ripgrep[bin=rg,default-features=false,features=\"pcre2,simd\",locked=true]@14.1"
        );
        assert_eq!(
            dynamic_identity_options(&parsed.id, parsed.options.as_map()).unwrap(),
            parsed.options
        );

        let defaults =
            ToolSpec::parse("cargo:ripgrep[default-features=TRUE,locked=false]@latest").unwrap();
        assert!(defaults.options.is_empty());

        let workspace = ToolSpec::parse(
            "cargo:https://git.example.test/team/workspace.git[crate=Rip_Grep]@tag:v1",
        )
        .unwrap();
        assert_eq!(workspace.options.get("crate").unwrap(), "rip_grep");
        let qualified =
            ToolSpec::parse("cargo:ripgrep[features='foo?/bar,dep:baz,plain,foo/bar,dep:baz']@14")
                .unwrap();
        assert_eq!(
            qualified.options.get("features").unwrap(),
            "dep:baz,foo/bar,foo?/bar,plain"
        );

        for invalid in [
            "cargo:ripgrep[crate=ripgrep]@14",
            "cargo:ripgrep[features=]@14",
            "cargo:ripgrep[features='simd,,pcre2']@14",
            "cargo:ripgrep[features='dep:']@14",
            "cargo:ripgrep[features='foo?']@14",
            "cargo:ripgrep[features='foo//bar']@14",
            "cargo:ripgrep[features='../bar']@14",
            "cargo:ripgrep[features='foo/..']@14",
            "cargo:ripgrep[default-features=yes]@14",
            "cargo:ripgrep[locked=1]@14",
            "cargo:ripgrep[bin=../rg]@14",
            "cargo:ripgrep[bin=bin/rg]@14",
            "cargo:ripgrep[bin='rg.']@14",
            "cargo:ripgrep[bin=CON]@14",
        ] {
            assert!(ToolSpec::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn syntax_parser_preserves_url_userinfo_at_signs() {
        let parts =
            ToolSpecParts::parse("http:https://user@example.test/releases/tool.tar.gz@1.2.3")
                .unwrap();
        assert_eq!(
            parts.id,
            "http:https://user@example.test/releases/tool.tar.gz"
        );
        assert_eq!(parts.selector.as_deref(), Some("1.2.3"));

        let without_selector =
            ToolSpecParts::parse("http:https://user@example.test/releases/tool.tar.gz").unwrap();
        assert_eq!(
            without_selector.id,
            "http:https://user@example.test/releases/tool.tar.gz"
        );
        assert_eq!(without_selector.selector, None);
    }

    #[test]
    fn http_specs_require_strict_https_templates_exact_versions_and_checksums() {
        let digest = "A".repeat(64);
        let parsed = ToolSpec::parse(&format!(
            "http:https://downloads.example.test/tool-{{version}}.tar.gz[sha256={digest},kind=tar.gz,bin=pkg/tool,subdir=dist,rename=tool,strip-components=1]@1.2.3"
        ))
        .unwrap();
        assert_eq!(
            parsed.id.to_string(),
            "http:https://downloads.example.test/tool-{version}.tar.gz"
        );
        assert_eq!(parsed.selector(), Some("1.2.3"));
        assert_eq!(parsed.options.get("sha256").unwrap(), &"a".repeat(64));
        assert_eq!(parsed.options.get("bins").unwrap(), "pkg/tool");

        for invalid in [
            "http:http://example.test/tool-{version}.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3",
            "http:https://user@example.test/tool-{version}.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3",
            "http:https://example.test/tool-{version}.zip?token=x[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3",
            "http:https://example.test/tool-{arch}.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3",
            "http:https://example.test/tool.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2.3",
            "http:https://example.test/tool-{version}.zip@1.2.3",
            "http:https://example.test/tool-{version}.zip[sha256=bad]@1.2.3",
            "http:https://example.test/tool-{version}.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@latest",
            "http:https://example.test/tool-{version}.zip[sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]@1.2",
        ] {
            assert!(ToolSpec::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn http_layout_options_are_canonical_and_cannot_escape() {
        let base = "http:https://example.test/tool-{version}.zip";
        let digest = "a".repeat(64);
        let singular = ToolSpec::parse(&format!(
            "{base}[sha256={digest},kind=zip,bin=dist/tool]@1.2.3"
        ))
        .unwrap();
        let plural = ToolSpec::parse(&format!(
            "{base}[kind=ZIP,bins=dist/tool,sha256={digest}]@1.2.3"
        ))
        .unwrap();
        assert_eq!(singular, plural);
        let inferred =
            ToolSpec::parse(&format!("{base}[sha256={digest},bin=dist/tool]@1.2.3")).unwrap();
        assert_eq!(inferred.options.get("bins").unwrap(), "dist/tool");
        assert!(ToolSpec::parse(&format!("{base}[sha256={digest}]@1.2.3")).is_err());

        for option in [
            "bin=../tool",
            "bins=/tool",
            "subdir=../dist",
            "rename=../tool",
            "kind=tar.zst",
            "kind=file,bin=tool",
            "kind=zip,bin=a,bins=b",
            "kind=zip,bins=a,b,rename=tool",
        ] {
            let request = format!("{base}[sha256={digest},{option}]@1.2.3");
            assert!(ToolSpec::parse(&request).is_err(), "{request}");
        }
    }

    #[test]
    fn http_templates_reject_noncanonical_paths_and_non_public_literals() {
        let digest = "a".repeat(64);
        for template in [
            "https://example.test/a/../tool-{version}.zip",
            "https://example.test/a/./tool-{version}.zip",
            "https://example.test/%2e%2e/tool-{version}.zip",
            "https://127.0.0.1/tool-{version}.zip",
            "https://169.254.169.254/tool-{version}.zip",
            "https://[::1]/tool-{version}.zip",
            "https://[::ffff:127.0.0.1]/tool-{version}.zip",
            "https://[::ffff:169.254.169.254]/tool-{version}.zip",
        ] {
            let request = format!("http:{template}[sha256={digest}]@1.2.3");
            assert!(ToolSpec::parse(&request).is_err(), "{request}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(!is_public_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn inline_options_are_schema_validated_sorted_and_canonicalized() {
        let parsed =
            ToolSpec::parse("npm:Prettier[installer=AUBE,allow_builds='Sharp, esbuild, sharp']@3")
                .unwrap();
        assert_eq!(parsed.options.get("allow_builds").unwrap(), "esbuild,sharp");
        assert_eq!(parsed.options.get("installer").unwrap(), "aube");
        assert!(ToolSpec::parse("npm:prettier[allow_builds='../evil']@3").is_err());
        assert!(ToolSpec::parse("npm:prettier[allow_builds='@scope/AUX']@3").is_err());
        assert_eq!(
            parsed.to_string(),
            "npm:prettier[allow_builds=\"esbuild,sharp\",installer=aube]@3"
        );
        assert_eq!(ToolSpec::parse(&parsed.to_string()).unwrap(), parsed);
    }

    #[test]
    fn quoted_and_escaped_values_round_trip() {
        let parsed = ToolSpec::parse(
            r#"github:owner/repo[asset-regex="^tool\[x\],v[0-9]+\.tgz$",asset-template=ignored]@latest"#,
        );
        assert!(parsed
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));

        let parsed =
            ToolSpec::parse(r#"github:owner/repo[asset-regex="^tool\[x\],v[0-9]+\.tgz$"]@latest"#)
                .unwrap();
        assert_eq!(
            parsed.options.get("asset-regex").unwrap(),
            r#"^tool\[x\],v[0-9]+\.tgz$"#
        );
        assert_eq!(ToolSpec::parse(&parsed.to_string()).unwrap(), parsed);
    }

    #[test]
    fn github_aliases_and_platform_values_share_canonical_options() {
        let singular =
            ToolSpec::parse("github:Owner/Repo.git[bin=bin/tool,os=darwin,arch=amd64]@1").unwrap();
        let plural =
            ToolSpec::parse("github:owner/repo[bins=bin/tool,arch=x64,os=macos]@1").unwrap();
        assert_eq!(singular, plural);
        assert_eq!(
            singular.to_string(),
            "github:owner/repo[arch=x64,bins=bin/tool,os=macos]@1"
        );
    }

    #[test]
    fn option_order_does_not_change_canonical_output() {
        let first =
            ToolSpec::parse("github:owner/repo[rename=rg,arch=amd64,os=darwin]@latest").unwrap();
        let second =
            ToolSpec::parse("github:OWNER/REPO[os=macos,rename=rg,arch=x64]@latest").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn catalog_location_is_validated_but_not_part_of_identity_projection() {
        let id = ToolId::parse("github:owner/repo").unwrap();
        let options = BTreeMap::from([
            (
                "catalog-url".into(),
                "https://example.test/catalog.json".into(),
            ),
            ("catalog-sha256".into(), "A".repeat(64)),
        ]);
        let canonical = canonicalize_dynamic_options(&id, &options).unwrap();
        assert!(canonical.get("catalog-url").is_some());
        let identity = dynamic_identity_options(&id, &options).unwrap();
        assert!(identity.get("catalog-url").is_none());
        assert_eq!(identity.get("catalog-sha256").unwrap(), &"a".repeat(64));
    }

    #[test]
    fn unknown_namespaces_and_options_fail_during_schema_validation() {
        assert!(matches!(
            ToolSpec::parse("pip:ripgrep@latest"),
            Err(Error::UnknownBackend(_))
        ));
        let error = ToolSpec::parse("npm:prettier[token=secret]@3").unwrap_err();
        assert!(error.to_string().contains("unsupported option `token`"));
        let error = ToolSpec::parse("node[token=secret]@20").unwrap_err();
        assert!(error.to_string().contains("fixed backend"));
    }

    #[test]
    fn private_replay_options_cannot_be_smuggled_through_inline_syntax() {
        let error = ToolSpec::parse("npm:prettier[__osdk_node_version=24]@3").unwrap_err();
        assert!(error.to_string().contains("internal option"));
    }

    #[test]
    fn rejects_ambiguous_or_malformed_forms() {
        for invalid in [
            "",
            "@20",
            "npm:",
            "npm:@scope",
            "npm:foo[installer]",
            "npm:foo[=aube]",
            "npm:foo[installer=aube,]",
            "npm:foo[installer=aube,installer=npm]",
            "npm:foo[installer='aube]",
            "npm:foo[installer=aube]trailing@3",
            "github:owner/repo[bin=a,bins=b]@1",
            "github:owner/repo[asset-regex=x,asset-template=y]@1",
        ] {
            assert!(ToolSpec::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn install_identity_hashes_all_durable_selector_inputs() {
        let dependencies = vec![InstallDependency {
            kind: InstallDependencyKind::Runtime,
            id: "node".into(),
            version: "24.1.0".into(),
            identity: Some("runtime-id".into()),
        }];
        let materials = BTreeMap::from([("root-sri".into(), "sha512-one".into())]);
        let first = InstallIdentity::new(
            "npm:Prettier",
            "3.6.2",
            "linux-x64",
            InstallScope::Isolated,
            &BTreeMap::from([("installer".into(), "AUBE".into())]),
            dependencies.clone(),
            materials.clone(),
        )
        .unwrap();
        assert_eq!(first.tool, "npm:prettier");
        assert_eq!(first.material_options["installer"], "aube");
        assert!(first.install_id.starts_with("b3-v2:"));
        first.validate().unwrap();

        let changed = InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            "linux-x64",
            InstallScope::Global,
            &BTreeMap::from([("installer".into(), "aube".into())]),
            dependencies,
            materials,
        )
        .unwrap();
        assert_ne!(first.install_id, changed.install_id);
    }
}
