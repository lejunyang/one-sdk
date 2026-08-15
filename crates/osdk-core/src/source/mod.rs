//! Multi-source model: each SDK ships a default list of sources (official +
//! authoritative mirrors); users can add custom sources or pin one. The
//! selection strategy (auto/pinned/ordered) is applied by [`select`].

use serde::{Deserialize, Serialize};

pub mod select;

/// A URL template with `{version}`, `{os}`, `{arch}`, `{file}`, `{ext}`
/// placeholders that backends substitute at download time.
pub type UrlTemplate = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// The canonical upstream source.
    Official,
    /// A well-known mirror/proxy.
    Mirror,
    /// A user-provided source.
    Custom,
}

/// A single download source for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    /// Version-index / metadata endpoint (may differ from the download host).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_url: Option<UrlTemplate>,
    /// Base URL for archive downloads.
    pub download_url: UrlTemplate,
    /// Extra request headers (e.g. a GitHub token for python-build-standalone).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Lower is preferred when strategy is `ordered` and no probe data exists.
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Source {
    pub fn official(id: &str, download_url: &str) -> Source {
        Source {
            id: id.to_string(),
            kind: SourceKind::Official,
            index_url: None,
            download_url: download_url.to_string(),
            headers: Vec::new(),
            priority: 0,
            enabled: true,
        }
    }

    pub fn mirror(id: &str, download_url: &str, priority: i32) -> Source {
        Source {
            id: id.to_string(),
            kind: SourceKind::Mirror,
            index_url: None,
            download_url: download_url.to_string(),
            headers: Vec::new(),
            priority,
            enabled: true,
        }
    }

    pub fn with_index(mut self, index_url: &str) -> Source {
        self.index_url = Some(index_url.to_string());
        self
    }
}

/// How to pick among sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    /// Probe candidates and pick the fastest (cached with TTL).
    #[default]
    Auto,
    /// Always use the pinned source id (falls back to ordered on failure).
    Pinned,
    /// Try in priority order, first reachable wins.
    Ordered,
}

/// Result of a speed probe against one source, cached to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub source_id: String,
    /// Measured throughput in bytes/sec (higher is better). 0 = failed.
    pub throughput: f64,
    /// Time-to-first-byte in milliseconds (lower is better, tiebreak).
    pub ttfb_ms: u64,
    /// Whether the probe succeeded.
    pub ok: bool,
    /// Unix epoch seconds when this probe was taken.
    pub measured_at: u64,
}

impl ProbeResult {
    pub fn failed(source_id: &str) -> ProbeResult {
        ProbeResult {
            source_id: source_id.to_string(),
            throughput: 0.0,
            ttfb_ms: u64::MAX,
            ok: false,
            measured_at: now_secs(),
        }
    }

    /// Score used for ranking: throughput primary, ttfb as tiebreak.
    pub fn score(&self) -> f64 {
        if !self.ok {
            return f64::MIN;
        }
        // Throughput dominates; subtract a small ttfb penalty.
        self.throughput - (self.ttfb_ms as f64)
    }
}

/// Cached probe results for a tool, with a measured timestamp for TTL checks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbeCache {
    pub results: Vec<ProbeResult>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_prefers_higher_throughput() {
        let a = ProbeResult { source_id: "a".into(), throughput: 100.0, ttfb_ms: 50, ok: true, measured_at: 0 };
        let b = ProbeResult { source_id: "b".into(), throughput: 200.0, ttfb_ms: 60, ok: true, measured_at: 0 };
        assert!(b.score() > a.score());
    }

    #[test]
    fn failed_probe_scores_lowest() {
        let f = ProbeResult::failed("x");
        let ok = ProbeResult { source_id: "y".into(), throughput: 1.0, ttfb_ms: 999, ok: true, measured_at: 0 };
        assert!(ok.score() > f.score());
    }

    #[test]
    fn source_builders() {
        let s = Source::mirror("tuna", "https://mirrors.tuna.tsinghua.edu.cn/nodejs-release/", 10)
            .with_index("https://mirrors.tuna.tsinghua.edu.cn/nodejs-release/index.json");
        assert_eq!(s.kind, SourceKind::Mirror);
        assert!(s.index_url.is_some());
        assert!(s.enabled);
    }

    #[test]
    fn probe_results_rank_best_first() {
        let mut results = vec![
            ProbeResult { source_id: "slow".into(), throughput: 10.0, ttfb_ms: 500, ok: true, measured_at: 0 },
            ProbeResult::failed("dead"),
            ProbeResult { source_id: "fast".into(), throughput: 5000.0, ttfb_ms: 300, ok: true, measured_at: 0 },
        ];
        results.sort_by(|a, b| b.score().total_cmp(&a.score()));
        let order: Vec<&str> = results.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(order, vec!["fast", "slow", "dead"]);
    }
}
