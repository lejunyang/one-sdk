//! Task execution: dependency order, parallel steps, and the task environment.
//!
//! Three decisions here are worth stating, because each replaces something that
//! looks simpler but is wrong:
//!
//! - **osdk injects the task environment itself; it does not pass `-NoProfile`.**
//!   mise suppresses the PowerShell profile so a stale activation snippet cannot
//!   shadow the task's own tools. That reasoning does not transfer: osdk's shims
//!   live on the persistent PATH, but `JAVA_HOME`/`GOROOT`-style exports come
//!   from the `hook-env` profile hook, so suppressing the profile would silently
//!   drop them and leave tools excluded by `ShimSettings` unreachable. Instead
//!   the runner computes the same delta `hook-env` would and hands it to the
//!   child, then sets [`TASK_MARKER`] so a profile hook that does run knows to
//!   stand down rather than layer a second activation on top.
//! - **Steps never rely on `&`.** Its meaning is not portable (see `tasks`), so
//!   "keep going" and "run these together" are explicit step kinds instead.
//! - **The whole graph is validated before the first command runs.** A typo in
//!   `depends` should not surface halfway through a pipeline that has already
//!   written files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::tasks::{RunStep, TaskDef, TaskSet};

/// Set for every task process so an `osdk activate` hook can skip itself.
///
/// Without this the profile hook would recompute and re-prepend an activation on
/// top of the one the runner just injected -- harmless in the common case, but
/// it would let an outer stale snippet win over the task's own declaration.
pub const TASK_MARKER: &str = "OSDK_TASK";

/// Names the task's own identity, for scripts that want to know.
pub const TASK_NAME_VAR: &str = "OSDK_TASK_NAME";

/// What the runner decided to do, without doing it.
///
/// `osdk run --dry-run` renders this, and the tests assert on it: checking the
/// plan is how the ordering and shell-selection rules get verified without
/// spawning a process per case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Tasks in execution order, dependencies first.
    pub steps: Vec<PlannedTask>,
}

/// One task's resolved commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    pub name: String,
    pub dir: Option<String>,
    pub commands: Vec<PlannedStep>,
    /// Wall-clock limit for each command in this task.
    pub timeout: Option<std::time::Duration>,
    /// Teardown steps, run after `commands` even when they failed.
    pub post: Vec<PlannedStep>,
    /// Input globs, for the freshness check.
    pub sources: Vec<String>,
    /// Output globs, for the freshness check.
    pub outputs: Vec<String>,
    /// How to compare inputs.
    pub freshness: crate::tasks::freshness::Freshness,
    /// Serialized definition, so editing a command invalidates a cached result.
    pub definition: String,
    /// Whether the top-level walk executes this task in its own right.
    ///
    /// A task reached only through a `{ tasks = [...] }` step still has to be
    /// *planned* -- the step looks its commands up here -- but it must not also
    /// run as an ordinary prerequisite, or it executes twice. Prerequisites and
    /// the root itself are `true`; entries present purely to be referenced are
    /// `false`.
    pub standalone: bool,
}

/// A single resolved step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedStep {
    /// Run one command through the shell.
    Command {
        shell: Vec<String>,
        command: String,
        ignore_error: bool,
    },
    /// Run these tasks concurrently, then continue.
    Parallel { tasks: Vec<String> },
    /// Exec a program directly, no shell involved.
    Argv {
        argv: Vec<String>,
        ignore_error: bool,
    },
}

/// The interpreter used when neither the task nor `[task_config]` names one.
///
/// `cmd /c` on Windows rather than `sh -c`: requiring `sh` would mean requiring
/// Git for Windows or Cygwin, which contradicts osdk's "installs and works"
/// premise. Note that a developer machine may well *have* `sh` on PATH -- it can
/// even be one of osdk's own shims -- which is exactly why picking it as the
/// default is a trap: it would work here and fail on a clean machine.
pub fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into()]
    } else {
        vec!["sh".into(), "-c".into()]
    }
}

/// Split a `shell` setting like `pwsh -Command` into program plus arguments.
///
/// Deliberately whitespace-splitting rather than shell-parsing: a shell setting
/// with quoting in it is a sign the user wants a script file, not a one-liner,
/// and pretending to parse quotes here would only fail in subtler ways.
fn parse_shell(spec: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = spec.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        return Err(Error::config("`shell` must name an interpreter"));
    }
    Ok(parts)
}

/// Resolve the interpreter for one task.
fn shell_for(def: &TaskDef, set: &TaskSet) -> Result<Vec<String>> {
    if let Some(spec) = def.shell.as_deref().or(set.config.shell.as_deref()) {
        return parse_shell(spec);
    }
    Ok(default_shell())
}

/// Build the execution plan for `root`.
///
/// Validation happens here, before any process starts: unknown references and
/// dependency cycles are reported as errors rather than discovered mid-run.
pub fn plan(set: &TaskSet, root: &str) -> Result<Plan> {
    if set.resolve(root).is_none() {
        // A task filtered out by `when` is not "unknown" -- saying so would send
        // the reader to check their spelling instead of the filter.
        if let Some(reason) = set.exclusion_reason(root) {
            return Err(Error::other(format!(
                "task `{root}` is not available on this platform ({reason})"
            )));
        }
        return Err(Error::other(format!("unknown task `{root}`")));
    }

    let missing = set.unknown_references();
    if let Some((task, reference)) = missing.first() {
        return Err(Error::config(format!(
            "task `{task}` references unknown task `{reference}`"
        )));
    }

    let windows = cfg!(windows);
    let mut order = set.execution_order(root)?;

    // Tasks named by a `{ tasks = [...] }` step are not prerequisites, so
    // `execution_order` does not include them -- it only follows `depends`.
    // They still have to be planned: `execute` looks their commands up in the
    // plan, and a lookup that misses would skip the whole parallel step without
    // a word, reporting success. Their own dependencies come along too.
    let standalone: std::collections::BTreeSet<String> = order.iter().cloned().collect();
    let mut pending: Vec<String> = order.clone();
    while let Some(name) = pending.pop() {
        let Some(def) = set.tasks.get(&name) else {
            continue;
        };
        // `run_post` can reference tasks too, and a reference the plan does not
        // contain gets silently skipped at execution time -- the same failure
        // mode parallel steps had. Both lists feed the walk.
        for step in def.steps_for(windows).into_iter().chain(def.post_steps()) {
            let RunStep::Parallel { tasks } = step else {
                continue;
            };
            for referenced in tasks {
                let Some(target) = set.resolve(&referenced) else {
                    continue;
                };
                if order.iter().any(|planned| planned == target) {
                    continue;
                }
                let target = target.to_string();
                for dependency in set.execution_order(&target)? {
                    if !order.iter().any(|planned| planned == &dependency) {
                        order.push(dependency.clone());
                        pending.push(dependency);
                    }
                }
            }
        }
    }

    // Ordering hints apply once membership is settled: `wait_for` can only
    // reorder what `depends` already pulled in.
    set.apply_wait_for(&mut order);

    let mut steps = Vec::new();
    for name in order {
        let def = set
            .tasks
            .get(&name)
            .ok_or_else(|| Error::other(format!("unknown task `{name}`")))?;
        let shell = shell_for(def, set)?;
        let mut commands = Vec::new();
        for step in def.steps_for(windows) {
            match step {
                RunStep::Parallel { tasks } => commands.push(PlannedStep::Parallel { tasks }),
                RunStep::Argv { argv, ignore_error } => {
                    commands.push(PlannedStep::Argv { argv, ignore_error })
                }
                other => {
                    let command = other
                        .command()
                        .expect("non-parallel step always carries a command")
                        .to_string();
                    commands.push(PlannedStep::Command {
                        shell: shell.clone(),
                        command,
                        ignore_error: other.ignore_error(),
                    });
                }
            }
        }
        let mut post = Vec::new();
        for step in def.post_steps() {
            match step {
                RunStep::Parallel { tasks } => post.push(PlannedStep::Parallel { tasks }),
                RunStep::Argv { argv, ignore_error } => {
                    post.push(PlannedStep::Argv { argv, ignore_error })
                }
                other => post.push(PlannedStep::Command {
                    shell: shell.clone(),
                    command: other
                        .command()
                        .expect("non-parallel step always carries a command")
                        .to_string(),
                    ignore_error: other.ignore_error(),
                }),
            }
        }

        steps.push(PlannedTask {
            timeout: def
                .timeout
                .as_deref()
                .and_then(crate::tasks::tree::parse_duration),
            post,
            standalone: standalone.contains(&name),
            sources: def.sources.clone(),
            outputs: def.outputs.clone(),
            freshness: def.freshness,
            // Serializing the whole definition is what makes "I edited the
            // command" count as a change; comparing only input files would keep
            // serving a stale result after the task itself was rewritten.
            definition: toml::to_string(def).unwrap_or_default(),
            name,
            dir: def.dir.clone().or_else(|| set.config.dir.clone()),
            commands,
        });
    }
    Ok(Plan { steps })
}

/// Environment handed to every task process.
#[derive(Debug, Clone, Default)]
pub struct TaskEnv {
    /// Directories prepended to PATH, highest priority first.
    pub path_prepend: Vec<PathBuf>,
    /// Variables to set (`GOROOT`, `JAVA_HOME`, ...).
    pub set_vars: BTreeMap<String, String>,
}

impl TaskEnv {
    /// Apply this environment plus the task's own vars to a command.
    ///
    /// Order matters: the activation delta goes on first, the task's `env` last,
    /// so a task can override an exported variable when it means to.
    pub fn apply(&self, command: &mut Command, task: &TaskDef, name: &str, current_path: &str) {
        for (key, value) in &self.set_vars {
            command.env(key, value);
        }
        if !self.path_prepend.is_empty() {
            let mut entries: Vec<PathBuf> = self.path_prepend.clone();
            entries.extend(std::env::split_paths(current_path));
            if let Ok(joined) = std::env::join_paths(entries) {
                command.env("PATH", joined);
            }
        }
        for (key, value) in &task.env {
            command.env(key, value);
        }
        command.env(TASK_MARKER, "1");
        command.env(TASK_NAME_VAR, name);
    }
}

/// Resolve a task's working directory against the config root.
pub fn resolve_dir(config_root: &Path, dir: Option<&str>) -> PathBuf {
    match dir {
        Some(dir) => {
            let candidate = Path::new(dir);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                config_root.join(candidate)
            }
        }
        None => config_root.to_path_buf(),
    }
}

/// Outcome of running one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutcome {
    pub name: String,
    /// Exit code of the step that decided the outcome, 0 when every step passed.
    pub code: i32,
    /// Steps whose failure was tolerated because of `ignore_error`.
    pub tolerated_failures: Vec<String>,
    /// Whether freshness let this task be skipped entirely.
    pub skipped: bool,
}

/// How to launch tasks. Split out so tests can observe without spawning.
pub trait Spawner {
    /// Run one command, returning its exit code.
    fn run(&mut self, task: &str, shell: &[String], command: &str, dir: &Path) -> Result<i32>;

    /// Exec a program directly, bypassing the shell.
    fn run_argv(&mut self, task: &str, argv: &[String], dir: &Path) -> Result<i32>;

    /// Set the wall-clock limit applying to subsequent calls.
    ///
    /// Passed out of band rather than as a parameter on every call so the
    /// trait stays small; the runner sets it once per task.
    fn set_timeout(&mut self, _limit: Option<std::time::Duration>) {}
}

/// Runs commands as real child processes.
pub struct ProcessSpawner {
    /// Environment computed the way `hook-env` would, injected into every child.
    pub env: TaskEnv,
    /// Per-task definitions, needed for their own `env` blocks.
    pub defs: BTreeMap<String, TaskDef>,
    /// PATH as inherited by osdk itself.
    pub base_path: String,
    /// Wall-clock limit for the task currently running.
    pub timeout: Option<std::time::Duration>,
    /// Argument values exposed as `osdk_arg_*`.
    ///
    /// Safe on every shell: these enter the child's environment block verbatim,
    /// never through a parser -- exactly the guarantee interpolating into a
    /// `cmd` string cannot make.
    pub arg_env: BTreeMap<String, String>,
}

impl Spawner for ProcessSpawner {
    fn set_timeout(&mut self, limit: Option<std::time::Duration>) {
        self.timeout = limit;
    }

    fn run(&mut self, task: &str, shell: &[String], command: &str, dir: &Path) -> Result<i32> {
        let (program, args) = shell
            .split_first()
            .ok_or_else(|| Error::config("`shell` must name an interpreter"))?;
        let mut child = Command::new(program);
        child.args(args).arg(command).current_dir(dir);
        if let Some(def) = self.defs.get(task) {
            self.env.apply(&mut child, def, task, &self.base_path);
        }
        for (key, value) in &self.arg_env {
            child.env(key, value);
        }
        self.run_to_completion(task, program, child)
    }

    fn run_argv(&mut self, task: &str, argv: &[String], dir: &Path) -> Result<i32> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| Error::config(format!("task `{task}`: `argv` names no program")))?;
        let mut child = Command::new(program);
        child.args(rest).current_dir(dir);
        if let Some(def) = self.defs.get(task) {
            self.env.apply(&mut child, def, task, &self.base_path);
        }
        for (key, value) in &self.arg_env {
            child.env(key, value);
        }
        self.run_to_completion(task, program, child)
    }
}

impl ProcessSpawner {
    /// Spawn inside a killable process group and wait, honouring the timeout.
    ///
    /// The grouping is established at spawn time even when no timeout is set:
    /// after a process has forked there is no reliable way to find what it
    /// started, so the decision cannot be deferred to the moment it is needed.
    fn run_to_completion(&self, task: &str, program: &str, mut command: Command) -> Result<i32> {
        use crate::tasks::tree;

        let (mut child, mut handle) = tree::spawn_in_tree(&mut command).map_err(|error| {
            Error::other(format!("task `{task}`: cannot run `{program}`: {error}"))
        })?;

        match tree::wait_with_timeout(&mut child, &mut handle, self.timeout) {
            // A signal-killed child reports no code; treat it as failure rather
            // than silently succeeding.
            Ok(Some(code)) => Ok(code),
            Ok(None) => Err(Error::other(format!(
                "task `{task}`: timed out after {}; the process tree was terminated",
                self.timeout
                    .map(|limit| format!("{}s", limit.as_secs()))
                    .unwrap_or_else(|| "the configured limit".into())
            ))),
            Err(error) => Err(Error::other(format!(
                "task `{task}`: waiting for `{program}` failed: {error}"
            ))),
        }
    }
}

/// Whether leftover command-line arguments can be appended to a task.
///
/// Keyed on **how many command steps the task has**, not on whether `run` was
/// written as a string or a list. `run = "x"` and `run = ["x"]` are the same
/// task expressed two ways, so rewriting one into the other must not change
/// whether arguments are accepted -- that is the "upgrading a tier never
/// rewrites meaning" rule the whole layering rests on.
///
/// With several steps there is no defensible answer to "append where": the last
/// command? every command? what about a `{ tasks = [...] }` step? npm can append
/// because a script is always exactly one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendPolicy {
    /// Exactly one command step: leftover arguments go to it.
    Append,
    /// Anything else: leftover arguments are an error that says how to fix it.
    Refuse,
}

/// Decide the append policy for a planned task.
pub fn append_policy(task: &PlannedTask) -> AppendPolicy {
    let mut commands = 0;
    let mut parallel = false;
    for step in &task.commands {
        match step {
            PlannedStep::Parallel { .. } => parallel = true,
            _ => commands += 1,
        }
    }
    if commands == 1 && !parallel {
        AppendPolicy::Append
    } else {
        AppendPolicy::Refuse
    }
}

/// A task the runner chose not to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
}

/// Execute a plan, stopping at the first intolerable failure.
///
/// Parallel steps are executed by running their tasks in sequence here; the
/// concurrency itself is the caller's to add once it owns a thread pool. What
/// matters at this layer is that the *semantics* are already right: the step
/// waits for all of its tasks and surfaces the first failure, which is exactly
/// what a shell `&` cannot do.
pub fn execute(
    plan: &Plan,
    config_root: &Path,
    spawner: &mut dyn Spawner,
) -> Result<Vec<TaskOutcome>> {
    execute_with_freshness(plan, config_root, spawner, &mut None)
}

/// Run a task's teardown steps, returning the first non-zero exit code.
///
/// Called on both the success and failure paths, which is the point: teardown
/// that only ran on success would be useless for the case it exists to serve.
/// It is *not* called when the task never started or was skipped as fresh --
/// nothing was set up, so there is nothing to tear down.
fn run_post_steps(
    task: &PlannedTask,
    plan: &Plan,
    config_root: &Path,
    spawner: &mut dyn Spawner,
    outcome: &mut TaskOutcome,
) -> Result<i32> {
    if task.post.is_empty() {
        return Ok(0);
    }
    let dir = resolve_dir(config_root, task.dir.as_deref());
    let mut first_failure = 0;

    for step in &task.post {
        let code = match step {
            PlannedStep::Command {
                shell,
                command,
                ignore_error,
            } => {
                let code = spawner.run(&task.name, shell, command, &dir)?;
                if code != 0 && *ignore_error {
                    outcome.tolerated_failures.push(command.clone());
                    continue;
                }
                code
            }
            PlannedStep::Argv { argv, ignore_error } => {
                let code = spawner.run_argv(&task.name, argv, &dir)?;
                if code != 0 && *ignore_error {
                    outcome.tolerated_failures.push(argv.join(" "));
                    continue;
                }
                code
            }
            PlannedStep::Parallel { tasks } => {
                let mut worst = 0;
                for name in tasks {
                    let Some(sub) = plan.steps.iter().find(|candidate| &candidate.name == name)
                    else {
                        continue;
                    };
                    let sub_dir = resolve_dir(config_root, sub.dir.as_deref());
                    for sub_step in &sub.commands {
                        let code = match sub_step {
                            PlannedStep::Command {
                                shell,
                                command,
                                ignore_error,
                            } => {
                                let code = spawner.run(&sub.name, shell, command, &sub_dir)?;
                                if code != 0 && *ignore_error {
                                    continue;
                                }
                                code
                            }
                            PlannedStep::Argv { argv, ignore_error } => {
                                let code = spawner.run_argv(&sub.name, argv, &sub_dir)?;
                                if code != 0 && *ignore_error {
                                    continue;
                                }
                                code
                            }
                            PlannedStep::Parallel { .. } => continue,
                        };
                        if code != 0 && worst == 0 {
                            worst = code;
                        }
                    }
                }
                worst
            }
        };
        // Every teardown step gets its turn even after one fails: stopping
        // halfway would leave exactly the resources this is meant to release.
        if code != 0 && first_failure == 0 {
            first_failure = code;
        }
    }
    Ok(first_failure)
}

/// Resolve one task's steps against parsed argument values.
///
/// Done here rather than during planning so `--dry-run` and the real run share
/// one implementation; a second copy would drift and the preview would stop
/// matching what executes.
fn apply_arguments(
    task: &PlannedTask,
    values: &crate::tasks::args::Values,
    has_spec: bool,
) -> Result<Vec<PlannedStep>> {
    use crate::tasks::args;

    // A template that names `{{args}}` has already said where leftovers go.
    let consumes_rest = task.commands.iter().any(|step| match step {
        PlannedStep::Argv { argv, .. } => argv
            .iter()
            .any(|element| element.trim() == args::REST_PLACEHOLDER),
        _ => false,
    });

    let mut resolved = Vec::with_capacity(task.commands.len());
    for step in &task.commands {
        match step {
            PlannedStep::Argv { argv, ignore_error } => resolved.push(PlannedStep::Argv {
                argv: args::expand_argv(&task.name, argv, values)?,
                ignore_error: *ignore_error,
            }),
            other => resolved.push(other.clone()),
        }
    }

    if values.rest.is_empty() || consumes_rest {
        return Ok(resolved);
    }

    match append_policy(task) {
        AppendPolicy::Append => {
            for step in resolved.iter_mut() {
                match step {
                    // An argv step keeps each leftover as its own entry, so
                    // spaces and metacharacters survive intact.
                    PlannedStep::Argv { argv, .. } => {
                        argv.extend(values.rest.iter().cloned());
                        return Ok(resolved);
                    }
                    // A shell string can only be appended textually. On `sh`
                    // and `pwsh` that is quotable; on `cmd` it is not fully
                    // solvable, which is documented and is why `argv` exists.
                    PlannedStep::Command { command, .. } => {
                        command.push(' ');
                        command.push_str(&values.rest.join(" "));
                        return Ok(resolved);
                    }
                    PlannedStep::Parallel { .. } => continue,
                }
            }
            Ok(resolved)
        }
        AppendPolicy::Refuse => Err(Error::other(if has_spec {
            format!(
                "task `{name}` got {count} argument(s) its declaration does not cover; \
                 add `{{{{args}}}}` to an argv step to receive them",
                name = task.name,
                count = values.rest.len()
            )
        } else {
            format!(
                "task `{name}` does not accept arguments: it has several steps, so there is no \
                 single place to append them. Add `{{{{args}}}}` to an argv step, or declare \
                 them with [[tasks.{name}.args]]",
                name = task.name
            )
        })),
    }
}

/// Execute a plan, consulting and updating freshness state when one is given.
///
/// Freshness is applied per task rather than to the plan as a whole: a fresh
/// dependency is skipped while its dependent still runs, which is the behaviour
/// make has and the reason `sources` is useful at all.
pub fn execute_with_freshness(
    plan: &Plan,
    config_root: &Path,
    spawner: &mut dyn Spawner,
    state: &mut Option<&mut crate::tasks::freshness::FreshnessState>,
) -> Result<Vec<TaskOutcome>> {
    let values = crate::tasks::args::Values::default();
    execute_full(plan, config_root, spawner, state, &values, false)
}

/// A copy of `plan` with argument substitution already applied.
///
/// Exists so `--dry-run` renders the same strings `execute_full` would run,
/// using the same `apply_arguments`. Rendering the raw template instead would
/// make the preview quietly disagree with reality, which defeats the point of
/// previewing.
pub fn resolve_for_preview(
    plan: &Plan,
    root: &str,
    values: &crate::tasks::args::Values,
    has_spec: bool,
) -> Result<Plan> {
    let mut steps = Vec::with_capacity(plan.steps.len());
    for task in &plan.steps {
        let mut task = task.clone();
        if task.name == root {
            task.commands = apply_arguments(&task, values, has_spec)?;
        }
        steps.push(task);
    }
    Ok(Plan { steps })
}

/// Execute a plan with argument values and freshness state.
pub fn execute_full(
    plan: &Plan,
    config_root: &Path,
    spawner: &mut dyn Spawner,
    state: &mut Option<&mut crate::tasks::freshness::FreshnessState>,
    values: &crate::tasks::args::Values,
    has_spec: bool,
) -> Result<Vec<TaskOutcome>> {
    use crate::tasks::freshness;

    // Arguments belong to the task the user named, which is the last standalone
    // entry in the plan. A value meant for `deploy` has no business reaching the
    // `build` it depends on.
    let root = plan
        .steps
        .iter()
        .rfind(|step| step.standalone)
        .map(|step| step.name.clone());

    let mut outcomes = Vec::new();
    for task in &plan.steps {
        // Present only so a parallel step can find its commands; running it here
        // as well would execute it twice.
        if !task.standalone {
            continue;
        }

        if let Some(state) = state.as_deref() {
            let inputs = freshness::Inputs {
                root: config_root,
                name: &task.name,
                sources: &task.sources,
                outputs: &task.outputs,
                freshness: task.freshness,
                definition: &task.definition,
            };
            if !freshness::decide(&inputs, state)?.should_run() {
                outcomes.push(TaskOutcome {
                    name: task.name.clone(),
                    code: 0,
                    tolerated_failures: Vec::new(),
                    skipped: true,
                });
                continue;
            }
        }

        spawner.set_timeout(task.timeout);
        let dir = resolve_dir(config_root, task.dir.as_deref());
        let steps = if root.as_deref() == Some(task.name.as_str()) {
            apply_arguments(task, values, has_spec)?
        } else {
            task.commands.clone()
        };
        let mut outcome = TaskOutcome {
            name: task.name.clone(),
            code: 0,
            tolerated_failures: Vec::new(),
            skipped: false,
        };
        for step in &steps {
            match step {
                PlannedStep::Argv { argv, ignore_error } => {
                    let code = match spawner.run_argv(&task.name, argv, &dir) {
                        Ok(code) => code,
                        Err(error) => {
                            let _ = run_post_steps(task, plan, config_root, spawner, &mut outcome);
                            return Err(error);
                        }
                    };
                    if code != 0 {
                        if *ignore_error {
                            outcome.tolerated_failures.push(argv.join(" "));
                            continue;
                        }
                        // The task body failed, but it *did* start, so teardown
                        // owes its work -- that is the whole reason `run_post`
                        // exists rather than a last line of `run`.
                        run_post_steps(task, plan, config_root, spawner, &mut outcome)?;
                        outcome.code = code;
                        outcomes.push(outcome);
                        return Ok(outcomes);
                    }
                }
                PlannedStep::Command {
                    shell,
                    command,
                    ignore_error,
                } => {
                    let code = match spawner.run(&task.name, shell, command, &dir) {
                        Ok(code) => code,
                        // A timeout arrives as an error, and the task did start
                        // -- so teardown is owed exactly as much as after an
                        // ordinary failure. Propagating straight out would skip
                        // the cleanup precisely when the runaway left the most
                        // behind.
                        Err(error) => {
                            let _ = run_post_steps(task, plan, config_root, spawner, &mut outcome);
                            return Err(error);
                        }
                    };
                    if code != 0 {
                        if *ignore_error {
                            outcome.tolerated_failures.push(command.clone());
                            continue;
                        }
                        run_post_steps(task, plan, config_root, spawner, &mut outcome)?;
                        outcome.code = code;
                        outcomes.push(outcome);
                        return Ok(outcomes);
                    }
                }
                PlannedStep::Parallel { tasks } => {
                    // Each referenced task has already been planned; run its own
                    // commands and collect the first failure.
                    for name in tasks {
                        let Some(sub) = plan.steps.iter().find(|t| &t.name == name) else {
                            continue;
                        };
                        let sub_dir = resolve_dir(config_root, sub.dir.as_deref());
                        for sub_step in &sub.commands {
                            let code = match sub_step {
                                PlannedStep::Command {
                                    shell,
                                    command,
                                    ignore_error,
                                } => {
                                    let code = spawner.run(&sub.name, shell, command, &sub_dir)?;
                                    if code != 0 && *ignore_error {
                                        continue;
                                    }
                                    code
                                }
                                PlannedStep::Argv { argv, ignore_error } => {
                                    let code = spawner.run_argv(&sub.name, argv, &sub_dir)?;
                                    if code != 0 && *ignore_error {
                                        continue;
                                    }
                                    code
                                }
                                PlannedStep::Parallel { .. } => continue,
                            };
                            if code != 0 {
                                run_post_steps(task, plan, config_root, spawner, &mut outcome)?;
                                outcome.code = code;
                                outcomes.push(outcome);
                                return Ok(outcomes);
                            }
                        }
                    }
                }
            }
        }
        // The body succeeded; teardown still runs, and its failure becomes the
        // task's failure -- a clean test run whose cleanup broke has not left
        // the machine in the state it promised.
        let post_code = run_post_steps(task, plan, config_root, spawner, &mut outcome)?;
        if post_code != 0 {
            outcome.code = post_code;
            outcomes.push(outcome);
            return Ok(outcomes);
        }

        // Only a clean run updates the record: storing state after a failure
        // would let the next invocation skip a task that never succeeded.
        if outcome.code == 0 {
            if let Some(state) = state.as_deref_mut() {
                let inputs = freshness::Inputs {
                    root: config_root,
                    name: &task.name,
                    sources: &task.sources,
                    outputs: &task.outputs,
                    freshness: task.freshness,
                    definition: &task.definition,
                };
                freshness::record(&inputs, state)?;
            }
        }
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskEntry;

    fn set_from(toml: &str) -> TaskSet {
        let entries: BTreeMap<String, TaskEntry> = toml::from_str(toml).expect("parse");
        let mut set = TaskSet::default();
        set.apply(entries).expect("apply");
        set
    }

    fn commands_of(plan: &Plan, name: &str) -> Vec<String> {
        plan.steps
            .iter()
            .find(|task| task.name == name)
            .expect("task in plan")
            .commands
            .iter()
            .filter_map(|step| match step {
                PlannedStep::Command { command, .. } => Some(command.clone()),
                PlannedStep::Argv { argv, .. } => Some(argv.join(" ")),
                PlannedStep::Parallel { .. } => None,
            })
            .collect()
    }

    /// Records what would have run, so execution semantics can be asserted
    /// without spawning a single process.
    #[derive(Default)]
    struct RecordingSpawner {
        ran: Vec<String>,
        /// Commands that should report failure, and with what code.
        fail: BTreeMap<String, i32>,
        /// Commands that should fail as an `Err`, the way a timeout does.
        error_on: BTreeMap<String, String>,
    }

    impl Spawner for RecordingSpawner {
        fn run(
            &mut self,
            _task: &str,
            _shell: &[String],
            command: &str,
            _dir: &Path,
        ) -> Result<i32> {
            self.ran.push(command.to_string());
            if let Some(message) = self.error_on.get(command) {
                return Err(Error::other(message.clone()));
            }
            Ok(self.fail.get(command).copied().unwrap_or(0))
        }

        /// Records argv entries joined by a unit separator, so a test can tell
        /// `["a b"]` (one argument containing a space) from `["a", "b"]` (two).
        /// Joining with a space would erase exactly the distinction that makes
        /// argv steps injection-proof.
        fn run_argv(&mut self, _task: &str, argv: &[String], _dir: &Path) -> Result<i32> {
            let recorded = argv.join("\u{1f}");
            self.ran.push(recorded.clone());
            Ok(self.fail.get(&recorded).copied().unwrap_or(0))
        }
    }

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
    }

    /// Separator the RecordingSpawner joins argv entries with, so tests can
    /// assert on argument *boundaries* rather than on a flattened string.
    const SEP: &str = "\u{1f}";

    fn values_from(spec_toml: &str, input: &[&str]) -> crate::tasks::args::Values {
        let spec: crate::tasks::args::Spec = toml::from_str(spec_toml).expect("spec");
        let input: Vec<String> = input.iter().map(|value| value.to_string()).collect();
        crate::tasks::args::parse("t", &spec, &input).expect("parse")
    }

    /// The central safety property: a value full of shell metacharacters stays
    /// exactly one argument.
    ///
    /// This is why substitution is confined to argv steps. On `cmd` there is no
    /// quoting that survives `%VAR%` expansion, so interpolating into a shell
    /// string could not offer this guarantee -- and a guarantee that holds on
    /// two platforms out of three is worse than none, because it gets trusted.
    #[test]
    fn a_substituted_value_cannot_break_out_into_extra_arguments() {
        let set = set_from(
            r#"
[deploy]
run = [{ argv = ["echo", "{{msg}}"] }]

[[deploy.args]]
name = "msg"
"#,
        );
        let built = plan(&set, "deploy").unwrap();
        let nasty = r#"prod & del /f /s /q C:\ | echo "pwned" %PATH% $(id) `id`"#;
        let values = values_from("[[args]]\nname = \"msg\"\n", &[nasty]);

        let mut spawner = RecordingSpawner::default();
        execute_full(
            &built,
            Path::new("."),
            &mut spawner,
            &mut None,
            &values,
            true,
        )
        .unwrap();

        let parts: Vec<&str> = spawner.ran[0].split(SEP).collect();
        assert_eq!(
            parts.len(),
            2,
            "value split into extra arguments: {parts:?}"
        );
        assert_eq!(parts[0], "echo");
        assert_eq!(parts[1], nasty, "value must arrive verbatim");
    }

    /// `{{args}}` must produce one argv entry per argument.
    #[test]
    fn rest_arguments_stay_separate_entries() {
        let set = set_from(
            r#"
[test]
run = [{ argv = ["cargo", "test", "{{args}}"] }]
"#,
        );
        let built = plan(&set, "test").unwrap();
        let values = values_from("", &["--nocapture", "one two"]);

        let mut spawner = RecordingSpawner::default();
        execute_full(
            &built,
            Path::new("."),
            &mut spawner,
            &mut None,
            &values,
            false,
        )
        .unwrap();

        let parts: Vec<&str> = spawner.ran[0].split(SEP).collect();
        assert_eq!(parts, vec!["cargo", "test", "--nocapture", "one two"]);
    }

    /// Both spellings of a single command accept appended arguments, because
    /// the policy keys on step count -- rewriting `"x"` as `["x"]` must not
    /// silently change whether arguments are taken.
    #[test]
    fn a_single_command_accepts_appended_arguments_in_either_spelling() {
        for source in [r#"t = "cargo test""#, r#"t = ["cargo test"]"#] {
            let set = set_from(source);
            let built = plan(&set, "t").unwrap();
            assert_eq!(
                append_policy(&built.steps[0]),
                AppendPolicy::Append,
                "spelling changed the policy: {source}"
            );

            let values = values_from("", &["--nocapture"]);
            let mut spawner = RecordingSpawner::default();
            execute_full(
                &built,
                Path::new("."),
                &mut spawner,
                &mut None,
                &values,
                false,
            )
            .unwrap();
            assert_eq!(spawner.ran, vec!["cargo test --nocapture"], "{source}");
        }
    }

    /// Several steps refuse, and the error says how to fix it.
    #[test]
    fn a_multi_step_task_refuses_arguments_with_an_actionable_message() {
        let set = set_from(
            r#"
[ci]
run = ["fmt", "test"]
"#,
        );
        let built = plan(&set, "ci").unwrap();
        assert_eq!(append_policy(&built.steps[0]), AppendPolicy::Refuse);

        let values = values_from("", &["--nocapture"]);
        let mut spawner = RecordingSpawner::default();
        let error = execute_full(
            &built,
            Path::new("."),
            &mut spawner,
            &mut None,
            &values,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not accept arguments"), "{error}");
        assert!(
            error.contains("{{args}}"),
            "message must show the fix: {error}"
        );
        assert!(spawner.ran.is_empty(), "nothing may run before the refusal");
    }

    /// A parallel step also removes the single obvious place to append.
    #[test]
    fn a_task_with_a_parallel_step_refuses_arguments() {
        let set = set_from(
            r#"
a = "run-a"

[all]
run = [{ tasks = ["a"] }]
"#,
        );
        let built = plan(&set, "all").unwrap();
        let all = built.steps.iter().find(|s| s.name == "all").unwrap();
        assert_eq!(append_policy(all), AppendPolicy::Refuse);
    }

    /// Arguments belong to the named task, not to what it depends on.
    #[test]
    fn appended_arguments_do_not_leak_into_dependencies() {
        let set = set_from(
            r#"
prep = "do-prep"

[build]
run = "do-build"
depends = ["prep"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let values = values_from("", &["--flag"]);

        let mut spawner = RecordingSpawner::default();
        execute_full(
            &built,
            Path::new("."),
            &mut spawner,
            &mut None,
            &values,
            false,
        )
        .unwrap();
        assert_eq!(spawner.ran, vec!["do-prep", "do-build --flag"]);
    }

    /// A template naming `{{args}}` consumes the leftovers, so nothing is
    /// appended a second time.
    #[test]
    fn an_explicit_rest_placeholder_suppresses_appending() {
        let set = set_from(
            r#"
[t]
run = [{ argv = ["prog", "{{args}}", "--tail"] }]
"#,
        );
        let built = plan(&set, "t").unwrap();
        let values = values_from("", &["x"]);

        let mut spawner = RecordingSpawner::default();
        execute_full(
            &built,
            Path::new("."),
            &mut spawner,
            &mut None,
            &values,
            false,
        )
        .unwrap();

        let parts: Vec<&str> = spawner.ran[0].split(SEP).collect();
        // `x` lands where the placeholder is, not tacked on the end.
        assert_eq!(parts, vec!["prog", "x", "--tail"]);
    }

    /// The reason `run_post` exists: cleanup that a final line of `run` could
    /// never reach, because a failing task stops before it.
    #[test]
    fn teardown_runs_even_when_the_task_body_failed() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = "stop-db"
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("pytest".into(), 5);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(
            spawner.ran,
            vec!["pytest", "stop-db"],
            "teardown must still run"
        );
        // The failure is still the task's outcome: cleaning up is not passing.
        assert_eq!(outcomes.last().unwrap().code, 5);
    }

    /// A timeout surfaces as an `Err`, and an early `?` would skip teardown --
    /// precisely when the runaway process left the most behind.
    #[test]
    fn teardown_runs_when_the_body_times_out() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = "stop-db"
timeout = "1s"
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner
            .error_on
            .insert("pytest".into(), "timed out after 1s".into());

        let error = execute(&built, Path::new("."), &mut spawner).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(
            spawner.ran,
            vec!["pytest", "stop-db"],
            "teardown must still run after a timeout: {:?}",
            spawner.ran
        );
    }

    #[test]
    fn teardown_also_runs_after_success() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = "stop-db"
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["pytest", "stop-db"]);
        assert_eq!(outcomes.last().unwrap().code, 0);
    }

    /// Nothing was set up, so nothing needs tearing down.
    #[test]
    fn teardown_is_skipped_when_a_dependency_failed_before_the_body_started() {
        let set = set_from(
            r#"
start = "start-db"

[e2e]
run = "pytest"
run_post = "stop-db"
depends = ["start"]
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("start-db".into(), 1);

        execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(
            spawner.ran,
            vec!["start-db"],
            "neither the body nor its teardown may run: {:?}",
            spawner.ran
        );
    }

    /// osdk-specific: a task skipped as up to date never started, so its
    /// teardown has nothing to undo. mise has no incrementality and so cannot
    /// hit this case.
    #[test]
    fn teardown_is_skipped_when_freshness_skipped_the_task() {
        use crate::tasks::freshness::FreshnessState;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[build]
run = "compile"
run_post = "cleanup"
sources = ["src/**/*.rs"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut state = FreshnessState::default();

        let mut spawner = RecordingSpawner::default();
        execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(spawner.ran, vec!["compile", "cleanup"], "first run");

        let mut spawner = RecordingSpawner::default();
        let outcomes =
            execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert!(outcomes[0].skipped);
        assert!(
            spawner.ran.is_empty(),
            "a skipped task must not run its teardown: {:?}",
            spawner.ran
        );
    }

    /// A clean body with broken cleanup is not a success: the machine is not in
    /// the state the task promised.
    #[test]
    fn a_failing_teardown_fails_the_task() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = "stop-db"
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("stop-db".into(), 7);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(outcomes.last().unwrap().code, 7);
    }

    /// The body's exit code wins: cleanup succeeding does not rescue a failed
    /// test run.
    #[test]
    fn the_bodys_failure_outranks_a_later_teardown_failure() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = "stop-db"
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("pytest".into(), 3);
        spawner.fail.insert("stop-db".into(), 9);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(
            outcomes.last().unwrap().code,
            3,
            "the body's code identifies what actually went wrong"
        );
    }

    /// Stopping at the first failed teardown step would strand exactly the
    /// resources this feature exists to release.
    #[test]
    fn every_teardown_step_runs_even_after_one_fails() {
        let set = set_from(
            r#"
[e2e]
run = "pytest"
run_post = ["stop-db", "remove-tmp", "notify"]
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("stop-db".into(), 2);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(
            spawner.ran,
            vec!["pytest", "stop-db", "remove-tmp", "notify"]
        );
        assert_eq!(outcomes.last().unwrap().code, 2);
    }

    /// The common case needs no separate task, but a shared teardown can still
    /// reference one.
    #[test]
    fn teardown_can_reference_tasks_as_well_as_commands() {
        let set = set_from(
            r#"
cleanup = "do-cleanup"

[e2e]
run = "pytest"
run_post = [{ tasks = ["cleanup"] }]
"#,
        );
        let built = plan(&set, "e2e").unwrap();
        let mut spawner = RecordingSpawner::default();
        execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["pytest", "do-cleanup"]);
    }

    #[test]
    fn a_fresh_task_is_skipped_and_a_changed_source_reruns_it() {
        use crate::tasks::freshness::FreshnessState;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[build]
run = "compile"
sources = ["src/**/*.rs"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut state = FreshnessState::default();

        // First run: nothing recorded yet, so it must execute.
        let mut spawner = RecordingSpawner::default();
        let outcomes =
            execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(spawner.ran, vec!["compile"]);
        assert!(!outcomes[0].skipped);

        // Second run with untouched inputs: skipped, and no process spawned.
        let mut spawner = RecordingSpawner::default();
        let outcomes =
            execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert!(spawner.ran.is_empty(), "ran anyway: {:?}", spawner.ran);
        assert!(outcomes[0].skipped);

        // Touch a source: it runs again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_file(root, "src/a.rs", "fn a() { changed }");
        let mut spawner = RecordingSpawner::default();
        execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(spawner.ran, vec!["compile"]);
    }

    /// A failed run must not be recorded, or the next invocation skips a task
    /// that never succeeded -- the worst possible direction for this feature.
    #[test]
    fn a_failing_task_is_not_recorded_as_up_to_date() {
        use crate::tasks::freshness::FreshnessState;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[build]
run = "compile"
sources = ["src/**/*.rs"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut state = FreshnessState::default();

        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("compile".into(), 1);
        let outcomes =
            execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(outcomes[0].code, 1);

        // Next invocation must try again rather than declare victory.
        let mut spawner = RecordingSpawner::default();
        execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(
            spawner.ran,
            vec!["compile"],
            "a failed task must not be skipped"
        );
    }

    /// Freshness is per task: a fresh dependency is skipped while its dependent
    /// still runs. Skipping the whole plan would make `sources` useless.
    #[test]
    fn a_fresh_dependency_is_skipped_but_its_dependent_still_runs() {
        use crate::tasks::freshness::FreshnessState;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[codegen]
run = "generate"
sources = ["src/**/*.rs"]

[build]
run = "compile"
depends = ["codegen"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut state = FreshnessState::default();

        let mut spawner = RecordingSpawner::default();
        execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        assert_eq!(spawner.ran, vec!["generate", "compile"]);

        let mut spawner = RecordingSpawner::default();
        execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state)).unwrap();
        // codegen is fresh; build has no sources so it always runs.
        assert_eq!(spawner.ran, vec!["compile"]);
    }

    /// Without a state store the runner must not silently skip anything.
    #[test]
    fn execute_without_state_never_skips() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[build]
run = "compile"
sources = ["src/**/*.rs"]
"#,
        );
        let built = plan(&set, "build").unwrap();

        for _ in 0..2 {
            let mut spawner = RecordingSpawner::default();
            let outcomes = execute(&built, root, &mut spawner).unwrap();
            assert_eq!(spawner.ran, vec!["compile"]);
            assert!(!outcomes[0].skipped);
        }
    }

    /// A typo in `sources` must stop the task, not make it vacuously fresh.
    #[test]
    fn a_sources_pattern_matching_nothing_fails_the_run() {
        use crate::tasks::freshness::FreshnessState;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_file(root, "src/a.rs", "fn a() {}");

        let set = set_from(
            r#"
[build]
run = "compile"
sources = ["src/**/*.typo"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut state = FreshnessState::default();
        let mut spawner = RecordingSpawner::default();

        let error = execute_with_freshness(&built, root, &mut spawner, &mut Some(&mut state))
            .unwrap_err()
            .to_string();
        assert!(error.contains("matched no files"), "{error}");
        assert!(spawner.ran.is_empty(), "must not run on a bad pattern");
    }

    #[test]
    fn a_failing_step_stops_the_task_and_later_steps_do_not_run() {
        let set = set_from(
            r#"
[ci]
run = ["first", "second", "third"]
"#,
        );
        let built = plan(&set, "ci").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("second".into(), 3);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["first", "second"], "third must not run");
        assert_eq!(outcomes.last().unwrap().code, 3, "exit code must propagate");
    }

    #[test]
    fn ignore_error_lets_the_next_step_run_and_is_reported() {
        let set = set_from(
            r#"
[ci]
run = [{ cmd = "flaky", ignore_error = true }, "after"]
"#,
        );
        let built = plan(&set, "ci").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("flaky".into(), 1);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["flaky", "after"]);
        let outcome = outcomes.last().unwrap();
        assert_eq!(outcome.code, 0, "tolerated failure must not fail the task");
        // Tolerated is not the same as unnoticed.
        assert_eq!(outcome.tolerated_failures, vec!["flaky".to_string()]);
    }

    #[test]
    fn dependencies_run_before_the_task_that_needs_them() {
        let set = set_from(
            r#"
prep = "do-prep"

[build]
run = "do-build"
depends = ["prep"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut spawner = RecordingSpawner::default();
        execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["do-prep", "do-build"]);
    }

    #[test]
    fn a_failing_dependency_stops_the_dependent_task() {
        let set = set_from(
            r#"
prep = "do-prep"

[build]
run = "do-build"
depends = ["prep"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("do-prep".into(), 2);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["do-prep"], "dependent must not run");
        assert_eq!(outcomes.last().unwrap().code, 2);
    }

    #[test]
    fn a_referenced_task_runs_once_inside_its_parallel_step_not_twice() {
        // Planning has to include `a`/`b` so the parallel step can find their
        // commands, but including them must not also run them as if they were
        // ordinary prerequisites.
        let set = set_from(
            r#"
a = "run-a"
b = "run-b"

[all]
run = [{ tasks = ["a", "b"] }, "after"]
"#,
        );
        let built = plan(&set, "all").unwrap();
        let mut spawner = RecordingSpawner::default();
        execute(&built, Path::new("."), &mut spawner).unwrap();

        assert_eq!(
            spawner.ran.iter().filter(|c| *c == "run-a").count(),
            1,
            "referenced task ran {:?}",
            spawner.ran
        );
        assert_eq!(spawner.ran.last().map(String::as_str), Some("after"));
    }

    #[test]
    fn a_parallel_step_waits_for_its_tasks_and_surfaces_failure() {
        // This is the property `&` cannot provide: the runner waits and collects
        // the exit code instead of orphaning a background process.
        let set = set_from(
            r#"
a = "run-a"
b = "run-b"

[all]
run = [{ tasks = ["a", "b"] }, "after"]
"#,
        );
        let built = plan(&set, "all").unwrap();
        let mut spawner = RecordingSpawner::default();
        spawner.fail.insert("run-b".into(), 7);

        let outcomes = execute(&built, Path::new("."), &mut spawner).unwrap();
        assert!(spawner.ran.contains(&"run-a".to_string()));
        assert!(spawner.ran.contains(&"run-b".to_string()));
        assert!(
            !spawner.ran.contains(&"after".to_string()),
            "must not continue past a failed parallel step"
        );
        assert_eq!(outcomes.last().unwrap().code, 7);
    }

    /// The distinction from `depends`: naming a task that is not scheduled has
    /// no effect at all, rather than pulling it in.
    #[test]
    fn wait_for_does_not_schedule_a_task_that_was_not_already_included() {
        let set = set_from(
            r#"
migrate = "run-migrate"

[serve]
run = "run-serve"
wait_for = ["migrate"]
"#,
        );
        let built = plan(&set, "serve").unwrap();
        let names: Vec<&str> = built.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["serve"], "wait_for must not pull migrate in");

        let mut spawner = RecordingSpawner::default();
        execute(&built, Path::new("."), &mut spawner).unwrap();
        assert_eq!(spawner.ran, vec!["run-serve"]);
    }

    /// When both are scheduled, the waiter goes second.
    #[test]
    fn wait_for_orders_two_tasks_that_are_both_in_the_plan() {
        let set = set_from(
            r#"
[migrate]
run = "run-migrate"

[serve]
run = "run-serve"
wait_for = ["migrate"]

[up]
run = "run-up"
depends = ["serve", "migrate"]
"#,
        );
        let built = plan(&set, "up").unwrap();
        let order: Vec<&str> = built.steps.iter().map(|s| s.name.as_str()).collect();
        let position = |name: &str| order.iter().position(|entry| *entry == name).unwrap();
        assert!(
            position("migrate") < position("serve"),
            "wait_for ignored: {order:?}"
        );
    }

    /// `depends` still wins on membership: it schedules, `wait_for` only sorts.
    #[test]
    fn depends_schedules_while_wait_for_only_reorders() {
        let set = set_from(
            r#"
[a]
run = "run-a"

[b]
run = "run-b"
depends = ["a"]
wait_for = ["ghost"]
"#,
        );
        let built = plan(&set, "b").unwrap();
        let names: Vec<&str> = built.steps.iter().map(|s| s.name.as_str()).collect();
        // `a` is present because of depends; `ghost` does not exist at all and
        // that is not an error.
        assert_eq!(names, vec!["a", "b"]);
    }

    /// Two hints that contradict each other must not hang or refuse to run.
    #[test]
    fn contradictory_wait_for_hints_still_produce_a_plan() {
        let set = set_from(
            r#"
[a]
run = "run-a"
wait_for = ["b"]

[b]
run = "run-b"
wait_for = ["a"]

[both]
run = "run-both"
depends = ["a", "b"]
"#,
        );
        let built = plan(&set, "both").unwrap();
        let names: Vec<&str> = built.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"both"));
    }

    #[test]
    fn plan_lists_dependencies_before_dependents() {
        let set = set_from(
            r#"
prep = "echo prep"

[build]
run = "echo build"
depends = ["prep"]
"#,
        );
        let built = plan(&set, "build").unwrap();
        let names: Vec<&str> = built.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["prep", "build"]);
    }

    #[test]
    fn unknown_task_and_filtered_task_get_different_messages() {
        let set = set_from(r#"build = "echo build""#);
        let error = plan(&set, "nope").unwrap_err().to_string();
        assert!(error.contains("unknown task"), "{error}");

        // A task excluded by `when` must not be reported as unknown: the reader
        // would go hunting for a typo instead of reading the filter.
        let mut filtered = TaskSet::default();
        let other_os = if cfg!(windows) { "linux" } else { "windows" };
        let entries: BTreeMap<String, TaskEntry> = toml::from_str(&format!(
            "[only]\nrun = \"x\"\nwhen = {{ os = \"{other_os}\" }}\n"
        ))
        .unwrap();
        filtered.apply(entries).unwrap();
        let error = plan(&filtered, "only").unwrap_err().to_string();
        assert!(error.contains("not available on this platform"), "{error}");
        assert!(!error.contains("unknown task"), "{error}");
    }

    #[test]
    fn ignore_error_survives_into_the_plan() {
        let set = set_from(
            r#"
[ci]
run = [{ cmd = "flaky", ignore_error = true }, "strict"]
"#,
        );
        let built = plan(&set, "ci").unwrap();
        let steps = &built.steps[0].commands;
        assert!(matches!(
            steps[0],
            PlannedStep::Command {
                ignore_error: true,
                ..
            }
        ));
        assert!(matches!(
            steps[1],
            PlannedStep::Command {
                ignore_error: false,
                ..
            }
        ));
    }

    #[test]
    fn parallel_step_stays_a_distinct_kind() {
        let set = set_from(
            r#"
a = "echo a"
b = "echo b"

[all]
run = ["echo start", { tasks = ["a", "b"] }, "echo done"]
"#,
        );
        let built = plan(&set, "all").unwrap();
        let all = built.steps.iter().find(|t| t.name == "all").unwrap();
        assert_eq!(all.commands.len(), 3);
        assert_eq!(
            all.commands[1],
            PlannedStep::Parallel {
                tasks: vec!["a".into(), "b".into()]
            }
        );
        // The surrounding commands stay ordered around it.
        assert_eq!(commands_of(&built, "all"), vec!["echo start", "echo done"]);
    }

    #[test]
    fn task_shell_overrides_task_config_which_overrides_the_default() {
        let mut set = set_from(
            r#"
plain = "echo plain"

[custom]
run = "echo custom"
shell = "pwsh -Command"
"#,
        );
        let built = plan(&set, "plain").unwrap();
        let PlannedStep::Command { shell, .. } = &built.steps[0].commands[0] else {
            panic!("expected a command");
        };
        assert_eq!(*shell, default_shell());

        let built = plan(&set, "custom").unwrap();
        let PlannedStep::Command { shell, .. } = &built.steps[0].commands[0] else {
            panic!("expected a command");
        };
        assert_eq!(shell.as_slice(), ["pwsh", "-Command"]);

        set.apply_config(crate::tasks::TaskConfig {
            shell: Some("bash -c".into()),
            dir: None,
        });
        let built = plan(&set, "plain").unwrap();
        let PlannedStep::Command { shell, .. } = &built.steps[0].commands[0] else {
            panic!("expected a command");
        };
        assert_eq!(shell.as_slice(), ["bash", "-c"]);
    }

    #[test]
    fn default_shell_is_cmd_on_windows_and_sh_elsewhere() {
        // Guards the Windows premise: picking `sh` would work on a machine that
        // happens to have it and fail on a clean one.
        if cfg!(windows) {
            assert_eq!(default_shell()[0], "cmd");
        } else {
            assert_eq!(default_shell()[0], "sh");
        }
    }

    #[test]
    fn empty_shell_setting_is_rejected() {
        assert!(parse_shell("   ").is_err());
        assert_eq!(parse_shell("sh -c").unwrap(), vec!["sh", "-c"]);
    }

    #[test]
    fn task_env_sets_the_marker_and_orders_overrides() {
        let mut env = TaskEnv::default();
        env.set_vars.insert("JAVA_HOME".into(), "/jdk".into());
        env.set_vars
            .insert("SHARED".into(), "from-activation".into());
        env.path_prepend.push(PathBuf::from("/managed/bin"));

        let def: TaskDef = toml::from_str("run = \"x\"\n[env]\nSHARED = \"from-task\"\n").unwrap();
        let mut command = Command::new("echo");
        env.apply(&mut command, &def, "demo", "/usr/bin");

        let vars: BTreeMap<String, String> = command
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().into(), v?.to_string_lossy().into())))
            .collect();

        assert_eq!(vars.get("JAVA_HOME").map(String::as_str), Some("/jdk"));
        assert_eq!(vars.get(TASK_MARKER).map(String::as_str), Some("1"));
        assert_eq!(vars.get(TASK_NAME_VAR).map(String::as_str), Some("demo"));
        // The task's own env wins over the activation delta.
        assert_eq!(vars.get("SHARED").map(String::as_str), Some("from-task"));
        // PATH keeps the managed dir first and the inherited entries after.
        let path = vars.get("PATH").expect("PATH set");
        assert!(path.starts_with(&PathBuf::from("/managed/bin").to_string_lossy().to_string()));
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn dir_resolves_against_the_config_root_but_absolute_wins() {
        let root = Path::new(if cfg!(windows) { r"C:\proj" } else { "/proj" });
        assert_eq!(resolve_dir(root, None), root);
        assert_eq!(resolve_dir(root, Some("sub")), root.join("sub"));

        let absolute = if cfg!(windows) { r"C:\other" } else { "/other" };
        assert_eq!(resolve_dir(root, Some(absolute)), PathBuf::from(absolute));
    }

    #[test]
    fn cycles_are_rejected_by_plan_not_discovered_while_running() {
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
        let error = plan(&set, "a").unwrap_err().to_string();
        assert!(error.contains("cycle"), "{error}");
    }
}
