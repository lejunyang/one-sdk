mod app;
mod cli;
mod commands;
mod config_edit;

use anyhow::Result;
use clap::Parser;

use app::{App, GlobalOverrides};
use cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.global.verbose);

    let overrides = GlobalOverrides {
        jobs: cli.global.jobs,
        yes: cli.global.yes,
        quiet: cli.global.quiet,
        source: cli.global.source.clone(),
        refresh_sources: cli.global.refresh_sources,
    };

    if let Err(e) = run(cli, overrides) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
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
        Command::Install { tools } => commands::install(app, tools).await,
        Command::List { tool } => commands::list(app, tool),
        Command::ListRemote { tool, filter } => commands::list_remote(app, tool, filter).await,
        Command::Use { tool, global } => commands::use_cmd(app, tool, global).await,
        Command::Uninstall { tool } => commands::uninstall(app, tool).await,
        Command::Current { tool } => commands::current(app, tool),
        Command::Where { tool } => commands::where_cmd(app, tool),
        Command::Reshim => commands::reshim(app),
        Command::Source { command } => commands::source(app, command).await,
        Command::Config { command } => commands::config(app, command),
        Command::Prune { dry_run } => commands::prune(app, dry_run),
        Command::Doctor => commands::doctor(app),
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("OSDK_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}
