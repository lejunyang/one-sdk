//! Process-isolated entry point for Aube operations that intentionally mutate
//! process-wide state (notably `aube add --global`).
//!
//! This helper is not an end-user command. The parent `osdk` process supplies
//! an isolated environment and validates the resulting filesystem before it is
//! published.

const OSDK_AUBE_HOST: aube::embed::Host = aube::embed::Host {
    name: "aube",
    display_name: "aube",
    vendor: None,
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("osdk-aube/", env!("CARGO_PKG_VERSION")),
    self_names: aube::embed::AUBE.self_names,
    compatible_names: aube::embed::AUBE.compatible_names,
    lockfile_basename: aube::embed::AUBE.lockfile_basename,
    workspace_yaml: aube::embed::AUBE.workspace_yaml,
    manifest_namespace: aube::embed::AUBE.manifest_namespace,
    env_prefix: Some("AUBE"),
    config_env_prefix: Some("AUBE"),
    cache_namespace: "aube",
    data_namespace: "aube",
    canonical_lockfile_always_wins: true,
    // osdk selects and prepends the managed Node runtime before launching the
    // helper. Do not let the sidecar replace it from project metadata.
    runtime_switching: false,
    // The helper version is the osdk release version, not Aube's package
    // manager version, so it must not satisfy or reject `engines.aube`.
    self_engines_check: false,
    self_update_enabled: false,
};

fn main() {
    std::process::exit(aube::cli_main(&OSDK_AUBE_HOST));
}
