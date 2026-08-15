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
}
