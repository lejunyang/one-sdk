use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::model::provider::huggingface::HuggingFace;
use crate::model::provider::modelscope::ModelScope;
use crate::model::provider::{ModelProvider, RemoteModelFile};
use crate::model::{ModelRef, ProviderId};
use crate::source::{ProbeCache, ProbeResult, Selection, Source};

pub fn default_sources(provider: ProviderId) -> Vec<Source> {
    match provider {
        ProviderId::HuggingFace => vec![
            Source::official("official", "https://huggingface.co"),
            // Built in rather than left to `source add`, because being built in is
            // what earns a mirror the right not to appear in the lock.
            // `canonical_provider_endpoint` and `source::canonical_upstream_url`
            // both fold only *declared* mirrors back to upstream; a user-added
            // source is `Custom`, so neither recognises it and the mirror's
            // hostname lands in `[models.<name>].endpoint` for everyone replaying
            // that lock -- including people who cannot reach it.
            //
            // Declaring this a mirror asserts the content is the same, and that was
            // measured before adding it: for `openai-community/gpt2` the revision
            // sha matched upstream exactly (607a30d7…) and `config.json` came back
            // byte-identical (665 bytes, same SHA-256).
            //
            // `Source::mirror` sets `forward_credentials: false`, so a Hugging Face
            // token is never sent here.
            Source::mirror("hf-mirror", "https://hf-mirror.com", 10),
        ],
        ProviderId::ModelScope => {
            let mut international = Source::official("modelscope-ai", "https://www.modelscope.ai");
            international.priority = 10;
            vec![
                Source::official("modelscope-cn", "https://modelscope.cn"),
                international,
            ]
        }
    }
}

/// The provider's own endpoint, for an endpoint that may be a mirror of it.
///
/// A model is identified by provider, repository and immutable revision, and the
/// lock already pins every file's SHA-256. The host is therefore not part of what
/// the lock promises -- it is how one machine reached it. Recording a mirror made
/// a local convenience into everyone's locked source: `--endpoint`, `HF_ENDPOINT`
/// and `MODELSCOPE_ENDPOINT` all flowed straight into `[models.<name>].endpoint`.
///
/// Only a provider's built-in endpoints are treated as equivalent. A custom
/// endpoint is returned unchanged: osdk cannot know which provider's content it
/// serves, and claiming otherwise would put a false origin in a committed file.
pub fn canonical_provider_endpoint(provider: ProviderId, endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    let known = default_sources(provider);
    let is_builtin = known
        .iter()
        .any(|source| source.download_url.trim_end_matches('/') == trimmed);
    if !is_builtin {
        return endpoint.to_string();
    }
    // The official source is the canonical one; ModelScope declares two built-in
    // hosts (`modelscope.cn` and `www.modelscope.ai`), and both should lock to
    // the same identity rather than to whichever answered faster.
    known
        .iter()
        .find(|source| matches!(source.kind, crate::source::SourceKind::Official))
        .map(|source| source.download_url.clone())
        .unwrap_or_else(|| endpoint.to_string())
}

pub fn effective_sources(ctx: &Ctx, provider: ProviderId) -> Vec<Source> {
    let mut sources = default_sources(provider);
    if let Some(config) = ctx.config.tool_sources(provider.as_str()) {
        sources.retain(|source| !config.disable.iter().any(|id| id == &source.id));
        for custom in &config.custom {
            sources.retain(|source| source.id != custom.id);
            sources.push(custom.clone());
        }
    }
    sources.retain(|source| source.enabled);
    sources.sort_by_key(|source| source.priority);
    sources
}

pub async fn ranked_sources(ctx: &Ctx, reference: &ModelRef, refresh: bool) -> Result<Vec<Source>> {
    let sources = effective_sources(ctx, reference.provider);
    if sources.is_empty() {
        return Err(Error::NoUsableSource {
            tool: reference.provider.to_string(),
            tried: 0,
        });
    }
    if let Some(pin) = ctx
        .config
        .tool_sources(reference.provider.as_str())
        .and_then(|config| config.pin.as_deref())
    {
        if let Some(index) = sources.iter().position(|source| source.id == pin) {
            let mut ordered = sources;
            let pinned = ordered.remove(index);
            ordered.insert(0, pinned);
            return Ok(ordered);
        }
    }
    if ctx.config.settings.offline
        || matches!(
            ctx.config.sources.selection,
            Selection::Ordered | Selection::Pinned
        )
    {
        return Ok(sources);
    }

    let results = if refresh {
        let results = probe_all(ctx, reference, &sources).await;
        save_cache(ctx, reference, &sources, &results);
        results
    } else if let Some(cache) = fresh_cache(ctx, reference, &sources) {
        cache.results
    } else {
        let results = probe_all(ctx, reference, &sources).await;
        save_cache(ctx, reference, &sources, &results);
        results
    };
    let mut results = results;
    results.sort_by(|left, right| right.score().total_cmp(&left.score()));
    let mut ranked = Vec::with_capacity(sources.len());
    for result in results.iter().filter(|result| result.ok) {
        if let Some(source) = sources.iter().find(|source| source.id == result.source_id) {
            ranked.push(source.clone());
        }
    }
    for source in sources {
        if !ranked.iter().any(|ranked| ranked.id == source.id) {
            ranked.push(source);
        }
    }
    Ok(ranked)
}

pub async fn refresh(ctx: &Ctx, reference: &ModelRef) -> Result<Vec<ProbeResult>> {
    if ctx.config.settings.offline {
        return Err(Error::other("cannot refresh model sources while offline"));
    }
    let sources = effective_sources(ctx, reference.provider);
    let results = probe_all(ctx, reference, &sources).await;
    save_cache(ctx, reference, &sources, &results);
    Ok(results)
}

pub async fn probe_all(ctx: &Ctx, reference: &ModelRef, sources: &[Source]) -> Vec<ProbeResult> {
    let timeout = Duration::from_millis(ctx.config.sources.model_probe_timeout_ms);
    let mut handles = Vec::with_capacity(sources.len());
    for source in sources {
        let ctx = ProbeContext {
            client: ctx.client.clone(),
            dirs: ctx.dirs.clone(),
            config: ctx.config.clone(),
            cas: ctx.cas.clone(),
            platform: ctx.platform,
        };
        let reference = reference.clone();
        let source = source.clone();
        handles.push(tokio::spawn(async move {
            probe_one(ctx, reference, source.clone(), timeout)
                .await
                .unwrap_or_else(|_| ProbeResult::failed(&source.id))
        }));
    }
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }
    results
}

struct ProbeContext {
    client: reqwest::Client,
    dirs: crate::dirs::Dirs,
    config: crate::config::Config,
    cas: std::sync::Arc<crate::store::Cas>,
    platform: crate::platform::Platform,
}

impl ProbeContext {
    fn as_ctx(&self) -> Ctx {
        Ctx {
            dirs: self.dirs.clone(),
            platform: self.platform,
            config: self.config.clone(),
            client: self.client.clone(),
            cas: self.cas.clone(),
            show_progress: false,
        }
    }
}

const MODEL_PROBE_SAMPLE_BYTES: u64 = 64 * 1024;

async fn probe_one(
    probe: ProbeContext,
    reference: ModelRef,
    source: Source,
    timeout: Duration,
) -> Result<ProbeResult> {
    let provider = provider(reference.provider, source.forward_credentials);
    let ctx = probe.as_ctx();
    // Metadata and sample transfer have independent budgets. Sharing one budget
    // made two individually healthy 1-second phases fail a 1.5-second probe.
    let snapshot = tokio::time::timeout(
        timeout,
        provider.resolve(&ctx, &reference, &source.download_url),
    )
    .await
    .map_err(|_| Error::other("model source metadata probe timed out"))??;
    let file = probe_file(&snapshot.files)
        .ok_or_else(|| Error::other("model source returned no probeable files"))?;
    let headers = header_map(&file.headers)?;
    let start = Instant::now();
    let response = tokio::time::timeout(
        timeout,
        probe
            .client
            .get(&file.url)
            .headers(headers)
            .header(RANGE, format!("bytes=0-{}", MODEL_PROBE_SAMPLE_BYTES - 1))
            .send(),
    )
    .await
    .map_err(|_| Error::other("model source response-header probe timed out"))?
    .map_err(|error| Error::network(&file.url, error))?
    .error_for_status()
    .map_err(|error| Error::network(&file.url, error))?;
    let ttfb = start.elapsed();
    let body_start = Instant::now();
    let downloaded = tokio::time::timeout(timeout, read_probe_sample(response)).await;
    // Receiving a successful response already proves reachability. If the sample
    // cannot complete within the throughput budget, keep the source usable with
    // unknown throughput instead of misclassifying it as unreachable.
    let downloaded = match downloaded {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            tracing::debug!(source = %source.id, %error, "model source reachable but sample read failed");
            0
        }
        Err(_) => {
            tracing::debug!(source = %source.id, "model source reachable but sample read timed out");
            0
        }
    };
    Ok(ProbeResult {
        source_id: source.id,
        throughput: if downloaded == 0 {
            0.0
        } else {
            downloaded as f64 / body_start.elapsed().as_secs_f64().max(0.001)
        },
        ttfb_ms: ttfb.as_millis() as u64,
        ok: true,
        measured_at: crate::source::now_secs(),
    })
}

async fn read_probe_sample(response: reqwest::Response) -> Result<u64> {
    let url = response.url().to_string();
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::network(&url, error))?;
        downloaded += chunk.len() as u64;
        if downloaded >= MODEL_PROBE_SAMPLE_BYTES {
            break;
        }
    }
    Ok(downloaded)
}

fn probe_file(files: &[RemoteModelFile]) -> Option<&RemoteModelFile> {
    // The largest blob is the worst possible reachability probe: CDN cold-start
    // cost grows with the artifact while reachability does not. Prefer the
    // smallest non-empty file, falling back only when sizes are unavailable.
    files
        .iter()
        .filter(|file| file.size.unwrap_or_default() > 0)
        .min_by_key(|file| file.size.unwrap_or_default())
        .or_else(|| files.first())
}

pub fn provider(provider: ProviderId, allow_auth: bool) -> Box<dyn ModelProvider> {
    match provider {
        ProviderId::HuggingFace => Box::new(HuggingFace::new(allow_auth)),
        ProviderId::ModelScope => Box::new(ModelScope::new(allow_auth)),
    }
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

fn cache_path(ctx: &Ctx, reference: &ModelRef, sources: &[Source]) -> std::path::PathBuf {
    let mut key = format!(
        "{}:{}@{}",
        reference.provider, reference.repository, reference.revision
    );
    for source in sources {
        key.push('\0');
        key.push_str(&source.id);
        key.push('\0');
        key.push_str(&source.download_url);
        key.push('\0');
        key.push_str(if source.forward_credentials {
            "credentials"
        } else {
            "anonymous"
        });
    }
    let hash = blake3::hash(key.as_bytes()).to_hex().to_string();
    ctx.dirs
        .sources_cache()
        .join("models")
        .join(reference.provider.as_str())
        .join(format!("{hash}.json"))
}

fn fresh_cache(ctx: &Ctx, reference: &ModelRef, sources: &[Source]) -> Option<ProbeCache> {
    let bytes = std::fs::read(cache_path(ctx, reference, sources)).ok()?;
    let cache: ProbeCache = serde_json::from_slice(&bytes).ok()?;
    let now = crate::source::now_secs();
    let ttl = ctx.config.sources.cache_ttl_secs();
    (!cache.results.is_empty()
        && cache
            .results
            .iter()
            .all(|result| now.saturating_sub(result.measured_at) <= ttl))
    .then_some(cache)
}

fn save_cache(ctx: &Ctx, reference: &ModelRef, sources: &[Source], results: &[ProbeResult]) {
    if !results.iter().any(|result| result.ok) {
        return;
    }
    let path = cache_path(ctx, reference, sources);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&ProbeCache {
        results: results.to_vec(),
    }) {
        let _ = std::fs::write(path, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use crate::config::{Config, Settings, ToolSources};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::store::Cas;

    /// The built-in mirror must be usable as a download source *and* be invisible
    /// to the lock.
    ///
    /// Being built in is the entire mechanism: `canonical_provider_endpoint` folds
    /// only declared mirrors, so before this the same host added via `source add`
    /// was `Custom` and its hostname was committed into
    /// `[models.<name>].endpoint`, pushing everyone who replayed that lock through
    /// one machine's mirror.
    #[test]
    fn the_builtin_huggingface_mirror_is_a_mirror_and_never_reaches_the_lock() {
        let sources = default_sources(ProviderId::HuggingFace);
        let mirror = sources
            .iter()
            .find(|source| source.id == "hf-mirror")
            .expect("huggingface must declare a built-in mirror");
        assert!(matches!(mirror.kind, crate::source::SourceKind::Mirror));
        // A mirror is anonymous: a Hugging Face token must not be sent to it.
        assert!(
            !mirror.forward_credentials,
            "a mirror must not receive provider credentials"
        );
        // Ranked after the official source rather than ahead of it.
        let official = sources
            .iter()
            .find(|source| matches!(source.kind, crate::source::SourceKind::Official))
            .expect("official source");
        assert!(mirror.priority > official.priority);

        // The property that matters: pulling through the mirror locks upstream.
        assert_eq!(
            canonical_provider_endpoint(ProviderId::HuggingFace, &mirror.download_url),
            "https://huggingface.co",
            "the built-in mirror must fold to upstream in the lock"
        );
    }

    #[test]
    fn a_mirror_endpoint_locks_as_the_providers_own() {
        // `--endpoint` / `HF_ENDPOINT` used to flow straight into the lock, so a
        // machine behind a mirror committed that mirror as everyone's source.
        assert_eq!(
            canonical_provider_endpoint(ProviderId::HuggingFace, "https://huggingface.co"),
            "https://huggingface.co"
        );
        // Both of ModelScope's built-in hosts collapse to the official one, so
        // the lock does not depend on which answered faster.
        assert_eq!(
            canonical_provider_endpoint(ProviderId::ModelScope, "https://www.modelscope.ai"),
            "https://modelscope.cn"
        );
        assert_eq!(
            canonical_provider_endpoint(ProviderId::ModelScope, "https://modelscope.cn/"),
            "https://modelscope.cn"
        );
        // A custom endpoint is left untouched: osdk cannot know whose content it
        // serves, and a committed file must not carry an invented origin.
        assert_eq!(
            canonical_provider_endpoint(ProviderId::HuggingFace, "https://hf-mirror.example"),
            "https://hf-mirror.example"
        );
    }

    #[test]
    fn model_sources_keep_credentials_off_custom_endpoints_by_default() {
        let official = default_sources(ProviderId::HuggingFace);
        assert!(official[0].forward_credentials);
        let custom = Source::mirror("custom", "https://mirror.example.test", 1);
        assert!(!custom.forward_credentials);
    }

    #[tokio::test]
    async fn source_probe_uses_target_model_range_and_cached_ranking() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
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
                assert!(!request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer"));
                if request_number == 0 {
                    let body =
                        r#"{"sha":"abc123","siblings":[{"rfilename":"weights.bin","size":4}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    assert!(request
                        .to_ascii_lowercase()
                        .contains("range: bytes=0-65535"));
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4\r\nConnection: close\r\n\r\ndata",
                        )
                        .unwrap();
                }
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        let mut source = Source::mirror("fixture", &format!("http://{address}"), 0);
        source.forward_credentials = false;
        ctx.config.sources.per_tool.insert(
            "huggingface".into(),
            ToolSources {
                custom: vec![source],
                disable: vec!["official".into()],
                ..Default::default()
            },
        );
        let reference = ModelRef::parse("hf:owner/repo@main").unwrap();
        let first = ranked_sources(&ctx, &reference, false).await.unwrap();
        server.join().unwrap();
        assert_eq!(first[0].id, "fixture");
        let second = ranked_sources(&ctx, &reference, false).await.unwrap();
        assert_eq!(second[0].id, "fixture");
    }

    #[test]
    fn probe_prefers_a_small_file_over_the_largest_blob() {
        let files = vec![
            RemoteModelFile {
                path: "weights.safetensors".into(),
                url: "https://example.test/weights".into(),
                size: Some(7_000_000_000),
                sha256: None,
                etag: None,
                headers: Vec::new(),
            },
            RemoteModelFile {
                path: "config.json".into(),
                url: "https://example.test/config".into(),
                size: Some(1024),
                sha256: None,
                etag: None,
                headers: Vec::new(),
            },
        ];
        assert_eq!(probe_file(&files).unwrap().path, "config.json");
    }

    #[tokio::test]
    async fn response_headers_prove_reachability_even_when_sample_body_is_slow() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
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
                if request_number == 0 {
                    let body =
                        r#"{"sha":"abc123","siblings":[{"rfilename":"config.json","size":65536}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 65536\r\nContent-Range: bytes 0-65535/65536\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(150));
                    let _ = stream.write_all(b"late");
                }
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path());
        let probe = ProbeContext {
            client: ctx.client.clone(),
            dirs: ctx.dirs.clone(),
            config: ctx.config.clone(),
            cas: ctx.cas.clone(),
            platform: ctx.platform,
        };
        let source = Source::mirror("slow-body", &format!("http://{address}"), 0);
        let result = probe_one(
            probe,
            ModelRef::parse("hf:owner/repo@main").unwrap(),
            source,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert!(result.ok, "response headers already proved reachability");
        assert_eq!(result.throughput, 0.0, "timed-out throughput is unknown");
    }

    #[test]
    fn all_failed_probe_results_are_not_cached_for_the_normal_ttl() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path());
        let reference = ModelRef::parse("hf:owner/repo@main").unwrap();
        let sources = vec![Source::mirror("dead", "https://dead.example", 0)];
        save_cache(&ctx, &reference, &sources, &[ProbeResult::failed("dead")]);
        assert!(
            !cache_path(&ctx, &reference, &sources).exists(),
            "a transient all-failed result must not be cached for six hours"
        );
    }

    fn test_ctx(root: &std::path::Path) -> Ctx {
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
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}
