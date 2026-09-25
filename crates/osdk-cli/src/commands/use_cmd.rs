//! `use_cmd` command handlers (split from commands.rs).

use super::*;

pub async fn use_cmd(app: &mut App, tool: String, global: bool, opts: Vec<String>) -> Result<()> {
    // Same ambiguity as `install`: pinning a bare name that no backend owns would
    // write a spec nothing can resolve.
    report_bare_tool_name(app, &tool).await?;
    let requested_spec = requested_spec_literal(&tool);
    let (mut req, configured_key) = if global {
        resolve_explicit_request_target(
            app,
            &tool,
            &app.ctx.config.global_tool_configs,
            &app.ctx.config.global_tools,
        )?
    } else {
        let (request, key) = resolve_explicit_request_target(
            app,
            &tool,
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )?;
        (request, key)
    };
    apply_use_options(
        &app.ctx.config,
        &mut req,
        global,
        &opts,
        configured_key.as_deref(),
    )?;
    if global && req.backend.starts_with("npm:") {
        if configured_key.is_some() {
            anyhow::bail!(
                "global npm package aliases are not supported; use the canonical npm:<package> request"
            );
        }
        return crate::global_npm_use::install(app, req, requested_spec).await;
    }
    if !global && req.backend.starts_with("npm:") {
        let cwd = std::env::current_dir()?;
        let requested_installer =
            osdk_core::npm_tools::installer_from_request_options(&req.options)?;
        let plan = osdk_core::npm_tools::plan_npm_installer(
            &cwd,
            requested_installer,
            osdk_core::npm_tools::ToolScope::Project,
            app.ctx.config.settings.npm.default_installer,
        )?;
        if let Some(project) = plan.project {
            return use_project_npm(
                app,
                req,
                requested_spec,
                project,
                requested_installer,
                configured_key,
            )
            .await;
        }
    }
    use_legacy_cmd(app, req, requested_spec, global, configured_key).await
}

pub(crate) async fn use_legacy_cmd(
    app: &mut App,
    req: ToolRequest,
    requested_spec: Option<String>,
    global: bool,
    configured_key: Option<String>,
) -> Result<()> {
    let runtime_override = if global && req.backend.starts_with("go:") {
        Some(global_go_dependency_request(app)?)
    } else {
        None
    };
    let mut install_input = vec![req.clone()];
    if let Some(runtime) = runtime_override {
        install_input.push(runtime);
    }
    let installed = install_requests(app, install_input, Vec::new(), false, false).await?;
    let tv = installed
        .iter()
        .find_map(|(request, version)| (request.backend == req.backend).then_some(version.clone()))
        .ok_or_else(|| anyhow!("requested tool was not installed"))?;
    let persisted_options = if req.backend.starts_with("go:") {
        osdk_core::backend::dynamic::identity_options(&req.backend, &tv.options)?
    } else {
        req.options.clone()
    };
    // Pin the exact spec string the user typed (verbatim after `@`), so
    // channels like `stable` or `temurin-17` are preserved rather than being
    // normalized to `latest`. Bare `tool` (no `@`) pins the resolved version.
    let spec = requested_spec.unwrap_or_else(|| tv.version.clone());
    let persist_target = select_use_persist_target(app, &tv.backend, global, configured_key)?;
    let persisted_version = persist_version_for_target(&persist_target, &tv.backend, &spec);
    if global {
        if persisted_options.is_empty() {
            crate::config_edit::set_global_tool(&app.ctx, &persist_target.key, &persisted_version)?;
        } else {
            crate::config_edit::set_global_tool_config(
                &app.ctx,
                &persist_target.key,
                &structured_tool_config(&persisted_version, &persisted_options),
            )?;
        }
        println!(
            "{}",
            t!("msg.pinned_global", tool = persist_target.key, ver = spec)
        );
    } else {
        let path = if persisted_options.is_empty() {
            crate::config_edit::set_project_tool(&persist_target.key, &persisted_version)?
        } else {
            crate::config_edit::set_project_tool_config(
                &persist_target.key,
                &structured_tool_config(&persisted_version, &persisted_options),
            )?
        };
        println!(
            "{}",
            t!(
                "msg.pinned_project",
                tool = persist_target.key,
                ver = spec,
                path = path.display()
            )
        );
    }
    if req.backend.starts_with("go:") {
        let lock_path = if global {
            app.ctx.dirs.user_lock_file()
        } else {
            project_lock_path(app, &std::env::current_dir()?)
        };
        crate::lockfile::upsert_resolved_many_with_scope(
            &lock_path,
            app.ctx.platform,
            &app.ctx.dirs,
            &installed,
            if global {
                crate::lockfile::LockScope::Global
            } else {
                crate::lockfile::LockScope::Project
            },
        )?;
    }
    Ok(())
}

pub(crate) fn global_go_dependency_request(app: &App) -> Result<ToolRequest> {
    let spec = app
        .ctx
        .config
        .global_tools
        .get("go")
        .ok_or_else(|| {
            anyhow!(
                "global Go tools require a global managed Go selection; configure `go = \"1.24\"` globally"
            )
        })?;
    Ok(ToolRequest {
        backend: "go".into(),
        spec: VersionSpec::parse(spec),
        options: app
            .ctx
            .config
            .global_tool_configs
            .get("go")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

#[derive(Debug)]
pub(crate) struct UsePersistTarget {
    key: String,
    indirect: bool,
}

pub(crate) fn select_use_persist_target(
    app: &App,
    backend: &str,
    global: bool,
    configured_key: Option<String>,
) -> Result<UsePersistTarget> {
    if let Some(key) = configured_key {
        return Ok(UsePersistTarget {
            indirect: key != backend,
            key,
        });
    }

    let tools = if global {
        &app.ctx.config.global_tools
    } else {
        &app.ctx.config.tools
    };
    let matches = tools
        .iter()
        .filter_map(|(key, value)| {
            (key != backend)
                .then(|| ToolRequest::parse(value).ok())
                .flatten()
                .filter(|request| request.backend == backend)
                .map(|_| key.clone())
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(UsePersistTarget {
            key: backend.to_string(),
            indirect: false,
        }),
        [key] => Ok(UsePersistTarget {
            key: key.clone(),
            indirect: true,
        }),
        _ => anyhow::bail!(
            "multiple configured tool keys resolve to `{backend}`; use the desired key explicitly"
        ),
    }
}

pub(crate) fn persist_version_for_target(
    target: &UsePersistTarget,
    backend: &str,
    spec: &str,
) -> String {
    if target.indirect {
        format!("{backend}@{spec}")
    } else {
        spec.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectDependencySection {
    Dependencies,
    DevDependencies,
    OptionalDependencies,
    PeerDependencies { also_dev: bool },
}

pub(crate) async fn use_project_npm(
    app: &mut App,
    request: ToolRequest,
    requested_spec: Option<String>,
    project: osdk_core::npm_tools::NpmProject,
    requested_installer: osdk_core::npm_tools::NpmInstaller,
    configured_key: Option<String>,
) -> Result<()> {
    let package = request
        .backend
        .strip_prefix("npm:")
        .ok_or_else(|| anyhow!("project npm install requires an npm: package request"))?
        .to_string();
    let node_request = project_node_request(app, &project.root)?;
    let (node_backend, node_version) = install_one_without_shims(app, &node_request, false).await?;
    generate_shims_including_dependencies(app, node_backend.as_ref(), &node_version)?;
    let node_bin_dir = managed_bin_paths(&app.ctx, node_backend.as_ref(), &node_version, None)?
        .into_iter()
        .find(|path| path.join(node_executable_name()).is_file())
        .ok_or_else(|| {
            anyhow!(t!(
                "err.managed_node_bin_dir_missing",
                version = node_version.version
            ))
        })?;

    apply_source_override(app, &request.backend);
    let backend = app.registry.get(&request.backend)?;
    let mut effective = expand_request_alias(app, backend.as_ref(), &request)?;
    effective.options.insert(
        osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
        node_version.version.clone(),
    );
    let mut version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    bind_dynamic_request_options(&effective, &mut version);
    let package_spec = project_package_spec(&package, requested_spec.as_deref(), &version.version);
    // Re-open and re-plan after taking the project lock below. Another osdk
    // process may have changed the manifest or incumbent native lock while
    // Node/package resolution was running.
    let config_path = project_config_path(app, &project.root);
    let metadata_rollback = ProjectNpmMetadataRollback::begin(
        &app.ctx.dirs,
        &project.root,
        &project.package_json,
        &config_path,
    )?;
    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    let configured_spec = persisted_spec.clone();
    let config_key = configured_key.as_deref().unwrap_or(&request.backend);
    let configured_specs = project_npm_configured_specs(
        app,
        &config_path,
        Some((config_key, &request.backend, &configured_spec)),
    )?;
    let project = osdk_core::npm_tools::inspect_npm_project(&project.root)?
        .ok_or_else(|| anyhow!("project package.json disappeared before npm install"))?;
    let installer = osdk_core::npm_tools::plan_npm_installer(
        &project.root,
        requested_installer,
        osdk_core::npm_tools::ToolScope::Project,
        app.ctx.config.settings.npm.default_installer,
    )?
    .installer;
    let section = project_dependency_section(&project.package_json, &package)?;
    let expected_native_lock = expected_project_native_lock(&project, installer);

    let result: Result<(String, std::path::PathBuf)> = async {
        match installer {
            osdk_core::npm_tools::NpmInstaller::Npm | osdk_core::npm_tools::NpmInstaller::Pnpm => {
                run_project_native_installer(
                    app,
                    installer,
                    &project.root,
                    &package_spec,
                    section,
                    &node_bin_dir,
                )
                .await?;
            }
            osdk_core::npm_tools::NpmInstaller::Auto => {
                unreachable!("npm installer planning always returns a concrete installer")
            }
        }

        osdk_core::backend::npm_package::NpmPackageBackend::from_id(&request.backend)
            .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?
            .validate_project_package_bins(&project.root, &version.version)?;
        let installed_project = osdk_core::npm_tools::inspect_npm_project(&project.root)?
            .ok_or_else(|| anyhow!("project package.json disappeared during npm install"))?;
        let native_lock = installed_project.native_lock.ok_or_else(|| {
            anyhow!(
                "installer `{installer}` did not write a recognized native lockfile in {}",
                project.root.display()
            )
        })?;
        validate_installed_project_native_lock(installer, &expected_native_lock, &native_lock)?;
        record_project_npm_metadata(&mut version, installer, &native_lock, &node_version.version)?;

        let lock_path = project.root.join(crate::lockfile::LOCKFILE_NAME);
        crate::lockfile::upsert_resolved_many_with_scope(
            &lock_path,
            app.ctx.platform,
            &app.ctx.dirs,
            &[
                (node_request.clone(), node_version.clone()),
                (request.clone(), version.clone()),
            ],
            crate::lockfile::LockScope::Project,
        )?;

        osdk_core::backend::npm_package::publish_project_bin_generation(
            &project.root,
            &osdk_core::backend::npm_package::ProjectNpmBinSelection {
                backend: request.backend.clone(),
                configured_spec: configured_spec.clone(),
                version: version.version.clone(),
            },
            &configured_specs,
        )?;
        let mut config_options = request.options.clone();
        config_options.insert("installer".into(), installer.as_str().into());
        let config_version = configured_key
            .as_ref()
            .map(|_| format!("{}@{}", request.backend, persisted_spec))
            .unwrap_or_else(|| persisted_spec.clone());
        crate::config_edit::set_project_npm_tool_at(
            &config_path,
            &node_version.version,
            config_key,
            &structured_tool_config(&config_version, &config_options),
        )?;
        osdk_core::trust::trust(&app.ctx.dirs.config, &config_path)?;
        Ok((persisted_spec.clone(), config_path.clone()))
    }
    .await;

    let (persisted_spec, config_path) = match result {
        Ok(result) => result,
        Err(error) => return Err(metadata_rollback.rollback(error)),
    };
    metadata_rollback.commit();
    println!(
        "{}",
        t!(
            "msg.pinned_project",
            tool = configured_key.as_deref().unwrap_or(&request.backend),
            ver = persisted_spec,
            path = config_path.display()
        )
    );
    Ok(())
}

pub(crate) fn project_npm_configured_specs(
    app: &App,
    config_path: &std::path::Path,
    replacement: Option<(&str, &str, &str)>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let project_root = config_path
        .parent()
        .ok_or_else(|| anyhow!("project config has no parent: {}", config_path.display()))?;
    let config = osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), project_root)?;
    let mut entries = Vec::new();
    for (key, value) in &config.tools {
        if !matches!(
            config.tool_origins.get(key),
            Some(osdk_core::config::ToolConfigOrigin::ProjectConfig(path))
                if same_config_path(path, config_path)
        ) {
            continue;
        }
        if replacement.is_some_and(|(replacement_key, _, _)| key == replacement_key) {
            continue;
        }
        entries.push((key.clone(), value.clone()));
    }
    if let Some((key, backend, spec)) = replacement {
        let value = if key == backend {
            spec.to_string()
        } else {
            format!("{backend}@{spec}")
        };
        entries.push((key.to_string(), value));
    }
    osdk_core::activate::canonical_project_npm_specs(entries).map_err(anyhow::Error::from)
}

pub(crate) fn expected_project_native_lock(
    project: &osdk_core::npm_tools::NpmProject,
    installer: osdk_core::npm_tools::NpmInstaller,
) -> (osdk_core::npm_tools::NativeLockKind, std::path::PathBuf) {
    if let Some(lock) = &project.native_lock {
        return (lock.kind, lock.path.clone());
    }
    let kind = match installer {
        osdk_core::npm_tools::NpmInstaller::Npm => {
            osdk_core::npm_tools::NativeLockKind::PackageLock
        }
        osdk_core::npm_tools::NpmInstaller::Pnpm => osdk_core::npm_tools::NativeLockKind::Pnpm,
        osdk_core::npm_tools::NpmInstaller::Auto => {
            unreachable!("npm installer planning always returns a concrete installer")
        }
    };
    (kind, project.root.join(kind.file_name()))
}

pub(crate) fn validate_installed_project_native_lock(
    installer: osdk_core::npm_tools::NpmInstaller,
    expected: &(osdk_core::npm_tools::NativeLockKind, std::path::PathBuf),
    actual: &osdk_core::npm_tools::NativeLock,
) -> Result<()> {
    if actual.kind != expected.0 || actual.path != expected.1 {
        anyhow::bail!(
            "installer `{installer}` changed native lock identity: expected {}, got {}",
            expected.1.display(),
            actual.path.display()
        );
    }
    Ok(())
}

/// Roll back user-authored manifests and osdk publication metadata. Package
/// manager materialization is intentionally outside this bounded snapshot;
/// activation never exposes it unless a validated curated generation commits.
pub(crate) struct ProjectNpmMetadataRollback {
    _lock: osdk_core::lock::FileLock,
    snapshots: Vec<ProjectFileSnapshot>,
}

pub(crate) struct ProjectFileSnapshot {
    path: std::path::PathBuf,
    bytes: Option<Vec<u8>>,
}

impl ProjectNpmMetadataRollback {
    fn begin(
        dirs: &osdk_core::dirs::Dirs,
        project_root: &std::path::Path,
        package_json: &std::path::Path,
        config_path: &std::path::Path,
    ) -> Result<Self> {
        let canonical_root = dunce::canonicalize(project_root)
            .with_context(|| format!("canonicalizing {}", project_root.display()))?;
        let project_key = osdk_core::pipeline::verify::hash_bytes(
            canonical_root.to_string_lossy().as_bytes(),
            osdk_core::pipeline::HashAlgo::Sha256,
        );
        let lock = osdk_core::lock::FileLock::acquire(
            dirs.data
                .join("locks/project-npm")
                .join(format!("{project_key}.lock")),
        )?;
        let mut paths = vec![
            package_json.to_path_buf(),
            project_root.join("pnpm-lock.yaml"),
            project_root.join("package-lock.json"),
            project_root.join("npm-shrinkwrap.json"),
            project_root.join(crate::lockfile::LOCKFILE_NAME),
            config_path.to_path_buf(),
            osdk_core::backend::npm_package::project_bin_current_path(project_root),
        ];
        paths.sort();
        paths.dedup();
        let snapshots = paths
            .into_iter()
            .map(ProjectFileSnapshot::capture)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            _lock: lock,
            snapshots,
        })
    }

    fn rollback(&self, error: anyhow::Error) -> anyhow::Error {
        let failures = self
            .snapshots
            .iter()
            .rev()
            .filter_map(|snapshot| snapshot.restore().err())
            .map(|failure| failure.to_string())
            .collect::<Vec<_>>();
        if failures.is_empty() {
            error
        } else {
            error.context(format!("project rollback failed: {}", failures.join("; ")))
        }
    }

    fn commit(self) {}
}

impl ProjectFileSnapshot {
    fn capture(path: std::path::PathBuf) -> Result<Self> {
        let bytes = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Some(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
            }
            Ok(_) => anyhow::bail!(
                "project transaction path must be a regular file: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()))
            }
        };
        Ok(Self { path, bytes })
    }

    pub(crate) fn restore(&self) -> Result<()> {
        match &self.bytes {
            Some(bytes) => atomic_restore_project_file(&self.path, bytes),
            None => match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(&self.path)
                    .with_context(|| format!("removing {} during rollback", self.path.display())),
                Ok(_) => anyhow::bail!(
                    "refusing to remove non-file rollback target {}",
                    self.path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error)
                    .with_context(|| format!("inspecting {} during rollback", self.path.display())),
            },
        }
    }
}

pub(crate) fn atomic_restore_project_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("rollback path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating rollback directory {}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("rollback path has no file name: {}", path.display()))?
        .to_string_lossy();
    let temporary = (0..1024u32)
        .map(|attempt| {
            parent.join(format!(
                ".{name}.osdk-rollback-{}-{attempt}",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| anyhow!("could not allocate rollback path for {}", path.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating rollback file {}", temporary.display()))?;
    use std::io::Write as _;
    file.write_all(bytes)
        .with_context(|| format!("writing rollback file {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing rollback file {}", temporary.display()))?;
    drop(file);
    if let Err(error) = replace_project_file(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn snapshot_project_file(path: &std::path::Path) -> Result<ProjectFileSnapshot> {
    ProjectFileSnapshot::capture(path.to_path_buf())
}

#[cfg(not(windows))]
pub(crate) fn replace_project_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| format!("restoring {}", destination.display()))
}

#[cfg(windows)]
pub(crate) fn replace_project_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            REPLACE_EXISTING | WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("restoring {}", destination.display()));
    }
    Ok(())
}

pub(crate) fn project_dependency_section(
    path: &std::path::Path,
    package: &str,
) -> Result<ProjectDependencySection> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let contains = |section: &str| {
        manifest
            .get(section)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|dependencies| dependencies.contains_key(package))
    };
    Ok(if contains("optionalDependencies") {
        ProjectDependencySection::OptionalDependencies
    } else if contains("dependencies") {
        ProjectDependencySection::Dependencies
    } else if contains("peerDependencies") {
        ProjectDependencySection::PeerDependencies {
            also_dev: contains("devDependencies"),
        }
    } else {
        ProjectDependencySection::DevDependencies
    })
}

pub(crate) fn project_package_spec(
    package: &str,
    requested: Option<&str>,
    resolved: &str,
) -> String {
    format!("{package}@{}", requested.unwrap_or(resolved))
}

pub(crate) fn node_executable_name() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

pub(crate) fn project_node_request(
    app: &App,
    project_root: &std::path::Path,
) -> Result<ToolRequest> {
    let backend = app.registry.get("node")?;
    let spec = osdk_core::version::resolver::resolve_active(
        "node",
        project_root,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .map(|active| {
        if active.is_range {
            VersionSpec::parse_range(&active.spec)
        } else {
            Ok(VersionSpec::parse(&active.spec))
        }
    })
    .transpose()?
    .unwrap_or(VersionSpec::Latest);
    Ok(ToolRequest {
        backend: "node".into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get("node")
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

pub(crate) async fn run_project_native_installer(
    app: &mut App,
    installer: osdk_core::npm_tools::NpmInstaller,
    project_root: &std::path::Path,
    package_spec: &str,
    section: ProjectDependencySection,
    node_bin_dir: &std::path::Path,
) -> Result<()> {
    let manager_id = installer
        .executable()
        .filter(|manager| matches!(*manager, "npm" | "pnpm"))
        .ok_or_else(|| anyhow!("project native installer must be npm or pnpm"))?;
    let manager_request = project_manager_request(app, manager_id, project_root)?;
    let (manager_backend, manager_version) =
        install_one_without_shims(app, &manager_request, false).await?;
    generate_shims_including_dependencies(app, manager_backend.as_ref(), &manager_version)?;
    let manager = find_managed_executable(
        &managed_bin_paths(&app.ctx, manager_backend.as_ref(), &manager_version, None)?,
        manager_id,
    )
    .ok_or_else(|| {
        anyhow!(
            "managed {manager_id} executable not found for {}@{}",
            manager_version.backend,
            manager_version.version
        )
    })?;
    let args = project_manager_args(installer, package_spec, section);
    let mut env = manager_backend.exec_env(&app.ctx, &manager_version)?;
    let mut paths = vec![manager.parent().unwrap_or(project_root).to_path_buf()];
    paths.push(node_bin_dir.to_path_buf());
    if let Some(existing_path) = std::env::var_os("PATH").filter(|path| !path.is_empty()) {
        paths.extend(std::env::split_paths(&existing_path));
    }
    env.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    let resolved = vec![(manager_request, manager_version)];
    apply_package_registry_plan_at(app, &resolved, manager_id, &args, &mut env, project_root)
        .await?;
    let status = command_for_program(&manager)
        .args(&args)
        .current_dir(project_root)
        .envs(env)
        .status()
        .with_context(|| format!("running {}", manager.display()))?;
    if !status.success() {
        anyhow::bail!("command exited with {status}");
    }
    Ok(())
}

pub(crate) fn project_manager_request(
    app: &App,
    manager: &str,
    project_root: &std::path::Path,
) -> Result<ToolRequest> {
    let backend = app.registry.get(manager)?;
    let spec = osdk_core::version::resolver::resolve_active(
        manager,
        project_root,
        &app.ctx.config.tools,
        backend.idiomatic_files(),
    )
    .map(|active| {
        if active.is_range {
            VersionSpec::parse_range(&active.spec)
                .unwrap_or_else(|_| VersionSpec::parse(&active.spec))
        } else {
            VersionSpec::parse(&active.spec)
        }
    })
    .unwrap_or(VersionSpec::Latest);
    Ok(ToolRequest {
        backend: manager.into(),
        spec,
        options: app
            .ctx
            .config
            .tool_configs
            .get(manager)
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    })
}

pub(crate) fn project_manager_args(
    installer: osdk_core::npm_tools::NpmInstaller,
    package_spec: &str,
    section: ProjectDependencySection,
) -> Vec<String> {
    let mut args = match installer {
        osdk_core::npm_tools::NpmInstaller::Npm => vec!["install".into()],
        osdk_core::npm_tools::NpmInstaller::Pnpm => vec!["add".into()],
        _ => unreachable!("only native installers have command arguments"),
    };
    match (installer, section) {
        (osdk_core::npm_tools::NpmInstaller::Npm, ProjectDependencySection::Dependencies) => {
            args.push("--save-prod".into())
        }
        (osdk_core::npm_tools::NpmInstaller::Npm, ProjectDependencySection::DevDependencies) => {
            args.push("--save-dev".into())
        }
        (
            osdk_core::npm_tools::NpmInstaller::Npm,
            ProjectDependencySection::OptionalDependencies,
        ) => args.push("--save-optional".into()),
        (
            osdk_core::npm_tools::NpmInstaller::Npm,
            ProjectDependencySection::PeerDependencies { also_dev },
        ) => {
            args.push("--save-peer".into());
            if also_dev {
                args.push("--save-dev".into());
            }
        }
        (osdk_core::npm_tools::NpmInstaller::Pnpm, ProjectDependencySection::Dependencies) => {
            args.push("-P".into())
        }
        (osdk_core::npm_tools::NpmInstaller::Pnpm, ProjectDependencySection::DevDependencies) => {
            args.push("-D".into())
        }
        (
            osdk_core::npm_tools::NpmInstaller::Pnpm,
            ProjectDependencySection::OptionalDependencies,
        ) => args.push("-O".into()),
        (
            osdk_core::npm_tools::NpmInstaller::Pnpm,
            ProjectDependencySection::PeerDependencies { also_dev },
        ) => {
            args.push("--save-peer".into());
            if also_dev {
                args.push("-D".into());
            }
        }
        _ => unreachable!("only native installers have command arguments"),
    }
    args.push("--ignore-scripts".into());
    args.push(package_spec.into());
    args
}

pub(crate) fn record_project_npm_metadata(
    version: &mut ToolVersion,
    installer: osdk_core::npm_tools::NpmInstaller,
    native_lock: &osdk_core::npm_tools::NativeLock,
    node_version: &str,
) -> Result<()> {
    let sha256 = osdk_core::pipeline::verify::hash_file(
        &native_lock.path,
        osdk_core::pipeline::HashAlgo::Sha256,
    )?;
    version.options.insert(
        osdk_core::npm_tools::INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
        osdk_core::npm_tools::ToolScope::Project.as_str().into(),
    );
    version.options.insert(
        osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
        node_version.into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
        native_lock.installer_name().into(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
        native_lock.format.clone(),
    );
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
        sha256,
    );
    Ok(())
}

pub(crate) fn project_config_path(app: &App, project_root: &std::path::Path) -> std::path::PathBuf {
    app.ctx
        .config
        .project_config_path
        .as_ref()
        .filter(|path| {
            path.parent()
                .is_some_and(|parent| same_config_path(parent, project_root))
        })
        .cloned()
        .unwrap_or_else(|| project_root.join("osdk.toml"))
}

pub(crate) fn same_config_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
        || match (dunce::canonicalize(left), dunce::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

pub(crate) fn structured_tool_config(
    version: &str,
    options: &std::collections::BTreeMap<String, String>,
) -> osdk_core::config::StructuredToolConfig {
    osdk_core::config::StructuredToolConfig {
        version: version.to_string(),
        // Generated entries target the machine they are written on, so they
        // carry no platform restriction.
        when: None,
        options: options
            .iter()
            .map(|(key, value)| (key.clone(), structured_tool_option(key, value)))
            .collect(),
    }
}

pub(crate) fn structured_tool_option(key: &str, value: &str) -> osdk_core::config::ToolConfigValue {
    if key == "allow_builds" {
        let lower = value.to_ascii_lowercase();
        if matches!(lower.as_str(), "true" | "false") {
            return osdk_core::config::ToolConfigValue::Bool(lower == "true");
        }
        return osdk_core::config::ToolConfigValue::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    osdk_core::config::ToolConfigValue::String(value.to_string())
}
