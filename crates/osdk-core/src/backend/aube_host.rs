//! osdk's narrowly-scoped host adapter for the embedded aube package engine.

use std::path::{Path, PathBuf};
use std::sync::Once;

use aube::embed::{
    self, AddToProjectOptions, DepSelection, EmbedderInstallOverrides, EmbedderRuntime, FrozenMode,
    InstallControl, NetworkMode,
};

use crate::error::{Error, Result};

static OSDK_HOST: embed::Host = embed::Host {
    name: "osdk",
    display_name: "osdk",
    vendor: None,
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("osdk/", env!("CARGO_PKG_VERSION")),
    // Keep aube's private synthetic-project format. These names never leak
    // into the user's project because osdk owns the whole project directory.
    self_names: embed::AUBE.self_names,
    compatible_names: embed::AUBE.compatible_names,
    lockfile_basename: embed::AUBE.lockfile_basename,
    workspace_yaml: embed::AUBE.workspace_yaml,
    manifest_namespace: embed::AUBE.manifest_namespace,
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "osdk-aube",
    data_namespace: "osdk-aube",
    canonical_lockfile_always_wins: true,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
};

static INIT: Once = Once::new();

pub struct EmbeddedInstallRequest<'a> {
    pub project_dir: &'a Path,
    pub packages: &'a [String],
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub scripts_enabled: bool,
    pub dangerously_allow_all_builds: bool,
    pub offline: bool,
}

pub struct EmbeddedFrozenInstallRequest<'a> {
    pub project_dir: &'a Path,
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub scripts_enabled: bool,
    pub dangerously_allow_all_builds: bool,
    pub offline: bool,
}

pub struct EmbeddedLockGraphRequest<'a> {
    pub project_dir: &'a Path,
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub offline: bool,
}

pub fn initialize() {
    INIT.call_once(|| embed::initialize(&OSDK_HOST, Vec::new()));
}

pub async fn install_packages(request: EmbeddedInstallRequest<'_>) -> Result<()> {
    initialize();

    let options = AddToProjectOptions {
        save_exact: true,
        ignore_scripts: !request.scripts_enabled,
        dangerously_allow_all_builds: request.dangerously_allow_all_builds,
        offline: request.offline,
        dep_selection: DepSelection::All,
        control: InstallControl::silent(),
        runtime: Some(EmbedderRuntime::selector(request.node_bin_dir)),
        ..Default::default()
    };
    let overrides = EmbedderInstallOverrides {
        use_global_virtual_store: Some(false),
        cache_dir: Some(request.cache_dir),
        store_dir: Some(request.store_dir),
    };

    embed::add_with_overrides(request.project_dir, request.packages, options, overrides)
        .await
        .map_err(|error| Error::other(format_aube_error(&error)))
}

pub async fn install_frozen(request: EmbeddedFrozenInstallRequest<'_>) -> Result<()> {
    initialize();

    let (options, overrides) = frozen_install_options(request);
    embed::install_with_overrides(options, overrides)
        .await
        .map_err(|error| Error::other(format_aube_error(&error)))
}

pub async fn prepare_lock_graph(request: EmbeddedLockGraphRequest<'_>) -> Result<()> {
    initialize();

    let (options, overrides) = lock_graph_options(request);
    embed::install_with_overrides(options, overrides)
        .await
        .map_err(|error| Error::other(format_aube_error(&error)))
}

fn frozen_install_options(
    request: EmbeddedFrozenInstallRequest<'_>,
) -> (embed::InstallOptions, EmbedderInstallOverrides) {
    let mut options = embed::InstallOptions::new(request.project_dir);
    options.frozen_mode = FrozenMode::Frozen;
    options.dep_selection = DepSelection::All;
    options.ignore_scripts = !request.scripts_enabled;
    options.run_root_lifecycle = request.scripts_enabled;
    options.network_mode = if request.offline {
        NetworkMode::Offline
    } else {
        NetworkMode::Online
    };
    options.strict_no_lockfile = true;
    options.dangerously_allow_all_builds = request.dangerously_allow_all_builds;
    options.control = InstallControl::silent();
    options.runtime = Some(EmbedderRuntime::selector(request.node_bin_dir));

    let overrides = EmbedderInstallOverrides {
        use_global_virtual_store: Some(false),
        cache_dir: Some(request.cache_dir),
        store_dir: Some(request.store_dir),
    };
    (options, overrides)
}

fn lock_graph_options(
    request: EmbeddedLockGraphRequest<'_>,
) -> (embed::InstallOptions, EmbedderInstallOverrides) {
    let mut options = embed::InstallOptions::new(request.project_dir);
    options.frozen_mode = FrozenMode::Prefer;
    options.dep_selection = DepSelection::All;
    options.ignore_scripts = true;
    options.run_root_lifecycle = false;
    options.lockfile_only = true;
    options.network_mode = if request.offline {
        NetworkMode::Offline
    } else {
        NetworkMode::Online
    };
    options.control = InstallControl::silent();
    options.runtime = Some(EmbedderRuntime::selector(request.node_bin_dir));

    let overrides = EmbedderInstallOverrides {
        use_global_virtual_store: Some(false),
        cache_dir: Some(request.cache_dir),
        store_dir: Some(request.store_dir),
    };
    (options, overrides)
}

fn format_aube_error(error: &impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_host_uses_osdk_identity_and_disables_owned_behaviors() {
        assert_eq!(OSDK_HOST.name, "osdk");
        assert_eq!(OSDK_HOST.display_name, "osdk");
        assert_eq!(OSDK_HOST.vendor, None);
        assert!(!OSDK_HOST.runtime_switching);
        assert!(!OSDK_HOST.self_engines_check);
        assert!(!OSDK_HOST.self_update_enabled);
        assert!(OSDK_HOST.canonical_lockfile_always_wins);
    }

    #[test]
    fn frozen_install_uses_strict_graph_and_host_owned_runtime_and_storage() {
        let project_dir = PathBuf::from("/tmp/osdk-aube-project");
        let cache_dir = PathBuf::from("/tmp/osdk-aube-cache");
        let store_dir = PathBuf::from("/tmp/osdk-aube-store");
        let node_bin_dir = PathBuf::from("/tmp/osdk-node-bin");
        let (options, overrides) = frozen_install_options(EmbeddedFrozenInstallRequest {
            project_dir: &project_dir,
            cache_dir: cache_dir.clone(),
            store_dir: store_dir.clone(),
            node_bin_dir,
            scripts_enabled: false,
            dangerously_allow_all_builds: false,
            offline: true,
        });

        assert_eq!(options.project_dir, project_dir);
        assert_eq!(options.frozen_mode, FrozenMode::Frozen);
        assert_eq!(options.dep_selection, DepSelection::All);
        assert!(options.ignore_scripts);
        assert!(!options.run_root_lifecycle);
        assert_eq!(options.network_mode, NetworkMode::Offline);
        assert!(options.strict_no_lockfile);
        assert!(!options.dangerously_allow_all_builds);
        assert!(options.runtime.is_some());
        assert_eq!(overrides.use_global_virtual_store, Some(false));
        assert_eq!(overrides.cache_dir, Some(cache_dir));
        assert_eq!(overrides.store_dir, Some(store_dir));
    }

    #[test]
    fn lock_graph_generation_is_lockfile_only_and_uses_managed_runtime() {
        let project_dir = PathBuf::from("/tmp/osdk-aube-project");
        let cache_dir = PathBuf::from("/tmp/osdk-aube-cache");
        let store_dir = PathBuf::from("/tmp/osdk-aube-store");
        let (options, overrides) = lock_graph_options(EmbeddedLockGraphRequest {
            project_dir: &project_dir,
            cache_dir: cache_dir.clone(),
            store_dir: store_dir.clone(),
            node_bin_dir: PathBuf::from("/tmp/osdk-node-bin"),
            offline: false,
        });

        assert_eq!(options.project_dir, project_dir);
        assert_eq!(options.frozen_mode, FrozenMode::Prefer);
        assert!(options.ignore_scripts);
        assert!(!options.run_root_lifecycle);
        assert!(options.lockfile_only);
        assert_eq!(options.network_mode, NetworkMode::Online);
        assert!(!options.strict_no_lockfile);
        assert!(options.runtime.is_some());
        assert_eq!(overrides.use_global_virtual_store, Some(false));
        assert_eq!(overrides.cache_dir, Some(cache_dir));
        assert_eq!(overrides.store_dir, Some(store_dir));
    }
}
