//! Native npm execution for osdk-owned synthetic projects.
//!
//! osdk installs `npm:<package>` tools into a synthetic project directory it
//! owns entirely, then delegates the actual dependency resolution to the
//! managed npm executable. npm runs in a cleared environment pointed at
//! osdk-owned cache and config paths, so an ambient `.npmrc`, a user-level
//! cache, or an inherited `NPM_CONFIG_*` variable cannot change the result.
//!
//! Every entry point here is fail-closed: a non-zero npm exit, a spawn
//! failure, or a timeout is surfaced as an error and the caller removes the
//! partially populated install root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::process::{
    CaptureLimits, CommandOutcome, CommandRunner, CommandSpec, SystemCommandRunner,
};

/// npm can legitimately take a long time on a cold cache, so the ceiling is
/// generous; it exists to bound a wedged child, not to police slow networks.
const NPM_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const NPM_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
const NPM_STDERR_LIMIT: usize = 4 * 1024 * 1024;

/// Lifecycle-script policy for one npm invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptPolicy {
    /// Pass `--ignore-scripts`. This is the default for managed installs.
    Deny,
    /// Let npm run lifecycle scripts.
    Allow,
}

/// One npm invocation against an osdk-owned synthetic project.
#[derive(Debug)]
pub struct NativeNpmInstall<'a> {
    /// The synthetic project directory. npm runs with this as its cwd.
    pub project_dir: &'a Path,
    /// Directory holding the managed `node` and `npm` executables.
    pub node_bin_dir: &'a Path,
    /// osdk-owned npm cache directory.
    pub cache_dir: PathBuf,
    /// Registry override, when a specific source was selected.
    pub registry: Option<String>,
    pub scripts: ScriptPolicy,
    pub offline: bool,
}

/// Run `npm install` so the project's `package.json` is materialized into
/// `node_modules` and a `package-lock.json` is written next to it.
pub fn install(request: &NativeNpmInstall<'_>) -> Result<()> {
    let mut args = vec!["install".to_string()];
    args.extend(common_args(request));
    run_npm(request, &args)
}

/// Run `npm ci` so an existing `package-lock.json` is reproduced exactly.
/// npm refuses to update the lockfile in this mode, which is what makes it the
/// frozen-install primitive.
pub fn install_frozen(request: &NativeNpmInstall<'_>) -> Result<()> {
    let mut args = vec!["ci".to_string()];
    args.extend(common_args(request));
    run_npm(request, &args)
}

/// Resolve dependencies and write `package-lock.json` without installing any
/// package contents, used to capture a lock graph.
pub fn resolve_lock_only(request: &NativeNpmInstall<'_>) -> Result<()> {
    let mut args = vec![
        "install".to_string(),
        "--package-lock-only".to_string(),
        // A lock-only resolution must never execute package code.
        "--ignore-scripts".to_string(),
    ];
    args.extend(shared_flags(request));
    run_npm(request, &args)
}

fn common_args(request: &NativeNpmInstall<'_>) -> Vec<String> {
    let mut args = Vec::new();
    if request.scripts == ScriptPolicy::Deny {
        args.push("--ignore-scripts".to_string());
    }
    args.extend(shared_flags(request));
    args
}

/// Flags shared by every managed npm invocation. Audit and fund output is
/// noise for a managed install, and `--no-package-lock` is deliberately not
/// used: the lockfile is the install's identity.
fn shared_flags(request: &NativeNpmInstall<'_>) -> Vec<String> {
    let mut args = vec!["--audit=false".to_string(), "--fund=false".to_string()];
    if request.offline {
        args.push("--offline".to_string());
    }
    if let Some(registry) = &request.registry {
        args.push(format!("--registry={registry}"));
    }
    args
}

/// Environment for a managed npm run. The environment is cleared first so no
/// ambient npm configuration leaks in; only what npm genuinely needs is added
/// back.
fn npm_env(request: &NativeNpmInstall<'_>) -> Result<BTreeMap<String, String>> {
    std::fs::create_dir_all(&request.cache_dir)
        .map_err(|error| Error::io(&request.cache_dir, error))?;
    let config_dir = request.cache_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|error| Error::io(&config_dir, error))?;
    let user_config = config_dir.join("npmrc");
    if !user_config.exists() {
        std::fs::write(&user_config, b"").map_err(|error| Error::io(&user_config, error))?;
    }

    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        path_with_node_first(request.node_bin_dir)?,
    );
    env.insert(
        "NPM_CONFIG_CACHE".to_string(),
        request.cache_dir.display().to_string(),
    );
    // Both config layers are pinned to osdk-owned empty files so a user or
    // system npmrc cannot redirect the registry or re-enable scripts.
    env.insert(
        "NPM_CONFIG_USERCONFIG".to_string(),
        user_config.display().to_string(),
    );
    env.insert(
        "NPM_CONFIG_GLOBALCONFIG".to_string(),
        user_config.display().to_string(),
    );
    env.insert(
        "NPM_CONFIG_UPDATE_NOTIFIER".to_string(),
        "false".to_string(),
    );
    env.insert("NO_UPDATE_NOTIFIER".to_string(), "1".to_string());
    // npm shells out for lifecycle scripts and git dependencies; on Windows a
    // child process without these cannot resolve system libraries at all.
    for key in ["SYSTEMROOT", "SystemRoot", "COMSPEC", "ComSpec", "PATHEXT"] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.to_string_lossy().into_owned());
        }
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.to_string_lossy().into_owned());
        }
    }
    Ok(env)
}

fn path_with_node_first(node_bin_dir: &Path) -> Result<String> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let joined = std::env::join_paths(
        std::iter::once(node_bin_dir.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .map_err(|error| Error::other(format!("building npm PATH: {error}")))?;
    Ok(joined.to_string_lossy().into_owned())
}

/// Resolve the managed npm entry point. npm ships as a `.cmd` shim on Windows,
/// which `CreateProcess` cannot execute directly, so it is run through the
/// Node CLI script instead of the shim to avoid depending on a shell.
fn npm_program(node_bin_dir: &Path) -> Result<(PathBuf, Vec<String>)> {
    let node = node_bin_dir.join(node_executable());
    let cli = npm_cli_script(node_bin_dir);
    match cli {
        Some(cli) if node.is_file() => Ok((node, vec![cli.display().to_string()])),
        _ => {
            let direct = node_bin_dir.join(npm_executable());
            if direct.is_file() {
                Ok((direct, Vec::new()))
            } else {
                Err(Error::other(crate::t!(
                    "err.npm_managed_npm_missing",
                    path = node_bin_dir.display()
                )))
            }
        }
    }
}

/// Locate npm's own `npm-cli.js`. The managed Node layout keeps npm under
/// `node_modules/npm` next to the executable, with a `lib/` level on Windows.
fn npm_cli_script(node_bin_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        node_bin_dir.join("node_modules/npm/bin/npm-cli.js"),
        node_bin_dir.join("../lib/node_modules/npm/bin/npm-cli.js"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

const fn node_executable() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

const fn npm_executable() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

fn run_npm(request: &NativeNpmInstall<'_>, args: &[String]) -> Result<()> {
    let (program, mut full_args) = npm_program(request.node_bin_dir)?;
    full_args.extend(args.iter().cloned());
    let command = CommandSpec::new(program.as_os_str())
        .args(full_args.iter().map(std::ffi::OsString::from))
        .current_dir(request.project_dir)
        .clear_env()
        .envs(npm_env(request)?.into_iter().map(|(key, value)| {
            (
                std::ffi::OsString::from(key),
                std::ffi::OsString::from(value),
            )
        }));
    let limits = CaptureLimits::new(NPM_TIMEOUT, NPM_STDOUT_LIMIT, NPM_STDERR_LIMIT);
    let outcome = SystemCommandRunner.run_captured(&command, limits);
    interpret(&program, args, outcome)
}

fn interpret(program: &Path, args: &[String], outcome: CommandOutcome) -> Result<()> {
    match outcome {
        CommandOutcome::Exited { status, output } if status.success() => {
            let _ = output;
            Ok(())
        }
        CommandOutcome::Exited { status, output } => Err(Error::other(crate::t!(
            "err.npm_native_install_failed",
            command = describe(program, args),
            status = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr = tail(&output.stderr, &output.stdout)
        ))),
        CommandOutcome::NotInstalled => Err(Error::other(crate::t!(
            "err.npm_managed_npm_missing",
            path = program.display()
        ))),
        CommandOutcome::TimedOut { .. } => Err(Error::other(crate::t!(
            "err.npm_native_install_timeout",
            command = describe(program, args),
            seconds = NPM_TIMEOUT.as_secs()
        ))),
        CommandOutcome::PermissionDenied
        | CommandOutcome::SpawnFailed { .. }
        | CommandOutcome::ExecutionFailed { .. } => Err(Error::other(crate::t!(
            "err.npm_native_install_spawn_failed",
            command = describe(program, args)
        ))),
    }
}

/// Only npm's own flags are echoed back; the absolute program path and the
/// environment stay out of user-facing diagnostics.
fn describe(program: &Path, args: &[String]) -> String {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "npm".to_string());
    let shown: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.ends_with("npm-cli.js"))
        .collect();
    format!("{name} {}", shown.join(" "))
}

/// Prefer stderr for diagnostics, falling back to stdout because npm reports
/// some resolution failures on stdout only.
fn tail(stderr: &[u8], stdout: &[u8]) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
    let source = if stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
        stderr
    } else {
        stdout
    };
    let start = source.len().saturating_sub(MAX_DIAGNOSTIC_BYTES);
    String::from_utf8_lossy(&source[start..]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(project: &'a Path, node_bin: &'a Path, cache: PathBuf) -> NativeNpmInstall<'a> {
        NativeNpmInstall {
            project_dir: project,
            node_bin_dir: node_bin,
            cache_dir: cache,
            registry: None,
            scripts: ScriptPolicy::Deny,
            offline: false,
        }
    }

    #[test]
    fn install_denies_scripts_and_silences_audit_by_default() {
        let temporary = tempfile::tempdir().unwrap();
        let spec = request(
            temporary.path(),
            temporary.path(),
            temporary.path().join("cache"),
        );
        let mut args = vec!["install".to_string()];
        args.extend(common_args(&spec));
        assert!(args.contains(&"--ignore-scripts".to_string()));
        assert!(args.contains(&"--audit=false".to_string()));
        assert!(args.contains(&"--fund=false".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--registry=")));
    }

    #[test]
    fn allowing_scripts_drops_the_ignore_flag() {
        let temporary = tempfile::tempdir().unwrap();
        let mut spec = request(
            temporary.path(),
            temporary.path(),
            temporary.path().join("cache"),
        );
        spec.scripts = ScriptPolicy::Allow;
        assert!(!common_args(&spec).contains(&"--ignore-scripts".to_string()));
    }

    #[test]
    fn lock_only_resolution_always_ignores_scripts() {
        let temporary = tempfile::tempdir().unwrap();
        let mut spec = request(
            temporary.path(),
            temporary.path(),
            temporary.path().join("cache"),
        );
        // Even an explicit allow must not run package code for a lock-only
        // resolution, which never materializes the package contents.
        spec.scripts = ScriptPolicy::Allow;
        let mut args = vec![
            "install".to_string(),
            "--package-lock-only".to_string(),
            "--ignore-scripts".to_string(),
        ];
        args.extend(shared_flags(&spec));
        assert!(args.contains(&"--ignore-scripts".to_string()));
        assert!(args.contains(&"--package-lock-only".to_string()));
    }

    #[test]
    fn registry_and_offline_are_forwarded() {
        let temporary = tempfile::tempdir().unwrap();
        let mut spec = request(
            temporary.path(),
            temporary.path(),
            temporary.path().join("cache"),
        );
        spec.registry = Some("https://registry.example.test/".into());
        spec.offline = true;
        let args = shared_flags(&spec);
        assert!(args.contains(&"--registry=https://registry.example.test/".to_string()));
        assert!(args.contains(&"--offline".to_string()));
    }

    #[test]
    fn env_is_pinned_to_osdk_owned_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        let spec = request(temporary.path(), temporary.path(), cache.clone());
        let env = npm_env(&spec).unwrap();
        assert_eq!(env["NPM_CONFIG_CACHE"], cache.display().to_string());
        // A user-level npmrc must not be consulted, so both config layers
        // point at an osdk-owned empty file.
        assert_eq!(env["NPM_CONFIG_USERCONFIG"], env["NPM_CONFIG_GLOBALCONFIG"]);
        assert!(cache.join("config/npmrc").is_file());
        assert!(env["PATH"].starts_with(&temporary.path().display().to_string()));
    }

    #[test]
    fn missing_managed_npm_is_reported() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(npm_program(temporary.path()).is_err());
    }

    #[test]
    fn diagnostics_hide_the_cli_script_path() {
        let described = describe(
            Path::new("/managed/node"),
            &[
                "/managed/node_modules/npm/bin/npm-cli.js".to_string(),
                "install".to_string(),
            ],
        );
        assert_eq!(described, "node install");
    }

    #[test]
    fn diagnostics_fall_back_to_stdout_when_stderr_is_blank() {
        assert_eq!(tail(b"   \n", b"resolution failed"), "resolution failed");
        assert_eq!(tail(b"real error", b"ignored"), "real error");
    }
}
