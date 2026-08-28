//! Strict, canonical OCI image-reference and platform values.
//!
//! This module is deliberately protocol-only. It performs no registry I/O and
//! retains no rejected input in its errors. Every public value is validated at
//! construction, so canonical strings are safe to use as identity components
//! in later anonymous registry diagnostics.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

const DOCKER_HUB_REGISTRY: &str = "docker.io";
const DEFAULT_DOCKER_TAG: &str = "latest";
const DOCKER_HUB_LIBRARY_NAMESPACE: &str = "library";
const MAX_REPOSITORY_NAME_LENGTH: usize = 255;
const MAX_REPOSITORY_COMPONENT_LENGTH: usize = MAX_REPOSITORY_NAME_LENGTH;
const MAX_TAG_LENGTH: usize = 128;
const SHA256_HEX_LENGTH: usize = 64;

/// A secret-safe parse error for OCI identity values.
///
/// Variants intentionally carry no source text. Callers may display or log an
/// error without accidentally echoing credentials or query parameters from a
/// rejected URL-shaped input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    #[error("invalid registry name")]
    InvalidRegistryName,
    #[error("invalid repository name")]
    InvalidRepositoryName,
    #[error("invalid image tag")]
    InvalidTag,
    #[error("invalid OCI digest")]
    InvalidDigest,
    #[error("unsupported OCI digest algorithm")]
    UnsupportedDigestAlgorithm,
    #[error("invalid OCI image reference")]
    InvalidImageReference,
    #[error("an image reference cannot contain both a tag and a digest")]
    TagAndDigest,
    #[error("OCI image reference is too long")]
    ImageReferenceTooLong,
    #[error("invalid OCI platform")]
    InvalidPlatform,
}

/// A canonical registry authority: a DNS host, IPv4 address, or explicitly
/// bracketed IPv6 literal, optionally followed by a TCP port.
///
/// DNS names and IP literals are rendered in lowercase/canonical form. Docker
/// Hub's transport aliases (`index.docker.io` and `registry-1.docker.io`) map
/// to the logical registry name `docker.io`. Every explicit port is retained,
/// including `80` and `443`, because a scheme-free registry name has no default
/// port; only redundant leading zeroes in the decimal spelling are removed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RegistryName(String);

impl RegistryName {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        if value.is_empty()
            || !value.is_ascii()
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
            || value
                .bytes()
                .any(|byte| matches!(byte, b'/' | b'\\' | b'@' | b'?' | b'#' | b'%'))
        {
            return Err(ReferenceError::InvalidRegistryName);
        }

        if value.starts_with('[') {
            return parse_bracketed_ipv6_registry(value);
        }
        if value.contains(['[', ']']) {
            return Err(ReferenceError::InvalidRegistryName);
        }

        let colon_count = value.bytes().filter(|byte| *byte == b':').count();
        if colon_count > 1 {
            // IPv6 literals must always use brackets so a port is unambiguous.
            return Err(ReferenceError::InvalidRegistryName);
        }
        let (raw_host, port) = if colon_count == 1 {
            let (host, raw_port) = value
                .split_once(':')
                .ok_or(ReferenceError::InvalidRegistryName)?;
            (host, Some(parse_port(raw_port)?))
        } else {
            (value, None)
        };

        let mut host = canonical_non_ipv6_host(raw_host)?;
        if matches!(host.as_str(), "index.docker.io" | "registry-1.docker.io") {
            host = DOCKER_HUB_REGISTRY.to_owned();
        }

        let canonical = match port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        Ok(Self(canonical))
    }

    /// The complete canonical registry authority, including brackets and port.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The canonical host without IPv6 brackets or a port.
    pub fn host(&self) -> &str {
        if self.0.starts_with('[') {
            let close = self
                .0
                .find(']')
                .expect("validated IPv6 registry has a closing bracket");
            &self.0[1..close]
        } else {
            self.0
                .split_once(':')
                .map_or(self.0.as_str(), |(host, _)| host)
        }
    }

    pub fn port(&self) -> Option<u16> {
        if self.0.starts_with('[') {
            let close = self.0.find(']')?;
            self.0
                .get(close + 1..)?
                .strip_prefix(':')
                .and_then(|port| port.parse().ok())
        } else {
            self.0
                .split_once(':')
                .and_then(|(_, port)| port.parse().ok())
        }
    }

    pub fn is_ipv6_literal(&self) -> bool {
        self.0.starts_with('[')
    }

    /// Whether this authority is the default, portless Docker Hub identity.
    /// An explicit port remains a distinct registry identity.
    pub fn is_docker_hub(&self) -> bool {
        self.0 == DOCKER_HUB_REGISTRY
    }
}

impl FromStr for RegistryName {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for RegistryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RegistryName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn parse_bracketed_ipv6_registry(value: &str) -> Result<RegistryName, ReferenceError> {
    let close = value.find(']').ok_or(ReferenceError::InvalidRegistryName)?;
    let raw_address = value
        .get(1..close)
        .ok_or(ReferenceError::InvalidRegistryName)?;
    if raw_address.is_empty() || raw_address.contains('%') {
        // Zone identifiers are local-interface state and are not registry
        // identity. They are deliberately unsupported, encoded or otherwise.
        return Err(ReferenceError::InvalidRegistryName);
    }
    let address = raw_address
        .parse::<Ipv6Addr>()
        .map_err(|_| ReferenceError::InvalidRegistryName)?;
    let suffix = value
        .get(close + 1..)
        .ok_or(ReferenceError::InvalidRegistryName)?;
    let port = if suffix.is_empty() {
        None
    } else {
        Some(parse_port(
            suffix
                .strip_prefix(':')
                .ok_or(ReferenceError::InvalidRegistryName)?,
        )?)
    };
    let canonical = match port {
        Some(port) => format!("[{address}]:{port}"),
        None => format!("[{address}]"),
    };
    Ok(RegistryName(canonical))
}

fn parse_port(value: &str) -> Result<u16, ReferenceError> {
    if value.is_empty() || value.len() > 5 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ReferenceError::InvalidRegistryName);
    }
    let port = value
        .parse::<u16>()
        .map_err(|_| ReferenceError::InvalidRegistryName)?;
    if port == 0 {
        return Err(ReferenceError::InvalidRegistryName);
    }
    Ok(port)
}

fn canonical_non_ipv6_host(value: &str) -> Result<String, ReferenceError> {
    if value.is_empty() || value.len() > 253 {
        return Err(ReferenceError::InvalidRegistryName);
    }
    if let Ok(address) = value.parse::<Ipv4Addr>() {
        return Ok(address.to_string());
    }
    // Reject numeric forms which different URL/network stacks may interpret as
    // non-canonical IPv4 addresses (for example, `127.1` or octal forms).
    if value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(ReferenceError::InvalidRegistryName);
    }

    let value = value.to_ascii_lowercase();
    for label in value.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(ReferenceError::InvalidRegistryName);
        }
    }
    Ok(value)
}

/// A lowercase OCI Distribution repository path without a registry or selector.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RepositoryName(String);

impl RepositoryName {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        if value.is_empty()
            || value.len() > MAX_REPOSITORY_NAME_LENGTH
            || !value.is_ascii()
            || value.split('/').any(|component| {
                component.is_empty()
                    || component.len() > MAX_REPOSITORY_COMPONENT_LENGTH
                    || component == "."
                    || component == ".."
                    || !is_repository_component(component)
            })
        {
            return Err(ReferenceError::InvalidRepositoryName);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/')
    }

    pub fn component_count(&self) -> usize {
        self.components().count()
    }
}

impl FromStr for RepositoryName {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for RepositoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepositoryName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn is_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    if !consume_lower_alphanumeric(bytes, &mut cursor) {
        return false;
    }

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'.' => cursor += 1,
            b'_' => {
                cursor += 1;
                if bytes.get(cursor) == Some(&b'_') {
                    cursor += 1;
                }
            }
            b'-' => {
                cursor += 1;
                while bytes.get(cursor) == Some(&b'-') {
                    cursor += 1;
                }
            }
            _ => return false,
        }
        if !consume_lower_alphanumeric(bytes, &mut cursor) {
            return false;
        }
    }
    true
}

fn consume_lower_alphanumeric(bytes: &[u8], cursor: &mut usize) -> bool {
    let start = *cursor;
    while bytes
        .get(*cursor)
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        *cursor += 1;
    }
    *cursor > start
}

/// A validated image tag. Tags are case-sensitive and retain their spelling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ImageTag(String);

impl ImageTag {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        let bytes = value.as_bytes();
        if value.is_empty()
            || value.len() > MAX_TAG_LENGTH
            || !value.is_ascii()
            || !bytes
                .first()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(ReferenceError::InvalidTag);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ImageTag {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for ImageTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ImageTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// An immutable OCI content digest. The initial implementation intentionally
/// admits only SHA-256 with exactly 64 lowercase hexadecimal digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct OciDigest(String);

impl OciDigest {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        let (algorithm, encoded) = value.split_once(':').ok_or(ReferenceError::InvalidDigest)?;
        if algorithm != "sha256" {
            return Err(ReferenceError::UnsupportedDigestAlgorithm);
        }
        if encoded.len() != SHA256_HEX_LENGTH
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ReferenceError::InvalidDigest);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub const fn algorithm(&self) -> &'static str {
        "sha256"
    }

    pub fn encoded(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl FromStr for OciDigest {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for OciDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OciDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The mutually exclusive mutable or immutable selector of an image.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "kind",
    content = "value",
    rename_all = "kebab-case"
)]
pub enum ImageSelector {
    Tag(ImageTag),
    Digest(OciDigest),
}

impl ImageSelector {
    pub fn tag(value: &str) -> Result<Self, ReferenceError> {
        Ok(Self::Tag(ImageTag::parse(value)?))
    }

    pub fn digest(value: &str) -> Result<Self, ReferenceError> {
        Ok(Self::Digest(OciDigest::parse(value)?))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Tag(tag) => tag.as_str(),
            Self::Digest(digest) => digest.as_str(),
        }
    }

    pub fn as_tag(&self) -> Option<&ImageTag> {
        match self {
            Self::Tag(tag) => Some(tag),
            Self::Digest(_) => None,
        }
    }

    pub fn as_digest(&self) -> Option<&OciDigest> {
        match self {
            Self::Digest(digest) => Some(digest),
            Self::Tag(_) => None,
        }
    }

    pub fn is_immutable(&self) -> bool {
        matches!(self, Self::Digest(_))
    }
}

impl fmt::Display for ImageSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ImageSelector {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.contains(':') {
            Self::digest(value)
        } else {
            Self::tag(value)
        }
    }
}

/// A canonical registry/repository image identity and exactly one selector.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ImageReference {
    registry: RegistryName,
    repository: RepositoryName,
    selector: ImageSelector,
}

impl ImageReference {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        if value.is_empty()
            || !value.is_ascii()
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
            || value.contains("://")
            || value
                .bytes()
                .any(|byte| matches!(byte, b'\\' | b'?' | b'#' | b'%'))
        {
            return Err(ReferenceError::InvalidImageReference);
        }

        let at_count = value.bytes().filter(|byte| *byte == b'@').count();
        if at_count > 1 {
            return Err(ReferenceError::InvalidImageReference);
        }

        let (name, selector) = if at_count == 1 {
            let (name, raw_digest) = value
                .split_once('@')
                .ok_or(ReferenceError::InvalidImageReference)?;
            if tag_separator(name).is_some() {
                return Err(ReferenceError::TagAndDigest);
            }
            (name, ImageSelector::Digest(OciDigest::parse(raw_digest)?))
        } else if let Some(separator) = tag_separator(value) {
            let name = value
                .get(..separator)
                .ok_or(ReferenceError::InvalidImageReference)?;
            let raw_tag = value
                .get(separator + 1..)
                .ok_or(ReferenceError::InvalidTag)?;
            (name, ImageSelector::Tag(ImageTag::parse(raw_tag)?))
        } else {
            (value, ImageSelector::tag(DEFAULT_DOCKER_TAG)?)
        };

        if name.is_empty() {
            return Err(ReferenceError::InvalidImageReference);
        }
        let (registry, repository) = split_registry_and_repository(name)?;
        Self::new(registry, repository, selector)
    }

    pub fn new(
        registry: RegistryName,
        mut repository: RepositoryName,
        selector: ImageSelector,
    ) -> Result<Self, ReferenceError> {
        if registry.is_docker_hub() && repository.component_count() == 1 {
            repository = RepositoryName::parse(&format!(
                "{DOCKER_HUB_LIBRARY_NAMESPACE}/{}",
                repository.as_str()
            ))?;
        }
        if registry.as_str().len() + 1 + repository.as_str().len() > MAX_REPOSITORY_NAME_LENGTH {
            return Err(ReferenceError::ImageReferenceTooLong);
        }
        Ok(Self {
            registry,
            repository,
            selector,
        })
    }

    pub fn registry(&self) -> &RegistryName {
        &self.registry
    }

    pub fn repository(&self) -> &RepositoryName {
        &self.repository
    }

    pub fn selector(&self) -> &ImageSelector {
        &self.selector
    }

    pub fn tag(&self) -> Option<&ImageTag> {
        self.selector.as_tag()
    }

    pub fn digest(&self) -> Option<&OciDigest> {
        self.selector.as_digest()
    }

    pub fn is_immutable(&self) -> bool {
        self.selector.is_immutable()
    }
}

impl FromStr for ImageReference {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for ImageReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.registry, self.repository)?;
        match &self.selector {
            ImageSelector::Tag(tag) => write!(formatter, ":{tag}"),
            ImageSelector::Digest(digest) => write!(formatter, "@{digest}"),
        }
    }
}

impl<'de> Deserialize<'de> for ImageReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReference {
            registry: RegistryName,
            repository: RepositoryName,
            selector: ImageSelector,
        }

        let wire = WireReference::deserialize(deserializer)?;
        Self::new(wire.registry, wire.repository, wire.selector).map_err(serde::de::Error::custom)
    }
}

fn tag_separator(value: &str) -> Option<usize> {
    let colon = value.rfind(':')?;
    let slash = value.rfind('/');
    (slash.is_none() || slash.is_some_and(|slash| colon > slash)).then_some(colon)
}

fn split_registry_and_repository(
    value: &str,
) -> Result<(RegistryName, RepositoryName), ReferenceError> {
    if let Some((first, remainder)) = value.split_once('/') {
        if is_explicit_registry_component(first) {
            return Ok((
                RegistryName::parse(first)?,
                RepositoryName::parse(remainder)?,
            ));
        }
    }
    Ok((
        RegistryName::parse(DOCKER_HUB_REGISTRY)?,
        RepositoryName::parse(value)?,
    ))
}

fn is_explicit_registry_component(value: &str) -> bool {
    value.eq_ignore_ascii_case("localhost")
        || value.starts_with('[')
        || value.contains('.')
        || value.contains(':')
}

/// A canonical OCI target platform independent of any native runtime adapter.
///
/// The string form is `os/architecture[/variant]`. Common client spellings
/// such as `macos`, `x86_64`, `x64`, and `aarch64` normalize to OCI values.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OciPlatform {
    os: String,
    architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
}

impl OciPlatform {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        let mut components = value.split('/');
        let os = components.next().ok_or(ReferenceError::InvalidPlatform)?;
        let architecture = components.next().ok_or(ReferenceError::InvalidPlatform)?;
        let variant = components.next();
        if components.next().is_some() {
            return Err(ReferenceError::InvalidPlatform);
        }
        Self::new(os, architecture, variant)
    }

    pub fn new(
        os: &str,
        architecture: &str,
        variant: Option<&str>,
    ) -> Result<Self, ReferenceError> {
        let os = canonical_platform_os(os)?;
        let (architecture, inferred_variant) = canonical_platform_architecture(architecture)?;
        let explicit_variant = variant.map(canonical_platform_component).transpose()?;
        let variant = match (explicit_variant, inferred_variant) {
            (Some(explicit), Some(inferred)) if explicit != inferred => {
                return Err(ReferenceError::InvalidPlatform);
            }
            (Some(explicit), _) => Some(explicit),
            (None, inferred) => inferred,
        };
        Ok(Self {
            os,
            architecture,
            variant,
        })
    }

    pub fn os(&self) -> &str {
        &self.os
    }

    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    pub fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }
}

impl FromStr for OciPlatform {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for OciPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.os, self.architecture)?;
        if let Some(variant) = &self.variant {
            write!(formatter, "/{variant}")?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for OciPlatform {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WirePlatform {
            os: String,
            architecture: String,
            #[serde(default)]
            variant: Option<String>,
        }

        let wire = WirePlatform::deserialize(deserializer)?;
        Self::new(&wire.os, &wire.architecture, wire.variant.as_deref())
            .map_err(serde::de::Error::custom)
    }
}

fn canonical_platform_os(value: &str) -> Result<String, ReferenceError> {
    let value = canonical_platform_component(value)?;
    Ok(match value.as_str() {
        "macos" | "macosx" | "osx" => "darwin".to_owned(),
        "win" => "windows".to_owned(),
        _ => value,
    })
}

fn canonical_platform_architecture(
    value: &str,
) -> Result<(String, Option<String>), ReferenceError> {
    let value = canonical_platform_component(value)?;
    let (architecture, variant) = match value.as_str() {
        "x86_64" | "x64" => ("amd64", None),
        "aarch64" => ("arm64", None),
        "x86" | "i386" | "i686" => ("386", None),
        "armv5" => ("arm", Some("v5")),
        "armv6" => ("arm", Some("v6")),
        "armv7" | "armv7l" => ("arm", Some("v7")),
        _ => return Ok((value, None)),
    };
    Ok((architecture.to_owned(), variant.map(str::to_owned)))
}

fn canonical_platform_component(value: &str) -> Result<String, ReferenceError> {
    if value.is_empty() || value.len() > 64 || !value.is_ascii() {
        return Err(ReferenceError::InvalidPlatform);
    }
    let value = value.to_ascii_lowercase();
    let bytes = value.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        || bytes
            .windows(2)
            .any(|pair| !pair[0].is_ascii_alphanumeric() && !pair[1].is_ascii_alphanumeric())
    {
        return Err(ReferenceError::InvalidPlatform);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn registry_names_canonicalize_hosts_ports_and_docker_aliases() {
        for (raw, expected, host, port) in [
            ("GHCR.IO", "ghcr.io", "ghcr.io", None),
            (
                "registry.example:05000",
                "registry.example:5000",
                "registry.example",
                Some(5000),
            ),
            (
                "registry.example:443",
                "registry.example:443",
                "registry.example",
                Some(443),
            ),
            ("INDEX.DOCKER.IO", "docker.io", "docker.io", None),
            (
                "registry-1.docker.io:443",
                "docker.io:443",
                "docker.io",
                Some(443),
            ),
            ("127.0.0.1:5000", "127.0.0.1:5000", "127.0.0.1", Some(5000)),
        ] {
            let registry = RegistryName::parse(raw).unwrap();
            assert_eq!(registry.as_str(), expected, "{raw}");
            assert_eq!(registry.host(), host, "{raw}");
            assert_eq!(registry.port(), port, "{raw}");
        }
    }

    #[test]
    fn ipv6_registries_require_brackets_and_are_canonical() {
        let registry = RegistryName::parse("[2001:0DB8:0:0:0:0:0:1]:05000").unwrap();
        assert_eq!(registry.as_str(), "[2001:db8::1]:5000");
        assert_eq!(registry.host(), "2001:db8::1");
        assert_eq!(registry.port(), Some(5000));
        assert!(registry.is_ipv6_literal());

        for invalid in [
            "2001:db8::1",
            "[2001:db8::1",
            "2001:db8::1]",
            "[2001:db8::1]extra",
            "[2001:db8::1]:",
            "[2001:db8::1]:70000",
            "[fe80::1%25eth0]",
            "[127.0.0.1]:5000",
        ] {
            assert_eq!(
                RegistryName::parse(invalid),
                Err(ReferenceError::InvalidRegistryName),
                "{invalid}"
            );
        }
    }

    #[test]
    fn malformed_registry_authorities_are_rejected() {
        for invalid in [
            "",
            "https://ghcr.io",
            "user@ghcr.io",
            "ghcr.io/path",
            "ghcr.io?token=x",
            "ghcr.io#fragment",
            "ghcr.io:0",
            "ghcr.io:65536",
            "ghcr.io:000080",
            "ghcr.io:port",
            "ghcr.io:",
            "-example.test",
            "example-.test",
            "example..test",
            "example.test.",
            "under_score.test",
            "127.1",
            "999.999.999.999",
        ] {
            assert!(RegistryName::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn repository_grammar_is_strict_and_lowercase() {
        for valid in [
            "library/ubuntu",
            "org/app",
            "team-name/app_name",
            "team__name/app.release",
            "a/b--c",
        ] {
            assert_eq!(RepositoryName::parse(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            "Ubuntu",
            "org/App",
            "/ubuntu",
            "ubuntu/",
            "org//app",
            "org/./app",
            "org/../app",
            "org/%2e%2e/app",
            "org\\app",
            "org:tag",
            "org/_app",
            "org/app_",
            "org/app...next",
        ] {
            assert_eq!(
                RepositoryName::parse(invalid),
                Err(ReferenceError::InvalidRepositoryName),
                "{invalid}"
            );
        }
        assert!(RepositoryName::parse(&"a".repeat(MAX_REPOSITORY_NAME_LENGTH)).is_ok());
        assert_eq!(
            RepositoryName::parse(&"a".repeat(MAX_REPOSITORY_NAME_LENGTH + 1)),
            Err(ReferenceError::InvalidRepositoryName)
        );
    }

    #[test]
    fn tags_are_bounded_case_sensitive_and_delimiter_free() {
        for valid in [
            "latest",
            "Release-1.2",
            "_internal",
            &"a".repeat(MAX_TAG_LENGTH),
        ] {
            assert_eq!(ImageTag::parse(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            ".latest",
            "-latest",
            "release/latest",
            "release:latest",
            "release@digest",
            "release?token=x",
            "release#fragment",
            &"a".repeat(MAX_TAG_LENGTH + 1),
        ] {
            assert_eq!(
                ImageTag::parse(invalid),
                Err(ReferenceError::InvalidTag),
                "{invalid}"
            );
        }
        assert_eq!(
            ImageReference::parse("ubuntu:"),
            Err(ReferenceError::InvalidTag)
        );
    }

    #[test]
    fn digest_is_sha256_only_and_lowercase() {
        let uppercase = format!("sha256:{}", "AB".repeat(32));
        assert_eq!(
            OciDigest::parse(&uppercase),
            Err(ReferenceError::InvalidDigest)
        );
        let digest = OciDigest::parse(SHA256).unwrap();
        assert_eq!(digest.as_str(), SHA256);
        assert_eq!(digest.algorithm(), "sha256");
        assert_eq!(digest.encoded().len(), 64);

        assert_eq!(
            OciDigest::parse(&format!("sha512:{}", "a".repeat(128))),
            Err(ReferenceError::UnsupportedDigestAlgorithm)
        );
        for invalid in [
            "sha256".to_owned(),
            "sha256:".to_owned(),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha256:{}", "a".repeat(65)),
            format!("sha256:{}g", "a".repeat(63)),
        ] {
            assert_eq!(
                OciDigest::parse(&invalid),
                Err(ReferenceError::InvalidDigest),
                "{invalid}"
            );
        }
    }

    #[test]
    fn docker_shorthand_and_aliases_have_one_canonical_identity() {
        for (raw, expected) in [
            ("ubuntu", "docker.io/library/ubuntu:latest"),
            ("ubuntu:24.04", "docker.io/library/ubuntu:24.04"),
            ("owner/image", "docker.io/owner/image:latest"),
            ("docker.io/ubuntu", "docker.io/library/ubuntu:latest"),
            ("index.docker.io/ubuntu", "docker.io/library/ubuntu:latest"),
            (
                "registry-1.docker.io/owner/image:V1",
                "docker.io/owner/image:V1",
            ),
        ] {
            let reference = ImageReference::parse(raw).unwrap();
            assert_eq!(reference.to_string(), expected, "{raw}");
        }

        // An explicit port is part of registry identity, so Docker Hub's
        // implicit `library/` namespace is not inferred for this spelling.
        assert_eq!(
            ImageReference::parse("docker.io:443/ubuntu")
                .unwrap()
                .to_string(),
            "docker.io:443/ubuntu:latest"
        );
    }

    #[test]
    fn explicit_registries_ports_ipv6_tags_and_digests_parse() {
        let ghcr = ImageReference::parse("GHCR.IO/org/app:Release-1").unwrap();
        assert_eq!(ghcr.to_string(), "ghcr.io/org/app:Release-1");
        assert_eq!(ghcr.registry().host(), "ghcr.io");
        assert_eq!(ghcr.repository().as_str(), "org/app");
        assert_eq!(ghcr.tag().unwrap().as_str(), "Release-1");
        assert!(!ghcr.is_immutable());

        let port = ImageReference::parse("registry.example:05000/team/app:v1").unwrap();
        assert_eq!(port.to_string(), "registry.example:5000/team/app:v1");

        let ipv6 = ImageReference::parse("[2001:db8::1]:5000/team/app:v1").unwrap();
        assert_eq!(ipv6.to_string(), "[2001:db8::1]:5000/team/app:v1");
        assert_eq!(
            ImageReference::parse("[::1]/team/app").unwrap().to_string(),
            "[::1]/team/app:latest"
        );

        let immutable = ImageReference::parse(&format!("ghcr.io/org/app@{SHA256}")).unwrap();
        assert_eq!(immutable.digest().unwrap().as_str(), SHA256);
        assert!(immutable.tag().is_none());
        assert!(immutable.is_immutable());
    }

    #[test]
    fn url_syntax_traversal_encodings_and_conflicting_selectors_are_rejected() {
        for invalid in [
            "https://ghcr.io/org/app:latest",
            "user:password@ghcr.io/org/app",
            "ghcr.io/org/app?token=secret",
            "ghcr.io/org/app#fragment",
            "ghcr.io/org/../app",
            "ghcr.io/org/%2e%2e/app",
            "ghcr.io/org/%2Fapp",
            "ghcr.io/org/%252e%252e/app",
            "ghcr.io/org\\app",
            "ghcr.io//app",
            "ghcr.io/org/App",
            "ghcr.io:bad/org/app",
            "2001:db8::1/org/app",
        ] {
            assert!(ImageReference::parse(invalid).is_err(), "{invalid}");
        }

        assert_eq!(
            ImageReference::parse(&format!("ghcr.io/org/app:v1@{SHA256}")),
            Err(ReferenceError::TagAndDigest)
        );
    }

    #[test]
    fn errors_never_retain_or_echo_rejected_input() {
        let secret = "super-secret-password";
        let input = format!("https://alice:{secret}@ghcr.io/org/app?token={secret}");
        let error = ImageReference::parse(&input).unwrap_err();
        let rendered = format!("{error:?}: {error}");
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("alice"));
    }

    #[test]
    fn platforms_canonicalize_aliases_and_cover_windows() {
        for (raw, expected) in [
            ("linux/amd64", "linux/amd64"),
            ("Linux/X86_64", "linux/amd64"),
            ("linux/aarch64/v8", "linux/arm64/v8"),
            ("linux/armv7", "linux/arm/v7"),
            ("macos/x64", "darwin/amd64"),
            ("windows/amd64", "windows/amd64"),
            ("WIN/ARM64", "windows/arm64"),
        ] {
            let platform = OciPlatform::parse(raw).unwrap();
            assert_eq!(platform.to_string(), expected, "{raw}");
        }

        let windows = OciPlatform::parse("windows/amd64").unwrap();
        assert_eq!(windows.os(), "windows");
        assert_eq!(windows.architecture(), "amd64");
        assert_eq!(windows.variant(), None);
    }

    #[test]
    fn malformed_platforms_are_rejected() {
        for invalid in [
            "",
            "linux",
            "linux/",
            "/amd64",
            "linux/amd64/",
            "linux/amd64/v8/extra",
            "linux/../amd64",
            "linux/%61md64",
            "linux/amd64?x",
            "linux/amd64-",
            "linux/amd..64",
            "linux/armv7/v6",
        ] {
            assert_eq!(
                OciPlatform::parse(invalid),
                Err(ReferenceError::InvalidPlatform),
                "{invalid}"
            );
        }
    }

    #[test]
    fn serde_is_canonical_validated_and_stable() {
        let reference = ImageReference::parse("ubuntu:24.04").unwrap();
        let json = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            json,
            r#"{"registry":"docker.io","repository":"library/ubuntu","selector":{"kind":"tag","value":"24.04"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ImageReference>(&json).unwrap(),
            reference
        );

        let platform = OciPlatform::parse("Windows/X64").unwrap();
        let json = serde_json::to_string(&platform).unwrap();
        assert_eq!(json, r#"{"os":"windows","architecture":"amd64"}"#);
        assert_eq!(
            serde_json::from_str::<OciPlatform>(&json).unwrap(),
            platform
        );

        assert!(serde_json::from_str::<RegistryName>(r#""https://ghcr.io""#).is_err());
        assert!(serde_json::from_str::<ImageReference>(
            r#"{"registry":"docker.io","repository":"Ubuntu","selector":{"kind":"tag","value":"latest"}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ImageSelector>(
            r#"{"kind":"tag","value":"latest","extra":true}"#
        )
        .is_err());
    }
}
