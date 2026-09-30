mod app;
mod cli;
mod commands;
mod config_edit;
mod container;
mod deps_cmd;
mod global_npm_use;
mod localize;
mod lockfile;
mod model_output;
mod model_view;
mod pkg;
mod prompt;
mod proxy_diag;
mod skills_cmd;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};
use std::process::ExitStatus;

use app::{App, GlobalOverrides};
use cli::{Cli, Command};
use osdk_core::i18n;

fn main() {
    // Everything below runs on an owned thread with an explicit stack rather than
    // on the main thread, whose size is fixed at link time. See
    // `DISPATCH_STACK_SIZE` for what overflowed and how it was found.
    let worker = std::thread::Builder::new()
        .name("osdk".into())
        .stack_size(DISPATCH_STACK_SIZE)
        .spawn(main_inner);
    match worker {
        Ok(handle) => {
            if handle.join().is_err() {
                // The panic has already printed itself; exiting non-zero without
                // a second message keeps the output honest.
                std::process::exit(101);
            }
        }
        Err(error) => {
            eprintln!("osdk: cannot start: {error}");
            std::process::exit(1);
        }
    }
}

fn main_inner() {
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
        model_jobs: cli.global.model_jobs,
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

/// Stack for the thread that runs everything after process start.
///
/// Two large frames live on this path, and both are reserved on entry regardless
/// of which command was asked for: `localize(Cli::command())` builds the whole
/// localized clap tree, and `dispatch` is one `async fn` whose future contains
/// every command's future inlined.
///
/// In a debug build that total already sat just under the 1MB the linker gives the
/// main thread. Adding three `bool` fields to `Command` crossed it, and the
/// failure was `osdk --version` dying with
/// `thread 'main' has overflowed its stack` -- a message pointing nowhere near the
/// change, since even `--version` has to build the command tree first. Bisection
/// was the only way to find it: one added field was fine, three were not.
///
/// Every tokio worker already gets its own configurable stack; the main thread was
/// the one place running these frames on a fixed one. 16MB is far beyond the
/// measured need and costs only address space -- the right trade for removing a
/// cliff the next contributor would otherwise fall off while adding an unrelated
/// flag.
const DISPATCH_STACK_SIZE: usize = 16 * 1024 * 1024;

fn run(cli: Cli, overrides: GlobalOverrides) -> Result<Option<ExitStatus>> {
    // Commands that need async use a runtime; sync ones don't strictly need it
    // but we build one uniformly for simplicity.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Three cases, distinguished by what the command can do.
        // `excludes_project_config` is about trust management, which must not
        // read the project at all. Otherwise the command's scopes decide:
        // none means it acts on nothing (read-only), so the project is loaded
        // without the gate; the gate is enforced only for the scopes the
        // command actually reaches.
        let mut app = if excludes_project_config(&cli.command) {
            App::init_without_project_config(overrides)?
        } else {
            let scopes = command_scopes(&cli.command);
            if scopes.is_empty() {
                App::init_read_only(overrides)?
            } else {
                App::init(overrides, scopes)?
            }
        };
        dispatch(&mut app, cli.command).await
    })
}

/// Does this command imply bringing declared \[deps]\ up to date first?
///
/// Only a **bare** \install\, plus un\ and \exec\. Two exclusions are
/// deliberate, and each has a test:
///
/// * \install\ **with operands** means "install this tool". Also rewriting the
///   project's dependency tree would be a side effect nobody asked for -- the same
///   reasoning that makes explicit operands skip lock replay.
/// * un --dry-run\ exists in order to have no effects, so materializing for it
///   would contradict the flag.
fn wants_auto_deps(command: &Command) -> bool {
    match command {
        Command::Install { tools, no_deps, .. } => tools.is_empty() && !no_deps,
        Command::Exec { no_deps, .. } => !no_deps,
        Command::Run {
            dry_run, no_deps, ..
        } => !no_deps && !dry_run,
        _ => false,
    }
}

/// Whether a command must not read the project configuration at all.
///
/// Trust management and the `config set`/`unset`/`migrate` escape hatch: letting an
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
                    | crate::cli::ConfigCommand::Migrate { .. }
            }
    )
}

/// The trust scopes one command reaches.
///
/// Trust gates only what the invocation can actually do: one scope per kind
/// of effect. A `[sys.pkg]` table cannot block `osdk model path`, and
/// `[task]` cannot block `osdk pkg status`, because neither command
/// reaches those tables. The empty list means the command acts on nothing
/// externally -- it is loaded read-only and is never refused.
///
/// Commands that bundle a second behavior carry both scopes. A bare
/// `install` also materializes opt-in auto dependencies; `run` and `exec`
/// do too unless `--no-deps` is passed. `lock` installs Node when the
/// graph contains npm packages. The self upgrader downloads and verifies
/// release bytes like the install path.
fn command_scopes(command: &Command) -> &'static [osdk_core::trust::Scope] {
    use osdk_core::trust::Scope;
    const NONE: &[Scope] = &[];
    const INSTALL: &[Scope] = &[Scope::Install];
    const INSTALL_DEPS: &[Scope] = &[Scope::Install, Scope::Deps];
    const DEPS: &[Scope] = &[Scope::Deps];
    const RUN: &[Scope] = &[Scope::Run];
    const RUN_DEPS: &[Scope] = &[Scope::Run, Scope::Deps];
    const SYSTEM: &[Scope] = &[Scope::SystemPackages];
    const CONTAINER: &[Scope] = &[Scope::Container];

    match command {
        Command::Install { tools, no_deps, .. } => {
            if tools.is_empty() && !no_deps {
                INSTALL_DEPS
            } else {
                INSTALL
            }
        }
        Command::Use { .. } | Command::Upgrade { .. } | Command::Lock { .. } => INSTALL,
        Command::Exec { no_deps, .. } => {
            if *no_deps {
                INSTALL
            } else {
                INSTALL_DEPS
            }
        }
        Command::Run {
            dry_run, no_deps, ..
        } => {
            if *dry_run {
                NONE
            } else if *no_deps {
                RUN
            } else {
                RUN_DEPS
            }
        }
        Command::Deps { .. } => DEPS,
        Command::Pkg {
            command: crate::cli::PkgCommand::Apply { .. },
        } => SYSTEM,
        Command::SelfCmd {
            command: crate::cli::SelfCommand::Upgrade { dry_run: false, .. },
        } => INSTALL,
        Command::Rust {
            command:
                crate::cli::RustCommand::Component {
                    command: crate::cli::RustItemCommand::Add { .. },
                }
                | crate::cli::RustCommand::Target {
                    command: crate::cli::RustItemCommand::Add { .. },
                },
        } => INSTALL,
        Command::Skills {
            command:
                crate::cli::SkillsCommand::Add { .. }
                | crate::cli::SkillsCommand::Sync { .. }
                | crate::cli::SkillsCommand::Update { .. }
                | crate::cli::SkillsCommand::Use { .. },
        } => INSTALL,
        Command::Container { .. } => CONTAINER,
        // Pure reporting, rendering, or changes confined to the managed dir.
        _ => NONE,
    }
}

async fn dispatch(app: &mut App, command: Command) -> Result<Option<ExitStatus>> {
    if let Command::Run { task, .. } = &command {
        commands::preflight_task_tool_references(app, task)?;
    }
    if wants_auto_deps(&command) {
        deps_cmd::materialize_auto(app).await?;
    }

    let result = match command {
        Command::Install {
            tools,
            opts,
            force,
            include_lazy,
            ..
        } => commands::install(app, tools, opts, force, include_lazy).await,
        Command::Lock { tools, opts } => commands::lock(app, tools, opts).await,
        Command::Outdated { tools } => commands::outdated(app, tools).await,
        Command::Upgrade { tools, opts } => commands::upgrade(app, tools, opts).await,
        Command::Exec { tools, command, .. } => commands::exec_cmd(app, tools, command).await,
        Command::Run {
            task,
            dry_run,
            args,
            ..
        } => return commands::run_task(app, task, dry_run, args).await,
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
        Command::Skills { command } => return skills_cmd::run(app, command).await.map(|()| None),
        Command::Deps {
            providers,
            list,
            all,
            dry_run,
            force,
            explain,
            skip,
            filter,
            no_install_tools,
            frozen,
            verify,
        } => {
            deps_cmd::deps(
                app,
                deps_cmd::DepsOptions {
                    providers,
                    list,
                    list_all: all,
                    dry_run,
                    force,
                    explain,
                    skip,
                    filter,
                    no_install_tools,
                    frozen,
                    verify,
                    // Explicit \osdk deps\ covers every configured provider:
                    // naming the command is itself the opt-in, so \uto\ does not
                    // narrow it.
                    auto_only: false,
                },
            )
            .await
        }
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
    use super::{command_scopes, excludes_project_config, wants_auto_deps};
    use crate::cli::{Cli, Command, TaskCommand};
    use clap::Parser;
    use osdk_core::trust::Scope;

    /// Parse one command line exactly the way main does.
    fn command(args: &[&str]) -> crate::cli::Command {
        let mut argv = vec!["osdk"];
        argv.extend_from_slice(args);
        Cli::parse_from(argv).command
    }

    /// The trust scopes are exactly the effects that command has -- no more,
    /// no less. This table is the security boundary: a command scoped too
    /// narrowly acts under unreviewed keys; scoped too broadly cries wolf and
    /// blocks unrelated work.
    #[test]
    fn commands_request_exactly_their_trust_scopes() {
        let cases: &[(&[&str], &[Scope])] = &[
            // Bare install also materializes opt-in deps.
            (&["install"], &[Scope::Install, Scope::Deps]),
            // One named tool: just that install.
            (&["install", "node@22"], &[Scope::Install]),
            (&["install", "--no-deps"], &[Scope::Install]),
            (&["use", "node@22"], &[Scope::Install]),
            (&["upgrade", "node"], &[Scope::Install]),
            // Lock installs Node when an npm graph needs it.
            (&["lock"], &[Scope::Install]),
            // Exec installs its named tools and, by default, brings deps.
            (
                &["exec", "--tool", "node", "--", "node", "-v"],
                &[Scope::Install, Scope::Deps],
            ),
            (
                &["exec", "--no-deps", "--tool", "node", "--", "node", "-v"],
                &[Scope::Install],
            ),
            // Run checks its own runner settings and, by default, deps. If the
            // selected task later needs a missing tool, that install scope is
            // checked immediately before acquisition.
            (&["run", "build"], &[Scope::Run, Scope::Deps]),
            (&["run", "--no-deps", "build"], &[Scope::Run]),
            (&["run", "--dry-run", "build"], &[]),
            // Explicit deps.
            (&["deps"], &[Scope::Deps]),
            // Container operations only reach the container scope.
            (&["container", "pull", "alpine:latest"], &[Scope::Container]),
            // System packages only for pkg apply.
            (&["pkg", "apply"], &[Scope::SystemPackages]),
            (&["pkg", "status"], &[]),
            (&["pkg", "plan"], &[]),
            // The self upgrader installs verified bytes; dry run does not.
            (&["self", "upgrade"], &[Scope::Install]),
            (&["self", "upgrade", "--dry-run"], &[]),
            // Rust component/target add downloads; list does not.
            (&["rust", "component", "add", "rustfmt"], &[Scope::Install]),
            (
                &["rust", "target", "add", "x86_64-linux-android"],
                &[Scope::Install],
            ),
            (&["rust", "component", "list"], &[]),
            // Skills content is installed by add/sync/update/use.
            (&["skills", "add", "github:o/r"], &[Scope::Install]),
            (&["skills", "sync"], &[Scope::Install]),
            (&["skills", "list"], &[]),
            // Models, inspection, rendering and managed-dir changes need nothing.
            (&["model", "path", "fixture"], &[]),
            (&["list"], &[]),
            (&["doctor"], &[]),
            (&["hook-env"], &[]),
            (&["activate", "bash"], &[]),
            (&["reshim"], &[]),
            (&["prune"], &[]),
        ];
        for (args, expected) in cases {
            assert_eq!(
                command_scopes(&command(args)),
                *expected,
                "scopes mismatch for: {}",
                args.join(" ")
            );
        }
    }

    /// Trust management and config writes/migrations are the one group that must not
    /// read the project config at all: an untrusted project cannot take
    /// part in the decision to trust it or the edit that undoes it.
    #[test]
    fn trust_management_and_config_writes_exclude_the_project_config() {
        let excluded: &[&[&str]] = &[
            &["trust"],
            &["untrust"],
            &["config", "set", "jobs", "4"],
            &["config", "unset", "jobs"],
            &["config", "migrate", "--dry-run"],
        ];
        for args in excluded {
            assert!(
                excludes_project_config(&command(args)),
                "must not read the project config: {}",
                args.join(" ")
            );
        }
        // Reporting commands still load the project they report on.
        let included: &[&[&str]] = &[
            &["config", "get", "jobs"],
            &["config", "list"],
            &["list"],
            &["task", "list"],
        ];
        for args in included {
            assert!(
                !excludes_project_config(&command(args)),
                "must still read the project config: {}",
                args.join(" ")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_signal_status_uses_shell_compatible_exit_code() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(native_exit_code(std::process::ExitStatus::from_raw(9)), 137);
    }

    fn install(tools: Vec<String>, no_deps: bool) -> Command {
        Command::Install {
            tools,
            opts: Vec::new(),
            force: false,
            include_lazy: false,
            no_deps,
        }
    }

    /// A bare `install`, `run` and `exec` bring declared dependencies up to date.
    ///
    /// This is the default the user chose, so it is pinned rather than left to the
    /// field: silently not materializing looks like a working command that simply
    /// used a stale environment.
    #[test]
    fn bare_install_run_and_exec_materialize_declared_dependencies() {
        assert!(wants_auto_deps(&install(Vec::new(), false)));
        assert!(wants_auto_deps(&Command::Run {
            task: "build".into(),
            dry_run: false,
            args: Vec::new(),
            no_deps: false,
        }));
        assert!(wants_auto_deps(&Command::Exec {
            tools: vec!["node@22".into()],
            command: vec!["node".into()],
            no_deps: false,
        }));
    }

    /// Valve 1: `--no-deps` opts out for exactly one invocation.
    #[test]
    fn no_deps_suppresses_materialization_on_every_entry_point() {
        assert!(!wants_auto_deps(&install(Vec::new(), true)));
        assert!(!wants_auto_deps(&Command::Run {
            task: "build".into(),
            dry_run: false,
            args: Vec::new(),
            no_deps: true,
        }));
        assert!(!wants_auto_deps(&Command::Exec {
            tools: vec!["node@22".into()],
            command: vec!["node".into()],
            no_deps: true,
        }));
    }

    /// Valve 2: `osdk install <tool>` installs that tool and nothing else.
    ///
    /// Rewriting a project's dependency tree as a side effect of asking for one
    /// tool would be the kind of surprise that makes people stop using the
    /// feature. Same reasoning that makes explicit operands skip lock replay.
    #[test]
    fn an_explicit_operand_keeps_install_to_just_that_tool() {
        assert!(!wants_auto_deps(&install(vec!["node@22".into()], false)));
        assert!(!wants_auto_deps(&install(
            vec!["node@22".into(), "python@3.12".into()],
            false
        )));
    }

    #[test]
    fn include_lazy_is_scoped_to_the_install_command() {
        let Command::Install {
            tools,
            include_lazy,
            ..
        } = command(&["install", "--include-lazy"])
        else {
            panic!("expected install command");
        };
        assert!(tools.is_empty());
        assert!(include_lazy);

        let Command::Install {
            tools,
            include_lazy,
            ..
        } = command(&["install", "node", "--include-lazy"])
        else {
            panic!("expected install command");
        };
        assert_eq!(tools, ["node"]);
        assert!(include_lazy);
    }

    /// Valve 3 (the half of it that is structural): `--dry-run` has no effects.
    ///
    /// The other half -- that the auto path checks freshness and never runs the
    /// deep `--verify` scan -- lives in `deps_cmd::materialize_auto`, which passes
    /// `verify: false`.
    #[test]
    fn dry_run_makes_no_changes_including_dependencies() {
        assert!(!wants_auto_deps(&Command::Run {
            task: "build".into(),
            dry_run: true,
            args: Vec::new(),
            no_deps: false,
        }));
    }

    /// Commands unrelated to running code must not acquire dependencies.
    ///
    /// Stated as a property over a sample rather than one case, so adding a
    /// command does not quietly join the auto path.
    #[test]
    fn unrelated_commands_never_materialize_dependencies() {
        for command in [
            Command::Trust {
                path: None,
                command: None,
            },
            Command::Untrust { path: None },
            Command::Current { tool: None },
            Command::Task {
                command: TaskCommand::List { hidden: false },
            },
        ] {
            assert!(
                !wants_auto_deps(&command),
                "{command:?} must not trigger dependency materialization"
            );
        }
    }

    /// Parsing and dispatch must not run on the process's initial stack.
    ///
    /// The bug this guards was expensive to attribute. `localize(Cli::command())`
    /// is one large frame and `dispatch` is another; the main thread's stack is
    /// fixed at link time, and once the two stopped fitting, the symptom was
    /// `osdk --version` aborting with `thread 'main' has overflowed its stack` --
    /// provoked by three added `bool` fields nowhere near either one.
    ///
    /// An earlier version of this test built the command tree on a probe thread
    /// sized to `DISPATCH_STACK_SIZE`, and was useless: shrinking the constant to
    /// 1MB kept it green, because clap's construction *alone* fits in 1MB. What did
    /// not fit was the real path, where both frames coexist. The probe measured
    /// something other than the thing that broke, so it could not fail.
    ///
    /// The property that actually prevents a relapse is structural -- the work must
    /// be handed to a thread whose stack size we choose -- so it is checked
    /// structurally, on the real source. Deleting the `stack_size` call or calling
    /// `main_inner` directly from `main` makes this fail.
    #[test]
    fn parsing_runs_on_an_explicitly_sized_stack() {
        let source = include_str!("main.rs");

        let main_body = source
            .split_once("\nfn main() {")
            .expect("main must exist")
            .1
            .split_once("\nfn ")
            .expect("main must be followed by another item")
            .0;

        assert!(
            main_body.contains("stack_size(DISPATCH_STACK_SIZE)"),
            "main must hand its work to a thread with an explicit stack; \
             otherwise parsing runs on the linker's fixed stack and \
             `osdk --version` can abort. main body was:\n{main_body}"
        );
        assert!(
            main_body.contains("spawn(main_inner)"),
            "the sized thread must be what runs main_inner, not something else"
        );
        assert!(
            !main_body.contains("main_inner()"),
            "main_inner must not also be called directly on the initial stack"
        );
    }
}
