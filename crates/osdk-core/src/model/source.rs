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
        ProviderId::HuggingFace => vec![Source::official("official", "https://huggingface.co")],
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
    let timeout = Duration::from_millis(ctx.config.sources.probe_timeout_ms);
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
            match tokio::time::timeout(timeout, probe_one(ctx, reference, source.clone())).await {
                Ok(Ok(result)) => result,
                _ => ProbeResult::failed(&source.id),
            }
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

async fn probe_one(
    probe: ProbeContext,
    reference: ModelRef,
    source: Source,
) -> Result<ProbeResult> {
    let provider = provider(reference.provider, source.forward_credentials);
    let ctx = probe.as_ctx();
    let snapshot = provider
        .resolve(&ctx, &reference, &source.download_url)
        .await?;
    let file = probe_file(&snapshot.files)
        .ok_or_else(|| Error::other("model source returned no probeable files"))?;
    let headers = header_map(&file.headers)?;
    let start = Instant::now();
    let response = probe
        .client
        .get(&file.url)
        .headers(headers)
        .header(RANGE, "bytes=0-1048575")
        .send()
        .await
        .map_err(|error| Error::network(&file.url, error))?
        .error_for_status()
        .map_err(|error| Error::network(&file.url, error))?;
    let ttfb = start.elapsed();
    let mut stream = response.bytes_stream();
    let body_start = Instant::now();
    let mut downloaded = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::network(&file.url, error))?;
        downloaded += chunk.len() as u64;
        if downloaded >= 1_048_576 {
            break;
        }
    }
    if downloaded == 0 {
        return Err(Error::other("model source probe returned no bytes"));
    }
    Ok(ProbeResult {
        source_id: source.id,
        throughput: downloaded as f64 / body_start.elapsed().as_secs_f64().max(0.001),
        ttfb_ms: ttfb.as_millis() as u64,
        ok: true,
        measured_at: crate::source::now_secs(),
    })
}

fn probe_file(files: &[RemoteModelFile]) -> Option<&RemoteModelFile> {
    files
        .iter()
        .filter(|file| file.size.unwrap_or_default() > 0)
        .max_by_key(|file| file.size.unwrap_or_default())
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
                        .contains("range: bytes=0-1048575"));
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
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}
