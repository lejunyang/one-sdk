use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::ModelRef;

pub mod huggingface;

#[derive(Debug, Clone)]
pub struct RemoteModelFile {
    pub path: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub etag: Option<String>,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct RemoteSnapshot {
    pub revision: String,
    pub endpoint: String,
    pub files: Vec<RemoteModelFile>,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn resolve(
        &self,
        ctx: &Ctx,
        reference: &ModelRef,
        endpoint: &str,
    ) -> Result<RemoteSnapshot>;
}

pub async fn get_cached_json<T: DeserializeOwned>(
    ctx: &Ctx,
    provider: &str,
    cache_identity: &str,
    url: &str,
    headers: &[(String, String)],
) -> Result<T> {
    let hash = blake3::hash(cache_identity.as_bytes()).to_hex().to_string();
    let cache = ctx
        .dirs
        .remote_cache()
        .join("models")
        .join(provider)
        .join(hash);
    if ctx.config.settings.offline {
        let bytes = std::fs::read(&cache).map_err(|_| {
            Error::other(format!(
                "offline model metadata cache miss for {cache_identity}"
            ))
        })?;
        return Ok(serde_json::from_slice(&bytes)?);
    }

    let mut request = ctx.client.get(url);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    match request.send().await {
        Ok(response) => match response
            .error_for_status()
            .map_err(|error| Error::network(url, error))
        {
            Ok(response) => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| Error::network(url, error))?;
                let parsed = serde_json::from_slice(&bytes)?;
                write_atomic(&cache, &bytes)?;
                Ok(parsed)
            }
            Err(error) => read_stale(&cache).or(Err(error)),
        },
        Err(error) => {
            let error = Error::network(url, error);
            read_stale(&cache).or(Err(error))
        }
    }
}

fn read_stale<T: DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
    tracing::warn!(path = %path.display(), "using stale cached model metadata");
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(path, error))
}
