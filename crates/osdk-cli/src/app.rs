//! Assembles the `osdk_core::Ctx` from resolved dirs + config + a built client.

use std::sync::Arc;

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::Config;
use osdk_core::dirs::Dirs;
use osdk_core::platform::Platform;
use osdk_core::store::Cas;
use osdk_core::{http, Registry};

use crate::prompt::{Prompt, TerminalPrompt};

/// Global flags that overlay config (highest precedence).
#[derive(Debug, Default, Clone)]
pub struct GlobalOverrides {
    pub jobs: Option<usize>,
    pub yes: bool,
    pub quiet: bool,
    pub source: Option<String>,
    pub refresh_sources: bool,
    pub source_mode: Option<osdk_core::source::SourceMode>,
    pub offline: bool,
    pub require_checksums: bool,
    pub attestations: Option<osdk_core::config::AttestationPolicy>,
    pub prerelease: Option<osdk_core::config::PrereleasePolicy>,
    pub lang: Option<String>,
}

pub struct App {
    pub ctx: Ctx,
    pub registry: Registry,
    pub prompt: Arc<dyn Prompt>,
    /// A one-shot source id override from `--source`.
    pub source_override: Option<String>,
    pub refresh_sources: bool,
    /// Memoized dynamic-tool inventory scan for this process.
    ///
    /// Walking the installs tree is the most expensive read osdk makes, and the
    /// call sites reach it through several layers that each used to scan again:
    /// `reshim` alone went through `all_display_backends`, then
    /// `request_selects_installed_version` and two `installed_shim_owners` calls
    /// per version, so the work grew as backends times versions times published
    /// command names. On a large installs tree that turned one command into
    /// hundreds of full walks and looked like a hang.
    ///
    /// A single command has to see one consistent inventory anyway, so memoizing
    /// it is also the more correct behaviour. Anything that changes the tree
    /// calls `invalidate_dynamic_scan` to drop the memo.
    dynamic_scan: std::cell::RefCell<Option<Arc<osdk_core::inventory::ScanReport>>>,
    /// Memoized shim-ownership map (command name -> owning tool ids).
    ///
    /// Building this walks every install's published commands and resolves each
    /// one, which measured ~3.3s on a large tree. `generate_shims_for` needed it
    /// twice (directly, and again through the conflict check) and `reshim` calls
    /// that once per tool per version, so the cost was paid ~34 times in one
    /// command -- around two minutes of pure recomputation of the same answer.
    ///
    /// Derived from the installs tree and config, so it is invalidated in exactly
    /// the same places as `dynamic_scan`.
    #[allow(clippy::type_complexity)]
    shim_owners: std::cell::RefCell<
        Option<Arc<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>>,
    >,
}

impl App {
    /// Assemble an `App` from already-built parts, with an empty scan memo.
    ///
    /// Used where a command rebuilds the app around freshly loaded config: that
    /// starts a new read cycle, so it must not inherit the previous memo.
    pub fn from_parts(
        ctx: Ctx,
        registry: Registry,
        prompt: Arc<dyn Prompt>,
        source_override: Option<String>,
        refresh_sources: bool,
    ) -> App {
        App {
            ctx,
            registry,
            prompt,
            source_override,
            refresh_sources,
            dynamic_scan: std::cell::RefCell::new(None),
            shim_owners: std::cell::RefCell::new(None),
        }
    }

    /// The dynamic-tool inventory for this process, scanning at most once.
    pub fn dynamic_scan_report(&self) -> Result<Arc<osdk_core::inventory::ScanReport>> {
        if let Some(cached) = self.dynamic_scan.borrow().as_ref() {
            return Ok(Arc::clone(cached));
        }
        let report = Arc::new(osdk_core::shim::scan_dynamic_installs_tolerant(&self.ctx)?);
        *self.dynamic_scan.borrow_mut() = Some(Arc::clone(&report));
        Ok(report)
    }

    /// The memoized shim-ownership map, or `None` when it has not been built.
    #[allow(clippy::type_complexity)]
    pub fn cached_shim_owners(
        &self,
    ) -> Option<Arc<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>> {
        self.shim_owners.borrow().clone()
    }

    /// Store the shim-ownership map for the rest of this command.
    #[allow(clippy::type_complexity)]
    pub fn cache_shim_owners(
        &self,
        owners: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    ) -> Arc<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>> {
        let owners = Arc::new(owners);
        *self.shim_owners.borrow_mut() = Some(Arc::clone(&owners));
        owners
    }

    /// Drop the memo after anything that adds, removes or rewrites an install.
    ///
    /// Callers that mutate the tree and then read it back within the same
    /// command would otherwise act on a stale inventory.
    pub fn invalidate_dynamic_scan(&self) {
        *self.dynamic_scan.borrow_mut() = None;
        // Ownership is derived from the same tree, so it goes stale together.
        *self.shim_owners.borrow_mut() = None;
    }
}

impl App {
    /// Build the app: resolve dirs, load config, overlay CLI flags, build ctx.
    pub fn init(overrides: GlobalOverrides) -> Result<App> {
        Self::init_with_trust(overrides, true)
    }

    pub fn init_without_trust_check(overrides: GlobalOverrides) -> Result<App> {
        Self::init_with_trust(overrides, false)
    }

    fn init_with_trust(overrides: GlobalOverrides, check_trust: bool) -> Result<App> {
        let dirs = Dirs::resolve().context("resolving osdk directories")?;
        dirs.ensure().context("creating osdk directories")?;

        let cwd = std::env::current_dir().context("getting current dir")?;
        if check_trust {
            if let Some(project_config) = osdk_core::trust::project_config(&cwd)? {
                let trusted_paths = std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS");
                let requirements = osdk_core::trust::trust_requirements(&project_config)?;
                if !requirements.is_empty()
                    && !osdk_core::trust::is_trusted(
                        &dirs.config,
                        &project_config,
                        trusted_paths.as_ref(),
                    )?
                {
                    return Err(anyhow::anyhow!(osdk_core::t!(
                        "err.untrusted_config",
                        path = project_config.display(),
                        requirements = osdk_core::trust::describe_requirements(&requirements)
                    )));
                }
            }
        }
        let mut config = if check_trust {
            Config::load(&dirs.user_config_file(), &cwd)
        } else {
            Config::load_user(&dirs.user_config_file())
        }
        .context("loading configuration")?;

        // Overlay CLI flags (highest precedence).
        if let Some(j) = overrides.jobs {
            if j > 0 {
                config.settings.jobs = j;
            }
        }
        if overrides.yes {
            config.settings.yes = true;
        }
        if overrides.offline {
            config.settings.offline = true;
        }
        if overrides.require_checksums {
            config.settings.require_checksums = true;
        }
        if let Some(policy) = overrides.attestations {
            config.settings.attestations = policy;
        }
        if let Some(policy) = overrides.prerelease {
            config.settings.prerelease = policy;
        }
        if let Some(mode) = overrides.source_mode {
            config.sources.mode = mode;
        }

        // Finalize language now that config is loaded. Precedence:
        // --lang / OSDK_LANG / locale (already applied in main) win; otherwise
        // a config `lang` setting takes effect.
        let explicit_or_env = overrides.lang.is_some() || std::env::var("OSDK_LANG").is_ok();
        if !explicit_or_env {
            if let Some(cfg_lang) = config.settings.lang.as_deref() {
                if let Some(l) = osdk_core::i18n::Lang::parse(cfg_lang) {
                    osdk_core::i18n::set_lang(l);
                }
            }
        }

        let client = http::client().context("building http client")?;
        let cas = Arc::new(Cas::new(dirs.store.clone()));
        let registry = Registry::load(&dirs).context("loading declarative backend definitions")?;
        let prompt = Arc::new(TerminalPrompt::new(config.settings.yes));

        let ctx = Ctx {
            dirs,
            platform: Platform::current(),
            config,
            client,
            cas,
            show_progress: !overrides.quiet,
        };

        Ok(App {
            ctx,
            registry,
            prompt,
            source_override: overrides.source,
            refresh_sources: overrides.refresh_sources,
            dynamic_scan: std::cell::RefCell::new(None),
            shim_owners: std::cell::RefCell::new(None),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use osdk_core::config::Config;
    use osdk_core::dirs::Dirs;

    fn test_app(root: &std::path::Path) -> App {
        let dirs = Dirs {
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config"),
            store: root.join("data/store"),
            installs: root.join("data/installs"),
        };
        dirs.ensure().unwrap();
        App::from_parts(
            Ctx {
                dirs,
                platform: Platform::current(),
                config: Config::load_user(&root.join("config/osdk.toml")).unwrap(),
                client: http::client().unwrap(),
                cas: Arc::new(Cas::new(root.join("data/store"))),
                show_progress: false,
            },
            Registry::new(),
            Arc::new(TerminalPrompt::new(true)),
            None,
            false,
        )
    }

    /// The memo has to be observable, otherwise "we cache now" is unfalsifiable.
    /// Writing a manifest between two reads and asserting the second read still
    /// reports the pre-write state is what proves the second call did not walk
    /// the tree again. Without the memo both reads would see one install and the
    /// assertion `installs.is_empty()` would fail.
    #[test]
    fn dynamic_scan_is_memoized_within_one_app() {
        let temporary = tempfile::tempdir().unwrap();
        let app = test_app(temporary.path());

        let first = app.dynamic_scan_report().unwrap();
        assert!(first.installs.is_empty());

        write_one_install(&app);

        let second = app.dynamic_scan_report().unwrap();
        assert!(
            second.installs.is_empty(),
            "second read must come from the memo, not a fresh walk"
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "must hand back the same report"
        );
    }

    /// The ownership map is what actually made `reshim` look like a hang: it cost
    /// ~3.3s on a large tree and was rebuilt for every tool and version. This
    /// asserts the second read is served from the memo, so it cannot silently go
    /// back to recomputing. Without the memo the second call returns a freshly
    /// built map and `Arc::ptr_eq` fails.
    #[test]
    fn shim_owners_are_memoized_within_one_app() {
        let temporary = tempfile::tempdir().unwrap();
        let app = test_app(temporary.path());

        assert!(app.cached_shim_owners().is_none(), "starts empty");
        let built = app.cache_shim_owners(std::collections::BTreeMap::from([(
            "prettier".to_string(),
            std::collections::BTreeSet::from(["npm:prettier".to_string()]),
        )]));
        let again = app.cached_shim_owners().expect("must be cached now");
        assert!(Arc::ptr_eq(&built, &again));

        // Invalidation must clear ownership too: it is derived from the same tree,
        // so keeping it while dropping the scan would hand out a stale map.
        app.invalidate_dynamic_scan();
        assert!(
            app.cached_shim_owners().is_none(),
            "ownership must be invalidated together with the scan"
        );
    }

    /// The other half: a memo that is never dropped would make an install
    /// invisible for the rest of the command, so invalidation must really clear
    /// it. This fails if `invalidate_dynamic_scan` is a no-op.
    #[test]
    fn invalidating_the_memo_exposes_a_new_install() {
        let temporary = tempfile::tempdir().unwrap();
        let app = test_app(temporary.path());

        assert!(app.dynamic_scan_report().unwrap().installs.is_empty());
        write_one_install(&app);
        app.invalidate_dynamic_scan();

        assert_eq!(
            app.dynamic_scan_report().unwrap().installs.len(),
            1,
            "after invalidation the scan must see the new install"
        );
    }

    fn write_one_install(app: &App) {
        use osdk_core::inventory::DynamicToolManifest;
        use osdk_core::tool::{InstallIdentity, InstallScope};
        use std::collections::BTreeMap;

        let identity = InstallIdentity::new(
            "npm:prettier",
            "3.6.2",
            app.ctx.platform.to_string(),
            InstallScope::Isolated,
            &BTreeMap::new(),
            Vec::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let root = app
            .ctx
            .dirs
            .installs
            .join(osdk_core::dirs::sanitize_tool_id(&identity.tool))
            .join(osdk_core::dirs::sanitize_version_component(
                &identity.version,
            ))
            .join(osdk_core::dirs::install_id_component(&identity.install_id).unwrap());
        DynamicToolManifest::from_identity(identity)
            .unwrap()
            .write_atomic(&root)
            .unwrap();
    }
}
