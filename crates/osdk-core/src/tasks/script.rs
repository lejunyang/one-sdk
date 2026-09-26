//! The fourth scripting tier: an embedded Lua interpreter.
//!
//! Reached only when the declarative tiers genuinely cannot express something --
//! conditionals, loops that generate work, cross-platform path arithmetic. The
//! first three tiers cover the large majority of tasks, and a task that could
//! have been `run = "cargo build"` should stay that way.
//!
//! Why Lua and not one of the twelve alternatives measured: at +421.5 KiB over
//! a same-profile baseline it was the cheapest usable engine, MIT licensed, and
//! its startup was indistinguishable from not having an interpreter at all. The
//! cost it carries is a C compiler at build time, which is why the whole module
//! sits behind the `scripts` feature.
//!
//! **Nothing here is a sandbox.** The task file has already passed the trust
//! gate, and the same file can run arbitrary shell through `run`, so confining
//! Lua would protect nothing while costing the C++ toolchain Luau needs. What
//! the host API does provide is *convenience with correct semantics*: `osdk.sh`
//! returns an exit code rather than throwing, `osdk.path.join` uses the host
//! separator, and `osdk.run` reuses the runner's own argv execution so a value
//! with spaces stays one argument.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mlua::{HookTriggers, Lua, Value, VmState};

use crate::error::{Error, Result};

/// Captured stdout and stderr are deliberately bounded. A task that needs to
/// stream an unbounded build log should use `run`, whose stdio is inherited.
const EXEC_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

struct ExecRequest {
    argv: Vec<String>,
    shell: bool,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    input: Option<Vec<u8>>,
    check: bool,
}

/// What a Lua script is allowed to know about its surroundings.
pub struct ScriptContext {
    /// Directory the task runs in.
    pub dir: PathBuf,
    /// Project root, for `osdk.project_root`.
    pub project_root: PathBuf,
    /// Task name, for diagnostics.
    pub name: String,
    /// Parsed argument values, exposed as `osdk.args`.
    pub args: BTreeMap<String, String>,
    /// Leftover arguments, exposed as `osdk.argv`.
    pub argv: Vec<String>,
    /// Environment the runner would give a spawned command.
    pub env: BTreeMap<String, String>,
    /// Wall-clock limit inherited from the task definition.
    pub timeout: Option<Duration>,
}

/// Evaluate `source`, returning the exit code the task should report.
///
/// A script that returns nothing succeeds; one that returns a number uses it as
/// the exit code; one that raises an error fails the task with that message.
/// Returning a code rather than only throwing matters because "this step failed
/// and here is its status" is the normal case for a task runner, not an
/// exceptional one.
pub fn eval(source: &str, context: &ScriptContext) -> Result<i32> {
    let lua = Lua::new();
    if let Some(limit) = context.timeout {
        let started = Instant::now();
        let task = context.name.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1_024),
            move |_, _| {
                if started.elapsed() >= limit {
                    return Err(mlua::Error::runtime(format!(
                        "task `{task}`: timed out after {}s",
                        limit.as_secs_f64()
                    )));
                }
                Ok(VmState::Continue)
            },
        );
    }
    install_host_api(&lua, context)?;

    let chunk = lua.load(source).set_name(format!("task {}", context.name));
    let value: Value = chunk
        .eval()
        .map_err(|error| Error::other(format!("task `{}`: {error}", context.name)))?;

    Ok(match value {
        Value::Nil | Value::Boolean(true) => 0,
        // `return false` reads as failure; mapping it to 1 avoids the trap of a
        // script that carefully returns false and is reported as success.
        Value::Boolean(false) => 1,
        Value::Integer(code) => code as i32,
        Value::Number(code) => code as i32,
        other => {
            return Err(Error::other(format!(
                "task `{}`: script returned {}, expected a number or nothing",
                context.name,
                other.type_name()
            )))
        }
    })
}

fn install_host_api(lua: &Lua, context: &ScriptContext) -> Result<()> {
    let to_lua = |error: mlua::Error| Error::other(format!("lua host api: {error}"));
    let globals = lua.globals();
    let osdk = lua.create_table().map_err(to_lua)?;

    osdk.set(
        "project_root",
        context.project_root.to_string_lossy().to_string(),
    )
    .map_err(to_lua)?;
    osdk.set("dir", context.dir.to_string_lossy().to_string())
        .map_err(to_lua)?;
    osdk.set("task", context.name.clone()).map_err(to_lua)?;

    // Platform facts, so a script can branch without shelling out to `uname`.
    let platform = lua.create_table().map_err(to_lua)?;
    platform
        .set(
            "os",
            if cfg!(windows) {
                "windows"
            } else if cfg!(target_os = "macos") {
                "macos"
            } else {
                "linux"
            },
        )
        .map_err(to_lua)?;
    platform.set("windows", cfg!(windows)).map_err(to_lua)?;
    platform
        .set("arch", std::env::consts::ARCH)
        .map_err(to_lua)?;
    osdk.set("platform", platform).map_err(to_lua)?;

    // Arguments, as a table keyed by declared name plus the leftovers.
    let args = lua.create_table().map_err(to_lua)?;
    for (key, value) in &context.args {
        args.set(key.as_str(), value.as_str()).map_err(to_lua)?;
    }
    osdk.set("args", args).map_err(to_lua)?;
    let argv = lua.create_table().map_err(to_lua)?;
    for (index, value) in context.argv.iter().enumerate() {
        // Lua is 1-based; using 0 here would make `ipairs` skip everything.
        argv.set(index + 1, value.as_str()).map_err(to_lua)?;
    }
    osdk.set("argv", argv).map_err(to_lua)?;

    // `osdk.sh(cmd)` -- run through the platform shell, return the exit code.
    let dir = context.dir.clone();
    let env = context.env.clone();
    let task = context.name.clone();
    let timeout = context.timeout;
    let sh = lua
        .create_function(move |_, command: String| {
            let mut child = shell_command(&command);
            child.current_dir(&dir);
            for (key, value) in &env {
                child.env(key, value);
            }
            crate::tasks::runner::run_command_to_completion(&task, "shell", child, timeout)
                .map_err(mlua::Error::external)
        })
        .map_err(to_lua)?;
    osdk.set("sh", sh).map_err(to_lua)?;

    // `osdk.run("prog", "arg")` -- exec directly, no shell.
    //
    // The counterpart to the `argv` step kind: each table entry becomes one
    // argument, so a value containing spaces or metacharacters cannot split or
    // be reinterpreted. Scripts that build commands from data should use this.
    let dir = context.dir.clone();
    let env = context.env.clone();
    let task = context.name.clone();
    let timeout = context.timeout;
    let run = lua
        .create_function(move |_, argv: mlua::Variadic<String>| {
            let argv: Vec<String> = argv.into_iter().collect();
            let Some((program, rest)) = argv.split_first() else {
                return Err(mlua::Error::external(format!(
                    "task `{task}`: osdk.run needs at least a program name"
                )));
            };
            let mut child = std::process::Command::new(program);
            child.args(rest).current_dir(&dir);
            for (key, value) in &env {
                child.env(key, value);
            }
            crate::tasks::runner::run_command_to_completion(&task, program, child, timeout)
                .map_err(mlua::Error::external)
        })
        .map_err(to_lua)?;
    osdk.set("run", run).map_err(to_lua)?;

    // `exec("prog", "arg")` captures a direct command. The table form adds
    // cwd/env/stdin/check without making the common case pay for ceremony:
    // `exec { "prog", "arg", cwd = "subdir" }`.
    let default_dir = context.dir.clone();
    let default_env = context.env.clone();
    let task = context.name.clone();
    let timeout = context.timeout;
    let exec = lua
        .create_function(move |lua, values: mlua::Variadic<Value>| {
            let request = parse_exec_request(values, &default_dir, &default_env)?;
            let (program, mut command) = if request.shell {
                ("shell".to_string(), shell_command(&request.argv[0]))
            } else {
                let Some((program, rest)) = request.argv.split_first() else {
                    return Err(mlua::Error::external(format!(
                        "task `{task}`: exec needs a program name"
                    )));
                };
                let mut command = std::process::Command::new(program);
                command.args(rest);
                (program.clone(), command)
            };
            command.current_dir(&request.cwd).envs(&request.env);
            let captured = crate::tasks::tree::run_captured(
                &mut command,
                timeout,
                request.input,
                EXEC_OUTPUT_LIMIT,
            )
            .map_err(|error| {
                mlua::Error::external(format!("task `{task}`: cannot run `{program}`: {error}"))
            })?
            .ok_or_else(|| {
                mlua::Error::external(format!(
                    "task `{task}`: timed out after {}; the process tree was terminated",
                    timeout
                        .map(|limit| format!("{}s", limit.as_secs()))
                        .unwrap_or_else(|| "the configured limit".into())
                ))
            })?;

            if request.check && captured.code != 0 {
                let stderr = String::from_utf8_lossy(&captured.stderr);
                let detail = stderr.trim();
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                return Err(mlua::Error::external(format!(
                    "task `{task}`: `{program}` exited with code {}{suffix}",
                    captured.code
                )));
            }

            let result = lua.create_table()?;
            result.set("code", captured.code)?;
            result.set("success", captured.code == 0)?;
            result.set("stdout", lua.create_string(&captured.stdout)?)?;
            result.set("stderr", lua.create_string(&captured.stderr)?)?;
            result.set("stdout_truncated", captured.stdout_truncated)?;
            result.set("stderr_truncated", captured.stderr_truncated)?;
            Ok(result)
        })
        .map_err(to_lua)?;
    osdk.set("exec", exec).map_err(to_lua)?;

    // Path helpers: the reason a script reaches for Lua in the first place is
    // often just "join these with the right separator".
    let path = lua.create_table().map_err(to_lua)?;
    let join = lua
        .create_function(|_, parts: mlua::Variadic<String>| {
            let mut buf = PathBuf::new();
            for part in parts {
                buf.push(part);
            }
            Ok(buf.to_string_lossy().to_string())
        })
        .map_err(to_lua)?;
    path.set("join", join).map_err(to_lua)?;
    let dir = context.dir.clone();
    let exists = lua
        .create_function(move |_, target: String| Ok(resolve_path(&dir, &target).exists()))
        .map_err(to_lua)?;
    path.set("exists", exists).map_err(to_lua)?;
    let dir = context.dir.clone();
    path.set(
        "is_file",
        lua.create_function(move |_, target: String| Ok(resolve_path(&dir, &target).is_file()))
            .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    path.set(
        "is_dir",
        lua.create_function(move |_, target: String| Ok(resolve_path(&dir, &target).is_dir()))
            .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    path.set(
        "is_absolute",
        lua.create_function(|_, target: String| Ok(Path::new(&target).is_absolute()))
            .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    path.set(
        "parent",
        lua.create_function(|_, target: String| {
            Ok(Path::new(&target)
                .parent()
                .map(|path| path.to_string_lossy().to_string()))
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    path.set(
        "basename",
        lua.create_function(|_, target: String| {
            Ok(Path::new(&target)
                .file_name()
                .map(|name| name.to_string_lossy().to_string()))
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    path.set(
        "extension",
        lua.create_function(|_, target: String| {
            Ok(Path::new(&target)
                .extension()
                .map(|extension| extension.to_string_lossy().to_string()))
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    path.set(
        "absolute",
        lua.create_function(move |_, target: String| {
            Ok(normalize_path(&resolve_path(&dir, &target))
                .to_string_lossy()
                .to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    path.set(
        "relative",
        lua.create_function(move |_, (target, base): (String, Option<String>)| {
            let target = normalize_path(&resolve_path(&dir, &target));
            let base = normalize_path(&resolve_path(&dir, base.as_deref().unwrap_or(".")));
            Ok(relative_path(&target, &base).to_string_lossy().to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    osdk.set("path", path).map_err(to_lua)?;

    // File helpers resolve relative paths from the task directory and raise on
    // failure. They make the portable spelling shorter than shelling out to
    // cp/mkdir/rm (which do not exist on a clean Windows machine).
    let fs = lua.create_table().map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "mkdir",
        lua.create_function(move |_, target: String| {
            let target = resolve_path(&dir, &target);
            std::fs::create_dir_all(&target).map_err(|error| fs_error("mkdir", &target, error))?;
            Ok(target.to_string_lossy().to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "read",
        lua.create_function(move |lua, target: String| {
            let target = resolve_path(&dir, &target);
            let contents =
                std::fs::read(&target).map_err(|error| fs_error("read", &target, error))?;
            lua.create_string(contents)
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "write",
        lua.create_function(move |_, (target, contents): (String, mlua::String)| {
            let target = resolve_path(&dir, &target);
            ensure_parent(&target)?;
            std::fs::write(&target, contents.as_bytes())
                .map_err(|error| fs_error("write", &target, error))?;
            Ok(target.to_string_lossy().to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "copy",
        lua.create_function(move |_, (source, target): (String, String)| {
            let source = resolve_path(&dir, &source);
            let target = resolve_path(&dir, &target);
            copy_path(&source, &target)?;
            Ok(target.to_string_lossy().to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "move",
        lua.create_function(move |_, (source, target): (String, String)| {
            let source = resolve_path(&dir, &source);
            let target = resolve_path(&dir, &target);
            ensure_parent(&target)?;
            std::fs::rename(&source, &target).map_err(|error| fs_error("move", &source, error))?;
            Ok(target.to_string_lossy().to_string())
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "remove",
        lua.create_function(move |_, target: String| {
            let target = resolve_path(&dir, &target);
            remove_path(&target)
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    let dir = context.dir.clone();
    fs.set(
        "glob",
        lua.create_function(move |lua, pattern: String| {
            let matches = glob_paths(&dir, &pattern)?;
            let result = lua.create_table()?;
            for (index, path) in matches.iter().enumerate() {
                result.set(index + 1, path.to_string_lossy().to_string())?;
            }
            Ok(result)
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;
    osdk.set("fs", fs).map_err(to_lua)?;

    let dir = context.dir.clone();
    let search_path = context.env.get("PATH").cloned();
    osdk.set(
        "which",
        lua.create_function(move |_, program: String| {
            Ok(which::which_in(&program, search_path.as_deref(), &dir)
                .ok()
                .map(|path| path.to_string_lossy().to_string()))
        })
        .map_err(to_lua)?,
    )
    .map_err(to_lua)?;

    // `osdk.env(name)` -- read an environment variable, nil when unset.
    let env_lookup = context.env.clone();
    let env_fn = lua
        .create_function(move |_, name: String| {
            Ok(env_lookup
                .get(&name)
                .cloned()
                .or_else(|| std::env::var(&name).ok()))
        })
        .map_err(to_lua)?;
    osdk.set("env", env_fn).map_err(to_lua)?;

    // The table remains the explicit, collision-resistant API. Task scripts
    // also get a small prelude so the common case reads like a task DSL rather
    // than host-API plumbing: `run(...)`, `exec(...)`, `root`, `args`.
    for name in ["run", "sh", "exec", "env", "which"] {
        let value: Value = osdk.get(name).map_err(to_lua)?;
        globals.set(name, value).map_err(to_lua)?;
    }
    let path: Value = osdk.get("path").map_err(to_lua)?;
    globals.set("path", path).map_err(to_lua)?;
    let path: mlua::Table = osdk.get("path").map_err(to_lua)?;
    for name in [
        "join",
        "exists",
        "is_file",
        "is_dir",
        "is_absolute",
        "parent",
        "basename",
        "extension",
        "absolute",
        "relative",
    ] {
        let value: Value = path.get(name).map_err(to_lua)?;
        globals.set(name, value).map_err(to_lua)?;
    }
    let fs: Value = osdk.get("fs").map_err(to_lua)?;
    globals.set("fs", fs).map_err(to_lua)?;
    let fs: mlua::Table = osdk.get("fs").map_err(to_lua)?;
    for name in ["mkdir", "read", "write", "copy", "move", "remove", "glob"] {
        let value: Value = fs.get(name).map_err(to_lua)?;
        globals.set(name, value).map_err(to_lua)?;
    }
    for (short, full) in [
        ("root", "project_root"),
        ("dir", "dir"),
        ("task", "task"),
        ("args", "args"),
        ("argv", "argv"),
        ("platform", "platform"),
    ] {
        let value: Value = osdk.get(full).map_err(to_lua)?;
        globals.set(short, value).map_err(to_lua)?;
    }
    globals.set("osdk", osdk).map_err(to_lua)?;

    // Replace `os.getenv` so it cannot quietly disagree with `osdk.env`.
    //
    // A task's `env` is applied to the *child processes* osdk spawns, not to
    // osdk's own process, so stock `os.getenv` returns nil for every variable
    // the task declared -- while `osdk.env`, and any command the script runs,
    // see it fine. That disagreement is silent and the nil looks like an unset
    // variable, so the natural conclusion is that the `env` table is broken.
    //
    // Injecting the values into the real process environment would make the two
    // agree, but `set_var` is a data race by definition (unsafe as of edition
    // 2024) and would leak one task's `env` into every later task, the
    // freshness state and the trust checks. Redirecting the read is the smaller
    // change and keeps the process environment honest.
    let table: mlua::Table = globals.get("os").map_err(to_lua)?;
    let env_lookup = context.env.clone();
    let getenv = lua
        .create_function(move |_, name: String| {
            Ok(env_lookup
                .get(&name)
                .cloned()
                .or_else(|| std::env::var(&name).ok()))
        })
        .map_err(to_lua)?;
    table.set("getenv", getenv).map_err(to_lua)?;

    Ok(())
}

/// A command that runs `command` through the platform shell.
fn shell_command(command: &str) -> std::process::Command {
    let shell = crate::tasks::runner::default_shell();
    let mut child = std::process::Command::new(&shell[0]);
    child.args(&shell[1..]).arg(command);
    child
}

fn parse_exec_request(
    values: mlua::Variadic<Value>,
    default_dir: &Path,
    default_env: &BTreeMap<String, String>,
) -> mlua::Result<ExecRequest> {
    let values: Vec<Value> = values.into_iter().collect();
    let (argv, shell, cwd, env, input, check) = if let [Value::Table(options)] = values.as_slice() {
        for pair in options.clone().pairs::<Value, Value>() {
            let (key, _) = pair?;
            match key {
                Value::Integer(_) => {}
                Value::String(key)
                    if matches!(
                        key.to_str()?.as_ref(),
                        "argv" | "command" | "cwd" | "env" | "stdin" | "check"
                    ) => {}
                Value::String(key) => {
                    return Err(mlua::Error::external(format!(
                        "unknown exec option `{}`",
                        key.to_string_lossy()
                    )));
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "exec option keys must be names or argv indexes, got {}",
                        other.type_name()
                    )));
                }
            }
        }
        let explicit_argv: Option<mlua::Table> = options.get("argv")?;
        let command: Option<String> = options.get("command")?;
        let positional = options.raw_len() > 0;
        let selected = usize::from(explicit_argv.is_some())
            + usize::from(command.is_some())
            + usize::from(positional);
        if selected != 1 {
            return Err(mlua::Error::external(
                "exec table sets exactly one of positional argv, `argv`, or `command`",
            ));
        }
        let (argv, shell) = if let Some(command) = command {
            (vec![command], true)
        } else {
            let table = explicit_argv.as_ref().unwrap_or(options);
            let argv = table
                .sequence_values::<String>()
                .collect::<mlua::Result<Vec<_>>>()?;
            (argv, false)
        };
        let cwd: Option<String> = options.get("cwd")?;
        let mut env = default_env.clone();
        if let Some(overrides) = options.get::<Option<mlua::Table>>("env")? {
            for pair in overrides.pairs::<String, String>() {
                let (key, value) = pair?;
                env.insert(key, value);
            }
        }
        let input = options
            .get::<Option<mlua::String>>("stdin")?
            .map(|value| value.as_bytes().to_vec());
        let check = options.get::<Option<bool>>("check")?.unwrap_or(false);
        (
            argv,
            shell,
            cwd.map_or_else(
                || default_dir.to_path_buf(),
                |cwd| resolve_path(default_dir, &cwd),
            ),
            env,
            input,
            check,
        )
    } else {
        let argv = values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => value.to_str().map(|value| value.to_string()),
                other => Err(mlua::Error::external(format!(
                    "exec arguments must be strings, got {}",
                    other.type_name()
                ))),
            })
            .collect::<mlua::Result<Vec<_>>>()?;
        (
            argv,
            false,
            default_dir.to_path_buf(),
            default_env.clone(),
            None,
            false,
        )
    };

    if argv.is_empty() || argv[0].is_empty() {
        return Err(mlua::Error::external("exec needs a program or command"));
    }
    Ok(ExecRequest {
        argv,
        shell,
        cwd,
        env,
        input,
        check,
    })
}

fn resolve_path(dir: &Path, target: &str) -> PathBuf {
    let target = Path::new(target);
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        dir.join(target)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized
                    .file_name()
                    .is_some_and(|name| name != std::ffi::OsStr::new(".."))
                {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn relative_path(target: &Path, base: &Path) -> PathBuf {
    let target: Vec<_> = target.components().collect();
    let base: Vec<_> = base.components().collect();
    let common = target
        .iter()
        .zip(&base)
        .take_while(|(left, right)| left == right)
        .count();

    // Different Windows drive prefixes (or absolute versus relative inputs)
    // have no meaningful lexical relative path.
    if common == 0 && (target.first() != base.first()) {
        return target
            .iter()
            .map(|component| component.as_os_str())
            .collect();
    }

    let mut result = PathBuf::new();
    for component in &base[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            result.push("..");
        }
    }
    for component in &target[common..] {
        result.push(component.as_os_str());
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

fn fs_error(action: &str, path: &Path, error: std::io::Error) -> mlua::Error {
    mlua::Error::external(format!("cannot {action} `{}`: {error}", path.display()))
}

fn ensure_parent(path: &Path) -> mlua::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| fs_error("create directory", parent, error))?;
    }
    Ok(())
}

fn copy_path(source: &Path, target: &Path) -> mlua::Result<()> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|error| fs_error("inspect", source, error))?;
    if metadata.file_type().is_symlink() {
        ensure_parent(target)?;
        let link =
            std::fs::read_link(source).map_err(|error| fs_error("read link", source, error))?;
        create_symlink(&link, target, source)
            .map_err(|error| fs_error("copy link", source, error))?;
        return Ok(());
    }
    if metadata.is_file() {
        ensure_parent(target)?;
        std::fs::copy(source, target).map_err(|error| fs_error("copy", source, error))?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(mlua::Error::external(format!(
            "cannot copy `{}`: unsupported file type",
            source.display()
        )));
    }

    std::fs::create_dir_all(target).map_err(|error| fs_error("create directory", target, error))?;
    for entry in
        std::fs::read_dir(source).map_err(|error| fs_error("read directory", source, error))?
    {
        let entry = entry.map_err(|error| fs_error("read directory", source, error))?;
        copy_path(&entry.path(), &target.join(entry.file_name()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(link: &Path, target: &Path, _source: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link, target)
}

#[cfg(windows)]
fn create_symlink(link: &Path, target: &Path, source: &Path) -> std::io::Result<()> {
    if std::fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(link, target)
    } else {
        std::os::windows::fs::symlink_file(link, target)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_link: &Path, _target: &Path, _source: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symbolic links are not supported on this platform",
    ))
}

fn remove_path(target: &Path) -> mlua::Result<bool> {
    let metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(fs_error("inspect", target, error)),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(target).map_err(|error| fs_error("remove", target, error))?;
    } else {
        std::fs::remove_file(target).map_err(|error| fs_error("remove", target, error))?;
    }
    Ok(true)
}

fn glob_paths(dir: &Path, pattern: &str) -> mlua::Result<Vec<PathBuf>> {
    if Path::new(pattern).is_absolute() {
        return Err(mlua::Error::external(
            "glob patterns must be relative to the task directory",
        ));
    }
    let pattern = pattern.replace('\\', "/");
    let matcher = globset::Glob::new(&pattern)
        .map_err(mlua::Error::external)?
        .compile_matcher();
    let mut base = dir.to_path_buf();
    for segment in pattern.split('/') {
        if segment.chars().any(|character| "*?[{".contains(character)) {
            break;
        }
        if !segment.is_empty() && segment != "." {
            base.push(segment);
        }
    }
    let mut matches = Vec::new();
    if !base.exists() {
        return Ok(matches);
    }
    for entry in walkdir::WalkDir::new(base).follow_links(false) {
        let entry = entry.map_err(mlua::Error::external)?;
        let relative = entry
            .path()
            .strip_prefix(dir)
            .map_err(mlua::Error::external)?;
        let portable = relative.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&portable) {
            matches.push(entry.into_path());
        }
    }
    matches.sort();
    Ok(matches)
}

#[cfg(test)]
mod tests {

    use super::*;

    /// `os.getenv` and `osdk.env` must never disagree.
    ///
    /// The task's `env` reaches spawned children but not osdk's own process, so
    /// an unpatched `os.getenv` returns nil for exactly the variables the task
    /// declared -- and a nil is indistinguishable from "not set", which makes
    /// the `env` table look broken. Both readers are asserted here because
    /// fixing one and forgetting the other is the failure this guards.
    #[test]
    fn os_getenv_sees_the_task_env_just_like_osdk_env() {
        let mut context = context();
        context
            .env
            .insert("OSDK_TEST_TASK_VAR".into(), "declared-by-task".into());

        let source = "local a = os.getenv('OSDK_TEST_TASK_VAR') \
             local b = osdk.env('OSDK_TEST_TASK_VAR') \
             if a ~= 'declared-by-task' then return 11 end \
             if b ~= 'declared-by-task' then return 12 end \
             if a ~= b then return 13 end \
             return 0";
        assert_eq!(
            eval(source, &context).unwrap(),
            0,
            "11 = os.getenv wrong, 12 = osdk.env wrong, 13 = they disagree"
        );
    }

    /// Overriding `os.getenv` must not blind it to the real environment.
    #[test]
    fn os_getenv_still_falls_back_to_the_process_environment() {
        let source = "if os.getenv('PATH') ~= nil then return 0 end return 1";
        assert_eq!(eval(source, &context()).unwrap(), 0);
    }

    /// An unset variable must still read as nil, not as an empty string.
    #[test]
    fn an_unset_variable_is_still_nil() {
        let source = "if os.getenv('OSDK_DEFINITELY_UNSET_XYZ') == nil then return 0 end \
             return 1";
        assert_eq!(eval(source, &context()).unwrap(), 0);
    }

    fn context() -> ScriptContext {
        ScriptContext {
            dir: std::env::temp_dir(),
            project_root: PathBuf::from(if cfg!(windows) { r"C:\proj" } else { "/proj" }),
            name: "demo".into(),
            args: BTreeMap::new(),
            argv: Vec::new(),
            env: BTreeMap::new(),
            timeout: None,
        }
    }

    #[test]
    fn a_script_that_returns_nothing_succeeds() {
        assert_eq!(eval("local x = 1 + 1", &context()).unwrap(), 0);
    }

    #[test]
    fn a_returned_number_becomes_the_exit_code() {
        assert_eq!(eval("return 3", &context()).unwrap(), 3);
        assert_eq!(eval("return 0", &context()).unwrap(), 0);
    }

    /// `return false` must not read as success: a script that deliberately
    /// signals failure and gets reported as passing is the worst outcome here.
    #[test]
    fn returning_false_is_a_failure() {
        assert_eq!(eval("return false", &context()).unwrap(), 1);
        assert_eq!(eval("return true", &context()).unwrap(), 0);
    }

    #[test]
    fn a_syntax_error_names_the_task() {
        let error = eval("this is not lua (", &context())
            .unwrap_err()
            .to_string();
        assert!(error.contains("demo"), "{error}");
    }

    #[test]
    fn a_runtime_error_fails_the_task() {
        let error = eval("error('boom')", &context()).unwrap_err().to_string();
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_non_numeric_return_is_rejected_rather_than_coerced() {
        // Silently treating a string as 0 would hide a script that meant to
        // return a status and got the type wrong.
        let error = eval("return {}", &context()).unwrap_err().to_string();
        assert!(error.contains("expected a number"), "{error}");
    }

    #[test]
    fn platform_facts_are_available() {
        let source = if cfg!(windows) {
            "return osdk.platform.windows and 0 or 1"
        } else {
            "return osdk.platform.windows and 1 or 0"
        };
        assert_eq!(eval(source, &context()).unwrap(), 0);
        assert_eq!(
            eval("return #osdk.platform.arch > 0 and 0 or 1", &context()).unwrap(),
            0
        );
    }

    #[test]
    fn path_join_uses_the_host_separator() {
        let source = r#"
            local p = osdk.path.join("a", "b", "c")
            local sep = osdk.platform.windows and "\\" or "/"
            return p == ("a" .. sep .. "b" .. sep .. "c") and 0 or 1
        "#;
        assert_eq!(eval(source, &context()).unwrap(), 0);
    }

    #[test]
    fn relative_path_checks_start_at_the_task_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("marker.txt"), b"ok").unwrap();
        let mut ctx = context();
        ctx.dir = temp.path().to_path_buf();

        assert_eq!(
            eval("return osdk.path.exists('marker.txt') and 0 or 1", &ctx).unwrap(),
            0
        );
    }

    #[test]
    fn project_root_and_task_name_are_exposed() {
        assert_eq!(
            eval("return #osdk.project_root > 0 and 0 or 1", &context()).unwrap(),
            0
        );
        assert_eq!(
            eval("return osdk.task == 'demo' and 0 or 1", &context()).unwrap(),
            0
        );
    }

    #[test]
    fn short_names_alias_the_explicit_host_api() {
        let source = r#"
            return run == osdk.run
                and sh == osdk.sh
                and exec == osdk.exec
                and env == osdk.env
                and path == osdk.path
                and fs == osdk.fs
                and which == osdk.which
                and join == osdk.path.join
                and exists == osdk.path.exists
                and mkdir == osdk.fs.mkdir
                and read == osdk.fs.read
                and write == osdk.fs.write
                and copy == osdk.fs.copy
                and move == osdk.fs.move
                and remove == osdk.fs.remove
                and glob == osdk.fs.glob
                and root == osdk.project_root
                and dir == osdk.dir
                and task == osdk.task
                and args == osdk.args
                and argv == osdk.argv
                and platform == osdk.platform
                and 0 or 1
        "#;
        assert_eq!(eval(source, &context()).unwrap(), 0);
    }

    #[test]
    fn filesystem_and_path_helpers_are_relative_to_the_task_directory() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context();
        ctx.dir = temp.path().to_path_buf();
        ctx.project_root = temp.path().to_path_buf();

        let source = r#"
            mkdir("source/nested")
            local bytes = "hello" .. string.char(0, 255)
            write("source/nested/a.txt", bytes)
            if read("source/nested/a.txt") ~= bytes then return 11 end
            if not is_file("source/nested/a.txt") then return 12 end
            if not is_dir("source/nested") then return 13 end

            copy("source", "copied")
            if read("copied/nested/a.txt") ~= bytes then return 14 end
            local matched = glob("copied/**/*.txt")
            if #matched ~= 1 then return 15 end

            local absolute_file = absolute("copied/nested/a.txt")
            if not is_absolute(absolute_file) then return 16 end
            if basename(absolute_file) ~= "a.txt" then return 17 end
            if extension(absolute_file) ~= "txt" then return 18 end
            if parent(absolute_file) ~= join(dir, "copied", "nested") then return 19 end
            if relative(absolute_file, dir) ~= join("copied", "nested", "a.txt") then return 20 end

            move("copied/nested/a.txt", "moved/result.txt")
            if exists("copied/nested/a.txt") or not exists("moved/result.txt") then return 21 end
            if not remove("copied") then return 22 end
            if remove("copied") then return 23 end
            return 0
        "#;
        assert_eq!(eval(source, &ctx).unwrap(), 0);
    }

    #[test]
    fn which_uses_the_task_path() {
        let executable = std::env::current_exe().unwrap();
        let mut ctx = context();
        ctx.dir = executable.parent().unwrap().to_path_buf();
        ctx.env.insert(
            "PATH".into(),
            std::env::join_paths([ctx.dir.clone()])
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
        let name = executable.file_name().unwrap().to_string_lossy();
        let name = name.replace('\\', "\\\\").replace('"', "\\\"");

        assert_eq!(
            eval(&format!(r#"return which("{name}") and 0 or 1"#), &ctx).unwrap(),
            0
        );
    }

    #[test]
    fn arguments_are_visible_as_a_table_and_argv_is_one_based() {
        let mut ctx = context();
        ctx.args.insert("env".into(), "prod".into());
        ctx.argv = vec!["first".into(), "second".into()];

        assert_eq!(
            eval("return osdk.args.env == 'prod' and 0 or 1", &ctx).unwrap(),
            0
        );
        // A 0-based table would make #osdk.argv report 0 and ipairs skip all.
        assert_eq!(eval("return #osdk.argv == 2 and 0 or 1", &ctx).unwrap(), 0);
        assert_eq!(
            eval("return osdk.argv[1] == 'first' and 0 or 1", &ctx).unwrap(),
            0
        );
    }

    #[test]
    fn sh_returns_an_exit_code_instead_of_throwing() {
        // `exit 4` happens to be spelled the same in cmd and sh.
        let source = "local code = osdk.sh('exit 4') return code";
        assert_eq!(eval(source, &context()).unwrap(), 4);
    }

    /// The argv path: a value with spaces must arrive as exactly one argument.
    ///
    /// The probe writes each received argument on its own line, so the
    /// assertion is on *boundaries* rather than on a count some interpreter
    /// computes for us. An earlier version counted `sys.argv` and was wrong
    /// about what it included -- the failure looked like broken quoting when
    /// the test itself was miscounting.
    #[test]
    fn run_keeps_each_entry_as_one_argument() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("args.txt");

        let mut ctx = context();
        ctx.dir = temp.path().to_path_buf();

        // A tiny script that echoes each argument on its own line.
        let script_path = if cfg!(windows) {
            let path = temp.path().join("echo.bat");
            std::fs::write(
                &path,
                "@echo off\r\n:loop\r\nif \"%~1\"==\"\" goto done\r\necho %~1>>\"%ECHO_OUT%\"\r\nshift\r\ngoto loop\r\n:done\r\n",
            )
            .unwrap();
            path
        } else {
            let path = temp.path().join("echo.sh");
            std::fs::write(
                &path,
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\" >> \"$ECHO_OUT\"; done\n",
            )
            .unwrap();
            path
        };
        ctx.env
            .insert("ECHO_OUT".into(), out.to_string_lossy().to_string());

        let script = script_path.to_string_lossy().replace('\\', "\\\\");
        let source = if cfg!(windows) {
            format!(r#"return osdk.run("cmd", "/c", "{script}", "a b c", "d")"#)
        } else {
            format!(r#"return osdk.run("sh", "{script}", "a b c", "d")"#)
        };

        eval(&source, &ctx).unwrap();

        let received = std::fs::read_to_string(&out).unwrap_or_default();
        let lines: Vec<&str> = received
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines,
            vec!["a b c", "d"],
            "argument boundaries were not preserved: {received:?}"
        );
    }

    #[test]
    fn env_lookup_prefers_the_task_environment() {
        let mut ctx = context();
        ctx.env.insert("OSDK_TEST_VAR".into(), "from-task".into());
        assert_eq!(
            eval(
                "return osdk.env('OSDK_TEST_VAR') == 'from-task' and 0 or 1",
                &ctx
            )
            .unwrap(),
            0
        );
        assert_eq!(
            eval(
                "return osdk.env('OSDK_NOT_SET_XYZ') == nil and 0 or 1",
                &ctx
            )
            .unwrap(),
            0
        );
    }

    /// The case the tier exists for: branching and loops that generate work.
    #[test]
    fn a_loop_can_generate_several_commands() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context();
        ctx.dir = temp.path().to_path_buf();

        let source = r#"
            local failures = 0
            for i = 1, 3 do
                local code = osdk.sh("exit 0")
                if code ~= 0 then failures = failures + 1 end
            end
            return failures
        "#;
        assert_eq!(eval(source, &ctx).unwrap(), 0);
    }

    #[test]
    fn exec_captures_both_streams_and_accepts_options() {
        const CHILD: &str = "OSDK_LUA_EXEC_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).unwrap();
            let cwd = std::env::current_dir().unwrap();
            println!("OUT:{input}:{}", cwd.file_name().unwrap().to_string_lossy());
            eprintln!("ERR:{}", std::env::var(CHILD).unwrap());
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        let executable = std::env::current_exe().unwrap();
        let executable = executable.to_string_lossy().replace('\\', "\\\\");
        let mut ctx = context();
        ctx.dir = temp.path().to_path_buf();
        let source = format!(
            r#"
            local result = exec {{
              "{executable}", "--exact", "tasks::script::tests::exec_captures_both_streams_and_accepts_options", "--nocapture",
              cwd = "nested",
              env = {{ {CHILD} = "from-option" }},
              stdin = "from-stdin",
            }}
            return result.success
              and result.code == 0
              and string.find(result.stdout, "OUT:from-stdin:nested", 1, true)
              and string.find(result.stderr, "ERR:from-option", 1, true)
              and not result.stdout_truncated
              and not result.stderr_truncated
              and 0 or 1
            "#
        );
        assert_eq!(eval(&source, &ctx).unwrap(), 0);
    }

    #[test]
    fn exec_check_turns_a_nonzero_status_into_an_error() {
        let error = eval(
            "return exec { command = 'exit 7', check = true }",
            &context(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("code 7"), "{error}");
    }

    #[test]
    fn exec_rejects_a_misspelled_option() {
        let error = eval("return exec { 'tool', stdn = 'oops' }", &context())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown exec option `stdn`"), "{error}");
    }

    #[test]
    fn captured_commands_honour_the_task_timeout() {
        let mut ctx = context();
        ctx.timeout = Some(Duration::from_millis(100));
        let source = if cfg!(windows) {
            "return exec { command = 'ping -n 60 127.0.0.1 >nul' }"
        } else {
            "return exec { command = 'sleep 60' }"
        };

        let error = eval(source, &ctx).unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn lua_instructions_honour_the_task_timeout() {
        let mut ctx = context();
        ctx.timeout = Some(Duration::from_millis(20));

        let error = eval("while true do end", &ctx).unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn commands_started_by_lua_honour_the_task_timeout() {
        const CHILD: &str = "OSDK_LUA_TIMEOUT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            std::thread::sleep(Duration::from_secs(5));
            return;
        }

        let executable = std::env::current_exe().unwrap();
        let executable = executable.to_string_lossy().replace('\\', "\\\\");
        let mut ctx = context();
        ctx.timeout = Some(Duration::from_millis(100));
        ctx.env.insert(CHILD.into(), "1".into());
        let source = format!(
            r#"return osdk.run("{executable}", "--exact", "tasks::script::tests::commands_started_by_lua_honour_the_task_timeout", "--nocapture")"#
        );

        let error = eval(&source, &ctx).unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
    }
}
