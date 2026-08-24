//! osdk's narrowly-scoped host adapter for the embedded aube package engine.

use std::path::{Path, PathBuf};
use std::sync::Once;

use aube::embed::{
    self, AddToProjectOptions, DepSelection, EmbedderInstallOverrides, EmbedderRuntime,
    InstallControl,
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
}
