//! Secret-safe evidence for container diagnostics.
//!
//! Raw URLs, header values, command arguments, environment values, and command
//! output never implement `Serialize`. The public evidence types below retain
//! only deliberately bounded metadata or values sanitized at construction.

use std::fmt;

use serde::Serialize;

use crate::process::CommandSpec;

pub const REDACTED: &str = "[redacted]";

/// A URL whose credentials, path, query, and fragment cannot reach output.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    /// Parse and sanitize an endpoint URL.
    ///
    /// The scheme, host, and explicit port are retained. User information is
    /// removed, a non-root path is replaced, query contents are replaced, and
    /// the fragment is discarded. Only schemes used by native runtime and
    /// registry endpoints are accepted.
    pub fn parse(raw: &str) -> Result<Self, RedactedUrlError> {
        let mut url = reqwest::Url::parse(raw).map_err(|_| RedactedUrlError::Invalid)?;
        if !matches!(
            url.scheme(),
            "http" | "https" | "tcp" | "ssh" | "unix" | "npipe"
        ) {
            return Err(RedactedUrlError::UnsupportedScheme);
        }

        if !url.username().is_empty() && url.set_username("").is_err() {
            return Err(RedactedUrlError::Invalid);
        }
        if url.password().is_some() && url.set_password(None).is_err() {
            return Err(RedactedUrlError::Invalid);
        }
        if !matches!(url.path(), "" | "/") {
            url.set_path(REDACTED);
        }
        if url.query().is_some() {
            url.set_query(Some("redacted"));
        }
        url.set_fragment(None);

        Ok(Self(url.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RedactedUrl").field(&self.0).finish()
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RedactedUrlError {
    #[error("invalid endpoint URL")]
    Invalid,
    #[error("unsupported endpoint URL scheme")]
    UnsupportedScheme,
}

/// Header names useful in diagnostics. Unknown names are collapsed rather
/// than copied from untrusted input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeaderName {
    Authorization,
    ProxyAuthorization,
    Cookie,
    SetCookie,
    WwwAuthenticate,
    ContentType,
    ContentLength,
    ContentRange,
    DockerContentDigest,
    Location,
    Other,
}

impl HeaderName {
    pub fn classify(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => Self::Authorization,
            "proxy-authorization" => Self::ProxyAuthorization,
            "cookie" => Self::Cookie,
            "set-cookie" => Self::SetCookie,
            "www-authenticate" => Self::WwwAuthenticate,
            "content-type" => Self::ContentType,
            "content-length" => Self::ContentLength,
            "content-range" => Self::ContentRange,
            "docker-content-digest" => Self::DockerContentDigest,
            "location" => Self::Location,
            _ => Self::Other,
        }
    }
}

/// A marker whose serialized representation is always the redaction sentinel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RedactedValue;

impl Serialize for RedactedValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(REDACTED)
    }
}

/// Header evidence that can record presence but never the raw value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct RedactedHeader {
    name: HeaderName,
    value: RedactedValue,
}

impl RedactedHeader {
    pub const fn present(name: HeaderName) -> Self {
        Self {
            name,
            value: RedactedValue,
        }
    }

    /// Build evidence from a raw header while deliberately discarding its
    /// value. The slice is accepted to make accidental retention unnecessary.
    pub fn from_raw(name: &str, _value: &[u8]) -> Self {
        Self::present(HeaderName::classify(name))
    }

    pub const fn name(&self) -> HeaderName {
        self.name
    }
}

/// A fixed set of executable identities used by native-container adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeProgram {
    Docker,
    Containerd,
    Ctr,
    Crictl,
    Nerdctl,
    Podman,
    Buildx,
    Buildctl,
    Other,
}

/// Typed purpose of a native command. No raw argument is needed to explain the
/// operation in a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandPurpose {
    Version,
    RuntimeInfo,
    ContextInspect,
    BuilderInspect,
    CacheStatus,
    Pull,
    Prune,
    Other,
}

/// Command evidence that records a typed executable and purpose plus only the
/// number of discarded raw arguments.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct RedactedCommand {
    program: NativeProgram,
    purpose: CommandPurpose,
    argument_count: usize,
    arguments: RedactedValue,
}

impl RedactedCommand {
    pub fn from_spec(
        program: NativeProgram,
        purpose: CommandPurpose,
        command: &CommandSpec,
    ) -> Self {
        Self {
            program,
            purpose,
            argument_count: command.arguments().len(),
            arguments: RedactedValue,
        }
    }

    pub const fn program(&self) -> NativeProgram {
        self.program
    }

    pub const fn purpose(&self) -> CommandPurpose {
        self.purpose
    }

    pub const fn argument_count(&self) -> usize {
        self.argument_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_serialization_removes_every_secret_bearing_component() {
        let raw =
            "https://alice:password@example.test/private/repository?token=abc123&sig=xyz#secret";
        let url = RedactedUrl::parse(raw).unwrap();
        let serialized = serde_json::to_string(&url).unwrap();

        assert_eq!(url.as_str(), "https://example.test/[redacted]?redacted");
        for secret in [
            "alice",
            "password",
            "private",
            "repository",
            "abc123",
            "xyz",
            "secret",
        ] {
            assert!(
                !serialized.contains(secret),
                "leaked {secret}: {serialized}"
            );
        }
    }

    #[test]
    fn header_and_command_evidence_never_retain_raw_values() {
        let header = RedactedHeader::from_raw("Authorization", b"Bearer top-secret");
        let command = CommandSpec::new("docker")
            .args(["login", "--password", "top-secret"])
            .env("REGISTRY_TOKEN", "top-secret");
        let evidence =
            RedactedCommand::from_spec(NativeProgram::Docker, CommandPurpose::Other, &command);

        let serialized = serde_json::to_string(&(&header, &evidence)).unwrap();
        assert!(!serialized.contains("top-secret"));
        assert!(!serialized.contains("Bearer"));
        assert!(!serialized.contains("password"));
        assert!(serialized.contains(REDACTED));
        assert_eq!(evidence.argument_count(), 3);
    }

    #[test]
    fn unsupported_url_errors_do_not_echo_input() {
        let secret = "credential://top-secret@example.test/path";
        let error = RedactedUrl::parse(secret).unwrap_err();
        assert_eq!(error, RedactedUrlError::UnsupportedScheme);
        assert!(!error.to_string().contains("top-secret"));
    }
}
