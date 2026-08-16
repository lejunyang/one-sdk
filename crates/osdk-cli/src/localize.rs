//! Runtime localization of the clap command tree.
//!
//! The derive macros bake English `///` docs in at compile time. To translate
//! help output we take the generated `Command`, then walk it applying
//! catalog-backed `about`/`long_about`/arg-help for the active language before
//! parsing. English strings in the derive stay as the developer-facing fallback.

use clap::Command;
use osdk_core::i18n::tr;

/// Apply localized help text to the whole command tree.
pub fn localize(cmd: Command) -> Command {
    let cmd = cmd
        .about(tr("help.about"))
        .long_about(tr("help.long_about"));
    let cmd = localize_global_args(cmd);
    localize_subcommands(cmd)
}

fn h(key: &str) -> String {
    tr(key)
}

fn localize_global_args(cmd: Command) -> Command {
    cmd.mut_arg("verbose", |a| a.help(h("help.flag.verbose")))
        .mut_arg("quiet", |a| a.help(h("help.flag.quiet")))
        .mut_arg("jobs", |a| a.help(h("help.flag.jobs")))
        .mut_arg("yes", |a| a.help(h("help.flag.yes")))
        .mut_arg("source", |a| a.help(h("help.flag.source")))
        .mut_arg("refresh_sources", |a| {
            a.help(h("help.flag.refresh_sources"))
        })
        .mut_arg("offline", |a| a.help(h("help.flag.offline")))
        .mut_arg("require_checksums", |a| {
            a.help(h("help.flag.require_checksums"))
        })
        .mut_arg("attestations", |a| a.help(h("help.flag.attestations")))
        .mut_arg("prerelease", |a| a.help(h("help.flag.prerelease")))
        .mut_arg("lang", |a| a.help(h("help.flag.lang")))
}

fn localize_subcommands(cmd: Command) -> Command {
    cmd.mut_subcommand("install", |c| {
        c.about(h("help.install.about"))
            .long_about(h("help.install.long"))
            .mut_arg("tools", |a| a.help(h("help.install.arg.tools")))
            .mut_arg("opts", |a| a.help(h("help.opt")))
    })
    .mut_subcommand("lock", |c| {
        c.about(h("help.lock.about"))
            .mut_arg("tools", |a| a.help(h("help.install.arg.tools")))
            .mut_arg("opts", |a| a.help(h("help.opt")))
    })
    .mut_subcommand("outdated", |c| c.about(h("help.outdated.about")))
    .mut_subcommand("upgrade", |c| {
        c.about(h("help.upgrade.about"))
            .mut_arg("tools", |a| a.help(h("help.install.arg.tools")))
            .mut_arg("opts", |a| a.help(h("help.opt")))
    })
    .mut_subcommand("exec", |c| c.about(h("help.exec.about")))
    .mut_subcommand("completions", |c| c.about(h("help.completions.about")))
    .mut_subcommand("alias", |c| c.about(h("help.alias.about")))
    .mut_subcommand("list", |c| {
        c.about(h("help.list.about"))
            .mut_arg("tool", |a| a.help(h("help.list.arg.tool")))
    })
    .mut_subcommand("list-remote", |c| {
        c.about(h("help.list_remote.about"))
            .mut_arg("tool", |a| a.help(h("help.list_remote.arg.tool")))
            .mut_arg("filter", |a| a.help(h("help.list_remote.arg.filter")))
    })
    .mut_subcommand("use", |c| {
        c.about(h("help.use.about"))
            .long_about(h("help.use.long"))
            .mut_arg("tool", |a| a.help(h("help.use.arg.tool")))
            .mut_arg("global", |a| a.help(h("help.use.flag.global")))
            .mut_arg("opts", |a| a.help(h("help.opt")))
    })
    .mut_subcommand("uninstall", |c| {
        c.about(h("help.uninstall.about"))
            .mut_arg("tool", |a| a.help(h("help.uninstall.arg.tool")))
    })
    .mut_subcommand("current", |c| c.about(h("help.current.about")))
    .mut_subcommand("where", |c| c.about(h("help.where.about")))
    .mut_subcommand("reshim", |c| c.about(h("help.reshim.about")))
    .mut_subcommand("activate", |c| {
        c.about(h("help.activate.about"))
            .long_about(h("help.activate.long"))
            .mut_arg("shell", |a| a.help(h("help.activate.arg.shell")))
    })
    .mut_subcommand("deactivate", |c| {
        c.about(h("help.deactivate.about"))
            .mut_arg("shell", |a| a.help(h("help.activate.arg.shell")))
    })
    .mut_subcommand("source", |c| {
        c.about(h("help.source.about"))
            .long_about(h("help.source.long"))
            .mut_subcommand("list", |s| s.about(h("help.source.list.about")))
            .mut_subcommand("test", |s| {
                s.about(h("help.source.test.about"))
                    .mut_arg("model", |a| a.help(h("help.source.test.flag.model")))
            })
            .mut_subcommand("add", |s| {
                s.about(h("help.source.add.about"))
                    .mut_arg("forward_credentials", |a| {
                        a.help(h("help.source.add.flag.forward_credentials"))
                    })
            })
            .mut_subcommand("remove", |s| s.about(h("help.source.remove.about")))
            .mut_subcommand("pin", |s| s.about(h("help.source.pin.about")))
            .mut_subcommand("unpin", |s| s.about(h("help.source.unpin.about")))
    })
    .mut_subcommand("config", |c| {
        c.about(h("help.config.about"))
            .mut_subcommand("path", |s| s.about(h("help.config.path.about")))
            .mut_subcommand("list", |s| s.about(h("help.config.list.about")))
    })
    .mut_subcommand("trust", |c| {
        c.about(h("help.trust.about"))
            .mut_arg("path", |a| a.help(h("help.trust.arg.path")))
            .mut_subcommand("list", |s| s.about(h("help.trust.list.about")))
    })
    .mut_subcommand("untrust", |c| {
        c.about(h("help.untrust.about"))
            .mut_arg("path", |a| a.help(h("help.trust.arg.path")))
    })
    .mut_subcommand("node", |c| {
        c.about(h("help.node.about"))
            .mut_subcommand("migrate-packages", |s| {
                s.about(h("help.node.migrate.about"))
                    .mut_arg("from", |a| a.help(h("help.node.migrate.arg.from")))
                    .mut_arg("to", |a| a.help(h("help.node.migrate.arg.to")))
                    .mut_arg("apply", |a| a.help(h("help.node.migrate.flag.apply")))
            })
    })
    .mut_subcommand("python", |c| {
        c.about(h("help.python.about")).mut_subcommand("find", |s| {
            s.about(h("help.python.find.about"))
                .mut_arg("request", |a| a.help(h("help.python.find.arg.request")))
        })
    })
    .mut_subcommand("model", |c| {
        c.about(h("help.model.about"))
            .mut_subcommand("pull", |s| {
                s.about(h("help.model.pull.about"))
                    .mut_arg("name", |a| a.help(h("help.model.pull.arg.name")))
                    .mut_arg("reference", |a| a.help(h("help.model.pull.arg.reference")))
                    .mut_arg("endpoint", |a| a.help(h("help.model.pull.flag.endpoint")))
                    .mut_arg("forward_credentials", |a| {
                        a.help(h("help.model.pull.flag.forward_credentials"))
                    })
                    .mut_arg("include", |a| a.help(h("help.model.pull.flag.include")))
                    .mut_arg("exclude", |a| a.help(h("help.model.pull.flag.exclude")))
                    .mut_arg("variant", |a| a.help(h("help.model.pull.flag.variant")))
                    .mut_arg("no_lock", |a| a.help(h("help.model.pull.flag.no_lock")))
            })
            .mut_subcommand("list", |s| s.about(h("help.model.list.about")))
            .mut_subcommand("path", |s| s.about(h("help.model.path.about")))
            .mut_subcommand("verify", |s| s.about(h("help.model.verify.about")))
            .mut_subcommand("remove", |s| s.about(h("help.model.remove.about")))
            .mut_subcommand("env", |s| {
                s.about(h("help.model.env.about"))
                    .mut_subcommand("enable", |e| {
                        e.about(h("help.model.env.enable.about"))
                            .mut_arg("provider", |a| {
                                a.help(h("help.model.env.enable.arg.provider"))
                            })
                            .mut_arg("force", |a| a.help(h("help.model.env.enable.flag.force")))
                    })
                    .mut_subcommand("disable", |e| {
                        e.about(h("help.model.env.disable.about"))
                            .mut_arg("provider", |a| {
                                a.help(h("help.model.env.enable.arg.provider"))
                            })
                    })
                    .mut_subcommand("list", |e| e.about(h("help.model.env.list.about")))
            })
    })
    .mut_subcommand("rust", |c| c.about(h("help.rust.about")))
    .mut_subcommand("cache", |c| {
        c.about(h("help.cache.about"))
            .mut_subcommand("dir", |s| s.about(h("help.cache.dir.about")))
            .mut_subcommand("env", |s| s.about(h("help.cache.env.about")))
            .mut_subcommand("clean", |s| s.about(h("help.cache.clean.about")))
    })
    .mut_subcommand("prune", |c| {
        c.about(h("help.prune.about"))
            .mut_arg("dry_run", |a| a.help(h("help.prune.flag.dry_run")))
    })
    .mut_subcommand("doctor", |c| c.about(h("help.doctor.about")))
}
