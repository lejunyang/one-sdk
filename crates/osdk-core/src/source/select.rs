//! Source selection: given a backend's default sources (plus user config),
//! pick which one to use — by pin, priority order, or fastest-probe (auto).
//!
//! Probing and disk-cached results are the meat of M3; this module also exposes
//! `active_source` used by every backend to resolve a single source now.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::backend::{Backend, Ctx};
use crate::error::{Error, Result};
use crate::source::env::{apply_env_source, read_env_source};
use crate::source::{
    candidate_fingerprint, ProbeCache, ProbeResult, Selection, Source, SourceKind,
};

const SOURCE_CACHE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct VersionedProbeCache {
    schema_version: u32,
    candidate_fingerprint: String,
    results: Vec<ProbeResult>,
}

impl VersionedProbeCache {
    fn new(candidate_fingerprint: String, results: Vec<ProbeResult>) -> Self {
        Self {
            schema_version: SOURCE_CACHE_SCHEMA_VERSION,
            candidate_fingerprint,
            results,
        }
    }

    fn is_compatible(&self, candidate_fingerprint: &str) -> bool {
        self.schema_version == SOURCE_CACHE_SCHEMA_VERSION
            && self.candidate_fingerprint == candidate_fingerprint
    }

    fn into_probe_cache(self) -> ProbeCache {
        ProbeCache {
            results: self.results,
        }
    }
}

/// Assemble the effective source list including any ambient mirror variable.
///
/// This is [`effective_sources`] plus the environment-derived candidate. It is
/// fallible because `--source-mode env` turns a missing or malformed variable
/// into an error instead of a silent fallback.
///
/// The ambient candidate is folded in *before* pin handling in
/// [`ranked_source_candidates`], so an explicit pin or `--source` still wins; the
/// environment only competes when no source was chosen deliberately.
pub fn effective_sources_with_env(ctx: &Ctx, backend: &dyn Backend) -> Result<Vec<Source>> {
    let sources = effective_sources(ctx, backend);
    let Some(mirror) = backend.env_mirror() else {
        // A backend with no native mirror variable has nothing to reconcile, and
        // `--source-mode env` cannot apply to it.
        return Ok(sources);
    };
    let env = read_env_source(
        &mirror,
        |name| std::env::var(name).ok(),
        |url| backend.validate_env_endpoint(url),
    );
    apply_env_source(
        ctx.config.sources.mode,
        backend.id(),
        sources,
        env,
        |message| tracing::warn!("{message}"),
    )
}

/// Assemble the effective source list for a backend: defaults minus disabled,
/// plus user custom sources, honoring per-tool config.
pub fn effective_sources(ctx: &Ctx, backend: &dyn Backend) -> Vec<Source> {
    let mut sources = backend.default_sources();
    if let Some(tool_cfg) = ctx.config.tool_sources(backend.id()) {
        if !tool_cfg.disable.is_empty() {
            sources.retain(|s| !tool_cfg.disable.iter().any(|d| d == &s.id));
        }
        for custom in &tool_cfg.custom {
            // custom overrides a builtin with the same id
            sources.retain(|s| s.id != custom.id);
            sources.push(custom.clone());
        }
    }
    sources.retain(|s| s.enabled);
    // Stable order by priority (lower first) for the `ordered` strategy.
    sources.sort_by_key(|s| s.priority);
    sources
}

/// Resolve the single source to use right now for `backend`.
///
/// - `pin` (config) always wins if the id exists.
/// - `Selection::Auto` uses cached probe results when fresh, else probes.
/// - `Selection::Ordered` returns the highest-priority enabled source.
pub async fn active_source(ctx: &Ctx, backend: &dyn Backend) -> Result<Source> {
    ranked_source_list(ctx, backend)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::NoUsableSource {
            tool: backend.id().to_string(),
            tried: 0,
        })
}

/// The full list of candidate sources, best-first. Used for download failover:
/// callers try each in order until one succeeds.
///
/// A config pin (or one-shot `--source`) moves that source to the front.
pub async fn ranked_source_list(ctx: &Ctx, backend: &dyn Backend) -> Result<Vec<Source>> {
    ranked_source_candidates(ctx, backend, effective_sources_with_env(ctx, backend)?).await
}

/// Rank an already-filtered effective source set with the same pin, cache,
/// offline, and live-probe policy as [`ranked_source_list`]. Backends use this
/// when they must narrow candidates before any network probe, for example to
/// keep private package names away from public registries.
pub async fn ranked_source_candidates(
    ctx: &Ctx,
    backend: &dyn Backend,
    sources: Vec<Source>,
) -> Result<Vec<Source>> {
    if sources.is_empty() {
        return Err(Error::NoUsableSource {
            tool: backend.id().to_string(),
            tried: 0,
        });
    }

    // Explicit pin wins: put it first, keep the rest as fallbacks.
    if let Some(tool_cfg) = ctx.config.tool_sources(backend.id()) {
        if let Some(pin) = &tool_cfg.pin {
            if let Some(idx) = sources.iter().position(|s| &s.id == pin) {
                let mut ordered = sources.clone();
                let pinned = ordered.remove(idx);
                let mut out = vec![pinned];
                out.extend(ordered);
                return Ok(out);
            }
        }
    }

    if ctx.config.settings.offline {
        if matches!(ctx.config.sources.selection, Selection::Auto) {
            if let Some(cache) = load_cache(ctx, backend.id(), &sources) {
                return Ok(order_sources_by_probe_results(sources, cache.results));
            }
        }
        return Ok(sources);
    }

    match ctx.config.sources.selection {
        Selection::Ordered | Selection::Pinned => Ok(sources),
        Selection::Auto => {
            let ranked_ids = ranked_sources(ctx, backend, &sources).await;
            Ok(order_sources_by_ids(sources, &ranked_ids))
        }
    }
}

fn order_sources_by_probe_results(
    sources: Vec<Source>,
    mut results: Vec<ProbeResult>,
) -> Vec<Source> {
    results.sort_by(|left, right| right.score().total_cmp(&left.score()));
    let ids = results
        .into_iter()
        .filter(|result| result.ok)
        .map(|result| result.source_id)
        .collect::<Vec<_>>();
    order_sources_by_ids(sources, &ids)
}

fn order_sources_by_ids(sources: Vec<Source>, ids: &[String]) -> Vec<Source> {
    // Append candidates without a successful probe so they remain available
    // as fallbacks after the ranked sources.
    let mut ordered = Vec::with_capacity(sources.len());
    for id in ids {
        if let Some(source) = sources.iter().find(|source| &source.id == id) {
            ordered.push(source.clone());
        }
    }
    for source in sources {
        if !ordered.iter().any(|ranked| ranked.id == source.id) {
            ordered.push(source);
        }
    }
    ordered
}

/// Return source ids ranked best-first, using fresh cache or a live probe.
async fn ranked_sources(ctx: &Ctx, backend: &dyn Backend, sources: &[Source]) -> Vec<String> {
    // Try fresh cache first.
    if let Some(cache) = load_cache(ctx, backend.id(), sources) {
        let ttl = ctx.config.sources.cache_ttl_secs();
        let now = crate::source::now_secs();
        let fresh = cache
            .results
            .iter()
            .all(|r| now.saturating_sub(r.measured_at) <= ttl)
            && !cache.results.is_empty();
        if fresh {
            let mut results = cache.results.clone();
            results.sort_by(|a, b| b.score().total_cmp(&a.score()));
            return results
                .into_iter()
                .filter(|r| r.ok)
                .map(|r| r.source_id)
                .collect();
        }
    }

    // Live probe.
    let results = probe_all(ctx, backend, sources).await;
    save_cache(ctx, backend.id(), sources, &results);
    let mut ok: Vec<ProbeResult> = results.into_iter().filter(|r| r.ok).collect();
    ok.sort_by(|a, b| b.score().total_cmp(&a.score()));
    ok.into_iter().map(|r| r.source_id).collect()
}

/// Probe every source concurrently, returning results (failed ones included).
pub async fn probe_all(ctx: &Ctx, backend: &dyn Backend, sources: &[Source]) -> Vec<ProbeResult> {
    let timeout = Duration::from_millis(ctx.config.sources.probe_timeout_ms);
    let mut handles = Vec::new();
    for s in sources {
        let url = backend.probe_url(ctx, s);
        let client = ctx.client.clone();
        let source = s.clone();
        let to = timeout;
        handles.push(tokio::spawn(async move {
            match url {
                Some(u) => probe_one(&client, &source, &u, to).await,
                None => ProbeResult::failed(&source.id),
            }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r)
        }
    }
    out
}

/// Probe a single URL: measure time-to-first-byte and throughput over a bounded
/// window, downloading at most ~1MB.
async fn probe_one(
    client: &reqwest::Client,
    source: &Source,
    url: &str,
    timeout: Duration,
) -> ProbeResult {
    use futures_util::StreamExt;

    let start = Instant::now();
    let fut = async {
        let resp = crate::http::get_source_response(client, source, url)
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let ttfb = start.elapsed();
        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let body_start = Instant::now();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => {
                    downloaded += c.len() as u64;
                    if downloaded >= 1_000_000 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let secs = body_start.elapsed().as_secs_f64().max(0.001);
        let throughput = downloaded as f64 / secs;
        Some((ttfb, throughput, downloaded))
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(Some((ttfb, throughput, downloaded))) if downloaded > 0 => ProbeResult {
            source_id: source.id.clone(),
            throughput,
            ttfb_ms: ttfb.as_millis() as u64,
            ok: true,
            measured_at: crate::source::now_secs(),
        },
        _ => ProbeResult::failed(&source.id),
    }
}

fn cache_path(ctx: &Ctx, tool: &str) -> PathBuf {
    let mut path = ctx
        .dirs
        .sources_cache()
        .join(crate::dirs::sanitize_tool_id(tool));
    path.set_extension("json");
    path
}

fn read_versioned_cache(path: &Path, candidate_fingerprint: &str) -> Option<ProbeCache> {
    let bytes = std::fs::read(path).ok()?;
    let cache: VersionedProbeCache = serde_json::from_slice(&bytes).ok()?;
    cache
        .is_compatible(candidate_fingerprint)
        .then(|| cache.into_probe_cache())
}

fn load_cache(ctx: &Ctx, tool: &str, sources: &[Source]) -> Option<ProbeCache> {
    let candidate_fingerprint = candidate_fingerprint(sources);
    read_versioned_cache(&cache_path(ctx, tool), &candidate_fingerprint)
}

fn save_cache(ctx: &Ctx, tool: &str, sources: &[Source], results: &[ProbeResult]) {
    let p = cache_path(ctx, tool);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = VersionedProbeCache::new(candidate_fingerprint(sources), results.to_vec());
    if let Ok(bytes) = serde_json::to_vec_pretty(&cache) {
        let _ = std::fs::write(&p, bytes);
    }
}

/// Force a refresh of the probe cache for a backend (used by `osdk source test`
/// and `--refresh-sources`). Returns the fresh results.
pub async fn refresh(ctx: &Ctx, backend: &dyn Backend) -> Result<Vec<ProbeResult>> {
    if ctx.config.settings.offline {
        return Err(Error::other("cannot refresh sources while offline"));
    }
    // Probe exactly what selection will rank, ambient candidate included;
    // otherwise `source test` would measure a different set than `install` uses
    // and the cached fingerprint would never match.
    let sources = effective_sources_with_env(ctx, backend)?;
    let results = probe_all(ctx, backend, &sources).await;
    save_cache(ctx, backend.id(), &sources, &results);
    Ok(results)
}

/// Human-readable kind label.
pub fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Official => "official",
        SourceKind::Mirror => "mirror",
        SourceKind::Custom => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::config::{Config, Settings};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::store::Cas;
    use crate::version::{ToolVersion, VersionInfo};

    struct FixtureBackend {
        id: &'static str,
        sources: Vec<Source>,
    }

    #[async_trait]
    impl Backend for FixtureBackend {
        fn id(&self) -> &str {
            self.id
        }

        fn default_sources(&self) -> Vec<Source> {
            self.sources.clone()
        }

        fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
            Some(source.download_url.clone())
        }

        async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
            Ok(Vec::new())
        }

        async fn install(
            &self,
            _ctx: &crate::backend::InstallCtx<'_>,
            _tv: &ToolVersion,
        ) -> Result<()> {
            Ok(())
        }

        fn bin_paths(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn cache_path_uses_nested_sanitized_tool_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        assert_eq!(
            cache_path(&ctx, "github:owner/repo"),
            ctx.dirs
                .sources_cache()
                .join("github")
                .join("owner")
                .join("repo.json")
        );
        assert_eq!(
            cache_path(&ctx, "../:owner\\\\repo"),
            ctx.dirs.sources_cache().join("owner").join("repo.json")
        );
    }

    #[test]
    fn load_cache_treats_legacy_cache_as_stale() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        let sources = vec![Source::mirror("fixture", "https://mirror.example.test", 1)];
        let legacy_path = cache_path(&ctx, "tool");
        std::fs::write(
            &legacy_path,
            serde_json::to_vec_pretty(&ProbeCache {
                results: vec![ProbeResult {
                    source_id: "fixture".into(),
                    throughput: 10.0,
                    ttfb_ms: 1,
                    ok: true,
                    measured_at: crate::source::now_secs(),
                }],
            })
            .unwrap(),
        )
        .unwrap();

        assert!(load_cache(&ctx, "tool", &sources).is_none());
    }

    #[test]
    fn load_cache_rejects_changed_candidate_sets() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        let base_sources = vec![
            Source::mirror("fixture", "https://mirror.example.test", 1),
            Source::mirror("extra", "https://extra.example.test", 2),
        ];
        let cache_path = cache_path(&ctx, "tool");
        save_cache(
            &ctx,
            "tool",
            &base_sources,
            &[
                ProbeResult {
                    source_id: "fixture".into(),
                    throughput: 10.0,
                    ttfb_ms: 1,
                    ok: true,
                    measured_at: crate::source::now_secs(),
                },
                ProbeResult {
                    source_id: "extra".into(),
                    throughput: 5.0,
                    ttfb_ms: 2,
                    ok: true,
                    measured_at: crate::source::now_secs(),
                },
            ],
        );

        let mut changed_download = base_sources.clone();
        changed_download[0].download_url = "https://other.example.test".into();
        assert!(load_cache(&ctx, "tool", &changed_download).is_none());

        let mut changed_priority = base_sources.clone();
        changed_priority[0].priority = 99;
        assert!(load_cache(&ctx, "tool", &changed_priority).is_none());

        let mut changed_enabled = base_sources.clone();
        changed_enabled[0].enabled = false;
        assert!(load_cache(&ctx, "tool", &changed_enabled).is_none());

        let changed_add = vec![
            base_sources[0].clone(),
            base_sources[1].clone(),
            Source::mirror("third", "https://third.example.test", 3),
        ];
        assert!(load_cache(&ctx, "tool", &changed_add).is_none());

        let changed_remove = vec![base_sources[0].clone()];
        assert!(load_cache(&ctx, "tool", &changed_remove).is_none());

        assert!(cache_path.is_file());
    }

    #[tokio::test]
    async fn ranked_sources_reprobes_when_candidates_change() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
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
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
                    )
                    .unwrap();
            }
        });

        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        let backend = FixtureBackend {
            id: "tool",
            sources: vec![Source::mirror("fixture", &format!("http://{address}"), 1)],
        };

        let first = ranked_sources(&ctx, &backend, &backend.sources).await;
        assert_eq!(first, vec!["fixture"]);

        let second = ranked_sources(&ctx, &backend, &backend.sources).await;
        assert_eq!(second, vec!["fixture"]);

        let changed_sources = vec![
            Source::mirror("fixture", &format!("http://{address}"), 1),
            Source::mirror("new", &format!("http://{address}"), 2),
        ];
        let mut third = ranked_sources(&ctx, &backend, &changed_sources).await;
        third.sort();
        assert_eq!(third, vec!["fixture", "new"]);

        server.join().unwrap();
    }

    #[tokio::test]
    async fn source_probe_applies_explicit_headers_without_forward_credentials() {
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
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.contains("x-probe-key: source-secret"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
                .unwrap();
        });

        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), false);
        let mut source = Source::mirror("fixture", &format!("http://{address}/"), 1);
        source.forward_credentials = false;
        source.headers = vec![("X-Probe-Key".into(), "source-secret".into())];
        let backend = FixtureBackend {
            id: "tool",
            sources: vec![source.clone()],
        };

        let results = probe_all(&ctx, &backend, &[source]).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].ok);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn offline_ranked_source_list_reuses_compatible_probe_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let online_ctx = test_ctx(temporary.path(), false);
        let backend = FixtureBackend {
            id: "github:owner/repo",
            sources: vec![
                Source::mirror("first", "https://fast.example.test", 20),
                Source::mirror("second", "https://slow.example.test", 10),
            ],
        };
        let effective = effective_sources(&online_ctx, &backend);
        save_cache(
            &online_ctx,
            backend.id(),
            &effective,
            &[
                ProbeResult {
                    source_id: "first".into(),
                    throughput: 100.0,
                    ttfb_ms: 10,
                    ok: true,
                    measured_at: crate::source::now_secs(),
                },
                ProbeResult {
                    source_id: "second".into(),
                    throughput: 10.0,
                    ttfb_ms: 100,
                    ok: true,
                    measured_at: crate::source::now_secs(),
                },
            ],
        );

        let ranked = ranked_sources(&online_ctx, &backend, &effective).await;
        assert_eq!(ranked, vec!["first", "second"]);

        let mut offline_ctx = test_ctx(temporary.path(), true);
        offline_ctx.config.sources.selection = Selection::Auto;
        let resolved = ranked_source_list(&offline_ctx, &backend).await.unwrap();
        let order: Vec<_> = resolved.into_iter().map(|source| source.id).collect();
        assert_eq!(order, vec!["first", "second"]);
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
        let settings = Settings {
            offline,
            ..Default::default()
        };
        Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config: Config {
                settings,
                sources: Default::default(),
                tools: BTreeMap::new(),
                tool_configs: BTreeMap::new(),
                global_tools: BTreeMap::new(),
                global_tool_configs: BTreeMap::new(),
                tool_origins: BTreeMap::new(),
                aliases: BTreeMap::new(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}
