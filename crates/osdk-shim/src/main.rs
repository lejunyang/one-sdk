//! `osdk-shim`: the tiny launcher every shim points at.
//!
//! It learns which tool to run from `argv[0]` (the shim's own name), resolves
//! the active version for the current directory (walking up config files), then
//! `exec`s the real binary from that version's install dir. Ordinary tools stay
//! on a synchronous hot path; dependency-fetching package-manager commands
//! create a short-lived current-thread runtime for a fresh registry preflight.

use std::path::PathBuf;
use std::process::Command;

use osdk_core::backend::registry::Registry;
use osdk_core::config::Config;
use osdk_core::dirs::Dirs;
use osdk_core::package_registry::{
    manager_for_command, plan, registry_env, should_plan, PackageManager, RegistryPlan,
};
use osdk_core::platform::Platform;
use osdk_core::version::resolver::resolve_active;
use osdk_core::version::{select_version, ToolVersion, VersionSpec};

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    // The tool name is argv[0]'s basename (e.g. the shim named "node"), unless
    // invoked directly as "osdk-shim <tool> <args...>" (windows .cmd wrapper).
    let (tool_name, forward_args) = parse_invocation(&args);
    let tool_name = match tool_name {
        Some(t) => t,
        None => {
            eprintln!("osdk-shim: could not determine tool name from argv[0]");
            return 1;
        }
    };
    if std::env::var_os("OSDK_SHIM_ACTIVE").is_some() {
        eprintln!("osdk-shim: recursive shim invocation for `{tool_name}`");
        return 126;
    }

    let dirs = match Dirs::resolve() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Err(error) = ensure_project_config_trusted(&dirs, &cwd) {
        eprintln!("osdk-shim: {error}");
        return 1;
    }
    let config = match Config::load(&dirs.user_config_file(), &cwd) {
        Ok(config) => config,
        Err(error) => {
            // A malformed configuration may contain execution-affecting
            // registry policy. Silently replacing it with public defaults
            // would make the shim behave differently from `osdk exec` and
            // could send requests to an unintended endpoint.
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };

    let registry = match Registry::load(&dirs) {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };

    // Build the (sync) context up front; needed to resolve which backend owns
    // this tool name by scanning installed bin names.
    let platform = Platform::current();
    let tools = config.tools.clone();
    let idiomatic_probe_cwd = cwd.clone();
    let ctx = make_ctx(dirs.clone(), platform, config);

    // Find which backend owns this tool name (its id, or one of the executables
    // an installed version provides, e.g. pip -> python, npm -> node).
    let backend = match owning_backend(&registry, &ctx, &idiomatic_probe_cwd, &tool_name) {
        Some(b) => b,
        None => {
            eprintln!("osdk-shim: no backend provides `{tool_name}`");
            return 127;
        }
    };

    // Resolve the active version spec for this backend.
    let active = resolve_active(
        backend.id(),
        &idiomatic_probe_cwd,
        &tools,
        backend.idiomatic_files(),
    );
    let (spec, is_range) = match active {
        Some(av) => (av.spec, av.is_range),
        None => {
            eprintln!(
                "osdk-shim: no version of `{}` selected (set one with `osdk use {}@<version>`)",
                backend.id(),
                backend.id()
            );
            return 1;
        }
    };

    // Resolve spec -> concrete installed version (offline: pick from installed).
    let expanded_spec = match ctx.config.expand_alias(backend.id(), &spec) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    let version = match resolve_installed(&ctx, backend.as_ref(), &expanded_spec, is_range) {
        Some(v) => v,
        None => {
            eprintln!(
                "osdk-shim: `{}@{}` is not installed (run `osdk install {}@{}`)",
                backend.id(),
                spec,
                backend.id(),
                spec
            );
            return 1;
        }
    };

    let tv = ToolVersion::new(backend.id(), &version);
    let bin_dirs = match backend.bin_paths(&ctx, &tv) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };

    let (executable_name, alias_subcommand) = routed_launcher(&tool_name, backend.id());
    let exe = match find_exe(&bin_dirs, executable_name) {
        Some(p) => p,
        None => {
            eprintln!(
                "osdk-shim: `{executable_name}` not found in {}@{}",
                backend.id(),
                version
            );
            return 127;
        }
    };

    let mut exec_env = match backend.exec_env(&ctx, &tv) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    // Activated shells put the shim directory on PATH. Never let lifecycle
    // subprocesses re-enter the shim: expose the owning backend's real bins
    // (plus Node for JavaScript launchers) ahead of the inherited PATH.
    // Remove both lexical and canonical matches so symlinked activation paths
    // cannot retain the shim directory under a different spelling.
    remove_env_path(&mut exec_env, &ctx.dirs.shims());
    if matches!(backend.id(), "npm" | "pnpm" | "yarn") {
        let node_backend = registry.get("node").unwrap();
        let active_node = resolve_active(
            "node",
            &idiomatic_probe_cwd,
            &tools,
            &[".nvmrc", ".node-version"],
        );
        let node_version = active_node
            .as_ref()
            .and_then(|active| {
                resolve_installed(&ctx, &*node_backend, &active.spec, active.is_range)
            })
            .or_else(|| node_backend.list_installed(&ctx).ok()?.into_iter().last());
        let Some(node_version) = node_version else {
            eprintln!(
                "osdk-shim: `{}` requires a managed Node installation",
                backend.id()
            );
            return 1;
        };
        let node = ToolVersion::new("node", node_version);
        if let Ok(paths) = node_backend.bin_paths(&ctx, &node) {
            prepend_env_path(&mut exec_env, paths);
        }
    }
    prepend_env_path(&mut exec_env, bin_dirs);

    if let Some(manager) = package_manager_for_backend(backend.id(), &tool_name, &version) {
        if should_plan(manager, &tool_name, forward_args) {
            if let Err(error) = apply_registry_preflight(
                &ctx,
                &cwd,
                manager,
                &tool_name,
                forward_args,
                &mut exec_env,
            ) {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    }

    exec_env.insert("OSDK_SHIM_ACTIVE".into(), tool_name);
    let routed_args;
    let exec_args = if let Some(subcommand) = alias_subcommand {
        routed_args = std::iter::once(subcommand.to_string())
            .chain(forward_args.iter().cloned())
            .collect::<Vec<_>>();
        routed_args.as_slice()
    } else {
        forward_args
    };
    exec(&exe, exec_args, &exec_env)
}

fn routed_launcher<'a>(tool_name: &'a str, backend: &str) -> (&'a str, Option<&'static str>) {
    match (backend, tool_name) {
        ("pnpm", "pnpx") => ("pnpm", Some("dlx")),
        ("bun", "bunx") => ("bun", Some("x")),
        _ => (tool_name, None),
    }
}

fn ensure_project_config_trusted(dirs: &Dirs, cwd: &std::path::Path) -> Result<(), String> {
    let Some(project_config) = osdk_core::trust::project_config(cwd).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    if !osdk_core::trust::requires_trust(&project_config).map_err(|e| e.to_string())? {
        return Ok(());
    }
    let trusted_paths = std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS");
    if osdk_core::trust::is_trusted(&dirs.config, &project_config, trusted_paths.as_ref())
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    Err(osdk_core::t!(
        "err.untrusted_config",
        path = project_config.display()
    ))
}

fn package_manager_for_backend(
    backend: &str,
    executable_alias: &str,
    backend_version: &str,
) -> Option<PackageManager> {
    let belongs_to_manager = match backend {
        // npm/npx may be supplied by the independent npm backend or by a Node
        // installation. npm registry behavior is version-independent.
        "npm" | "node" => matches!(executable_alias, "npm" | "npx"),
        "pnpm" => matches!(executable_alias, "pnpm" | "pnpx"),
        "yarn" => matches!(executable_alias, "yarn" | "yarnpkg"),
        "bun" => matches!(executable_alias, "bun" | "bunx"),
        "deno" => executable_alias == "deno",
        _ => false,
    };
    belongs_to_manager
        .then(|| manager_for_command(executable_alias, Some(backend_version)))
        .flatten()
}

fn apply_registry_preflight(
    ctx: &osdk_core::backend::Ctx,
    cwd: &std::path::Path,
    manager: PackageManager,
    executable_alias: &str,
    args: &[String],
    exec_env: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| osdk_core::t!("err.registry_preflight_runtime", error = error))?;
    let registry_plan = runtime
        .block_on(plan(ctx, cwd, manager, executable_alias, args, |key| {
            std::env::var(key).ok()
        }))
        .map_err(|error| osdk_core::t!("err.registry_preflight", error = error))?;
    match registry_plan {
        RegistryPlan::PassThrough { .. } => Ok(()),
        RegistryPlan::Selected { url, .. } => {
            exec_env.insert(registry_env(manager).into(), url);
            Ok(())
        }
        RegistryPlan::Unavailable { probes } => {
            let details = probes
                .iter()
                .map(|probe| {
                    probe.error.as_deref().map_or_else(
                        || probe.url.clone(),
                        |error| format!("{}: {error}", probe.url),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            if details.is_empty() {
                Err(osdk_core::t!(
                    "err.registry_unavailable",
                    executable = executable_alias
                ))
            } else {
                Err(osdk_core::t!(
                    "err.registry_unavailable_details",
                    executable = executable_alias,
                    details = details
                ))
            }
        }
    }
}

fn prepend_env_path(env: &mut std::collections::BTreeMap<String, String>, paths: Vec<PathBuf>) {
    let existing = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let mut combined = paths;
    combined.extend(std::env::split_paths(&existing));
    if let Ok(value) = std::env::join_paths(combined) {
        env.insert("PATH".into(), value.to_string_lossy().into_owned());
    }
}

fn remove_env_path(env: &mut std::collections::BTreeMap<String, String>, remove: &std::path::Path) {
    let existing = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let remove_canonical = std::fs::canonicalize(remove).ok();
    let retained = std::env::split_paths(&existing)
        .filter(|path| {
            path != remove
                && remove_canonical.as_ref().is_none_or(|canonical| {
                    std::fs::canonicalize(path)
                        .map(|path| path != *canonical)
                        .unwrap_or(true)
                })
        })
        .collect::<Vec<_>>();
    if let Ok(value) = std::env::join_paths(retained) {
        env.insert("PATH".into(), value.to_string_lossy().into_owned());
    }
}

/// Determine the tool name and args to forward.
fn parse_invocation(args: &[String]) -> (Option<String>, &[String]) {
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("");
    let base = basename_no_ext(argv0);
    if base == "osdk-shim" {
        // Direct form: osdk-shim <tool> <args...>
        let tool = args.get(1).map(|s| basename_no_ext(s));
        let rest = if args.len() > 2 { &args[2..] } else { &[] };
        (tool, rest)
    } else if base.is_empty() {
        (None, &[])
    } else {
        (Some(base), &args[1..])
    }
}

fn basename_no_ext(p: &str) -> String {
    let name = std::path::Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    #[cfg(windows)]
    {
        for ext in [".exe", ".cmd", ".bat"] {
            if name.to_ascii_lowercase().ends_with(ext) {
                return name[..name.len() - ext.len()].to_string();
            }
        }
    }
    name
}

/// Find the backend that owns a tool name. First checks backend ids directly,
/// then scans each backend's exposed bin names across installed versions so
/// tools like `pip`, `npm`, `pnpx`, `gofmt`, `cargo` route to the right SDK.
/// Also scans dynamically-installed `github:owner/repo` backends.
fn owning_backend(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    cwd: &std::path::Path,
    tool_name: &str,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
    if matches!(tool_name, "npm" | "npx") {
        let npm = registry.get("npm").ok()?;
        // An explicit independent npm selection is authoritative, including
        // when its selected version is missing: do not silently fall back to
        // the bundled copy and hide a broken project pin.
        if resolve_active("npm", cwd, &ctx.config.tools, npm.idiomatic_files()).is_some() {
            return Some(npm);
        }

        // Node intentionally does not claim npm/npx in `bin_names`, because
        // the independent npm backend owns those public tool IDs. A routing
        // shim may still dispatch to the selected Node installation's bundled
        // launcher when no independent npm version is selected.
        let node = registry.get("node").ok()?;
        if let Some(active) = resolve_active("node", cwd, &ctx.config.tools, node.idiomatic_files())
        {
            if let Some(version) =
                resolve_installed(ctx, node.as_ref(), &active.spec, active.is_range)
            {
                let version = ToolVersion::new("node", version);
                if node
                    .bin_paths(ctx, &version)
                    .ok()
                    .and_then(|paths| find_exe(&paths, tool_name))
                    .is_some()
                {
                    return Some(node);
                }
            }
        }

        return (tool_name == "npm").then_some(npm);
    }
    if let Ok(b) = registry.get(tool_name) {
        return Some(b);
    }
    // Scan compiled-in backends' installed versions' bin names.
    for backend in registry.all() {
        if let Ok(versions) = backend.list_installed(ctx) {
            for v in versions {
                let tv = ToolVersion::new(backend.id(), &v);
                if let Ok(names) = backend.bin_names(ctx, &tv) {
                    if names.iter().any(|n| n == tool_name) {
                        return Some(backend.clone());
                    }
                }
            }
        }
    }
    // Scan dynamically-installed github backends: installs/github/<owner>/<repo>.
    for id in installed_github_ids(ctx) {
        if let Ok(backend) = registry.get(&id) {
            if let Ok(versions) = backend.list_installed(ctx) {
                for v in versions {
                    let tv = ToolVersion::new(backend.id(), &v);
                    if let Ok(names) = backend.bin_names(ctx, &tv) {
                        if names.iter().any(|n| n == tool_name) {
                            return Some(backend);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Enumerate installed `github:owner/repo` ids from the installs tree.
fn installed_github_ids(ctx: &osdk_core::backend::Ctx) -> Vec<String> {
    let mut out = Vec::new();
    let base = ctx.dirs.installs.join("github");
    let owners = match std::fs::read_dir(&base) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for owner in owners.flatten() {
        if !owner.path().is_dir() {
            continue;
        }
        let owner_name = owner.file_name().to_string_lossy().to_string();
        if let Ok(repos) = std::fs::read_dir(owner.path()) {
            for repo in repos.flatten() {
                if repo.path().is_dir() {
                    let repo_name = repo.file_name().to_string_lossy().to_string();
                    out.push(format!("github:{owner_name}/{repo_name}"));
                }
            }
        }
    }
    out
}

fn make_ctx(dirs: Dirs, platform: Platform, config: Config) -> osdk_core::backend::Ctx {
    use std::sync::Arc;
    // A minimal client is required by Ctx; the shim never uses it for network.
    let client = osdk_core::http::client().unwrap_or_default();
    let cas = Arc::new(osdk_core::store::Cas::new(dirs.store.clone()));
    osdk_core::backend::Ctx {
        dirs,
        platform,
        config,
        client,
        cas,
        show_progress: false,
    }
}

/// Resolve a spec against locally installed versions (no network).
fn resolve_installed(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    spec: &str,
    is_range: bool,
) -> Option<String> {
    let installed = backend.list_installed(ctx).ok()?;
    if installed.is_empty() {
        return None;
    }
    if backend.id() == "python" {
        return osdk_core::backend::python::select_installed(spec, &installed);
    }
    // Strip a leading distribution prefix like `temurin-` (java) so the version
    // part matches the installed dir names (e.g. `17.0.20+8`).
    let spec = strip_distribution_prefix(spec);
    let parsed = if is_range {
        VersionSpec::parse_range(spec).ok()?
    } else {
        VersionSpec::parse(spec)
    };
    match &parsed {
        VersionSpec::Exact(v) => installed.iter().find(|i| *i == v).cloned(),
        _ => {
            let infos: Vec<_> = installed
                .iter()
                .map(osdk_core::version::VersionInfo::stable)
                .collect();
            select_version(&parsed, &infos).map(|vi| vi.version.clone())
        }
    }
}

/// Strip a leading `<word>-` distribution prefix (e.g. `temurin-17` -> `17`).
/// Only strips when the left side is purely alphabetic, so real versions like
/// `1.22` or prereleases are untouched.
fn strip_distribution_prefix(spec: &str) -> &str {
    if let Some((left, right)) = spec.split_once('-') {
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_alphabetic()) && !right.is_empty() {
            return right;
        }
    }
    spec
}

fn find_exe(bin_dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let candidates = exe_candidates(name);
    for dir in bin_dirs {
        for cand in &candidates {
            let p = dir.join(cand);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(windows)]
fn exe_candidates(name: &str) -> Vec<String> {
    vec![
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.to_string(),
    ]
}

#[cfg(not(windows))]
fn exe_candidates(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[cfg(unix)]
fn exec(exe: &PathBuf, args: &[String], env: &std::collections::BTreeMap<String, String>) -> i32 {
    use std::os::unix::process::CommandExt;
    // Replace the current process so signals/exit codes pass through cleanly.
    let err = Command::new(exe).args(args).envs(env).exec();
    eprintln!("osdk-shim: failed to exec {}: {err}", exe.display());
    126
}

#[cfg(not(unix))]
fn exec(exe: &PathBuf, args: &[String], env: &std::collections::BTreeMap<String, String>) -> i32 {
    let extension = exe
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let mut command =
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let mut command = Command::new(
                std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
            );
            command.args(["/D", "/S", "/C", "call"]).arg(exe);
            command
        } else {
            Command::new(exe)
        };
    match command.args(args).envs(env).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("osdk-shim: failed to run {}: {e}", exe.display());
            126
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn batch_targets_run_through_comspec() {
        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("fixture with spaces.cmd");
        let output = temporary.path().join("output.txt");
        std::fs::write(
            &script,
            "@echo off\r\n> \"%~dp0output.txt\" echo %~1\r\nexit /b 23\r\n",
        )
        .unwrap();

        let code = exec(
            &script,
            &["forwarded value".into()],
            &std::collections::BTreeMap::new(),
        );

        assert_eq!(code, 23);
        assert_eq!(
            std::fs::read_to_string(output).unwrap().trim(),
            "forwarded value"
        );
    }
}
