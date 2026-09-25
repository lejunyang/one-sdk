//! `runtimes` command handlers (split from commands.rs).

use super::*;

pub fn node(app: &App, command: NodeCommand) -> Result<()> {
    match command {
        NodeCommand::MigratePackages { from, to, apply } => {
            migrate_node_packages(app, &from, &to, apply)
        }
    }
}

pub fn python(app: &App, command: PythonCommand) -> Result<()> {
    match command {
        PythonCommand::Find { request } => find_python(app, request.as_deref()),
    }
}

pub async fn android(app: &App, command: AndroidCommand) -> Result<()> {
    match command {
        AndroidCommand::Licenses { command } => android_licenses(app, command).await,
        AndroidCommand::SdkRoot { command } => android_sdk_root(app, command),
        AndroidCommand::Avd { command } => android_avd(app, command),
    }
}

/// Inspect or rebuild the shared SDK root.
pub(crate) fn android_sdk_root(app: &App, command: AndroidSdkRootCommand) -> Result<()> {
    use osdk_core::backend::android::{AndroidBackend, ID_PREFIX, SUPPORTED_FAMILIES};

    let root = AndroidBackend::sdk_root(&app.ctx);
    match command {
        AndroidSdkRootCommand::Show => {
            println!("sdk root: {}", root.display());
            // Reported first because the emulator rejects the whole root without
            // it, whatever else is installed.
            let marker = root.join("platform-tools");
            println!(
                "valid for the emulator: {}",
                if marker.exists() {
                    "yes"
                } else {
                    "no (platform-tools missing)"
                }
            );
            let mut listed = 0usize;
            for family in SUPPORTED_FAMILIES {
                let backend = AndroidBackend::new(family);
                let installed = backend.list_installed(&app.ctx).unwrap_or_default();
                for version in installed {
                    let Some(relative) = AndroidBackend::sdk_root_relative_path(family, &version)
                    else {
                        continue;
                    };
                    let link = root.join(&relative);
                    let real = app
                        .ctx
                        .dirs
                        .install_path(&format!("{ID_PREFIX}{family}"), &version);
                    // Whether the bridge is in place, and whether the index
                    // Google's tools read is present: both are needed for
                    // avdmanager to see the package at all.
                    let bridged = std::fs::canonicalize(&link)
                        .ok()
                        .zip(std::fs::canonicalize(&real).ok())
                        .map(|(a, b)| a == b)
                        .unwrap_or(false);
                    let indexed = real
                        .join(osdk_core::android::package_xml::PACKAGE_XML)
                        .is_file();
                    println!(
                        "  {:<22} {:<34} bridged={} indexed={}",
                        family,
                        version,
                        if bridged { "yes" } else { "no " },
                        if indexed { "yes" } else { "no" }
                    );
                    listed += 1;
                }
            }
            if listed == 0 {
                println!("  (no Android packages installed)");
            }
            // Links whose package is gone are invisible to the loop above, which
            // only walks what is installed. They matter: a dangling entry still
            // passes the existence checks the emulator and Gradle make, so the
            // root looks healthy and fails deeper in.
            let dangling = AndroidBackend::dangling_sdk_root_links(&app.ctx);
            if !dangling.is_empty() {
                println!("dangling links (target no longer installed):");
                for path in &dangling {
                    println!("  {}", path.display());
                }
                println!("remove them with `osdk android sdk-root repair`");
            }
            Ok(())
        }
        AndroidSdkRootCommand::Repair => {
            let mut written = 0usize;
            let mut linked = 0usize;
            for family in SUPPORTED_FAMILIES {
                let backend = AndroidBackend::new(family);
                for version in backend.list_installed(&app.ctx).unwrap_or_default() {
                    if AndroidBackend::repair_package_index(&app.ctx, family, &version) {
                        written += 1;
                    }
                    if AndroidBackend::link_into_sdk_root(&app.ctx, family, &version).is_ok() {
                        linked += 1;
                    }
                }
            }
            // Relinking only covers packages that are still installed, so it
            // cannot see a link whose package is gone. Sweep the root itself.
            let pruned = AndroidBackend::prune_dangling_sdk_root_links(&app.ctx);
            println!(
                "wrote {written} package index file(s) and checked {linked} SDK root link(s) under {}",
                root.display()
            );
            if written > 0 {
                // Worth saying explicitly: this is the failure the repair fixes.
                println!(
                    "`avdmanager` and `sdkmanager` can now see these packages; \
                     without the index avdmanager reports `Package path is not valid`"
                );
            }
            if pruned.is_empty() {
                println!("no dangling links to remove");
            } else {
                println!(
                    "removed {} dangling link(s) left by an uninstall:",
                    pruned.len()
                );
                for path in &pruned {
                    println!("  {}", path.display());
                }
            }
            Ok(())
        }
    }
}

/// Create, list and delete AVDs without avdmanager.
pub(crate) fn android_avd(app: &App, command: AndroidAvdCommand) -> Result<()> {
    use osdk_core::android::avd::{self, CreateOptions, ImageId};
    use osdk_core::backend::android::{AndroidBackend, SYSTEM_IMAGES_FAMILY};

    match command {
        AndroidAvdCommand::List => {
            let devices = avd::list(&app.ctx.dirs);
            if devices.is_empty() {
                println!("no AVDs under {}", avd::avd_home(&app.ctx.dirs).display());
                return Ok(());
            }
            for device in devices {
                // The image is reported present or missing rather than just
                // echoed: an AVD whose image was uninstalled looks fine here but
                // dies inside the emulator.
                let present = device.image_present();
                println!(
                    "{:<24} {:<12} image={}",
                    device.name,
                    device.target.as_deref().unwrap_or("-"),
                    if present { "ok" } else { "MISSING" }
                );
                println!("  {}", device.path.display());
            }
            Ok(())
        }
        AndroidAvdCommand::Create {
            name,
            image,
            force,
            data_size,
            sdcard_size,
        } => {
            let id = ImageId::parse(&image)?;
            let backend = AndroidBackend::new(SYSTEM_IMAGES_FAMILY);
            let version = id.as_version();
            let installed = backend.list_installed(&app.ctx).unwrap_or_default();
            if !installed.iter().any(|candidate| candidate == &version) {
                return Err(anyhow!(
                    "system image `{version}` is not installed; install it with \
                     `osdk install \"android-system-images@{version}\"`{}",
                    if installed.is_empty() {
                        String::new()
                    } else {
                        format!("\ninstalled images: {}", installed.join(", "))
                    }
                ));
            }
            // Two paths reach the same payload: the versioned install directory,
            // and the bridged one under the SDK root. The emulator expands
            // `%VAR%` in `image.sysdir.1`, and osdk's versioned directory names
            // are percent-encoded, so the bridged path is the only one it can
            // read. Verified: the install path yields `Broken AVD system path`.
            let install_dir = app.ctx.dirs.install_path("android-system-images", &version);
            let bridged = AndroidBackend::sdk_root_relative_path(SYSTEM_IMAGES_FAMILY, &version)
                .map(|relative| AndroidBackend::sdk_root(&app.ctx).join(relative))
                .filter(|path| path.join("system.img").is_file());
            let image_dir = match bridged {
                Some(path) => path,
                None => {
                    // Without the bridge there is no `%`-free path to the image,
                    // so say what to do rather than writing a config that fails
                    // later inside the emulator.
                    return Err(anyhow!(
                        "the system image is installed at {} but is not exposed \
                         under the SDK root, and the emulator cannot read that \
                         path directly; run `osdk android sdk-root repair` first",
                        install_dir.display()
                    ));
                }
            };
            // The display name shown by the emulator, read from the image's own
            // metadata so it matches what Google's tools would show.
            let tag_display = osdk_core::android::package_xml::read_source_properties(&install_dir)
                .and_then(|fields| fields.get("SystemImage.TagDisplay").cloned())
                .unwrap_or_else(|| id.tag.clone());
            let options = CreateOptions {
                force,
                data_size,
                sdcard_size,
            };
            let path = avd::create(
                &app.ctx.dirs,
                &name,
                &id,
                &image_dir,
                &tag_display,
                &options,
            )?;
            println!("created AVD `{name}` at {}", path.display());
            // Both are required to actually boot, and neither is implied by
            // installing the image.
            let root = AndroidBackend::sdk_root(&app.ctx);
            if !root.join("platform-tools").exists() {
                println!(
                    "note: the emulator will reject this SDK root until \
                     platform-tools is installed"
                );
            }
            println!("start it with: emulator -avd {name}");
            Ok(())
        }
        AndroidAvdCommand::Delete { name } => {
            if avd::delete(&app.ctx.dirs, &name)? {
                println!("deleted AVD `{name}`");
            } else {
                println!("no AVD named `{name}`");
            }
            Ok(())
        }
    }
}

pub(crate) async fn android_licenses(app: &App, command: AndroidLicensesCommand) -> Result<()> {
    use osdk_core::android::license;
    use osdk_core::backend::android::AndroidBackend;

    let sdk_root = AndroidBackend::sdk_root(&app.ctx);
    match command {
        AndroidLicensesCommand::Show { tool, digest_only } => {
            let request = ToolRequest::parse(&tool)?;
            if !AndroidBackend::owns_id(&request.backend) {
                return Err(anyhow!(
                    "`{}` is not an Android SDK tool; expected e.g. `android-ndk@29.0.14206865`",
                    request.backend
                ));
            }
            let backend = app.registry.get(&request.backend)?;
            let version = backend.resolve_version(&app.ctx, &request).await?;
            // Reach the concrete backend for its manifest helpers.
            let family = request
                .backend
                .strip_prefix(osdk_core::backend::android::ID_PREFIX)
                .unwrap_or_default();
            let android = AndroidBackend::all()
                .into_iter()
                .find(|candidate| candidate.family() == family)
                .ok_or_else(|| anyhow!("unknown Android family `{family}`"))?;
            let manifest = android.manifest(&app.ctx).await?;
            let package = android.package(&manifest, &version.version)?;
            let Some(license) = manifest.license_for(package) else {
                println!("{} requires no license agreement", package.path);
                return Ok(());
            };
            let recorded = license::is_recorded(&sdk_root, license);
            println!("package:  {}", package.path);
            println!("license:  {}", license.id);
            println!("digest:   {}", license.hash());
            println!("accepted: {}", if recorded { "yes" } else { "no" });
            if !digest_only {
                println!();
                println!("{}", license.text);
            }
            if !recorded {
                println!();
                println!(
                    "to accept: osdk install {tool} -o accept-license={}",
                    license.id
                );
            }
            Ok(())
        }
        AndroidLicensesCommand::Status => {
            let dir = sdk_root.join(license::LICENSES_DIR);
            let mut found = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        found.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
            found.sort();
            if found.is_empty() {
                println!("no Android licenses recorded under {}", dir.display());
                return Ok(());
            }
            println!("recorded under {}:", dir.display());
            for id in found {
                let body = std::fs::read_to_string(dir.join(&id)).unwrap_or_default();
                let digests: Vec<&str> = body
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                println!("  {id}  ({})", digests.join(", "));
            }
            Ok(())
        }
        AndroidLicensesCommand::Export { sdk_root: dest } => {
            let source = sdk_root.join(license::LICENSES_DIR);
            let target = dest.join(license::LICENSES_DIR);
            let entries = std::fs::read_dir(&source).map_err(|error| {
                anyhow!("no recorded licenses at {}: {error}", source.display())
            })?;
            std::fs::create_dir_all(&target)?;
            let mut copied = 0usize;
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let to = target.join(entry.file_name());
                std::fs::copy(&path, &to)?;
                copied += 1;
            }
            println!(
                "exported {copied} license record(s) to {}",
                target.display()
            );
            Ok(())
        }
    }
}

pub async fn model(app: &App, command: ModelCommand) -> Result<()> {
    let store = osdk_core::model::ModelStore::new(
        app.ctx.dirs.clone(),
        app.ctx.cas.clone(),
        app.ctx.config.settings.link_mode,
    );
    match command {
        ModelCommand::Pull {
            name,
            reference,
            endpoint,
            forward_credentials,
            include,
            exclude,
            variant,
            no_lock,
        } => {
            let declaration = applicable_model_declaration(app, &name);
            let reference = reference
                .or_else(|| declaration.map(|entry| entry.source.clone()))
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "model pull {name} needs a reference or an applicable [models.{name}] declaration"
                    )
                })?;
            let reference = osdk_core::model::ModelRef::parse(&reference)?;
            let options = osdk_core::model::pull::PullOptions {
                include: if include.is_empty() {
                    declaration
                        .map(|entry| entry.include.clone())
                        .unwrap_or_default()
                } else {
                    include
                },
                exclude: if exclude.is_empty() {
                    declaration
                        .map(|entry| entry.exclude.clone())
                        .unwrap_or_default()
                } else {
                    exclude
                },
                variant: variant.or_else(|| declaration.and_then(|entry| entry.variant.clone())),
            };
            let endpoint = endpoint
                .or_else(|| declaration.and_then(|entry| entry.endpoint.clone()))
                .or_else(|| provider_endpoint_env(reference.provider));
            let installed = pull_model_from_sources(
                app,
                &name,
                &reference,
                &options,
                endpoint,
                forward_credentials,
            )
            .await?;
            if !no_lock {
                let path = persist_model_pull(app, &name, &installed)?;
                println!("updated {}", path.display());
            }
            println!(
                "{} {}@{} -> {}",
                installed.manifest.name,
                installed.manifest.repository,
                installed.manifest.revision,
                installed.path.display()
            );
        }
        ModelCommand::List => {
            for installed in store.list()? {
                println!(
                    "{}  {}:{}@{}  {}",
                    installed.manifest.name,
                    installed.manifest.provider,
                    installed.manifest.repository,
                    installed.manifest.revision,
                    installed.path.display()
                );
            }
        }
        ModelCommand::Path { name, stable } => {
            let path = if stable {
                store.stable_path(&name)?
            } else {
                store.current(&name)?.path
            };
            println!("{}", path.display());
        }
        ModelCommand::Verify { name } => {
            let manifest = store.verify(&name)?;
            println!(
                "verified {}: {} file(s), revision {}",
                manifest.name,
                manifest.files.len(),
                manifest.revision
            );
        }
        ModelCommand::Sync { prune, dry_run } => model_sync(app, &store, prune, dry_run).await?,
        ModelCommand::Remove { name, keep_lock } => {
            let removed = store.remove(&name)?;
            if removed {
                let models = app.ctx.dirs.models();
                let (pruned, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
                println!(
                    "removed model {name}; pruned {} object(s), {} freed",
                    pruned,
                    human_bytes(bytes)
                );
            } else {
                println!("model {name} is not installed");
            }
            // Deleting the snapshot while the lock still claims it left the two
            // disagreeing, and the next `sync` would faithfully restore exactly
            // what was just removed. Dropped unless the caller asks to keep it,
            // which is the way to remove a snapshot locally without changing what
            // the project declares.
            if !keep_lock {
                let cwd = std::env::current_dir()?;
                let path = project_lock_path(app, &cwd);
                if crate::lockfile::remove_model(&path, &name)? {
                    println!("dropped {name} from {}", path.display());
                }
            }
        }
        ModelCommand::Env { command } => model_env(app, command)?,
        ModelCommand::View { command } => crate::model_view::model_view(app, command)?,
    }
    Ok(())
}

pub(crate) fn applicable_model_declaration<'a>(
    app: &'a App,
    name: &str,
) -> Option<&'a osdk_core::config::ModelDeclaration> {
    app.ctx.config.models.get(name).filter(|entry| {
        entry
            .when
            .as_ref()
            .map(|filter| filter.matches(&app.ctx.platform))
            .unwrap_or(true)
    })
}

pub(crate) async fn pull_model_from_sources(
    app: &App,
    name: &str,
    reference: &osdk_core::model::ModelRef,
    options: &osdk_core::model::pull::PullOptions,
    endpoint: Option<String>,
    forward_credentials: bool,
) -> Result<osdk_core::model::InstalledModel> {
    let sources = if let Some(endpoint) = endpoint {
        let mut source = osdk_core::source::Source::mirror("explicit", &endpoint, i32::MIN);
        source.kind = osdk_core::source::SourceKind::Custom;
        source.forward_credentials =
            forward_credentials || official_model_endpoint(reference.provider, &endpoint);
        vec![source]
    } else {
        let mut sources =
            osdk_core::model::source::ranked_sources(&app.ctx, reference, app.refresh_sources)
                .await?;
        if let Some(id) = app.source_override.as_deref() {
            let index = sources
                .iter()
                .position(|source| source.id == id)
                .ok_or_else(|| {
                    anyhow!(t!("err.unknown_source", id = id, tool = reference.provider))
                })?;
            let selected = sources.remove(index);
            sources.insert(0, selected);
        }
        sources
    };

    let mut last_error = None;
    for source in sources {
        let provider =
            osdk_core::model::source::provider(reference.provider, source.forward_credentials);
        match osdk_core::model::pull::pull(
            &app.ctx,
            provider.as_ref(),
            name,
            reference,
            &source.download_url,
            options,
        )
        .await
        {
            Ok(model) => return Ok(model),
            Err(error) => {
                tracing::warn!(
                    source = %source.id,
                    endpoint = %source.download_url,
                    error = %error,
                    "model source failed, trying next endpoint"
                );
                last_error = Some(error);
            }
        }
    }
    Err(anyhow!(
        "{}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no model source candidates".into())
    ))
}

pub(crate) fn persist_model_pull(
    app: &App,
    name: &str,
    installed: &osdk_core::model::InstalledModel,
) -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    crate::lockfile::merge_model(&path, &installed.manifest)?;
    if let Some(declaration) = applicable_model_declaration(app, name) {
        let views = crate::lockfile::locked_views_from_declaration(&declaration.views);
        crate::lockfile::set_model_views(&path, name, views.clone())?;
        crate::model_view::reconcile_declared_views(app, name, &views)?;
    }
    Ok(path)
}

/// Whether a `[models.<name>]` declaration is already faithfully described by the
/// lock, absent from it, or described with a different identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelLockState {
    /// The lock has an entry whose provider, repository, requested revision, and
    /// variant all match the declaration. Nothing to (re-)pull here; the replay
    /// pass verifies the snapshot and restores it if the bytes are missing.
    UpToDate,
    /// The lock has no entry for this name yet -- the bootstrap case.
    Missing,
    /// The lock has an entry, but the declaration now asks for a different
    /// identity (edited `source` or `variant`), so it must be re-pulled and the
    /// lock entry rewritten.
    Changed,
}

/// Compare a declaration against its lock entry over the fields the lock can
/// hold verbatim.
///
/// `include`/`exclude` are deliberately not compared: they are globs, and the
/// lock stores only their expanded file list, so there is nothing to compare
/// them against without re-resolving the remote. Widening a selection therefore
/// stays a `model pull`, which is the one operation that re-resolves files. A
/// `source` that cannot be parsed is treated as `Changed` so the pull below
/// surfaces the real parse error instead of being silently skipped.
pub(crate) fn model_declaration_lock_state(
    declaration: &osdk_core::config::ModelDeclaration,
    locked: Option<&crate::lockfile::LockedModel>,
) -> ModelLockState {
    let Some(entry) = locked else {
        return ModelLockState::Missing;
    };
    let Ok(reference) = osdk_core::model::ModelRef::parse(&declaration.source) else {
        return ModelLockState::Changed;
    };
    let same = reference.provider == entry.provider
        && reference.repository == entry.repository
        && reference.revision == entry.requested_revision
        && declaration.variant == entry.variant;
    if same {
        ModelLockState::UpToDate
    } else {
        ModelLockState::Changed
    }
}

/// Materialize what the project declares: replay the lock, and pull any
/// `[models]` entry the lock does not yet describe or describes differently.
///
/// `install` deliberately does not reach for models (weights are far too large to
/// fetch as a side effect of installing tools), which is why this is its own
/// verb. Two passes run in order: first every applicable declaration is compared
/// against the lock and (re-)pulled when it is missing or changed, writing the
/// resulting entry back; then every locked entry is replayed.
///
/// A snapshot already present is verified rather than re-fetched: the lock carries
/// each file's SHA-256, so "is this the thing the lock describes" is answerable
/// locally, and re-downloading gigabytes to answer it would be absurd. A snapshot
/// that fails verification is re-pulled, because at that point the local copy is
/// not what was committed.
pub(crate) async fn model_sync(
    app: &App,
    store: &osdk_core::model::ModelStore,
    prune: bool,
    dry_run: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_lock_path(app, &cwd);
    let mut locked = crate::lockfile::locked_models(&path)?;
    let configured: Vec<_> = app
        .ctx
        .config
        .models
        .iter()
        .filter(|(name, _)| applicable_model_declaration(app, name).is_some())
        .map(|(name, declaration)| (name.clone(), declaration.clone()))
        .collect();
    let mut restored = 0usize;
    let mut bootstrapped = std::collections::BTreeSet::new();

    // A declared model is (re-)pulled when the lock does not describe it, or
    // describes a different identity than the declaration now asks for. This is
    // what lets `sync` pick up a `[models]` entry added or edited by hand,
    // rather than only bootstrapping when the whole model lock is empty. The
    // comparison is over what the lock can faithfully hold -- provider,
    // repository, requested revision, and variant -- because `include`/`exclude`
    // are globs the lock stores only as their expanded file list; changing those
    // to widen a selection is still a `model pull`.
    //
    // The list is settled first so the downloads can run concurrently: several
    // models are large and independent, so fetching them one after another wastes
    // the link. `sources.model_jobs` bounds how many run at once (default 2), kept
    // separate from `settings.jobs`, which parallelizes the files within one
    // model -- the two multiply. Writing the lock is deliberately left serial
    // after the downloads join, so concurrent pulls cannot race on `osdk.lock`.
    let mut to_pull = Vec::new();
    for (name, declaration) in &configured {
        if declaration.source.trim().is_empty() {
            anyhow::bail!("[models.{name}].source cannot be empty");
        }
        let locked_entry = locked.iter().find(|(locked_name, _)| locked_name == name);
        let change =
            model_declaration_lock_state(declaration, locked_entry.map(|(_, entry)| entry));
        let reason = match change {
            ModelLockState::UpToDate => continue,
            ModelLockState::Missing => "project declaration",
            ModelLockState::Changed => "declaration changed",
        };
        if dry_run {
            let verb = if matches!(change, ModelLockState::Changed) {
                "would re-lock"
            } else {
                "would pull"
            };
            println!("{verb} {name} ({}) from {reason}", declaration.source);
            // Recorded even in a dry run so the replay pass below does not also
            // report this name (a `Changed` entry still exists in `locked`).
            bootstrapped.insert(name.clone());
            restored += 1;
            continue;
        }
        to_pull.push((name.clone(), declaration.clone(), change));
    }

    if !to_pull.is_empty() {
        let model_jobs = app.ctx.config.sources.model_jobs.max(1);
        // Each future only reads `app` (network + CAS); the lock write is not in
        // here, so these can share `&app` and run concurrently.
        let downloads = to_pull.iter().map(|(name, declaration, _change)| {
            let name = name.clone();
            async move {
                let reference = match osdk_core::model::ModelRef::parse(&declaration.source) {
                    Ok(reference) => reference,
                    Err(error) => return (name, Err(anyhow::Error::from(error))),
                };
                let options = osdk_core::model::pull::PullOptions {
                    include: declaration.include.clone(),
                    exclude: declaration.exclude.clone(),
                    variant: declaration.variant.clone(),
                };
                let endpoint = declaration
                    .endpoint
                    .clone()
                    .or_else(|| provider_endpoint_env(reference.provider));
                let result =
                    pull_model_from_sources(app, &name, &reference, &options, endpoint, false)
                        .await
                        .with_context(|| format!("pulling declared model {name}"));
                (name, result)
            }
        });
        let results: Vec<(String, Result<osdk_core::model::InstalledModel>)> =
            stream::iter(downloads)
                .buffer_unordered(model_jobs)
                .collect()
                .await;

        // Serial tail: persist each successful pull to the lock, report each
        // outcome, and remember the first failure so a partial batch still writes
        // the lock entries it did complete before surfacing the error.
        let mut first_error = None;
        for (name, result) in results {
            let change = to_pull
                .iter()
                .find(|(candidate, _, _)| candidate == &name)
                .map(|(_, _, change)| *change)
                .unwrap_or(ModelLockState::Missing);
            match result {
                Ok(installed) => {
                    let lock_path = persist_model_pull(app, &name, &installed)?;
                    let verb = if matches!(change, ModelLockState::Changed) {
                        "re-locked declared model"
                    } else {
                        "pulled declared model"
                    };
                    println!(
                        "{verb} {name} at revision {} -> {}",
                        installed.manifest.revision,
                        installed.path.display()
                    );
                    println!("updated {}", lock_path.display());
                    bootstrapped.insert(name);
                    restored += 1;
                }
                Err(error) => {
                    eprintln!("failed to pull {name}: {error:#}");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
    }
    if !dry_run && !bootstrapped.is_empty() {
        locked = crate::lockfile::locked_models(&path)?;
    }
    if locked.is_empty() && configured.is_empty() && !prune {
        println!(
            "no models declared in {} or project configuration",
            path.display()
        );
        return Ok(());
    }
    // First decide, serially, which locked models actually need fetching: an
    // intact snapshot is left alone (and only its views reconciled), a dry run
    // just reports. Verifying is local IO, so it stays out of the concurrent
    // section; only the network restores below are parallelized.
    let mut to_restore = Vec::new();
    for (name, entry) in &locked {
        if bootstrapped.contains(name) {
            continue;
        }
        // Verify before deciding, so an intact snapshot is left alone and a
        // corrupted one is not mistaken for a present one.
        let present = store
            .verify(name)
            .map(|manifest| manifest.revision == entry.revision)
            .unwrap_or(false);
        if present {
            println!("{name} is up to date at revision {}", entry.revision);
            // Views are part of what the lock declares; reconciling here makes
            // `model sync` also (re)build them on a machine that never ran
            // `model view add`.
            crate::model_view::reconcile_declared_views(app, name, &entry.views)?;
            continue;
        }
        if dry_run {
            println!(
                "would pull {name} ({}:{}@{})",
                entry.provider, entry.repository, entry.revision
            );
            restored += 1;
            continue;
        }
        to_restore.push((name.clone(), entry));
    }

    if !to_restore.is_empty() {
        let model_jobs = app.ctx.config.sources.model_jobs.max(1);
        // Restores run concurrently up to `model_jobs`; each future only reads
        // `app` and downloads into the CAS. The lock-facing tail (digest check
        // against the lock, view reconcile) is serial below.
        let restores = to_restore.iter().map(|(name, entry)| {
            let name = name.clone();
            async move {
                // The lock pins the immutable revision, so the reference is rebuilt
                // from it rather than from `requested_revision`: replaying a branch
                // name would resolve to whatever it points at now.
                let reference = osdk_core::model::ModelRef {
                    provider: entry.provider,
                    repository: entry.repository.clone(),
                    revision: entry.revision.clone(),
                };
                // Only the files the lock names, so a repository that gained files
                // since the lock was written does not silently grow the snapshot.
                let options = osdk_core::model::pull::PullOptions {
                    include: entry
                        .files
                        .iter()
                        .map(|file| glob_escape(&file.path))
                        .collect(),
                    exclude: Vec::new(),
                    variant: entry.variant.clone(),
                };
                let sources = match osdk_core::model::source::ranked_sources(
                    &app.ctx,
                    &reference,
                    app.refresh_sources,
                )
                .await
                {
                    Ok(sources) => sources,
                    Err(error) => return (name, Err(anyhow::Error::from(error))),
                };
                let mut installed = None;
                let mut last_error = None;
                for source in sources {
                    let provider = osdk_core::model::source::provider(
                        reference.provider,
                        source.forward_credentials,
                    );
                    match osdk_core::model::pull::pull(
                        &app.ctx,
                        provider.as_ref(),
                        &name,
                        &reference,
                        &source.download_url,
                        &options,
                    )
                    .await
                    {
                        Ok(model) => {
                            installed = Some(model);
                            break;
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
                let result = installed.ok_or_else(|| {
                    anyhow!(
                        "cannot restore model {name}: {}",
                        last_error
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "no usable model source".into())
                    )
                });
                (name, result)
            }
        });
        let results: Vec<(String, Result<osdk_core::model::InstalledModel>)> =
            stream::iter(restores)
                .buffer_unordered(model_jobs)
                .collect()
                .await;

        let mut first_error = None;
        for (name, result) in results {
            let entry = to_restore
                .iter()
                .find(|(candidate, _)| candidate == &name)
                .map(|(_, entry)| *entry)
                .expect("restored name is one we queued");
            let installed = match result {
                Ok(installed) => installed,
                Err(error) => {
                    eprintln!("failed to restore {name}: {error:#}");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
            // A restore that produced different bytes is a failure, not a success:
            // the point of the lock is that it pins content.
            if let Err(error) = verify_restored_against_lock(&name, entry, &installed.manifest) {
                eprintln!("failed to restore {name}: {error:#}");
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            crate::model_view::reconcile_declared_views(app, &name, &entry.views)?;
            println!(
                "restored {name} at revision {} -> {}",
                installed.manifest.revision,
                installed.path.display()
            );
            restored += 1;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
    }

    if prune {
        let declared: std::collections::BTreeSet<_> = if locked.is_empty() {
            configured.iter().map(|(name, _)| name.clone()).collect()
        } else {
            locked.iter().map(|(name, _)| name.clone()).collect()
        };
        let mut pruned = 0usize;
        for installed in store.list()? {
            let name = installed.manifest.name.clone();
            if declared.contains(&name) {
                continue;
            }
            if dry_run {
                println!("would remove {name} (not declared in the lock)");
            } else if store.remove(&name)? {
                println!("removed {name} (not declared in the lock)");
            }
            pruned += 1;
        }
        if pruned > 0 && !dry_run {
            let models = app.ctx.dirs.models();
            let (objects, bytes) = app.ctx.cas.gc_roots(&[&app.ctx.dirs.installs, &models])?;
            println!("pruned {} object(s), {} freed", objects, human_bytes(bytes));
        }
    }

    if restored == 0 && !dry_run {
        println!("all declared models are present");
    }
    Ok(())
}

/// Require a restored snapshot to match what the lock committed.
///
/// Checked per file rather than by count alone: an equal number of differing
/// files would otherwise pass.
pub(crate) fn verify_restored_against_lock(
    name: &str,
    entry: &crate::lockfile::LockedModel,
    manifest: &osdk_core::model::SnapshotManifest,
) -> Result<()> {
    if manifest.revision != entry.revision {
        anyhow::bail!(
            "model {name} restored revision {} but the lock declares {}",
            manifest.revision,
            entry.revision
        );
    }
    for locked in &entry.files {
        let actual = manifest
            .files
            .iter()
            .find(|file| file.path == locked.path)
            .ok_or_else(|| {
                anyhow!(
                    "model {name} is missing locked file {} after restore",
                    locked.path
                )
            })?;
        if actual.size != locked.size {
            anyhow::bail!(
                "model {name} file {} has size {} but the lock declares {}",
                locked.path,
                actual.size,
                locked.size
            );
        }
        match actual.sha256.as_deref() {
            Some(digest) if digest.eq_ignore_ascii_case(&locked.sha256) => {}
            Some(digest) => anyhow::bail!(
                "model {name} file {} restored digest {digest} but the lock declares {}",
                locked.path,
                locked.sha256
            ),
            None => anyhow::bail!(
                "model {name} file {} was restored without a digest to compare",
                locked.path
            ),
        }
    }
    Ok(())
}

/// Quote a literal path so it survives being used as an include glob.
///
/// The lock stores exact paths, and a file legitimately containing `[`, `{`, `*`
/// or `?` would otherwise be read as a pattern and silently fail to match itself.
pub(crate) fn glob_escape(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            escaped.push('[');
            escaped.push(character);
            escaped.push(']');
        } else {
            escaped.push(character);
        }
    }
    escaped
}

pub(crate) fn model_env(app: &App, command: ModelEnvCommand) -> Result<()> {
    match command {
        ModelEnvCommand::Enable { provider, force } => {
            for provider in selected_providers(provider) {
                crate::config_edit::set_model_env(&app.ctx, provider, true, force)?;
                println!(
                    "{}",
                    t!(
                        "msg.model_env_enabled",
                        provider = provider,
                        mode = if force {
                            t!("label.force")
                        } else {
                            String::new()
                        }
                    )
                );
            }
            println!("{}", t!("msg.model_env_refresh"));
        }
        ModelEnvCommand::Disable { provider } => {
            for provider in selected_providers(provider) {
                crate::config_edit::set_model_env(&app.ctx, provider, false, false)?;
                println!("{}", t!("msg.model_env_disabled", provider = provider));
            }
            println!("{}", t!("msg.model_env_refresh"));
        }
        ModelEnvCommand::List => {
            for provider in [
                osdk_core::model::ProviderId::HuggingFace,
                osdk_core::model::ProviderId::ModelScope,
            ] {
                let config = app.ctx.config.tool_sources(provider.as_str());
                let enabled = config.is_some_and(|config| config.env);
                let force = config.is_some_and(|config| config.env_force);
                println!(
                    "{}: {}{}",
                    provider,
                    if enabled {
                        t!("label.enabled")
                    } else {
                        t!("label.disabled")
                    },
                    if force {
                        t!("label.force")
                    } else {
                        String::new()
                    }
                );
            }
            let environment =
                osdk_core::model::env::configured_env(&app.ctx, |key| std::env::var(key).ok());
            for (key, value) in environment {
                let display = if key.contains("TOKEN") && value.is_empty() {
                    "<disabled>"
                } else {
                    &value
                };
                println!("  {key}={display}");
            }
        }
    }
    Ok(())
}

pub(crate) fn selected_providers(
    provider: Option<osdk_core::model::ProviderId>,
) -> Vec<osdk_core::model::ProviderId> {
    provider.map_or_else(
        || {
            vec![
                osdk_core::model::ProviderId::HuggingFace,
                osdk_core::model::ProviderId::ModelScope,
            ]
        },
        |provider| vec![provider],
    )
}

pub(crate) fn model_reference(
    provider: osdk_core::model::ProviderId,
    value: &str,
) -> Result<osdk_core::model::ModelRef> {
    if value.contains(':') {
        let reference = osdk_core::model::ModelRef::parse(value)?;
        if reference.provider != provider {
            return Err(anyhow!(
                "model reference provider {} does not match {}",
                reference.provider,
                provider
            ));
        }
        Ok(reference)
    } else {
        osdk_core::model::ModelRef::parse(&format!("{provider}:{value}")).map_err(Into::into)
    }
}

pub(crate) fn provider_endpoint_env(provider: osdk_core::model::ProviderId) -> Option<String> {
    match provider {
        osdk_core::model::ProviderId::HuggingFace => std::env::var("HF_ENDPOINT").ok(),
        osdk_core::model::ProviderId::ModelScope => std::env::var("MODELSCOPE_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("MODELSCOPE_DOMAIN").ok()),
    }
}

pub(crate) fn official_model_endpoint(
    provider: osdk_core::model::ProviderId,
    endpoint: &str,
) -> bool {
    let endpoint = endpoint.trim().trim_end_matches('/').to_ascii_lowercase();
    match provider {
        osdk_core::model::ProviderId::HuggingFace => endpoint == "https://huggingface.co",
        osdk_core::model::ProviderId::ModelScope => {
            matches!(
                endpoint.as_str(),
                "https://modelscope.cn" | "https://www.modelscope.ai"
            )
        }
    }
}

pub async fn rust(app: &mut App, command: RustCommand) -> Result<()> {
    match command {
        RustCommand::Component { command } => rust_item(app, "component", command).await,
        RustCommand::Target { command } => rust_item(app, "target", command).await,
        RustCommand::Check { repair } => rust_check(app, repair),
        RustCommand::Override { command } => rust_override(app, command),
        RustCommand::Toolchain { command } => rust_toolchain(app, command),
    }
}

/// The source `rust` operations should drive, honoring `--source` and the
/// configured pin. Adding a component or target downloads from the dist server,
/// so it must use the same selection the install path uses instead of whatever
/// the ambient environment happens to hold.
pub(crate) async fn selected_rust_source(
    app: &mut App,
) -> Result<Option<osdk_core::source::Source>> {
    apply_source_override(app, "rust");
    let backend = app.registry.get("rust")?;
    match osdk_core::source::select::active_source(&app.ctx, backend.as_ref()).await {
        Ok(source) => Ok(Some(source)),
        // Selection needs no network when a pin resolves, but probing can fail
        // offline. rustup still has its own default host, so a failed selection
        // must not block a local operation.
        Err(error) => {
            tracing::debug!(%error, "falling back to rustup's default dist server");
            Ok(None)
        }
    }
}

pub(crate) async fn rust_item(app: &mut App, kind: &str, command: RustItemCommand) -> Result<()> {
    let (operation, name, toolchain) = match command {
        RustItemCommand::Add { name, toolchain } => ("add", Some(name), toolchain),
        RustItemCommand::Remove { name, toolchain } => ("remove", Some(name), toolchain),
        RustItemCommand::List { toolchain } => ("list", None, toolchain),
    };
    let mut args = vec![kind, operation];
    if let Some(name) = name.as_deref() {
        args.push(name);
    }
    args.extend(["--toolchain", &toolchain]);
    // Only `add` downloads; the others are local and must not pay for a probe.
    let source = if operation == "add" {
        selected_rust_source(app).await?
    } else {
        None
    };
    if let Some(source) = &source {
        tracing::info!(
            source = %source.id,
            dist = %source.download_url,
            "{}",
            osdk_core::i18n::tr("log.rustup_dist_server")
        );
    }
    let output =
        osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &args, None, source.as_ref())?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

pub(crate) fn rust_check(app: &App, repair: bool) -> Result<()> {
    let output =
        osdk_core::backend::rust::RustBackend::run_rustup(&app.ctx, &["check"], None, None)?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if repair {
        let (created, removed) =
            osdk_core::backend::rust::RustBackend::reconcile_markers(&app.ctx)?;
        println!(
            "{}",
            t!(
                "msg.rust_markers_repaired",
                created = created,
                removed = removed
            )
        );
    }
    Ok(())
}

pub(crate) fn rust_override(app: &App, command: RustOverrideCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match command {
        RustOverrideCommand::Import { path } => {
            let directory = path.unwrap_or(cwd);
            let output = osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["override", "list"],
                None,
                None,
            )?;
            let text = String::from_utf8_lossy(&output.stdout);
            let canonical = dunce_path(&directory)?;
            let toolchain = parse_rustup_override(&text, &canonical).ok_or_else(|| {
                anyhow!(
                    "no isolated rustup override found for {}",
                    canonical.display()
                )
            })?;
            let path = crate::config_edit::set_project_tool("rust", &toolchain)?;
            println!(
                "{}",
                t!(
                    "msg.rust_override_imported",
                    toolchain = toolchain,
                    path = path.display()
                )
            );
        }
        RustOverrideCommand::Export { path } => {
            let directory = path.unwrap_or(cwd);
            let active = osdk_core::version::resolver::resolve_active(
                "rust",
                &directory,
                &app.ctx.config.tools,
                &["rust-toolchain.toml", "rust-toolchain"],
            )
            .ok_or_else(|| anyhow!("no active osdk Rust version for {}", directory.display()))?;
            let canonical = dunce_path(&directory)?;
            let path_arg = canonical.display().to_string();
            osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["override", "set", &active.spec, "--path", &path_arg],
                None,
                None,
            )?;
            println!(
                "{}",
                t!(
                    "msg.rust_override_exported",
                    toolchain = active.spec,
                    path = canonical.display()
                )
            );
        }
    }
    Ok(())
}

pub(crate) fn rust_toolchain(app: &App, command: RustToolchainCommand) -> Result<()> {
    match command {
        RustToolchainCommand::Link { name, path } => {
            validate_rust_link_name(&name)?;
            let canonical = dunce_path(&path)?;
            if !canonical.join("bin").is_dir() {
                return Err(anyhow!(
                    "linked Rust toolchain must contain bin/: {}",
                    canonical.display()
                ));
            }
            let path_arg = canonical.display().to_string();
            osdk_core::backend::rust::RustBackend::run_rustup(
                &app.ctx,
                &["toolchain", "link", &name, &path_arg],
                None,
                None,
            )?;
            osdk_core::backend::rust::RustBackend::record_linked_toolchain(
                &app.ctx, &name, &canonical,
            )?;
            println!(
                "{}",
                t!(
                    "msg.rust_toolchain_linked",
                    name = name,
                    path = canonical.display()
                )
            );
        }
    }
    Ok(())
}

pub(crate) fn dunce_path(path: &std::path::Path) -> Result<std::path::PathBuf> {
    dunce::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))
}

pub(crate) fn parse_rustup_override(text: &str, path: &std::path::Path) -> Option<String> {
    text.lines().find_map(|line| {
        let (directory, toolchain) = line.rsplit_once(char::is_whitespace)?;
        (dunce::canonicalize(directory).ok().as_deref() == Some(path))
            .then(|| toolchain.trim().to_string())
    })
}

pub(crate) fn validate_rust_link_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
        || name.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("invalid linked Rust toolchain name `{name}`"));
    }
    Ok(())
}

pub(crate) fn find_python(app: &App, request: Option<&str>) -> Result<()> {
    let backend = app.registry.get("python")?;
    let installed = backend.list_installed(&app.ctx)?;
    let selected = request
        .map(|request| osdk_core::backend::python::select_installed(request, &installed))
        .unwrap_or(None);
    let mut seen = std::collections::BTreeSet::new();
    let mut found = false;

    for identity in installed {
        if selected
            .as_deref()
            .is_some_and(|selected| selected != identity)
        {
            continue;
        }
        let version = ToolVersion::new("python", &identity);
        for directory in backend.bin_paths(&app.ctx, &version)? {
            for name in python_executable_names() {
                let path = directory.join(name);
                if path.is_file() && seen.insert(canonical_or_original(&path)) {
                    println!("managed\t{}\t{}", identity, path.display());
                    found = true;
                }
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in python_executable_names() {
                let candidate = directory.join(name);
                if candidate.is_file() && seen.insert(canonical_or_original(&candidate)) {
                    println!("path\t-\t{}", candidate.display());
                    found = true;
                }
            }
        }
    }

    for candidate in system_python_candidates() {
        if candidate.is_file() && seen.insert(canonical_or_original(&candidate)) {
            println!("system\t-\t{}", candidate.display());
            found = true;
        }
    }
    if !found {
        return Err(anyhow!(t!("err.python_not_found")));
    }
    Ok(())
}

pub(crate) fn python_executable_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "python.exe",
            "python3.exe",
            "pypy.exe",
            "pypy3.exe",
            "graalpy.exe",
        ]
    }
    #[cfg(not(windows))]
    {
        &["python", "python3", "pypy", "pypy3", "graalpy"]
    }
}

pub(crate) fn canonical_or_original(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn system_python_candidates() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Some(directory) = std::env::var_os("SystemRoot") {
            candidates.push(std::path::PathBuf::from(directory).join("py.exe"));
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![
            "/usr/bin/python3".into(),
            "/usr/local/bin/python3".into(),
            "/opt/homebrew/bin/python3".into(),
        ]
    }
}

pub(crate) fn migrate_node_packages(app: &App, from: &str, to: &str, apply: bool) -> Result<()> {
    let source = managed_node_tools(app, from)?;
    let target = managed_node_tools(app, to)?;
    let source_packages = list_global_npm_packages(&source)?;
    let target_packages = list_global_npm_packages(&target)?;
    let target_names: std::collections::BTreeSet<_> = target_packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let mut planned = Vec::new();
    for package in source_packages {
        if package.name == "npm" {
            println!("{}", t!("msg.node_migrate_skip_npm"));
            continue;
        }
        if package.native {
            println!(
                "{}",
                t!("msg.node_migrate_skip_native", package = package.spec())
            );
            continue;
        }
        if !target_names.contains(package.name.as_str()) {
            planned.push(package);
        }
    }

    if planned.is_empty() {
        println!("{}", t!("msg.node_migrate_nothing"));
        return Ok(());
    }
    for package in &planned {
        println!("{}", t!("msg.node_migrate_plan", package = package.spec()));
    }
    if !apply {
        println!("{}", t!("msg.node_migrate_dry_run"));
        return Ok(());
    }

    let before = target_packages;
    let specs: Vec<String> = planned.iter().map(NpmPackage::spec).collect();
    if let Err(error) = npm_install_global(&target, &specs) {
        return match restore_global_npm_packages(&target, &before) {
            Ok(()) => Err(error.context(t!("err.node_migrate_rolled_back"))),
            Err(rollback) => Err(error.context(format!(
                "{}: {rollback:#}",
                t!("err.node_migrate_rollback_failed")
            ))),
        };
    }
    println!(
        "{}",
        t!(
            "msg.node_migrate_applied",
            count = planned.len(),
            version = to
        )
    );
    Ok(())
}

#[derive(Debug)]
pub(crate) struct ManagedNodeTools {
    bin: std::path::PathBuf,
    npm: std::path::PathBuf,
}

pub(crate) fn managed_node_tools(app: &App, version: &str) -> Result<ManagedNodeTools> {
    let install = app.ctx.dirs.install_path("node", version);
    if !osdk_core::pipeline::is_installed(&app.ctx.dirs, "node", version) {
        return Err(anyhow!(t!(
            "err.not_installed",
            tool = "node",
            ver = version
        )));
    }
    let bin = match app.ctx.platform.os {
        osdk_core::platform::Os::Windows => install,
        _ => install.join("bin"),
    };
    let npm = if matches!(app.ctx.platform.os, osdk_core::platform::Os::Windows) {
        bin.join("npm.cmd")
    } else {
        bin.join("npm")
    };
    if !npm.is_file() {
        return Err(anyhow!(
            "managed npm executable not found at {}",
            npm.display()
        ));
    }
    Ok(ManagedNodeTools { bin, npm })
}

#[derive(Debug, Clone)]
pub(crate) struct NpmPackage {
    name: String,
    version: String,
    native: bool,
}

impl NpmPackage {
    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

pub(crate) fn list_global_npm_packages(tools: &ManagedNodeTools) -> Result<Vec<NpmPackage>> {
    let output = npm_command(tools, &["ls", "-g", "--depth=0", "--json", "--long"])?;
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing managed npm package list")?;
    let mut packages = Vec::new();
    if let Some(dependencies) = value
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
    {
        for (name, metadata) in dependencies {
            let Some(version) = metadata.get("version").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let native = metadata
                .get("hasInstallScript")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || metadata
                    .get("gypfile")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            packages.push(NpmPackage {
                name: name.clone(),
                version: version.to_string(),
                native,
            });
        }
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

pub(crate) fn npm_install_global(tools: &ManagedNodeTools, specs: &[String]) -> Result<()> {
    let mut args = vec!["install", "-g"];
    args.extend(specs.iter().map(String::as_str));
    npm_command(tools, &args).map(|_| ())
}

pub(crate) fn restore_global_npm_packages(
    tools: &ManagedNodeTools,
    packages: &[NpmPackage],
) -> Result<()> {
    let current = list_global_npm_packages(tools)?;
    let removable: Vec<String> = current
        .iter()
        .filter(|package| package.name != "npm")
        .map(|package| package.name.clone())
        .collect();
    if !removable.is_empty() {
        let mut args = vec!["uninstall", "-g"];
        args.extend(removable.iter().map(String::as_str));
        npm_command(tools, &args)?;
    }
    let desired: Vec<String> = packages
        .iter()
        .filter(|package| package.name != "npm")
        .map(NpmPackage::spec)
        .collect();
    if !desired.is_empty() {
        npm_install_global(tools, &desired)?;
    }
    Ok(())
}

pub(crate) fn npm_command(tools: &ManagedNodeTools, args: &[&str]) -> Result<std::process::Output> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![tools.bin.clone()];
    paths.extend(std::env::split_paths(&inherited));
    let path = std::env::join_paths(paths)?;
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/D", "/S", "/C"]).arg(&tools.npm);
        command
    } else {
        std::process::Command::new(&tools.npm)
    };
    let output = command
        .args(args)
        .env("PATH", path)
        .output()
        .with_context(|| format!("running managed npm at {}", tools.npm.display()))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(anyhow!(
            "managed npm {} failed with {}:\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
