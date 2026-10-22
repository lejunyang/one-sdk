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
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd");
    if let Some(manager) = package_registry::manager_for_command(program, yarn_version) {
        let result = package_registry::plan(ctx, cwd, manager, program, args, |key| {
            env.get(key).cloned().or_else(|| std::env::var(key).ok())
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
                    ctx.config.settings.precedence.packages
                ))
            }
            RegistryPlan::Unavailable { .. } => Err(Error::config(
                "no reachable package registry: all candidates failed; command was not started",
            )),
        };
    }
    if program == "go" && args.first().is_some_and(|arg| arg == "mod") {
        if let Some(proxy) = &ctx.config.registries().go.proxy {
            let getenv = |key: &str| env.get(key).cloned().or_else(|| std::env::var(key).ok());
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
            if ctx.config.settings.precedence.packages == SourcePrecedence::Native && native_env {
                return Ok(
                    "native Go environment file retained; effective GOPROXY resolved by Go".into(),
                );
            }
            if !proxy.is_empty()
                && !env.contains_key("GOPROXY")
                && std::env::var_os("GOPROXY").is_none()
            {
                env.insert("GOPROXY".into(), proxy.clone());
            }
        }
        return Ok("Go delegates proxy chains, credentials and fallback to GOPROXY".into());
    }
    if matches!(program, "yarn" | "yarnpkg") && yarn_version.is_none() {
        return Ok("Yarn major is unknown; native configuration preserved".into());
    }
    if program != "uv" || !uv_fetch(args) {
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
            "--offline",
            "--directory",
            "--project",
            "--config-file",
            "--no-config",
        ]
        .iter()
        .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
    }) {
        return Ok("uv command-line configuration/offline is authoritative".into());
    }
    let settings = &ctx.config.registries().python;
    let getenv = |key: &str| env.get(key).cloned().or_else(|| std::env::var(key).ok());
    let ambient_default = ["UV_DEFAULT_INDEX", "UV_INDEX_URL", "UV_NO_INDEX"]
        .iter()
        .any(|key| getenv(key).is_some());
    let ambient_indexes = ["UV_INDEX", "UV_EXTRA_INDEX_URL"]
        .iter()
        .any(|key| getenv(key).is_some());
    let native = python_native_configuration(cwd, getenv)?;
    let native_first = ctx.config.settings.precedence.packages == SourcePrecedence::Native;
    if native_first && native {
        return Ok(
            "native Python configuration has priority; credentials and package bindings preserved"
                .into(),
        );
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
            if args.iter().any(|arg| arg.starts_with("--index-strategy")) {
                return Err(Error::config("configured Python indexes require first-index; leave strategy in native configuration instead"));
            }
            if env
                .get("UV_INDEX_STRATEGY")
                .cloned()
                .or_else(|| std::env::var("UV_INDEX_STRATEGY").ok())
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
            "Python default/additional indexes resolved; packages={:?}; no mirror fallback",
            ctx.config.settings.precedence.packages
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

/// Conservative native detection: unknown configuration is delegated intact.
/// A malformed known TOML config is an error, not evidence of absence.
fn python_native_configuration(
    cwd: &Path,
    getenv: impl Fn(&str) -> Option<String>,
) -> Result<bool> {
    for key in [
        "UV_CONFIG_FILE",
        "UV_NO_CONFIG",
        "UV_DEFAULT_INDEX",
        "UV_INDEX_URL",
        "UV_INDEX",
        "UV_EXTRA_INDEX_URL",
        "UV_NO_INDEX",
        "UV_FIND_LINKS",
        "UV_INDEX_STRATEGY",
    ] {
        if getenv(key).is_some() {
            return Ok(true);
        }
    }
    let mut paths = native_files(cwd, "uv", &getenv);
    paths.extend(cwd.ancestors().map(|dir| dir.join("requirements.txt")));
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        if path.extension().is_some_and(|ext| ext == "toml") {
            let value: toml::Value = toml::from_str(&text)
                .map_err(|_| Error::config("invalid native Python TOML configuration"))?;
            if path
                .file_name()
                .is_some_and(|name| name == "pyproject.toml")
            {
                if value.get("tool").and_then(|tool| tool.get("uv")).is_some() {
                    return Ok(true);
                }
            } else {
                return Ok(true);
            }
        } else if text.lines().any(|line| line.trim_start().starts_with('-')) {
            return Ok(true);
        }
    }
    Ok(false)
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
    paths.sort();
    paths.dedup();
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
    fn context(root: &Path) -> Ctx {
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
        for args in [
            vec!["sync".into()],
            vec![
                "pip".into(),
                "install".into(),
                "-r".into(),
                "requirements.txt".into(),
            ],
        ] {
            let mut env = BTreeMap::from([("UV_INDEX_CORP_PASSWORD".into(), "secret".into())]);
            apply(&ctx, temp.path(), "uv", None, &args, &mut env)
                .await
                .unwrap();
            assert_eq!(env["UV_DEFAULT_INDEX"], "https://default.example/simple/");
            assert_eq!(
                env["UV_INDEX"],
                "https://private.example/simple/ https://second.example/simple/"
            );
            assert_eq!(env["UV_INDEX_STRATEGY"], "first-index");
            assert_eq!(env["UV_INDEX_CORP_PASSWORD"], "secret");
        }
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
