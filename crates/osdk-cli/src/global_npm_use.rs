//! Global `npm:<package>` installation for `osdk use --global`.
//!
//! Global means user-selected and shim-visible in osdk. Native npm and pnpm
//! execute their real global-add modes against an osdk-owned prefix; embedded
//! Aube uses its safe synthetic-project equivalent. Neither path mutates the
//! caller's project or an ambient Node installation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use osdk_core::backend::aube_host::{self, EmbeddedInstallRequest};
use osdk_core::backend::npm_package::{NpmPackageBackend, LOCKED_NPM_NODE_VERSION_OPTION};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::npm_tools::{
    self, NpmInstaller, ToolScope, INSTALLER_OPTION, LOCKED_NPM_INSTALLER_OPTION,
    LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION, LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
    LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION, LOCKED_NPM_SCOPE_OPTION,
};
use osdk_core::package_registry::{self, PackageManager, RegistryPlan, RegistryProbe};
use osdk_core::pipeline::{self, HashAlgo};
use osdk_core::source::select;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};

use crate::app::App;

const NATIVE_CONFIG_DIR: &str = "native-config";

#[derive(Debug, Clone)]
struct ManagedRuntime {
    node_request: ToolRequest,
    node_version: ToolVersion,
    node_bin: PathBuf,
    manager: Option<(ToolRequest, ToolVersion, PathBuf)>,
}

/// Install, globally select, lock, and expose one dynamic npm tool.
pub async fn install(
    app: &mut App,
    mut request: ToolRequest,
    requested_spec: Option<String>,
) -> Result<()> {
    if !request.backend.starts_with("npm:") {
        return Err(anyhow!(
            "global npm installer requires an npm:<package> request"
        ));
    }
    apply_source_override(app, &request.backend);
    let requested_installer = npm_tools::installer_from_request_options(&request.options)?;
    let cwd = std::env::current_dir().context("getting current dir for global npm install")?;
    let plan = npm_tools::plan_npm_installer(&cwd, requested_installer, ToolScope::Global)?;
    let runtime = ensure_managed_runtime(app, plan.installer).await?;

    request.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        plan.installer.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_NODE_VERSION_OPTION.into(),
        runtime.node_version.version.clone(),
    );

    let backend = NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    if app.refresh_sources {
        select::refresh(&app.ctx, &backend).await?;
    }
    let effective = expand_alias(app, &request)?;
    let mut version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    version.options = request.options.clone();
    version
        .options
        .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());

    let install_root = backend.install_root(&app.ctx, &version.version);
    let lock_path = app.ctx.dirs.lock_dir(backend.id()).join(format!(
        "{}.global.lock",
        osdk_core::dirs::sanitize_version_component(&version.version)
    ));
    let _mutation_lock = osdk_core::lock::FileLock::acquire(lock_path)?;

    let mut installed_native_lock = native_lock_path(&backend, &app.ctx, &version, plan.installer);
    if !completed_install_matches(
        &backend,
        &app.ctx,
        &version,
        plan.installer,
        &runtime.node_version.version,
        installed_native_lock.as_deref(),
    )? {
        if install_root.exists() {
            std::fs::remove_dir_all(&install_root).with_context(|| {
                format!("removing stale global install {}", install_root.display())
            })?;
        }
        std::fs::create_dir_all(&install_root)
            .with_context(|| format!("creating global install {}", install_root.display()))?;
        if let Err(error) =
            run_global_install(app, &backend, &version, plan.installer, &runtime).await
        {
            let _ = std::fs::remove_dir_all(&install_root);
            return Err(error);
        }
        validate_global_package_identity(&backend, &app.ctx, &version, plan.installer)?;
        installed_native_lock = native_lock_path(&backend, &app.ctx, &version, plan.installer);
        let bin_dir = global_bin_dir(&backend, &app.ctx, &version, plan.installer);
        let native = installed_native_lock
            .as_deref()
            .filter(|path| path.is_file())
            .map(|path| read_native_lock(path, plan.installer))
            .transpose()?;
        inject_native_metadata(
            &mut version,
            plan.installer,
            &runtime.node_version.version,
            native.as_ref(),
        );
        if let Err(error) = backend.finalize_global_install(
            &app.ctx,
            &version,
            &bin_dir,
            &runtime.node_version.version,
            plan.installer.as_str(),
            native
                .as_ref()
                .map(|native| (native.format.as_str(), native.sha256.as_str())),
        ) {
            let _ = std::fs::remove_dir_all(&install_root);
            return Err(anyhow::Error::new(error));
        }
    } else {
        let native = installed_native_lock
            .as_deref()
            .filter(|path| path.is_file())
            .map(|path| read_native_lock(path, plan.installer))
            .transpose()?;
        inject_native_metadata(
            &mut version,
            plan.installer,
            &runtime.node_version.version,
            native.as_ref(),
        );
    }

    let _global_mutation_lock =
        osdk_core::lock::FileLock::acquire(app.ctx.dirs.data.join("locks/global-npm-state.lock"))?;
    persist_global_lock(app, &request, &version, &runtime)?;
    crate::commands::reshim(app)?;
    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    persist_global_config(app, &request, &persisted_spec, plan.installer)?;
    println!(
        "{}",
        osdk_core::t!(
            "msg.pinned_global",
            tool = request.backend,
            ver = persisted_spec
        )
    );

    Ok(())
}

async fn ensure_managed_runtime(app: &mut App, installer: NpmInstaller) -> Result<ManagedRuntime> {
    let node_request = configured_request(app, "node");
    let (node_backend, node_version) = install_backend(app, &node_request).await?;
    let node_bin = find_bin_dir(&app.ctx, node_backend.as_ref(), &node_version, "node")?;
    let manager_id = match installer {
        NpmInstaller::Npm => Some("npm"),
        NpmInstaller::Pnpm => Some("pnpm"),
        NpmInstaller::Aube => None,
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    };
    let manager = if let Some(manager_id) = manager_id {
        let request = configured_request(app, manager_id);
        let (backend, version) = install_backend(app, &request).await?;
        let executable = find_executable(&backend.bin_paths(&app.ctx, &version)?, manager_id)
            .ok_or_else(|| anyhow!("managed {manager_id} executable was not installed"))?;
        Some((exact_request(request, &version), version, executable))
    } else {
        None
    };
    Ok(ManagedRuntime {
        node_request: exact_request(node_request, &node_version),
        node_version,
        node_bin,
        manager,
    })
}

fn exact_request(mut request: ToolRequest, version: &ToolVersion) -> ToolRequest {
    request.spec = VersionSpec::Exact(version.version.clone());
    request
}

fn configured_request(app: &App, backend: &str) -> ToolRequest {
    let entry = app.ctx.config.global_tool_configs.get(backend);
    ToolRequest {
        backend: backend.into(),
        spec: entry
            .map(|entry| VersionSpec::parse(entry.version()))
            .unwrap_or(VersionSpec::Latest),
        options: entry
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    }
}

async fn install_backend(
    app: &mut App,
    request: &ToolRequest,
) -> Result<(std::sync::Arc<dyn Backend>, ToolVersion)> {
    apply_source_override(app, &request.backend);
    let backend = app.registry.get(&request.backend)?;
    if app.refresh_sources {
        select::refresh(&app.ctx, backend.as_ref()).await?;
    }
    let effective = expand_alias(app, request)?;
    let version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    if pipeline::is_installed(&app.ctx.dirs, backend.id(), &version.version) {
        backend.ensure_post_install(&app.ctx, &version)?;
    } else {
        backend
            .install(&InstallCtx { ctx: &app.ctx }, &version)
            .await
            .with_context(|| format!("installing {}", version))?;
    }
    Ok((backend, version))
}

async fn run_global_install(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
) -> Result<()> {
    let project = backend.project_dir(&app.ctx, &version.version);
    match installer {
        NpmInstaller::Aube => {
            backend
                .write_empty_project_manifest(&app.ctx, version)
                .map_err(anyhow::Error::new)?;
            let source = selected_package_source(app, backend).await?;
            write_npmrc(&project, source.as_deref())?;
            let package_spec = format!("{}@{}", backend.package(), version.version);
            let (scripts_enabled, allow_all) = build_policy_flags(version)?;
            aube_host::install_global_package(EmbeddedInstallRequest {
                project_dir: &project,
                packages: std::slice::from_ref(&package_spec),
                cache_dir: NpmPackageBackend::aube_cache_dir(&app.ctx),
                store_dir: NpmPackageBackend::aube_store_dir(&app.ctx),
                node_bin_dir: runtime.node_bin.clone(),
                scripts_enabled,
                dangerously_allow_all_builds: allow_all,
                offline: app.ctx.config.settings.offline,
            })
            .await
            .map_err(anyhow::Error::new)?;
            publish_aube_global_bins(backend, &app.ctx, version)
        }
        NpmInstaller::Npm | NpmInstaller::Pnpm => {
            run_native_installer(app, backend, version, installer, runtime).await
        }
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    }
}

fn publish_aube_global_bins(
    backend: &NpmPackageBackend,
    ctx: &osdk_core::backend::Ctx,
    version: &ToolVersion,
) -> Result<()> {
    let install_root = backend.install_root(ctx, &version.version);
    let source = backend
        .project_dir(ctx, &version.version)
        .join("node_modules/.bin");
    let target = install_root.join("bin");
    std::fs::create_dir_all(&target)?;
    let canonical_root = dunce::canonicalize(&install_root)?;
    let mut published = 0usize;
    for entry in std::fs::read_dir(&source)
        .with_context(|| format!("reading Aube global bins {}", source.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        #[cfg(windows)]
        let Some(name) = file_name
            .to_ascii_lowercase()
            .strip_suffix(".cmd")
            .map(str::to_string)
        else {
            continue;
        };
        #[cfg(not(windows))]
        let name = file_name.to_string();
        let entry_path = entry.path();
        let canonical = dunce::canonicalize(&entry_path)?;
        if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
            return Err(anyhow!(
                "Aube global bin `{name}` escapes {}",
                install_root.display()
            ));
        }
        #[cfg(unix)]
        {
            let destination = target.join(&name);
            let _ = std::fs::remove_file(&destination);
            // Keep the package-manager launcher itself. Recreating a direct
            // link to its resolved JS target can lose the launcher's shebang,
            // relative module-resolution base, or wrapper environment.
            std::os::unix::fs::symlink(&entry_path, &destination)?;
        }
        #[cfg(windows)]
        {
            let destination = target.join(format!("{name}.cmd"));
            let wrapper = format!("@echo off\r\ncall \"{}\" %*\r\n", entry.path().display());
            std::fs::write(destination, wrapper)?;
        }
        published += 1;
    }
    if published == 0 {
        return Err(anyhow!(
            "Aube installed no executable bins for {}",
            backend.id()
        ));
    }
    Ok(())
}

async fn selected_package_source(app: &App, backend: &dyn Backend) -> Result<Option<String>> {
    let sources = select::ranked_source_list(&app.ctx, backend).await?;
    Ok(sources.first().map(|source| source.download_url.clone()))
}

async fn run_native_installer(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
) -> Result<()> {
    let (_, manager_version, executable) = runtime
        .manager
        .as_ref()
        .ok_or_else(|| anyhow!("managed {installer} was not prepared"))?;
    let (manager, executable_alias) = match installer {
        NpmInstaller::Npm => (PackageManager::Npm, "npm"),
        NpmInstaller::Pnpm => (PackageManager::Pnpm, "pnpm"),
        _ => unreachable!(),
    };
    let package_spec = format!("{}@{}", backend.package(), version.version);
    let install_root = backend.install_root(&app.ctx, &version.version);
    let bin_dir = global_bin_dir(backend, &app.ctx, version, installer);
    let args = native_args(
        installer,
        &install_root,
        &bin_dir,
        &package_spec,
        version,
        &app.ctx.dirs,
        app.ctx.config.settings.offline,
    )?;
    let logical_args =
        native_preflight_args(installer, &package_spec, app.ctx.config.settings.offline);
    let registry_plan = package_registry::plan(
        &app.ctx,
        &install_root,
        manager,
        executable_alias,
        &logical_args,
        |_| None,
    )
    .await?;
    let mut env = isolated_native_env(
        app,
        &install_root,
        &bin_dir,
        installer,
        &runtime.node_bin,
        manager_version,
    )?;
    match registry_plan {
        RegistryPlan::Selected { url, .. } => {
            env.insert(package_registry::registry_env(manager).into(), url);
        }
        RegistryPlan::Unavailable { probes } => {
            return Err(unavailable_registry_error(manager, &probes));
        }
        RegistryPlan::PassThrough { .. } if app.ctx.config.settings.offline => {}
        RegistryPlan::PassThrough { reason } => {
            return Err(anyhow!(
                "cannot safely isolate global {manager} install: registry preflight passed through ({reason})"
            ));
        }
    }
    run_managed_command(executable, &args, &env, &install_root)
}

fn native_args(
    installer: NpmInstaller,
    install_root: &Path,
    bin_dir: &Path,
    package_spec: &str,
    version: &ToolVersion,
    dirs: &osdk_core::dirs::Dirs,
    offline: bool,
) -> Result<Vec<String>> {
    let allow_builds = version.options.get("allow_builds").map(String::as_str);
    match installer {
        NpmInstaller::Npm => {
            let mut args = vec![
                "install".into(),
                "--global".into(),
                "--prefix".into(),
                install_root.display().to_string(),
                "--audit=false".into(),
                "--fund=false".into(),
            ];
            match allow_builds {
                None | Some("" | "false" | "0" | "no" | "off") => {
                    args.push("--ignore-scripts".into())
                }
                Some("true" | "1" | "yes" | "on") => {}
                Some(_) => {
                    return Err(anyhow!(
                        "installer `npm` cannot enforce a package allowlist; use allow_builds=false or true"
                    ))
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        NpmInstaller::Pnpm => {
            let mut args = vec![
                "add".into(),
                "--global".into(),
                "--global-dir".into(),
                install_root.join("pnpm-global").display().to_string(),
                "--global-bin-dir".into(),
                bin_dir.display().to_string(),
                "--store-dir".into(),
                dirs.store.join("pnpm-store").display().to_string(),
            ];
            match allow_builds {
                None | Some("" | "false" | "0" | "no" | "off") => {
                    args.push("--ignore-scripts".into())
                }
                Some("true" | "1" | "yes" | "on") => {
                    args.push("--dangerously-allow-all-builds".into())
                }
                Some(packages) => {
                    for package in packages
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                    {
                        args.push(format!("--allow-build={package}"));
                    }
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        _ => unreachable!(),
    }
}

fn native_preflight_args(
    installer: NpmInstaller,
    package_spec: &str,
    offline: bool,
) -> Vec<String> {
    let mut args = vec![
        if installer == NpmInstaller::Npm {
            "install".into()
        } else {
            "add".into()
        },
        package_spec.into(),
    ];
    if offline {
        args.push("--offline".into());
    }
    args
}

fn isolated_native_env(
    app: &App,
    install_root: &Path,
    bin_dir: &Path,
    installer: NpmInstaller,
    node_bin: &Path,
    manager_version: &ToolVersion,
) -> Result<BTreeMap<String, String>> {
    let native = install_root.join(NATIVE_CONFIG_DIR);
    std::fs::create_dir_all(&native)?;
    let user_config = native.join("user.npmrc");
    let global_config = native.join("global.npmrc");
    for path in [&user_config, &global_config] {
        if !path.exists() {
            std::fs::write(path, b"")?;
        }
    }
    let manager_dir = runtime_manager_dir(app, installer, manager_version)?;
    let mut path_entries = vec![bin_dir.to_path_buf(), manager_dir, node_bin.to_path_buf()];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries)?;
    let cache_root = osdk_core::cache::downstream_root(&app.ctx.dirs.cache);
    let mut env = BTreeMap::from([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        ("HOME".into(), native.join("home").display().to_string()),
        (
            "npm_config_userconfig".into(),
            user_config.display().to_string(),
        ),
        (
            "npm_config_globalconfig".into(),
            global_config.display().to_string(),
        ),
        ("npm_config_update_notifier".into(), "false".into()),
        ("npm_config_audit".into(), "false".into()),
        ("npm_config_fund".into(), "false".into()),
        ("COREPACK_ENABLE_PROJECT_SPEC".into(), "0".into()),
    ]);
    std::fs::create_dir_all(native.join("home"))?;
    match installer {
        NpmInstaller::Npm => {
            env.insert(
                "npm_config_prefix".into(),
                install_root.display().to_string(),
            );
            env.insert(
                "npm_config_cache".into(),
                cache_root.join("npm").display().to_string(),
            );
        }
        NpmInstaller::Pnpm => {
            env.insert("PNPM_HOME".into(), bin_dir.display().to_string());
            env.insert(
                "pnpm_config_cache_dir".into(),
                cache_root.join("pnpm").display().to_string(),
            );
            env.insert(
                "pnpm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "npm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "pnpm_config_state_dir".into(),
                app.ctx
                    .dirs
                    .data
                    .join("npm-native-state/pnpm")
                    .display()
                    .to_string(),
            );
        }
        _ => unreachable!(),
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    Ok(env)
}

fn runtime_manager_dir(
    app: &App,
    installer: NpmInstaller,
    manager_version: &ToolVersion,
) -> Result<PathBuf> {
    let backend = app.registry.get(installer.as_str())?;
    backend
        .bin_paths(&app.ctx, manager_version)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("managed {installer} has no bin directory"))
}

fn run_managed_command(
    executable: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<()> {
    let mut command = if cfg!(windows)
        && executable
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            }) {
        let mut command = Command::new(
            std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
        );
        command.args(["/D", "/S", "/C", "call"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let output = command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running managed installer {}", executable.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "managed installer {} failed with {}:\n{}",
            executable.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn native_lock_path(
    backend: &NpmPackageBackend,
    ctx: &osdk_core::backend::Ctx,
    version: &ToolVersion,
    installer: NpmInstaller,
) -> Option<PathBuf> {
    match installer {
        NpmInstaller::Aube => Some(
            backend
                .project_dir(ctx, &version.version)
                .join("aube-lock.yaml"),
        ),
        NpmInstaller::Npm => None,
        NpmInstaller::Pnpm => find_lockfile(
            &backend
                .install_root(ctx, &version.version)
                .join("pnpm-global"),
            "pnpm-lock.yaml",
        ),
        NpmInstaller::Auto => unreachable!(),
    }
}

fn find_lockfile(root: &Path, file_name: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
        .map(|entry| entry.into_path())
        .min()
}

fn global_bin_dir(
    backend: &NpmPackageBackend,
    ctx: &osdk_core::backend::Ctx,
    version: &ToolVersion,
    installer: NpmInstaller,
) -> PathBuf {
    let root = backend.install_root(ctx, &version.version);
    match installer {
        NpmInstaller::Aube => root.join("bin"),
        NpmInstaller::Npm if ctx.platform.os == osdk_core::platform::Os::Windows => root,
        NpmInstaller::Npm | NpmInstaller::Pnpm => root.join("bin"),
        NpmInstaller::Auto => unreachable!(),
    }
}

fn validate_global_package_identity(
    backend: &NpmPackageBackend,
    ctx: &osdk_core::backend::Ctx,
    version: &ToolVersion,
    installer: NpmInstaller,
) -> Result<()> {
    let root = backend.install_root(ctx, &version.version);
    let package_json = match installer {
        NpmInstaller::Aube => backend
            .project_dir(ctx, &version.version)
            .join("node_modules")
            .join(backend.package())
            .join("package.json"),
        NpmInstaller::Npm => {
            #[cfg(windows)]
            let modules = root.join("node_modules");
            #[cfg(not(windows))]
            let modules = root.join("lib/node_modules");
            modules.join(backend.package()).join("package.json")
        }
        NpmInstaller::Pnpm => {
            let global = root.join("pnpm-global");
            find_package_json(&global, backend.package()).ok_or_else(|| {
                anyhow!(
                    "managed pnpm global install did not contain {}@{} under {}",
                    backend.package(),
                    version.version,
                    global.display()
                )
            })?
        }
        NpmInstaller::Auto => unreachable!(),
    };
    let bytes = std::fs::read(&package_json)
        .with_context(|| format!("reading installed package {}", package_json.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing installed package {}", package_json.display()))?;
    let actual_name = manifest.get("name").and_then(serde_json::Value::as_str);
    let actual_version = manifest.get("version").and_then(serde_json::Value::as_str);
    if actual_name != Some(backend.package()) || actual_version != Some(version.version.as_str()) {
        return Err(anyhow!(
            "installed global package identity mismatch: expected {}@{}, found {}@{}",
            backend.package(),
            version.version,
            actual_name.unwrap_or("<missing>"),
            actual_version.unwrap_or("<missing>")
        ));
    }
    Ok(())
}

fn find_package_json(root: &Path, package: &str) -> Option<PathBuf> {
    let suffix = format!("node_modules/{package}/package.json").replace('\\', "/");
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "package.json")
        .map(|entry| entry.into_path())
        .filter(|path| path.to_string_lossy().replace('\\', "/").ends_with(&suffix))
        .min()
}

#[derive(Debug)]
struct NativeLockIdentity {
    kind: &'static str,
    format: String,
    sha256: String,
}

fn read_native_lock(path: &Path, installer: NpmInstaller) -> Result<NativeLockIdentity> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading native lock {}", path.display()))?;
    let (kind, format) = match installer {
        NpmInstaller::Aube | NpmInstaller::Pnpm => {
            let value: serde_yaml::Value = serde_yaml::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = yaml_lock_major(
                value
                    .get("lockfileVersion")
                    .ok_or_else(|| anyhow!("{} is missing lockfileVersion", path.display()))?,
            )?;
            let kind = if installer == NpmInstaller::Aube {
                "aube"
            } else {
                "pnpm"
            };
            (kind, format!("{kind}-v{major}"))
        }
        NpmInstaller::Npm => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = value
                .get("lockfileVersion")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("{} is missing numeric lockfileVersion", path.display()))?;
            ("npm", format!("package-lock-v{major}"))
        }
        NpmInstaller::Auto => unreachable!(),
    };
    Ok(NativeLockIdentity {
        kind,
        format,
        sha256: osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256),
    })
}

fn yaml_lock_major(value: &serde_yaml::Value) -> Result<u64> {
    let raw = match value {
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => return Err(anyhow!("native lockfileVersion must be a string or number")),
    };
    raw.trim()
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("native lockfileVersion is malformed"))
}

fn inject_native_metadata(
    version: &mut ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    native: Option<&NativeLockIdentity>,
) {
    version.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    version
        .options
        .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.into());
    for key in [
        LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
        LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION,
        LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION,
    ] {
        version.options.remove(key);
    }
    if let Some(native) = native {
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            native.kind.into(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            native.format.clone(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
            native.sha256.clone(),
        );
    }
}

fn completed_install_matches(
    backend: &NpmPackageBackend,
    ctx: &osdk_core::backend::Ctx,
    version: &ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    lock_path: Option<&Path>,
) -> Result<bool> {
    let root = backend.install_root(ctx, &version.version);
    if !root.join(".osdk-complete").is_file() {
        return Ok(false);
    }
    if installer != NpmInstaller::Npm && lock_path.is_none() {
        return Ok(false);
    }
    let manifest = match osdk_core::inventory::DynamicToolManifest::load(&root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let expected_native = match lock_path {
        Some(path) if path.is_file() => match read_native_lock(path, installer) {
            Ok(native) => Some(native),
            Err(_) => return Ok(false),
        },
        Some(_) if installer != NpmInstaller::Npm => return Ok(false),
        _ => None,
    };
    let native_matches = match expected_native {
        Some(native) => {
            manifest.metadata.get("native_lock_format") == Some(&native.format)
                && manifest.metadata.get("lock_sha256") == Some(&native.sha256)
        }
        None => {
            !manifest.metadata.contains_key("native_lock_format")
                && !manifest.metadata.contains_key("lock_sha256")
        }
    };
    let bin_dir = global_bin_dir(backend, ctx, version, installer);
    Ok(manifest.id == version.backend
        && manifest.version.as_deref() == Some(version.version.as_str())
        && manifest.metadata.get("installer").map(String::as_str) == Some(installer.as_str())
        && manifest.metadata.get("scope").map(String::as_str) == Some("global")
        && manifest.metadata.get("node_version").map(String::as_str) == Some(node_version)
        && native_matches
        && bin_dir.is_dir())
}

fn persist_global_lock(
    app: &App,
    request: &ToolRequest,
    version: &ToolVersion,
    runtime: &ManagedRuntime,
) -> Result<()> {
    let path = app.ctx.dirs.user_lock_file();
    let mut resolved = vec![(runtime.node_request.clone(), runtime.node_version.clone())];
    if let Some((manager_request, manager_version, _)) = &runtime.manager {
        resolved.push((manager_request.clone(), manager_version.clone()));
    }
    let locked_request = exact_request(request.clone(), version);
    resolved.push((locked_request, version.clone()));
    crate::lockfile::upsert_resolved_many_with_scope(
        &path,
        app.ctx.platform,
        &app.ctx.dirs,
        &resolved,
        crate::lockfile::LockScope::Global,
    )?;
    Ok(())
}

fn persist_global_config(
    app: &App,
    request: &ToolRequest,
    spec: &str,
    installer: NpmInstaller,
) -> Result<()> {
    let mut options = request
        .options
        .iter()
        .filter(|(key, _)| !key.starts_with("__osdk_"))
        .map(|(key, value)| (key.clone(), option_value(key, value)))
        .collect::<BTreeMap<_, _>>();
    options.insert(
        INSTALLER_OPTION.into(),
        osdk_core::config::ToolConfigValue::String(installer.as_str().into()),
    );
    crate::config_edit::set_global_tool_config(
        &app.ctx,
        &request.backend,
        &osdk_core::config::StructuredToolConfig {
            version: spec.into(),
            options,
        },
    )
}

fn option_value(key: &str, value: &str) -> osdk_core::config::ToolConfigValue {
    if key == "allow_builds" {
        return match value.to_ascii_lowercase().as_str() {
            "true" => osdk_core::config::ToolConfigValue::Bool(true),
            "false" => osdk_core::config::ToolConfigValue::Bool(false),
            _ => osdk_core::config::ToolConfigValue::Array(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
        };
    }
    osdk_core::config::ToolConfigValue::String(value.into())
}

fn build_policy_flags(version: &ToolVersion) -> Result<(bool, bool)> {
    let Some(raw) = version.options.get("allow_builds") else {
        return Ok((false, false));
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "false" | "0" | "no" | "off" => Ok((false, false)),
        "true" | "1" | "yes" | "on" => Ok((true, true)),
        _ => Ok((true, false)),
    }
}

fn apply_source_override(app: &mut App, tool: &str) {
    if let Some(id) = app.source_override.clone() {
        app.ctx
            .config
            .sources
            .per_tool
            .entry(tool.into())
            .or_default()
            .pin = Some(id);
    }
}

fn expand_alias(app: &App, request: &ToolRequest) -> Result<ToolRequest> {
    let mut effective = request.clone();
    effective.spec = VersionSpec::parse(
        &app.ctx
            .config
            .expand_alias(&request.backend, &request.spec.to_string())?,
    );
    Ok(effective)
}

fn find_bin_dir(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
    executable: &str,
) -> Result<PathBuf> {
    backend
        .bin_paths(ctx, version)?
        .into_iter()
        .find(|path| find_executable(std::slice::from_ref(path), executable).is_some())
        .ok_or_else(|| anyhow!("managed {executable} executable was not installed"))
}

fn find_executable(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.into(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    paths.iter().find_map(|directory| {
        candidates
            .iter()
            .map(|candidate| directory.join(candidate))
            .find(|candidate| candidate.is_file())
    })
}

fn write_npmrc(project: &Path, registry: Option<&str>) -> Result<()> {
    let path = project.join(".npmrc");
    match registry {
        Some(registry) => std::fs::write(&path, format!("registry={registry}\n"))?,
        None if path.exists() => std::fs::remove_file(&path)?,
        None => {}
    }
    Ok(())
}

fn unavailable_registry_error(manager: PackageManager, probes: &[RegistryProbe]) -> anyhow::Error {
    let details = probes
        .iter()
        .map(|probe| {
            format!(
                "{} ({})",
                probe.url,
                probe.error.as_deref().unwrap_or("unreachable")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!("no usable {manager} registry; manager was not started: {details}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_lock_identity_reads_all_supported_formats() {
        let temporary = tempfile::tempdir().unwrap();
        let cases = [
            (
                NpmInstaller::Aube,
                "aube-lock.yaml",
                "lockfileVersion: '9.0'\n",
                "aube-v9",
            ),
            (
                NpmInstaller::Pnpm,
                "pnpm-lock.yaml",
                "lockfileVersion: '9.0'\n",
                "pnpm-v9",
            ),
            (
                NpmInstaller::Npm,
                "package-lock.json",
                "{\"lockfileVersion\":3}",
                "package-lock-v3",
            ),
        ];
        for (installer, name, contents, format) in cases {
            let path = temporary.path().join(name);
            std::fs::write(&path, contents).unwrap();
            let identity = read_native_lock(&path, installer).unwrap();
            assert_eq!(identity.format, format);
            assert_eq!(identity.sha256.len(), 64);
        }
    }

    #[test]
    fn native_arguments_use_real_global_modes_with_controlled_roots() {
        let install_root = Path::new("/tmp/osdk-global/install");
        let bin_dir = install_root.join("bin");
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-global/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-global/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-global/config".into()),
            _ => None,
        })
        .unwrap();
        let npm = native_args(
            NpmInstaller::Npm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            false,
        )
        .unwrap();
        assert_eq!(npm.first().map(String::as_str), Some("install"));
        assert!(npm.iter().any(|arg| arg == "--global"));
        assert!(npm
            .windows(2)
            .any(|pair| pair[0] == "--prefix" && pair[1] == install_root.display().to_string()));
        assert!(npm.iter().any(|arg| arg == "--ignore-scripts"));

        let pnpm = native_args(
            NpmInstaller::Pnpm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            false,
        )
        .unwrap();
        assert_eq!(pnpm.first().map(String::as_str), Some("add"));
        assert!(pnpm.iter().any(|arg| arg == "--global"));
        assert!(pnpm
            .windows(2)
            .any(|pair| pair[0] == "--global-bin-dir" && pair[1] == bin_dir.display().to_string()));
        assert!(pnpm.windows(2).any(|pair| pair[0] == "--store-dir"
            && pair[1] == dirs.store.join("pnpm-store").display().to_string()));

        let offline = native_args(
            NpmInstaller::Npm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            true,
        )
        .unwrap();
        assert!(offline.iter().any(|arg| arg == "--offline"));
        assert_eq!(
            native_preflight_args(NpmInstaller::Npm, "prettier@3.6.2", true),
            vec!["install", "prettier@3.6.2", "--offline"]
        );
    }

    #[test]
    fn npm_global_completion_does_not_require_a_native_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs: dirs.clone(),
            platform: osdk_core::platform::Platform::current(),
            config: osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap(),
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let root = backend.install_root(&ctx, &version.version);
        std::fs::create_dir_all(global_bin_dir(&backend, &ctx, &version, NpmInstaller::Npm))
            .unwrap();
        let mut manifest = osdk_core::inventory::DynamicToolManifest::new(backend.id()).unwrap();
        manifest.version = Some(version.version.clone());
        manifest.metadata = BTreeMap::from([
            ("installer".into(), "npm".into()),
            ("scope".into(), "global".into()),
            ("node_version".into(), "22.1.0".into()),
        ]);
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();

        assert!(completed_install_matches(
            &backend,
            &ctx,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            None,
        )
        .unwrap());
    }

    #[test]
    fn completed_global_install_requires_matching_native_digest_and_runtime() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs: dirs.clone(),
            platform: osdk_core::platform::Platform::current(),
            config: osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap(),
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let root = backend.install_root(&ctx, &version.version);
        let project = backend.project_dir(&ctx, &version.version);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(project.join("aube-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let digest = read_native_lock(&project.join("aube-lock.yaml"), NpmInstaller::Aube).unwrap();
        let mut manifest = osdk_core::inventory::DynamicToolManifest::new("npm:prettier").unwrap();
        manifest.version = Some("3.6.2".into());
        manifest.metadata = BTreeMap::from([
            ("installer".into(), "aube".into()),
            ("scope".into(), "global".into()),
            ("node_version".into(), "22.1.0".into()),
            ("native_lock_format".into(), "aube-v9".into()),
            ("lock_sha256".into(), digest.sha256),
        ]);
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        assert!(completed_install_matches(
            &backend,
            &ctx,
            &version,
            NpmInstaller::Aube,
            "22.1.0",
            Some(&project.join("aube-lock.yaml"))
        )
        .unwrap());
        std::fs::write(
            project.join("aube-lock.yaml"),
            "lockfileVersion: '9.0'\nchanged: true\n",
        )
        .unwrap();
        assert!(!completed_install_matches(
            &backend,
            &ctx,
            &version,
            NpmInstaller::Aube,
            "22.1.0",
            Some(&project.join("aube-lock.yaml"))
        )
        .unwrap());
    }
}
