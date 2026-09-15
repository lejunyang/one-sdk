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

use super::report::{ManagerKind, SourceRecord};
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

/// The name winget's built-in package source carries.
///
/// Load-bearing rather than cosmetic: this source always participates in a call
/// without being named, and applying a mirror *replaces it under this same
/// name* -- the only shape available, since two `Microsoft.PreIndexed.Package`
/// sources cannot coexist under one fixed MSIX identity. So a candidate
/// carrying this name never needs `--source`, whatever endpoint it points at.
pub const DEFAULT_WINGET_SOURCE_NAME: &str = "winget";

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

/// Why osdk is not passing `--source` on a winget call it issues itself.
///
/// Each variant is a distinct, reportable situation rather than a generic
/// failure, because the remedy differs: an unregistered mirror is fixed by
/// registering it, while a user pin is not a problem at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum NoPreferredSource {
    /// The user pinned a source, so osdk defers to that choice.
    UserPinned(String),
    /// No mirror osdk knows about is registered on this host.
    ///
    /// On winget this is the normal state, and not a gap to be closed by
    /// registering a mirror under its own name: a
    /// `Microsoft.PreIndexed.Package` source installs under one fixed MSIX
    /// identity (`Microsoft.Winget.Source_8wekyb3d8bbwe`), so a second such
    /// source cannot coexist -- verified on an elevated host, where adding one
    /// fails with `0x80073D06`. A winget mirror is applied by replacing the
    /// source named `winget`, after which the mirror *is* the default and no
    /// `--source` is wanted at all.
    NoMirrorRegistered,
    /// The fastest registered candidate is already the source winget uses by
    /// default, so no `--source` is wanted.
    ///
    /// Covers both a stock host, where that source is Microsoft's CDN, and a
    /// mirrored host, where a mirror has replaced it under the same name.
    AlreadyTheDefaultSource,
    /// Mirrors are registered, but nothing has been measured yet and osdk is
    /// offline, so there is no basis for preferring one.
    NoMeasurement,
}

/// Which registered winget source osdk should pass to its own calls.
///
/// Returns the source's **registered `Name`**, because that is the only token
/// `winget --source` accepts. osdk's own mirror ids (`ustc`, `nju`, ...) are a
/// separate namespace and are matched to the host's registrations by endpoint,
/// never assumed to coincide: passing a name winget does not know fails the
/// whole call with `0x8A150012` (verified on a real host), turning an
/// installable package into an error. Preferring nothing is always safe;
/// preferring a guess is not.
///
/// `registered` comes from `winget source export`, whose `Name` field is an
/// identifier rather than a label and so does not vary with display language.
///
/// # Why this yields no acceleration on winget
///
/// Measured on an elevated host: a winget mirror cannot be registered
/// alongside the official source, because both install under the same fixed
/// MSIX identity. A mirror is applied by replacing the source named `winget`,
/// so afterwards it already is what every winget call uses by default, and
/// naming it would only restrict the call and hide `msstore`. In practice this
/// function therefore returns `Err` on winget. It is kept because it still
/// honours a user pin, refuses to pass osdk's internal ids as source names, and
/// is the shape Homebrew needs, where a mirror is chosen per invocation through
/// environment variables rather than a shared registration.
pub fn preferred_winget_source(
    registered: &[SourceRecord],
    measured: &[MirrorMeasurement],
    user_pin: Option<&str>,
) -> std::result::Result<String, NoPreferredSource> {
    // An explicit choice always wins, exactly as the Go backend leaves a
    // user-set GOPROXY alone. Honour it even if it names something osdk has
    // never measured: the user knows their host better than osdk's defaults do.
    if let Some(pin) = user_pin {
        return if registered.iter().any(|source| source.name == pin) {
            Ok(pin.to_owned())
        } else {
            // A pin naming an unregistered source would fail the call. Report
            // it rather than silently substituting a different source.
            Err(NoPreferredSource::UserPinned(pin.to_owned()))
        };
    }

    let reachable: Vec<&MirrorMeasurement> = measured
        .iter()
        .filter(|m| m.reachable == Some(true) && m.throughput.is_some())
        .collect();
    if reachable.is_empty() {
        return Err(NoPreferredSource::NoMeasurement);
    }

    // Fastest first, and take the first one this host actually has registered.
    let mut ranked = reachable;
    ranked.sort_by(|a, b| {
        b.throughput
            .unwrap_or(0.0)
            .total_cmp(&a.throughput.unwrap_or(0.0))
    });

    // The fastest candidate this host has registered, mirror or not.
    let best = ranked.iter().find_map(|measurement| {
        registered
            .iter()
            .find(|source| endpoints_match(source.endpoint.as_deref(), &measurement.endpoint))
            .map(|source| (source, *measurement))
    });

    let Some((source, _measurement)) = best else {
        return Err(NoPreferredSource::NoMirrorRegistered);
    };

    // The real question is not "is this the official endpoint" but "is this
    // source already what winget uses by default", and for winget that is
    // decided by the *name*, not the endpoint. `winget` is the built-in source
    // name and always participates in a call, whether it still points at
    // Microsoft's CDN or has been replaced by a mirror -- and replacing it is
    // the only way a winget mirror can be applied at all, since two
    // `Microsoft.PreIndexed.Package` sources cannot coexist under the same
    // fixed MSIX identity (verified: adding a second fails with 0x80073D06).
    //
    // Either way, naming it buys nothing and costs something: `--source`
    // restricts the call to that one source, which silently excludes `msstore`,
    // so a Store-only package would report as not found. Omitting the argument
    // keeps winget's normal multi-source behaviour.
    if source.name == DEFAULT_WINGET_SOURCE_NAME {
        return Err(NoPreferredSource::AlreadyTheDefaultSource);
    }

    Ok(source.name.clone())
}

/// Whether a registered source and a measured candidate are the same endpoint.
///
/// Compared after trimming a trailing slash, since `winget source add` preserves
/// whatever form the user typed and a mismatch here would silently demote a
/// mirror that is in fact registered.
fn endpoints_match(registered: Option<&str>, measured: &str) -> bool {
    registered.is_some_and(|registered| {
        registered.trim_end_matches('/') == measured.trim_end_matches('/')
    })
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
    use crate::syspkg::report::SourceTrust;

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

    fn registered(name: &str, endpoint: &str) -> SourceRecord {
        SourceRecord {
            identifier: format!("{name}.Identifier"),
            name: name.to_owned(),
            endpoint: Some(endpoint.to_owned()),
            kind: Some("Microsoft.PreIndexed.Package".to_owned()),
            trust: SourceTrust::Trusted,
        }
    }

    fn measured(id: &str, endpoint: &str, throughput: Option<f64>) -> MirrorMeasurement {
        MirrorMeasurement {
            source_id: id.to_owned(),
            endpoint: endpoint.to_owned(),
            kind: SourceKind::Mirror,
            acceleration: Acceleration::IndexOnly,
            reachable: Some(throughput.is_some()),
            ttfb_ms: throughput.map(|_| 100),
            throughput,
        }
    }

    fn measured_official(throughput: Option<f64>) -> MirrorMeasurement {
        MirrorMeasurement {
            kind: SourceKind::Official,
            ..measured("official", WINGET_OFFICIAL_ENDPOINT, throughput)
        }
    }

    #[test]
    fn the_default_source_is_never_named_explicitly() {
        let hosts = [registered("winget", WINGET_OFFICIAL_ENDPOINT)];
        let speeds = [measured_official(Some(700_000.0))];

        // `--source winget` restricts the call to that one source, hiding
        // msstore and buying nothing: it is already winget's default.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::AlreadyTheDefaultSource)
        );
    }

    #[test]
    fn a_registered_mirror_beating_the_official_source_is_named() {
        let hosts = [
            registered("winget", WINGET_OFFICIAL_ENDPOINT),
            registered("ustc-winget", "https://mirrors.ustc.edu.cn/winget-source"),
        ];
        let speeds = [
            measured(
                "ustc",
                "https://mirrors.ustc.edu.cn/winget-source",
                Some(5_000_000.0),
            ),
            measured_official(Some(700_000.0)),
        ];

        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Ok("ustc-winget".to_owned())
        );
    }

    #[test]
    fn a_mirror_slower_than_the_default_source_is_not_named() {
        let hosts = [
            registered("winget", WINGET_OFFICIAL_ENDPOINT),
            registered("slow-mirror", "https://slow.invalid/winget-source"),
        ];
        let speeds = [
            measured_official(Some(9_000_000.0)),
            measured(
                "slow",
                "https://slow.invalid/winget-source",
                Some(300_000.0),
            ),
        ];

        // Redirecting to a slower mirror would be a pessimisation.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::AlreadyTheDefaultSource)
        );
    }

    #[test]
    fn the_fastest_registered_mirror_is_preferred_by_its_registered_name() {
        let hosts = [registered(
            "ustc-winget",
            "https://mirrors.ustc.edu.cn/winget-source",
        )];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        // The registered Name, not osdk's internal mirror id: only the former is
        // a token `winget --source` accepts.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Ok("ustc-winget".to_owned())
        );
    }

    #[test]
    fn a_faster_but_unregistered_mirror_never_displaces_a_registered_one() {
        let hosts = [registered(
            "ustc-winget",
            "https://mirrors.ustc.edu.cn/winget-source",
        )];
        let speeds = [
            // Fastest, but this host has not registered it.
            measured(
                "huaweicloud",
                "https://mirrors.huaweicloud.com/winget-source",
                Some(11_000_000.0),
            ),
            measured(
                "ustc",
                "https://mirrors.ustc.edu.cn/winget-source",
                Some(5_000_000.0),
            ),
        ];

        // Naming the faster one would fail the call with 0x8A150012.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Ok("ustc-winget".to_owned())
        );
    }

    #[test]
    fn nothing_is_preferred_when_no_measured_mirror_is_registered() {
        let hosts = [registered("winget", WINGET_OFFICIAL_ENDPOINT)];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        // Omitting --source is the safe degradation; guessing is not.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::NoMirrorRegistered)
        );
    }

    #[test]
    fn an_unreachable_mirror_is_not_preferred_even_when_registered() {
        let hosts = [registered(
            "ustc-winget",
            "https://mirrors.ustc.edu.cn/winget-source",
        )];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            None,
        )];

        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::NoMeasurement)
        );
    }

    #[test]
    fn a_user_pin_wins_over_the_fastest_measurement() {
        let hosts = [
            registered("winget", WINGET_OFFICIAL_ENDPOINT),
            registered("ustc-winget", "https://mirrors.ustc.edu.cn/winget-source"),
        ];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(9_000_000.0),
        )];

        assert_eq!(
            preferred_winget_source(&hosts, &speeds, Some("winget")),
            Ok("winget".to_owned())
        );
    }

    #[test]
    fn a_pin_naming_an_unregistered_source_is_reported_not_substituted() {
        let hosts = [registered("winget", WINGET_OFFICIAL_ENDPOINT)];
        let speeds = [measured_official(Some(1_000_000.0))];

        // Substituting the working source would hide a broken configuration.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, Some("typo-mirror")),
            Err(NoPreferredSource::UserPinned("typo-mirror".to_owned()))
        );
    }

    #[test]
    fn a_trailing_slash_difference_does_not_hide_a_registered_mirror() {
        let hosts = [registered(
            "ustc-winget",
            "https://mirrors.ustc.edu.cn/winget-source/",
        )];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Ok("ustc-winget".to_owned())
        );
    }

    #[test]
    fn a_different_host_on_the_same_path_is_not_treated_as_a_match() {
        let hosts = [registered("impostor", "https://evil.invalid/winget-source")];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        // Matching on the path alone would repoint osdk at an unrelated host.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::NoMirrorRegistered)
        );
    }

    #[test]
    fn a_mirror_that_replaced_the_official_source_is_still_not_named() {
        // The shape a real winget mirror takes, verified on an elevated host: it
        // cannot coexist with the official source, so applying it *replaces* the
        // source named `winget`. The name stays, the endpoint becomes the
        // mirror's.
        let hosts = [
            registered("winget", "https://mirrors.ustc.edu.cn/winget-source"),
            registered("msstore", "https://storeedgefd.dsx.mp.microsoft.com/v9.0"),
        ];
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        // The mirror is already what winget uses by default here, so naming it
        // buys nothing and would hide msstore -- the same trap as naming the
        // official source, reached from the opposite direction.
        assert_eq!(
            preferred_winget_source(&hosts, &speeds, None),
            Err(NoPreferredSource::AlreadyTheDefaultSource),
            "a mirror occupying the default source name is already in effect"
        );
    }

    #[test]
    fn a_source_without_an_endpoint_never_matches() {
        let mut without = registered("odd", "https://mirrors.ustc.edu.cn/winget-source");
        without.endpoint = None;
        let speeds = [measured(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            Some(5_000_000.0),
        )];

        assert_eq!(
            preferred_winget_source(&[without], &speeds, None),
            Err(NoPreferredSource::NoMirrorRegistered)
        );
    }
}
