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

pub mod activate;
pub mod backend;
pub mod cache;
pub mod config;
pub mod dirs;
pub mod error;
pub mod http;
pub mod i18n;
pub mod lock;
pub mod model;
pub mod npm;
pub mod pipeline;
pub mod platform;
pub mod process;
pub mod shim;
pub mod source;
pub mod store;
pub mod trust;
pub mod verification;
pub mod version;

pub use backend::registry::Registry;
pub use backend::{Backend, Ctx, InstallCtx};
pub use error::{Error, Result};
