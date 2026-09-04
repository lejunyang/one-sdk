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
//! - [`lock`]     — cross-process file locks.

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
pub mod dirs;
pub mod error;
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
pub mod shim;
pub mod source;
pub mod store;
pub mod tool;
pub mod trust;
pub mod verification;
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
