mod app;
mod cli;
mod commands;
mod config_edit;
mod localize;
mod lockfile;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

use app::{App, GlobalOverrides};
use cli::{Cli, Command};
use osdk_core::i18n;

fn main() {
    // Phase 1: pick the language before building help, so `--help`/errors are
    // already localized. `--lang` is scanned from raw args; otherwise fall back
    // to OSDK_LANG / locale.
    let raw: Vec<String> = std::env::args().collect();
    let explicit = scan_lang_flag(&raw);
    let lang = i18n::detect(explicit.as_deref(), |k| std::env::var(k).ok());
    i18n::set_lang(lang);

    // Phase 2: build a localized command tree and parse.
    let cmd = localize::localize(Cli::command());
    let matches = cmd.get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };

    init_tracing(cli.global.verbose);

    let overrides = GlobalOverrides {
        jobs: cli.global.jobs,
        yes: cli.global.yes,
        quiet: cli.global.quiet,
        source: cli.global.source.clone(),
        refresh_sources: cli.global.refresh_sources,
        offline: cli.global.offline,
        require_checksums: cli.global.require_checksums,
        attestations: cli.global.attestations,
        lang: cli.global.lang.clone(),
    };

    if let Err(e) = run(cli, overrides) {
        // Localize osdk-core errors; anyhow wrappers show their chain.
        let msg = e
            .downcast_ref::<osdk_core::Error>()
            .map(|oe| oe.localized())
            .unwrap_or_else(|| format!("{e:#}"));
        eprintln!("{}: {}", i18n::tr("label.error"), msg);
        std::process::exit(1);
    }
}

/// Scan raw argv for `--lang <v>` or `--lang=<v>` (before clap parses).
fn scan_lang_flag(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--lang" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--lang=") {
            return Some(v.to_string());
        }
    }
    None
}

fn run(cli: Cli, overrides: GlobalOverrides) -> Result<()> {
    // Commands that need async use a runtime; sync ones don't strictly need it
    // but we build one uniformly for simplicity.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let mut app = App::init(overrides)?;
        dispatch(&mut app, cli.command).await
    })
}

async fn dispatch(app: &mut App, command: Command) -> Result<()> {
    match command {
        Command::Install { tools, opts } => commands::install(app, tools, opts).await,
        Command::Lock { tools, opts } => commands::lock(app, tools, opts).await,
        Command::Outdated { tools } => commands::outdated(app, tools).await,
        Command::Upgrade { tools, opts } => commands::upgrade(app, tools, opts).await,
        Command::Exec { tools, command } => commands::exec_cmd(app, tools, command).await,
        Command::Completions { shell } => commands::completions(shell),
        Command::Alias { command } => commands::alias(app, command),
        Command::List { tool } => commands::list(app, tool),
        Command::ListRemote { tool, filter } => commands::list_remote(app, tool, filter).await,
        Command::Use { tool, global, opts } => commands::use_cmd(app, tool, global, opts).await,
        Command::Uninstall { tool } => commands::uninstall(app, tool).await,
        Command::Current { tool } => commands::current(app, tool),
        Command::Where { tool } => commands::where_cmd(app, tool),
        Command::Reshim => commands::reshim(app),
        Command::Activate { shell } => commands::activate(app, shell),
        Command::Deactivate { shell } => commands::deactivate(shell),
        Command::HookEnv { shell } => commands::hook_env(app, shell),
        Command::Source { command } => commands::source(app, command).await,
        Command::Config { command } => commands::config(app, command),
        Command::Cache { command } => commands::cache(app, command),
        Command::Prune { dry_run } => commands::prune(app, dry_run),
        Command::Doctor => commands::doctor(app),
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Scope verbosity to osdk crates so -vv/OSDK_LOG=debug doesn't drown the
    // user in reqwest/h2/rustls internals. A bare level still applies globally
    // via OSDK_LOG (e.g. `OSDK_LOG=debug` for everything).
    let default = match verbose {
        0 => "warn",
        1 => "warn,osdk=info,osdk_core=info,osdk_cli=info",
        2 => "warn,osdk=debug,osdk_core=debug,osdk_cli=debug",
        _ => "info,osdk=trace,osdk_core=trace,osdk_cli=trace",
    };
    let filter = EnvFilter::try_from_env("OSDK_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}
