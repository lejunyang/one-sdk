use std::path::{Path, PathBuf};

use futures_util::stream::{self, StreamExt, TryStreamExt};
use globset::{Glob, GlobSet, GlobSetBuilder};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::provider::{ModelProvider, RemoteModelFile};
use crate::model::{DownloadedModelFile, InstalledModel, ModelRef, ModelStore, SnapshotIdentity};
use crate::pipeline::download;
use crate::pipeline::verify::{hash_file, HashAlgo};

#[derive(Debug, Clone, Default)]
pub struct PullOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub variant: Option<String>,
}

pub async fn pull(
    ctx: &Ctx,
    provider: &dyn ModelProvider,
    name: &str,
    reference: &ModelRef,
    endpoint: &str,
    options: &PullOptions,
) -> Result<InstalledModel> {
    let remote = provider.resolve(ctx, reference, endpoint).await?;
    let selector = FileSelector::new(&options.include, &options.exclude)?;
    let files: Vec<_> = remote
        .files
        .into_iter()
        .filter(|file| selector.matches(&file.path))
        .collect();
    if files.is_empty() {
        return Err(Error::other(format!(
            "model file selection for {reference} matched no files"
        )));
    }

    let jobs = ctx.config.settings.jobs.max(1);
    let revision = remote.revision.clone();
    let downloaded = stream::iter(
        files
            .into_iter()
            .map(|file| download_file(ctx, reference, &revision, file)),
    )
    .buffer_unordered(jobs)
    .try_collect::<Vec<_>>()
    .await?;
    let store = ModelStore::new(
        ctx.dirs.clone(),
        ctx.cas.clone(),
        ctx.config.settings.link_mode,
    );
    store.publish(
        SnapshotIdentity {
            name: name.to_string(),
            provider: reference.provider,
            repository: reference.repository.clone(),
            requested_revision: reference.revision.clone(),
            revision: remote.revision,
            endpoint: remote.endpoint,
            variant: options.variant.clone(),
        },
        downloaded,
    )
}

async fn download_file(
    ctx: &Ctx,
    reference: &ModelRef,
    revision: &str,
    file: RemoteModelFile,
) -> Result<DownloadedModelFile> {
    let relative = crate::model::safe_relative_path(&file.path)?;
    let destination = download_path(ctx, reference, revision, &relative);
    if ctx.config.settings.offline && !destination.is_file() {
        return Err(Error::other(format!(
            "offline model file cache miss for {} ({})",
            reference, file.path
        )));
    }
    if !ctx.config.settings.offline {
        let headers = header_map(&file.headers)?;
        download::download_with_headers(
            &ctx.client,
            &file.url,
            &destination,
            &format!("{}:{}", reference.repository, file.path),
            ctx.show_progress,
            &headers,
        )
        .await?;
    }
    let size = std::fs::metadata(&destination)
        .map_err(|error| Error::io(&destination, error))?
        .len();
    if let Some(expected) = file.size {
        if expected != size {
            if !ctx.config.settings.offline {
                let _ = std::fs::remove_file(&destination);
            }
            return Err(Error::other(format!(
                "model file size mismatch for {}: expected {}, got {}",
                file.path, expected, size
            )));
        }
    }
    let sha256 = match file.sha256 {
        Some(expected) => {
            crate::pipeline::verify::verify_file(
                &destination,
                &expected,
                HashAlgo::Sha256,
                &file.path,
            )?;
            Some(expected)
        }
        None => Some(hash_file(&destination, HashAlgo::Sha256)?),
    };
    Ok(DownloadedModelFile {
        path: file.path,
        source: destination,
        size,
        sha256,
        etag: file.etag,
    })
}

fn download_path(ctx: &Ctx, reference: &ModelRef, revision: &str, path: &Path) -> PathBuf {
    ctx.dirs
        .downloads()
        .join("models")
        .join(reference.provider.as_str())
        .join(crate::dirs::sanitize_tool_id(&reference.repository))
        .join(crate::dirs::sanitize_tool_id(revision))
        .join(path)
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        let key = HeaderName::from_bytes(key.as_bytes())
            .map_err(|error| Error::config(format!("invalid HTTP header `{key}`: {error}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| Error::config(format!("invalid HTTP header value: {error}")))?;
        map.insert(key, value);
    }
    Ok(map)
}

struct FileSelector {
    include: Option<GlobSet>,
    exclude: GlobSet,
}

impl FileSelector {
    fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: if include.is_empty() {
                None
            } else {
                Some(build_globs(include)?)
            },
            exclude: build_globs(exclude)?,
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.include
            .as_ref()
            .map(|include| include.is_match(path))
            .unwrap_or(true)
            && !self.exclude.is_match(path)
    }
}

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern)
                .map_err(|error| Error::config(format!("invalid model file glob: {error}")))?,
        );
    }
    builder
        .build()
        .map_err(|error| Error::config(format!("invalid model file globs: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use crate::config::{Config, Settings};
    use crate::dirs::Dirs;
    use crate::model::provider::huggingface::HuggingFace;
    use crate::model::provider::RemoteSnapshot;
    use crate::platform::Platform;
    use crate::store::link::LinkMode;
    use crate::store::Cas;

    #[test]
    fn selector_supports_includes_and_excludes() {
        let selector = FileSelector::new(
            &["*.json".into(), "*.safetensors".into()],
            &["tokenizer.json".into()],
        )
        .unwrap();
        assert!(selector.matches("config.json"));
        assert!(selector.matches("model.safetensors"));
        assert!(!selector.matches("tokenizer.json"));
        assert!(!selector.matches("README.md"));
    }

    #[tokio::test]
    async fn huggingface_pull_locks_revision_downloads_and_rebuilds_offline() {
        let config_bytes = br#"{"model":"fixture"}"#.to_vec();
        let digest = crate::pipeline::verify::hash_bytes(&config_bytes, HashAlgo::Sha256);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_digest = digest.clone();
        let server_bytes = config_bytes.clone();
        let server = std::thread::spawn(move || {
            for request_number in 0..2 {
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
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fixture-token"));
                if request_number == 0 {
                    let body = format!(
                        r#"{{"sha":"abc123","siblings":[{{"rfilename":"config.json","lfs":{{"sha256":"{}","size":{}}}}}]}}"#,
                        server_digest,
                        server_bytes.len()
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
                        server_bytes.len()
                    )
                    .unwrap();
                    stream.write_all(&server_bytes).unwrap();
                }
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), false);
        let reference = ModelRef::parse("hf:owner/repo@main").unwrap();
        let endpoint = format!("http://{address}");
        let installed = pull(
            &ctx,
            &HuggingFace::with_token("fixture-token"),
            "fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(installed.manifest.revision, "abc123");
        assert_eq!(
            std::fs::read(installed.path.join("config.json")).unwrap(),
            config_bytes
        );

        ModelStore::new(
            ctx.dirs.clone(),
            ctx.cas.clone(),
            ctx.config.settings.link_mode,
        )
        .remove("fixture")
        .unwrap();
        ctx.config.settings.offline = true;
        let rebuilt = pull(
            &ctx,
            &HuggingFace::default(),
            "fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(rebuilt.manifest.revision, "abc123");
    }

    struct EmptyProvider;

    #[async_trait]
    impl ModelProvider for EmptyProvider {
        async fn resolve(
            &self,
            _ctx: &Ctx,
            _reference: &ModelRef,
            _endpoint: &str,
        ) -> Result<RemoteSnapshot> {
            Ok(RemoteSnapshot {
                revision: "abc".into(),
                endpoint: "https://example.test".into(),
                files: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn empty_selection_fails_before_publish() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        let error = pull(
            &ctx,
            &EmptyProvider,
            "fixture",
            &ModelRef::parse("hf:owner/repo").unwrap(),
            "https://example.test",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("matched no files"));
    }

    fn test_ctx(root: &Path, offline: bool) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config: Config {
                settings: Settings {
                    offline,
                    link_mode: LinkMode::Copy,
                    ..Default::default()
                },
                sources: Default::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}
