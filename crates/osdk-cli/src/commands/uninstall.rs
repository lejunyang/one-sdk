//! `uninstall` command handlers (split from commands.rs).

use super::*;

pub(crate) const GLOBAL_NPM_UNINSTALL_JOURNAL_DIR: &str = "transactions/global-npm-uninstall";

pub(crate) static NEXT_GLOBAL_NPM_UNINSTALL_FILE: AtomicU64 = AtomicU64::new(0);

pub async fn uninstall(app: &App, tool: String, global: bool) -> Result<()> {
    let req = if global {
        resolve_explicit_request(
            app,
            &tool,
            &app.ctx.config.global_tool_configs,
            &app.ctx.config.global_tools,
        )?
    } else {
        resolve_explicit_request(
            app,
            &tool,
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )?
    };
    let backend = app.registry.get(&req.backend)?;
    if global && !req.backend.starts_with("npm:") {
        anyhow::bail!("--global is only supported for npm:<package> uninstall requests");
    }
    if global {
        crate::global_npm_use::with_global_npm_state_lock(&app.ctx.dirs, || {
            recover_interrupted_global_npm_uninstalls(app)
        })?;
    }
    let npm_backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&req.backend);
    let selected_npm = if global {
        Some(select_global_npm_version(
            app,
            &req,
            requested_spec_literal(&tool).is_some(),
        )?)
    } else if let Some(npm) = npm_backend.as_ref() {
        let hint = npm_scope_hint(&req, false);
        let installed = npm.list_installed_identities_for(&app.ctx, &hint)?;
        let spec = match &req.spec {
            VersionSpec::Exact(_) | VersionSpec::Prefix(_) => &req.spec,
            other => return Err(anyhow!(t!("err.specify_exact", spec = other))),
        };
        Some(select_installed_npm_identity(
            &req.backend,
            spec,
            installed,
        )?)
    } else {
        None
    };
    let version = if let Some(selected) = selected_npm.as_ref() {
        selected.version.clone()
    } else {
        match &req.spec {
            VersionSpec::Exact(v) => v.clone(),
            VersionSpec::Latest if backend.id() == "rust" => "stable".to_string(),
            VersionSpec::Prefix(p) => {
                // pick the installed version matching the prefix
                let installed = backend.list_installed(&app.ctx)?;
                installed
                    .into_iter()
                    .rfind(|v| v.starts_with(p.as_str()))
                    .ok_or_else(|| {
                        anyhow!(t!("err.no_installed_match", tool = req.backend, spec = p))
                    })?
            }
            other => return Err(anyhow!(t!("err.specify_exact", spec = other))),
        }
    };
    let mut tv = selected_npm.unwrap_or_else(|| ToolVersion::new(&req.backend, &version));
    if tv.options.is_empty() {
        tv.options = req.options.clone();
    }
    let question = t!("prompt.uninstall", tool = tv);
    if !app.prompt.confirm(&question)? {
        println!("{}", t!("msg.cancelled"));
        return Ok(());
    }
    if global {
        uninstall_global_npm(app, &tv)?;
    } else {
        backend.uninstall(&app.ctx, &tv).await?;
        app.invalidate_dynamic_scan();
        reconcile_managed_shims(app)?;
    }
    println!("{}", t!("msg.uninstalled", tool = tv));
    // Reclaim now-unreferenced store objects.
    let models = app.ctx.dirs.models();
    let (removed, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
    if removed > 0 {
        println!(
            "{}",
            t!(
                "msg.pruned_store",
                count = removed,
                size = human_bytes(bytes)
            )
        );
    }
    Ok(())
}

pub(crate) fn npm_scope_hint(request: &ToolRequest, global: bool) -> ToolVersion {
    let mut hint = ToolVersion::new(&request.backend, "scope-selection");
    hint.options = request.options.clone();
    if global {
        hint.options.insert(
            osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
            osdk_core::npm_tools::ToolScope::Global.as_str().into(),
        );
    }
    hint
}

pub(crate) fn select_global_npm_version(
    app: &App,
    request: &ToolRequest,
    explicit_spec: bool,
) -> Result<ToolVersion> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    let hint = npm_scope_hint(request, true);
    let installed = backend.list_installed_identities_for(&app.ctx, &hint)?;
    let spec = global_npm_selection_spec(app, request, explicit_spec)?;
    let selected_version = select_installed_version(
        &request.backend,
        &spec,
        installed
            .iter()
            .map(|candidate| candidate.version.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    )?;
    if !explicit_spec {
        let lock_path = app.ctx.dirs.user_lock_file();
        if lock_path.is_file() {
            if let Some(locked) = crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
                .into_iter()
                .flatten()
                .find(|locked| {
                    locked.backend == request.backend
                        && locked.spec == VersionSpec::Exact(selected_version.clone())
                        && locked
                            .options
                            .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                            .map(String::as_str)
                            == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
                })
            {
                let mut version = ToolVersion::new(&request.backend, &selected_version);
                version.options = locked.options;
                if backend
                    .where_install_root_for(&app.ctx, &version)?
                    .is_some()
                {
                    return Ok(version);
                }
            }
        }
    }
    let mut version = select_installed_npm_identity(
        &request.backend,
        &VersionSpec::Exact(selected_version),
        installed,
    )?;
    version.options.insert(
        osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION.into(),
        osdk_core::npm_tools::ToolScope::Global.as_str().into(),
    );
    Ok(version)
}

pub(crate) fn global_npm_selection_spec(
    app: &App,
    request: &ToolRequest,
    explicit_spec: bool,
) -> Result<VersionSpec> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let spec = if explicit_spec {
        request.spec.clone()
    } else {
        let configured = config
            .global_tool_configs
            .get(&request.backend)
            .map(|entry| VersionSpec::parse(entry.version()));
        match configured {
            Some(VersionSpec::Exact(version)) => VersionSpec::Exact(version),
            Some(spec) => {
                let lock_path = app.ctx.dirs.user_lock_file();
                if lock_path.is_file() {
                    if let Some(locked) =
                        crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
                            .unwrap_or_default()
                            .into_iter()
                            .find(|locked| {
                                locked.backend == request.backend
                                    && locked
                                        .options
                                        .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                                        .map(String::as_str)
                                        == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
                            })
                    {
                        locked.spec
                    } else {
                        spec
                    }
                } else {
                    spec
                }
            }
            None => request.spec.clone(),
        }
    };
    let expanded = config.expand_alias(&request.backend, &spec.to_string())?;
    Ok(VersionSpec::parse(&expanded))
}

pub(crate) fn select_installed_version(
    backend: &str,
    spec: &VersionSpec,
    installed: Vec<String>,
) -> Result<String> {
    if let VersionSpec::Exact(version) = spec {
        if installed.iter().any(|candidate| candidate == version) {
            return Ok(version.clone());
        }
        anyhow::bail!("{backend}@{version} is not installed");
    }
    let infos = installed
        .iter()
        .map(osdk_core::version::VersionInfo::stable)
        .collect::<Vec<_>>();
    osdk_core::version::select_version(spec, &infos)
        .map(|version| version.version.clone())
        .ok_or_else(|| anyhow!("{backend} is not installed"))
}

pub(crate) fn select_installed_npm_identity(
    backend: &str,
    spec: &VersionSpec,
    installed: Vec<ToolVersion>,
) -> Result<ToolVersion> {
    let versions = installed
        .iter()
        .map(|candidate| candidate.version.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let selected = select_installed_version(backend, spec, versions)?;
    let mut matches = installed
        .into_iter()
        .filter(|candidate| candidate.version == selected);
    let candidate = matches
        .next()
        .ok_or_else(|| anyhow!("{backend}@{selected} is not installed"))?;
    if matches.next().is_some() {
        anyhow::bail!(
            "{backend}@{selected} has multiple installed identities; use a lockfile-backed selection or remove an obsolete variant"
        );
    }
    Ok(candidate)
}

pub(crate) fn uninstall_global_npm(app: &App, version: &ToolVersion) -> Result<()> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", version.backend))?;
    crate::global_npm_use::with_global_npm_state_lock(&app.ctx.dirs, || {
        recover_interrupted_global_npm_uninstalls(app)?;
        let roots = global_npm_install_roots(&app.ctx, &backend, version)?;
        if roots.is_empty() {
            anyhow::bail!("{} is not installed in global scope", version);
        }
        let bin_names = roots
            .iter()
            .map(|root| osdk_core::inventory::DynamicToolManifest::load(root))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(|manifest| manifest.bins.into_iter().map(|bin| bin.name))
            .collect::<std::collections::BTreeSet<_>>();
        let installed_before = backend.list_installed_for(
            &app.ctx,
            &npm_scope_hint(&exact_request_for_version(version), true),
        )?;
        let remove_config = global_config_selects_version(app, version, &installed_before)?;
        let remove_lock = global_lock_selects_version(app, version)?;
        let snapshots = global_npm_metadata_snapshots(app, &bin_names)?;
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            app,
            version,
            &roots,
            &bin_names,
            remove_config,
            remove_lock,
        )?;
        for index in 0..transaction.journal.roots.len() {
            let original = transaction.journal.roots[index].original.clone();
            let backup = transaction.journal.roots[index].backup.clone();
            if let Err(error) = rename_global_npm_uninstall_path(&original, &backup) {
                let rollback = transaction.rollback_roots();
                return Err(with_uninstall_rollback(
                    anyhow!(error).context(format!("staging removal of {}", original.display())),
                    rollback.err(),
                ));
            }
        }
        // Every root is now recoverable from a durable backup. Crossing this
        // write-ahead commit makes a crash complete the uninstall; normal
        // returned errors still use the in-process snapshots to roll back.
        transaction.mark_committed()?;
        let metadata_result = (|| {
            if remove_config {
                crate::config_edit::remove_global_tool_unlocked(&app.ctx, &version.backend)?;
            }
            if remove_lock {
                crate::lockfile::remove_tool(
                    &app.ctx.dirs.user_lock_file(),
                    app.ctx.platform,
                    &version.backend,
                )?;
            }
            remove_unowned_global_npm_shims(app, &bin_names)
        })();
        if let Err(error) = metadata_result {
            let metadata_rollback = restore_global_npm_metadata(&snapshots);
            let install_rollback = transaction.rollback_roots();
            let shim_rollback = refreshed_global_npm_app(app)
                .and_then(|app| generate_global_npm_shims_for(&app, version, &bin_names));
            let rollback = combine_rollback_errors(
                combine_rollback_errors(metadata_rollback.err(), install_rollback.err()),
                shim_rollback.err(),
            );
            return Err(with_uninstall_rollback(error, rollback));
        }
        transaction.finish();
        Ok(())
    })
}

pub(crate) fn global_npm_install_roots(
    ctx: &osdk_core::backend::Ctx,
    backend: &osdk_core::backend::npm_package::NpmPackageBackend,
    version: &ToolVersion,
) -> Result<Vec<std::path::PathBuf>> {
    let mut roots = Vec::new();
    if let Some(root) = backend.existing_global_install_root(ctx, version)? {
        roots.push(root);
    }
    if let Some(legacy) = backend.legacy_global_install_root(ctx, version)? {
        if !roots.contains(&legacy) {
            roots.push(legacy);
        }
    }
    Ok(roots)
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobalNpmUninstallJournal {
    pub(crate) backend: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) options: std::collections::BTreeMap<String, String>,
    pub(crate) roots: Vec<GlobalNpmUninstallRoot>,
    pub(crate) bin_names: Vec<String>,
    pub(crate) config_entry: Option<osdk_core::config::ToolConfigEntry>,
    pub(crate) lock_entry: Option<GlobalNpmUninstallLockEntry>,
    pub(crate) committed: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobalNpmUninstallRoot {
    pub(crate) original: std::path::PathBuf,
    pub(crate) backup: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobalNpmUninstallLockEntry {
    version: String,
    options: std::collections::BTreeMap<String, String>,
}

pub(crate) struct GlobalNpmUninstallTransaction {
    pub(crate) path: std::path::PathBuf,
    pub(crate) journal: GlobalNpmUninstallJournal,
    completed: bool,
}

impl GlobalNpmUninstallTransaction {
    pub(crate) fn prepare(
        app: &App,
        version: &ToolVersion,
        roots: &[std::path::PathBuf],
        bin_names: &std::collections::BTreeSet<String>,
        remove_config: bool,
        remove_lock: bool,
    ) -> Result<Self> {
        let roots = roots
            .iter()
            .map(|original| {
                Ok(GlobalNpmUninstallRoot {
                    original: original.clone(),
                    backup: unused_sibling_path(original, "uninstall-backup")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let path = global_npm_uninstall_journal_path(&app.ctx.dirs, version);
        if path.exists() {
            anyhow::bail!(
                "global npm uninstall journal already exists for {}",
                version
            );
        }
        let transaction = Self {
            path,
            journal: GlobalNpmUninstallJournal {
                backend: version.backend.clone(),
                version: version.version.clone(),
                options: version.options.clone(),
                roots,
                bin_names: bin_names.iter().cloned().collect(),
                config_entry: remove_config
                    .then(|| capture_global_npm_uninstall_config_entry(app, version))
                    .transpose()?
                    .flatten(),
                lock_entry: remove_lock
                    .then(|| capture_global_npm_uninstall_lock_entry(app, version))
                    .transpose()?
                    .flatten(),
                committed: false,
            },
            completed: false,
        };
        transaction.persist()?;
        Ok(transaction)
    }

    fn persist(&self) -> Result<()> {
        write_global_npm_uninstall_journal(&self.path, &self.journal)
    }

    pub(crate) fn mark_committed(&mut self) -> Result<()> {
        let mut committed = self.journal.clone();
        committed.committed = true;
        write_global_npm_uninstall_journal(&self.path, &committed)?;
        self.journal = committed;
        Ok(())
    }

    fn rollback_roots(&mut self) -> Result<()> {
        let result = restore_moved_global_npm_roots(&self.journal.roots);
        if result.is_ok() {
            remove_global_npm_uninstall_path(&self.path)?;
            self.completed = true;
        }
        result
    }

    fn finish(&mut self) {
        let mut cleanup_failed = false;
        for root in &self.journal.roots {
            if let Err(error) = remove_global_npm_uninstall_path(&root.backup) {
                cleanup_failed = true;
                tracing::warn!(
                    error = %error,
                    path = %root.backup.display(),
                    "failed to remove committed global npm uninstall backup"
                );
            }
        }
        if !cleanup_failed {
            if let Err(error) = remove_global_npm_uninstall_path(&self.path) {
                tracing::warn!(
                    error = %error,
                    path = %self.path.display(),
                    "failed to remove committed global npm uninstall journal"
                );
            } else {
                self.completed = true;
            }
        }
    }
}

impl Drop for GlobalNpmUninstallTransaction {
    fn drop(&mut self) {
        // A normal error before the commit marker restores availability. A
        // committed transaction is deliberately left for roll-forward unless
        // the caller explicitly rolls it back with its metadata snapshots.
        // Panics model abrupt termination and likewise leave the journal for
        // deterministic recovery on the next locked operation.
        if !self.completed && !self.journal.committed && !std::thread::panicking() {
            let _ = self.rollback_roots();
        }
    }
}

pub(crate) fn global_npm_uninstall_journal_dir(dirs: &osdk_core::dirs::Dirs) -> std::path::PathBuf {
    dirs.data.join(GLOBAL_NPM_UNINSTALL_JOURNAL_DIR)
}

pub(crate) fn global_npm_uninstall_journal_path(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
) -> std::path::PathBuf {
    let backend = osdk_core::pipeline::verify::hash_bytes(
        version.backend.as_bytes(),
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    let version = osdk_core::pipeline::verify::hash_bytes(
        version.version.as_bytes(),
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    global_npm_uninstall_journal_dir(dirs).join(format!("{backend}-{version}.json"))
}

pub(crate) fn capture_global_npm_uninstall_config_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<osdk_core::config::ToolConfigEntry>> {
    Ok(
        osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?
            .global_tool_configs
            .get(&version.backend)
            .cloned(),
    )
}

pub(crate) fn capture_global_npm_uninstall_lock_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<GlobalNpmUninstallLockEntry>> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(None);
    }
    Ok(crate::lockfile::locked_requests(&path, app.ctx.platform)?
        .unwrap_or_default()
        .into_iter()
        .find(|request| {
            request.backend == version.backend
                && request.spec == VersionSpec::Exact(version.version.clone())
                && request
                    .options
                    .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                    .map(String::as_str)
                    == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
        })
        .map(|request| GlobalNpmUninstallLockEntry {
            version: version.version.clone(),
            options: request.options,
        }))
}

pub(crate) fn write_global_npm_uninstall_journal(
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("uninstall journal has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating uninstall journal directory {}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("uninstall journal has no file name: {}", path.display()))?
        .to_string_lossy();
    let temporary = loop {
        let nonce = NEXT_GLOBAL_NPM_UNINSTALL_FILE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.write-{}-{nonce}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "creating global npm uninstall journal {}",
                        candidate.display()
                    )
                })
            }
        }
    };
    let (temporary_path, mut temporary_file) = temporary;
    let result = (|| -> Result<()> {
        use std::io::Write as _;
        temporary_file.write_all(&serde_json::to_vec(journal)?)?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        replace_project_file(&temporary_path, path)?;
        sync_global_npm_uninstall_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result.with_context(|| format!("writing global npm uninstall journal {}", path.display()))
}

#[cfg(unix)]
pub(crate) fn sync_global_npm_uninstall_directory(path: &std::path::Path) -> Result<()> {
    std::fs::File::open(path)?
        .sync_all()
        .with_context(|| format!("syncing uninstall journal directory {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn sync_global_npm_uninstall_directory(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

pub(crate) fn read_global_npm_uninstall_journal(
    path: &std::path::Path,
) -> Result<GlobalNpmUninstallJournal> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading global npm uninstall journal {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing global npm uninstall journal {}", path.display()))
}

/// Complete or roll back global npm uninstalls left by a terminated process.
/// Callers must hold the shared global npm state lock.
pub(crate) fn recover_interrupted_global_npm_uninstalls(app: &App) -> Result<()> {
    let directory = global_npm_uninstall_journal_dir(&app.ctx.dirs);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading global npm uninstall journals {}",
                    directory.display()
                )
            })
        }
    };
    let mut journals = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    journals.sort();
    for path in journals {
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        recover_global_npm_uninstall(app, &path)?;
    }
    Ok(())
}

pub(crate) fn recover_global_npm_uninstall(app: &App, path: &std::path::Path) -> Result<()> {
    let journal = read_global_npm_uninstall_journal(path)?;
    validate_global_npm_uninstall_journal(app, path, &journal)?;
    let mut version = ToolVersion::new(&journal.backend, &journal.version);
    version.options = journal.options.clone();
    let bin_names = journal.bin_names.iter().cloned().collect();
    if journal.committed {
        finish_recovered_global_npm_uninstall(app, path, &journal, &version, &bin_names)
    } else {
        rollback_recovered_global_npm_uninstall(app, path, &journal, &version, &bin_names)
    }
}

pub(crate) fn validate_global_npm_uninstall_journal(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
) -> Result<()> {
    let backend = osdk_core::backend::npm_package::NpmPackageBackend::from_id(&journal.backend)
        .filter(|backend| backend.id() == journal.backend)
        .ok_or_else(|| {
            anyhow!(
                "invalid backend `{}` in global npm uninstall journal {}",
                journal.backend,
                path.display()
            )
        })?;
    if journal.version.is_empty() || journal.roots.is_empty() {
        anyhow::bail!("incomplete global npm uninstall journal {}", path.display());
    }
    let mut version = ToolVersion::new(&journal.backend, &journal.version);
    version.options = journal.options.clone();
    let expected_path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);
    if path != expected_path {
        anyhow::bail!(
            "unsafe global npm uninstall journal path {} (expected {})",
            path.display(),
            expected_path.display()
        );
    }
    let expected_roots = [
        backend.global_install_root_for(&app.ctx, &version)?,
        backend.legacy_global_install_root_path(&app.ctx, &journal.version),
        backend.legacy_isolated_install_root(&app.ctx, &journal.version),
    ];
    let mut originals = std::collections::BTreeSet::new();
    let mut backups = std::collections::BTreeSet::new();
    for root in &journal.roots {
        if !expected_roots.contains(&root.original) || !originals.insert(root.original.clone()) {
            anyhow::bail!(
                "unsafe original path {} in global npm uninstall journal {}",
                root.original.display(),
                path.display()
            );
        }
        let parent = root
            .original
            .parent()
            .ok_or_else(|| anyhow!("global npm uninstall root has no parent"))?;
        let name = root
            .original
            .file_name()
            .ok_or_else(|| anyhow!("global npm uninstall root has no file name"))?
            .to_string_lossy();
        let prefix = format!(".{name}.osdk-uninstall-backup-");
        if root.backup.parent() != Some(parent)
            || !root
                .backup
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            || !backups.insert(root.backup.clone())
        {
            anyhow::bail!(
                "unsafe backup path {} in global npm uninstall journal {}",
                root.backup.display(),
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn finish_recovered_global_npm_uninstall(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let config_matches = match &journal.config_entry {
        Some(expected) => {
            global_npm_uninstall_config_entry(app, version)?.as_ref() == Some(expected)
        }
        None => false,
    };
    if config_matches {
        crate::config_edit::remove_global_tool_unlocked(&app.ctx, &version.backend)?;
    }
    let config_is_safe = match &journal.config_entry {
        Some(expected) => {
            global_npm_uninstall_config_entry(app, version)?.as_ref() != Some(expected)
        }
        None => true,
    };
    let lock_matches = match &journal.lock_entry {
        Some(expected) => global_npm_uninstall_lock_entry(app, version)?.as_ref() == Some(expected),
        None => false,
    };
    if lock_matches {
        crate::lockfile::remove_tool(
            &app.ctx.dirs.user_lock_file(),
            app.ctx.platform,
            &version.backend,
        )?;
    }
    let lock_is_safe = match &journal.lock_entry {
        Some(expected) => global_npm_uninstall_lock_entry(app, version)?.as_ref() != Some(expected),
        None => true,
    };
    if !config_is_safe || !lock_is_safe {
        anyhow::bail!(
            "cannot finish interrupted global npm uninstall for {}: active metadata still matches the removed install",
            version
        );
    }
    remove_unowned_global_npm_shims(app, bin_names)?;
    for root in &journal.roots {
        remove_global_npm_uninstall_path(&root.backup)?;
    }
    remove_global_npm_uninstall_path(path)
}

pub(crate) fn rollback_recovered_global_npm_uninstall(
    app: &App,
    path: &std::path::Path,
    journal: &GlobalNpmUninstallJournal,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    restore_moved_global_npm_roots(&journal.roots)?;
    if global_config_still_selects_version(app, version)? {
        let refreshed = refreshed_global_npm_app(app)?;
        generate_global_npm_shims_for(&refreshed, version, bin_names)?;
    }
    remove_global_npm_uninstall_path(path)
}

pub(crate) fn global_config_still_selects_version(
    app: &App,
    version: &ToolVersion,
) -> Result<bool> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let Some(entry) = config.global_tool_configs.get(&version.backend) else {
        return Ok(false);
    };
    let spec = VersionSpec::parse(&config.expand_alias(&version.backend, entry.version())?);
    match spec {
        VersionSpec::Exact(selected) => Ok(selected == version.version),
        _ => Ok(global_lock_selects_version(app, version)?),
    }
}

pub(crate) fn global_npm_uninstall_config_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<osdk_core::config::ToolConfigEntry>> {
    capture_global_npm_uninstall_config_entry(app, version)
}

pub(crate) fn global_npm_uninstall_lock_entry(
    app: &App,
    version: &ToolVersion,
) -> Result<Option<GlobalNpmUninstallLockEntry>> {
    capture_global_npm_uninstall_lock_entry(app, version)
}

pub(crate) fn refreshed_global_npm_app(app: &App) -> Result<App> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    Ok(App::from_parts(
        ctx,
        osdk_core::Registry::load(&app.ctx.dirs)?,
        app.prompt.clone(),
        app.source_override.clone(),
        app.refresh_sources,
    ))
}

pub(crate) fn generate_global_npm_shims_for(
    app: &App,
    version: &ToolVersion,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if global_config_still_selects_version(app, version)? {
        if let Some(shim_binary) = osdk_core::shim::find_shim_binary(&app.ctx.dirs) {
            for name in bin_names {
                osdk_core::shim::generate_shim(&app.ctx.dirs, name, &shim_binary)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn remove_global_npm_uninstall_path(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

pub(crate) fn rename_global_npm_uninstall_path(
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..4 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
    }
    Err(last.expect("rename was attempted"))
}

pub(crate) fn global_config_selects_version(
    app: &App,
    version: &ToolVersion,
    installed: &[String],
) -> Result<bool> {
    let config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    let Some(entry) = config.global_tool_configs.get(&version.backend) else {
        return Ok(false);
    };
    let spec = VersionSpec::parse(&config.expand_alias(&version.backend, entry.version())?);
    if let VersionSpec::Exact(selected) = spec {
        return Ok(selected == version.version);
    }
    let lock_path = app.ctx.dirs.user_lock_file();
    if lock_path.is_file() {
        if let Some(selected) = crate::lockfile::locked_requests(&lock_path, app.ctx.platform)?
            .unwrap_or_default()
            .into_iter()
            .find(|request| {
                request.backend == version.backend
                    && request
                        .options
                        .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                        .map(String::as_str)
                        == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
            })
        {
            return Ok(selected.spec == VersionSpec::Exact(version.version.clone()));
        }
    }
    Ok(
        select_installed_version(&version.backend, &spec, installed.to_vec())
            .is_ok_and(|selected| selected == version.version),
    )
}

pub(crate) fn global_lock_selects_version(app: &App, version: &ToolVersion) -> Result<bool> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(false);
    }
    Ok(crate::lockfile::locked_requests(&path, app.ctx.platform)?
        .unwrap_or_default()
        .into_iter()
        .any(|request| {
            request.backend == version.backend
                && request.spec == VersionSpec::Exact(version.version.clone())
                && request
                    .options
                    .get(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION)
                    .map(String::as_str)
                    == Some(osdk_core::npm_tools::ToolScope::Global.as_str())
        }))
}

pub(crate) fn remove_unowned_global_npm_shims(
    app: &App,
    bin_names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    let refreshed = App::from_parts(
        ctx,
        osdk_core::Registry::load(&app.ctx.dirs)?,
        app.prompt.clone(),
        app.source_override.clone(),
        app.refresh_sources,
    );
    let owners = installed_shim_owners(&refreshed)?;
    for name in bin_names {
        if !owners
            .get(name)
            .is_some_and(|owner_ids| !owner_ids.is_empty())
        {
            osdk_core::shim::remove_managed_shim(&app.ctx.dirs, name)?;
        }
    }
    Ok(())
}

pub(crate) fn global_npm_metadata_snapshots(
    app: &App,
    _bin_names: &std::collections::BTreeSet<String>,
) -> Result<Vec<GlobalNpmPathSnapshot>> {
    let paths = vec![
        app.ctx.dirs.user_config_file(),
        app.ctx.dirs.user_lock_file(),
    ];
    paths
        .into_iter()
        .map(GlobalNpmPathSnapshot::capture)
        .collect()
}

pub(crate) fn restore_global_npm_metadata(snapshots: &[GlobalNpmPathSnapshot]) -> Result<()> {
    let failures = snapshots
        .iter()
        .rev()
        .filter_map(|snapshot| snapshot.restore().err())
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", failures.join("; "))
    }
}

pub(crate) struct GlobalNpmPathSnapshot {
    path: std::path::PathBuf,
    state: GlobalNpmPathState,
}

pub(crate) enum GlobalNpmPathState {
    Absent,
    File {
        bytes: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    Symlink(std::path::PathBuf),
}

impl GlobalNpmPathSnapshot {
    pub(crate) fn capture(path: std::path::PathBuf) -> Result<Self> {
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                GlobalNpmPathState::Symlink(std::fs::read_link(&path)?)
            }
            Ok(metadata) if metadata.is_file() => GlobalNpmPathState::File {
                bytes: std::fs::read(&path)?,
                permissions: metadata.permissions(),
            },
            Ok(_) => anyhow::bail!("cannot snapshot non-file path {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                GlobalNpmPathState::Absent
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()))
            }
        };
        Ok(Self { path, state })
    }

    pub(crate) fn restore(&self) -> Result<()> {
        match &self.state {
            GlobalNpmPathState::Absent => remove_global_npm_snapshot_path(&self.path),
            GlobalNpmPathState::File { bytes, permissions } => {
                atomic_restore_project_file(&self.path, bytes)?;
                std::fs::set_permissions(&self.path, permissions.clone())
                    .with_context(|| format!("restoring permissions for {}", self.path.display()))
            }
            GlobalNpmPathState::Symlink(target) => {
                remove_global_npm_snapshot_path(&self.path)?;
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &self.path)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(target, &self.path)?;
                Ok(())
            }
        }
    }
}

pub(crate) fn remove_global_npm_snapshot_path(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => anyhow::bail!("refusing to remove non-file path {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

pub(crate) fn restore_moved_global_npm_roots(moved: &[GlobalNpmUninstallRoot]) -> Result<()> {
    let failures = moved
        .iter()
        .rev()
        .filter_map(|root| {
            if !root.backup.exists() {
                return None;
            }
            if root.original.exists() {
                return Some(anyhow!(
                    "refusing to overwrite existing global npm install {} while backup {} also exists",
                    root.original.display(),
                    root.backup.display()
                ));
            }
            rename_global_npm_uninstall_path(&root.backup, &root.original)
                .with_context(|| {
                    format!(
                        "restoring global npm install {}",
                        root.original.display()
                    )
                })
                .err()
        })
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", failures.join("; "))
    }
}

pub(crate) fn unused_sibling_path(
    path: &std::path::Path,
    kind: &str,
) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?
        .to_string_lossy();
    (0..1024u32)
        .map(|attempt| {
            parent.join(format!(
                ".{name}.osdk-{kind}-{}-{attempt}",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| anyhow!("could not allocate transaction path for {}", path.display()))
}

pub(crate) fn combine_rollback_errors(
    first: Option<anyhow::Error>,
    second: Option<anyhow::Error>,
) -> Option<anyhow::Error> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.context(second.to_string())),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

pub(crate) fn with_uninstall_rollback(
    error: anyhow::Error,
    rollback: Option<anyhow::Error>,
) -> anyhow::Error {
    match rollback {
        Some(rollback) => error.context(format!(
            "global npm uninstall rollback failed: {rollback:#}"
        )),
        None => error,
    }
}
