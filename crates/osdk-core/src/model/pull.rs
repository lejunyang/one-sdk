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
    pub kind: Option<crate::model::ModelKind>,
    pub family: Option<String>,
    pub derived_from: Option<String>,
}

pub async fn pull(
    ctx: &Ctx,
    provider: &dyn ModelProvider,
    name: &str,
    reference: &ModelRef,
    endpoint: &str,
    options: &PullOptions,
) -> Result<InstalledModel> {
    let session = crate::cache::downloads::Session::begin(ctx).await?;
    let selector = FileSelector::new(&options.include, &options.exclude)?;
    if ctx.config.settings.offline {
        if let Some(installed) = replay_offline(ctx, name, reference, options, &selector)? {
            session.finish(ctx, &[]);
            return Ok(installed);
        }
    }
    let remote = provider.resolve(ctx, reference, endpoint).await?;
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
    let mut seen = std::collections::BTreeSet::new();
    for file in &files {
        let relative = crate::model::safe_relative_path(&file.path)?;
        let key: PathBuf = relative.components().collect();
        #[cfg(windows)]
        let key = PathBuf::from(key.to_string_lossy().to_lowercase());
        if !seen.insert(key) {
            return Err(Error::other("model provider returned duplicate file paths"));
        }
    }

    // Acquire every exact-file lock in canonical order and keep them through
    // publish. Releasing after HTTP alone allowed another pull's failed
    // validation to unlink a source while this transaction was ingesting it.
    let mut lock_paths = files
        .iter()
        .map(|file| {
            let relative = crate::model::safe_relative_path(&file.path)?;
            Ok(download_lock_path(&download_path(
                ctx,
                reference,
                &remote.revision,
                &relative,
            )))
        })
        .collect::<Result<Vec<_>>>()?;
    lock_paths.sort();
    lock_paths.dedup();
    let mut file_locks = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        crate::cache::downloads::safe_path(&ctx.dirs.cache, &path)?;
        file_locks.push(
            tokio::task::spawn_blocking(move || crate::lock::FileLock::acquire(path))
                .await
                .map_err(|error| Error::other(format!("model file lock failed: {error}")))??,
        );
    }
    let jobs = ctx.config.settings.jobs.max(1);
    let revision = remote.revision.clone();
    // A client with a read (no-progress) timeout, so a connection that dies
    // mid-stream fails and hits the retry+resume loop instead of hanging forever
    // (bug 007). Built once here and shared by every file's download.
    let download_client =
        crate::http::client_with_read_timeout(ctx.config.sources.model_read_timeout_ms)?;
    let downloaded = stream::iter(
        files
            .into_iter()
            .map(|file| download_file(ctx, &download_client, reference, &revision, file)),
    )
    .buffer_unordered(jobs)
    .try_collect::<Vec<_>>()
    .await?;
    let store = ModelStore::new(
        ctx.dirs.clone(),
        ctx.cas.clone(),
        ctx.config.settings.link_mode,
    );
    let paths = downloaded
        .iter()
        .map(|file| file.source.clone())
        .collect::<Vec<_>>();
    let installed = store.publish(
        SnapshotIdentity {
            name: name.to_string(),
            provider: reference.provider,
            repository: reference.repository.clone(),
            requested_revision: reference.revision.clone(),
            revision: remote.revision,
            endpoint: remote.endpoint,
            variant: options.variant.clone(),
            kind: crate::model::effective_model_kind(reference.provider, options.kind),
            family: options.family.clone(),
            derived_from: options.derived_from.clone(),
        },
        downloaded,
    )?;
    drop(file_locks);
    session.finish(ctx, &paths);
    Ok(installed)
}

async fn download_file(
    ctx: &Ctx,
    client: &reqwest::Client,
    reference: &ModelRef,
    revision: &str,
    file: RemoteModelFile,
) -> Result<DownloadedModelFile> {
    let relative = crate::model::safe_relative_path(&file.path)?;
    let destination = download_path(ctx, reference, revision, &relative);
    crate::cache::downloads::record(&ctx.dirs, &destination, false)?;
    if ctx.config.settings.offline && !destination.is_file() {
        return Err(Error::other(format!(
            "offline model file cache miss for {} ({}): {}; no matching snapshot manifest/CAS replay was available; retain downloads with a positive download_cache.max_size_bytes for cache-based offline pulls",
            reference, file.path, destination.display()
        )));
    }
    if !ctx.config.settings.offline {
        // pull holds the exact-file locks through validation and publication.
        let headers = header_map(&file.headers)?;
        download::download_with_headers_and_policy(
            client,
            &file.url,
            &destination,
            &format!("{}:{}", reference.repository, file.path),
            ctx.show_progress,
            &headers,
            download::RetryPolicy {
                max_attempts: ctx.config.sources.model_download_attempts,
                base_delay: std::time::Duration::from_millis(
                    ctx.config.sources.model_download_retry_base_ms,
                ),
                max_delay: std::time::Duration::from_secs(8),
                visible: true,
            },
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
            if let Err(error) = crate::pipeline::verify::verify_file(
                &destination,
                &expected,
                HashAlgo::Sha256,
                &file.path,
            ) {
                if !ctx.config.settings.offline {
                    let _ = std::fs::remove_file(&destination);
                }
                return Err(error);
            }
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

/// Replay only recorded snapshots: an arbitrary SHA-256 from a remote index
/// cannot identify a BLAKE3 CAS object. Never scan blobs looking for a match.
fn replay_offline(
    ctx: &Ctx,
    name: &str,
    reference: &ModelRef,
    options: &PullOptions,
    selector: &FileSelector,
) -> Result<Option<InstalledModel>> {
    let root = ctx.dirs.models();
    if !root.exists() {
        return Ok(None);
    }
    let mut snapshots = Vec::new();
    for item in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .max_depth(4)
    {
        let item =
            item.map_err(|error| Error::other(format!("offline model manifest scan: {error}")))?;
        if !item.file_type().is_file() || item.file_name() != super::MODEL_MANIFEST_FILE {
            continue;
        }
        let relative = item.path().strip_prefix(&root).unwrap();
        let parts: Vec<_> = relative.components().collect();
        if parts.len() != 4 || parts[1].as_os_str() != "snapshots" {
            continue;
        }
        let snapshot = parts[2].as_os_str().to_string_lossy();
        if snapshot.len() != 24 || !snapshot.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let bytes = crate::inventory::read_stable_regular_file(item.path(), 16 * 1024 * 1024)
            .map_err(|error| Error::io(item.path(), error))?;
        let manifest: super::SnapshotManifest =
            serde_json::from_slice(&bytes).map_err(|error| {
                Error::other(format!(
                    "invalid offline model manifest {}: {error}",
                    item.path().display()
                ))
            })?;
        if manifest.provider != reference.provider
            || manifest.repository != reference.repository
            || (manifest.requested_revision != reference.revision
                && manifest.revision != reference.revision)
            || manifest.variant != options.variant
        {
            continue;
        }
        if manifest.schema != 1 || manifest.files.is_empty() {
            return Err(Error::other("unsupported or empty offline model manifest"));
        }
        // Cross-check the CAS manifest rather than accepting detached hashes.
        let parent = item.path().parent().unwrap();
        let cas_path = crate::store::manifest::Manifest::manifest_path(parent);
        let bytes = crate::inventory::read_stable_regular_file(&cas_path, 16 * 1024 * 1024)
            .map_err(|error| {
                Error::other(format!(
                    "offline model CAS manifest missing/invalid at {}: {error}",
                    cas_path.display()
                ))
            })?;
        let cas_manifest: crate::store::manifest::Manifest = serde_json::from_slice(&bytes)?;
        if manifest.name != parts[0].as_os_str().to_string_lossy()
            || cas_manifest.tool != format!("model:{}", manifest.name)
            || cas_manifest.version != snapshot
        {
            return Err(Error::other(
                "offline model manifest identity does not match snapshot path",
            ));
        }
        for file in &manifest.files {
            crate::model::safe_relative_path(&file.path)?;
            if file.cas_hash.len() != 64
                || !file.cas_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !cas_manifest.files.iter().any(|entry| {
                    entry.path == file.path
                        && entry.hash.as_deref() == Some(&file.cas_hash)
                        && entry.symlink.is_none()
                })
            {
                return Err(Error::other(format!(
                    "offline model manifests disagree for {}",
                    file.path
                )));
            }
        }
        let current_path = parent
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(super::CURRENT_FILE);
        let current = crate::inventory::read_stable_regular_file(&current_path, 4096)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<super::CurrentSnapshot>(&bytes).ok())
            .is_some_and(|current| current.snapshot == snapshot);
        snapshots.push((manifest, current));
    }
    // An alias may have several recorded revisions. Prefer the named model,
    // then the newest recorded snapshot; no attempt to resolve an alias online.
    snapshots.sort_by(|(a, a_current), (b, b_current)| {
        (b.name == name)
            .cmp(&(a.name == name))
            .then(b_current.cmp(a_current))
            .then(b.created_at.cmp(&a.created_at))
            .then(a.revision.cmp(&b.revision))
    });
    let Some((manifest, _)) = snapshots.into_iter().find(|(manifest, _)| {
        manifest
            .files
            .iter()
            .any(|file| selector.matches(&file.path))
    }) else {
        return Ok(None);
    };
    let mut files = Vec::new();
    for file in manifest
        .files
        .iter()
        .filter(|file| selector.matches(&file.path))
    {
        let source = ctx.cas.object_path(&file.cas_hash);
        crate::cache::downloads::safe_path(&ctx.dirs.store, &source)?;
        let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            Error::other(format!(
                "offline model CAS blob missing for {}: {} ({error})",
                file.path,
                source.display()
            ))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != file.size {
            return Err(Error::other(format!(
                "invalid offline model CAS blob for {}: {}",
                file.path,
                source.display()
            )));
        }
        let actual = crate::store::hash_file(&source)?;
        if actual != file.cas_hash {
            return Err(Error::other(format!(
                "offline model CAS checksum mismatch for {}: expected {}, got {actual}",
                file.path, file.cas_hash
            )));
        }
        if let Some(expected) = &file.sha256 {
            crate::pipeline::verify::verify_file(&source, expected, HashAlgo::Sha256, &file.path)?;
        }
        files.push(DownloadedModelFile {
            path: file.path.clone(),
            source,
            size: file.size,
            sha256: file.sha256.clone(),
            etag: file.etag.clone(),
        });
    }
    let store = ModelStore::new(
        ctx.dirs.clone(),
        ctx.cas.clone(),
        ctx.config.settings.link_mode,
    );
    store
        .publish(
            SnapshotIdentity {
                name: name.to_string(),
                provider: manifest.provider,
                repository: manifest.repository,
                requested_revision: reference.revision.clone(),
                revision: manifest.revision,
                endpoint: manifest.endpoint,
                variant: options.variant.clone(),
                kind: crate::model::effective_model_kind(reference.provider, options.kind),
                family: options.family.clone(),
                derived_from: options.derived_from.clone(),
            },
            files,
        )
        .map(Some)
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

/// The cross-process lock path for a downloaded file: `<file>.osdk-lock`.
///
/// A distinct suffix so it is never confused with the `.partial`/`.partial.json`
/// the download itself writes, and built by appending to the full file name so a
/// real extension (like `.gguf`) is preserved rather than replaced.
fn download_lock_path(destination: &Path) -> PathBuf {
    let mut name = destination.as_os_str().to_os_string();
    name.push(".osdk-lock");
    PathBuf::from(name)
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
    use crate::model::provider::civitai::Civitai;
    use crate::model::provider::huggingface::HuggingFace;
    use crate::model::provider::modelscope::ModelScope;
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

    #[tokio::test]
    async fn modelscope_pull_verifies_manifest_and_rebuilds_offline() {
        let config_bytes = br#"{"model":"modelscope-fixture"}"#.to_vec();
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
                let lower = request.to_ascii_lowercase();
                assert!(lower.contains("authorization: bearer fixture-token"));
                assert!(lower.contains("cookie: m_session_id=fixture-token"));
                if request_number == 0 {
                    assert!(request.contains("/repo/files?Revision=master&Recursive=true"));
                    let body = format!(
                        r#"{{"Code":200,"Success":true,"Message":"success","Data":{{"Files":[{{"Path":"config.json","Size":{},"Sha256":"{}","Type":"blob"}}]}}}}"#,
                        server_bytes.len(),
                        server_digest
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    assert!(request.contains("/repo?Revision=master&FilePath=config.json"));
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
        let reference = ModelRef::parse("ms:owner/repo@master").unwrap();
        let endpoint = format!("http://{address}");
        let installed = pull(
            &ctx,
            &ModelScope::with_token("fixture-token"),
            "modelscope-fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert!(installed.manifest.revision.starts_with("master+manifest-"));
        assert_eq!(
            std::fs::read(installed.path.join("config.json")).unwrap(),
            config_bytes
        );

        ModelStore::new(
            ctx.dirs.clone(),
            ctx.cas.clone(),
            ctx.config.settings.link_mode,
        )
        .remove("modelscope-fixture")
        .unwrap();
        ctx.config.settings.offline = true;
        let rebuilt = pull(
            &ctx,
            &ModelScope::default(),
            "modelscope-fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(rebuilt.manifest.revision, installed.manifest.revision);
    }

    #[tokio::test]
    async fn civitai_pull_verifies_download_redirect_and_rebuilds_offline() {
        let weights = vec![42u8; 2048];
        let digest = crate::pipeline::verify::hash_bytes(&weights, HashAlgo::Sha256);
        let cdn = TcpListener::bind("127.0.0.1:0").unwrap();
        let cdn_address = cdn.local_addr().unwrap();
        let cdn_weights = weights.clone();
        let cdn_server = std::thread::spawn(move || {
            let (mut stream, _) = cdn.accept().unwrap();
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
            assert!(request.starts_with("GET /lora.safetensors HTTP/1.1"));
            assert!(
                !request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer"),
                "Civitai credentials must be stripped on a cross-origin CDN redirect"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
                cdn_weights.len()
            )
            .unwrap();
            stream.write_all(&cdn_weights).unwrap();
        });

        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_address = api.local_addr().unwrap();
        let server_digest = digest.clone();
        let api_server = std::thread::spawn(move || {
            for request_number in 0..2 {
                let (mut stream, _) = api.accept().unwrap();
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
                    assert!(request.starts_with("GET /api/v1/model-versions/123 HTTP/1.1"));
                    let body = format!(
                        r#"{{"id":123,"modelId":456,"model":{{"type":"LORA"}},"files":[{{"name":"lora.safetensors","sizeKB":2,"type":"Model","primary":true,"metadata":{{"format":"SafeTensor"}},"hashes":{{"SHA256":"{server_digest}"}},"downloadUrl":"http://{api_address}/api/download/models/123"}}]}}"#
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    assert!(request.starts_with("GET /api/download/models/123 HTTP/1.1"));
                    write!(
                        stream,
                        "HTTP/1.1 302 Found\r\nLocation: http://{cdn_address}/lora.safetensors\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), false);
        let reference = ModelRef::parse("civitai:456@123").unwrap();
        let endpoint = format!("http://{api_address}");
        let installed = pull(
            &ctx,
            &Civitai::with_token("fixture-token"),
            "civitai-fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        api_server.join().unwrap();
        cdn_server.join().unwrap();
        assert_eq!(
            installed.manifest.provider,
            crate::model::ProviderId::Civitai
        );
        assert_eq!(installed.manifest.repository, "456");
        assert_eq!(installed.manifest.requested_revision, "123");
        assert_eq!(installed.manifest.revision, "123");
        assert_eq!(installed.manifest.kind, Some(crate::model::ModelKind::Lora));
        assert_eq!(installed.manifest.files[0].path, "loras/lora.safetensors");
        assert_eq!(
            crate::model::view::comfyui::classify(
                &installed.manifest.files[0].path,
                &std::collections::BTreeMap::new(),
            )
            .unwrap()
            .category,
            "loras"
        );
        assert_eq!(
            installed.manifest.files[0].sha256.as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            std::fs::read(installed.path.join("loras/lora.safetensors")).unwrap(),
            weights
        );

        ModelStore::new(
            ctx.dirs.clone(),
            ctx.cas.clone(),
            ctx.config.settings.link_mode,
        )
        .remove("civitai-fixture")
        .unwrap();
        ctx.config.settings.offline = true;
        let rebuilt = pull(
            &ctx,
            &Civitai::with_token("fixture-token"),
            "civitai-fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(rebuilt.manifest.revision, "123");
        assert_eq!(
            std::fs::read(rebuilt.path.join("loras/lora.safetensors")).unwrap(),
            weights
        );
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

    #[test]
    fn download_lock_path_is_a_distinct_sibling_that_keeps_the_extension() {
        let dest = Path::new("/data/models/hf/org/repo/rev/qwen.gguf");
        let lock = download_lock_path(dest);
        // Distinct suffix, so it never collides with the download's own scratch
        // files, and the real extension is preserved rather than replaced.
        assert_eq!(
            lock,
            Path::new("/data/models/hf/org/repo/rev/qwen.gguf.osdk-lock")
        );
        assert_ne!(lock, dest.with_extension("partial"));
        assert!(lock.to_string_lossy().ends_with(".gguf.osdk-lock"));
    }

    // Bug 007: two processes pulling the same file resolve to the same
    // `download_path`, so the download now takes a cross-process lock on it. This
    // asserts the primitive that guard relies on -- a second acquirer of the same
    // lock path is refused while the first holds it, and succeeds once released.
    #[test]
    fn same_download_lock_path_is_mutually_exclusive() {
        let temporary = tempfile::tempdir().unwrap();
        let dest = temporary.path().join("weights.safetensors");
        let lock_path = download_lock_path(&dest);

        let first = crate::lock::FileLock::try_acquire(&lock_path).unwrap();
        assert!(first.is_some(), "first acquirer should get the lock");
        // While held, a second attempt on the same path is refused rather than
        // both proceeding to write the same partial.
        assert!(
            crate::lock::FileLock::try_acquire(&lock_path)
                .unwrap()
                .is_none(),
            "a second acquirer must be refused while the lock is held"
        );
        drop(first);
        // Released: the path is acquirable again.
        assert!(
            crate::lock::FileLock::try_acquire(&lock_path)
                .unwrap()
                .is_some(),
            "the lock should be free again after the holder drops"
        );
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

    pub(super) fn test_ctx(root: &Path, offline: bool) -> Ctx {
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
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                tool_aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::config::DownloadCacheLimit;
    use crate::model::provider::RemoteSnapshot;
    use async_trait::async_trait;

    struct FixtureProvider;
    #[async_trait]
    impl ModelProvider for FixtureProvider {
        async fn resolve(&self, ctx: &Ctx, _: &ModelRef, endpoint: &str) -> Result<RemoteSnapshot> {
            assert!(
                !ctx.config.settings.offline,
                "offline replay must never resolve remotely"
            );
            let bytes = b"model bytes";
            Ok(RemoteSnapshot {
                revision: "exact-revision".into(),
                endpoint: "https://invalid.example".into(),
                files: vec![RemoteModelFile {
                    path: "model.gguf".into(),
                    size: Some(bytes.len() as u64),
                    sha256: Some(crate::pipeline::verify::hash_bytes(bytes, HashAlgo::Sha256)),
                    etag: None,
                    url: format!("{endpoint}/model.gguf"),
                    headers: vec![],
                }],
            })
        }
    }

    fn fixture() -> (tempfile::TempDir, Ctx, ModelRef, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = super::tests::test_ctx(temporary.path(), false);
        ctx.config.settings.download_cache.max_size_bytes = DownloadCacheLimit::Bytes(0);
        let reference = ModelRef::parse("hf:owner/repo@main").unwrap();
        let cache = download_path(&ctx, &reference, "exact-revision", Path::new("model.gguf"));
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"model bytes").unwrap();
        (temporary, ctx, reference, cache)
    }

    #[tokio::test]
    async fn zero_retention_preserves_verified_cas_and_rebuilds_offline_without_download_metadata()
    {
        let (_temporary, mut ctx, reference, cache) = fixture();
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert!(!cache.exists());
        let hash = installed.manifest.files[0].cas_hash.clone();
        assert!(ctx.cas.object_path(&hash).is_file());
        std::fs::remove_dir_all(ctx.dirs.remote_cache()).unwrap();
        std::fs::remove_file(installed.path.join("model.gguf")).unwrap();
        ctx.config.settings.offline = true;
        let rebuilt = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(rebuilt.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
        assert!(rebuilt.path.join(".osdk-complete").is_file());
        assert!(!cache.exists());
        let clone = pull(
            &ctx,
            &FixtureProvider,
            "other-name",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(clone.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
        std::fs::remove_file(ctx.cas.object_path(&hash)).unwrap();
        let error = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("CAS blob missing"), "{error}");
        assert!(error.to_string().contains("model.gguf"));
        assert_eq!(
            std::fs::read(rebuilt.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
    }

    #[tokio::test]
    async fn failed_publish_keeps_download_for_retry_then_success_removes_oversized_copy() {
        let (_temporary, mut ctx, reference, cache) = fixture();
        let error = pull(
            &ctx,
            &FixtureProvider,
            "../invalid",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("model name"));
        assert!(cache.is_file());
        ctx.config.settings.download_cache.max_size_bytes = DownloadCacheLimit::Bytes(1);
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert!(!cache.exists());
        assert_eq!(
            std::fs::read(installed.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
    }

    #[tokio::test]
    async fn corrupt_or_missing_replay_metadata_and_corrupt_blobs_fail_closed() {
        let (_temporary, mut ctx, reference, _cache) = fixture();
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        ctx.config.settings.offline = true;
        let object = ctx.cas.object_path(&installed.manifest.files[0].cas_hash);
        std::fs::write(&object, b"wrong bytes").unwrap();
        let error = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert!(
            object.exists(),
            "offline verification must not delete a CAS blob"
        );
        let cas_manifest = installed.path.join(crate::store::manifest::MANIFEST_FILE);
        std::fs::remove_file(&cas_manifest).unwrap();
        let error = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("CAS manifest missing/invalid"),
            "{error}"
        );
        assert!(object.exists());
    }

    #[tokio::test]
    async fn real_http_download_larger_than_budget_commits_and_discards_only_the_copy() {
        use std::io::{Read, Write};
        let (_temporary, mut ctx, reference, cache) = fixture();
        std::fs::remove_file(&cache).unwrap();
        ctx.config.settings.download_cache.max_size_bytes = DownloadCacheLimit::Bytes(1);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nmodel bytes").unwrap();
        });
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            &endpoint,
            &PullOptions::default(),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(
            std::fs::read(installed.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
        assert!(ctx
            .cas
            .object_path(&installed.manifest.files[0].cas_hash)
            .exists());
        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn concurrent_zero_retention_pulls_keep_shared_sources_until_both_publish() {
        let (_temporary, ctx, reference, cache) = fixture();
        let options = PullOptions::default();
        let (first, second) = tokio::join!(
            pull(
                &ctx,
                &FixtureProvider,
                "first",
                &reference,
                "https://invalid.example",
                &options
            ),
            pull(
                &ctx,
                &FixtureProvider,
                "second",
                &reference,
                "https://invalid.example",
                &options
            )
        );
        for installed in [first.unwrap(), second.unwrap()] {
            assert_eq!(
                std::fs::read(installed.path.join("model.gguf")).unwrap(),
                b"model bytes"
            );
        }
        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn failed_offline_repair_restores_original_snapshot_metadata_and_missing_file_state() {
        let (_temporary, mut ctx, reference, _cache) = fixture();
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        let root = installed.path.parent().unwrap().parent().unwrap();
        let current_path = root.join(super::super::CURRENT_FILE);
        let current = std::fs::read(&current_path).unwrap();
        let manifest_path = installed.path.join(super::super::MODEL_MANIFEST_FILE);
        let mut manifest = installed.manifest.clone();
        manifest.created_at = 123;
        let original = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(&manifest_path, &original).unwrap();
        std::fs::remove_file(installed.path.join("model.gguf")).unwrap();
        std::fs::remove_file(&current_path).unwrap();
        std::fs::create_dir(&current_path).unwrap();
        ctx.config.settings.offline = true;
        assert!(pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default()
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&manifest_path).unwrap(), original);
        assert!(!installed.path.join("model.gguf").exists());
        assert_eq!(
            std::fs::read(ctx.cas.object_path(&manifest.files[0].cas_hash)).unwrap(),
            b"model bytes"
        );
        std::fs::remove_dir(&current_path).unwrap();
        std::fs::write(&current_path, current).unwrap();
        let repaired = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(repaired.path.join("model.gguf")).unwrap(),
            b"model bytes"
        );
    }

    #[tokio::test]
    async fn offline_repair_preserves_untracked_and_modified_snapshot_data() {
        let (_temporary, mut ctx, reference, _cache) = fixture();
        let installed = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap();
        let extra = installed.path.join("my-unique-model.bin");
        std::fs::write(&extra, b"user data").unwrap();
        std::fs::remove_file(installed.path.join("model.gguf")).unwrap();
        ctx.config.settings.offline = true;
        let error = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("untracked"), "{error}");
        assert_eq!(std::fs::read(&extra).unwrap(), b"user data");
        std::fs::remove_file(&extra).unwrap();
        std::fs::write(installed.path.join("model.gguf"), b"user edited").unwrap();
        let error = pull(
            &ctx,
            &FixtureProvider,
            "fixture",
            &reference,
            "https://invalid.example",
            &PullOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("modified"), "{error}");
        assert_eq!(
            std::fs::read(installed.path.join("model.gguf")).unwrap(),
            b"user edited"
        );
    }
}
