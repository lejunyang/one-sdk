//! Project tasks: declarative commands with a dependency graph.
//!
//! The shape is deliberately layered so that the simple case stays one line and
//! the complex case does not force the simple ones to be rewritten:
//!
//! ```toml
//! [tasks]
//! build = "cargo build --release"          # tier 1: bare string
//!
//! [tasks.ci]
//! run = ["cargo fmt --check", "cargo test"] # tier 2: sequence + metadata
//! depends = ["build"]
//! ```
//!
//! Two decisions here are load-bearing and are the reason this module exists at
//! all rather than the runner reading raw TOML:
//!
//! - **`&` is never the answer.** Its meaning is not portable: on `cmd` it is
//!   unconditional sequencing that *swallows the preceding failure*, on
//!   PowerShell 7 it backgrounds a job, and on PowerShell 5.1 it is a parse
//!   error. A `run` array that relied on it would do two different things on two
//!   platforms without reporting anything. So the two things people actually
//!   want are separate fields: [`RunStep::Command::ignore_error`] for "keep
//!   going" and [`RunStep::Parallel`] for real concurrency, where the runner can
//!   wait and collect exit codes.
//! - **Platform filtering reuses `when`**, the same nested vocabulary as
//!   `[tools]`, rather than a second spelling. A filtered-out task is remembered
//!   (see [`TaskSet::excluded`]) so `osdk run` can say why it is absent instead
//!   of reporting an unknown name.

pub mod args;
pub mod files;
pub mod freshness;
pub mod runner;
// The embedded interpreter is the fourth tier and needs a C compiler to build,
// so it is opt-out via scripts. Gating it also keeps the engine out of the
// shim, which never evaluates a task.
#[cfg(feature = "scripts")]
pub mod script;
pub mod tree;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::platform::{Platform, PlatformFilter};

/// Config key carrying a task's platform filter, shared with `[tools]`.
pub const PLATFORM_FILTER_KEY: &str = "when";

/// One entry of a `run` list.
///
/// Ordinary commands are plain strings; the table forms exist for the two cases
/// a shell operator would otherwise be reached for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RunStep {
    /// `run = ["cargo build"]` -- run it, stop the task if it fails.
    Simple(String),
    /// `{ cmd = "...", ignore_error = true }` -- run it, keep going on failure.
    ///
    /// This is what `cmd`'s `&` actually does, given a name. make spells it with
    /// a leading `-`, Task calls it `ignore_error`, cargo-make `ignore_errors`;
    /// all three use a field rather than a shell operator, and so does this.
    Command {
        cmd: String,
        #[serde(default)]
        ignore_error: bool,
    },
    /// `{ tasks = ["a", "b"] }` -- run these tasks concurrently, then continue.
    ///
    /// Distinct from `depends`: prerequisites carry no ordering among
    /// themselves, so they cannot express "first A, then B and C together".
    Parallel { tasks: Vec<String> },
    /// `{ argv = ["kubectl", "apply", "-f", "{{manifest}}"] }` -- exec directly.
    ///
    /// The only step where `{{placeholders}}` are substituted, and the reason is
    /// structural rather than careful: each element becomes one argv entry with
    /// no shell in between, so there is no parser for a value to be
    /// reinterpreted by. Interpolating into a shell string cannot be made safe
    /// on `cmd`, where `%VAR%` expands before quoting is considered -- see
    /// `tasks::args`.
    ///
    /// The cost is that pipes, redirection and globbing are unavailable here;
    /// those still belong in a `cmd` string or a script file.
    Argv {
        argv: Vec<String>,
        #[serde(default)]
        ignore_error: bool,
    },
}

/// Hand-written deserializers, because `#[serde(untagged)]` cannot report which
/// field was wrong.
///
/// With a derived untagged enum, `dependson = [...]` produces
/// `data did not match any variant of untagged enum TaskEntry` -- a message that
/// names neither the typo nor the line. serde tries each variant, discards the
/// individual errors, and reports only that all of them failed. Since a typo in
/// `depends` silently means "no prerequisites", the task would run too early and
/// the error message would point nowhere near the cause.
///
/// Dispatching on the TOML shape first, then deserializing exactly one variant,
/// lets that variant's own `deny_unknown_fields` error through intact.
mod de {
    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct CommandStep {
        pub cmd: String,
        #[serde(default)]
        pub ignore_error: bool,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ParallelStep {
        pub tasks: Vec<String>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct ArgvStep {
        pub argv: Vec<String>,
        #[serde(default)]
        pub ignore_error: bool,
    }
}

impl<'de> Deserialize<'de> for RunStep {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(cmd) => Ok(RunStep::Simple(cmd)),
            toml::Value::Table(ref table) => {
                let has_cmd = table.contains_key("cmd");
                let has_tasks = table.contains_key("tasks");
                if table.contains_key("argv") {
                    if has_cmd || has_tasks {
                        return Err(D::Error::custom(
                            "a run step sets `argv`, `cmd`, or `tasks` -- not several",
                        ));
                    }
                    return de::ArgvStep::deserialize(value)
                        .map(|step| RunStep::Argv {
                            argv: step.argv,
                            ignore_error: step.ignore_error,
                        })
                        .map_err(D::Error::custom);
                }
                match (has_cmd, has_tasks) {
                    (true, true) => Err(D::Error::custom(
                        "a run step sets either `cmd` or `tasks`, not both",
                    )),
                    (true, false) => de::CommandStep::deserialize(value)
                        .map(|step| RunStep::Command {
                            cmd: step.cmd,
                            ignore_error: step.ignore_error,
                        })
                        .map_err(D::Error::custom),
                    (false, true) => de::ParallelStep::deserialize(value)
                        .map(|step| RunStep::Parallel { tasks: step.tasks })
                        .map_err(D::Error::custom),
                    (false, false) => Err(D::Error::custom(
                        "a run step table needs `cmd` (through a shell), `argv` (executed \
                         directly, supports {{placeholders}}), or `tasks` (run in parallel)",
                    )),
                }
            }
            other => Err(D::Error::custom(format!(
                "a run step is a command string or a table, found {}",
                other.type_str()
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for RunSpec {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(cmd) => Ok(RunSpec::One(cmd)),
            toml::Value::Array(_) => Vec::<RunStep>::deserialize(value)
                .map(RunSpec::Many)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "`run` is a command string or a list of steps, found {}",
                other.type_str()
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for TaskEntry {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(_) | toml::Value::Array(_) => RunSpec::deserialize(value)
                .map(TaskEntry::Bare)
                .map_err(D::Error::custom),
            toml::Value::Table(_) => TaskDef::deserialize(value)
                .map(|def| TaskEntry::Full(Box::new(def)))
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "a task is a command string, a list of steps, or a table, found {}",
                other.type_str()
            ))),
        }
    }
}

impl RunStep {
    /// The command text, for the two variants that have one.
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::Simple(cmd) => Some(cmd),
            Self::Command { cmd, .. } => Some(cmd),
            Self::Parallel { .. } | Self::Argv { .. } => None,
        }
    }

    /// The argv template, for the one step kind that has one.
    pub fn argv(&self) -> Option<&[String]> {
        match self {
            Self::Argv { argv, .. } => Some(argv),
            _ => None,
        }
    }

    /// Whether a non-zero exit from this step should be tolerated.
    pub fn ignore_error(&self) -> bool {
        matches!(
            self,
            Self::Command {
                ignore_error: true,
                ..
            } | Self::Argv {
                ignore_error: true,
                ..
            }
        )
    }
}

/// `run` accepts a single string or a list of steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RunSpec {
    One(String),
    Many(Vec<RunStep>),
}

impl RunSpec {
    /// Normalize both spellings to a step list.
    pub fn steps(&self) -> Vec<RunStep> {
        match self {
            Self::One(cmd) => vec![RunStep::Simple(cmd.clone())],
            Self::Many(steps) => steps.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::One(cmd) => cmd.trim().is_empty(),
            Self::Many(steps) => steps.is_empty(),
        }
    }
}

/// A task as written in config, before platform filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskEntry {
    /// `build = "cargo build"`, or `build = ["a", "b"]`.
    Bare(RunSpec),
    /// The full table form.
    Full(Box<TaskDef>),
}

/// A fully-specified task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskDef {
    /// Commands to run. Steps are sequential; any failure stops the task unless
    /// that step opted into `ignore_error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSpec>,
    /// Set when discovery found a script Windows cannot launch.
    ///
    /// Not a config field: it is a fact about the file, and letting a user
    /// write it would invite disagreement with what is actually on disk.
    #[serde(skip)]
    pub windows_invisible: bool,
    /// A script file to execute, the third tier.
    ///
    /// Relative to the config file. Use it when a `run` string has outgrown
    /// TOML -- past roughly twenty lines the editor stops highlighting it and
    /// the escaping starts to cost more than it saves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Embedded Lua, the fourth tier.
    ///
    /// A distinct field rather than a flavour of `run` so the tier is visible
    /// at a glance and a task that sets both can be rejected outright, instead
    /// of the runner guessing from the content which one was meant.
    ///
    /// Only for what the declarative tiers cannot say: conditionals, loops that
    /// generate work, cross-platform path arithmetic. Requires the `scripts`
    /// feature, which is on by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lua: Option<String>,
    /// Windows-only replacement for `run`.
    ///
    /// Replaces rather than appends: a task that needs different commands on
    /// Windows needs *those* commands, not both sets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_windows: Option<RunSpec>,
    /// Steps that run after `run`, **including when `run` failed**.
    ///
    /// Named after `run` rather than `depends` on purpose. mise calls the same
    /// idea `depends_post`, which reads as a kind of dependency and is not one:
    /// prerequisites run before and gate the task, these run after and cannot.
    /// Sitting in the `run` family also means it takes commands, so the common
    /// case -- one line of cleanup -- needs no separate task that nobody would
    /// ever invoke directly:
    ///
    /// ```toml
    /// run = "pytest tests/e2e"
    /// run_post = "docker compose down"
    /// ```
    ///
    /// Reusing [`RunSpec`] keeps `{ tasks = [...] }` available for the case
    /// where the teardown really is shared.
    ///
    /// "After" has three boundaries, and the last one is specific to osdk:
    /// it runs when `run` executed and failed; it is skipped when a dependency
    /// failed so `run` never started (nothing was set up, so nothing needs
    /// tearing down); and it is skipped when freshness deemed the task already
    /// up to date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_post: Option<RunSpec>,
    /// Help text shown by `osdk task list`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Alternate names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alias: Vec<String>,
    /// Prerequisites. They join the execution graph and run at most once each,
    /// in no guaranteed order relative to one another.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    /// Tasks to wait for **only if they are already in the plan**.
    ///
    /// The difference from `depends` is what happens when the named task is not
    /// otherwise scheduled: `depends` pulls it in and runs it, `wait_for` does
    /// nothing at all. It expresses "if we are both running, I go second"
    /// without making the other task a prerequisite -- useful when two tasks
    /// touch the same resource but neither actually needs the other's work.
    ///
    /// Naming a task that is not in the plan is therefore not an error; it is
    /// the condition the field exists to handle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_for: Vec<String>,
    /// Task-level environment variables.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory, relative to the config file that declared the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Interpreter override, e.g. `pwsh -Command`.
    ///
    /// Note that osdk does **not** inject `-NoProfile` here the way mise does:
    /// osdk's shims live on the persistent PATH but `JAVA_HOME`-style exports
    /// come from the profile hook, so suppressing the profile would silently
    /// drop them. The runner instead injects the task environment itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Hide from listings and completion.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide: bool,
    /// Suppress osdk's own progress output (not the task's).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// Platform filter, same vocabulary as `[tools]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<PlatformFilter>,
    /// Declared arguments. Empty means the task takes none.
    #[serde(default, flatten)]
    pub spec: crate::tasks::args::Spec,
    /// Wall-clock limit, e.g. `30s`, `5m`, `1h`; a bare number means seconds.
    ///
    /// On expiry osdk kills the task's **entire process tree**, not just the
    /// command it launched. Killing the direct child alone would be the wrong
    /// behaviour in the usual case: `cmd /c npm test` exits the moment cmd.exe
    /// dies while the `node` it started keeps holding the port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Input globs. When they are unchanged, the task is skipped.
    ///
    /// A pattern matching nothing is an error rather than a vacuous "fresh":
    /// see `freshness::resolve` for why that failure would otherwise be silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Output globs. Absent ones simply mean "not built yet", not a config error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// How to compare inputs: `mtime` (default), `hash`, or `always`.
    ///
    /// Offered as a choice because neither answer is right everywhere: mtime is
    /// cheap but a `git checkout` or CI cache restore rewrites it without
    /// changing content, while hashing reads every input. Task (go-task) ships
    /// the same three, and mise added a hash option after starting with mtime.
    #[serde(default, skip_serializing_if = "is_default_freshness")]
    pub freshness: crate::tasks::freshness::Freshness,
}

fn is_default_freshness(value: &crate::tasks::freshness::Freshness) -> bool {
    *value == crate::tasks::freshness::Freshness::default()
}

impl TaskEntry {
    /// Normalize either spelling into a [`TaskDef`].
    pub fn into_def(self) -> TaskDef {
        match self {
            Self::Bare(run) => TaskDef {
                run: Some(run),
                ..TaskDef::default()
            },
            Self::Full(def) => *def,
        }
    }
}

impl TaskDef {
    /// The step list that applies to this platform.
    pub fn steps_for(&self, windows: bool) -> Vec<RunStep> {
        let spec = if windows {
            self.run_windows.as_ref().or(self.run.as_ref())
        } else {
            self.run.as_ref()
        };
        spec.map(RunSpec::steps).unwrap_or_default()
    }

    /// The teardown steps, if any.
    pub fn post_steps(&self) -> Vec<RunStep> {
        self.run_post
            .as_ref()
            .map(RunSpec::steps)
            .unwrap_or_default()
    }

    /// Reject definitions that would fail confusingly at run time.
    fn validate(&self, name: &str) -> Result<()> {
        let has_run = self.run.as_ref().is_some_and(|r| !r.is_empty());
        let has_windows = self.run_windows.as_ref().is_some_and(|r| !r.is_empty());
        let has_lua = self
            .lua
            .as_ref()
            .is_some_and(|source| !source.trim().is_empty());
        let has_file = self
            .file
            .as_ref()
            .is_some_and(|path| !path.trim().is_empty());

        // A task belongs to exactly one tier. Naming the offending pair beats a
        // generic "conflicting fields": the writer usually pasted one in and
        // forgot to delete the other, and wants to be told which two.
        let declared: Vec<&str> = [
            (has_run || has_windows, "run"),
            (has_file, "file"),
            (has_lua, "lua"),
        ]
        .into_iter()
        .filter_map(|(present, label)| present.then_some(label))
        .collect();
        if declared.len() > 1 {
            return Err(Error::config(format!(
                "task `{name}`: sets `{}`; a task is written in one tier, and guessing which \
                 was meant would be worse than asking",
                declared.join("` and `")
            )));
        }

        if has_file {
            self.spec.validate(name)?;
            if let Some(text) = &self.timeout {
                if crate::tasks::tree::parse_duration(text).is_none() {
                    return Err(Error::config(format!(
                        "task `{name}`: `timeout = \"{text}\"` is not a duration"
                    )));
                }
            }
            return Ok(());
        }
        if has_lua {
            #[cfg(not(feature = "scripts"))]
            return Err(Error::config(format!(
                "task `{name}`: `lua` needs the `scripts` feature, which this build does not \
                 have; rewrite it with `run`, or use a build with scripting enabled"
            )));
            #[cfg(feature = "scripts")]
            {
                self.spec.validate(name)?;
                if let Some(text) = &self.timeout {
                    if crate::tasks::tree::parse_duration(text).is_none() {
                        return Err(Error::config(format!(
                            "task `{name}`: `timeout = \"{text}\"` is not a duration"
                        )));
                    }
                }
                return Ok(());
            }
        }
        if !has_run && !has_windows {
            return Err(Error::config(format!(
                "task `{name}`: needs `run`, `file`, or `lua`"
            )));
        }
        // A Windows-only task is legitimate, but pairing an empty `run` with a
        // populated `run_windows` would vanish on Unix without a word. Being
        // explicit costs one line and removes a silent no-op.
        if !has_run && has_windows && self.when.is_none() {
            return Err(Error::config(format!(
                "task `{name}`: has only `run_windows`; add `when = {{ os = \"windows\" }}` \
                 to say it is Windows-only, or give it a `run`"
            )));
        }
        for step in self
            .run
            .iter()
            .chain(self.run_windows.iter())
            .flat_map(RunSpec::steps)
        {
            if let Some(cmd) = step.command() {
                if cmd.trim().is_empty() {
                    return Err(Error::config(format!("task `{name}`: empty command")));
                }
            }
            if let RunStep::Parallel { tasks } = &step {
                if tasks.is_empty() {
                    return Err(Error::config(format!(
                        "task `{name}`: `{{ tasks = [] }}` step lists no tasks"
                    )));
                }
            }
            if let RunStep::Argv { argv, .. } = &step {
                if argv.is_empty() {
                    return Err(Error::config(format!(
                        "task `{name}`: `{{ argv = [] }}` names no program"
                    )));
                }
            }
        }
        for step in self.post_steps() {
            if let Some(cmd) = step.command() {
                if cmd.trim().is_empty() {
                    return Err(Error::config(format!(
                        "task `{name}`: empty command in `run_post`"
                    )));
                }
            }
            if let RunStep::Parallel { tasks } = &step {
                if tasks.is_empty() {
                    return Err(Error::config(format!(
                        "task `{name}`: `run_post` has a `{{ tasks = [] }}` step listing no tasks"
                    )));
                }
            }
            if let RunStep::Argv { argv, .. } = &step {
                if argv.is_empty() {
                    return Err(Error::config(format!(
                        "task `{name}`: `run_post` has an `{{ argv = [] }}` naming no program"
                    )));
                }
            }
        }
        self.spec.validate(name)?;
        if let Some(text) = &self.timeout {
            if crate::tasks::tree::parse_duration(text).is_none() {
                return Err(Error::config(format!(
                    "task `{name}`: `timeout = \"{text}\"` is not a duration; \
                     write it as `30s`, `5m`, `1h`, or a plain number of seconds"
                )));
            }
        }

        // Appending is decided by step count (see `runner::append_policy`), so a
        // task whose two platforms disagree would append on one and refuse on
        // the other. Catching it at load time beats a "works on my machine"
        // report from whichever platform the author did not test.
        if self.run.is_some() && self.run_windows.is_some() {
            let unix = self.steps_for(false).len();
            let windows = self.steps_for(true).len();
            let single = |count: usize| count == 1;
            if single(unix) != single(windows) {
                return Err(Error::config(format!(
                    "task `{name}`: `run` has {unix} step(s) but `run_windows` has {windows}; \
                     argument appending would differ by platform. Give both a single command, \
                     or declare arguments explicitly"
                )));
            }
        }
        Ok(())
    }
}

/// Runner-wide defaults, `[task_config]`.
///
/// Replaced as a unit by a higher-precedence layer, matching `[registries]` and
/// `[containers]`: a project that states its runner defaults means that set,
/// not that set merged into whatever the user's global config happened to hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// Sub-project directories whose tasks join this set, e.g.
    /// `roots = ["apps/*", "packages/*"]`.
    ///
    /// Nothing below the config root is discovered without an entry here. That is
    /// the whole point: the set of sub-projects whose tasks osdk will run has to be
    /// the set the project declared, not whatever a subtree walk turns up -- a
    /// `run` line is an arbitrary command, so finding one by accident is finding
    /// code to execute by accident.
    ///
    /// Shares `deps::expand_roots` with `[deps].roots`, so the guarantees are the
    /// same ones and not a second implementation of them: segment-by-segment
    /// descent, single-level `*` only, `..` rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    /// Default interpreter for every task in scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Default working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Directories searched for file tasks, replacing the defaults.
    ///
    /// Replacing rather than extending: a project that moves its scripts
    /// somewhere else almost never wants the default directories still
    /// searched, and silently keeping them would resurrect tasks the move was
    /// meant to retire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
}

/// The merged, platform-filtered task set.
#[derive(Debug, Clone, Default)]
pub struct TaskSet {
    /// Tasks that apply to this host, by name.
    pub tasks: BTreeMap<String, TaskDef>,
    /// Runner defaults.
    pub config: TaskConfig,
    /// Names dropped by their platform filter, mapped to the restriction.
    ///
    /// Kept for the same reason `[tools]` keeps `excluded_tools`: without it a
    /// filtered task looks simply unknown, sending the user to check their
    /// spelling rather than the `when` line that is doing its job.
    pub excluded: BTreeMap<String, String>,
    /// Alias -> canonical task name.
    pub aliases: BTreeMap<String, String>,
}

impl TaskSet {
    /// Merge one config layer's `[tasks]` on top of this set.
    ///
    /// Same-name tasks are replaced **whole**, not field-by-field. Field-level
    /// merging produces results nobody can predict from reading one file -- a
    /// project overriding only `description` would inherit the global `run` --
    /// whereas whole replacement says "this name is mine now".
    pub fn apply(&mut self, entries: BTreeMap<String, TaskEntry>) -> Result<()> {
        self.apply_from(entries, None)
    }

    /// Merge one layer, resolving `file` against the directory that declared it.
    ///
    /// The base is the declaring file's own directory, not the final merged
    /// root: with several configs in play those differ, and resolving against
    /// the root would make a global config's `file = "scripts/x.sh"` point into
    /// whichever project happened to be current.
    pub fn apply_from(
        &mut self,
        entries: BTreeMap<String, TaskEntry>,
        base: Option<&Path>,
    ) -> Result<()> {
        let host = Platform::current();
        for (name, entry) in entries {
            validate_name(&name)?;
            let mut def = entry.into_def();
            def.validate(&name)?;
            if let (Some(base), Some(file)) = (base, def.file.as_ref()) {
                let candidate = Path::new(file);
                if candidate.is_relative() {
                    def.file = Some(base.join(candidate).to_string_lossy().into_owned());
                }
            }

            let filter = def.when.clone().unwrap_or_default();
            if !filter.matches(&host) {
                // A lower layer may have declared this name without a filter;
                // this layer excluding it must not resurrect that one.
                self.tasks.remove(&name);
                self.aliases.retain(|_, target| target != &name);
                self.excluded.insert(name.clone(), filter.describe());
                continue;
            }
            self.excluded.remove(&name);
            self.aliases.retain(|_, target| target != &name);
            for alias in &def.alias {
                validate_name(alias)?;
                self.aliases.insert(alias.clone(), name.clone());
            }
            self.tasks.insert(name, def);
        }
        self.check_alias_collisions()?;
        Ok(())
    }

    /// Merge discovered script files into the set.
    ///
    /// Applied before `[tasks]` so a TOML entry of the same name wins: the
    /// explicit declaration is the more specific statement, and a file dropped
    /// into the directory should not silently shadow it.
    ///
    /// A task Windows cannot execute is still inserted. Dropping it here would
    /// make it read as a typo at the call site; keeping it lets the runner say
    /// why it cannot start, which is the actionable message.
    pub fn apply_files(&mut self, files: &BTreeMap<String, crate::tasks::files::FileTask>) {
        for (name, found) in files {
            if validate_name(name).is_err() {
                // A filename that cannot be a task name is not an error worth
                // failing the whole config over -- the directory may hold a
                // README or an editor backup.
                continue;
            }
            self.tasks.insert(
                name.clone(),
                TaskDef {
                    description: found.description.clone(),
                    depends: found.depends.clone(),
                    file: Some(found.path.to_string_lossy().into_owned()),
                    windows_invisible: !found.windows_visible,
                    ..TaskDef::default()
                },
            );
        }
    }

    /// Replace the runner defaults wholesale.
    pub fn apply_config(&mut self, config: TaskConfig) {
        self.config = config;
    }

    /// Resolve a name or alias to a canonical task name.
    ///
    /// Returns a borrow of the stored key rather than of `name`, so callers can
    /// hold the result while still using `self`.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        if let Some((key, _)) = self.tasks.get_key_value(name) {
            return Some(key.as_str());
        }
        let target = self.aliases.get(name)?;
        self.tasks
            .get_key_value(target.as_str())
            .map(|(k, _)| k.as_str())
    }

    /// Why `name` is absent, when it is absent because of a platform filter.
    pub fn exclusion_reason(&self, name: &str) -> Option<&str> {
        self.excluded.get(name).map(String::as_str)
    }

    fn check_alias_collisions(&self) -> Result<()> {
        for (alias, target) in &self.aliases {
            if self.tasks.contains_key(alias) {
                return Err(Error::config(format!(
                    "task alias `{alias}` (of `{target}`) collides with a task of the same name"
                )));
            }
        }
        Ok(())
    }

    /// Every name referenced by a task that does not resolve.
    ///
    /// Checked up front so a typo in `depends` fails before the first command
    /// runs, rather than halfway through a pipeline that already had effects.
    ///
    /// `wait_for` is deliberately excluded: naming a task that is not scheduled
    /// is its normal case, so requiring the name to resolve would reject the
    /// very usage it exists for. A misspelling there is silent, which is the
    /// price of the semantics -- `osdk task info` prints the field so it can be
    /// checked when something does not order the way it was meant to.
    pub fn unknown_references(&self) -> Vec<(String, String)> {
        let windows = cfg!(windows);
        let mut missing = Vec::new();
        for (name, def) in &self.tasks {
            let mut referenced: Vec<String> = def.depends.clone();
            for step in def.steps_for(windows).into_iter().chain(def.post_steps()) {
                if let RunStep::Parallel { tasks } = step {
                    referenced.extend(tasks);
                }
            }
            for reference in referenced {
                if self.resolve(&reference).is_none() {
                    missing.push((name.clone(), reference));
                }
            }
        }
        missing
    }

    /// Execution order for `root` and its prerequisites, dependencies first.
    ///
    /// Returns an error naming the cycle rather than looping forever or
    /// overflowing the stack.
    pub fn execution_order(&self, root: &str) -> Result<Vec<String>> {
        let Some(root) = self.resolve(root) else {
            return Err(Error::other(format!("unknown task `{root}`")));
        };
        let mut order = Vec::new();
        let mut done = BTreeSet::new();
        let mut path = Vec::new();
        self.visit(root, &mut order, &mut done, &mut path)?;
        Ok(order)
    }

    /// Reorder `order` so every `wait_for` target precedes the task naming it.
    ///
    /// Applied after the dependency sort rather than inside it, because the two
    /// answer different questions: `depends` decides *what* runs, `wait_for`
    /// only adjusts *when* among things already chosen. Folding it into the
    /// graph would turn an ordering hint into a prerequisite -- exactly the
    /// distinction the field exists to draw.
    ///
    /// A target that is not in `order` is skipped, which is the documented
    /// case. A cycle formed purely by `wait_for` is also skipped rather than
    /// reported: these are hints, and refusing to run because two hints
    /// disagree would be a worse outcome than running in the original order.
    pub fn apply_wait_for(&self, order: &mut Vec<String>) {
        // Bounded by the number of tasks: each pass can only move one entry
        // later, so `len` passes is enough to reach a fixed point or to
        // conclude the constraints are unsatisfiable.
        for _ in 0..order.len() {
            let mut moved = false;
            for index in 0..order.len() {
                let name = order[index].clone();
                let Some(def) = self.tasks.get(&name) else {
                    continue;
                };
                for target in &def.wait_for {
                    let Some(target) = self.resolve(target) else {
                        continue;
                    };
                    let Some(target_index) = order.iter().position(|entry| entry == target) else {
                        // Not scheduled: nothing to wait for.
                        continue;
                    };
                    if target_index <= index {
                        continue;
                    }
                    // The waiter currently runs first; move it after the target.
                    let waiter = order.remove(index);
                    order.insert(target_index, waiter);
                    moved = true;
                    break;
                }
                if moved {
                    break;
                }
            }
            if !moved {
                return;
            }
        }
    }

    fn visit(
        &self,
        name: &str,
        order: &mut Vec<String>,
        done: &mut BTreeSet<String>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if path.iter().any(|seen| seen == name) {
            path.push(name.to_string());
            return Err(Error::config(format!(
                "task dependency cycle: {}",
                path.join(" -> ")
            )));
        }
        path.push(name.to_string());

        let def = self
            .tasks
            .get(name)
            .ok_or_else(|| Error::other(format!("unknown task `{name}`")))?;
        for dependency in &def.depends {
            let resolved = self.resolve(dependency).ok_or_else(|| {
                Error::config(format!("task `{name}`: unknown dependency `{dependency}`"))
            })?;
            // `resolved` borrows self; copy before recursing.
            let resolved = resolved.to_string();
            self.visit(&resolved, order, done, path)?;
        }

        path.pop();
        done.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }
}

/// Reject names that would be ambiguous on the command line.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::config("task name must not be empty"));
    }
    if name.starts_with('-') {
        return Err(Error::config(format!(
            "task name `{name}` must not start with `-` (it would parse as a flag)"
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '$' | '&' | '|' | ';'))
    {
        return Err(Error::config(format!(
            "task name `{name}` must not contain `{bad}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> BTreeMap<String, TaskEntry> {
        toml::from_str(toml).expect("parse")
    }

    fn set_from(toml: &str) -> TaskSet {
        let mut set = TaskSet::default();
        set.apply(parse(toml)).expect("apply");
        set
    }

    #[test]
    fn bare_string_and_table_forms_are_equivalent() {
        let bare = set_from(r#"build = "cargo build""#);
        let table = set_from("[build]\nrun = \"cargo build\"\n");
        assert_eq!(
            bare.tasks["build"].steps_for(false),
            table.tasks["build"].steps_for(false)
        );
        assert_eq!(
            bare.tasks["build"].steps_for(false),
            vec![RunStep::Simple("cargo build".into())]
        );
    }

    #[test]
    fn run_array_accepts_plain_ignore_error_and_parallel_steps() {
        let set = set_from(
            r#"
[ci]
run = [
  "cargo fmt --check",
  { cmd = "cargo clippy", ignore_error = true },
  { tasks = ["test", "doc"] },
]
"#,
        );
        let steps = set.tasks["ci"].steps_for(false);
        assert_eq!(steps.len(), 3);
        assert!(!steps[0].ignore_error());
        assert!(steps[1].ignore_error());
        assert_eq!(steps[1].command(), Some("cargo clippy"));
        assert_eq!(
            steps[2],
            RunStep::Parallel {
                tasks: vec!["test".into(), "doc".into()]
            }
        );
        // A parallel step is not a command; the runner must not try to spawn it
        // as one.
        assert_eq!(steps[2].command(), None);
    }

    #[test]
    fn run_windows_replaces_run_on_windows_only() {
        let set = set_from(
            r#"
[build]
run = "make"
run_windows = "nmake"
"#,
        );
        let def = &set.tasks["build"];
        assert_eq!(def.steps_for(false), vec![RunStep::Simple("make".into())]);
        assert_eq!(def.steps_for(true), vec![RunStep::Simple("nmake".into())]);
    }

    #[test]
    fn same_name_replaces_whole_definition_across_layers() {
        let mut set = TaskSet::default();
        set.apply(parse(
            r#"
[build]
run = "global"
description = "from global"
"#,
        ))
        .unwrap();
        set.apply(parse("[build]\nrun = \"project\"\n")).unwrap();

        let def = &set.tasks["build"];
        assert_eq!(
            def.steps_for(false),
            vec![RunStep::Simple("project".into())]
        );
        // Whole replacement, not field merge: the global description must not
        // survive onto a definition the project rewrote.
        assert_eq!(def.description, None);
    }

    #[test]
    fn different_names_across_layers_union() {
        let mut set = TaskSet::default();
        set.apply(parse(r#"a = "one""#)).unwrap();
        set.apply(parse(r#"b = "two""#)).unwrap();
        assert_eq!(set.tasks.len(), 2);
    }

    #[test]
    fn execution_order_puts_dependencies_first_and_runs_shared_once() {
        let set = set_from(
            r#"
base = "echo base"

[left]
run = "echo left"
depends = ["base"]

[right]
run = "echo right"
depends = ["base"]

[top]
run = "echo top"
depends = ["left", "right"]
"#,
        );
        let order = set.execution_order("top").unwrap();
        assert_eq!(
            order.len(),
            4,
            "shared dependency must appear once: {order:?}"
        );
        let position = |name: &str| order.iter().position(|n| n == name).unwrap();
        assert!(position("base") < position("left"));
        assert!(position("base") < position("right"));
        assert!(position("left") < position("top"));
        assert!(position("right") < position("top"));
    }

    #[test]
    fn dependency_cycle_is_reported_with_the_path() {
        let set = set_from(
            r#"
[a]
run = "echo a"
depends = ["b"]

[b]
run = "echo b"
depends = ["a"]
"#,
        );
        let error = set.execution_order("a").unwrap_err().to_string();
        assert!(error.contains("cycle"), "{error}");
        assert!(error.contains("a -> b -> a"), "{error}");
    }

    #[test]
    fn self_dependency_is_a_cycle_not_a_hang() {
        let set = set_from("[a]\nrun = \"echo a\"\ndepends = [\"a\"]\n");
        let error = set.execution_order("a").unwrap_err().to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn unknown_dependency_is_caught_before_running_anything() {
        let set = set_from("[a]\nrun = \"echo a\"\ndepends = [\"nope\"]\n");
        assert_eq!(
            set.unknown_references(),
            vec![("a".to_string(), "nope".to_string())]
        );
        assert!(set.execution_order("a").is_err());
    }

    #[test]
    fn parallel_step_references_are_checked_too() {
        let set = set_from(
            r#"[a]
run = [{ tasks = ["ghost"] }]
"#,
        );
        assert_eq!(
            set.unknown_references(),
            vec![("a".to_string(), "ghost".to_string())]
        );
    }

    #[test]
    fn aliases_resolve_and_collisions_are_rejected() {
        let set = set_from("[build]\nrun = \"cargo build\"\nalias = [\"b\"]\n");
        assert_eq!(set.resolve("b"), Some("build"));
        assert_eq!(set.resolve("missing"), None);

        let mut collide = TaskSet::default();
        let error = collide
            .apply(parse(
                r#"
[build]
run = "x"
alias = ["test"]

[test]
run = "y"
"#,
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("collides"), "{error}");
    }

    #[test]
    fn task_without_any_run_is_rejected() {
        let mut set = TaskSet::default();
        let error = set
            .apply(parse("[a]\ndescription = \"nothing\"\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs `run`"), "{error}");
    }

    #[test]
    fn windows_only_run_requires_an_explicit_filter() {
        let mut set = TaskSet::default();
        let error = set
            .apply(parse("[a]\nrun_windows = \"nmake\"\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Windows-only"), "{error}");

        // With the filter spelled out it is accepted.
        let mut ok = TaskSet::default();
        ok.apply(parse(
            "[a]\nrun_windows = \"nmake\"\nwhen = { os = \"windows\" }\n",
        ))
        .expect("explicit filter accepted");
    }

    #[test]
    fn unknown_field_is_rejected_rather_than_ignored() {
        // A misspelled key must not be silently dropped: `dependson` would
        // otherwise mean "no prerequisites" and the task would run too early.
        let error = toml::from_str::<BTreeMap<String, TaskEntry>>(
            "[a]\nrun = \"x\"\ndependson = [\"b\"]\n",
        )
        .unwrap_err()
        .to_string();
        // The message must name the offending key. An untagged enum would say
        // only "data did not match any variant", which points nowhere.
        assert!(error.contains("dependson"), "unhelpful message: {error}");
    }

    #[test]
    fn names_that_would_be_ambiguous_on_the_command_line_are_rejected() {
        for bad in ["-x", "a b", "a&b", "a;b", "a|b"] {
            let mut set = TaskSet::default();
            let mut entries = BTreeMap::new();
            entries.insert(bad.to_string(), TaskEntry::Bare(RunSpec::One("x".into())));
            assert!(set.apply(entries).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn empty_command_and_empty_parallel_list_are_rejected() {
        let mut set = TaskSet::default();
        assert!(set.apply(parse("a = \"   \"\n")).is_err());

        let mut set = TaskSet::default();
        let error = set
            .apply(parse("[a]\nrun = [{ tasks = [] }]\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("lists no tasks"), "{error}");
    }

    #[test]
    fn task_config_is_replaced_as_a_unit() {
        let mut set = TaskSet::default();
        set.apply_config(TaskConfig {
            shell: Some("sh -c".into()),
            dir: Some("/global".into()),
            ..Default::default()
        });
        set.apply_config(TaskConfig {
            shell: Some("pwsh -Command".into()),
            dir: None,
            ..Default::default()
        });
        assert_eq!(set.config.shell.as_deref(), Some("pwsh -Command"));
        // Unit replacement: the previous `dir` must not survive.
        assert_eq!(set.config.dir, None);
    }
}
