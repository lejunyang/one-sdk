//! Manager-native downstream package caches (dedup layer 2).
//!
//! Each language package manager keeps its own global cache/store. By pointing
//! each manager at a stable directory under an osdk-managed root, different
//! projects and SDK versions can reuse that manager's downloaded dependencies.
//! The directories are deliberately separate; osdk doesn't provide a universal
//! cross-manager package CAS.
//!
//! These are emitted during shell activation, `osdk exec`, and direct shim
//! execution (and can be inspected via `osdk cache env`). By default a variable
//! is set only when the user hasn't already set it, so explicit choices win;
//! when the global or per-tool override switch is on, the managed value forces
//! over a pre-existing variable for the covered tool(s).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The shared downstream-cache root, `<cache>/pkg`.
pub fn downstream_root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("pkg")
}

/// Compute the env vars that redirect package-manager caches to the shared
/// root. `getenv` lets callers avoid overriding user-set values; `override_for`
/// maps a tool name to whether its managed value must force over a pre-existing
/// variable (the global/per-tool override switch).
pub fn cache_env(
    cache_dir: &Path,
    override_for: &impl Fn(&str) -> bool,
    getenv: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let root = downstream_root(cache_dir);
    let mut env = BTreeMap::new();

    let mut set_if_unset = |key: &str, tool: &str, path: PathBuf| {
        let force = override_for(tool);
        if variable_is_available(key, &getenv, force) {
            env.insert(key.to_string(), path.display().to_string());
        }
    };

    // npm is also exposed by Node distributions, so keep its cache available
    // even when a project selects only the Node backend.
    set_if_unset("npm_config_cache", "npm", root.join("npm"));
    // pip: download/wheel cache
    set_if_unset("PIP_CACHE_DIR", "python", root.join("pip"));
    // uv keeps its own cache and deliberately ignores pip.conf / PIP_* settings,
    // so the PIP_CACHE_DIR above does not cover it.
    set_if_unset("UV_CACHE_DIR", "python", root.join("uv"));
    // Go: module cache
    set_if_unset("GOMODCACHE", "go", root.join("go-mod"));
    set_if_unset("GOCACHE", "go", root.join("go-build"));
    // Cargo: registry + git caches (shared home; note this also holds bins)
    set_if_unset("CARGO_HOME", "rust", root.join("cargo"));
    // Maven / Gradle (java ecosystem)
    set_if_unset("GRADLE_USER_HOME", "java", root.join("gradle"));

    env
}

/// Compute version-specific environment variables for one package manager.
///
/// `mappings` contains `(environment variable, directory below <cache>/pkg)`.
/// A value already present in the process is preserved unless it was set by a
/// previous osdk shell hook, in which case `OSDK_ORIG_<key>_SET` is present and
/// the managed value may be refreshed safely.
pub fn manager_env(
    cache_dir: &Path,
    mappings: &[(&str, &str)],
    force: bool,
    getenv: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let root = downstream_root(cache_dir);
    mappings
        .iter()
        .filter(|(key, _)| variable_is_available(key, &getenv, force))
        .map(|(key, directory)| {
            (
                (*key).to_string(),
                root.join(directory).display().to_string(),
            )
        })
        .collect()
}

/// Use the current process environment when computing manager cache settings.
pub fn manager_exec_env(
    cache_dir: &Path,
    mappings: &[(&str, &str)],
    force: bool,
) -> BTreeMap<String, String> {
    manager_env(cache_dir, mappings, force, |key| std::env::var(key).ok())
}

fn variable_is_available(key: &str, getenv: &impl Fn(&str) -> Option<String>, force: bool) -> bool {
    force || getenv(&format!("OSDK_ORIG_{key}_SET")).is_some() || getenv(key).is_none()
}

/// Human-readable listing of what the shared caches map to.
pub fn describe(cache_dir: &Path) -> Vec<(String, String)> {
    let mut env = cache_env(cache_dir, &|_| false, |_| None);
    for (key, value) in manager_env(
        cache_dir,
        &[
            ("npm_config_cache", "npm"),
            ("PNPM_HOME", "pnpm"),
            ("npm_config_store_dir", "pnpm-store"),
            ("pnpm_config_store_dir", "pnpm-store"),
            ("YARN_CACHE_FOLDER", "yarn-classic"),
            ("YARN_GLOBAL_FOLDER", "yarn"),
            ("BUN_INSTALL_CACHE_DIR", "bun"),
            ("DENO_DIR", "deno"),
        ],
        false,
        |_| None,
    ) {
        env.insert(key, value);
    }
    env.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn maps_known_managers() {
        let cache = PathBuf::from("/x/cache");
        let env = cache_env(&cache, &|_| false, |_| None);
        assert_eq!(
            PathBuf::from(env.get("PIP_CACHE_DIR").unwrap()),
            cache.join("pkg/pip")
        );
        assert_eq!(
            PathBuf::from(env.get("GOMODCACHE").unwrap()),
            cache.join("pkg/go-mod")
        );
        assert_eq!(
            PathBuf::from(env.get("npm_config_cache").unwrap()),
            cache.join("pkg/npm")
        );

        // uv keeps its own cache and ignores pip.conf / PIP_*, so it needs a
        // redirect of its own next to pip's.
        assert_eq!(
            PathBuf::from(env.get("UV_CACHE_DIR").unwrap()),
            cache.join("pkg/uv")
        );
        assert_ne!(
            env.get("UV_CACHE_DIR").unwrap(),
            env.get("PIP_CACHE_DIR").unwrap(),
            "uv and pip must not share one cache directory"
        );

        let npm = manager_env(&cache, &[("npm_config_cache", "npm")], false, |_| None);
        assert_eq!(
            PathBuf::from(npm.get("npm_config_cache").unwrap()),
            cache.join("pkg/npm")
        );
    }

    #[test]
    fn respects_user_set_vars() {
        let cache = PathBuf::from("/x/cache");
        let mut user = HashMap::new();
        user.insert("PIP_CACHE_DIR".to_string(), "/custom/pip".to_string());
        let env = cache_env(&cache, &|_| false, |k| user.get(k).cloned());
        // user's PIP_CACHE_DIR is left untouched (not in our delta)
        assert!(!env.contains_key("PIP_CACHE_DIR"));
        // others are still set
        assert!(env.contains_key("GOMODCACHE"));

        let managed = cache_env(&cache, &|_| false, |key| match key {
            "PIP_CACHE_DIR" => Some("/old/osdk/pkg/pip".into()),
            "OSDK_ORIG_PIP_CACHE_DIR_SET" => Some("1".into()),
            _ => None,
        });
        assert_eq!(
            PathBuf::from(managed.get("PIP_CACHE_DIR").unwrap()),
            cache.join("pkg/pip")
        );
    }

    #[test]
    fn uv_cache_follows_the_same_ownership_rules_as_pip() {
        let cache = PathBuf::from("/x/cache");

        // A user-chosen UV_CACHE_DIR must win: it stays out of our delta.
        let env = cache_env(&cache, &|_| false, |key| {
            (key == "UV_CACHE_DIR").then(|| "/custom/uv".to_string())
        });
        assert!(!env.contains_key("UV_CACHE_DIR"));
        // Redirecting uv must not disturb the neighbouring managers.
        assert!(env.contains_key("PIP_CACHE_DIR"));

        // A value a previous osdk hook set may be refreshed to the current root.
        let managed = cache_env(&cache, &|_| false, |key| match key {
            "UV_CACHE_DIR" => Some("/old/osdk/pkg/uv".into()),
            "OSDK_ORIG_UV_CACHE_DIR_SET" => Some("1".into()),
            _ => None,
        });
        assert_eq!(
            PathBuf::from(managed.get("UV_CACHE_DIR").unwrap()),
            cache.join("pkg/uv")
        );
    }

    #[test]
    fn describe_reports_the_uv_cache() {
        let cache = PathBuf::from("/x/cache");
        let described = describe(&cache).into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(
            PathBuf::from(described.get("UV_CACHE_DIR").unwrap()),
            cache.join("pkg/uv")
        );
    }

    #[test]
    fn manager_env_preserves_user_values_but_refreshes_hook_managed_values() {
        let cache = PathBuf::from("/x/cache");
        let mappings = &[("npm_config_cache", "npm")];
        let user = manager_env(&cache, mappings, false, |key| {
            (key == "npm_config_cache").then(|| "/custom/npm".into())
        });
        assert!(user.is_empty());

        let managed = manager_env(&cache, mappings, false, |key| match key {
            "npm_config_cache" => Some("/old/osdk/pkg/npm".into()),
            "OSDK_ORIG_npm_config_cache_SET" => Some("1".into()),
            _ => None,
        });
        assert_eq!(
            PathBuf::from(managed.get("npm_config_cache").unwrap()),
            cache.join("pkg/npm")
        );
    }

    #[test]
    fn force_overrides_a_user_set_manager_value() {
        let cache = PathBuf::from("/x/cache");
        let mappings = &[("npm_config_cache", "npm")];
        let user_env = |key: &str| (key == "npm_config_cache").then(|| "/user/npm".to_string());

        // force=false: a user-set variable stays out of the delta.
        let preserved = manager_env(&cache, mappings, false, user_env);
        assert!(!preserved.contains_key("npm_config_cache"));

        // force=true: the managed value wins even though the user set it.
        let forced = manager_env(&cache, mappings, true, user_env);
        assert_eq!(
            PathBuf::from(forced.get("npm_config_cache").unwrap()),
            cache.join("pkg/npm")
        );
    }

    #[test]
    fn cache_env_force_is_scoped_per_tool() {
        let cache = PathBuf::from("/x/cache");
        // Only python is overridden: a user-set PIP_CACHE_DIR is forced to the
        // managed path, while a user-set npm_config_cache is still preserved.
        let env = cache_env(&cache, &|tool| tool == "python", |key| match key {
            "PIP_CACHE_DIR" => Some("/user/pip".into()),
            "npm_config_cache" => Some("/user/npm".into()),
            _ => None,
        });
        assert_eq!(
            PathBuf::from(env.get("PIP_CACHE_DIR").unwrap()),
            cache.join("pkg/pip")
        );
        assert!(!env.contains_key("npm_config_cache"));
    }

    #[test]
    fn describe_includes_bun_and_deno_manager_caches() {
        let cache = PathBuf::from("/x/cache");
        let described = describe(&cache).into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(
            PathBuf::from(described.get("BUN_INSTALL_CACHE_DIR").unwrap()),
            cache.join("pkg/bun")
        );
        assert_eq!(
            PathBuf::from(described.get("DENO_DIR").unwrap()),
            cache.join("pkg/deno")
        );
    }
}
