//! osdk's narrowly-scoped host adapter for the embedded aube package engine.

use std::path::{Path, PathBuf};
use std::sync::Once;

use aube::cli_args::NetworkArgs;
use aube::embed::{
    self, AddToProjectOptions, DepSelection, EmbedderInstallOverrides, EmbedderRuntime, FrozenMode,
    InstallControl, NetworkMode,
};
use tokio::sync::Mutex;

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
// Aube 2.1 stores its CLI registry override in a process-global RwLock. Keep
// every embedded call in one critical section so each operation observes only
// its own override. The reset guard is declared after the mutex guard below,
// ensuring cancellation clears the override before another caller acquires it.
static AUBE_OPERATION_LOCK: Mutex<()> = Mutex::const_new(());
static AUBE_REGISTRY_OVERRIDE_INSTALLER: AubeRegistryOverrideInstaller =
    AubeRegistryOverrideInstaller;

trait RegistryOverrideInstaller {
    fn install(&self, registry: Option<String>);
}

struct AubeRegistryOverrideInstaller;

impl RegistryOverrideInstaller for AubeRegistryOverrideInstaller {
    fn install(&self, registry: Option<String>) {
        NetworkArgs {
            registry,
            ..Default::default()
        }
        .install_overrides();
    }
}

struct RegistryOverrideReset<'a, I: RegistryOverrideInstaller + ?Sized> {
    installer: &'a I,
}

impl<'a, I: RegistryOverrideInstaller + ?Sized> RegistryOverrideReset<'a, I> {
    fn install(installer: &'a I, registry: Option<String>) -> Self {
        installer.install(registry);
        Self { installer }
    }
}

impl<I: RegistryOverrideInstaller + ?Sized> Drop for RegistryOverrideReset<'_, I> {
    fn drop(&mut self) {
        self.installer.install(None);
    }
}

async fn with_registry_override<T, F, Fut, I>(
    operation_lock: &Mutex<()>,
    installer: &I,
    registry: Option<String>,
    operation: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
    I: RegistryOverrideInstaller + ?Sized,
{
    let _operation_guard = operation_lock.lock().await;
    let _reset = RegistryOverrideReset::install(installer, registry);
    operation().await
}

async fn with_aube_registry_override<T, F, Fut>(registry: Option<String>, operation: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    with_registry_override(
        &AUBE_OPERATION_LOCK,
        &AUBE_REGISTRY_OVERRIDE_INSTALLER,
        registry,
        operation,
    )
    .await
}

pub struct EmbeddedInstallRequest<'a> {
    pub project_dir: &'a Path,
    pub packages: &'a [String],
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub scripts_enabled: bool,
    pub dangerously_allow_all_builds: bool,
    pub offline: bool,
    pub registry: Option<String>,
}

/// Add one or more dependencies directly to a user-owned project. Project
/// installs deliberately share osdk's Aube cache and store, but always use the
/// caller-selected managed Node runtime and never execute lifecycle scripts.
pub struct EmbeddedProjectAddRequest<'a> {
    pub project_dir: &'a Path,
    pub packages: &'a [String],
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub save_dev: bool,
    pub save_optional: bool,
    pub save_peer: bool,
    pub offline: bool,
    pub registry: Option<String>,
}

pub struct EmbeddedFrozenInstallRequest<'a> {
    pub project_dir: &'a Path,
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub scripts_enabled: bool,
    pub dangerously_allow_all_builds: bool,
    pub offline: bool,
    pub registry: Option<String>,
}

pub struct EmbeddedLockGraphRequest<'a> {
    pub project_dir: &'a Path,
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub node_bin_dir: PathBuf,
    pub offline: bool,
    pub registry: Option<String>,
}

pub fn initialize() {
    INIT.call_once(|| embed::initialize(&OSDK_HOST, Vec::new()));
}

pub async fn install_packages(request: EmbeddedInstallRequest<'_>) -> Result<()> {
    let registry = request.registry.clone();
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

    with_aube_registry_override(registry, || async {
        initialize();
        embed::add_with_overrides(request.project_dir, request.packages, options, overrides).await
    })
    .await
    .map_err(|error| Error::other(format_aube_error(&error)))
}

pub async fn add_to_project(request: EmbeddedProjectAddRequest<'_>) -> Result<()> {
    let registry = request.registry.clone();
    let (options, overrides) = project_add_options(&request);

    with_aube_registry_override(registry, || async {
        initialize();
        embed::add_with_overrides(request.project_dir, request.packages, options, overrides).await
    })
    .await
    .map_err(|error| Error::other(format_aube_error(&error)))
}

fn project_add_options(
    request: &EmbeddedProjectAddRequest<'_>,
) -> (AddToProjectOptions, EmbedderInstallOverrides) {
    let options = AddToProjectOptions {
        save_dev: request.save_dev,
        save_exact: false,
        save_optional: request.save_optional,
        save_peer: request.save_peer,
        ignore_scripts: true,
        dangerously_allow_all_builds: false,
        offline: request.offline,
        dep_selection: DepSelection::All,
        control: InstallControl::silent(),
        runtime: Some(EmbedderRuntime::selector(request.node_bin_dir.clone())),
        ..Default::default()
    };
    let overrides = EmbedderInstallOverrides {
        use_global_virtual_store: Some(false),
        cache_dir: Some(request.cache_dir.clone()),
        store_dir: Some(request.store_dir.clone()),
    };
    (options, overrides)
}

pub async fn install_frozen(request: EmbeddedFrozenInstallRequest<'_>) -> Result<()> {
    let registry = request.registry.clone();
    let (options, overrides) = frozen_install_options(request);
    with_aube_registry_override(registry, || async {
        initialize();
        embed::install_with_overrides(options, overrides).await
    })
    .await
    .map_err(|error| Error::other(format_aube_error(&error)))
}

pub async fn prepare_lock_graph(request: EmbeddedLockGraphRequest<'_>) -> Result<()> {
    let registry = request.registry.clone();
    let (options, overrides) = lock_graph_options(request);
    with_aube_registry_override(registry, || async {
        initialize();
        embed::install_with_overrides(options, overrides).await
    })
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
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use tokio::sync::{Barrier, Notify};

    #[derive(Default)]
    struct RecordingRegistryOverrideInstaller {
        state: StdMutex<RecordingRegistryOverrideState>,
    }

    #[derive(Default)]
    struct RecordingRegistryOverrideState {
        current: Option<String>,
        history: Vec<Option<String>>,
    }

    impl RegistryOverrideInstaller for RecordingRegistryOverrideInstaller {
        fn install(&self, registry: Option<String>) {
            let mut state = self.state.lock().expect("recording lock poisoned");
            state.current = registry.clone();
            state.history.push(registry);
        }
    }

    impl RecordingRegistryOverrideInstaller {
        fn current(&self) -> Option<String> {
            self.state
                .lock()
                .expect("recording lock poisoned")
                .current
                .clone()
        }

        fn history(&self) -> Vec<Option<String>> {
            self.state
                .lock()
                .expect("recording lock poisoned")
                .history
                .clone()
        }
    }

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
            registry: None,
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
            registry: None,
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

    #[test]
    fn project_add_options_disable_scripts_and_preserve_selected_section() {
        let project_dir = PathBuf::from("/tmp/osdk-aube-project");
        let cache_dir = PathBuf::from("/tmp/osdk-aube-cache");
        let store_dir = PathBuf::from("/tmp/osdk-aube-store");
        let request = EmbeddedProjectAddRequest {
            project_dir: &project_dir,
            packages: &["prettier@3".into()],
            cache_dir: cache_dir.clone(),
            store_dir: store_dir.clone(),
            node_bin_dir: PathBuf::from("/tmp/osdk-node-bin"),
            save_dev: true,
            save_optional: false,
            save_peer: false,
            offline: false,
            registry: None,
        };
        let (options, overrides) = project_add_options(&request);

        assert!(options.save_dev);
        assert!(!options.save_optional);
        assert!(!options.save_peer);
        assert!(options.ignore_scripts);
        assert!(!options.dangerously_allow_all_builds);
        assert!(options.runtime.is_some());
        assert_eq!(overrides.use_global_virtual_store, Some(false));
        assert_eq!(overrides.cache_dir, Some(cache_dir));
        assert_eq!(overrides.store_dir, Some(store_dir));
    }

    #[tokio::test]
    async fn registry_override_scope_passes_some_and_none_then_resets() {
        let operation_lock = Mutex::new(());
        let installer = RecordingRegistryOverrideInstaller::default();
        let registry = "https://registry.example.test/".to_string();

        let observed = with_registry_override(
            &operation_lock,
            &installer,
            Some(registry.clone()),
            || async { installer.current() },
        )
        .await;
        assert_eq!(observed, Some(registry.clone()));
        assert_eq!(installer.current(), None);

        let observed = with_registry_override(&operation_lock, &installer, None, || async {
            installer.current()
        })
        .await;
        assert_eq!(observed, None);
        assert_eq!(installer.current(), None);
        assert_eq!(installer.history(), vec![Some(registry), None, None, None]);
    }

    #[tokio::test]
    async fn registry_override_scope_resets_after_error() {
        let operation_lock = Mutex::new(());
        let installer = RecordingRegistryOverrideInstaller::default();
        let registry = "https://broken.example.test/".to_string();

        let result: std::result::Result<(), &'static str> = with_registry_override(
            &operation_lock,
            &installer,
            Some(registry.clone()),
            || async {
                assert_eq!(installer.current(), Some(registry));
                Err("expected failure")
            },
        )
        .await;

        assert_eq!(result, Err("expected failure"));
        assert_eq!(installer.current(), None);
        assert_eq!(
            installer.history(),
            vec![Some("https://broken.example.test/".to_string()), None]
        );
    }

    #[tokio::test]
    async fn registry_override_scope_resets_before_unlock_when_cancelled() {
        let operation_lock = Mutex::new(());
        let installer = RecordingRegistryOverrideInstaller::default();
        let entered = Notify::new();

        let mut operation = Box::pin(with_registry_override(
            &operation_lock,
            &installer,
            Some("https://cancelled.example.test/".to_string()),
            || async {
                entered.notify_one();
                std::future::pending::<()>().await;
            },
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                () = entered.notified() => {}
                () = &mut operation => panic!("operation unexpectedly completed"),
            }
        })
        .await
        .expect("registry-scoped operation did not start");
        drop(operation);

        assert_eq!(installer.current(), None);
        assert!(operation_lock.try_lock().is_ok());
        assert_eq!(
            installer.history(),
            vec![Some("https://cancelled.example.test/".to_string()), None]
        );
    }

    #[tokio::test]
    async fn concurrent_registry_override_scopes_do_not_cross() {
        let operation_lock = Mutex::new(());
        let installer = RecordingRegistryOverrideInstaller::default();
        let first_registry = "https://first.example.test/".to_string();
        let second_registry = "https://second.example.test/".to_string();
        let first_started = Barrier::new(3);
        let release_first = Notify::new();

        let first = with_registry_override(
            &operation_lock,
            &installer,
            Some(first_registry.clone()),
            || async {
                assert_eq!(installer.current(), Some(first_registry.clone()));
                first_started.wait().await;
                release_first.notified().await;
                assert_eq!(installer.current(), Some(first_registry.clone()));
            },
        );
        let second = async {
            first_started.wait().await;
            with_registry_override(
                &operation_lock,
                &installer,
                Some(second_registry.clone()),
                || async {
                    assert_eq!(installer.current(), Some(second_registry.clone()));
                },
            )
            .await;
        };
        let observe_while_second_is_waiting = async {
            first_started.wait().await;
            tokio::task::yield_now().await;
            assert_eq!(installer.current(), Some(first_registry.clone()));
            assert_eq!(installer.history(), vec![Some(first_registry.clone())]);
            release_first.notify_one();
        };

        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(first, second, observe_while_second_is_waiting);
        })
        .await
        .expect("concurrent registry override scopes did not complete");

        assert_eq!(installer.current(), None);
        assert_eq!(
            installer.history(),
            vec![Some(first_registry), None, Some(second_registry), None]
        );
    }
}
