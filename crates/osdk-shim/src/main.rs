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
use osdk_core::inventory::ScanReport;
use osdk_core::package_registry::{manager_for_command, PackageManager};
use osdk_core::platform::Platform;
use osdk_core::version::resolver::resolve_active_config;
use osdk_core::version::{select_version, ToolVersion, VersionSpec};

// The shim resolves and execs already-installed tools; it never installs. Its
// whole size advantage comes from `osdk-core` being compiled without the
// `install` feature, which drops the download pipeline and sigstore
// verification (measured 7.38 MB -> 3.18 MB).
//
// Cargo unifies features across a single `cargo build --workspace`, which
// re-enables `install` for this binary and silently gives back the saving with a
// perfectly working executable and no warning. Fail the build instead.
// Only release builds ship, so only they are checked; this keeps
// `cargo check/test/clippy --workspace` usable during development.
#[cfg(not(debug_assertions))]
const _: () = assert!(
    !osdk_core::INSTALL_PATH_LINKED,
    "osdk-shim was built with osdk-core's `install` feature enabled, which links \
     the download pipeline and sigstore verification into the shim and roughly \
     doubles its size. This is usually Cargo feature unification from building \
     several workspace members at once: build the shim in its own invocation \
     (`cargo build -p osdk-shim`) rather than with `--workspace`."
);

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let lang = osdk_core::i18n::detect(None, |key| std::env::var(key).ok());
    osdk_core::i18n::set_lang(lang);
    // The tool name is argv[0]'s basename (e.g. the shim named "node"), unless
    // invoked directly as "osdk-shim <tool> <args...>" (windows .cmd wrapper).
    // Answer self-identification before touching any state. These need no
    // config, no registry and no inventory, and must keep working when the tree
    // is in a state this build cannot read -- otherwise the one command that
    // reveals which shim build is installed is unavailable exactly when a
    // version mismatch is the thing being diagnosed (docs/bugs/007).
    if is_direct_invocation(&args)
        && matches!(
            args.get(1).map(String::as_str),
            Some("--version") | Some("-V")
        )
    {
        println!("osdk-shim {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let (tool_name, forward_args) = parse_invocation(&args);
    let tool_name = match tool_name {
        Some(t) => t,
        None => {
            eprintln!("osdk-shim: could not determine tool name from argv[0]");
            return 1;
        }
    };
    // Recursion means *this same tool* re-entering its own shim, which is the
    // runaway `sh -> shims/sh -> sh` structure docs/bugs/005 is about. A
    // different tool name is not recursion: it is the ordinary case of one
    // managed tool invoking another, and `make` (itself a shim) running a
    // recipe that calls `go` is exactly that.
    //
    // The guard used to be a bare presence check on the variable, so every
    // nested shim call inherited it and was refused with exit 126 whichever
    // tool it was. Under `make` that made `go` / `gofmt` unreachable from
    // any recipe, surfacing as `command not found` style breakage whose real
    // cause was this guard rather than PATH.
    //
    // Compare against the recorded name instead: a genuine self-re-entry is
    // still refused, while sibling tools pass through. Nested calls carry the
    // innermost tool name, which suffices because every level rewrites the
    // variable before exec, so any true cycle must revisit its own name.
    if std::env::var("OSDK_SHIM_ACTIVE").is_ok_and(|active| active == tool_name) {
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
    if std::env::var_os("OSDK_LANG").is_none() {
        if let Some(lang) = config
            .settings
            .lang
            .as_deref()
            .and_then(osdk_core::i18n::Lang::parse)
        {
            osdk_core::i18n::set_lang(lang);
        }
    }

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
    // Tolerant, not fail-closed: this scan only answers "which backend owns
    // this tool name", so one unreadable manifest elsewhere in the tree must
    // not deny service to every tool. Under the fail-closed default a single
    // conda install carrying an option this build does not know about made the
    // shim refuse *everything* -- `cargo`, `node`, even `osdk-shim --version`
    // -- and since rebuilding the shim needs `cargo`, the failure locked the
    // user out of its own fix. See docs/bugs/007.
    //
    // This weakens no guarantee. The scan was never what made an install
    // trustworthy: whatever is actually about to run still goes through
    // `validated_dynamic_install`, which re-reads that manifest and re-checks
    // identity, completion marker and provider evidence by itself. A damaged or
    // unrecognized neighbour is skipped, never silently executed.
    let dynamic_report = match osdk_core::shim::scan_dynamic_installs_tolerant(&ctx) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };

    // Find which backend owns this tool name (its id, or one of the executables
    // an installed version provides, e.g. pip -> python, npm -> node).
    let backend = match owning_backend(
        &registry,
        &ctx,
        &idiomatic_probe_cwd,
        &tool_name,
        Some(&dynamic_report),
    ) {
        Some(b) => b,
        None => {
            // A package installed directly with `npm i -g` / `pnpm add -g` is
            // not a registered tool version; resolve it from the redirected
            // global bin dir and run it under the owning package manager.
            if let Some(command) = global_command_for_name(&registry, &ctx, &tool_name) {
                return exec_global_command(&ctx, &registry, &tools, &command, forward_args);
            }
            if let Some((request, version)) =
                configured_dynamic_request_for_bin(&registry, &ctx, &tool_name, &dynamic_report)
            {
                if let Err(error) = osdk_core::shim::validated_dynamic_install(
                    &ctx,
                    &dynamic_report,
                    &request,
                    &version,
                ) {
                    eprintln!("osdk-shim: {error}");
                    return 1;
                }
            }
            eprintln!("osdk-shim: no backend provides `{tool_name}`");
            return 127;
        }
    };

    if osdk_core::execution::enabled(&ctx) {
        match osdk_core::execution::resolve(&ctx, backend.as_ref(), &idiomatic_probe_cwd) {
            Ok(selection) => {
                if let Some(osdk_core::execution::ExecutionTarget::System(target)) =
                    selection.target
                {
                    return exec_system_target(
                        &ctx,
                        &registry,
                        &idiomatic_probe_cwd,
                        target,
                        &tool_name,
                        forward_args,
                    );
                }
            }
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    }
    // Resolve the active version spec for this backend.
    let dynamic_request = osdk_core::shim::dynamic_request_from_config(&ctx, backend.id());
    let active = (match resolve_active_config(
        backend.id(),
        &idiomatic_probe_cwd,
        &ctx.config,
        backend.idiomatic_files(),
    ) {
        Ok(active) => active,
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    })
    .map(|active| (active.spec, active.is_range))
    .or_else(|| {
        dynamic_request
            .as_ref()
            .map(|request| (request.spec.to_string(), false))
    });
    let (spec, is_range) = match active {
        Some(active) => active,
        None => {
            eprintln!(
                "osdk-shim: no version of `{}` selected (set one with `osdk use {}@<version>`)",
                backend.id(),
                backend.id()
            );
            if matches!(tool_name.as_str(), "npm" | "npx") {
                eprintln!("For bundled npm, select an installed Node with `osdk use node@<version>` in a project or `osdk use --global node@<version>` as the outside-project default. Inspect `osdk current node --sources`. Installing Node alone does not select a default.");
            }
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
    let parsed_dynamic_spec = dynamic_request
        .as_ref()
        .map(|_| VersionSpec::parse(strip_distribution_prefix(&expanded_spec)));
    let version = match parsed_dynamic_spec {
        Some(VersionSpec::Exact(version)) => Some(version),
        _ => resolve_installed(&ctx, backend.as_ref(), &expanded_spec, is_range),
    };
    let version = match version {
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

    let mut tv = ToolVersion::new(backend.id(), &version);
    let dynamic_install = if let Some(request) = dynamic_request.as_ref() {
        tv.options = request.options.clone();
        match osdk_core::shim::validated_dynamic_install(&ctx, &dynamic_report, request, &version) {
            Ok(install) => Some(install),
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    } else {
        None
    };
    let bin_dirs = dynamic_install
        .as_ref()
        .map(osdk_core::shim::ValidatedDynamicInstall::bin_paths)
        .unwrap_or_else(|| managed_bin_paths(&ctx, backend.as_ref(), &tv));

    let (executable_name, alias_subcommand) = routed_launcher(&tool_name, backend.id());
    let exe = dynamic_install
        .as_ref()
        .and_then(|install| install.executable(executable_name))
        .or_else(|| {
            dynamic_install
                .is_none()
                .then(|| managed_executable_path(&ctx, backend.as_ref(), &tv, executable_name))
                .flatten()
        });
    let exe = match exe {
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
    // The shim directory is deliberately left on PATH.
    //
    // It used to be stripped here so a lifecycle subprocess could not re-enter
    // the shim. That is unnecessary -- this tool's real bin directories are
    // prepended below, so its own name already resolves to the real binary, and
    // the `OSDK_SHIM_ACTIVE` name check above is the backstop for a backend that
    // does not publish it. And it was actively harmful: a build orchestrator is
    // itself a shim, so removing the directory took away every *other* managed
    // tool with it. Under `make` no recipe could see `go`, `node` or `gofmt`, and
    // no PATH the user set could bring them back, because the entry was dropped
    // after their PATH had already been assembled.
    // JVM tools that bundle no runtime abort unless a JDK is visible. A
    // JAVA_HOME the user set themselves is a deliberate choice and is left
    // alone; one this tool's own activation exported describes whichever
    // directory the last prompt saw, so it is recomputed here rather than
    // allowed to outlive that directory. Injected before the tool's own bins so
    // those stay in front.
    if osdk_core::shim::requires_external_jdk(backend.id())
        && !exec_env.contains_key("JAVA_HOME")
        && !osdk_core::shim::process_java_home_is_user_owned()
    {
        match osdk_core::execution::jdk_environment(&ctx, &registry, &idiomatic_probe_cwd) {
            Ok(Some((jdk_paths, jdk_env))) => {
                exec_env.extend(jdk_env);
                prepend_env_path(&mut exec_env, jdk_paths);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    }
    if backend.id().starts_with("npm:") {
        let node_backend = registry.get("node").unwrap();
        let node_bin_paths = match manifest_bound_node_bin_paths(
            &ctx,
            backend.id(),
            dynamic_install
                .as_ref()
                .expect("configured npm backend has validated inventory"),
            &*node_backend,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        };
        let mut exact_runtime_path = node_bin_paths;
        exact_runtime_path.extend(bin_dirs.clone());
        prepend_env_path(&mut exec_env, exact_runtime_path);
    } else if osdk_core::execution::enabled(&ctx) && matches!(backend.id(), "npm" | "pnpm" | "yarn")
    {
        let (mut node_bins, node_env) = match osdk_core::execution::runtime_environment(
            &ctx,
            &registry,
            &idiomatic_probe_cwd,
            "node",
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        };
        exec_env.extend(node_env);
        node_bins.extend(bin_dirs);
        match osdk_core::execution::path_with(&ctx, node_bins) {
            Ok(path) => {
                exec_env.insert("PATH".into(), path);
            }
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        }
    } else if backend.id() == "pnpm" || backend.id() == "yarn" {
        let node_backend = registry.get("node").unwrap();
        let active_node = match resolve_active_config(
            "node",
            &idiomatic_probe_cwd,
            &ctx.config,
            &[".nvmrc", ".node-version"],
        ) {
            Ok(active) => active,
            Err(error) => {
                eprintln!("osdk-shim: {error}");
                return 1;
            }
        };
        let node_version = active_node
            .as_ref()
            .and_then(|active| {
                resolve_installed(&ctx, &*node_backend, &active.spec, active.is_range)
            })
            .or_else(|| node_backend.list_installed(&ctx).ok()?.into_iter().last());
        let Some(node_version) = node_version else {
            eprintln!(
                "osdk-shim: {}",
                osdk_core::t!("err.shim_managed_node_required", tool = backend.id())
            );
            return 1;
        };
        let node = ToolVersion::new("node", node_version);
        prepend_env_path(
            &mut exec_env,
            managed_bin_paths(&ctx, &*node_backend, &node),
        );
        prepend_env_path(&mut exec_env, bin_dirs);
        if backend.id() == "pnpm" {
            // pnpm 11 self-checks that its global bin dir ($PNPM_HOME/bin) is on
            // PATH before a global install. Put it there so a redirected
            // `pnpm add -g` is accepted; the user's shell still resolves these
            // commands through the shims dir, which activation keeps ahead.
            prepend_env_path(&mut exec_env, vec![ctx.dirs.pnpm_global_home().join("bin")]);
        }
    } else {
        prepend_env_path(&mut exec_env, bin_dirs);
    }

    if package_manager_for_backend(backend.id(), &tool_name, &version).is_some()
        || tool_name == "uv"
        || tool_name == "go"
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let result = runtime.map_err(|e| e.to_string()).and_then(|runtime| {
            runtime
                .block_on(osdk_core::package_sources::apply(
                    &ctx,
                    &cwd,
                    &tool_name,
                    Some(&version),
                    forward_args,
                    &mut exec_env,
                ))
                .map_err(|e| e.to_string())
        });
        if let Err(error) = result {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    }

    // Decide global-bin ownership before tool_name is moved into the env below.
    let global_owner = global_owner_for_tool(&tool_name);
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
    match global_owner {
        // Package managers that persist global bins: run to completion, then
        // sync their redirected global bin shims so no manual reshim is needed.
        Some(owner) => run_and_sync_global(
            &ctx,
            &registry,
            &dynamic_report,
            &cwd,
            owner,
            &exe,
            exec_args,
            &exec_env,
        ),
        None => exec(&exe, exec_args, &exec_env),
    }
}

fn exec_system_target(
    ctx: &osdk_core::backend::Ctx,
    registry: &osdk_core::backend::registry::Registry,
    cwd: &std::path::Path,
    mut target: osdk_core::execution::SystemTarget,
    name: &str,
    args: &[String],
) -> i32 {
    let prepare = || -> osdk_core::error::Result<_> {
        let mut bins = Vec::new();
        let mut env = std::collections::BTreeMap::new();
        if matches!(target.tool.as_str(), "npm" | "pnpm" | "yarn") || matches!(name, "npm" | "npx")
        {
            let (node_bins, node_env) =
                osdk_core::execution::runtime_environment(ctx, registry, cwd, "node")?;
            bins.extend(node_bins);
            env.extend(node_env);
        }
        bins.push(target.bin_dir().to_path_buf());
        env.insert("PATH".into(), osdk_core::execution::path_with(ctx, bins)?);
        Ok(env)
    };
    let mut env = match prepare() {
        Ok(env) => env,
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };
    if let Err(error) = target.verify(ctx, &env) {
        eprintln!("osdk-shim: {error}");
        return 1;
    }
    let (executable_name, subcommand) = routed_launcher(name, &target.tool);
    let executable = match target.command(ctx, executable_name) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            return 1;
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let applied = runtime.map_err(|e| e.to_string()).and_then(|runtime| {
        runtime
            .block_on(osdk_core::package_sources::apply(
                ctx,
                cwd,
                name,
                target.version.as_deref(),
                args,
                &mut env,
            ))
            .map_err(|e| e.to_string())
    });
    if let Err(error) = applied {
        eprintln!("osdk-shim: {error}");
        return 1;
    }
    env.insert("OSDK_SHIM_ACTIVE".into(), name.into());
    let args: Vec<_> = subcommand
        .into_iter()
        .map(str::to_string)
        .chain(args.iter().cloned())
        .collect();
    let mut command = Command::new(&executable);
    command
        .args(args)
        .env_clear()
        .envs(target.environment(&env));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("osdk-shim: system execution failed: {error}");
        126
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!("osdk-shim: system execution failed: {error}");
                126
            }
        }
    }
}

fn manifest_bound_node_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    npm_backend: &str,
    install: &osdk_core::shim::ValidatedDynamicInstall,
    node_backend: &dyn osdk_core::backend::Backend,
) -> Result<Vec<PathBuf>, String> {
    let required = || osdk_core::t!("err.shim_managed_node_required", tool = npm_backend);
    let recorded = install
        .identity()
        .dependencies
        .iter()
        .find(|dependency| {
            dependency.kind == osdk_core::tool::InstallDependencyKind::Runtime
                && dependency.id == "node"
        })
        .map(|dependency| dependency.version.as_str())
        .ok_or_else(&required)?;
    let node_version = match VersionSpec::parse(recorded) {
        // The manifest records the concrete runtime identity selected during
        // installation. Reject aliases, ranges, prefixes, and even textual
        // normalization so execution cannot drift to another installed Node.
        VersionSpec::Exact(version) if version == recorded => version,
        _ => return Err(required()),
    };
    let installed = node_backend.list_installed(ctx).map_err(|_| required())?;
    if !installed.iter().any(|version| version == &node_version) {
        return Err(required());
    }
    let node = ToolVersion::new("node", node_version);
    let bin_paths = managed_bin_paths(ctx, node_backend, &node);
    if find_exe(&bin_paths, "node").is_none() {
        return Err(required());
    }
    Ok(bin_paths)
}

fn routed_launcher<'a>(tool_name: &'a str, backend: &str) -> (&'a str, Option<&'static str>) {
    match (backend, tool_name) {
        ("pnpm", "pnpx") => ("pnpm", Some("dlx")),
        ("bun", "bunx") => ("bun", Some("x")),
        _ => (tool_name, None),
    }
}

/// Refuse to dispatch a tool when the project config carries an unreviewed key
/// that would change *this* dispatch.
///
/// Only those keys. The shim runs on every command invocation, so anything it
/// refuses becomes unusable in that directory -- and a refusal the shim cannot
/// act on is pure cost. Adding a `[sys.pkg]` block used to make `cargo --version`
/// fail here with "project config is not trusted", a message about installing
/// system packages produced by a command that installs nothing; trust is bound
/// to the file hash, so every later edit re-locked every tool again.
///
/// `osdk install`, `osdk pkg` and `osdk run` still evaluate the full set before
/// they act, which is where those keys actually take effect.
fn ensure_project_config_trusted(dirs: &Dirs, cwd: &std::path::Path) -> Result<(), String> {
    let Some(project_config) = osdk_core::trust::project_config(cwd).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let rel_base = osdk_core::project::discover(cwd, &dirs.config)
        .ok()
        .flatten()
        .filter(|ctx| ctx.is_external())
        .map(|ctx| ctx.root);
    for project_config in
        osdk_core::config::inheritance::sources(&project_config, rel_base.as_deref())
            .map_err(|e| e.to_string())?
    {
        let requirements: Vec<_> =
            osdk_core::trust::trust_requirements(&project_config, &osdk_core::trust::Scope::ALL)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(osdk_core::trust::affects_tool_dispatch)
                .collect();
        if requirements.is_empty() {
            continue;
        }
        let trusted_paths = std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS");
        if !osdk_core::trust::is_trusted(&dirs.config, &project_config, trusted_paths.as_ref())
            .map_err(|e| e.to_string())?
        {
            return Err(osdk_core::t!(
                "err.untrusted_config",
                path = project_config.display(),
                requirements = osdk_core::trust::describe_requirements(&requirements)
            ));
        }
    }
    Ok(())
}

fn package_manager_for_backend(
    backend: &str,
    executable_alias: &str,
    backend_version: &str,
) -> Option<PackageManager> {
    let belongs_to_manager = match backend {
        // Package-manager launchers may come from their independent backend
        // or from Node/Corepack. Registry behavior follows the command name.
        "node" => osdk_core::shim::package_manager_backend_for_command(executable_alias).is_some(),
        "npm" => matches!(executable_alias, "npm" | "npx"),
        "pnpm" => matches!(executable_alias, "pnpm" | "pnpx"),
        "yarn" => matches!(executable_alias, "yarn" | "yarnpkg"),
        "bun" => matches!(executable_alias, "bun" | "bunx"),
        "deno" => executable_alias == "deno",
        _ => false,
    };
    let version = if backend == "node" && matches!(executable_alias, "yarn" | "yarnpkg") {
        None
    } else {
        Some(backend_version)
    };
    belongs_to_manager
        .then(|| manager_for_command(executable_alias, version))
        .flatten()
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

/// Determine the tool name and args to forward.
/// Was the binary run as `osdk-shim ...` rather than through a shim name?
///
/// Shares `parse_invocation`'s rule so the two cannot drift: only the direct
/// form may carry shim flags, because under a tool name `--version` belongs to
/// that tool and must be forwarded untouched.
fn is_direct_invocation(args: &[String]) -> bool {
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("");
    basename_no_ext(argv0) == "osdk-shim"
}

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
    dynamic_report: Option<&ScanReport>,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
    if let Some(manager_id) = osdk_core::shim::package_manager_backend_for_command(tool_name) {
        let manager = registry.get(manager_id).ok()?;
        if osdk_core::execution::enabled(ctx)
            && matches!(
                osdk_core::execution::resolve_without_probes(ctx, manager.as_ref(), cwd)
                    .ok()
                    .and_then(|s| s.target),
                Some(osdk_core::execution::ExecutionTarget::System(_))
            )
        {
            return Some(manager);
        }
        // An explicit independent package-manager selection is authoritative,
        // including when its selected version is missing: do not silently fall
        // back to the bundled copy and hide a broken project pin.
        if (resolve_active_config(manager_id, cwd, &ctx.config, manager.idiomatic_files())
            .ok()
            .flatten())
        .is_some()
        {
            return Some(manager);
        }

        // Node intentionally does not claim package managers in `bin_names`,
        // because independent backends own those public tool IDs. A routing
        // shim may still dispatch to the selected Node installation's bundled
        // launcher when no independent package manager is selected.
        let node = registry.get("node").ok()?;
        if let Some(active) =
            resolve_active_config("node", cwd, &ctx.config, node.idiomatic_files())
                .ok()
                .flatten()
        {
            // Bundled npm belongs to the selected Node even if that install
            // is missing. Let normal resolution report the broken Node pin.
            if matches!(tool_name, "npm" | "npx") {
                return Some(node);
            }
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

        return (tool_name == manager_id || tool_name == "npx").then_some(manager);
    }
    if let Ok(b) = registry.get(tool_name) {
        return Some(b);
    }
    if let Some(report) = dynamic_report {
        if let Some(backend) = dynamic_backend_for_bin(registry, ctx, report, tool_name) {
            return Some(backend);
        }
    }
    // Scan compiled-in backends' installed versions' bin names. Collect every
    // claimant rather than taking the first: when two families of one ecosystem
    // ship the same launcher, registry order would otherwise decide, and the
    // shim would dispatch to a different copy than the one it was written for.
    let mut claimants: Vec<std::sync::Arc<dyn osdk_core::backend::Backend>> = Vec::new();
    for backend in registry.all() {
        // Routing obeys the same shim filter as generation, so a family the
        // user excluded cannot still be reached through a name that another
        // family generated.
        if !osdk_core::shim::shim_is_enabled(&ctx.config.settings.shims, backend.id(), tool_name) {
            continue;
        }
        if osdk_core::execution::enabled(ctx) {
            if let Some(osdk_core::execution::ExecutionTarget::System(target)) =
                osdk_core::execution::resolve_without_probes(ctx, backend.as_ref(), cwd)
                    .ok()
                    .and_then(|s| s.target)
            {
                if target
                    .published_names(ctx)
                    .iter()
                    .any(|name| name == tool_name)
                {
                    claimants.push(backend.clone());
                    continue;
                }
            }
        }
        if let Ok(versions) = backend.list_installed(ctx) {
            for v in versions {
                let tv = ToolVersion::new(backend.id(), &v);
                if let Ok(names) = backend.bin_names(ctx, &tv) {
                    if names.iter().any(|n| n == tool_name) {
                        claimants.push(backend.clone());
                        break;
                    }
                }
            }
        }
    }
    if claimants.len() > 1 {
        let owner_ids = claimants
            .iter()
            .map(|backend| backend.id().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        if let Some(winner) = osdk_core::shim::precedence_winner(tool_name, &owner_ids) {
            if let Some(backend) = claimants.iter().find(|backend| backend.id() == winner) {
                return Some(backend.clone());
            }
        }
    }
    claimants.into_iter().next()
}

fn dynamic_backend_for_bin(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    report: &ScanReport,
    tool_name: &str,
) -> Option<std::sync::Arc<dyn osdk_core::backend::Backend>> {
    // Honour the shim settings here too: a command the user exposed has a shim on
    // disk, so this lookup must be able to route it, or the shim would exist and
    // then fail to find its own backend (docs/bugs/008).
    let owners =
        osdk_core::shim::dynamic_bin_ownership_with_settings(report, &ctx.config.settings.shims);
    let candidates = owners.get(tool_name)?;
    let owner_ids = candidates
        .iter()
        .map(|candidate| candidate.canonical_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let configured = osdk_core::shim::configured_dynamic_ids(ctx, report);
    let matching_configured = owner_ids
        .iter()
        .filter(|owner_id| {
            configured
                .iter()
                .any(|configured_id| configured_id == *owner_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if matching_configured.len() == 1 {
        return registry.get(&matching_configured[0]).ok();
    }
    if owner_ids.len() == 1 {
        // The sole installed candidate is not selected, so it cannot run: the
        // launcher would only fail with "no version selected". Do not claim the
        // name then, so another source (such as a same-named `npm i -g` command)
        // can handle it through the caller's global-command fallback.
        return None;
    }
    let conflicting = if matching_configured.is_empty() {
        owner_ids.into_iter().collect::<Vec<_>>()
    } else {
        matching_configured
    };
    let owners = conflicting.join(", ");
    if !owners.is_empty() {
        eprintln!(
            "osdk-shim: {}",
            osdk_core::t!(
                "err.shim_dynamic_route_conflict",
                tool = tool_name,
                owners = owners
            )
        );
    }
    None
}

fn configured_dynamic_request_for_bin(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    tool_name: &str,
    report: &ScanReport,
) -> Option<(osdk_core::version::ToolRequest, String)> {
    for backend_id in osdk_core::shim::configured_dynamic_ids(ctx, report) {
        let Some(request) = osdk_core::shim::dynamic_request_from_config(ctx, &backend_id) else {
            continue;
        };
        let VersionSpec::Exact(version) = request.spec.clone() else {
            continue;
        };
        if registry.get(&backend_id).is_err() {
            continue;
        }
        let directly_configured = ctx.config.tools.iter().any(|(key, value)| {
            let direct_id = osdk_core::inventory::canonical_dynamic_id(key).ok();
            let targets_backend = direct_id.as_deref() == Some(backend_id.as_str())
                || osdk_core::version::ToolRequest::parse(value)
                    .is_ok_and(|candidate| candidate.backend == backend_id);
            targets_backend
                && (key.strip_prefix("tool.") == Some(tool_name)
                    || direct_id
                        .as_deref()
                        .is_some_and(|id| dynamic_default_bin(id) == Some(tool_name)))
        });
        if directly_configured {
            return Some((request, version));
        }
    }
    None
}

fn dynamic_default_bin(backend_id: &str) -> Option<&str> {
    backend_id.split_once(':')?.1.rsplit('/').next()
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

fn managed_bin_paths(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    version: &ToolVersion,
) -> Vec<PathBuf> {
    let mut paths = backend.bin_paths(ctx, version).unwrap_or_default();
    paths.sort();
    paths.dedup();
    paths
}

fn managed_executable_path(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn osdk_core::backend::Backend,
    version: &ToolVersion,
    executable_name: &str,
) -> Option<PathBuf> {
    find_exe(&managed_bin_paths(ctx, backend, version), executable_name)
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

/// A command resolved from a redirected npm/pnpm global bin directory.
struct GlobalCommand {
    name: String,
    owner: std::sync::Arc<dyn osdk_core::backend::Backend>,
    launcher: PathBuf,
    bin_dir: PathBuf,
}

/// Find a package installed directly with `npm i -g` / `pnpm add -g`, by name,
/// scanning the redirected global bin directories.
fn global_command_for_name(
    registry: &Registry,
    ctx: &osdk_core::backend::Ctx,
    name: &str,
) -> Option<GlobalCommand> {
    for source in osdk_core::shim::global_bin_sources(ctx) {
        if !source.names.iter().any(|candidate| candidate == name) {
            continue;
        }
        let Some(launcher) = find_exe(std::slice::from_ref(&source.bin_dir), name) else {
            continue;
        };
        let Ok(owner) = registry.get(source.owner) else {
            continue;
        };
        return Some(GlobalCommand {
            name: name.into(),
            owner,
            launcher,
            bin_dir: source.bin_dir,
        });
    }
    None
}

/// Native globals are applications, not installations of their package manager.
/// Supply only their runtime host, using the same selection policy as `exec`.
fn exec_global_command(
    ctx: &osdk_core::backend::Ctx,
    registry: &Registry,
    _tools: &std::collections::BTreeMap<String, String>,
    command: &GlobalCommand,
    forward_args: &[String],
) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let host = osdk_core::shim::native_global_runtime(&command.launcher, command.owner.id());
    let prepare = || -> osdk_core::error::Result<_> {
        let mut env = std::collections::BTreeMap::new();
        let mut bins = Vec::new();
        let mut external = None;
        if let Some(host) = host {
            let backend = registry.get(host)?;
            match osdk_core::execution::resolve(ctx, backend.as_ref(), &cwd)?.target {
                Some(osdk_core::execution::ExecutionTarget::System(mut target)) => {
                    env.insert(
                        "PATH".into(),
                        osdk_core::execution::path_with(ctx, [target.bin_dir().to_path_buf()])?,
                    );
                    target.verify(ctx, &env)?;
                    bins.push(target.bin_dir().to_path_buf());
                    external = Some(target);
                }
                _ => {
                    let (runtime_bins, runtime_env) =
                        osdk_core::execution::runtime_environment(ctx, registry, &cwd, host)?;
                    let executable = find_exe(&runtime_bins, host).filter(|path| {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::metadata(path)
                                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = path;
                            true
                        }
                    });
                    if executable.is_none() {
                        return Err(osdk_core::error::Error::config(format!(
                            "selected {host} runtime executable is missing or not executable"
                        )));
                    }
                    bins.extend(runtime_bins);
                    env.extend(runtime_env);
                }
            }
        }
        // The selected host precedes global bins: a package named `node` must
        // not replace the interpreter. Keep native launchers intact.
        bins.push(command.bin_dir.clone());
        if external.is_some() {
            env.insert("PATH".into(), osdk_core::execution::path_with(ctx, bins)?);
        } else {
            prepend_env_path(&mut env, bins);
        }
        env.insert("OSDK_SHIM_ACTIVE".into(), command.name.clone());
        Ok((env, external))
    };
    match prepare() {
        Ok((env, Some(target))) => exec_environment(
            &command.launcher,
            forward_args,
            &target.environment(&env),
            true,
        ),
        Ok((env, None)) => exec(&command.launcher, forward_args, &env),
        Err(error) => {
            eprintln!("osdk-shim: {error}");
            if let Some(host) = host {
                eprintln!("This global package is retained, but needs a selected, installed {host} runtime. Inspect `osdk current {host} --sources`; select one for this project with `osdk use {host}@<version>` or outside projects with `osdk use --global {host}@<version>`. No runtime was installed automatically.");
            }
            1
        }
    }
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

fn exec(exe: &PathBuf, args: &[String], env: &std::collections::BTreeMap<String, String>) -> i32 {
    let env = env.iter().map(|(k, v)| (k.into(), v.into())).collect();
    exec_environment(exe, args, &env, false)
}

#[cfg(unix)]
fn exec_environment(
    exe: &PathBuf,
    args: &[String],
    env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    clear: bool,
) -> i32 {
    use std::os::unix::process::CommandExt;
    // Replace the current process so signals/exit codes pass through cleanly.
    let mut command = Command::new(exe);
    if clear {
        command.env_clear();
    }
    let err = command.args(args).envs(env).exec();
    eprintln!("osdk-shim: failed to exec {}: {err}", exe.display());
    126
}

#[cfg(not(unix))]
fn exec_environment(
    exe: &PathBuf,
    args: &[String],
    env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    clear: bool,
) -> i32 {
    let extension = exe
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let mut command =
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let mut command = Command::new(
                std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
            );
            let command_path = match windows_command_path(exe) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("osdk-shim: failed to prepare {}: {error}", exe.display());
                    return 126;
                }
            };
            command.args(["/D", "/S", "/C", "call"]).arg(command_path);
            command
        } else {
            Command::new(exe)
        };
    if clear {
        command.env_clear();
    }
    match command.args(args).envs(env).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("osdk-shim: failed to run {}: {e}", exe.display());
            126
        }
    }
}

/// Map the invoked command to the package-manager owner whose redirected global
/// bin dir should be reconciled after the process exits. Only package managers
/// that persist global bins are listed; npx/bunx/dlx run ephemerally.
fn global_owner_for_tool(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "npm" => Some("npm"),
        "pnpm" => Some("pnpm"),
        "yarn" | "yarnpkg" => Some("yarn"),
        "bun" => Some("bun"),
        "deno" => Some("deno"),
        _ => None,
    }
}

/// Run a package manager to completion, then reconcile its redirected global
/// bin shims so a global install/uninstall takes effect without a manual reshim.
/// Returns the child's exit code.
#[allow(clippy::too_many_arguments)]
fn run_and_sync_global(
    ctx: &osdk_core::backend::Ctx,
    registry: &Registry,
    dynamic_report: &ScanReport,
    cwd: &std::path::Path,
    owner: &str,
    exe: &PathBuf,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> i32 {
    #[cfg(unix)]
    let code = run_child_status(exe, args, env);
    #[cfg(not(unix))]
    // The Windows exec already spawns and waits (there is no process replacement).
    let code = exec(exe, args, env);
    reconcile_after_run(ctx, registry, dynamic_report, cwd, owner);
    code
}

/// Reconcile one owner's global bin shims after its process exited. A vanished
/// name is kept when it is still a managed tool's bin; sync errors are warnings
/// and never change the command's exit code.
fn reconcile_after_run(
    ctx: &osdk_core::backend::Ctx,
    registry: &Registry,
    dynamic_report: &ScanReport,
    cwd: &std::path::Path,
    owner: &str,
) {
    let is_protected =
        |name: &str| owning_backend(registry, ctx, cwd, name, Some(dynamic_report)).is_some();
    match osdk_core::shim::sync_owner_global_shims(ctx, owner, is_protected) {
        Ok(outcome) => {
            for name in outcome.added {
                eprintln!("osdk: global command `{name}` is now available");
            }
            for name in outcome.removed {
                eprintln!("osdk: global command `{name}` removed");
            }
        }
        Err(error) => eprintln!("osdk-shim: failed to sync global bins: {error}"),
    }
}

#[cfg(unix)]
/// Spawn and wait (instead of replacing the process) so the global bins can be
/// reconciled once the package manager exits.
fn run_child_status(
    exe: &PathBuf,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match Command::new(exe).args(args).envs(env).status() {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(1),
        Err(error) => {
            eprintln!("osdk-shim: failed to run {}: {error}", exe.display());
            126
        }
    }
}

#[cfg(windows)]
fn windows_command_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    // `GetShortPathNameW` resolves its argument through the MAX_PATH-limited
    // path API, so a longer path fails with ERROR_PATH_NOT_FOUND unless it
    // carries the extended-length prefix. Query with `\\?\`, then strip it
    // again, because `cmd.exe` cannot consume a verbatim path.
    let wide = verbatim_wide(path);
    let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if written == 0 || written as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error());
    }
    buffer.truncate(written as usize);
    let short: PathBuf = std::ffi::OsString::from_wide(&buffer).into();
    let short = strip_verbatim_prefix(&short);
    // A volume with 8.3 name creation disabled returns the long path unchanged.
    // `cmd.exe` still cannot address it, so fail with an explanatory error
    // instead of returning a path whose invocation dies as "cannot find path".
    if short.as_os_str().encode_wide().count() >= MAX_COMMAND_PATH_WIDE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path needs a short name to stay under {MAX_COMMAND_PATH_WIDE} characters, \
                 but 8.3 short names are unavailable on this volume"
            ),
        ));
    }
    Ok(short)
}

/// `cmd.exe` resolves its target through the MAX_PATH-limited Win32 path API.
#[cfg(windows)]
const MAX_COMMAND_PATH_WIDE: usize = 260;

#[cfg(windows)]
const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Encode `path` for the wide path APIs, adding `\\?\` when that is both needed
/// and valid. A UNC path would require `\\?\UNC\...` and a relative path cannot
/// be prefixed at all, so both are passed through unchanged.
#[cfg(windows)]
fn verbatim_wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let is_verbatim = encoded.starts_with(&VERBATIM_PREFIX);
    let is_unc = encoded.starts_with(&[b'\\' as u16, b'\\' as u16]);
    let mut wide = Vec::with_capacity(encoded.len() + VERBATIM_PREFIX.len() + 1);
    if !is_verbatim && !is_unc && path.is_absolute() {
        wide.extend_from_slice(&VERBATIM_PREFIX);
    }
    wide.extend_from_slice(&encoded);
    wide.push(0);
    wide
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: &std::path::Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    match encoded.strip_prefix(&VERBATIM_PREFIX[..]) {
        Some(rest) => std::ffi::OsString::from_wide(rest).into(),
        None => path.to_path_buf(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    fn corepack_package_managers_use_safe_registry_classification() {
        assert_eq!(
            package_manager_for_backend("node", "npm", "22.0.0"),
            Some(PackageManager::Npm)
        );
        assert_eq!(
            package_manager_for_backend("node", "pnpm", "22.0.0"),
            Some(PackageManager::Pnpm)
        );
        assert_eq!(package_manager_for_backend("node", "yarn", "22.0.0"), None);
        assert_eq!(
            package_manager_for_backend("yarn", "yarn", "4.10.3"),
            Some(PackageManager::YarnBerry)
        );
    }

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

    #[test]
    fn batch_targets_forward_standard_streams_and_exit_code() {
        use std::io::Write;
        use std::process::Stdio;

        let temporary = tempfile::tempdir().unwrap();
        let mut long_root = temporary.path().to_path_buf();
        for index in 0..8 {
            long_root.push(format!("segment-{index}-abcdefghijklmnopqrstuvwxyz"));
        }
        std::fs::create_dir_all(&long_root).unwrap();
        let script = long_root.join("stream fixture.cmd");
        assert!(script.as_os_str().encode_wide().count() > 260);
        // `cmd.exe` can only reach a path this long through its 8.3 short name.
        // Volumes created with 8.3 generation disabled cannot produce one, so
        // the contract under test is unobservable there.
        if windows_command_path(&script).is_err() {
            eprintln!(
                "skipping: {} has no 8.3 short name for a >260 character path",
                long_root.display()
            );
            return;
        }
        let stdout_path = temporary.path().join("stdout.txt");
        let stderr_path = temporary.path().join("stderr.txt");
        std::fs::write(
            &script,
            "@echo off\r\nset /p line=\r\necho out:%~1:%line%\r\necho err:%~2 1>&2\r\nexit /b 23\r\n",
        )
        .unwrap();

        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("tests::run_batch_target_fixture")
            .arg("--ignored")
            .env("OSDK_SHIM_TEST_BATCH_TARGET", &script)
            .env("OSDK_SHIM_TEST_BATCH_ARG1", "first arg")
            .env("OSDK_SHIM_TEST_BATCH_ARG2", "second arg")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"input\n").unwrap();
        drop(child.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                panic!("batch target stream forwarding timed out after 10 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(status.code(), Some(23));
        let stdout = std::fs::read_to_string(stdout_path).unwrap();
        let stderr = std::fs::read_to_string(stderr_path).unwrap();
        assert!(
            stdout
                .lines()
                .any(|line| line.trim() == "out:first arg:input"),
            "{stdout}"
        );
        assert!(
            stderr.lines().any(|line| line.trim() == "err:second arg"),
            "{stderr}"
        );
    }

    #[test]
    #[ignore = "helper process for the redirected batch-target contract"]
    fn run_batch_target_fixture() {
        let Some(script) = std::env::var_os("OSDK_SHIM_TEST_BATCH_TARGET") else {
            return;
        };
        let first = std::env::var("OSDK_SHIM_TEST_BATCH_ARG1").unwrap();
        let second = std::env::var("OSDK_SHIM_TEST_BATCH_ARG2").unwrap();
        std::process::exit(exec(
            &PathBuf::from(script),
            &[first, second],
            &std::collections::BTreeMap::new(),
        ));
    }
}
