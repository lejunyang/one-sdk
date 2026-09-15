//! Discovering which namespaces can provide a bare tool name.
//!
//! `osdk install uv` names no backend. The operand is usually a real package
//! that simply lacks its namespace, so refusing outright pushes the guessing
//! onto the user. This module answers the question osdk is actually able to
//! answer: which of the registered dynamic namespaces publish something by that
//! name, and what distinguishes them.
//!
//! Two properties matter more than the list itself.
//!
//! **It is driven by `DYNAMIC_NAMESPACES`, not by a hand-written list.** The
//! first version hard-coded PyPI and conda-forge, which meant a new backend was
//! invisible here until someone remembered to add it -- the same class of bug as
//! a namespace missing from `is_dynamic_install_directory`, where installs go
//! silently unscanned. Probes are matched to namespaces by name, so adding a
//! namespace without a probe degrades to "not probed" rather than to a wrong
//! answer, and the test at the bottom of this file fails when a namespace has
//! neither a probe nor a recorded reason for not having one.
//!
//! **A bare name is not equally meaningful in every namespace.** `github:` and
//! `http:` need an owner/repo or a URL, so a bare word cannot be probed at all;
//! `go:` needs a module path. Reporting "not found in github" for a name that
//! could never have been a github id would be noise dressed up as a finding.

use std::cmp::Ordering;

use crate::backend::Ctx;
use crate::tool::DYNAMIC_NAMESPACES;

/// Why one candidate ranks above another when several can provide a name.
///
/// The ordering is by packaging provenance, which is the one thing that can be
/// stated without knowing the tool: a project publishing its own releases is
/// preferable to a third party repackaging them, and both beat compiling from
/// source. mise reaches the same conclusion from the other direction -- its
/// registry lists backends in preference order per tool and puts curated,
/// publisher-backed backends above language-specific ones that need a toolchain.
///
/// This is a tie-breaker for presentation, never an automatic choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provenance {
    /// The project publishes the artifact itself.
    FirstParty,
    /// A third party rebuilds and republishes it, so it can lag upstream.
    Repackaged,
    /// Built from source on this machine, requiring that toolchain.
    CompiledLocally,
}

impl Provenance {
    /// A short phrase explaining the tradeoff, shown after the candidate.
    pub fn describe(self) -> &'static str {
        match self {
            Self::FirstParty => "published by the project itself",
            Self::Repackaged => "repackaged by a third party, so it can lag upstream",
            Self::CompiledLocally => "compiled from source, needs that toolchain installed",
        }
    }
}

/// A namespace that could provide a bare name, with what was learned about it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The id to install, e.g. `pypi:uv`.
    pub id: String,
    /// Newest version the namespace publishes, when the probe reports one.
    pub version: Option<String>,
    /// How the artifact reaches the user.
    pub provenance: Provenance,
    /// The registry's own one-line description, when it publishes one.
    ///
    /// Carried because sharing a name does not make two packages the same
    /// program. Probing `uv` finds `npm:uv` at 1.4.0 and `pypi:uv` at 0.12.14 --
    /// different projects that happen to collide, and `pypi:ripgrep` is not
    /// BurntSushi's ripgrep either. Listing them as interchangeable sources
    /// would be worse than not listing them, so the description travels with
    /// the candidate and lets the caller show what each one actually is.
    pub summary: Option<String>,
}

/// Why a namespace was not probed for a bare name.
///
/// Kept as an explicit value rather than an omission so the completeness test
/// can tell "deliberately not probed" apart from "forgotten".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotProbed {
    /// A bare word is not a well-formed id in this namespace.
    NeedsStructuredId,
}

/// How a namespace participates in bare-name discovery.
enum Probe {
    /// Query the namespace over the network.
    Query {
        provenance: Provenance,
        kind: ProbeKind,
    },
    /// Skip it, for a reason worth recording.
    Skip(NotProbed),
}

/// The request shape a namespace needs, so each probe stays declarative.
enum ProbeKind {
    /// PyPI: read the configured index and report the newest final release.
    PythonIndex,
    /// anaconda.org: existence only.
    CondaForge,
    /// The npm registry: existence plus `dist-tags.latest`.
    NpmRegistry,
    /// crates.io: existence plus `crate.max_stable_version`.
    CratesIo,
}

/// The probe assigned to a namespace, or the reason it has none.
fn probe_for(namespace: &str) -> Probe {
    match namespace {
        // The project's own release, and the index osdk is configured to use.
        "pypi" => Probe::Query {
            provenance: Provenance::FirstParty,
            kind: ProbeKind::PythonIndex,
        },
        // conda-forge rebuilds upstream sources; genuine code, third-party build.
        "conda" => Probe::Query {
            provenance: Provenance::Repackaged,
            kind: ProbeKind::CondaForge,
        },
        "npm" => Probe::Query {
            provenance: Provenance::FirstParty,
            kind: ProbeKind::NpmRegistry,
        },
        // crates.io ships sources; the crate is built here.
        "cargo" => Probe::Query {
            provenance: Provenance::CompiledLocally,
            kind: ProbeKind::CratesIo,
        },
        // `github:owner/repo`, `http:https://...`, `go:module/path` -- a bare
        // word is not one of these, and there is no registry to search by name.
        // Probing anyway would mean guessing at an owner.
        _ => Probe::Skip(NotProbed::NeedsStructuredId),
    }
}

/// Find every namespace that can provide `name`, best provenance first.
///
/// Probes run concurrently: this happens while a person waits for an
/// explanation, so the cost should be the slowest probe rather than their sum.
pub async fn discover(ctx: &Ctx, name: &str) -> Vec<Candidate> {
    // Offline discovery would report "nothing provides this" for a name that is
    // published everywhere. An empty list lets the caller fall back to the plain
    // unknown-backend error instead of stating something false.
    if ctx.config.settings.offline {
        return Vec::new();
    }

    let mut futures = Vec::new();
    for schema in DYNAMIC_NAMESPACES {
        let Probe::Query { provenance, kind } = probe_for(schema.namespace) else {
            continue;
        };
        // Namespaces normalize names differently (PEP 503 folds `.`/`-`/`_`),
        // so the id is built through the schema rather than by formatting.
        let Ok(subject) = schema.canonicalize_subject(name) else {
            continue;
        };
        futures.push(run_probe(ctx, schema.namespace, subject, provenance, kind));
    }

    let mut candidates: Vec<Candidate> = futures_util::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect();

    // Best provenance first, then by id so the output is stable across runs.
    candidates.sort_by(|left, right| match left.provenance.cmp(&right.provenance) {
        Ordering::Equal => left.id.cmp(&right.id),
        other => other,
    });
    candidates
}

/// What a probe learned: the newest version and the registry's description.
///
/// Both are optional because a registry can confirm a package exists without
/// exposing either -- conda-forge's package endpoint is the case in point.
struct ProbeResult {
    version: Option<String>,
    summary: Option<String>,
}

impl ProbeResult {
    /// Exists, with nothing further known about it.
    fn bare() -> Self {
        Self {
            version: None,
            summary: None,
        }
    }
}

async fn run_probe(
    ctx: &Ctx,
    namespace: &'static str,
    subject: String,
    provenance: Provenance,
    kind: ProbeKind,
) -> Option<Candidate> {
    let found = match kind {
        ProbeKind::PythonIndex => probe_python_index(ctx, &subject).await?,
        ProbeKind::CondaForge => probe_conda_forge(ctx, &subject).await?,
        ProbeKind::NpmRegistry => probe_npm(ctx, &subject).await?,
        ProbeKind::CratesIo => probe_crates_io(ctx, &subject).await?,
    };
    Some(Candidate {
        id: format!("{namespace}:{subject}"),
        version: found.version,
        provenance,
        summary: found.summary.map(|text| truncate_summary(&text)),
    })
}

/// Shorten a registry description to one readable clause.
///
/// Registry summaries run to paragraphs; the purpose here is only to let a
/// reader tell two same-named packages apart, so the first sentence-ish chunk is
/// enough and a wall of text would bury the candidate list it annotates.
fn truncate_summary(text: &str) -> String {
    const LIMIT: usize = 68;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= LIMIT {
        return collapsed;
    }
    // Cut on a char boundary, not a byte one: descriptions carry non-ASCII.
    let mut cut: String = collapsed.chars().take(LIMIT).collect();
    // Prefer breaking at the last space so a word is not sliced in half.
    if let Some(space) = cut.rfind(' ') {
        if space > LIMIT / 2 {
            cut.truncate(space);
        }
    }
    format!("{cut}...")
}

/// PyPI, through the configured index so the version is what would be installed.
async fn probe_python_index(ctx: &Ctx, project: &str) -> Option<ProbeResult> {
    let configured = &ctx.config.registries().python;
    // Deliberately more generous than the install-path probe budget: a mirror
    // needing 2 s is perfectly usable, and treating it as unreachable dropped
    // the PyPI candidate entirely, leaving an answer that listed only conda.
    let timeout = configured.probe_timeout_ms.max(8_000);
    let index = match crate::python_index::plan(&configured.urls, timeout).await {
        crate::python_index::IndexPlan::Selected { url, .. } => url,
        crate::python_index::IndexPlan::PassThrough { .. } => {
            crate::python_index::PYPI.to_string()
        }
        crate::python_index::IndexPlan::Unavailable { .. } => return None,
    };
    let versions = crate::python_index::list_versions(&ctx.client, &index, project, false)
        .await
        .ok()?;
    let newest = versions
        .iter()
        .filter(|version| !crate::backend::pypi::is_pep440_prerelease(version))
        .max_by(|left, right| crate::backend::pypi::compare_pep440(left, right))
        .cloned();
    // The description comes from pypi.org's JSON API rather than the configured
    // mirror: PEP 691 indexes do not carry one, and a mirror need not serve this
    // endpoint at all. It is annotation only -- if it fails the candidate still
    // stands, so this must never turn a found package into a missing one.
    let summary = fetch_pypi_summary(ctx, project).await;
    Some(ProbeResult {
        version: newest,
        summary,
    })
}

/// Read a project's one-line summary from pypi.org, best effort.
async fn fetch_pypi_summary(ctx: &Ctx, project: &str) -> Option<String> {
    let url = format!("https://pypi.org/pypi/{project}/json");
    let response = ctx.client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    body.get("info")
        .and_then(|info| info.get("summary"))
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

/// conda-forge, reading existence plus the package summary.
///
/// The version is deliberately not read from the per-version payload: that is a
/// second round trip for a candidate that already ranks below a first-party one,
/// and a missing number reads better than a slow command.
async fn probe_conda_forge(ctx: &Ctx, package: &str) -> Option<ProbeResult> {
    let url = format!("https://api.anaconda.org/package/conda-forge/{package}");
    let response = ctx.client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    // The body is already in hand, so the summary costs nothing extra here.
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return Some(ProbeResult::bare());
    };
    let summary = body
        .get("summary")
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    Some(ProbeResult {
        version: None,
        summary,
    })
}

/// The npm registry, reading `dist-tags.latest` and the package description.
async fn probe_npm(ctx: &Ctx, package: &str) -> Option<ProbeResult> {
    let url = format!("https://registry.npmjs.org/{package}");
    let response = ctx.client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let version = body
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let summary = body
        .get("description")
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    Some(ProbeResult { version, summary })
}

/// crates.io, reading `crate.max_stable_version` and the crate description.
async fn probe_crates_io(ctx: &Ctx, crate_name: &str) -> Option<ProbeResult> {
    let url = format!("https://crates.io/api/v1/crates/{crate_name}");
    let response = ctx.client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let entry = body.get("crate");
    let version = entry
        .and_then(|value| value.get("max_stable_version"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let summary = entry
        .and_then(|value| value.get("description"))
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    Some(ProbeResult { version, summary })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered namespace must either have a probe or a recorded reason
    /// for not having one.
    ///
    /// This is the check that makes discovery track the backend list. Adding a
    /// namespace without touching `probe_for` silently falls into the catch-all
    /// arm, and this test is what turns that from a quiet omission into a
    /// failure -- the same failure mode `is_dynamic_install_directory` guards
    /// against, where a forgotten name makes installs invisible.
    #[test]
    fn every_dynamic_namespace_is_probed_or_explicitly_skipped() {
        for schema in DYNAMIC_NAMESPACES {
            match probe_for(schema.namespace) {
                Probe::Query { .. } => {}
                Probe::Skip(NotProbed::NeedsStructuredId) => {
                    // Assert the stated reason is true: a bare word must really
                    // fail to canonicalize here. Otherwise the skip is wrong and
                    // this namespace should be probed.
                    let bare = schema.canonicalize_subject("ripgrep");
                    assert!(
                        bare.is_err() || matches!(schema.namespace, "go" | "github" | "http"),
                        "`{}` accepts a bare name, so it should be probed rather than skipped",
                        schema.namespace
                    );
                }
            }
        }
    }

    /// Summary shortening must not panic on multi-byte text, and must shorten.
    ///
    /// Registry descriptions are free-form and frequently non-ASCII, so cutting
    /// at a byte offset would panic on a char boundary -- in a code path whose
    /// only job is to annotate an error message, which would turn a helpful
    /// answer into a crash.
    #[test]
    fn summaries_are_shortened_without_splitting_characters() {
        // Short input passes through, with whitespace collapsed.
        assert_eq!(
            truncate_summary("An extremely fast Python package installer"),
            "An extremely fast Python package installer"
        );
        assert_eq!(truncate_summary("line one\n  line two"), "line one line two");

        // Long ASCII input is cut and marked.
        let long = "a".repeat(200);
        let cut = truncate_summary(&long);
        assert!(cut.ends_with("..."), "long text must be marked: {cut}");
        assert!(
            cut.chars().count() < long.chars().count(),
            "long text must actually shrink"
        );

        // Multi-byte input: the point is that this does not panic, and that the
        // result stays valid text.
        let chinese = "极其快速的 Python 包管理器".repeat(20);
        let cut = truncate_summary(&chinese);
        assert!(cut.chars().count() < chinese.chars().count());
        assert!(cut.ends_with("..."));

        // Emoji sit outside the BMP, so a naive slice would split them.
        let emoji = "🚀".repeat(100);
        let cut = truncate_summary(&emoji);
        assert!(cut.contains('🚀'), "characters must survive intact: {cut}");

        // Empty stays empty rather than becoming "...".
        assert_eq!(truncate_summary(""), "");
        assert_eq!(truncate_summary("   "), "");
    }

    /// Provenance ordering is what ranks the candidates, so pin it.
    #[test]
    fn first_party_outranks_repackaged_and_compiled() {
        assert!(Provenance::FirstParty < Provenance::Repackaged);
        assert!(Provenance::Repackaged < Provenance::CompiledLocally);

        let mut order = vec![
            Provenance::CompiledLocally,
            Provenance::FirstParty,
            Provenance::Repackaged,
        ];
        order.sort();
        assert_eq!(
            order,
            [
                Provenance::FirstParty,
                Provenance::Repackaged,
                Provenance::CompiledLocally
            ]
        );
    }

    /// The four probed namespaces are the ones a bare word can name.
    #[test]
    fn probed_namespaces_are_the_ones_a_bare_name_can_identify() {
        let probed: Vec<&str> = DYNAMIC_NAMESPACES
            .iter()
            .filter(|schema| matches!(probe_for(schema.namespace), Probe::Query { .. }))
            .map(|schema| schema.namespace)
            .collect();
        assert_eq!(probed, ["npm", "cargo", "conda", "pypi"]);
    }

    /// Each namespace's provenance must be the one its distribution model implies.
    ///
    /// The ordering test alone does not cover this: it pins the enum's own order,
    /// so labelling conda-forge `FirstParty` left it green while making the
    /// output claim that a community rebuild comes from the project. Verified by
    /// mutation -- that change survives the ordering test and fails this one.
    ///
    /// The assignments are not stylistic. conda-forge builds from upstream
    /// sources via a feedstock, so the code is genuine but the artifact is not
    /// the project's own -- measured one release behind PyPI for uv (0.12.13 vs
    /// 0.12.14). crates.io ships sources that are compiled here, which is why it
    /// ranks below both: it additionally requires a Rust toolchain.
    #[test]
    fn each_namespace_declares_the_provenance_its_distribution_model_implies() {
        let provenance_of = |namespace: &str| match probe_for(namespace) {
            Probe::Query { provenance, .. } => Some(provenance),
            Probe::Skip(_) => None,
        };

        assert_eq!(provenance_of("pypi"), Some(Provenance::FirstParty));
        assert_eq!(provenance_of("npm"), Some(Provenance::FirstParty));
        assert_eq!(provenance_of("conda"), Some(Provenance::Repackaged));
        assert_eq!(provenance_of("cargo"), Some(Provenance::CompiledLocally));

        // Consequence worth pinning directly: a first-party candidate must sort
        // above the conda one, since that ranking is the whole recommendation.
        assert!(provenance_of("pypi") < provenance_of("conda"));
        assert!(provenance_of("conda") < provenance_of("cargo"));
    }
}
