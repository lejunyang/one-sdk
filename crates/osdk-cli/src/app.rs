//! Assembles the `osdk_core::Ctx` from resolved dirs + config + a built client.

use std::sync::Arc;

use anyhow::{Context, Result};
use osdk_core::backend::Ctx;
use osdk_core::config::Config;
use osdk_core::dirs::Dirs;
use osdk_core::platform::Platform;
use osdk_core::store::Cas;
use osdk_core::{http, Registry};

/// Global flags that overlay config (highest precedence).
#[derive(Debug, Default, Clone)]
pub struct GlobalOverrides {
    pub jobs: Option<usize>,
    pub yes: bool,
    pub quiet: bool,
    pub source: Option<String>,
    pub refresh_sources: bool,
}

pub struct App {
    pub ctx: Ctx,
    pub registry: Registry,
    /// A one-shot source id override from `--source`.
    pub source_override: Option<String>,
    pub refresh_sources: bool,
}

impl App {
    /// Build the app: resolve dirs, load config, overlay CLI flags, build ctx.
    pub fn init(overrides: GlobalOverrides) -> Result<App> {
        let dirs = Dirs::resolve().context("resolving osdk directories")?;
        dirs.ensure().context("creating osdk directories")?;

        let cwd = std::env::current_dir().context("getting current dir")?;
        let mut config =
            Config::load(&dirs.user_config_file(), &cwd).context("loading configuration")?;

        // Overlay CLI flags (highest precedence).
        if let Some(j) = overrides.jobs {
            if j > 0 {
                config.settings.jobs = j;
            }
        }
        if overrides.yes {
            config.settings.yes = true;
        }

        let client = http::client().context("building http client")?;
        let cas = Arc::new(Cas::new(dirs.store.clone()));

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
            registry: Registry::new(),
            source_override: overrides.source,
            refresh_sources: overrides.refresh_sources,
        })
    }
}
