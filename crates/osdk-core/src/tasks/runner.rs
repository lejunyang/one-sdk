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
        for step in def.steps_for(windows) {
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
        steps.push(PlannedTask {
            standalone: standalone.contains(&name),
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
}

/// How to launch tasks. Split out so tests can observe without spawning.
pub trait Spawner {
    /// Run one command, returning its exit code.
    fn run(&mut self, task: &str, shell: &[String], command: &str, dir: &Path) -> Result<i32>;
}

/// Runs commands as real child processes.
pub struct ProcessSpawner {
    /// Environment computed the way `hook-env` would, injected into every child.
    pub env: TaskEnv,
    /// Per-task definitions, needed for their own `env` blocks.
    pub defs: BTreeMap<String, TaskDef>,
    /// PATH as inherited by osdk itself.
    pub base_path: String,
}

impl Spawner for ProcessSpawner {
    fn run(&mut self, task: &str, shell: &[String], command: &str, dir: &Path) -> Result<i32> {
        let (program, args) = shell
            .split_first()
            .ok_or_else(|| Error::config("`shell` must name an interpreter"))?;
        let mut child = Command::new(program);
        child.args(args).arg(command).current_dir(dir);
        if let Some(def) = self.defs.get(task) {
            self.env.apply(&mut child, def, task, &self.base_path);
        }
        let status = child.status().map_err(|error| {
            Error::other(format!("task `{task}`: cannot run `{program}`: {error}"))
        })?;
        // A signal-killed child reports no code; treat it as failure rather than
        // silently succeeding.
        Ok(status.code().unwrap_or(1))
    }
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
    let mut outcomes = Vec::new();
    for task in &plan.steps {
        // Present only so a parallel step can find its commands; running it here
        // as well would execute it twice.
        if !task.standalone {
            continue;
        }
        let dir = resolve_dir(config_root, task.dir.as_deref());
        let mut outcome = TaskOutcome {
            name: task.name.clone(),
            code: 0,
            tolerated_failures: Vec::new(),
        };
        for step in &task.commands {
            match step {
                PlannedStep::Command {
                    shell,
                    command,
                    ignore_error,
                } => {
                    let code = spawner.run(&task.name, shell, command, &dir)?;
                    if code != 0 {
                        if *ignore_error {
                            outcome.tolerated_failures.push(command.clone());
                            continue;
                        }
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
                            if let PlannedStep::Command {
                                shell,
                                command,
                                ignore_error,
                            } = sub_step
                            {
                                let code = spawner.run(&sub.name, shell, command, &sub_dir)?;
                                if code != 0 && !*ignore_error {
                                    outcome.code = code;
                                    outcomes.push(outcome);
                                    return Ok(outcomes);
                                }
                            }
                        }
                    }
                }
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
            Ok(self.fail.get(command).copied().unwrap_or(0))
        }
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
