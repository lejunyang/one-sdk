//! Assembles the `osdk_core::Ctx` from resolved dirs + config + a built client.

use std::path::Path;
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
    pub model_jobs: Option<usize>,
    pub yes: bool,
    pub quiet: bool,
    pub source: Option<String>,
    pub refresh_sources: bool,
    pub source_mode: Option<osdk_core::source::SourceMode>,
    pub offline: bool,
    pub download_cache_max_size: Option<osdk_core::config::DownloadCacheLimit>,
    pub require_checksums: bool,
    pub attestations: Option<osdk_core::config::AttestationPolicy>,
    pub prerelease: Option<osdk_core::config::PrereleasePolicy>,
    pub lang: Option<String>,
}

impl GlobalOverrides {
    fn apply_to(&self, config: &mut Config) {
        if let Some(j) = self.jobs {
            if j > 0 {
                config.settings.jobs = j;
            }
        }
        if let Some(j) = self.model_jobs {
            if j > 0 {
                config.sources.model_jobs = j;
            }
        }
        if self.yes {
            config.settings.yes = true;
        }
        if self.offline {
            config.settings.offline = true;
        }
        if let Some(limit) = self.download_cache_max_size {
            config.settings.download_cache.max_size_bytes = limit;
        }
        if self.require_checksums {
            config.settings.require_checksums = true;
        }
        if let Some(policy) = self.attestations {
            config.settings.attestations = policy;
        }
        if let Some(policy) = self.prerelease {
            config.settings.prerelease = policy;
        }
        if let Some(mode) = self.source_mode {
            config.sources.mode = mode;
        }
    }
}

pub struct App {
    pub ctx: Ctx,
    pub registry: Registry,
    pub prompt: Arc<dyn Prompt>,
    /// A one-shot source id override from `--source`.
    pub source_override: Option<String>,
    pub refresh_sources: bool,
    config_overrides: GlobalOverrides,
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
    /// Rebuild a dependency tool context for the project's own effective config,
    /// preserving the CLI overlays from the original invocation.
    pub fn config_for_project(&self, path: &Path) -> Result<Config> {
        let user = self.ctx.dirs.user_config_file();
        let original = match self
            .ctx
            .config
            .project_config_path
            .as_deref()
            .and_then(Path::parent)
        {
            Some(base) => Config::load(&user, base)?,
            None => Config::load_user(&user)?,
        };
        let mut config = Config::load(&user, path.parent().unwrap())?;
        macro_rules! overlay {
            ($($field:ident).+) => {
                if self.ctx.config.$($field).+ != original.$($field).+ { config.$($field).+ = self.ctx.config.$($field).+.clone(); }
            };
        }
        overlay!(settings.jobs);
        overlay!(settings.yes);
        overlay!(settings.offline);
        overlay!(settings.require_checksums);
        overlay!(settings.attestations);
        overlay!(settings.prerelease);
        overlay!(settings.download_cache.max_size_bytes);
        overlay!(sources.model_jobs);
        overlay!(sources.mode);
        self.config_overrides.apply_to(&mut config);
        Ok(config)
    }

    pub fn with_config(&self, config: Config) -> Result<App> {
        let registry = Registry::load(&self.ctx.dirs)?;
        let prompt = if config.settings.yes == self.ctx.config.settings.yes {
            self.prompt.clone()
        } else {
            Arc::new(TerminalPrompt::new(config.settings.yes)) as Arc<dyn Prompt>
        };
        let mut app = App::from_parts(
            Ctx {
                dirs: self.ctx.dirs.clone(),
                platform: self.ctx.platform,
                config,
                client: self.ctx.client.clone(),
                cas: self.ctx.cas.clone(),
                show_progress: self.ctx.show_progress,
            },
            registry,
            prompt,
            self.source_override.clone(),
            self.refresh_sources,
        );
        app.config_overrides = self.config_overrides.clone();
        Ok(app)
    }

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
            config_overrides: GlobalOverrides::default(),
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

    /// Enforce an additional trust scope after the command has inspected its
    /// concrete plan.
    ///
    /// Task tool dependencies use this only when at least one requested tool is
    /// missing. Keeping the check late avoids making an unrelated `[sources]`
    /// override block tasks that install nothing, while still applying the
    /// normal install gate before any download or install begins.
    pub fn ensure_project_trusted(&self, scopes: &[osdk_core::trust::Scope]) -> Result<()> {
        let Some(project_config) = self.ctx.config.project_config_path.as_ref() else {
            return Ok(());
        };
        let rel_base = self
            .ctx
            .config
            .project
            .as_ref()
            .filter(|p| p.is_external())
            .map(|p| p.root.as_path());
        for source in osdk_core::config::inheritance::sources(project_config, rel_base)? {
            self.ensure_config_trusted(&source, scopes)?;
        }
        Ok(())
    }

    pub fn ensure_config_trusted(
        &self,
        project_config: &Path,
        scopes: &[osdk_core::trust::Scope],
    ) -> Result<()> {
        let requirements = osdk_core::trust::trust_requirements(project_config, scopes)?;
        if requirements.is_empty()
            || osdk_core::trust::is_trusted(
                &self.ctx.dirs.config,
                project_config,
                std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS").as_ref(),
            )?
        {
            return Ok(());
        }
        Err(anyhow::anyhow!(osdk_core::t!(
            "err.untrusted_config",
            path = project_config.display(),
            requirements = osdk_core::trust::describe_requirements(&requirements)
        )))
    }
}

/// How much of the project configuration a command may see, and whether the
/// trust gate applies to it.
///
/// This was one boolean, which conflated two independent questions and so could
/// not express the middle case. `task list` needs the project's `[tasks]` table
/// -- printing it is the command's entire purpose -- but must not be refused on
/// an unreviewed config. With a single flag it got both or neither, and "neither"
/// meant it reported "no tasks defined" for every project on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectAccess {
    /// Load the project config and refuse if it carries an untrusted key
    /// relevant to one of these scopes.
    Gated(&'static [osdk_core::trust::Scope]),
    /// Load the project config but do not enforce the gate: the command only
    /// reports what the config says and acts on nothing.
    ReadOnly,
    /// Do not load the project config at all. Trust management uses this so an
    /// untrusted project cannot influence the decision to trust it.
    Excluded,
}

impl App {
    /// Build the app: resolve dirs, load config, overlay CLI flags, build ctx,
    /// gating on the scopes this command reaches.
    pub fn init(
        overrides: GlobalOverrides,
        scopes: &'static [osdk_core::trust::Scope],
    ) -> Result<App> {
        Self::init_with_project_access(overrides, ProjectAccess::Gated(scopes))
    }

    /// Load the project config without enforcing the trust gate.
    ///
    /// For commands that only *report* what the project declares: they need the
    /// project layer, because it is the thing they exist to print, while
    /// refusing them on an unreviewed config would make the directory hostile
    /// and hide the exit route.
    pub fn init_read_only(overrides: GlobalOverrides) -> Result<App> {
        Self::init_with_project_access(overrides, ProjectAccess::ReadOnly)
    }

    /// Build with the project config excluded entirely.
    ///
    /// For trust management and `config set`/`unset`: an untrusted project must
    /// not influence the decision to trust it, nor the edit that undoes it.
    pub fn init_without_project_config(overrides: GlobalOverrides) -> Result<App> {
        Self::init_with_project_access(overrides, ProjectAccess::Excluded)
    }

    fn init_with_project_access(overrides: GlobalOverrides, access: ProjectAccess) -> Result<App> {
        let dirs = Dirs::resolve().context("resolving osdk directories")?;
        dirs.ensure().context("creating osdk directories")?;

        let cwd = std::env::current_dir().context("getting current dir")?;
        if let ProjectAccess::Gated(scopes) = access {
            if let Some(project_config) = osdk_core::trust::project_config(&cwd)? {
                let rel_base = osdk_core::project::discover(&cwd, &dirs.config)?
                    .filter(|ctx| ctx.is_external())
                    .map(|ctx| ctx.root);
                for project_config in
                    osdk_core::config::inheritance::sources(&project_config, rel_base.as_deref())?
                {
                    let trusted_paths = std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS");
                    let requirements =
                        osdk_core::trust::trust_requirements(&project_config, scopes)?;
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
        }
        // Only `Excluded` drops the project layer. `ReadOnly` must keep it: it
        // skips the *gate*, not the *file*. Loading user-global config alone made
        // `task list` print "no tasks defined" for every project, while
        // `osdk run` -- gated, and therefore using the full loader -- ran those
        // very tasks. Two code paths disagreeing about whether a table exists.
        let mut config = match access {
            ProjectAccess::Gated(_) | ProjectAccess::ReadOnly => {
                Config::load(&dirs.user_config_file(), &cwd)
            }
            ProjectAccess::Excluded => Config::load_user(&dirs.user_config_file()),
        }
        .context("loading configuration")?;

        overrides.apply_to(&mut config);

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

        let app = App {
            config_overrides: overrides.clone(),
            ctx,
            registry,
            prompt,
            source_override: overrides.source,
            refresh_sources: overrides.refresh_sources,
            dynamic_scan: std::cell::RefCell::new(None),
            shim_owners: std::cell::RefCell::new(None),
        };
        if let ProjectAccess::Gated(scopes) = access {
            if scopes.contains(&osdk_core::trust::Scope::Run) {
                if let Some(path) = &app.ctx.config.project_config_path {
                    let rel_base = app
                        .ctx
                        .config
                        .project
                        .as_ref()
                        .filter(|p| p.is_external())
                        .map(|p| p.root.as_path());
                    let sources = osdk_core::config::inheritance::sources(path, rel_base)?;
                    let base = osdk_core::config::inheritance::task_root(&sources, rel_base);
                    for expanded in osdk_core::deps::expand_roots(
                        &base,
                        &app.ctx.config.tasks.config.roots,
                        "task.roots",
                    )? {
                        if let Some(path) = osdk_core::config::PROJECT_CONFIG_NAMES
                            .iter()
                            .map(|name| expanded.directory.join(name))
                            .find(|path| path.is_file())
                        {
                            for source in osdk_core::config::inheritance::sources(&path, None)? {
                                app.ensure_config_trusted(&source, scopes)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(app)
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
