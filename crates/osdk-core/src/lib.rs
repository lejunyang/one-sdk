//! osdk-core: the reusable library behind the `osdk` universal SDK manager.
//!
//! Module map (built incrementally):
//! - [`platform`] — host OS/arch/libc detection + per-SDK token mapping.
//! - [`dirs`]     — data/store/installs/shims/cache directory resolution.
//! - [`config`]   — layered configuration (CLI > env > project > user).
//! - [`store`]    — content-addressed store + link-mode materialization (dedup).
//! - [`source`]   — multi-source model + fastest-mirror selection.
//! - [`version`]  — version spec parsing + resolution.
//! - [`lock`]     — cross-process file locks.

pub mod config;
pub mod dirs;
pub mod error;
pub mod lock;
pub mod platform;
pub mod source;
pub mod store;
pub mod version;

pub use error::{Error, Result};
