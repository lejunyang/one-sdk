//! Error types for osdk-core.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    PlainIo(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("network {kind}: {url}{status}", status = .status.map(|status| format!(" ({status})")).unwrap_or_default())]
    Network {
        kind: NetworkErrorKind,
        url: String,
        status: Option<u16>,
    },

    #[error("config error: {0}")]
    Config(String),

    #[error("toml parse error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("tool `{0}` is not a known backend")]
    UnknownBackend(String),

    #[error("could not resolve version `{spec}` for `{tool}`{}", .hint.as_ref().map(|h| format!(": {h}")).unwrap_or_default())]
    VersionResolve {
        tool: String,
        spec: String,
        hint: Option<String>,
    },

    #[error("tool `{tool}@{version}` is not installed")]
    NotInstalled { tool: String, version: String },

    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },

    #[error("no usable source for `{tool}`: all {tried} candidate(s) failed or were unreachable")]
    NoUsableSource { tool: String, tried: usize },

    #[error("unsupported archive: {0}")]
    UnsupportedArchive(String),

    #[error("unsupported platform: os={os}, arch={arch}")]
    UnsupportedPlatform { os: String, arch: String },

    #[error("external command `{cmd}` failed with status {status}{}", .stderr.as_ref().map(|s| format!(":\n{s}")).unwrap_or_default())]
    Command {
        cmd: String,
        status: String,
        stderr: Option<String>,
    },

    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkErrorKind {
    Forbidden,
    RateLimited,
    Server,
    Timeout,
    Interrupted,
    Connect,
    InvalidMetadata,
}

impl std::fmt::Display for NetworkErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Forbidden => "forbidden",
            Self::RateLimited => "rate-limited",
            Self::Server => "server-error",
            Self::Timeout => "timeout",
            Self::Interrupted => "interrupted",
            Self::Connect => "connect-error",
            Self::InvalidMetadata => "invalid-metadata",
        })
    }
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// Wrap an io error with the path it occurred at, for better diagnostics.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn network(url: &str, error: reqwest::Error) -> Self {
        let status = error.status().map(|status| status.as_u16());
        let kind = match error.status() {
            Some(reqwest::StatusCode::FORBIDDEN) => NetworkErrorKind::Forbidden,
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => NetworkErrorKind::RateLimited,
            Some(status) if status.is_server_error() => NetworkErrorKind::Server,
            _ if error.is_timeout() => NetworkErrorKind::Timeout,
            _ if error.is_body() || error.is_decode() => NetworkErrorKind::Interrupted,
            _ => NetworkErrorKind::Connect,
        };
        Error::Network {
            kind,
            url: url.into(),
            status,
        }
    }

    /// A localized, user-facing message for this error in the active language.
    ///
    /// Structured variants map to catalog keys; free-form variants (`Other`,
    /// `Config`, wrapped IO/HTTP errors) fall back to the Display text, which is
    /// already meaningful.
    pub fn localized(&self) -> String {
        use crate::i18n::trf;
        match self {
            Error::UnknownBackend(name) => trf("err.unknown_backend", &[("name", name)]),
            Error::NotInstalled { tool, version } => {
                trf("err.not_installed", &[("tool", tool), ("ver", version)])
            }
            Error::NoUsableSource { tool, tried } => trf(
                "err.no_usable_source",
                &[("tool", tool), ("tried", &tried.to_string())],
            ),
            Error::ChecksumMismatch {
                name,
                expected,
                actual,
            } => trf(
                "err.checksum_mismatch",
                &[("name", name), ("expected", expected), ("actual", actual)],
            ),
            Error::VersionResolve { tool, spec, hint } => {
                let base = trf("err.version_resolve", &[("tool", tool), ("spec", spec)]);
                match hint {
                    Some(h) => format!("{base}: {h}"),
                    None => base,
                }
            }
            Error::UnsupportedPlatform { os, arch } => {
                trf("err.unsupported_platform", &[("os", os), ("arch", arch)])
            }
            // Free-form / wrapped: Display is already the best message.
            _ => self.to_string(),
        }
    }
}
