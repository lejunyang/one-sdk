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

use mlua::{Lua, Value};

use crate::error::{Error, Result};

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
    let sh = lua
        .create_function(move |_, command: String| {
            let mut child = shell_command(&command);
            child.current_dir(&dir);
            for (key, value) in &env {
                child.env(key, value);
            }
            let status = child.status().map_err(|error| {
                mlua::Error::external(format!("task `{task}`: cannot run shell: {error}"))
            })?;
            Ok(status.code().unwrap_or(1))
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
            let status = child.status().map_err(|error| {
                mlua::Error::external(format!("task `{task}`: cannot run `{program}`: {error}"))
            })?;
            Ok(status.code().unwrap_or(1))
        })
        .map_err(to_lua)?;
    osdk.set("run", run).map_err(to_lua)?;

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
    let exists = lua
        .create_function(|_, target: String| Ok(Path::new(&target).exists()))
        .map_err(to_lua)?;
    path.set("exists", exists).map_err(to_lua)?;
    osdk.set("path", path).map_err(to_lua)?;

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
}
