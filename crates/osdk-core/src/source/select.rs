//! Source selection: given a backend's default sources (plus user config),
//! pick which one to use — by pin, priority order, or fastest-probe (auto).
//!
//! Probing and disk-cached results are the meat of M3; this module also exposes
//! `active_source` used by every backend to resolve a single source now.

use std::time::{Duration, Instant};

use crate::backend::{Backend, Ctx};
use crate::error::{Error, Result};
use crate::source::{ProbeCache, ProbeResult, Selection, Source, SourceKind};

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
    let sources = effective_sources(ctx, backend);
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

    match ctx.config.sources.selection {
        Selection::Ordered | Selection::Pinned => Ok(sources),
        Selection::Auto => {
            let ranked_ids = ranked_sources(ctx, backend, &sources).await;
            // Reorder `sources` by the ranked id order; append any not ranked
            // (e.g. failed probes) at the end so they can still be tried.
            let mut out: Vec<Source> = Vec::with_capacity(sources.len());
            for id in &ranked_ids {
                if let Some(s) = sources.iter().find(|s| &s.id == id) {
                    out.push(s.clone());
                }
            }
            for s in &sources {
                if !out.iter().any(|o| o.id == s.id) {
                    out.push(s.clone());
                }
            }
            Ok(out)
        }
    }
}

/// Return source ids ranked best-first, using fresh cache or a live probe.
async fn ranked_sources(ctx: &Ctx, backend: &dyn Backend, sources: &[Source]) -> Vec<String> {
    // Try fresh cache first.
    if let Some(cache) = load_cache(ctx, backend.id()) {
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
            return results.into_iter().filter(|r| r.ok).map(|r| r.source_id).collect();
        }
    }

    // Live probe.
    let results = probe_all(ctx, backend, sources).await;
    save_cache(ctx, backend.id(), &results);
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
        let id = s.id.clone();
        let to = timeout;
        handles.push(tokio::spawn(async move {
            match url {
                Some(u) => probe_one(&client, &id, &u, to).await,
                None => ProbeResult::failed(&id),
            }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        match h.await {
            Ok(r) => out.push(r),
            Err(_) => {}
        }
    }
    out
}

/// Probe a single URL: measure time-to-first-byte and throughput over a bounded
/// window, downloading at most ~1MB.
async fn probe_one(
    client: &reqwest::Client,
    id: &str,
    url: &str,
    timeout: Duration,
) -> ProbeResult {
    use futures_util::StreamExt;

    let start = Instant::now();
    let fut = async {
        let resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
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
            source_id: id.to_string(),
            throughput,
            ttfb_ms: ttfb.as_millis() as u64,
            ok: true,
            measured_at: crate::source::now_secs(),
        },
        _ => ProbeResult::failed(id),
    }
}

fn cache_path(ctx: &Ctx, tool: &str) -> std::path::PathBuf {
    ctx.dirs.sources_cache().join(format!("{tool}.json"))
}

fn load_cache(ctx: &Ctx, tool: &str) -> Option<ProbeCache> {
    let p = cache_path(ctx, tool);
    let bytes = std::fs::read(&p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_cache(ctx: &Ctx, tool: &str, results: &[ProbeResult]) {
    let p = cache_path(ctx, tool);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = ProbeCache {
        results: results.to_vec(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&cache) {
        let _ = std::fs::write(&p, bytes);
    }
}

/// Force a refresh of the probe cache for a backend (used by `osdk source test`
/// and `--refresh-sources`). Returns the fresh results.
pub async fn refresh(ctx: &Ctx, backend: &dyn Backend) -> Result<Vec<ProbeResult>> {
    let sources = effective_sources(ctx, backend);
    let results = probe_all(ctx, backend, &sources).await;
    save_cache(ctx, backend.id(), &results);
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
