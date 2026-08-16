//! Helpers for running external commands (used by delegate backends like
//! rustup and corepack).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Run a command to completion, capturing stderr on failure. `env` overrides are
/// applied on top of the inherited environment.
pub fn run(
    program: &str,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd.output().map_err(|e| Error::Command {
        cmd: format!("{program} {}", args.join(" ")),
        status: format!("failed to spawn: {e}"),
        stderr: None,
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Command {
            cmd: format!("{program} {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        })
    }
}

pub fn output(
    program: &str,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> Result<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args);
    command.envs(env);
    if let Some(directory) = cwd {
        command.current_dir(directory);
    }
    let output = command.output().map_err(|error| Error::Command {
        cmd: format!("{program} {}", args.join(" ")),
        status: format!("failed to spawn: {error}"),
        stderr: None,
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::Command {
            cmd: format!("{program} {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        })
    }
}

/// Whether `program` is resolvable on PATH.
pub fn exists(program: &str) -> bool {
    which::which(program).is_ok()
}
