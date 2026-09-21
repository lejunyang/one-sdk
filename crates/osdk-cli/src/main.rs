mod app;
mod cli;
mod commands;
mod config_edit;
mod container;
mod global_npm_use;
mod localize;
mod lockfile;
mod model_view;
mod pkg;
mod prompt;
mod proxy_diag;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};
use std::process::ExitStatus;

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
        source_mode: cli.global.source_mode,
        offline: cli.global.offline,
        require_checksums: cli.global.require_checksums,
        attestations: cli.global.attestations,
        prerelease: cli.global.prerelease,
        lang: cli.global.lang.clone(),
    };

    match run(cli, overrides) {
        Ok(Some(status)) if !status.success() => std::process::exit(native_exit_code(status)),
        Ok(_) => {}
        Err(e) => {
            // Localize osdk-core errors; anyhow wrappers show their chain.
            let msg = e
                .downcast_ref::<osdk_core::Error>()
                .map(|oe| oe.localized())
                .unwrap_or_else(|| format!("{e:#}"));
            eprintln!("{}: {}", i18n::tr("label.error"), msg);
            std::process::exit(1);
        }
    }
}

fn native_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
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

fn run(cli: Cli, overrides: GlobalOverrides) -> Result<Option<ExitStatus>> {
    // Commands that need async use a runtime; sync ones don't strictly need it
    // but we build one uniformly for simplicity.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Three cases, not two. `excludes_project_config` is about trust
        // management, which must not read the project at all;
        // `bypasses_trust_check` is about read-only commands, which must read it
        // and only skip the refusal.
        let mut app = if excludes_project_config(&cli.command) {
            App::init_without_project_config(overrides)?
        } else if bypasses_trust_check(&cli.command) {
            App::init_read_only(overrides)?
        } else {
            App::init(overrides)?
        };
        dispatch(&mut app, cli.command).await
    })
}

/// Whether a command must not read the project configuration at all.
///
/// Trust management and the `config set`/`unset` escape hatch: letting an
/// untrusted project take part in the decision to trust it, or in the edit that
/// brings it back into shape, would defeat the point of the gate.
///
/// Distinct from [`bypasses_trust_check`], which is about commands that *do*
/// read the project config and merely skip the refusal. Conflating the two is
/// what made `osdk task list` report "no tasks defined" for every project.
fn excludes_project_config(command: &Command) -> bool {
    matches!(
        command,
        Command::Trust { .. }
            | Command::Untrust { .. }
            | Command::Config {
                command: crate::cli::ConfigCommand::Set { .. }
                    | crate::cli::ConfigCommand::Unset { .. }
            }
    )
}

/// Whether a command runs before the project config is trusted.
///
/// Trust exists to stop an unreviewed config from *doing* something. A command
/// that acts on nothing has nothing to gate, and refusing it only makes the
/// directory hostile: `osdk list` printing "project config is not trusted" tells
/// the user a config they have not reviewed is preventing them from *looking at
/// what is already installed*, which is both useless and alarming. Worse, it
/// hides the exit route -- `osdk trust` wants to be read before it is run, and
/// the commands that show you what you are about to approve were themselves
/// refused.
///
/// Three groups are exempt.
///
/// Trust management itself, for the obvious reason.
///
/// `config set` / `config unset`: the way an untrusted config is edited back
/// into shape. Gating them would block the only exit with the very config being
/// undone. Each addresses one named key in one named file and never acts on what
/// the untrusted config asks for. `config get` and `config list` stay gated
/// precisely because they *do* report that config's merged values.
///
/// Read-only inspection: these resolve and print state, and reach no install,
/// build, download, subprocess or host mutation. They are also what a person
/// runs *while deciding* whether to trust a project.
///
/// Everything else stays gated, which is the fail-closed direction: a new
/// command is gated until someone deliberately lists it here, rather than
/// slipping through because it was forgotten.
fn bypasses_trust_check(command: &Command) -> bool {
    matches!(
        command,
        Command::Trust { .. }
            | Command::Untrust { .. }
            | Command::Config {
                command: crate::cli::ConfigCommand::Set { .. }
                    | crate::cli::ConfigCommand::Unset { .. }
            }
            // Read-only: report existing state, act on nothing.
            | Command::List { .. }
            | Command::Current { .. }
            | Command::Where { .. }
            | Command::Doctor { .. }
            | Command::Completions { .. }
            // `task list` / `info` / `deps` only print what the file declares;
            // `osdk run` is what would execute it, and stays gated.
            | Command::Task {
                command: crate::cli::TaskCommand::List { .. }
                    | crate::cli::TaskCommand::Info { .. }
                    | crate::cli::TaskCommand::Deps { .. }
            }
    )
}

async fn dispatch(app: &mut App, command: Command) -> Result<Option<ExitStatus>> {
    let result = match command {
        Command::Install { tools, opts, force } => commands::install(app, tools, opts, force).await,
        Command::Lock { tools, opts } => commands::lock(app, tools, opts).await,
        Command::Outdated { tools } => commands::outdated(app, tools).await,
        Command::Upgrade { tools, opts } => commands::upgrade(app, tools, opts).await,
        Command::Exec { tools, command } => commands::exec_cmd(app, tools, command).await,
        Command::Run {
            task,
            dry_run,
            args,
        } => return commands::run_task(app, task, dry_run, args),
        Command::Task { command } => commands::task(app, command),
        Command::Completions { shell } => commands::completions(shell),
        Command::Alias { command } => commands::alias(app, command),
        Command::List { tool } => commands::list(app, tool),
        Command::ListRemote { tool, filter } => commands::list_remote(app, tool, filter).await,
        Command::Use { tool, global, opts } => commands::use_cmd(app, tool, global, opts).await,
        Command::Uninstall { tool, global } => commands::uninstall(app, tool, global).await,
        Command::Current { tool } => commands::current(app, tool),
        Command::Where { tool, global, bins } => commands::where_cmd(app, tool, global, bins),
        Command::Reshim => commands::reshim(app),
        Command::Activate { shell } => commands::activate(app, shell),
        Command::Deactivate { shell } => commands::deactivate(shell),
        Command::HookEnv { shell } => commands::hook_env(app, shell),
        Command::Source { command } => commands::source(app, command).await,
        Command::Registry { command } => commands::registry(app, command).await,
        Command::Config { command } => commands::config(app, command),
        Command::Trust { path, command } => commands::trust(app, path, command),
        Command::Untrust { path } => commands::untrust(app, path),
        Command::Node { command } => commands::node(app, command),
        Command::Python { command } => commands::python(app, command),
        Command::Android { command } => commands::android(app, command).await,
        Command::Model { command } => commands::model(app, command).await,
        Command::Rust { command } => commands::rust(app, command).await,
        Command::Cache { command } => commands::cache(app, command),
        Command::Container { command } => return container::run(app, command).await,
        Command::Pkg { command } => pkg::run(app, command).await,
        Command::SelfCmd { command } => commands::self_command(app, command).await,
        Command::Prune { dry_run } => commands::prune(app, dry_run),
        Command::Doctor { verify, tool } => commands::doctor(app, verify, tool),
    };
    result.map(|()| None)
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::native_exit_code;
    use super::{bypasses_trust_check, excludes_project_config};
    use crate::cli::{Command, ConfigCommand, TaskCommand};

    #[test]
    fn only_trust_management_and_config_writes_skip_the_trust_gate() {
        // This list is a security boundary: anything exempted here runs with an
        // untrusted project config present. Reading commands must stay gated,
        // because they report that config's merged values.
        assert!(bypasses_trust_check(&Command::Trust {
            path: None,
            command: None
        }));
        assert!(bypasses_trust_check(&Command::Untrust { path: None }));
        assert!(bypasses_trust_check(&Command::Config {
            command: ConfigCommand::Set {
                key: "jobs".into(),
                value: "4".into(),
                global: false
            }
        }));
        assert!(bypasses_trust_check(&Command::Config {
            command: ConfigCommand::Unset {
                key: "jobs".into(),
                global: false
            }
        }));

        assert!(!bypasses_trust_check(&Command::Config {
            command: ConfigCommand::Get {
                key: "jobs".into(),
                global: false
            }
        }));
        assert!(!bypasses_trust_check(&Command::Config {
            command: ConfigCommand::List
        }));
        assert!(!bypasses_trust_check(&Command::Config {
            command: ConfigCommand::Path
        }));
    }

    /// The two exemptions are different things and must not drift back together.
    ///
    /// `excludes_project_config` means "do not read the project at all", and only
    /// trust management may claim it. `bypasses_trust_check` means "read it, but
    /// do not refuse" -- and a command in that group that also excluded the
    /// config would be reporting on a file it never opened, which is exactly the
    /// bug where `task list` printed "no tasks defined" for every project.
    #[test]
    fn read_only_commands_read_the_project_config_while_trust_management_does_not() {
        for command in [
            Command::Trust {
                path: None,
                command: None,
            },
            Command::Untrust { path: None },
            Command::Config {
                command: ConfigCommand::Set {
                    key: "jobs".into(),
                    value: "4".into(),
                    global: false,
                },
            },
        ] {
            assert!(
                excludes_project_config(&command),
                "trust management must not read the project config: {command:?}"
            );
        }

        // Every read-only command reports on the project config, so none of them
        // may exclude it.
        for command in [
            Command::List { tool: None },
            Command::Current { tool: None },
            Command::Doctor {
                verify: false,
                tool: None,
            },
            Command::Task {
                command: TaskCommand::List { hidden: false },
            },
            Command::Task {
                command: TaskCommand::Info {
                    task: "build".into(),
                },
            },
            Command::Task {
                command: TaskCommand::Deps {
                    task: "build".into(),
                },
            },
        ] {
            assert!(
                !excludes_project_config(&command),
                "read-only command must still load the project config it reports on: {command:?}"
            );
        }
    }

    /// Read-only commands must not be refused, and acting commands must be.
    ///
    /// Both directions matter, and they fail in opposite ways. Gating a
    /// read-only command makes the directory hostile for no safety at all --
    /// `osdk list` refusing to show what is already installed, and, worse,
    /// hiding the very commands a person would use to decide whether to trust
    /// the project. Exempting an acting command is the real hazard: it would run
    /// under a config nobody reviewed.
    #[test]
    fn read_only_commands_are_not_gated_but_acting_ones_are() {
        // Resolve and print state; reach no install, download, subprocess or
        // host mutation.
        for command in [
            Command::List { tool: None },
            Command::Current { tool: None },
            Command::Where {
                tool: "node".into(),
                global: false,
                bins: false,
            },
            Command::Doctor {
                verify: false,
                tool: None,
            },
            Command::Completions {
                shell: clap_complete::Shell::Bash,
            },
            Command::Task {
                command: TaskCommand::List { hidden: false },
            },
            Command::Task {
                command: TaskCommand::Info {
                    task: "build".into(),
                },
            },
            Command::Task {
                command: TaskCommand::Deps {
                    task: "build".into(),
                },
            },
        ] {
            assert!(
                bypasses_trust_check(&command),
                "read-only command must not be refused: {command:?}"
            );
        }

        // These install, build, download, mutate the host, or run something the
        // untrusted config chose. They stay gated.
        for command in [
            Command::Run {
                task: "build".into(),
                dry_run: false,
                args: Vec::new(),
            },
            Command::Reshim,
            Command::Prune { dry_run: false },
            Command::HookEnv {
                shell: "bash".into(),
            },
        ] {
            assert!(
                !bypasses_trust_check(&command),
                "command that acts must stay gated: {command:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_signal_status_uses_shell_compatible_exit_code() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(native_exit_code(std::process::ExitStatus::from_raw(9)), 137);
    }
}
