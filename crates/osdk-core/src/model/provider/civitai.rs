use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::provider::{get_cached_json, ModelProvider, RemoteModelFile, RemoteSnapshot};
use crate::model::{ModelRef, ProviderId};

pub struct Civitai {
    token: Option<String>,
    allow_auth: bool,
}

impl Civitai {
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

impl Default for Civitai {
    fn default() -> Self {
        Self::new(true)
    }
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    id: u64,
    #[serde(rename = "modelId")]
    model_id: u64,
    model: ModelInfo,
    #[serde(default)]
    files: Vec<FileInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    #[serde(rename = "type")]
    model_type: String,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    name: String,
    #[serde(default, rename = "sizeKB")]
    size_kb: Option<f64>,
    #[serde(default, rename = "type")]
    file_type: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    metadata: FileMetadata,
    #[serde(default)]
    hashes: FileHashes,
    #[serde(rename = "downloadUrl")]
    download_url: String,
}

#[derive(Debug, Default, Deserialize)]
struct FileMetadata {
    #[serde(default, rename = "format")]
    format: String,
}

#[derive(Debug, Default, Deserialize)]
struct FileHashes {
    #[serde(default, rename = "SHA256")]
    sha256: String,
}

#[async_trait]
impl ModelProvider for Civitai {
    async fn resolve(
        &self,
        ctx: &Ctx,
        reference: &ModelRef,
        endpoint: &str,
    ) -> Result<RemoteSnapshot> {
        if reference.provider != ProviderId::Civitai {
            return Err(Error::config(format!(
                "Civitai provider cannot resolve {}",
                reference.provider
            )));
        }
        let version_id = parse_id(&reference.revision, "model version")?;
        let endpoint = endpoint.trim_end_matches('/');
        let metadata_url = version_url(endpoint, version_id)?;
        let headers = if self.allow_auth {
            auth_headers(self.token.as_deref())
        } else {
            Vec::new()
        };
        let auth_identity = headers
            .first()
            .map(|(_, value)| blake3::hash(value.as_bytes()).to_hex().to_string())
            .unwrap_or_else(|| "anonymous".into());
        let cache_identity = format!(
            "{}:{}:{}:{}",
            reference.provider, endpoint, version_id, auth_identity
        );
        let info: VersionInfo = get_cached_json(
            ctx,
            reference.provider.as_str(),
            &cache_identity,
            &metadata_url,
            &headers,
        )
        .await?;
        if info.id != version_id {
            return Err(Error::other(format!(
                "Civitai returned model version {} for requested version {version_id}",
                info.id
            )));
        }
        let model_id = parse_id(&reference.repository, "model")?;
        if info.model_id != model_id {
            return Err(Error::other(format!(
                "Civitai returned model {} for requested model {model_id}",
                info.model_id
            )));
        }
        if !is_lora_type(&info.model.model_type) {
            return Err(Error::other(format!(
                "Civitai model {model_id} is type `{}`; only LoRA-family resources are supported",
                info.model.model_type
            )));
        }

        let selected = select_weight_file(info.files)?;
        let snapshot_path = format!("loras/{}", selected.name);
        crate::model::safe_relative_path(&snapshot_path)?;
        let sha256 = normalized_sha256(&selected.hashes.sha256, &selected.name)?;
        let size = size_bytes(selected.size_kb, &selected.name)?;
        let download_url = validate_download_url(&selected.download_url)?;
        let file_headers = if trusted_download_origin(endpoint, &download_url) {
            headers
        } else {
            Vec::new()
        };

        Ok(RemoteSnapshot {
            revision: version_id.to_string(),
            endpoint: endpoint.to_string(),
            files: vec![RemoteModelFile {
                path: snapshot_path,
                size: Some(size),
                sha256: Some(sha256.clone()),
                etag: Some(sha256),
                url: download_url,
                headers: file_headers,
            }],
        })
    }
}

fn is_lora_type(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "lora" | "locon" | "lycoris" | "dora"
    )
}

fn select_weight_file(files: Vec<FileInfo>) -> Result<FileInfo> {
    files
        .into_iter()
        .enumerate()
        .filter(|(_, file)| {
            let name = file.name.to_ascii_lowercase();
            file.file_type.eq_ignore_ascii_case("model")
                && matches!(
                    name.rsplit_once('.').map(|(_, extension)| extension),
                    Some("safetensors" | "ckpt" | "pt")
                )
        })
        .min_by_key(|(index, file)| {
            let safe_tensor = file.name.to_ascii_lowercase().ends_with(".safetensors")
                || file.metadata.format.eq_ignore_ascii_case("safetensor");
            (!safe_tensor, !file.primary, *index)
        })
        .map(|(_, file)| file)
        .ok_or_else(|| Error::other("Civitai model version contains no supported weight file"))
}

fn version_url(endpoint: &str, version_id: u64) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config(format!("invalid Civitai endpoint: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::config("Civitai endpoint cannot be a base URL"))?;
        segments.pop_if_empty();
        segments.extend(["api", "v1", "model-versions", &version_id.to_string()]);
    }
    Ok(url.into())
}

fn validate_download_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| Error::other(format!("invalid Civitai download URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::other(format!(
            "unsupported Civitai download URL scheme `{}`",
            url.scheme()
        )));
    }
    Ok(url.into())
}

fn trusted_download_origin(endpoint: &str, download_url: &str) -> bool {
    let Ok(endpoint) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let Ok(download) = reqwest::Url::parse(download_url) else {
        return false;
    };
    same_origin(&endpoint, &download)
        || (is_official_civitai_origin(&endpoint) && is_official_civitai_origin(&download))
}

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_official_civitai_origin(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("civitai.com") || host.eq_ignore_ascii_case("civitai.red")
        })
}

fn normalized_sha256(value: &str, name: &str) -> Result<String> {
    let value = value.trim();
    if value.len() != 64 || !value.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(Error::other(format!(
            "Civitai file {name} has no valid SHA-256"
        )));
    }
    Ok(value.to_ascii_lowercase())
}

fn size_bytes(value: Option<f64>, name: &str) -> Result<u64> {
    let Some(value) = value else {
        return Err(Error::other(format!("Civitai file {name} has no size")));
    };
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::other(format!(
            "Civitai file {name} has an invalid size"
        )));
    }
    let bytes = (value * 1024.0).round();
    if bytes > u64::MAX as f64 {
        return Err(Error::other(format!(
            "Civitai file {name} is too large to represent"
        )));
    }
    Ok(bytes as u64)
}

fn parse_id(value: &str, kind: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::config(format!("Civitai {kind} id must be a positive integer")))
}

fn auth_headers(explicit: Option<&str>) -> Vec<(String, String)> {
    let token = explicit.map(str::to_string).or_else(|| {
        ["OSDK_CIVITAI_TOKEN", "CIVITAI_API_TOKEN", "CIVITAI_TOKEN"]
            .iter()
            .find_map(|key| {
                std::env::var(key).ok().and_then(|value| {
                    let value = value.trim().to_string();
                    (!value.is_empty()).then_some(value)
                })
            })
    });
    token
        .map(|token| vec![("Authorization".into(), format!("Bearer {token}"))])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use crate::config::{Config, Settings};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::store::Cas;

    fn file(name: &str, format: &str, primary: bool) -> FileInfo {
        FileInfo {
            name: name.into(),
            size_kb: Some(1.0),
            file_type: "Model".into(),
            primary,
            metadata: FileMetadata {
                format: format.into(),
            },
            hashes: FileHashes {
                sha256: "a".repeat(64),
            },
            download_url: "https://civitai.com/api/download/models/123".into(),
        }
    }

    #[test]
    fn builds_version_url_and_prefers_safe_tensor_then_primary() {
        assert_eq!(
            version_url("https://civitai.com", 123).unwrap(),
            "https://civitai.com/api/v1/model-versions/123"
        );
        let selected = select_weight_file(vec![
            file("primary.ckpt", "PickleTensor", true),
            file("secondary.safetensors", "SafeTensor", false),
            file("primary-safe.safetensors", "SafeTensor", true),
        ])
        .unwrap();
        assert_eq!(selected.name, "primary-safe.safetensors");
    }

    #[tokio::test]
    async fn resolves_exact_version_with_token_and_selects_one_weight() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /api/v1/model-versions/123 HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-token"));
            let body = format!(
                r#"{{"id":123,"modelId":456,"model":{{"type":"LORA"}},"files":[{{"name":"unsafe.ckpt","sizeKB":1,"type":"Model","primary":true,"metadata":{{"format":"PickleTensor"}},"hashes":{{"SHA256":"{}"}},"downloadUrl":"http://{}/unsafe"}},{{"name":"lora.safetensors","sizeKB":2,"type":"Model","primary":false,"metadata":{{"format":"SafeTensor"}},"hashes":{{"SHA256":"{}"}},"downloadUrl":"https://download.example/lora.safetensors"}}]}}"#,
                "b".repeat(64),
                address,
                "a".repeat(64),
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let temporary = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let ctx = Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: crate::http::client().unwrap(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let reference = ModelRef::parse("civitai:456@123").unwrap();
        let snapshot = Civitai::with_token("fixture-token")
            .resolve(&ctx, &reference, &format!("http://{address}"))
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(snapshot.revision, "123");
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "loras/lora.safetensors");
        assert_eq!(snapshot.files[0].size, Some(2048));
        let expected_sha256 = "a".repeat(64);
        assert_eq!(
            snapshot.files[0].sha256.as_deref(),
            Some(expected_sha256.as_str())
        );
        assert!(
            snapshot.files[0].headers.is_empty(),
            "credentials must not be attached to a cross-origin download URL"
        );
    }

    #[test]
    fn rejects_missing_hash_and_limits_credentials_to_trusted_origins() {
        assert!(normalized_sha256("short", "model.safetensors").is_err());
        assert!(is_lora_type("LORA"));
        assert!(is_lora_type("LyCORIS"));
        assert!(!is_lora_type("Checkpoint"));
        assert!(trusted_download_origin(
            "https://civitai.com",
            "https://civitai.com/api/download/models/123"
        ));
        assert!(trusted_download_origin(
            "https://civitai.com",
            "https://civitai.red/api/download/models/123"
        ));
        assert!(trusted_download_origin(
            "https://civitai.red",
            "https://civitai.com/api/download/models/123"
        ));
        assert!(!trusted_download_origin(
            "https://civitai.red",
            "https://download.example/model.safetensors"
        ));
        assert!(!trusted_download_origin(
            "https://mirror.example",
            "https://civitai.red/api/download/models/123"
        ));
    }
}
