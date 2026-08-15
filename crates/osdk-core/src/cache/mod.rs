//! Unified downstream package caches (dedup layer 2).
//!
//! Each language package manager keeps its own global cache/store. By pointing
//! them all at a shared osdk-managed root, different projects — and different
//! SDK versions — reuse already-downloaded dependencies instead of re-fetching
//! into per-project or per-version caches.
//!
//! These are emitted as environment variables during shell activation (and can
//! be inspected via `osdk cache dir`). We only set a variable if the user
//! hasn't already set it, so we never override an explicit choice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The shared downstream-cache root, `<cache>/pkg`.
pub fn downstream_root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("pkg")
}

/// Compute the env vars that redirect package-manager caches to the shared
/// root. `getenv` lets callers avoid overriding user-set values.
pub fn cache_env(
    cache_dir: &Path,
    getenv: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let root = downstream_root(cache_dir);
    let mut env = BTreeMap::new();

    let mut set_if_unset = |key: &str, path: PathBuf| {
        if getenv(key).is_none() {
            env.insert(key.to_string(), path.display().to_string());
        }
    };

    // npm: package cache
    set_if_unset("npm_config_cache", root.join("npm"));
    // pnpm: content-addressable store (its own dedup, rooted in our shared area)
    set_if_unset("PNPM_HOME", root.join("pnpm"));
    set_if_unset("npm_config_store_dir", root.join("pnpm-store"));
    // yarn (berry): global folder
    set_if_unset("YARN_GLOBAL_FOLDER", root.join("yarn"));
    // pip: download/wheel cache
    set_if_unset("PIP_CACHE_DIR", root.join("pip"));
    // Go: module cache
    set_if_unset("GOMODCACHE", root.join("go-mod"));
    set_if_unset("GOCACHE", root.join("go-build"));
    // Cargo: registry + git caches (shared home; note this also holds bins)
    set_if_unset("CARGO_HOME", root.join("cargo"));
    // Maven / Gradle (java ecosystem)
    set_if_unset("GRADLE_USER_HOME", root.join("gradle"));

    env
}

/// Human-readable listing of what the shared caches map to.
pub fn describe(cache_dir: &Path) -> Vec<(String, String)> {
    cache_env(cache_dir, |_| None).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn maps_known_managers() {
        let cache = PathBuf::from("/x/cache");
        let env = cache_env(&cache, |_| None);
        assert_eq!(env.get("PIP_CACHE_DIR").unwrap(), "/x/cache/pkg/pip");
        assert_eq!(env.get("GOMODCACHE").unwrap(), "/x/cache/pkg/go-mod");
        assert_eq!(env.get("npm_config_cache").unwrap(), "/x/cache/pkg/npm");
    }

    #[test]
    fn respects_user_set_vars() {
        let cache = PathBuf::from("/x/cache");
        let mut user = HashMap::new();
        user.insert("PIP_CACHE_DIR".to_string(), "/custom/pip".to_string());
        let env = cache_env(&cache, |k| user.get(k).cloned());
        // user's PIP_CACHE_DIR is left untouched (not in our delta)
        assert!(!env.contains_key("PIP_CACHE_DIR"));
        // others are still set
        assert!(env.contains_key("GOMODCACHE"));
    }
}
