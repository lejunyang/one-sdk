//! Canonical tool identities and the shared dynamic request grammar.
//!
//! A tool id is either a fixed backend name (`node`) or a namespaced dynamic
//! identity (`npm:prettier`).  Dynamic namespaces own both subject
//! canonicalization and their public option schema so parsing, inventory, and
//! fingerprint callers can share one definition of identity.

use std::collections::BTreeMap;
use std::fmt;

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

type OptionSetValidator = fn(&BTreeMap<String, String>, &CanonicalOptions) -> Result<()>;

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
        (self.validator)(options, &canonical)?;
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
    pub options: OptionSchema,
}

impl NamespaceSchema {
    pub fn canonicalize_subject(&self, subject: &str) -> Result<String> {
        (self.subject_canonicalizer)(subject)
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
        Ok(Self {
            id,
            options,
            selector: parts.selector.filter(|selector| !selector.is_empty()),
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
    options: OptionSchema {
        definitions: NPM_OPTIONS,
        validator: validate_npm_options,
    },
};

static GITHUB_SCHEMA: NamespaceSchema = NamespaceSchema {
    namespace: "github",
    subject_canonicalizer: canonical_github_subject,
    options: OptionSchema {
        definitions: GITHUB_OPTIONS,
        validator: validate_github_options,
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
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
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
    _raw: &BTreeMap<String, String>,
    _canonical: &CanonicalOptions,
) -> Result<()> {
    Ok(())
}

fn validate_github_options(
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
        for (offset, character) in subject.char_indices() {
            if character != '@' {
                continue;
            }
            let candidate = &subject[..offset];
            if schema.canonicalize_subject(candidate).is_ok() {
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
            ToolSpec::parse("cargo:ripgrep@latest"),
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
}
