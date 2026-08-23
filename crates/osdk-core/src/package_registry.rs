//! Safe preflight selection for npm-compatible package registries.
//!
//! The planner never executes a package manager. It runs fresh, anonymous
//! probes before a registry-fetching command starts and returns the one
//! environment override the caller may apply to that single process.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Deserialize;

use crate::backend::Ctx;
use crate::config::normalize_registry_url;
use crate::error::{Error, Result};

const NPMMIRROR: &str = "https://registry.npmmirror.com/";
const NPMJS: &str = "https://registry.npmjs.org/";
const MAX_PROBE_BODY: usize = 64 * 1024;
const MAX_PROBE_REDIRECTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageManager {
    Npm,
    Pnpm,
    YarnClassic,
    YarnBerry,
    Bun,
    Deno,
}

impl fmt::Display for PackageManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::YarnClassic => "yarn-classic",
            Self::YarnBerry => "yarn-berry",
            Self::Bun => "bun",
            Self::Deno => "deno",
        })
    }
}

impl FromStr for PackageManager {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "npm" | "npx" => Ok(Self::Npm),
            "pnpm" | "pnpx" => Ok(Self::Pnpm),
            "yarn-classic" | "yarn1" | "yarn@1" => Ok(Self::YarnClassic),
            "yarn-berry" | "yarn2" | "yarn3" | "yarn4" | "yarn@2" | "yarn@3" | "yarn@4" => {
                Ok(Self::YarnBerry)
            }
            "yarn" | "yarnpkg" => Err(Error::config(
                "Yarn major is required; use yarn-classic or yarn-berry",
            )),
            "bun" | "bunx" => Ok(Self::Bun),
            "deno" => Ok(Self::Deno),
            other => Err(Error::config(format!("unknown package manager `{other}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryProbe {
    pub url: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    /// A bounded, credential-free diagnostic suitable for display.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryPlan {
    PassThrough {
        reason: String,
    },
    Selected {
        url: String,
        probes: Vec<RegistryProbe>,
    },
    Unavailable {
        probes: Vec<RegistryProbe>,
    },
}

/// Resolve an executable name to a package-manager family. Yarn is only
/// classified when its major version is known; callers must otherwise pass it
/// through unchanged.
pub fn manager_for_command(command: &str, backend_version: Option<&str>) -> Option<PackageManager> {
    let command = executable_name(command);
    match command.as_str() {
        "npm" | "npx" => Some(PackageManager::Npm),
        "pnpm" | "pnpx" => Some(PackageManager::Pnpm),
        "yarn" | "yarnpkg" => {
            let major = backend_version.and_then(parse_major)?;
            Some(if major == 1 {
                PackageManager::YarnClassic
            } else {
                PackageManager::YarnBerry
            })
        }
        "bun" | "bunx" => Some(PackageManager::Bun),
        "deno" => Some(PackageManager::Deno),
        _ => None,
    }
}

fn parse_major(value: &str) -> Option<u64> {
    value
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn executable_name(command: &str) -> String {
    let basename = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if let Some(stem) = basename.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    basename
}

/// Environment variable injected into the one package-manager process after a
/// successful plan. pnpm intentionally uses its own variable; pnpm 11 does not
/// honor npm_config_registry for this purpose.
pub fn registry_env(manager: PackageManager) -> &'static str {
    match manager {
        PackageManager::Pnpm => "pnpm_config_registry",
        PackageManager::YarnClassic => "YARN_REGISTRY",
        PackageManager::YarnBerry => "YARN_NPM_REGISTRY_SERVER",
        PackageManager::Bun => "BUN_CONFIG_REGISTRY",
        PackageManager::Deno => "NPM_CONFIG_REGISTRY",
        PackageManager::Npm => "npm_config_registry",
    }
}

/// Return whether this invocation may resolve or fetch npm packages. This is a
/// deliberately explicit allow-list so routine commands never make probes.
pub fn should_plan(manager: PackageManager, executable_alias: &str, args: &[String]) -> bool {
    let invocation = analyze_invocation(manager, executable_alias, args);
    if explicit_registry_arg(&invocation.options)
        || explicit_config_context_arg(manager, &invocation.options)
        || explicit_offline_arg(manager, invocation.command, &invocation.options)
        || leading_introspection(&invocation.options)
    {
        return false;
    }
    let executable = executable_name(executable_alias);
    let command = invocation.command;
    match manager {
        PackageManager::Npm if executable == "npx" => {
            command.is_some() || npx_fetch_form(&invocation.options)
        }
        PackageManager::Npm => matches!(
            command,
            Some("install" | "i" | "ci" | "add" | "update" | "up" | "exec")
        ),
        PackageManager::Pnpm if executable == "pnpx" => command.is_some(),
        PackageManager::Pnpm => matches!(
            command,
            Some("install" | "i" | "add" | "update" | "up" | "fetch" | "dlx" | "deploy")
        ),
        PackageManager::YarnClassic | PackageManager::YarnBerry => {
            command.is_none()
                || matches!(
                    command,
                    Some("install" | "add" | "upgrade" | "up" | "dlx" | "create")
                )
        }
        PackageManager::Bun if executable == "bunx" => command.is_some(),
        PackageManager::Bun => {
            matches!(
                command,
                Some("install" | "i" | "ci" | "add" | "update" | "x")
            )
        }
        PackageManager::Deno => matches!(
            command,
            Some(
                "add"
                    | "bench"
                    | "cache"
                    | "check"
                    | "ci"
                    | "compile"
                    | "doc"
                    | "eval"
                    | "info"
                    | "install"
                    | "outdated"
                    | "run"
                    | "serve"
                    | "task"
                    | "test"
                    | "update"
            )
        ),
    }
}

struct ManagerInvocation<'a> {
    command: Option<&'a str>,
    options: Vec<&'a str>,
}

fn is_launcher_alias(executable: &str) -> bool {
    matches!(executable, "npx" | "pnpx" | "bunx")
}

fn analyze_invocation<'a>(
    manager: PackageManager,
    executable_alias: &str,
    args: &'a [String],
) -> ManagerInvocation<'a> {
    let executable = executable_name(executable_alias);
    let command = first_command(manager, &executable, args);
    let scope = manager_option_scope(manager, &executable, args);
    let launcher_alias = is_launcher_alias(&executable);
    let command_index = if launcher_alias {
        None
    } else {
        first_positional_index(manager, &executable, None, scope, 0)
    };
    let scoped_command = command_index.map(|index| scope[index].as_str());
    let mut options = Vec::new();
    let mut index = 0;
    while index < scope.len() {
        if command_index == Some(index) {
            index += 1;
            continue;
        }
        let argument = scope[index].as_str();
        if argument.starts_with('-') {
            options.push(argument);
            if option_takes_separate_value(manager, &executable, scoped_command, argument) {
                index += 2;
                continue;
            }
        }
        index += 1;
    }
    ManagerInvocation { command, options }
}

fn first_command<'a>(
    manager: PackageManager,
    executable: &str,
    args: &'a [String],
) -> Option<&'a str> {
    let separator = args
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(args.len());
    first_positional_index(manager, executable, None, &args[..separator], 0)
        .map(|index| args[index].as_str())
        .or_else(|| {
            (separator < args.len())
                .then(|| args.get(separator + 1))
                .flatten()
                .map(String::as_str)
        })
}

fn first_positional_index(
    manager: PackageManager,
    executable: &str,
    command: Option<&str>,
    args: &[String],
    start: usize,
) -> Option<usize> {
    let mut skip_value = false;
    for (index, argument) in args.iter().enumerate().skip(start) {
        if skip_value {
            skip_value = false;
            continue;
        }
        let argument = argument.as_str();
        if option_takes_separate_value(manager, executable, command, argument) {
            skip_value = true;
        } else if !argument.starts_with('-') {
            return Some(index);
        }
    }
    None
}

fn option_takes_separate_value(
    manager: PackageManager,
    executable: &str,
    command: Option<&str>,
    argument: &str,
) -> bool {
    match manager {
        PackageManager::Npm => {
            matches!(
                argument,
                "--cache"
                    | "--config"
                    | "--globalconfig"
                    | "--prefix"
                    | "--registry"
                    | "--script-shell"
                    | "--userconfig"
                    | "--workspace"
                    | "-w"
            ) || executable == "npx"
                && matches!(
                    argument,
                    "--allow-scripts" | "--call" | "--package" | "--shell" | "-c" | "-p"
                )
        }
        PackageManager::Pnpm => {
            matches!(
                argument,
                "--cache-dir"
                    | "--config"
                    | "--config-dir"
                    | "--config-file"
                    | "--dir"
                    | "--filter"
                    | "--global-bin-dir"
                    | "--global-dir"
                    | "--globalconfig"
                    | "--prefix"
                    | "--registry"
                    | "--reporter"
                    | "--state-dir"
                    | "--store-dir"
                    | "--userconfig"
                    | "--virtual-store-dir"
                    | "-C"
                    | "-F"
            ) || (executable == "pnpx" || command == Some("dlx"))
                && matches!(argument, "--allow-build" | "--package" | "-p")
        }
        PackageManager::YarnClassic | PackageManager::YarnBerry => {
            matches!(
                argument,
                "--cache-folder"
                    | "--cwd"
                    | "--mutex"
                    | "--npm-registry-server"
                    | "--registry"
                    | "--use-yarnrc"
                    | "-C"
            ) || matches!(command, Some("dlx" | "create")) && matches!(argument, "--package" | "-p")
        }
        PackageManager::Bun => {
            matches!(
                argument,
                "--backend" | "--cache-dir" | "--config" | "--cwd" | "--linker" | "--registry"
            ) || (executable == "bunx" || command == Some("x"))
                && matches!(argument, "--package" | "-p")
        }
        PackageManager::Deno => {
            matches!(
                argument,
                "--cert"
                    | "--config"
                    | "--config-file"
                    | "--cwd"
                    | "--env-file"
                    | "--import-map"
                    | "--inspect"
                    | "--inspect-brk"
                    | "--inspect-wait"
                    | "--location"
                    | "--lock"
                    | "--log-level"
                    | "--node-modules-dir"
                    | "--seed"
                    | "--v8-flags"
                    | "--watch-exclude"
                    | "-c"
            ) || command == Some("compile")
                && matches!(
                    argument,
                    "--exclude" | "--icon" | "--include" | "--output" | "--target" | "-o"
                )
                || command == Some("eval") && argument == "--ext"
                || command == Some("serve") && matches!(argument, "--host" | "--port")
                || command == Some("task") && matches!(argument, "--filter" | "-F")
        }
    }
}

/// Return the prefix whose flags are interpreted by the package manager. A
/// literal separator always ends that prefix. A few launcher-style commands
/// also hand every argument after their executable/script operand to the
/// child, even when the separator is omitted.
fn manager_option_scope<'a>(
    manager: PackageManager,
    executable_alias: &str,
    args: &'a [String],
) -> &'a [String] {
    let separator = args
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(args.len());
    let before_separator = &args[..separator];
    let executable = executable_name(executable_alias);
    let Some(command_index) =
        first_positional_index(manager, &executable, None, before_separator, 0)
    else {
        return before_separator;
    };
    let command = before_separator[command_index].as_str();

    let target_index = if matches!(executable.as_str(), "npx" | "pnpx" | "bunx") {
        Some(command_index)
    } else if matches!(manager, PackageManager::Pnpm) && command == "dlx"
        || matches!(
            manager,
            PackageManager::YarnClassic | PackageManager::YarnBerry
        ) && matches!(command, "dlx" | "create")
        || matches!(manager, PackageManager::Bun) && command == "x"
        || matches!(manager, PackageManager::Deno)
            && matches!(command, "compile" | "eval" | "run" | "serve" | "task")
    {
        first_positional_index(
            manager,
            &executable,
            Some(command),
            before_separator,
            command_index + 1,
        )
    } else {
        None
    };

    target_index
        .map(|index| &before_separator[..=index])
        .unwrap_or(before_separator)
}

fn explicit_registry_arg(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower == "--registry"
            || lower.starts_with("--registry=")
            || lower == "--npm-registry"
            || lower.starts_with("--npm-registry=")
            || lower == "--npm-registry-server"
            || lower.starts_with("--npm-registry-server=")
    })
}

fn explicit_config_context_arg(manager: PackageManager, args: &[&str]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "--cwd"
                | "--dir"
                | "--prefix"
                | "--config"
                | "--config-file"
                | "--userconfig"
                | "--globalconfig"
                | "--use-yarnrc"
        ) || [
            "--cwd=",
            "--dir=",
            "--prefix=",
            "--config=",
            "--config-file=",
            "--userconfig=",
            "--globalconfig=",
            "--use-yarnrc=",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            || manager == PackageManager::Deno && lower == "-c"
    })
}

fn explicit_offline_arg(manager: PackageManager, command: Option<&str>, args: &[&str]) -> bool {
    if manager == PackageManager::Deno {
        let supports_cached_only = matches!(
            command,
            Some(
                "bench" | "check" | "compile" | "doc" | "eval" | "info" | "run" | "serve" | "test"
            )
        );
        return supports_cached_only
            && args
                .iter()
                .any(|arg| boolean_flag_enabled(arg, "--cached-only"));
    }
    args.iter()
        .any(|arg| boolean_flag_enabled(arg, "--offline"))
}

fn boolean_flag_enabled(argument: &str, name: &str) -> bool {
    argument == name
        || argument
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true"))
}

fn leading_introspection(args: &[&str]) -> bool {
    args.iter()
        .any(|arg| matches!(*arg, "--help" | "-h" | "--version" | "-v"))
}

fn npx_fetch_form(options: &[&str]) -> bool {
    options.iter().any(|argument| {
        matches!(*argument, "--call" | "--package" | "-c" | "-p")
            || argument.starts_with("--call=")
            || argument.starts_with("--package=")
            || argument.starts_with("-c=")
            || argument.starts_with("-p=")
    })
}

/// Build a one-shot registry plan. Each call performs fresh concurrent probes;
/// the selected URL is the first healthy candidate in configured order.
pub async fn plan<F>(
    ctx: &Ctx,
    cwd: &Path,
    manager: PackageManager,
    executable_alias: &str,
    args: &[String],
    getenv: F,
) -> Result<RegistryPlan>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    let invocation = analyze_invocation(manager, executable_alias, args);
    if !should_plan(manager, executable_alias, args) {
        let reason = if explicit_registry_arg(&invocation.options) {
            "the command has an explicit registry"
        } else if explicit_config_context_arg(manager, &invocation.options) {
            "the command selects a different working directory or native configuration"
        } else if explicit_offline_arg(manager, invocation.command, &invocation.options) {
            "the package manager was explicitly asked to use only local cache data"
        } else {
            "the command does not require registry preflight"
        };
        return Ok(RegistryPlan::PassThrough {
            reason: reason.into(),
        });
    }
    if ctx.config.settings.offline {
        return Ok(RegistryPlan::PassThrough {
            reason: "osdk is offline".into(),
        });
    }
    if let Some(name) = explicit_registry_env(manager, getenv) {
        return Ok(RegistryPlan::PassThrough {
            reason: format!("registry is explicitly configured by environment variable {name}"),
        });
    }

    let native = match native_registry_candidates(ctx, cwd, manager, getenv)? {
        NativeDecision::PassThrough(reason) => return Ok(RegistryPlan::PassThrough { reason }),
        NativeDecision::Candidates(native) => native,
    };
    let preserve_order = !native.is_empty() || !ctx.config.registries().npm.urls.is_empty();
    let candidates = effective_candidates(ctx, native)?;
    let probes = probe_all(&candidates, ctx.config.registries().npm.probe_timeout_ms).await;
    let selected = select_probe(&probes, preserve_order);
    if let Some(selected) = selected {
        Ok(RegistryPlan::Selected {
            url: selected.url.clone(),
            probes,
        })
    } else {
        Ok(RegistryPlan::Unavailable { probes })
    }
}

fn select_probe(probes: &[RegistryProbe], preserve_order: bool) -> Option<&RegistryProbe> {
    if preserve_order {
        probes.iter().find(|probe| probe.ok)
    } else {
        probes
            .iter()
            .filter(|probe| probe.ok)
            .min_by_key(|probe| probe.latency_ms.unwrap_or(u64::MAX))
    }
}

fn explicit_registry_env<F>(manager: PackageManager, getenv: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    let names: &[&str] = match manager {
        PackageManager::Pnpm => &[
            "pnpm_config_registry",
            "PNPM_CONFIG_REGISTRY",
            "npm_config_registry",
            "NPM_CONFIG_REGISTRY",
        ],
        PackageManager::YarnBerry => &[
            "YARN_NPM_REGISTRY_SERVER",
            "yarn_npm_registry_server",
            "npm_config_registry",
            "NPM_CONFIG_REGISTRY",
        ],
        PackageManager::YarnClassic => &[
            "YARN_REGISTRY",
            "yarn_registry",
            "npm_config_registry",
            "NPM_CONFIG_REGISTRY",
        ],
        PackageManager::Bun => &[
            "BUN_CONFIG_REGISTRY",
            "bun_config_registry",
            "npm_config_registry",
            "NPM_CONFIG_REGISTRY",
        ],
        PackageManager::Deno => &["NPM_CONFIG_REGISTRY", "npm_config_registry"],
        _ => &["npm_config_registry", "NPM_CONFIG_REGISTRY"],
    };
    names
        .iter()
        .copied()
        .find(|name| getenv(name).is_some_and(|value| !value.trim().is_empty()))
}

#[derive(Debug)]
enum NativeDecision {
    PassThrough(String),
    Candidates(Vec<String>),
}

fn effective_candidates(ctx: &Ctx, native: Vec<String>) -> Result<Vec<String>> {
    let configured = &ctx.config.registries().npm.urls;
    let values: Vec<String> = if !configured.is_empty() {
        configured.clone()
    } else {
        native
            .into_iter()
            .chain([NPMMIRROR.to_string(), NPMJS.to_string()])
            .collect()
    };
    normalize_candidates(values)
}

fn normalize_candidates(values: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for value in values {
        let value = normalize_registry_url(&value)?;
        if seen.insert(value.clone()) {
            output.push(value);
        }
    }
    Ok(output)
}

fn native_registry_candidates<F>(
    ctx: &Ctx,
    cwd: &Path,
    manager: PackageManager,
    getenv: F,
) -> Result<NativeDecision>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    let mut files = Vec::new();
    if let Some(name) = uncertain_native_config_path_env(getenv) {
        return Ok(NativeDecision::PassThrough(format!(
            "cannot safely resolve native registry configuration selected by environment variable {name}"
        )));
    }
    match manager {
        PackageManager::Npm | PackageManager::Pnpm | PackageManager::YarnClassic => {
            if let Err(path) = push_nearest(cwd, ".npmrc", &mut files) {
                return Ok(unreadable_native_config(path));
            }
            if manager == PackageManager::YarnClassic {
                if let Err(path) = push_nearest(cwd, ".yarnrc", &mut files) {
                    return Ok(unreadable_native_config(path));
                }
            }
        }
        PackageManager::YarnBerry => {
            if let Err(path) = push_nearest(cwd, ".yarnrc.yml", &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
        PackageManager::Bun => {
            if let Err(path) = push_nearest(cwd, "bunfig.toml", &mut files) {
                return Ok(unreadable_native_config(path));
            }
            if let Err(path) = push_nearest(cwd, ".npmrc", &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
        PackageManager::Deno => {
            if let Err(path) = push_nearest(cwd, ".npmrc", &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
    }
    let reads_npm_config = matches!(
        manager,
        PackageManager::Npm
            | PackageManager::Pnpm
            | PackageManager::YarnClassic
            | PackageManager::Bun
            | PackageManager::Deno
    );
    if reads_npm_config {
        for name in ["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"] {
            if let Some(path) = env_path(cwd, name, getenv) {
                if let Err(path) = push_if_present(path, &mut files) {
                    return Ok(unreadable_native_config(path));
                }
            }
        }
    }
    for home in native_home_directories(cwd, getenv, cfg!(windows)) {
        if reads_npm_config {
            if let Err(path) = push_if_present(home.join(".npmrc"), &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
        if matches!(
            manager,
            PackageManager::YarnClassic | PackageManager::YarnBerry
        ) {
            for path in [home.join(".yarnrc"), home.join(".yarnrc.yml")] {
                if let Err(path) = push_if_present(path, &mut files) {
                    return Ok(unreadable_native_config(path));
                }
            }
        }
        if manager == PackageManager::Bun {
            for path in [
                home.join(".bunfig.toml"),
                home.join(".config/bun/bunfig.toml"),
            ] {
                if let Err(path) = push_if_present(path, &mut files) {
                    return Ok(unreadable_native_config(path));
                }
            }
        }
    }
    if reads_npm_config {
        let mut explicit_global_config = false;
        for name in ["NPM_CONFIG_GLOBALCONFIG", "npm_config_globalconfig"] {
            if let Some(path) = env_path(cwd, name, getenv) {
                explicit_global_config = true;
                if let Err(path) = push_if_present(path, &mut files) {
                    return Ok(unreadable_native_config(path));
                }
            }
        }
        if !explicit_global_config {
            let mut explicit_prefix = false;
            for name in ["NPM_CONFIG_PREFIX", "npm_config_prefix"] {
                if let Some(prefix) = env_path(cwd, name, getenv) {
                    explicit_prefix = true;
                    if let Err(path) = push_if_present(prefix.join("etc/npmrc"), &mut files) {
                        return Ok(unreadable_native_config(path));
                    }
                }
            }
            if !explicit_prefix {
                if let Some(prefix) = env_path(cwd, "PREFIX", getenv) {
                    if let Err(path) = push_if_present(prefix.join("etc/npmrc"), &mut files) {
                        return Ok(unreadable_native_config(path));
                    }
                }
            }
        }
        for path in managed_npm_config_files(ctx) {
            if let Err(path) = push_if_present(path, &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
    }
    if manager == PackageManager::Bun {
        if let Some(config_home) =
            getenv("XDG_CONFIG_HOME").filter(|value| !value.trim().is_empty())
        {
            let config_home = resolve_from(cwd, config_home);
            if let Err(path) = push_if_present(config_home.join("bun/bunfig.toml"), &mut files) {
                return Ok(unreadable_native_config(path));
            }
        }
    }

    if let Some(name) = global_auth_env(getenv) {
        return Ok(NativeDecision::PassThrough(format!(
            "global registry authentication is configured by environment variable {name}"
        )));
    }
    if let Some(name) = global_tls_policy_env(getenv) {
        return Ok(NativeDecision::PassThrough(format!(
            "global registry TLS policy is configured by environment variable {name}"
        )));
    }
    if let Some(name) = global_native_proxy_env(getenv) {
        return Ok(NativeDecision::PassThrough(format!(
            "native registry proxy is configured by environment variable {name}"
        )));
    }

    let mut public = Vec::new();
    for file in files {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(_) => {
                return Ok(NativeDecision::PassThrough(format!(
                    "cannot safely read native registry configuration {}",
                    file.display()
                )));
            }
        };
        let analysis = match analyze_native_file(&file, &text) {
            Ok(analysis) => analysis,
            Err(_) => {
                return Ok(NativeDecision::PassThrough(format!(
                    "cannot safely parse native registry configuration {}",
                    file.display()
                )));
            }
        };
        if analysis.auth {
            return Ok(NativeDecision::PassThrough(format!(
                "native registry authentication is configured in {}",
                file.display()
            )));
        }
        if analysis.tls_policy {
            return Ok(NativeDecision::PassThrough(format!(
                "native registry TLS policy is configured in {}",
                file.display()
            )));
        }
        if analysis.config_location {
            return Ok(NativeDecision::PassThrough(format!(
                "native registry configuration selects another config location in {}",
                file.display()
            )));
        }
        if analysis.proxy {
            return Ok(NativeDecision::PassThrough(format!(
                "native registry proxy is configured in {}",
                file.display()
            )));
        }
        if analysis.scoped_registry {
            return Ok(NativeDecision::PassThrough(format!(
                "a scoped registry is configured in {}",
                file.display()
            )));
        }
        if analysis.registry.is_none() {
            continue;
        }
        if let Some(registry) = analysis.registry {
            let parsed = match reqwest::Url::parse(registry.trim()) {
                Ok(parsed) => parsed,
                Err(_) => {
                    return Ok(NativeDecision::PassThrough(format!(
                        "private or unknown native registry is configured in {}",
                        file.display()
                    )));
                }
            };
            if !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Ok(NativeDecision::PassThrough(format!(
                    "native registry authentication is configured in {}",
                    file.display()
                )));
            }
            let Ok(normalized) = normalize_registry_url(&registry) else {
                return Ok(NativeDecision::PassThrough(format!(
                    "private or unknown native registry is configured in {}",
                    file.display()
                )));
            };
            if !is_known_public_registry(&normalized) {
                return Ok(NativeDecision::PassThrough(format!(
                    "private or unknown native registry is configured in {}",
                    file.display()
                )));
            }
            public.push(normalized);
        }
    }
    if let Some(primary) = public.first().cloned() {
        return Ok(NativeDecision::Candidates(vec![primary]));
    }
    Ok(NativeDecision::Candidates(Vec::new()))
}

fn managed_npm_config_files(ctx: &Ctx) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(ctx.dirs.installs.join("node")) else {
        return files;
    };
    for entry in entries.flatten() {
        let root = entry.path();
        if !root.is_dir() || !root.join(".osdk-complete").is_file() {
            continue;
        }
        let prefix = if ctx.platform.os == crate::platform::Os::Windows {
            root.clone()
        } else {
            // Node's POSIX executable is <root>/bin/node, so npm derives the
            // global prefix as dirname(dirname(execPath)) == <root>.
            root.clone()
        };
        push_unique(prefix.join("etc/npmrc"), &mut files);
        for path in [
            root.join("lib/node_modules/npm/npmrc"),
            root.join("lib/node_modules/npm/.npmrc"),
            root.join("node_modules/npm/npmrc"),
            root.join("node_modules/npm/.npmrc"),
        ] {
            push_unique(path, &mut files);
        }
    }
    files
}

fn unreadable_native_config(path: PathBuf) -> NativeDecision {
    NativeDecision::PassThrough(format!(
        "cannot safely read native registry configuration {}",
        path.display()
    ))
}

fn push_nearest(
    cwd: &Path,
    name: &str,
    files: &mut Vec<PathBuf>,
) -> std::result::Result<(), PathBuf> {
    for directory in cwd.ancestors() {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                push_unique(path, files);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(path),
        }
    }
    Ok(())
}

fn push_if_present(path: PathBuf, files: &mut Vec<PathBuf>) -> std::result::Result<(), PathBuf> {
    match std::fs::symlink_metadata(&path) {
        Ok(_) => push_unique(path, files),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(path),
    }
    Ok(())
}

fn push_unique(path: PathBuf, files: &mut Vec<PathBuf>) {
    if !files.contains(&path) {
        files.push(path);
    }
}

fn env_path<F>(cwd: &Path, name: &str, getenv: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    getenv(name)
        .filter(|value| !value.trim().is_empty())
        .map(|value| resolve_from(cwd, value))
}

fn uncertain_native_config_path_env<F>(getenv: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    [
        "NPM_CONFIG_USERCONFIG",
        "npm_config_userconfig",
        "NPM_CONFIG_GLOBALCONFIG",
        "npm_config_globalconfig",
        "NPM_CONFIG_PREFIX",
        "npm_config_prefix",
        "PREFIX",
        "XDG_CONFIG_HOME",
    ]
    .into_iter()
    .find(|name| {
        getenv(name).is_some_and(|value| {
            let value = value.trim();
            value.contains("${") || value.starts_with("~/") || value.starts_with("~\\")
        })
    })
}

fn resolve_from(cwd: &Path, value: String) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() || is_windows_absolute(&path) {
        path
    } else {
        cwd.join(path)
    }
}

fn native_home_directories<F>(cwd: &Path, getenv: F, windows: bool) -> Vec<PathBuf>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    let names = if windows {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    let mut homes = Vec::new();
    for name in names {
        if let Some(value) = getenv(name).filter(|value| !value.trim().is_empty()) {
            let path = PathBuf::from(value);
            let path = if path.is_absolute() || (windows && is_windows_absolute(&path)) {
                path
            } else {
                cwd.join(path)
            };
            if !homes.contains(&path) {
                homes.push(path);
            }
        }
    }
    homes
}

fn is_windows_absolute(path: &Path) -> bool {
    let value = path.to_string_lossy().as_bytes().to_vec();
    value.starts_with(b"\\\\")
        || value.starts_with(b"//")
        || (value.len() >= 3
            && value[0].is_ascii_alphabetic()
            && value[1] == b':'
            && matches!(value[2], b'/' | b'\\'))
}

#[derive(Default)]
struct NativeAnalysis {
    registry: Option<String>,
    auth: bool,
    tls_policy: bool,
    scoped_registry: bool,
    config_location: bool,
    proxy: bool,
}

fn analyze_native_file(path: &Path, text: &str) -> Result<NativeAnalysis> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".yarnrc.yml") => analyze_yarn_yaml(text),
        Some("bunfig.toml") | Some(".bunfig.toml") => analyze_bun_toml(text),
        Some(".yarnrc") => Ok(analyze_yarn_classic(text)),
        _ => Ok(analyze_npmrc(text)),
    }
}

fn analyze_npmrc(text: &str) -> NativeAnalysis {
    let mut analysis = NativeAnalysis::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key_lower = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if auth_key(&key_lower) {
            // A single half of an mTLS pair can be completed by another npmrc
            // layer. Treat every non-empty identity value as authentication.
            analysis.auth |= !value.is_empty()
                && (!key_lower.ends_with("always-auth") || !value.eq_ignore_ascii_case("false"));
        } else if tls_policy_key(&key_lower) {
            // CA and certificate-validation settings are not client identity,
            // but osdk's anonymous probe cannot faithfully reproduce them.
            analysis.tls_policy = true;
        } else if native_proxy_key(&key_lower) {
            analysis.proxy = true;
        } else if matches!(key_lower.as_str(), "prefix" | "globalconfig" | "userconfig") {
            // npm can use these values to load another npmrc. Avoid claiming
            // the native configuration is fully inspected when it is not.
            analysis.config_location = true;
        } else if key_lower == "registry" {
            analysis.registry = Some(value.trim().trim_matches(&['"', '\''][..]).into());
        } else if key_lower.starts_with('@') && key_lower.ends_with(":registry") {
            analysis.scoped_registry = true;
        }
    }
    analysis
}

fn analyze_yarn_classic(text: &str) -> NativeAnalysis {
    let mut analysis = NativeAnalysis::default();
    for raw in text.lines() {
        let line = raw.trim();
        let mut pieces = line.split_whitespace();
        let key = pieces.next().unwrap_or("").trim_matches(&['"', '\''][..]);
        let value = pieces.next().unwrap_or("").trim_matches(&['"', '\''][..]);
        if auth_key(&key.to_ascii_lowercase()) && !value.is_empty() {
            analysis.auth = true;
        }
        if tls_policy_key(&key.to_ascii_lowercase()) {
            analysis.tls_policy = true;
        }
        if native_proxy_key(&key.to_ascii_lowercase()) {
            analysis.proxy = true;
        }
        if key.starts_with('@') && key.ends_with(":registry") {
            analysis.scoped_registry = true;
        }
        if let Some(value) = line
            .strip_prefix("registry ")
            .or_else(|| line.strip_prefix("--registry "))
        {
            analysis.registry = Some(value.trim().trim_matches(&['"', '\''][..]).into());
        }
    }
    analysis
}

fn analyze_yarn_yaml(text: &str) -> Result<NativeAnalysis> {
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|error| crate::error::Error::config(format!("invalid Yarn YAML: {error}")))?;
    analyze_yarn_yaml_value(&value)
}

fn analyze_yarn_yaml_value(value: &serde_yaml::Value) -> Result<NativeAnalysis> {
    let mut analysis = NativeAnalysis::default();
    scan_yarn_yaml_security(value, &mut analysis)?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| crate::error::Error::config("Yarn configuration must be a YAML mapping"))?;
    for (key, value) in mapping {
        let key = key.as_str().ok_or_else(|| {
            crate::error::Error::config("Yarn configuration contains a non-string key")
        })?;
        let lower = key.to_ascii_lowercase();
        if lower == "npmregistryserver" {
            analysis.registry = Some(
                value
                    .as_str()
                    .ok_or_else(|| {
                        crate::error::Error::config("Yarn npmRegistryServer must be a string")
                    })?
                    .to_owned(),
            );
        }
    }
    Ok(analysis)
}

fn scan_yarn_yaml_security(value: &serde_yaml::Value, analysis: &mut NativeAnalysis) -> Result<()> {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            for (key, value) in mapping {
                let key = key.as_str().ok_or_else(|| {
                    crate::error::Error::config("Yarn configuration contains a non-string key")
                })?;
                match key.to_ascii_lowercase().as_str() {
                    "npmauthtoken" | "npmauthident" | "npmauthalways" | "httpscertfilepath"
                    | "httpskeyfilepath" => analysis.auth = true,
                    "httpscafilepath" | "enablestrictssl" => analysis.tls_policy = true,
                    "httpproxy" | "httpsproxy" => analysis.proxy = true,
                    "npmscopes" | "npmregistries" => analysis.scoped_registry = true,
                    _ => {}
                }
                scan_yarn_yaml_security(value, analysis)?;
            }
        }
        serde_yaml::Value::Sequence(values) => {
            for value in values {
                scan_yarn_yaml_security(value, analysis)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => {
            scan_yarn_yaml_security(&tagged.value, analysis)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
fn analyze_yaml_conservative(text: &str) -> NativeAnalysis {
    analyze_yarn_yaml(text).expect("test Yarn YAML should parse")
}

fn analyze_bun_toml(text: &str) -> Result<NativeAnalysis> {
    let value: toml::Value = toml::from_str(text)?;
    let Some(install) = value.get("install").and_then(toml::Value::as_table) else {
        return Ok(NativeAnalysis::default());
    };
    let mut analysis = NativeAnalysis::default();
    if let Some(registry) = install.get("registry") {
        match registry {
            toml::Value::String(value) => analysis.registry = Some(value.clone()),
            toml::Value::Table(table) => {
                analysis.registry = table
                    .get("url")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned);
                analysis.auth = table.keys().any(|key| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "token" | "username" | "password" | "cert" | "key" | "certfile" | "keyfile"
                    )
                });
                analysis.tls_policy = table.keys().any(|key| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "ca" | "cafile" | "strict-ssl" | "strictssl"
                    )
                });
            }
            _ => {
                analysis.auth = true;
            }
        }
    }
    if install.contains_key("scopes") {
        analysis.scoped_registry = true;
    }
    analysis.proxy |= [
        "proxy",
        "httpProxy",
        "httpsProxy",
        "http_proxy",
        "https_proxy",
    ]
    .into_iter()
    .any(|key| install.contains_key(key));
    Ok(analysis)
}

fn auth_key(key: &str) -> bool {
    key == "_auth"
        || key == "_authtoken"
        || key == "username"
        || key == "password"
        || key == "_password"
        || key == "always-auth"
        || key == "cert"
        || key == "key"
        || key == "certfile"
        || key == "keyfile"
        || key.ends_with(":_auth")
        || key.ends_with(":_authtoken")
        || key.ends_with(":_password")
        || key.ends_with(":username")
        || key.ends_with(":password")
        || key.ends_with(":always-auth")
        || key.ends_with(":cert")
        || key.ends_with(":key")
        || key.ends_with(":certfile")
        || key.ends_with(":keyfile")
}

fn tls_policy_key(key: &str) -> bool {
    matches!(key, "ca" | "cafile" | "strict-ssl")
        || key.ends_with(":ca")
        || key.ends_with(":cafile")
        || key.ends_with(":strict-ssl")
}

fn native_proxy_key(key: &str) -> bool {
    matches!(
        key,
        "proxy" | "https-proxy" | "http-proxy" | "noproxy" | "no-proxy"
    )
}

fn global_auth_env<F>(getenv: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    [
        "NODE_AUTH_TOKEN",
        "NPM_TOKEN",
        "YARN_NPM_AUTH_TOKEN",
        "YARN_NPM_AUTH_IDENT",
        "NPM_CONFIG__AUTH",
        "npm_config__auth",
        "NPM_CONFIG__AUTHTOKEN",
        "npm_config__authToken",
        "npm_config__authtoken",
        "NPM_CONFIG__AUTH_TOKEN",
        "npm_config__auth_token",
        "NPM_CONFIG_CERT",
        "npm_config_cert",
        "NPM_CONFIG_KEY",
        "npm_config_key",
        "NPM_CONFIG_CERTFILE",
        "npm_config_certfile",
        "NPM_CONFIG_KEYFILE",
        "npm_config_keyfile",
        "YARN_HTTPS_CERT_FILE_PATH",
        "YARN_HTTPS_KEY_FILE_PATH",
        "NPM_AUTH_TOKEN",
        "npm_auth_token",
        "YARN_AUTH_TOKEN",
        "BUN_CONFIG_TOKEN",
    ]
    .into_iter()
    .find(|name| getenv(name).is_some_and(|value| !value.trim().is_empty()))
}

fn global_tls_policy_env<F>(getenv: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    [
        "NPM_CONFIG_CA",
        "npm_config_ca",
        "NPM_CONFIG_CAFILE",
        "npm_config_cafile",
        "NPM_CONFIG_STRICT_SSL",
        "npm_config_strict_ssl",
        "NODE_EXTRA_CA_CERTS",
        "NODE_TLS_REJECT_UNAUTHORIZED",
        "YARN_HTTPS_CA_FILE_PATH",
        "YARN_ENABLE_STRICT_SSL",
    ]
    .into_iter()
    .find(|name| getenv(name).is_some_and(|value| !value.trim().is_empty()))
}

fn global_native_proxy_env<F>(getenv: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    [
        "NPM_CONFIG_PROXY",
        "npm_config_proxy",
        "NPM_CONFIG_HTTPS_PROXY",
        "npm_config_https_proxy",
        "NPM_CONFIG_HTTP_PROXY",
        "npm_config_http_proxy",
        "YARN_HTTP_PROXY",
        "YARN_HTTPS_PROXY",
        "BUN_CONFIG_PROXY",
        "BUN_CONFIG_HTTPS_PROXY",
    ]
    .into_iter()
    .find(|name| getenv(name).is_some_and(|value| !value.trim().is_empty()))
}

fn is_known_public_registry(value: &str) -> bool {
    same_registry(value, NPMJS) || same_registry(value, NPMMIRROR)
}

fn same_registry(left: &str, right: &str) -> bool {
    normalize_registry_url(left).ok() == normalize_registry_url(right).ok()
}

async fn probe_all(candidates: &[String], timeout_ms: u64) -> Vec<RegistryProbe> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let client = match reqwest::Client::builder()
        .user_agent(concat!(
            "osdk/",
            env!("CARGO_PKG_VERSION"),
            " registry-probe"
        ))
        .redirect(registry_probe_redirect_policy())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return candidates
                .iter()
                .map(|url| RegistryProbe {
                    url: url.clone(),
                    ok: false,
                    latency_ms: None,
                    error: Some(format!("client error: {error}")),
                })
                .collect();
        }
    };
    let futures = candidates.iter().cloned().map(|url| {
        let client = client.clone();
        async move { probe_one(&client, url, timeout).await }
    });
    futures_util::future::join_all(futures).await
}

fn registry_probe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        match validate_registry_probe_redirect(attempt.url(), attempt.previous()) {
            Ok(()) => attempt.follow(),
            Err(error) => attempt.error(error),
        }
    })
}

/// Registry probes are anonymous, but following an attacker-controlled
/// redirect could still turn them into network-reachability probes. Requiring
/// the exact original HTTPS origin on every hop prevents redirects to local or
/// internal services, redirects through another public host, and HTTPS
/// downgrades without having to trust DNS-based address classification.
fn validate_registry_probe_redirect(
    next: &reqwest::Url,
    previous: &[reqwest::Url],
) -> std::result::Result<(), &'static str> {
    let Some(initial) = previous.first() else {
        return Err("registry probe redirect has no origin");
    };
    if previous.len() >= MAX_PROBE_REDIRECTS {
        return Err("registry probe redirect limit exceeded");
    }
    if initial.scheme() != "https" || next.scheme() != "https" {
        return Err("registry probe redirects must remain on HTTPS");
    }
    if !next.username().is_empty() || next.password().is_some() {
        return Err("registry probe redirect must not contain credentials");
    }
    if initial.host_str() != next.host_str()
        || initial.port_or_known_default() != next.port_or_known_default()
    {
        return Err("registry probe redirect must remain on the original origin");
    }
    if previous.iter().any(|url| url == next) {
        return Err("registry probe redirect loop detected");
    }
    Ok(())
}

#[derive(Deserialize)]
struct NpmMetadata {
    name: String,
    version: String,
}

async fn probe_one(client: &reqwest::Client, base: String, timeout: Duration) -> RegistryProbe {
    let started = Instant::now();
    let endpoint = format!("{}npm/latest", base.trim_end_matches('/').to_owned() + "/");
    let result = tokio::time::timeout(timeout, async {
        let response = client.get(&endpoint).send().await.map_err(probe_error)?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(probe_error)?;
            if body.len().saturating_add(chunk.len()) > MAX_PROBE_BODY {
                return Err(format!("response exceeds {MAX_PROBE_BODY} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err("empty response".into());
        }
        let metadata: NpmMetadata =
            serde_json::from_slice(&body).map_err(|_| "invalid npm metadata JSON".to_string())?;
        if metadata.name != "npm" || metadata.version.trim().is_empty() {
            return Err("npm metadata has an unexpected name or missing version".into());
        }
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => RegistryProbe {
            url: base,
            ok: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(Err(error)) => RegistryProbe {
            url: base,
            ok: false,
            latency_ms: None,
            error: Some(error),
        },
        Err(_) => RegistryProbe {
            url: base,
            ok: false,
            latency_ms: None,
            error: Some(format!("timed out after {} ms", timeout.as_millis())),
        },
    }
}

fn probe_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".into()
    } else if error.is_connect() {
        "connection failed".into()
    } else if error.is_body() || error.is_decode() {
        "invalid response body".into()
    } else {
        "request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, COOKIE};

    use crate::config::{Config, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::store::Cas;

    #[test]
    fn manager_detection_and_registry_environment_are_version_aware() {
        assert_eq!(
            manager_for_command("C:\\tools\\yarn.cmd", Some("1.22.22")),
            Some(PackageManager::YarnClassic)
        );
        assert_eq!(
            manager_for_command("/tools/yarn", Some("4.9.2")),
            Some(PackageManager::YarnBerry)
        );
        assert_eq!(manager_for_command("yarn", None), None);
        assert_eq!(registry_env(PackageManager::Npm), "npm_config_registry");
        assert_eq!(registry_env(PackageManager::Pnpm), "pnpm_config_registry");
        assert_eq!(registry_env(PackageManager::YarnClassic), "YARN_REGISTRY");
        assert_eq!(
            registry_env(PackageManager::YarnBerry),
            "YARN_NPM_REGISTRY_SERVER"
        );
        assert_eq!(registry_env(PackageManager::Bun), "BUN_CONFIG_REGISTRY");
        assert_eq!(registry_env(PackageManager::Deno), "NPM_CONFIG_REGISTRY");
        assert!("yarn".parse::<PackageManager>().is_err());
    }

    #[test]
    fn command_filter_only_plans_registry_fetching_invocations() {
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<_>>()
        };
        assert!(should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["install"])
        ));
        assert!(should_plan(
            PackageManager::Npm,
            "npx",
            &strings(&["eslint"])
        ));
        assert!(!should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["run", "test"])
        ));
        assert!(!should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["--version"])
        ));
        assert!(!should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["install", "--registry=https://private.test"])
        ));
        assert!(!should_plan(
            PackageManager::YarnBerry,
            "yarn",
            &strings(&["install", "--npm-registry-server=https://private.test"])
        ));
        assert!(!should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["--dir", "elsewhere", "install"])
        ));
        assert!(!should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["install", "--offline"])
        ));
        assert!(should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["install", "--offline=false"])
        ));
        assert!(should_plan(
            PackageManager::Npm,
            "npm",
            &strings(&["install", "--prefer-offline"])
        ));
        assert!(should_plan(
            PackageManager::YarnBerry,
            "yarn",
            &strings(&["install", "--immutable-cache"])
        ));
        assert!(should_plan(PackageManager::YarnBerry, "yarn", &[]));
        assert!(should_plan(
            PackageManager::YarnBerry,
            "yarn",
            &strings(&["--immutable-cache"])
        ));
        assert!(should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["install", "--prefer-offline"])
        ));
        for args in [
            vec!["--package", "foo", "-c", "foo --version"],
            vec!["--package=foo", "--call=foo --help"],
        ] {
            assert!(
                should_plan(PackageManager::Npm, "npx", &strings(&args)),
                "npx package/call forms fetch even without a positional command"
            );
        }
        assert!(should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["--reporter", "ndjson", "dlx", "foo"])
        ));
        assert!(!should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["--reporter", "ndjson", "dlx", "--offline", "foo"])
        ));
        assert!(!should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["dlx", "--allow-build", "esbuild", "--offline", "foo"])
        ));
        assert!(should_plan(
            PackageManager::Pnpm,
            "pnpm",
            &strings(&["dlx", "--allow-build", "esbuild", "foo", "--offline"])
        ));
        assert!(!should_plan(
            PackageManager::Npm,
            "npx",
            &strings(&["--allow-scripts", "foo", "--offline", "bar"])
        ));
        assert!(should_plan(
            PackageManager::Npm,
            "npx",
            &strings(&["--allow-scripts", "foo", "bar", "--offline"])
        ));
        assert!(should_plan(
            PackageManager::Deno,
            "deno",
            &strings(&["eval", "-p", "import('npm:foo')", "--offline"])
        ));
        assert!(should_plan(PackageManager::Bun, "bun", &strings(&["ci"])));
        for command in ["ci", "outdated", "update"] {
            assert!(
                should_plan(
                    PackageManager::Deno,
                    "deno",
                    &strings(&[command, "--cached-only"])
                ),
                "deno {command} does not support --cached-only"
            );
        }
        assert!(should_plan(
            PackageManager::Deno,
            "deno",
            &strings(&["run", "--offline", "npm:foo"])
        ));
        assert!(!should_plan(
            PackageManager::Deno,
            "deno",
            &strings(&["-c", "deno.json", "eval", "import('npm:foo')"])
        ));
        for (manager, executable, args) in [
            (PackageManager::Npm, "npx", vec!["foo", "--", "--version"]),
            (
                PackageManager::Pnpm,
                "pnpm",
                vec!["dlx", "foo", "--", "--help"],
            ),
            (PackageManager::Bun, "bunx", vec!["foo", "--", "--version"]),
            (
                PackageManager::Deno,
                "deno",
                vec!["run", "npm:foo", "--", "--offline"],
            ),
        ] {
            assert!(
                should_plan(manager, executable, &strings(&args)),
                "child arguments after `--` must not suppress {executable} preflight"
            );
        }
        for (manager, executable, args) in [
            (PackageManager::Npm, "npx", vec!["foo", "--version"]),
            (PackageManager::Pnpm, "pnpm", vec!["dlx", "foo", "--help"]),
            (PackageManager::Bun, "bunx", vec!["foo", "--version"]),
            (
                PackageManager::Deno,
                "deno",
                vec!["run", "npm:foo", "--offline"],
            ),
        ] {
            assert!(
                should_plan(manager, executable, &strings(&args)),
                "child flags without a separator must not suppress {executable} preflight"
            );
        }
        for (manager, executable, args) in [
            (PackageManager::Npm, "npx", vec!["--offline", "foo"]),
            (
                PackageManager::Pnpm,
                "pnpm",
                vec!["dlx", "--offline", "foo"],
            ),
            (PackageManager::Bun, "bunx", vec!["--help", "foo"]),
            (
                PackageManager::Deno,
                "deno",
                vec!["run", "--cached-only", "npm:foo"],
            ),
        ] {
            assert!(
                !should_plan(manager, executable, &strings(&args)),
                "manager-owned flags must still suppress {executable} preflight"
            );
        }
        for command in [
            "add", "bench", "cache", "check", "ci", "compile", "doc", "eval", "info", "install",
            "outdated", "run", "serve", "task", "test", "update",
        ] {
            assert!(
                should_plan(PackageManager::Deno, "deno", &strings(&[command])),
                "deno {command} may fetch npm packages"
            );
        }
    }

    #[test]
    fn builtin_selection_uses_latency_but_declared_selection_uses_order() {
        let probes = vec![
            RegistryProbe {
                url: "primary".into(),
                ok: true,
                latency_ms: Some(80),
                error: None,
            },
            RegistryProbe {
                url: "fast".into(),
                ok: true,
                latency_ms: Some(5),
                error: None,
            },
        ];
        assert_eq!(select_probe(&probes, true).unwrap().url, "primary");
        assert_eq!(select_probe(&probes, false).unwrap().url, "fast");
    }

    #[test]
    fn explicit_registry_candidates_override_native_public_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path(), Vec::new(), false);
        let candidates = effective_candidates(&ctx, vec![NPMMIRROR.into()]).unwrap();
        assert_eq!(candidates, [NPMMIRROR, NPMJS]);

        let candidates = effective_candidates(&ctx, vec![NPMJS.into()]).unwrap();
        assert_eq!(candidates, [NPMJS, NPMMIRROR]);

        let ctx = test_ctx(
            temp.path(),
            vec![
                "https://registry.example.test/".into(),
                "https://registry.backup.test/".into(),
            ],
            false,
        );
        let candidates = effective_candidates(&ctx, vec![NPMJS.into()]).unwrap();
        assert_eq!(
            candidates,
            [
                "https://registry.example.test/",
                "https://registry.backup.test/"
            ]
        );
    }

    #[test]
    fn registry_probe_redirects_require_the_original_https_origin() {
        let initial = reqwest::Url::parse("https://registry.example/npm/latest").unwrap();
        let same_origin =
            reqwest::Url::parse("https://registry.example:443/metadata/npm?source=probe").unwrap();
        assert_eq!(
            validate_registry_probe_redirect(&same_origin, std::slice::from_ref(&initial)),
            Ok(())
        );

        let unsafe_targets = [
            (
                "http://registry.example/npm/latest",
                "registry probe redirects must remain on HTTPS",
            ),
            (
                "https://127.0.0.1/npm/latest",
                "registry probe redirect must remain on the original origin",
            ),
            (
                "https://10.0.0.1/npm/latest",
                "registry probe redirect must remain on the original origin",
            ),
            (
                "https://169.254.169.254/latest/meta-data",
                "registry probe redirect must remain on the original origin",
            ),
            (
                "https://metadata.internal/latest",
                "registry probe redirect must remain on the original origin",
            ),
            (
                "https://registry.example:444/npm/latest",
                "registry probe redirect must remain on the original origin",
            ),
        ];
        for (target, expected) in unsafe_targets {
            let target = reqwest::Url::parse(target).unwrap();
            assert_eq!(
                validate_registry_probe_redirect(&target, std::slice::from_ref(&initial)),
                Err(expected),
                "target {target}"
            );
        }
    }

    #[test]
    fn registry_probe_redirects_reject_loops_and_enforce_the_hop_limit() {
        let urls = (0..=4)
            .map(|index| {
                reqwest::Url::parse(&format!("https://registry.example/redirect/{index}")).unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            validate_registry_probe_redirect(&urls[2], &urls[..2]),
            Ok(()),
            "the third URL in the redirect chain is allowed"
        );
        assert_eq!(
            validate_registry_probe_redirect(&urls[3], &urls[..3]),
            Err("registry probe redirect limit exceeded")
        );
        assert_eq!(
            validate_registry_probe_redirect(&urls[1], &urls[..2]),
            Err("registry probe redirect loop detected")
        );
    }

    #[test]
    fn scoped_registry_and_host_credentials_are_detected_conservatively() {
        let scoped = analyze_npmrc("@private:registry=https://packages.test/\n");
        assert!(scoped.scoped_registry);
        let host_auth = analyze_npmrc("//packages.test/:_authToken=secret\n");
        assert!(host_auth.auth);
        let host_auth_with_port = analyze_npmrc("//packages.test:4873/:_authToken=secret\n");
        assert!(host_auth_with_port.auth);

        let berry = analyze_yaml_conservative(
            r#"
npmRegistries:
  //packages.test:
    npmAuthToken: secret
"#,
        );
        assert!(berry.scoped_registry);

        for config in [
            r#""npmScopes": {private: {npmRegistryServer: "https://packages.test/"}}"#,
            r#"'npmRegistries': {'//packages.test': {npmAuthToken: secret}}"#,
        ] {
            let berry = analyze_yarn_yaml(config).unwrap();
            assert!(
                berry.scoped_registry,
                "quoted or flow-style Yarn registry configuration was missed: {config}"
            );
        }
        assert!(analyze_yarn_yaml(r#""npmAuthToken": secret"#).unwrap().auth);
        assert!(
            analyze_yarn_yaml(
                r#"npmScopes:
  private:
    "npmAuthToken": secret
"#,
            )
            .unwrap()
            .auth
        );
        assert!(analyze_yarn_yaml("[not, a, mapping]").is_err());
    }

    #[test]
    fn yarn_yaml_quoted_and_flow_security_settings_force_native_pass_through() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        for config in [
            r#""npmScopes": {private: {npmRegistryServer: "https://packages.test/"}}"#,
            r#"'npmRegistries': {'//packages.test': {npmAuthToken: secret}}"#,
            r#""npmAuthToken": secret"#,
        ] {
            std::fs::write(project.join(".yarnrc.yml"), config).unwrap();
            let ctx = test_ctx(temp.path(), vec![unused_loopback_url()], false);
            let decision =
                native_registry_candidates(&ctx, &project, PackageManager::YarnBerry, |_| None)
                    .unwrap();
            assert!(
                matches!(decision, NativeDecision::PassThrough(_)),
                "Yarn security configuration was not passed through: {config}"
            );
        }
    }

    #[test]
    fn npm_client_identity_and_tls_policy_are_detected_conservatively() {
        for config in [
            "cert=-----BEGIN CERTIFICATE-----\n",
            "key=-----BEGIN PRIVATE KEY-----\n",
            "//packages.test/:certfile=/secure/client.pem\n",
            "//packages.test/team/:keyfile=/secure/client.key\n",
        ] {
            assert!(analyze_npmrc(config).auth, "missed identity: {config}");
        }
        for config in [
            "cafile=/secure/corporate-ca.pem\n",
            "strict-ssl=false\n",
            "//packages.test/:cafile=/secure/corporate-ca.pem\n",
        ] {
            let analysis = analyze_npmrc(config);
            assert!(!analysis.auth, "CA policy is not client identity: {config}");
            assert!(analysis.tls_policy, "missed TLS policy: {config}");
        }

        let berry = analyze_yaml_conservative(
            "httpsCertFilePath: /secure/client.pem\nhttpsKeyFilePath: /secure/client.key\n",
        );
        assert!(berry.auth);
        let berry_ca =
            analyze_yaml_conservative("httpsCaFilePath: /secure/ca.pem\nenableStrictSsl: false\n");
        assert!(!berry_ca.auth);
        assert!(berry_ca.tls_policy);
    }

    #[test]
    fn npm_auth_and_tls_environment_keys_are_detected_without_values_leaking() {
        for expected in [
            "NPM_CONFIG_CERT",
            "npm_config_key",
            "NPM_CONFIG_CERTFILE",
            "npm_config_keyfile",
            "YARN_HTTPS_CERT_FILE_PATH",
        ] {
            assert_eq!(
                global_auth_env(|key| (key == expected).then(|| "secret-path".into())),
                Some(expected)
            );
        }
        for expected in [
            "NPM_CONFIG_CAFILE",
            "npm_config_strict_ssl",
            "NODE_EXTRA_CA_CERTS",
            "YARN_HTTPS_CA_FILE_PATH",
        ] {
            assert_eq!(
                global_tls_policy_env(|key| (key == expected).then(|| "secret-path".into())),
                Some(expected)
            );
        }
    }

    #[test]
    fn native_home_discovery_is_platform_ordered_and_deduplicated() {
        let cwd = Path::new("/work");
        let windows_home = PathBuf::from(r"C:\Users\person");
        let getenv = |key: &str| match key {
            "HOME" => Some("/posix-home".into()),
            "USERPROFILE" => Some("C:\\Users\\person".into()),
            _ => None,
        };
        let non_windows_userprofile = if windows_home.is_absolute() {
            windows_home.clone()
        } else {
            cwd.join(&windows_home)
        };
        assert_eq!(
            native_home_directories(cwd, getenv, false),
            [PathBuf::from("/posix-home"), non_windows_userprofile]
        );
        assert_eq!(
            native_home_directories(cwd, getenv, true),
            [windows_home, PathBuf::from("/posix-home")]
        );
        assert_eq!(
            native_home_directories(
                cwd,
                |key| matches!(key, "HOME" | "USERPROFILE").then(|| "/same".into()),
                true
            ),
            [PathBuf::from("/same")]
        );
        assert_eq!(
            native_home_directories(
                cwd,
                |key| (key == "HOME").then(|| "relative-home".into()),
                false
            ),
            [PathBuf::from("/work/relative-home")]
        );
    }

    #[test]
    fn native_proxy_settings_pass_through_without_claiming_authentication() {
        for config in [
            "proxy=http://proxy.example.test:8080\n",
            "https-proxy=http://proxy.example.test:8080\n",
        ] {
            let analysis = analyze_npmrc(config);
            assert!(analysis.proxy, "missed npm proxy: {config}");
            assert!(!analysis.auth);
        }
        let berry = analyze_yaml_conservative(
            "httpProxy: http://proxy.example.test:8080\nhttpsProxy: http://proxy.example.test:8080\n",
        );
        assert!(berry.proxy);
        let bun = analyze_bun_toml(
            "[install]\nregistry = \"https://registry.npmjs.org/\"\nhttpsProxy = \"http://proxy.example.test:8080\"\n",
        )
        .unwrap();
        assert!(bun.proxy);

        for expected in [
            "NPM_CONFIG_PROXY",
            "npm_config_https_proxy",
            "YARN_HTTP_PROXY",
            "BUN_CONFIG_PROXY",
        ] {
            assert_eq!(
                global_native_proxy_env(|key| (key == expected).then(|| "secret-proxy".into())),
                Some(expected)
            );
        }
        assert_eq!(
            global_native_proxy_env(|key| {
                matches!(key, "HTTP_PROXY" | "HTTPS_PROXY")
                    .then(|| "http://environment-proxy".into())
            }),
            None
        );
    }

    #[test]
    fn explicit_and_prefix_derived_npm_global_configs_are_inspected() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let home = temp.path().join("home");
        let prefix = temp.path().join("prefix");
        let explicit = temp.path().join("explicit/npmrc");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(prefix.join("etc")).unwrap();
        std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        std::fs::write(
            prefix.join("etc/npmrc"),
            "//packages.test/:certfile=/secure/client.pem\n",
        )
        .unwrap();
        std::fs::write(&explicit, "cafile=/secure/corporate-ca.pem\n").unwrap();

        let home_value = home.display().to_string();
        let prefix_value = prefix.display().to_string();
        let ctx = test_ctx(temp.path(), Vec::new(), false);
        let derived =
            native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| match key {
                "HOME" => Some(home_value.clone()),
                "npm_config_prefix" => Some(prefix_value.clone()),
                _ => None,
            })
            .unwrap();
        let NativeDecision::PassThrough(reason) = derived else {
            panic!("expected derived global config pass-through");
        };
        assert!(reason.contains("authentication"));
        assert!(reason.contains("etc/npmrc"));
        assert!(!reason.contains("client.pem"));

        let explicit_value = explicit.display().to_string();
        let explicit_decision =
            native_registry_candidates(&ctx, &cwd, PackageManager::Pnpm, |key| match key {
                "HOME" => Some(home_value.clone()),
                "NPM_CONFIG_GLOBALCONFIG" => Some(explicit_value.clone()),
                _ => None,
            })
            .unwrap();
        let NativeDecision::PassThrough(reason) = explicit_decision else {
            panic!("expected explicit global config pass-through");
        };
        assert!(reason.contains("TLS policy"));
        assert!(reason.contains("explicit/npmrc"));
        assert!(!reason.contains("corporate-ca.pem"));

        let raw_prefix = temp.path().join("raw-prefix");
        std::fs::create_dir_all(raw_prefix.join("etc")).unwrap();
        std::fs::write(
            raw_prefix.join("etc/npmrc"),
            "proxy=http://proxy.example.test:8080\n",
        )
        .unwrap();
        let raw_prefix_value = raw_prefix.display().to_string();
        let decision =
            native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| match key {
                "HOME" => Some(home_value.clone()),
                "PREFIX" => Some(raw_prefix_value.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(decision, NativeDecision::PassThrough(reason) if reason.contains("proxy"))
        );
    }

    #[test]
    fn managed_node_default_global_and_builtin_npmrc_are_inspected() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let ctx = test_ctx(temp.path(), Vec::new(), false);
        let node = ctx.dirs.install_path("node", "22.0.0");
        std::fs::create_dir_all(node.join("etc")).unwrap();
        std::fs::create_dir_all(node.join("lib/node_modules/npm")).unwrap();
        std::fs::write(node.join(".osdk-complete"), b"").unwrap();
        std::fs::write(
            node.join("etc/npmrc"),
            "proxy=http://proxy.example.test:8080\n",
        )
        .unwrap();
        let home_value = home.display().to_string();
        let decision = native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| {
            (key == "HOME").then(|| home_value.clone())
        })
        .unwrap();
        assert!(
            matches!(decision, NativeDecision::PassThrough(reason) if reason.contains("proxy"))
        );

        std::fs::remove_file(node.join("etc/npmrc")).unwrap();
        std::fs::write(
            node.join("lib/node_modules/npm/npmrc"),
            "//packages.test/:certfile=/secure/client.pem\n",
        )
        .unwrap();
        let decision = native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| {
            (key == "HOME").then(|| home_value.clone())
        })
        .unwrap();
        assert!(
            matches!(decision, NativeDecision::PassThrough(reason) if reason.contains("authentication"))
        );
    }

    #[test]
    fn both_home_candidates_are_inspected_and_redirects_pass_through() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let home = temp.path().join("home");
        let userprofile = temp.path().join("userprofile");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&userprofile).unwrap();
        let ctx = test_ctx(temp.path(), Vec::new(), false);
        std::fs::write(
            userprofile.join(".npmrc"),
            "//packages.test/:keyfile=/secure/client.key\n",
        )
        .unwrap();
        let home_value = home.display().to_string();
        let userprofile_value = userprofile.display().to_string();
        let decision =
            native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| match key {
                "HOME" => Some(home_value.clone()),
                "USERPROFILE" => Some(userprofile_value.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(decision, NativeDecision::PassThrough(reason) if reason.contains("authentication"))
        );

        std::fs::write(home.join(".npmrc"), "globalconfig=${CUSTOM_NPMRC}\n").unwrap();
        std::fs::remove_file(userprofile.join(".npmrc")).unwrap();
        let decision =
            native_registry_candidates(&ctx, &cwd, PackageManager::Npm, |key| match key {
                "HOME" => Some(home_value.clone()),
                "USERPROFILE" => Some(userprofile_value.clone()),
                "CUSTOM_NPMRC" => Some("/not-inspected/npmrc".into()),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(decision, NativeDecision::PassThrough(reason) if reason.contains("another config location"))
        );
    }

    #[tokio::test]
    async fn failed_primary_falls_back_and_probe_is_anonymous() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dead_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}/", dead_listener.local_addr().unwrap());
        let (healthy, request, server) = registry_server(
            "200 OK",
            r#"{"name":"npm","version":"11.0.0"}"#,
            Duration::ZERO,
        );
        drop(dead_listener);
        let ctx = test_ctx(temp.path(), vec![dead.clone(), healthy.clone()], false);
        let args = vec!["install".into()];
        let home_value = home.display().to_string();
        let plan = plan(
            &ctx,
            temp.path(),
            PackageManager::Npm,
            "npm",
            &args,
            |key| (key == "HOME").then(|| home_value.clone()),
        )
        .await
        .unwrap();
        let RegistryPlan::Selected { url, probes } = plan else {
            panic!("expected selected plan");
        };
        assert_eq!(url, healthy);
        assert_eq!(probes.len(), 2);
        assert!(!probes[0].ok);
        assert!(probes[1].ok);
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(request.starts_with("GET /npm/latest HTTP/1.1"), "{request}");
        assert!(!lower.contains("authorization:"), "{request}");
        assert!(!lower.contains("cookie:"), "{request}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn all_failed_candidates_are_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let candidates = vec![unused_loopback_url(), unused_loopback_url()];
        let ctx = test_ctx(temp.path(), candidates, false);
        let args = vec!["install".into()];
        let home_value = home.display().to_string();
        let plan = plan(
            &ctx,
            temp.path(),
            PackageManager::Npm,
            "npm",
            &args,
            |key| (key == "HOME").then(|| home_value.clone()),
        )
        .await
        .unwrap();
        let RegistryPlan::Unavailable { probes } = plan else {
            panic!("expected unavailable plan");
        };
        assert_eq!(probes.len(), 2);
        assert!(probes.iter().all(|probe| !probe.ok));
    }

    #[tokio::test]
    async fn malformed_metadata_is_not_healthy() {
        let (url, _request, server) = registry_server(
            "200 OK",
            r#"{"name":"other","version":"1"}"#,
            Duration::ZERO,
        );
        let probe = probe_one(&reqwest::Client::new(), url, Duration::from_secs(2)).await;
        assert!(!probe.ok);
        assert!(probe.error.unwrap().contains("unexpected name"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn non_success_status_is_not_healthy() {
        let (url, _request, server) = registry_server(
            "503 Service Unavailable",
            r#"{"name":"npm","version":"11.0.0"}"#,
            Duration::ZERO,
        );
        let probe = probe_one(&reqwest::Client::new(), url, Duration::from_secs(2)).await;
        assert!(!probe.ok);
        assert_eq!(probe.error.as_deref(), Some("HTTP 503"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn registry_probe_does_not_follow_a_cross_origin_loopback_redirect() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!("http://{}/private", target.local_addr().unwrap());
        let (url, request, server) = redirect_server(location);
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::custom(|_| None::<reqwest::Url>))
            .redirect(registry_probe_redirect_policy())
            .build()
            .unwrap();

        let probe = probe_one(&client, url, Duration::from_secs(2)).await;
        assert!(!probe.ok);
        assert!(probe.error.is_some());
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(request.starts_with("GET /npm/latest HTTP/1.1"), "{request}");
        server.join().unwrap();

        let error = target.accept().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[cfg(not(windows))]
    #[test]
    fn registry_probe_honors_http_proxy_without_forwarding_credentials() {
        const CHILD_MARKER: &str = "OSDK_REGISTRY_PROXY_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let probes =
                runtime.block_on(probe_all(&["http://registry-probe.invalid/".into()], 2_000));
            assert_eq!(probes.len(), 1);
            assert!(probes[0].ok, "{probes:?}");
            return;
        }

        let (proxy, request, server) = registry_server(
            "200 OK",
            r#"{"name":"npm","version":"11.0.0"}"#,
            Duration::ZERO,
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "package_registry::tests::registry_probe_honors_http_proxy_without_forwarding_credentials",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env("HTTP_PROXY", &proxy)
            .env("http_proxy", &proxy)
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "proxy test child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(
            request.starts_with("GET http://registry-probe.invalid/npm/latest HTTP/1.1"),
            "{request}"
        );
        assert!(!lower.contains("authorization:"), "{request}");
        assert!(!lower.contains("cookie:"), "{request}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_private_registry_and_offline_mode_do_not_probe() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".npmrc"),
            "registry=https://packages.corp.invalid/\n",
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let candidate = format!("http://{}/", listener.local_addr().unwrap());
        let args = vec!["install".into()];
        let home_value = home.display().to_string();

        let ctx = test_ctx(temp.path(), vec![candidate.clone()], false);
        let private_plan = plan(
            &ctx,
            temp.path(),
            PackageManager::Npm,
            "npm",
            &args,
            |key| (key == "HOME").then(|| home_value.clone()),
        )
        .await
        .unwrap();
        assert!(matches!(private_plan, RegistryPlan::PassThrough { .. }));
        assert!(listener.accept().is_err());

        std::fs::remove_file(home.join(".npmrc")).unwrap();
        let ctx = test_ctx(temp.path(), vec![candidate], true);
        let offline_plan = plan(
            &ctx,
            temp.path(),
            PackageManager::Npm,
            "npm",
            &args,
            |key| (key == "HOME").then(|| home_value.clone()),
        )
        .await
        .unwrap();
        assert!(matches!(offline_plan, RegistryPlan::PassThrough { .. }));
        assert!(listener.accept().is_err());
    }

    #[tokio::test]
    async fn explicit_manager_environment_passes_through() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path(), vec![unused_loopback_url()], false);
        let args = vec!["install".into()];
        let plan = plan(
            &ctx,
            temp.path(),
            PackageManager::Pnpm,
            "pnpm",
            &args,
            |key| {
                (key == "pnpm_config_registry")
                    .then(|| "https://private.test/token-redacted".into())
            },
        )
        .await
        .unwrap();
        let RegistryPlan::PassThrough { reason } = plan else {
            panic!("expected pass-through plan");
        };
        assert!(reason.contains("pnpm_config_registry"));
        assert!(!reason.contains("token-redacted"));
    }

    fn test_ctx(root: &Path, candidates: Vec<String>, offline: bool) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        let mut sources = SourcesConfig::default();
        sources.registries.npm.urls = candidates;
        sources.registries.npm.probe_timeout_ms = 250;
        let settings = crate::config::Settings {
            offline,
            ..Default::default()
        };
        let config = Config {
            settings,
            sources,
            tools: Default::default(),
            aliases: Default::default(),
            project_config_path: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(COOKIE, HeaderValue::from_static("session=secret"));
        Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config,
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap(),
            cas: Arc::new(Cas::new(dirs.store)),
            show_progress: false,
        }
    }

    fn unused_loopback_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}/")
    }

    fn registry_server(
        status: &'static str,
        body: &'static str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "no registry probe arrived");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accepting registry probe: {error}"),
                }
            };
            let request = read_request(&mut stream);
            sender.send(request).unwrap();
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (format!("http://{address}/"), receiver, handle)
    }

    fn redirect_server(
        location: String,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            sender.send(read_request(&mut stream)).unwrap();
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        (format!("http://{address}/"), receiver, handle)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        // Accepted sockets can inherit the listener's nonblocking mode on
        // Windows/Wine. Return to blocking I/O before applying the bounded
        // read timeout so a transient WouldBlock is not treated as a broken
        // registry response.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "probe closed before sending headers");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(
                bytes.len() < 32 * 1024,
                "probe headers are unexpectedly large"
            );
        }
        String::from_utf8(bytes).unwrap()
    }
}
