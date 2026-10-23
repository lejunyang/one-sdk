//! Shared process-local package-source adapters. Never writes native config,
//! never retries a package-manager command, and never probes authenticated URLs.
use crate::backend::Ctx;
use crate::config::{RegistriesConfig, SourcePrecedence};
use crate::error::{Error, Result};
use crate::package_registry::{self, RegistryPlan};
use std::collections::BTreeMap;
use std::path::Path;

/// Resolve package sources for exec, shim and deps with the same native adapters.
/// Returned text is evidence only, never execution material.
pub async fn apply(
    ctx: &Ctx,
    cwd: &Path,
    program: &str,
    yarn_version: Option<&str>,
    args: &[String],
    env: &mut BTreeMap<String, String>,
) -> Result<String> {
    apply_with_environment(ctx, cwd, program, yarn_version, args, env, &|key| {
        std::env::var(key).ok()
    })
    .await
}
#[allow(clippy::too_many_arguments)]
async fn apply_with_environment(
    ctx: &Ctx,
    cwd: &Path,
    program: &str,
    yarn_version: Option<&str>,
    args: &[String],
    env: &mut BTreeMap<String, String>,
    ambient: &impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd");
    if let Some(manager) = package_registry::manager_for_command(program, yarn_version) {
        let result = package_registry::plan(ctx, cwd, manager, program, args, |key| {
            env.get(key).cloned().or_else(|| ambient(key))
        })
        .await?;
        return match result {
            RegistryPlan::PassThrough { reason } => Ok(reason),
            RegistryPlan::Selected { url, .. } => {
                for key in package_registry::registry_envs(manager) {
                    env.insert(key.into(), url.clone());
                }
                Ok(format!(
                    "registries.npm selected via {manager}; packages={:?}",
                    ctx.config.settings.precedence.package_policy("npm")
                ))
            }
            RegistryPlan::Unavailable { .. } => Err(Error::config(
                "no reachable package registry: all candidates failed; command was not started",
            )),
        };
    }
    if program == "go" && args.first().is_some_and(|arg| arg == "mod") {
        if let Some(proxy) = &ctx.config.registries().go.proxy {
            let getenv = |key: &str| env.get(key).cloned().or_else(|| ambient(key));
            let native_env = match getenv("GOENV").as_deref() {
                Some("off") => false,
                Some(path) if !path.is_empty() => Path::new(path).is_file(),
                _ => {
                    getenv("XDG_CONFIG_HOME")
                        .is_some_and(|base| Path::new(&base).join("go/env").is_file())
                        || ["HOME", "USERPROFILE"]
                            .iter()
                            .filter_map(|key| getenv(key))
                            .any(|home| {
                                [
                                    ".config/go/env",
                                    "Library/Application Support/go/env",
                                    "AppData/Roaming/go/env",
                                ]
                                .iter()
                                .any(|name| Path::new(&home).join(name).is_file())
                            })
                }
            };
            if ctx.config.settings.precedence.package_policy("go") == SourcePrecedence::Native
                && native_env
            {
                return Ok(
                    "native Go environment file retained; effective GOPROXY resolved by Go".into(),
                );
            }
            if !proxy.is_empty() && !env.contains_key("GOPROXY") && ambient("GOPROXY").is_none() {
                env.insert("GOPROXY".into(), proxy.clone());
            }
        }
        return Ok("Go delegates proxy chains, credentials and fallback to GOPROXY".into());
    }
    if matches!(program, "yarn" | "yarnpkg") && yarn_version.is_none() {
        return Ok("Yarn major is unknown; native configuration preserved".into());
    }
    let uv_args = uv_command_args(args);
    if program != "uv" || !uv_fetch(uv_args) {
        return Ok("native command; no package-source adapter".into());
    }
    // Context-changing CLI flags are passed through, never interpreted against
    // the wrong project. CLI indexes and offline flags are authoritative.
    if args.iter().any(|arg| {
        [
            "--index",
            "--default-index",
            "--index-url",
            "--extra-index-url",
            "--no-index",
            "--find-links",
            "-i",
            "-f",
            "--offline",
            "--directory",
            "-C",
            "--project",
            "--config-file",
            "--no-config",
            "--env-file",
        ]
        .iter()
        .any(|flag| {
            arg == flag
                || arg.starts_with(&format!("{flag}="))
                || (flag.len() == 2 && arg.starts_with(flag) && arg.len() > 2)
        })
    }) {
        return Ok("uv command-line configuration/offline is authoritative".into());
    }
    if uv_args.first().is_some_and(|arg| arg == "pip")
        && requirements_have_native_options(cwd, uv_args)?
    {
        return Ok(
            "uv requirements input carries native options or unresolved context; delegated intact"
                .into(),
        );
    }
    let settings = &ctx.config.registries().python;
    let getenv = |key: &str| env.get(key).cloned().or_else(|| ambient(key));
    if getenv("UV_CONFIG_FILE").is_some()
        || getenv("UV_ENV_FILE").is_some()
        || ["UV_NO_CONFIG", "UV_NO_INDEX", "UV_OFFLINE"]
            .iter()
            .any(|key| uv_env_enabled(getenv(key).as_deref()))
    {
        return Ok("uv invocation configuration context/no-index/offline is authoritative; osdk source overrides not injected".into());
    }
    let ambient_default = ["UV_DEFAULT_INDEX", "UV_INDEX_URL"]
        .iter()
        .any(|key| getenv(key).is_some());
    let ambient_indexes = ["UV_INDEX", "UV_EXTRA_INDEX_URL"]
        .iter()
        .any(|key| getenv(key).is_some());
    let native_config = discover_python_configuration(
        cwd,
        uv_args.first().is_some_and(|arg| arg == "pip"),
        &getenv,
    )?;
    let native = native_config.present;
    let locations = native_config
        .files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if native_config.workspace {
        return Ok(format!("uv workspace discovery delegated intact; candidate boundary={locations}; member eligibility and effective fields unknown; osdk source overrides not injected"));
    }
    let native_first =
        ctx.config.settings.precedence.package_policy("python") == SourcePrecedence::Native;
    if native_first && native {
        return Ok(format!("packages=native: native Python configuration group retained; candidate files={locations}; osdk default/index overrides not injected; exact effective values and winning fields delegated to uv"));
    }
    let injecting = (!ambient_default && settings.default.as_ref().is_some_and(|v| !v.is_empty()))
        || (!ambient_indexes && settings.indexes.as_ref().is_some_and(|v| !v.is_empty()));
    if native_config.complex && injecting {
        return Err(Error::config(format!("cannot safely overlay osdk Python indexes on complex native configuration at {locations}; use precedence.python='native' or configure indexes entirely in uv")));
    }
    let explicit = settings.default.is_some() || settings.indexes.is_some();
    if let Some(url) = settings.default.as_ref().filter(|url| !url.is_empty()) {
        if !ambient_default {
            env.insert("UV_DEFAULT_INDEX".into(), url.clone());
        }
    }
    if let Some(indexes) = settings
        .indexes
        .as_ref()
        .filter(|indexes| !indexes.is_empty())
    {
        if !ambient_indexes {
            // Named indexes and package bindings remain in native config. uv's
            // --index is higher priority than the default and is not fallback.
            let unsafe_strategy = args
                .iter()
                .enumerate()
                .filter_map(|(i, arg)| {
                    arg.strip_prefix("--index-strategy=").or_else(|| {
                        (arg == "--index-strategy")
                            .then(|| args.get(i + 1).map(String::as_str))
                            .flatten()
                    })
                })
                .any(|strategy| strategy != "first-index");
            if unsafe_strategy {
                return Err(Error::config("configured Python indexes require first-index; explicit unsafe strategy cannot be combined with osdk indexes"));
            }
            if env
                .get("UV_INDEX_STRATEGY")
                .cloned()
                .or_else(|| ambient("UV_INDEX_STRATEGY"))
                .is_some_and(|v| v != "first-index")
            {
                return Err(Error::config(
                    "configured Python indexes require UV_INDEX_STRATEGY=first-index",
                ));
            }
            env.insert("UV_INDEX".into(), indexes.join(" "));
            env.insert("UV_INDEX_STRATEGY".into(), "first-index".into());
        }
    }
    if explicit || ambient_default || ambient_indexes || native {
        return Ok(format!(
            "packages={:?}: Python default={}, indexes={}; native named indexes, package bindings and other fields remain native; effective files delegated to uv; no mirror fallback",
            ctx.config.settings.precedence.package_policy("python"),
            if ambient_default { "invocation environment" } else if settings.default.as_ref().is_some_and(|s| !s.is_empty()) { "osdk UV_DEFAULT_INDEX" } else { "native (osdk unset/cleared)" },
            if ambient_indexes { "invocation environment" } else if settings.indexes.as_ref().is_some_and(|s| !s.is_empty()) { "osdk UV_INDEX before native indexes, first-index" } else { "native (osdk unset/cleared)" },
        ));
    }
    if ctx.config.settings.offline {
        return Ok("offline; no mirror probes".into());
    }
    let candidates = crate::python_index::effective_candidates(&settings.urls);
    match crate::python_index::plan(&candidates, settings.probe_timeout_ms).await {
        crate::python_index::IndexPlan::Selected { url, .. } => {
            env.insert("UV_DEFAULT_INDEX".into(), url);
            Ok("Python mirror selected for default index only".into())
        }
        crate::python_index::IndexPlan::PassThrough { reason } => Ok(reason),
        crate::python_index::IndexPlan::Unavailable { .. } => Err(Error::config(
            "all Python mirror candidates failed; command was not started",
        )),
    }
}

fn uv_fetch(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("sync" | "lock" | "add" | "run")
    ) || (args.first().is_some_and(|arg| arg == "pip")
        && args
            .get(1)
            .is_some_and(|arg| matches!(arg.as_str(), "install" | "sync" | "compile")))
}

// Positional compile/sync inputs have their own native option grammar. Delegate
// them rather than interpreting an incomplete subset of requirements semantics.
fn requirements_have_native_options(cwd: &Path, args: &[String]) -> Result<bool> {
    if args
        .get(1)
        .is_some_and(|arg| matches!(arg.as_str(), "sync" | "compile"))
    {
        return Ok(true);
    }
    for value in requirements_arguments(args) {
        if value == "-" {
            return Ok(true);
        }
        let path = cwd.join(value);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Ok(true),
        };
        if text.lines().any(|line| line.trim_start().starts_with('-')) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn requirements_arguments(args: &[String]) -> Vec<&str> {
    let mut values = Vec::new();
    for (i, arg) in args.iter().take_while(|a| a.as_str() != "--").enumerate() {
        for flag in [
            "-r",
            "--requirements",
            "-c",
            "--constraint",
            "-b",
            "--build-constraint",
            "--overrides",
        ] {
            let value = if arg == flag {
                args.get(i + 1).map(String::as_str)
            } else if flag.len() == 2 {
                arg.strip_prefix(flag).filter(|v| !v.is_empty())
            } else {
                arg.strip_prefix(&format!("{flag}="))
            };
            if let Some(value) = value {
                values.push(value);
            }
        }
    }
    values
}
// False-like uv booleans are not an explicit request to disable configuration
// or networking. Unknown values remain uv's own validation responsibility.
fn uv_env_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        )
    })
}
fn uv_command_args(args: &[String]) -> &[String] {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if !arg.starts_with('-') {
            return &args[i..];
        }
        if matches!(
            arg.as_str(),
            "--directory"
                | "-C"
                | "--project"
                | "--config-file"
                | "--cache-dir"
                | "--color"
                | "--python"
                | "--index-strategy"
        ) {
            i += 2;
        } else {
            i += 1;
        }
    }
    &[]
}
#[derive(Default)]
struct PythonConfiguration {
    present: bool,
    complex: bool,
    workspace: bool,
    files: Vec<std::path::PathBuf>,
}
fn discover_python_configuration(
    cwd: &Path,
    pip: bool,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<PythonConfiguration> {
    let mut result = PythonConfiguration::default();
    for key in [
        "UV_DEFAULT_INDEX",
        "UV_INDEX_URL",
        "UV_INDEX",
        "UV_EXTRA_INDEX_URL",
        "UV_FIND_LINKS",
        "UV_INDEX_STRATEGY",
    ] {
        result.present |= getenv(key).is_some();
    }
    // Workspace membership/exclusion/globs and package discovery belong to uv.
    // Detect a possible workspace before reading member config; do not reject a
    // member's ignored settings or pretend to know its effective field sources.
    for dir in cwd.ancestors() {
        let path = dir.join("pyproject.toml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = toml::from_str::<toml::Value>(&text) {
                if value
                    .get("tool")
                    .and_then(|t| t.get("uv"))
                    .and_then(|u| u.get("workspace"))
                    .is_some()
                {
                    result.present = true;
                    result.workspace = true;
                    result.files.push(path);
                    return Ok(result);
                }
            }
        }
    }
    for dir in cwd.ancestors() {
        let uv = dir.join("uv.toml");
        let py = dir.join("pyproject.toml");
        // uv.toml takes precedence over the sibling tool.uv table.
        if uv.is_file() {
            inspect_python_config(&uv, false, pip, &mut result)?;
            break;
        }
        if py.is_file() && inspect_python_config(&py, true, pip, &mut result)? {
            break;
        }
    }
    #[cfg(not(windows))]
    let user = getenv("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| getenv("HOME").map(|h| Path::new(&h).join(".config")));
    #[cfg(windows)]
    let user = getenv("APPDATA").map(std::path::PathBuf::from);
    if let Some(base) = user {
        let path = base.join("uv/uv.toml");
        if path.is_file() {
            inspect_python_config(&path, false, pip, &mut result)?;
        }
    }
    #[cfg(not(windows))]
    let system = getenv("XDG_CONFIG_DIRS")
        .map(|s| std::env::split_paths(&s).collect::<Vec<_>>())
        .unwrap_or_else(|| vec!["/etc/xdg".into()]);
    #[cfg(windows)]
    let system = getenv("PROGRAMDATA")
        .map(|s| vec![std::path::PathBuf::from(s)])
        .unwrap_or_default();
    #[cfg(not(windows))]
    let system = system
        .into_iter()
        .chain([std::path::PathBuf::from("/etc")])
        .collect::<Vec<_>>();
    for base in system {
        let path = base.join("uv/uv.toml");
        if path.is_file() {
            inspect_python_config(&path, false, pip, &mut result)?;
            break;
        }
    }
    // Requirements files are invocation inputs, not automatic configuration.
    // Requirements containing native options are delegated before discovery.
    Ok(result)
}
fn inspect_python_config(
    path: &Path,
    pyproject: bool,
    pip: bool,
    result: &mut PythonConfiguration,
) -> Result<bool> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let value: toml::Value = toml::from_str(&text).map_err(|_| {
        Error::config(format!(
            "invalid eligible Python TOML configuration: {}",
            path.display()
        ))
    })?;
    let config = if pyproject {
        let Some(v) = value.get("tool").and_then(|t| t.get("uv")) else {
            return Ok(false);
        };
        v
    } else {
        &value
    };
    result.present = true;
    result.files.push(path.into());
    // Array merging, named bindings, auth, TLS and discovery settings cannot
    // safely be synthesized into an osdk per-field override.
    let simple = ["index-url", "extra-index-url", "index-strategy", "pip"];
    if let Some(table) = config.as_table() {
        result.complex |= table.keys().any(|k| !simple.contains(&k.as_str()));
        if pip {
            if let Some(table) = table.get("pip").and_then(toml::Value::as_table) {
                result.complex |= table.keys().any(|k| !simple.contains(&k.as_str()));
            }
        }
    } else {
        result.complex = true;
    }
    Ok(true)
}
#[cfg(test)]
fn python_native_configuration(
    cwd: &Path,
    pip: bool,
    getenv: impl Fn(&str) -> Option<String>,
) -> Result<bool> {
    Ok(discover_python_configuration(cwd, pip, &getenv)?.present)
}

/// Potential native input paths, including missing files so creation/removal
/// changes the fingerprint. Contents are hashed, never included in diagnostics.
pub fn native_files(
    cwd: &Path,
    program: &str,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Vec<std::path::PathBuf> {
    let names: &[&str] = match program {
        "uv" => &["uv.toml", "pyproject.toml"],
        "cargo" => &[".cargo/config", ".cargo/config.toml"],
        "go" => &["go.env"],
        _ => &[
            ".npmrc",
            ".yarnrc",
            ".yarnrc.yml",
            "bunfig.toml",
            ".bunfig.toml",
            "pnpm-workspace.yaml",
        ],
    };
    let mut paths: Vec<_> = cwd
        .ancestors()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .collect();
    for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "APPDATA"] {
        if let Some(home) = getenv(key) {
            paths.extend(names.iter().map(|name| Path::new(&home).join(name)));
            if program == "go" {
                paths.push(Path::new(&home).join("go/env"));
            }
            if program == "uv" {
                paths.push(Path::new(&home).join(".config/uv/uv.toml"));
                paths.push(Path::new(&home).join("uv/uv.toml"));
            }
        }
    }
    if let Some(files) = getenv("UV_ENV_FILE") {
        let direct = Path::new(&files);
        if direct.is_file() {
            paths.push(direct.to_path_buf());
        } else {
            paths.extend(files.split_whitespace().map(|p| cwd.join(p)));
        }
    }
    for key in [
        "NPM_CONFIG_USERCONFIG",
        "npm_config_userconfig",
        "NPM_CONFIG_GLOBALCONFIG",
        "npm_config_globalconfig",
        "UV_CONFIG_FILE",
        "GOENV",
    ] {
        if let Some(path) = getenv(key) {
            paths.push(path.into());
        }
    }
    for dir in cwd.ancestors() {
        for name in [
            ".nvmrc",
            ".node-version",
            ".python-version",
            ".tool-versions",
            "rust-toolchain.toml",
            "rust-toolchain",
            "go.mod",
            "package.json",
        ] {
            paths.push(dir.join(name));
        }
    }
    for key in [
        "CARGO_HOME",
        "NPM_CONFIG_PREFIX",
        "npm_config_prefix",
        "PREFIX",
    ] {
        if let Some(base) = getenv(key) {
            for suffix in ["config", "config.toml", "etc/npmrc"] {
                paths.push(Path::new(&base).join(suffix));
            }
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Some(base) = getenv(key) {
            for suffix in [
                ".netrc",
                "_netrc",
                ".config/go/env",
                "Library/Application Support/go/env",
                "AppData/Roaming/go/env",
                ".cargo/config",
                ".cargo/config.toml",
                ".config/bun/bunfig.toml",
            ] {
                paths.push(Path::new(&base).join(suffix));
            }
        }
    }
    if program == "uv" {
        #[cfg(not(windows))]
        {
            paths.push("/etc/uv/uv.toml".into());
            if getenv("XDG_CONFIG_DIRS").is_none() {
                paths.push("/etc/xdg/uv/uv.toml".into());
            }
        }
        if let Some(base) = getenv("PROGRAMDATA") {
            paths.push(Path::new(&base).join("uv/uv.toml"));
        }
        if let Some(bases) = getenv("XDG_CONFIG_DIRS") {
            paths.extend(std::env::split_paths(&bases).map(|base| base.join("uv/uv.toml")));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Explicit invocation files supplement the broad fingerprint inventory. This
/// does not assign precedence or parse their contents for execution.
pub fn invocation_files(cwd: &Path, program: &str, args: &[String]) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let args = &args[..args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len())];
    let option = |i: usize, flag: &str| {
        let arg = &args[i];
        arg.strip_prefix(&format!("{flag}="))
            .or_else(|| {
                (flag == "-C")
                    .then(|| arg.strip_prefix(flag).filter(|v| !v.is_empty()))
                    .flatten()
            })
            .or_else(|| {
                (arg == flag)
                    .then(|| args.get(i + 1).map(String::as_str))
                    .flatten()
            })
    };
    let directory =
        (0..args.len()).find_map(|i| option(i, "--directory").or_else(|| option(i, "-C")));
    let effective_cwd = directory.map_or_else(|| cwd.to_path_buf(), |value| cwd.join(value));
    for i in 0..args.len() {
        for flag in [
            "--config-file",
            "--env-file",
            "--project",
            "--directory",
            "-C",
        ] {
            if let Some(value) = option(i, flag) {
                let path = if matches!(flag, "--directory" | "-C") {
                    cwd.join(value)
                } else {
                    effective_cwd.join(value)
                };
                if matches!(flag, "--project" | "--directory" | "-C") {
                    paths.extend(native_files(&path, program, &|_| None));
                } else {
                    paths.push(path);
                }
            }
        }
    }
    if program == "uv" {
        paths.extend(
            requirements_arguments(args)
                .into_iter()
                .filter(|v| *v != "-")
                .map(|v| effective_cwd.join(v)),
        );
    }
    paths
}

pub fn validate_override(provider: &str, registries: &mut RegistriesConfig) -> Result<()> {
    registries.validate()?;
    let blank = RegistriesConfig::default();
    let valid = match provider {
        "npm" | "pnpm" | "yarn" | "bun" => {
            registries.python == blank.python && registries.go == blank.go
        }
        "uv" | "pip-requirements" => registries.npm == blank.npm && registries.go == blank.go,
        "go" => registries.npm == blank.npm && registries.python == blank.python,
        _ => false,
    };
    if !valid {
        return Err(Error::config(
            "unsupported package-source fields for deps provider; use its native configuration",
        ));
    }
    Ok(())
}

/// Hash execution material separately from display. Never expose raw environment
/// or native file contents in explain output or osdk.lock.
pub fn fingerprint(
    cwd: &Path,
    program: &str,
    execution: &str,
    extra_paths: &[std::path::PathBuf],
) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    hash.update(execution.as_bytes());
    let mut environment: Vec<_> = std::env::vars()
        .filter(|(key, _)| {
            let key = key.to_ascii_uppercase();
            [
                "UV_", "PIP_", "NPM_", "PNPM_", "YARN_", "BUN_", "CARGO_", "GO", "XDG_",
            ]
            .iter()
            .any(|prefix| key.starts_with(prefix))
        })
        .collect();
    environment.sort();
    for (key, value) in environment {
        hash.update(key.as_bytes());
        hash.update(&[0]);
        hash.update(value.as_bytes());
        hash.update(&[0]);
    }
    let mut paths = native_files(cwd, program, &|key| std::env::var(key).ok());
    paths.extend_from_slice(extra_paths);
    paths.sort();
    paths.dedup();
    for path in paths {
        hash.update(path.to_string_lossy().as_bytes());
        match std::fs::read(&path) {
            Ok(bytes) => {
                hash.update(&[1]);
                hash.update(&bytes);
                // Native ${NAME}, ${NAME:-fallback} and $NAME interpolation
                // can source auth from names outside standard manager prefixes.
                for reference in String::from_utf8_lossy(&bytes).split('$').skip(1) {
                    let reference = reference.strip_prefix('{').unwrap_or(reference);
                    let name: String = reference
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        hash.update(name.as_bytes());
                        if let Some(value) = std::env::var_os(&name) {
                            hash.update(value.to_string_lossy().as_bytes());
                        }
                        hash.update(&[0]);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                hash.update(&[0]);
            }
            Err(e) => return Err(Error::io(path, e)),
        }
    }
    Ok(hash.finalize().to_hex().to_string())
}

/// URLs may contain credentials in userinfo, query, fragment or even path.
/// Diagnostic values are deliberately opaque; native processes get originals.
pub fn display_value(value: &str) -> String {
    if value.contains("://")
        || value.to_ascii_lowercase().contains("token")
        || value.to_ascii_lowercase().contains("password")
    {
        "[redacted]".into()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Source-plan fixtures own all invocation inputs; host CA/proxy settings
    // must not turn an expected probe failure into native pass-through.
    async fn apply(
        ctx: &Ctx,
        cwd: &Path,
        program: &str,
        yarn_version: Option<&str>,
        args: &[String],
        env: &mut BTreeMap<String, String>,
    ) -> Result<String> {
        super::apply_with_environment(ctx, cwd, program, yarn_version, args, env, &|_| None).await
    }

    pub(super) fn context(root: &Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" | "OSDK_CACHE_DIR" | "OSDK_CONFIG_DIR" | "OSDK_STORE_DIR"
            | "OSDK_INSTALL_DIR" => Some(root.join(key).display().to_string()),
            _ => None,
        })
        .unwrap();
        let mut config = crate::config::Config::default();
        config.settings.precedence.packages = SourcePrecedence::Osdk;
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            config,
            platform: crate::platform::Platform::current(),
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }
    #[tokio::test]
    async fn all_node_adapters_share_default_resolution_and_preserve_native_scopes_auth() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        let native = "@team:registry=https://private.example/\n//private.example/:_authToken=sentinel-secret\nregistry=https://native.example/\n";
        std::fs::write(temp.path().join(".npmrc"), native).unwrap();
        ctx.config.sources.registries.npm.default = Some("https://osdk.example/".into());
        for (program, version, key) in [
            ("npm", None, "npm_config_registry"),
            ("pnpm", None, "pnpm_config_registry"),
            ("yarn", Some("1.22.22"), "YARN_REGISTRY"),
            ("yarn", Some("4.10.3"), "YARN_NPM_REGISTRY_SERVER"),
            ("bun", None, "BUN_CONFIG_REGISTRY"),
        ] {
            let mut env = BTreeMap::new();
            let evidence = apply(
                &ctx,
                temp.path(),
                program,
                version,
                &["install".into()],
                &mut env,
            )
            .await
            .unwrap();
            assert_eq!(env[key], "https://osdk.example/");
            assert_eq!(env.len(), if program == "pnpm" { 2 } else { 1 });
            if program == "pnpm" {
                assert_eq!(env["npm_config_registry"], "https://osdk.example/");
            }
            assert!(!evidence.contains("sentinel-secret"));
        }
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".npmrc")).unwrap(),
            native
        );
        ctx.config.settings.precedence.packages = SourcePrecedence::Native;
        let mut env = BTreeMap::new();
        let reason = apply(
            &ctx,
            temp.path(),
            "npm",
            None,
            &["install".into()],
            &mut env,
        )
        .await
        .unwrap();
        assert!(env.is_empty());
        assert!(reason.contains("native"));
    }
    #[tokio::test]
    async fn invocation_registry_and_auth_values_are_preserved_not_probed() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        ctx.config.sources.registries.npm.default = Some("https://osdk.example/".into());
        let mut env = BTreeMap::from([(
            "npm_config_registry".into(),
            "https://user:secret@private.example/token".into(),
        )]);
        let before = env.clone();
        let reason = apply(
            &ctx,
            temp.path(),
            "npm",
            None,
            &["install".into()],
            &mut env,
        )
        .await
        .unwrap();
        assert_eq!(env, before);
        assert!(!reason.contains("secret"));
        let mut env = BTreeMap::new();
        apply(
            &ctx,
            temp.path(),
            "npm",
            None,
            &["install".into(), "--registry=https://cli.example/".into()],
            &mut env,
        )
        .await
        .unwrap();
        assert!(env.is_empty());
    }
    #[tokio::test]
    async fn both_python_providers_use_default_and_ordered_indexes_not_mirror_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        ctx.config.sources.registries.python.default =
            Some("https://default.example/simple/".into());
        ctx.config.sources.registries.python.indexes = Some(vec![
            "https://private.example/simple/".into(),
            "https://second.example/simple/".into(),
        ]);
        let native = "[[tool.uv.index]]\nname='corp'\nurl='https://corp.example/simple/'\nexplicit=true\n[tool.uv.sources]\ninternal={index='corp'}\n";
        std::fs::write(temp.path().join("pyproject.toml"), native).unwrap();
        let mut env = BTreeMap::from([("UV_INDEX_CORP_PASSWORD".into(), "secret".into())]);
        let before = env.clone();
        let error = apply(&ctx, temp.path(), "uv", None, &["sync".into()], &mut env)
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("secret"));
        assert_eq!(env, before);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("pyproject.toml")).unwrap(),
            native
        );
        ctx.config.settings.precedence.packages = SourcePrecedence::Native;
        let mut env = BTreeMap::new();
        apply(&ctx, temp.path(), "uv", None, &["sync".into()], &mut env)
            .await
            .unwrap();
        assert!(env.is_empty());
    }
    #[tokio::test]
    async fn clear_means_no_osdk_injection_and_unsafe_strategy_is_not_silently_changed() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        ctx.config.sources.registries.python.default = Some(String::new());
        ctx.config.sources.registries.python.indexes = Some(vec![]);
        let mut env = BTreeMap::new();
        apply(&ctx, temp.path(), "uv", None, &["sync".into()], &mut env)
            .await
            .unwrap();
        assert!(env.is_empty());
        ctx.config.sources.registries.python.indexes =
            Some(vec!["https://private.example/simple/".into()]);
        env.insert("UV_INDEX_STRATEGY".into(), "unsafe-best-match".into());
        assert!(
            apply(&ctx, temp.path(), "uv", None, &["sync".into()], &mut env)
                .await
                .is_err()
        );
    }
    #[test]
    fn python_detection_ignores_unrelated_freshness_inputs() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            ".python-version",
            ".nvmrc",
            "package.json",
            "rust-toolchain.toml",
        ] {
            std::fs::write(temp.path().join(name), "not uv TOML").unwrap();
        }
        assert!(!python_native_configuration(temp.path(), false, |_| None).unwrap());
        std::fs::write(
            temp.path().join("requirements.txt"),
            "--index-url https://native.example/simple/\n",
        )
        .unwrap();
        assert!(!python_native_configuration(temp.path(), false, |_| None).unwrap());
        assert!(!python_native_configuration(temp.path(), true, |_| None).unwrap());
        std::fs::write(temp.path().join("uv.toml"), "invalid TOML!").unwrap();
        assert!(python_native_configuration(temp.path(), false, |_| None).is_err());
    }

    #[tokio::test]
    async fn python_invocation_context_and_offline_environment_preserve_native_execution() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        ctx.config.sources.registries.python.default = Some("https://osdk.example/simple/".into());
        ctx.config.sources.registries.python.indexes =
            Some(vec!["https://extra.example/simple/".into()]);
        // Context overrides must bypass parsing the current project's configuration.
        std::fs::write(temp.path().join("uv.toml"), "invalid TOML!").unwrap();
        for (key, value) in [
            ("UV_CONFIG_FILE", "alternate.toml"),
            ("UV_NO_CONFIG", "1"),
            ("UV_NO_INDEX", "1"),
            ("UV_OFFLINE", "1"),
        ] {
            let mut env = BTreeMap::from([(key.into(), value.into())]);
            let before = env.clone();
            let evidence = apply(&ctx, temp.path(), "uv", None, &["sync".into()], &mut env)
                .await
                .unwrap();
            assert_eq!(env, before);
            assert!(evidence.contains("authoritative"));
        }
    }

    #[tokio::test]
    async fn python_explicit_first_index_is_accepted_but_unsafe_strategies_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        ctx.config.sources.registries.python.indexes =
            Some(vec!["https://extra.example/simple/".into()]);
        for flags in [
            vec!["--index-strategy", "first-index"],
            vec!["--index-strategy=first-index"],
        ] {
            let args = std::iter::once("sync")
                .chain(flags)
                .map(String::from)
                .collect::<Vec<_>>();
            let mut env = BTreeMap::new();
            apply(&ctx, temp.path(), "uv", None, &args, &mut env)
                .await
                .unwrap();
            assert_eq!(env["UV_INDEX_STRATEGY"], "first-index");
        }
        for flags in [
            vec!["--index-strategy", "unsafe-best-match"],
            vec!["--index-strategy=unsafe-first-match"],
            vec![
                "--index-strategy=first-index",
                "--index-strategy=unsafe-best-match",
            ],
        ] {
            let args = std::iter::once("sync")
                .chain(flags)
                .map(String::from)
                .collect::<Vec<_>>();
            assert!(
                apply(&ctx, temp.path(), "uv", None, &args, &mut BTreeMap::new())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn offline_never_probes_and_all_source_failure_stops_before_execution() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path());
        // Known closed port; HTTPS transport fails before any package manager starts.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}/", listener.local_addr().unwrap());
        drop(listener);
        ctx.config.sources.registries.npm.urls = vec![url];
        ctx.config.sources.registries.npm.probe_timeout_ms = 10;
        let mut env = BTreeMap::new();
        ctx.config.settings.offline = true;
        apply(
            &ctx,
            temp.path(),
            "npm",
            None,
            &["install".into()],
            &mut env,
        )
        .await
        .unwrap();
        assert!(env.is_empty());
        ctx.config.settings.offline = false;
        let error = apply(
            &ctx,
            temp.path(),
            "npm",
            None,
            &["install".into()],
            &mut env,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("command was not started"));
        assert!(env.is_empty());
    }
    #[test]
    fn native_changes_and_secret_rotation_affect_only_opaque_fingerprints() {
        let temp = tempfile::tempdir().unwrap();
        let first = fingerprint(temp.path(), "npm", "TOKEN=first-secret", &[]).unwrap();
        let changed = fingerprint(temp.path(), "npm", "TOKEN=second-secret", &[]).unwrap();
        assert_ne!(first, changed);
        assert!(!first.contains("secret"));
        let path = temp.path().join(".npmrc");
        std::fs::write(
            &path,
            "registry=https://one.example/\n_authToken=third-secret",
        )
        .unwrap();
        let added = fingerprint(temp.path(), "npm", "TOKEN=first-secret", &[]).unwrap();
        assert_ne!(first, added);
        std::fs::write(&path, "registry=https://two.example/").unwrap();
        assert_ne!(
            added,
            fingerprint(temp.path(), "npm", "TOKEN=first-secret", &[]).unwrap()
        );
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            first,
            fingerprint(temp.path(), "npm", "TOKEN=first-secret", &[]).unwrap()
        );
    }
    #[test]
    fn managed_config_creation_and_changes_invalidate_freshness() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed-node/etc/npmrc");
        let paths = vec![path.clone()];
        let before = fingerprint(temp.path(), "npm", "install", &paths).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "registry=https://configured.example/").unwrap();
        assert_ne!(
            before,
            fingerprint(temp.path(), "npm", "install", &paths).unwrap()
        );
    }

    #[test]
    fn unknown_and_unsupported_fields_fail_without_legacy_aliases() {
        for text in [
            "[cargo]\nindex='https://example/'",
            "[deno]\nregistry='https://example/'",
            "[npm]\nscopes={corp='https://example/'}",
        ] {
            assert!(toml::from_str::<RegistriesConfig>(text).is_err());
        }
        for provider in ["cargo", "deno", "custom"] {
            assert!(validate_override(provider, &mut RegistriesConfig::default()).is_err());
        }
        #[cfg(feature = "install")]
        for text in [
            "index='https://example/'",
            "extra_index='https://example/'",
            "registry='https://example/'",
        ] {
            let error = toml::from_str::<crate::config::DepsProviderEntry>(text).unwrap_err();
            assert!(error.message().contains("unknown field"));
        }
    }
}

#[cfg(test)]
mod native_candidates_tests {
    use super::*;
    #[test]
    fn python_system_configuration_candidates_are_tracked() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("system");
        let program_data = temp.path().join("program-data");
        let paths = native_files(temp.path(), "uv", &|key| match key {
            "XDG_CONFIG_DIRS" => Some(system.display().to_string()),
            "PROGRAMDATA" => Some(program_data.display().to_string()),
            _ => None,
        });
        assert!(paths.contains(&system.join("uv/uv.toml")));
        assert!(paths.contains(&program_data.join("uv/uv.toml")));
        #[cfg(not(windows))]
        assert!(paths.contains(&std::path::PathBuf::from("/etc/uv/uv.toml")));
    }

    #[test]
    fn nested_workspace_and_user_candidates_are_tracked_without_claiming_a_winner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cwd = root.join("workspace/packages/app");
        std::fs::create_dir_all(&cwd).unwrap();
        for (program, names) in [
            ("pnpm", vec![".npmrc", "pnpm-workspace.yaml"]),
            ("yarn", vec![".yarnrc", ".yarnrc.yml"]),
            ("bun", vec!["bunfig.toml", ".npmrc"]),
        ] {
            let paths = native_files(&cwd, program, &|key| {
                (key == "HOME").then(|| root.join("home").display().to_string())
            });
            for name in names {
                assert!(paths.contains(&cwd.join(name)));
                assert!(paths.contains(&root.join("workspace").join(name)));
                assert!(paths.contains(&root.join("home").join(name)));
            }
        }
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    fn write(root: &Path, name: &str, text: &str) {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    #[test]
    fn uv_toml_hides_sibling_settings_and_nearest_hides_parent() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(p, "uv.toml", "malformed!");
        write(
            p,
            "child/uv.toml",
            "index-url='https://native.example/simple'",
        );
        write(p, "child/pyproject.toml", "[tool.uv]\nindex=false");
        let d = discover_python_configuration(&p.join("child"), false, &|_| None).unwrap();
        assert_eq!(d.files, vec![p.join("child/uv.toml")]);
        assert!(!d.complex);
    }
    #[test]
    fn workspace_members_are_delegated_without_parsing_member_config() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(
            p,
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers=['packages/*']",
        );
        write(p, "packages/a/uv.toml", "malformed!");
        let d = discover_python_configuration(&p.join("packages/a"), false, &|_| None).unwrap();
        assert!(d.workspace);
        assert_eq!(d.files, vec![p.join("pyproject.toml")]);
    }
    #[test]
    fn user_and_first_system_files_are_candidates_not_field_winners() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        for name in ["project/uv.toml", "user/uv/uv.toml", "system/uv/uv.toml"] {
            write(p, name, "index-url='https://native.example/simple'");
        }
        let d = discover_python_configuration(&p.join("project"), false, &|key| match key {
            "XDG_CONFIG_HOME" | "APPDATA" => Some(p.join("user").display().to_string()),
            "XDG_CONFIG_DIRS" | "PROGRAMDATA" => Some(p.join("system").display().to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(d.files.len(), 3);
    }
    #[test]
    fn uv_boolean_and_global_argument_semantics() {
        for value in ["0", "false", "FALSE", "no", "off", ""] {
            assert!(!uv_env_enabled(Some(value)));
        }
        for value in ["1", "true", "unknown"] {
            assert!(uv_env_enabled(Some(value)));
        }
        for raw in [
            vec!["--verbose", "sync"],
            vec!["--directory", "somewhere", "pip", "install"],
            vec!["--no-config", "sync"],
            vec!["--offline", "lock"],
        ] {
            let args = raw.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(uv_fetch(uv_command_args(&args)));
        }
    }
    #[tokio::test]
    async fn false_booleans_do_not_disable_osdk_indexes_but_explicit_context_does() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let mut ctx = super::tests::context(p);
        ctx.config.sources.registries.python.default = Some("https://osdk.example/simple/".into());
        for value in ["0", "false", "no", "off"] {
            let mut env = BTreeMap::from([
                ("UV_NO_CONFIG".into(), value.into()),
                ("UV_OFFLINE".into(), value.into()),
            ]);
            apply(
                &ctx,
                p,
                "uv",
                None,
                &["--verbose".into(), "sync".into()],
                &mut env,
            )
            .await
            .unwrap();
            assert_eq!(env["UV_DEFAULT_INDEX"], "https://osdk.example/simple/");
        }
        write(p, "uv.toml", "malformed!");
        for flags in [
            vec!["--config-file", "elsewhere.toml"],
            vec!["--project", "elsewhere"],
            vec!["--directory", "elsewhere"],
            vec!["-Celsewhere"],
            vec!["-ihttps://cli.example/simple"],
            vec!["--no-config"],
            vec!["--offline"],
        ] {
            let args = flags
                .into_iter()
                .chain(["sync"])
                .map(String::from)
                .collect::<Vec<_>>();
            let mut env = BTreeMap::new();
            let evidence = apply(&ctx, p, "uv", None, &args, &mut env).await.unwrap();
            assert!(env.is_empty());
            assert!(evidence.contains("authoritative"));
        }
    }
    #[test]
    fn freshness_tracks_creation_deletion_policy_and_invocation_material() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let first = fingerprint(p, "uv", "policy=native;--offline", &[]).unwrap();
        for name in [
            ".nvmrc",
            ".node-version",
            "package.json",
            "uv.toml",
            "pyproject.toml",
        ] {
            write(p, name, "secret");
            let changed = fingerprint(p, "uv", "policy=native;--offline", &[]).unwrap();
            assert_ne!(first, changed);
            assert!(!changed.contains("secret"));
            std::fs::remove_file(p.join(name)).unwrap();
            assert_eq!(
                first,
                fingerprint(p, "uv", "policy=native;--offline", &[]).unwrap()
            );
        }
        assert_ne!(
            first,
            fingerprint(p, "uv", "policy=osdk;--offline", &[]).unwrap()
        );
        assert_ne!(
            first,
            fingerprint(p, "uv", "policy=native;--project=other", &[]).unwrap()
        );
    }
}

#[cfg(test)]
mod invocation_tests {
    use super::*;
    #[tokio::test]
    async fn plain_requirements_receive_indexes_but_native_directives_are_preserved() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let mut ctx = super::tests::context(p);
        ctx.config.sources.registries.python.default =
            Some("https://default.example/simple/".into());
        ctx.config.sources.registries.python.indexes = Some(vec![
            "https://first.example/simple/".into(),
            "https://second.example/simple/".into(),
        ]);
        let args = ["pip", "install", "-r", "requirements.txt"].map(String::from);
        std::fs::write(p.join("requirements.txt"), "fixture==1.0\n").unwrap();
        let mut env = BTreeMap::new();
        apply(&ctx, p, "uv", None, &args, &mut env).await.unwrap();
        assert_eq!(
            env["UV_INDEX"],
            "https://first.example/simple/ https://second.example/simple/"
        );
        assert_eq!(env["UV_DEFAULT_INDEX"], "https://default.example/simple/");
        std::fs::write(
            p.join("requirements.txt"),
            "--index-url https://user:SECRET@native.example/simple\nfixture==1.0\n",
        )
        .unwrap();
        let mut env = BTreeMap::new();
        let reason = apply(&ctx, p, "uv", None, &args, &mut env).await.unwrap();
        assert!(env.is_empty());
        assert!(!reason.contains("SECRET"));
    }
    #[tokio::test]
    async fn constraints_and_positional_inputs_preserve_native_options() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        let mut ctx = super::tests::context(p);
        ctx.config.sources.registries.python.default = Some("https://osdk.example/simple".into());
        std::fs::write(
            p.join("constraints.txt"),
            "--index-url https://user:SECRET@native.example/simple",
        )
        .unwrap();
        for raw in [
            vec!["pip", "install", "fixture", "-cconstraints.txt"],
            vec!["pip", "compile", "constraints.txt"],
            vec!["pip", "sync", "constraints.txt"],
        ] {
            let args = raw.into_iter().map(String::from).collect::<Vec<_>>();
            let mut env = BTreeMap::new();
            let reason = apply(&ctx, p, "uv", None, &args, &mut env).await.unwrap();
            assert!(env.is_empty());
            assert!(reason.contains("delegated"));
            assert!(!reason.contains("SECRET"));
        }
        let args = [
            "-C",
            "other",
            "pip",
            "install",
            "-rreq.txt",
            "--config-file",
            "alternate.toml",
        ]
        .map(String::from);
        let paths = invocation_files(p, "uv", &args);
        assert!(paths.contains(&p.join("other/req.txt")));
        assert!(paths.contains(&p.join("other/alternate.toml")));
    }
    #[tokio::test]
    async fn ambient_indexes_do_not_trigger_an_unused_complex_overlay() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        std::fs::write(
            p.join("uv.toml"),
            "[[index]]\nname='internal'\nurl='https://native.example/simple'\n",
        )
        .unwrap();
        let mut ctx = super::tests::context(p);
        ctx.config.sources.registries.python.default = Some("https://osdk.example/simple".into());
        let mut env = BTreeMap::from([(
            "UV_DEFAULT_INDEX".into(),
            "https://invocation.example/simple".into(),
        )]);
        let reason = apply(&ctx, p, "uv", None, &["sync".into()], &mut env)
            .await
            .unwrap();
        assert_eq!(env["UV_DEFAULT_INDEX"], "https://invocation.example/simple");
        assert!(reason.contains("invocation environment"));
    }
    #[test]
    fn explicit_files_and_contexts_invalidate_fingerprints() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        std::fs::create_dir_all(p.join("other")).unwrap();
        let args = [
            "--config-file",
            "alternate.toml",
            "--project",
            "other",
            "sync",
        ]
        .map(String::from);
        let paths = invocation_files(p, "uv", &args);
        assert!(paths.contains(&p.join("alternate.toml")));
        assert!(paths.contains(&p.join("other/uv.toml")));
        let before = fingerprint(p, "uv", "invocation", &paths).unwrap();
        std::fs::write(p.join("alternate.toml"), "secret").unwrap();
        assert_ne!(before, fingerprint(p, "uv", "invocation", &paths).unwrap());
    }
}
