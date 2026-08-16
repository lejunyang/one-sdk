use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::provider::{get_cached_json, ModelProvider, RemoteModelFile, RemoteSnapshot};
use crate::model::{ModelRef, ProviderId};

#[derive(Default)]
pub struct HuggingFace {
    token: Option<String>,
}

impl HuggingFace {
    #[cfg(test)]
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    sha: String,
    #[serde(default)]
    siblings: Vec<RepoSibling>,
}

#[derive(Debug, Deserialize)]
struct RepoSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, rename = "blobId")]
    blob_id: Option<String>,
    #[serde(default)]
    lfs: Option<LfsFile>,
}

#[derive(Debug, Deserialize)]
struct LfsFile {
    sha256: String,
    size: u64,
}

#[async_trait]
impl ModelProvider for HuggingFace {
    async fn resolve(
        &self,
        ctx: &Ctx,
        reference: &ModelRef,
        endpoint: &str,
    ) -> Result<RemoteSnapshot> {
        if reference.provider != ProviderId::HuggingFace {
            return Err(Error::config(format!(
                "Hugging Face provider cannot resolve {}",
                reference.provider
            )));
        }
        let endpoint = endpoint.trim_end_matches('/');
        let metadata_url = metadata_url(endpoint, &reference.repository, &reference.revision)?;
        let headers = auth_headers(self.token.as_deref());
        let cache_identity = format!(
            "{}:{}:{}@{}",
            reference.provider, endpoint, reference.repository, reference.revision
        );
        let info: ModelInfo = get_cached_json(
            ctx,
            reference.provider.as_str(),
            &cache_identity,
            &metadata_url,
            &headers,
        )
        .await?;
        if info.sha.trim().is_empty() {
            return Err(Error::other("Hugging Face response has no commit revision"));
        }
        let mut files = Vec::with_capacity(info.siblings.len());
        for sibling in info.siblings {
            crate::model::safe_relative_path(&sibling.rfilename)?;
            let sha256 = sibling.lfs.as_ref().map(|lfs| lfs.sha256.clone());
            let size = sibling.lfs.as_ref().map(|lfs| lfs.size).or(sibling.size);
            files.push(RemoteModelFile {
                url: file_url(
                    endpoint,
                    &reference.repository,
                    &info.sha,
                    &sibling.rfilename,
                )?,
                path: sibling.rfilename,
                size,
                etag: sha256.clone().or(sibling.blob_id),
                sha256,
                headers: headers.clone(),
            });
        }
        if files.is_empty() {
            return Err(Error::other(format!(
                "Hugging Face repository {}@{} contains no files",
                reference.repository, reference.revision
            )));
        }
        Ok(RemoteSnapshot {
            revision: info.sha,
            endpoint: endpoint.to_string(),
            files,
        })
    }
}

fn metadata_url(endpoint: &str, repository: &str, revision: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config(format!("invalid Hugging Face endpoint: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::config("Hugging Face endpoint cannot be a base URL"))?;
        segments.pop_if_empty();
        segments.extend(["api", "models"]);
        for part in repository.split('/') {
            segments.push(part);
        }
        segments.extend(["revision", revision]);
    }
    url.query_pairs_mut().append_pair("blobs", "true");
    Ok(url.into())
}

fn file_url(endpoint: &str, repository: &str, revision: &str, path: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config(format!("invalid Hugging Face endpoint: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::config("Hugging Face endpoint cannot be a base URL"))?;
        segments.pop_if_empty();
        for part in repository.split('/') {
            segments.push(part);
        }
        segments.extend(["resolve", revision]);
        for part in path.split('/') {
            segments.push(part);
        }
    }
    Ok(url.into())
}

fn auth_headers(explicit: Option<&str>) -> Vec<(String, String)> {
    if let Some(value) = explicit {
        return vec![("Authorization".into(), format!("Bearer {value}"))];
    }
    for key in ["OSDK_HF_TOKEN", "HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return vec![("Authorization".into(), format!("Bearer {value}"))];
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_encoded_metadata_and_file_urls() {
        let metadata =
            metadata_url("https://hub.example.test", "owner/repo", "feature/branch").unwrap();
        assert!(metadata.contains("/api/models/owner/repo/revision/feature%2Fbranch"));
        assert!(metadata.contains("blobs=true"));
        let file = file_url(
            "https://hub.example.test",
            "owner/repo",
            "abc123",
            "weights/model.bin",
        )
        .unwrap();
        assert_eq!(
            file,
            "https://hub.example.test/owner/repo/resolve/abc123/weights/model.bin"
        );
    }
}
