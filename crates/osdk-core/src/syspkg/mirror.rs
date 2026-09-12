//! Mirror candidates and speed probing for system package managers.
//!
//! This module plans and measures; it writes nothing. Applying a mirror is a
//! separate, explicitly confirmed step, mirroring how `container/mirror.rs`
//! keeps planning apart from `container/apply.rs`.
//!
//! Probing reuses `source::select`, with a pseudo-tool id standing in for the
//! backend a real download would name. That path already handles pins, the
//! probe cache and its fingerprint, `--refresh-sources`, offline mode, and the
//! one-shot `--source` override, and its documentation says outright that it
//! exists so "a non-backend downloader cannot drift into its own mirror
//! policy". Building a second probing mechanism here would be exactly that
//! drift.
//!
//! # What a mirror actually accelerates
//!
//! winget and Homebrew differ structurally, and stating it plainly is the whole
//! point of [`Acceleration`]:
//!
//! - A winget source is a manifest index. The manifests it serves carry
//!   `InstallerUrl` values pointing at the vendor's own servers, so a mirror
//!   speeds up *finding* a package and does nothing at all for *downloading*
//!   it.
//! - Homebrew bottles are hosted centrally and can be redirected wholesale, so
//!   a mirror accelerates both halves.
//!
//! A user who switches winget sources expecting faster downloads will conclude
//! the feature is broken. Reporting must say which half is affected.

use serde::Serialize;

use super::report::ManagerKind;
use crate::error::Result;
use crate::source::{select, Source, SourceKind};
use crate::Ctx;

/// Pseudo-tool id for winget index probing.
///
/// The `pkg:` prefix keeps these out of the namespace real tools occupy, so
/// `osdk source` configuration for a tool can never collide with a package
/// manager's mirror configuration.
pub const WINGET_SOURCE_TOOL: &str = "pkg:winget-source";

/// Which half of a package manager's work a mirror speeds up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Acceleration {
    /// Only package discovery. Installers still come from upstream.
    IndexOnly,
    /// Both metadata and the artifacts themselves.
    IndexAndArtifacts,
}

impl Acceleration {
    /// Whether switching to this mirror will make downloads faster.
    pub const fn accelerates_downloads(self) -> bool {
        matches!(self, Self::IndexAndArtifacts)
    }
}

/// A mirror osdk knows about, before any measurement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MirrorCandidate {
    /// Stable id used in configuration and in `--source`.
    pub id: &'static str,
    /// Base URL of the mirrored source.
    pub endpoint: &'static str,
    /// What this mirror speeds up.
    pub acceleration: Acceleration,
    /// Whether the mirror operator documents this endpoint.
    ///
    /// An undocumented endpoint may work today and vanish tomorrow, so osdk
    /// ranks it below documented ones and says so rather than hiding it.
    pub documented: bool,
}

/// Mirrors for the winget manifest index.
///
/// Contrary to a widespread impression, Chinese mirrors of the winget source do
/// exist. Two frequently cited ones do not, and are deliberately absent:
/// TUNA has no winget source, and the Tencent Cloud URL that circulates in
/// blog posts returns 404. That same blog post also recommends a
/// `winget source pin` command, which is not a winget subcommand at all --
/// a good reason to treat the whole source as unreliable.
///
/// Every entry here is index-only. That is not an oversight: a winget source
/// mirror cannot be anything else, because the manifests it serves point at
/// vendor download servers.
pub const WINGET_MIRRORS: &[MirrorCandidate] = &[
    MirrorCandidate {
        id: "ustc",
        endpoint: "https://mirrors.ustc.edu.cn/winget-source",
        acceleration: Acceleration::IndexOnly,
        documented: true,
    },
    MirrorCandidate {
        id: "nju",
        endpoint: "https://mirrors.nju.edu.cn/winget-source",
        acceleration: Acceleration::IndexOnly,
        // Reachable, but the mirror publishes no help page for it.
        documented: false,
    },
    MirrorCandidate {
        id: "huaweicloud",
        endpoint: "https://mirrors.huaweicloud.com/winget-source",
        acceleration: Acceleration::IndexOnly,
        documented: false,
    },
];

/// The official winget source, as the baseline every mirror is ranked against.
pub const WINGET_OFFICIAL_ENDPOINT: &str = "https://cdn.winget.microsoft.com/cache";

/// The file whose transfer speed stands in for a winget source.
///
/// A winget source is an MSIX package wrapping a SQLite index, and this is the
/// artifact winget itself downloads when it updates a source. Measuring the
/// real payload avoids the trap of timing a small metadata file and concluding
/// a mirror is fast when the thing users actually wait for is slow.
pub const WINGET_PROBE_FILE: &str = "source.msix";

/// Build the source candidate set for winget index probing.
///
/// The official endpoint is included so ranking can conclude that no mirror
/// beats it -- a real and useful outcome, especially outside China.
pub fn winget_sources() -> Vec<Source> {
    let mut sources = vec![Source::official("official", WINGET_OFFICIAL_ENDPOINT)];
    for (index, candidate) in WINGET_MIRRORS.iter().enumerate() {
        // Documented mirrors sort ahead of undocumented ones when no
        // measurement exists to separate them.
        let priority = if candidate.documented {
            index as i32 + 1
        } else {
            index as i32 + 100
        };
        sources.push(Source::mirror(candidate.id, candidate.endpoint, priority));
    }
    sources
}

/// The URL whose transfer speed stands in for a winget source.
pub fn winget_probe_url(source: &Source) -> Option<String> {
    let base = source.download_url.trim_end_matches('/');
    Some(format!("{base}/{WINGET_PROBE_FILE}"))
}

/// Look up what a known mirror id accelerates.
pub fn acceleration_of(manager: ManagerKind, source_id: &str) -> Option<Acceleration> {
    match manager {
        ManagerKind::Winget => {
            if source_id == "official" {
                return Some(Acceleration::IndexOnly);
            }
            WINGET_MIRRORS
                .iter()
                .find(|candidate| candidate.id == source_id)
                .map(|candidate| candidate.acceleration)
        }
        // Homebrew is not wired up yet; claiming knowledge here would be a lie.
        ManagerKind::Homebrew => None,
    }
}

/// One measured mirror, ready to report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MirrorMeasurement {
    pub source_id: String,
    pub endpoint: String,
    pub kind: SourceKind,
    pub acceleration: Acceleration,
    /// `None` when the source was not probed, for example when offline.
    pub reachable: Option<bool>,
    /// Observed time to first byte, when measured.
    pub ttfb_ms: Option<u64>,
    /// Observed throughput in bytes per second, when measured.
    pub throughput: Option<f64>,
}

/// How long a winget source probe gets.
///
/// `source.msix` is a real package index, not a small metadata file, so the
/// default 1.5s window tuned for version indexes would time every candidate
/// out, mark them all unreachable, and silently fall back to fixed priority
/// order — the opposite of measuring which route is faster. Probes run
/// concurrently, so this bounds the whole command rather than each source.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// The probe deadline, honouring a raised `probe_timeout_ms` but never dropping
/// below the floor an index-sized payload needs.
fn probe_timeout(ctx: &Ctx) -> std::time::Duration {
    std::time::Duration::from_millis(ctx.config.sources.probe_timeout_ms).max(PROBE_TIMEOUT)
}

/// The winget source candidates after user configuration.
///
/// Goes through `effective_sources_for` so `osdk source disable` and custom
/// sources apply here exactly as they do for a tool download.
pub fn effective_winget_sources(ctx: &Ctx) -> Vec<Source> {
    select::effective_sources_for(ctx, WINGET_SOURCE_TOOL, winget_sources())
}

/// Measure every winget source candidate and return the fresh results.
///
/// This writes nothing to winget. It fetches bytes over HTTP to see which
/// endpoint is quickest, and reports what it found.
pub async fn probe_winget_sources(ctx: &Ctx) -> Result<Vec<MirrorMeasurement>> {
    let sources = effective_winget_sources(ctx);
    let results = select::refresh_with_timeout(
        ctx,
        WINGET_SOURCE_TOOL,
        sources.clone(),
        probe_timeout(ctx),
        |_ctx, source| winget_probe_url(source),
    )
    .await?;

    let mut measurements: Vec<MirrorMeasurement> = results
        .into_iter()
        .map(|result| {
            let source = sources.iter().find(|s| s.id == result.source_id);
            MirrorMeasurement {
                endpoint: source.map(|s| s.download_url.clone()).unwrap_or_default(),
                kind: source.map(|s| s.kind).unwrap_or(SourceKind::Custom),
                acceleration: acceleration_of(ManagerKind::Winget, &result.source_id)
                    // An unrecognised id came from user configuration. It is
                    // still a winget source, and a winget source cannot
                    // accelerate artifacts whoever operates it.
                    .unwrap_or(Acceleration::IndexOnly),
                reachable: Some(result.ok),
                ttfb_ms: result.ok.then_some(result.ttfb_ms),
                throughput: result.ok.then_some(result.throughput),
                source_id: result.source_id,
            }
        })
        .collect();

    // Fastest first; unreachable sources sink to the bottom.
    measurements.sort_by(|a, b| {
        b.reachable
            .unwrap_or(false)
            .cmp(&a.reachable.unwrap_or(false))
            .then(
                b.throughput
                    .unwrap_or(0.0)
                    .total_cmp(&a.throughput.unwrap_or(0.0)),
            )
            .then(a.source_id.cmp(&b.source_id))
    });
    Ok(measurements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_official_source_is_among_the_candidates() {
        let sources = winget_sources();

        assert!(
            sources.iter().any(|source| source.id == "official"),
            "ranking must be able to conclude that no mirror is worth switching to"
        );
    }

    #[test]
    fn documented_mirrors_outrank_undocumented_ones_before_measurement() {
        let sources = winget_sources();

        let ustc = sources.iter().find(|s| s.id == "ustc").unwrap();
        let nju = sources.iter().find(|s| s.id == "nju").unwrap();
        assert!(
            ustc.priority < nju.priority,
            "an endpoint its operator documents is the safer default"
        );
    }

    #[test]
    fn every_winget_mirror_is_honest_about_accelerating_only_discovery() {
        for candidate in WINGET_MIRRORS {
            assert_eq!(
                candidate.acceleration,
                Acceleration::IndexOnly,
                "{} claims artifact acceleration winget sources cannot provide",
                candidate.id
            );
            assert!(!candidate.acceleration.accelerates_downloads());
        }
    }

    #[test]
    fn the_debunked_mirrors_are_absent() {
        // TUNA serves no winget source, and the widely copied Tencent Cloud URL
        // 404s. Both were verified rather than assumed.
        for absent in ["tuna", "tencent"] {
            assert!(
                !WINGET_MIRRORS.iter().any(|m| m.id == absent),
                "{absent} does not mirror the winget source"
            );
        }
    }

    #[test]
    fn probe_urls_point_at_the_payload_winget_downloads() {
        let sources = winget_sources();
        let ustc = sources.iter().find(|s| s.id == "ustc").unwrap();

        assert_eq!(
            winget_probe_url(ustc).as_deref(),
            Some("https://mirrors.ustc.edu.cn/winget-source/source.msix")
        );
    }

    #[test]
    fn a_trailing_slash_does_not_produce_a_doubled_path_separator() {
        let source = Source::mirror("x", "https://example.invalid/winget-source/", 1);

        assert_eq!(
            winget_probe_url(&source).as_deref(),
            Some("https://example.invalid/winget-source/source.msix")
        );
    }

    #[test]
    fn the_pseudo_tool_id_cannot_collide_with_a_real_tool() {
        assert!(
            WINGET_SOURCE_TOOL.starts_with("pkg:"),
            "the namespace prefix is what keeps `osdk source` config from colliding"
        );
    }

    #[test]
    fn homebrew_acceleration_is_unknown_rather_than_guessed() {
        assert_eq!(
            acceleration_of(ManagerKind::Homebrew, "tuna"),
            None,
            "Homebrew is not implemented yet; reporting a value would be inventing one"
        );
    }

    #[test]
    fn known_winget_ids_resolve_to_index_only() {
        assert_eq!(
            acceleration_of(ManagerKind::Winget, "ustc"),
            Some(Acceleration::IndexOnly)
        );
        assert_eq!(acceleration_of(ManagerKind::Winget, "nonexistent"), None);
    }
}
