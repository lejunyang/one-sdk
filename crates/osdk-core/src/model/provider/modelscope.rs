use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::provider::{get_cached_json, ModelProvider, RemoteModelFile, RemoteSnapshot};
use crate::model::{ModelRef, ProviderId};

pub struct ModelScope {
    token: Option<String>,
    allow_auth: bool,
}

impl ModelScope {
    pub fn new(allow_auth: bool) -> Self {
        Self {
            token: None,
            allow_auth,
        }
    }

    #[cfg(test)]
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
            allow_auth: true,
        }
    }
}

impl Default for ModelScope {
    fn default() -> Self {
        Self::new(true)
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    #[serde(rename = "Code")]
    code: i64,
    #[serde(rename = "Success", default)]
    success: bool,
    #[serde(rename = "Message", default)]
    message: String,
    #[serde(rename = "Data")]
    data: T,
}

#[derive(Debug, Deserialize)]
struct FilesData {
    #[serde(rename = "Files", default)]
    files: Vec<FileInfo>,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "Sha256")]
    sha256: String,
    #[serde(rename = "Type", default)]
    file_type: String,
}

#[async_trait]
impl ModelProvider for ModelScope {
    async fn resolve(
        &self,
        ctx: &Ctx,
        reference: &ModelRef,
        endpoint: &str,
    ) -> Result<RemoteSnapshot> {
        if reference.provider != ProviderId::ModelScope {
            return Err(Error::config(format!(
                "ModelScope provider cannot resolve {}",
                reference.provider
            )));
        }
        let endpoint = endpoint.trim_end_matches('/');
        let metadata_url = files_url(endpoint, &reference.repository, &reference.revision)?;
        let headers = if self.allow_auth {
            auth_headers(self.token.as_deref())
        } else {
            Vec::new()
        };
        let cache_identity = format!(
            "{}:{}:{}@{}",
            reference.provider, endpoint, reference.repository, reference.revision
        );
        let response: ApiResponse<FilesData> = get_cached_json(
            ctx,
            reference.provider.as_str(),
            &cache_identity,
            &metadata_url,
            &headers,
        )
        .await?;
        if !response.success || response.code != 200 {
            return Err(Error::other(format!(
                "ModelScope API failed with code {}: {}",
                response.code, response.message
            )));
        }

        let mut files = Vec::new();
        for file in response.data.files {
            if file.file_type.eq_ignore_ascii_case("tree") {
                continue;
            }
            crate::model::safe_relative_path(&file.path)?;
            if file.sha256.len() != 64
                || !file
                    .sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(Error::other(format!(
                    "ModelScope file {} has no valid SHA-256",
                    file.path
                )));
            }
            files.push(RemoteModelFile {
                url: download_url(
                    endpoint,
                    &reference.repository,
                    &reference.revision,
                    &file.path,
                )?,
                path: file.path,
                size: Some(file.size),
                etag: Some(file.sha256.clone()),
                sha256: Some(file.sha256),
                headers: headers.clone(),
            });
        }
        if files.is_empty() {
            return Err(Error::other(format!(
                "ModelScope repository {}@{} contains no files",
                reference.repository, reference.revision
            )));
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let revision = manifest_revision(&reference.revision, &files);
        Ok(RemoteSnapshot {
            revision,
            endpoint: endpoint.to_string(),
            files,
        })
    }
}

fn files_url(endpoint: &str, repository: &str, revision: &str) -> Result<String> {
    let mut url = repo_url(endpoint, repository)?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::config("ModelScope endpoint cannot be a base URL"))?;
        segments.extend(["repo", "files"]);
    }
    url.query_pairs_mut()
        .append_pair("Revision", revision)
        .append_pair("Recursive", "true");
    Ok(url.into())
}

fn download_url(
    endpoint: &str,
    repository: &str,
    revision: &str,
    file_path: &str,
) -> Result<String> {
    let mut url = repo_url(endpoint, repository)?;
    url.path_segments_mut()
        .map_err(|_| Error::config("ModelScope endpoint cannot be a base URL"))?
        .push("repo");
    url.query_pairs_mut()
        .append_pair("Revision", revision)
        .append_pair("FilePath", file_path);
    Ok(url.into())
}

fn repo_url(endpoint: &str, repository: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config(format!("invalid ModelScope endpoint: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::config("ModelScope endpoint cannot be a base URL"))?;
        segments.pop_if_empty();
        segments.extend(["api", "v1", "models"]);
        for part in repository.split('/') {
            segments.push(part);
        }
    }
    Ok(url)
}

fn manifest_revision(requested: &str, files: &[RemoteModelFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.size.unwrap_or_default().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(file.sha256.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
    }
    format!("{requested}+manifest-{}", &hasher.finalize().to_hex()[..16])
}

fn auth_headers(explicit: Option<&str>) -> Vec<(String, String)> {
    let token = explicit.map(str::to_string).or_else(|| {
        ["OSDK_MODELSCOPE_TOKEN", "MODELSCOPE_API_TOKEN"]
            .iter()
            .find_map(|key| {
                std::env::var(key).ok().and_then(|value| {
                    let value = value.trim().to_string();
                    (!value.is_empty()).then_some(value)
                })
            })
    });
    match token {
        Some(token) => vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("Cookie".into(), format!("m_session_id={token}")),
        ],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_modelscope_urls_and_stable_manifest_revision() {
        let files = files_url(
            "https://modelscope.example.test",
            "owner/repo",
            "release/v1",
        )
        .unwrap();
        assert_eq!(
            files,
            "https://modelscope.example.test/api/v1/models/owner/repo/repo/files?Revision=release%2Fv1&Recursive=true"
        );
        let download = download_url(
            "https://modelscope.example.test",
            "owner/repo",
            "master",
            "weights/model.safetensors",
        )
        .unwrap();
        assert_eq!(
            download,
            "https://modelscope.example.test/api/v1/models/owner/repo/repo?Revision=master&FilePath=weights%2Fmodel.safetensors"
        );
        let files = vec![RemoteModelFile {
            path: "config.json".into(),
            size: Some(10),
            sha256: Some("a".repeat(64)),
            etag: None,
            url: String::new(),
            headers: Vec::new(),
        }];
        assert_eq!(
            manifest_revision("master", &files),
            manifest_revision("master", &files)
        );
        assert!(manifest_revision("master", &files).starts_with("master+manifest-"));
    }
}
