//! `osdk-shim`: the tiny launcher every shim points at.
//!
//! It learns which tool to run from `argv[0]` (the shim's own name), resolves
//! the active version for the current directory (walking up config files), then
//! `exec`s the real binary from that version's install dir. Everything here is
//! synchronous and avoids a tokio runtime / network to keep per-call overhead
//! minimal.

use std::path::PathBuf;
use std::process::Command;

use osdk_core::backend::registry::Registry;
use osdk_core::config::Config;
use osdk_core::dirs::Dirs;
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

    let dirs = match Dirs::resolve() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = Config::load(&dirs.user_config_file(), &cwd).unwrap_or_else(|_| {
        // Fall back to defaults if config fails to load; shims should be robust.
        Config {
            settings: Default::default(),
            sources: Default::default(),
            tools: Default::default(),
            aliases: Default::default(),
            project_config_path: None,
        }
    });

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
    let backend = match owning_backend(&registry, &ctx, &tool_name) {
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
    let spec = match active {
        Some(av) => av.spec,
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
    let version = match resolve_installed(&ctx, backend.as_ref(), &expanded_spec) {
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

    let exe = match find_exe(&bin_dirs, &tool_name) {
        Some(p) => p,
        None => {
            eprintln!(
                "osdk-shim: `{tool_name}` not found in {}@{}",
                backend.id(),
                version
            );
            return 127;
        }
    };

    let exec_env = match backend.exec_env(&ctx, &tv) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("osdk-shim: {e}");
            return 1;
        }
    };

    exec(&exe, forward_args, &exec_env)
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
    tool_name: &str,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
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
) -> Option<String> {
    let installed = backend.list_installed(ctx).ok()?;
    if installed.is_empty() {
        return None;
    }
    // Strip a leading distribution prefix like `temurin-` (java) so the version
    // part matches the installed dir names (e.g. `17.0.20+8`).
    let spec = strip_distribution_prefix(spec);
    let parsed = VersionSpec::parse(spec);
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
    match Command::new(exe).args(args).envs(env).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("osdk-shim: failed to run {}: {e}", exe.display());
            126
        }
    }
}
