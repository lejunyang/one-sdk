//! osdk-core: the reusable library behind the `osdk` universal SDK manager.
//!
//! Module map:
//! - [`platform`] — host OS/arch/libc detection + per-SDK token mapping.
//! - [`dirs`]     — data/store/installs/shims/cache directory resolution.
//! - [`config`]   — layered configuration (CLI > env > project > user).
//! - [`store`]    — content-addressed store + link-mode materialization (dedup).
//! - [`source`]   — multi-source model + fastest-mirror selection.
//! - [`version`]  — version spec parsing + resolution + active-version walk-up.
//! - [`http`]     — shared HTTP client + helpers.
//! - [`pipeline`] — download → verify → extract → CAS ingest orchestrator.
//! - [`backend`]  — the `Backend` trait, contexts, registry, and SDK impls.
//! - [`shim`]     — shim launcher generation.
//! - `skills`     — staging agent-skill packages and linking them into agents.
//! - `syspkg`     — read-only discovery of host package managers (winget/brew).
//! - [`lock`]     — cross-process file locks.
//! - `self_update` — replacing osdk's own binaries from its GitHub releases.

// Compiling without the `install` feature intentionally removes the download
// pipeline and every backend method that reaches it, which leaves their helpers,
// imports and constants unreferenced. Those specific lints are therefore
// expected noise in that configuration only -- the default build keeps them at
// full strength, and CI lints with default features so genuine dead code in the
// install path is still reported.
#![cfg_attr(
    not(feature = "install"),
    allow(unused_imports, dead_code, unused_variables, unused_mut)
)]
pub mod activate;
pub mod android;
pub mod backend;
pub mod cache;
pub mod config;
pub mod container;
// Application dependency manifests are read only by `osdk deps` (and the
// commands that opt into it). The shim dispatches an already-installed tool and
// never materializes a project's dependency closure, so gating the module keeps
// its provider tables and manifest parsing out of the shim's binary.
#[cfg(feature = "install")]
pub mod deps;
pub mod dirs;
pub mod error;
pub mod fs;
pub mod http;
pub mod i18n;
pub mod inventory;
pub mod lock;
pub mod model;
pub mod npm;
pub mod npm_tools;
pub mod package_registry;
pub mod pipeline;
pub mod platform;
pub mod process;
// Picking a Python index only ever happens while installing packages; the shim
// just launches an already-installed interpreter. Gating it keeps the probe
// client, and the reqwest/serde_json machinery it needs, out of the shim build.
#[cfg(feature = "install")]
pub mod python_index;
// Bare-name discovery exists only to explain an install request, and every probe
// it runs is an HTTP call. Gating it keeps those probes out of the shim, which
// never resolves a bare name -- it dispatches an already-installed tool.
#[cfg(feature = "install")]
pub mod backend_discovery;
// Updating osdk itself downloads and unpacks a release, which is exactly the
// machinery the shim drops. Keeping it behind `install` means the shim's build
// never links it; `osdk self upgrade` is a CLI-only command anyway.
#[cfg(feature = "install")]
pub mod self_update;
pub mod shim;
// Agent skills are staged and linked only by `osdk skills` (a CLI-only command).
// The shim dispatches an already-installed tool and never touches a skill, so
// gating the module keeps its agent table, source parsing and staging out of the
// shim's binary -- the same reasoning as `deps`/`tasks`/`syspkg`.
#[cfg(feature = "install")]
pub mod skills;
pub mod source;
pub mod store;
// Inspecting the host's package managers is a CLI diagnostic: the shim only
// launches tools and never asks what winget or brew are doing. Gating the
// subsystem here keeps it, and its process probes, out of the shim's build.
#[cfg(feature = "install")]
pub mod syspkg;
// Task definitions are read only by osdk run / osdk task. The shim never runs
// a task -- it dispatches an already-installed tool -- so gating the module keeps
// its parsing out of the shim's binary. A [tasks] table in a config the shim
// reads stays harmless: serde ignores tables the build does not know.
#[cfg(feature = "install")]
pub mod tasks;
pub mod tool;
pub mod trust;
#[cfg(feature = "install")]
pub mod verification;

/// Stand-ins for the attestation types when built without `install`.
///
/// The shim never verifies attestations, so sigstore and the real
/// implementation are compiled out. These two names stay available as types that
/// cannot be constructed, which keeps every signature threading
/// `Option<&GithubAttestation>` compiling unchanged: an `Option` of an
/// uninhabited type is always `None`, so the verification branches are
/// statically unreachable. The alternative -- `#[cfg]` on thirty-odd references
/// including public struct fields -- would be far harder to follow.
#[cfg(not(feature = "install"))]
pub mod verification {
    /// Uninhabited: with no install path there is no attestation to verify.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GithubAttestation {}

    /// Uninhabited counterpart of the real evidence record. Still serializable so
    /// `InstallPlan`'s `Vec<VerificationEvidence>` field needs no `#[cfg]`.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum VerificationEvidence {}

    impl VerificationEvidence {
        /// Unreachable: the type has no values. Mirrors the real accessor so both
        /// configurations share one spelling at the call sites.
        pub fn digest(&self) -> &str {
            match *self {}
        }
    }

    /// Never called: its `attestation` argument cannot be constructed.
    ///
    /// Present only so the call sites in `pipeline`, which sit inside
    /// `if let Some(attestation) = attestation` branches that are statically
    /// unreachable here, still resolve the name.
    pub async fn verify_github_attestation(
        _client: &reqwest::Client,
        _dirs: &crate::dirs::Dirs,
        _offline: bool,
        _archive: &std::path::Path,
        attestation: &GithubAttestation,
    ) -> crate::error::Result<Option<VerificationEvidence>> {
        match *attestation {}
    }
}

pub mod version;

pub use backend::registry::Registry;
pub use backend::{Backend, Ctx, InstallCtx};
pub use error::{Error, Result};

/// Whether this build of the crate includes the install path (remote version
/// listing, the download pipeline and sigstore verification).
///
/// Exposed so a binary that must stay lean can assert it at compile time.
/// Cargo unifies features across a single `--workspace` build, so a dependant
/// asking for `default-features = false` can still silently get the install
/// path linked in; only a build-time check catches that.
pub const INSTALL_PATH_LINKED: bool = cfg!(feature = "install");
