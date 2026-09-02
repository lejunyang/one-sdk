//! Shim generation.
//!
//! A shim is a stand-in for a tool's executable, placed in the shims dir (which
//! the user puts on PATH). Invoking it dispatches to the active version via the
//! `osdk-shim` launcher.
//!
//! - Unix: a symlink from `shims/<name>` to the `osdk-shim` binary. The launcher
//!   inspects argv[0] to learn which tool to run.
//! - Windows: no symlink (privilege). We emit `shims/<name>.cmd` and an
//!   extension-less bash wrapper `shims/<name>` so cmd.exe/PowerShell and
//!   Git-Bash both work, each invoking `osdk-shim.exe`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::backend::{Backend, Ctx};
use crate::dirs::{create_dir_all, Dirs};
use crate::error::{Error, Result};
use crate::inventory::{self, BinOwnerCandidate, DynamicToolManifest, ScanOptions, ScanReport};
use crate::npm_tools::{ToolScope, LOCKED_NPM_SCOPE_OPTION};
use crate::version::ToolVersion;
use crate::version::{ToolRequest, VersionSpec};

/// Executable names that should route through the shim for an installed
/// backend version. These are deliberately separate from backend ownership:
/// Node does not own npm/npx, but its bundled launchers still need routing
/// shims so Node-only activations cannot bypass package-registry preflight.
pub fn routed_bin_names(
    ctx: &Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
) -> Result<Vec<String>> {
    let mut names = backend
        .bin_names(ctx, version)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if backend.id() == "node" {
        for name in crate::backend::bin_names_in_dirs(&backend.bin_paths(ctx, version)?) {
            if matches!(name.as_str(), "npm" | "npx") {
                names.insert(name);
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// The curated owner for an executable name that several tools of one
/// ecosystem ship, or `None` when no rule applies.
///
/// Two Android SDK families legitimately ship the same R8 launchers: the
/// `build-tools` copy is the one a build invokes, while `cmdline-tools` bundles
/// them alongside `sdkmanager`. Without a rule this is an unresolvable
/// conflict, which would refuse every shim of whichever family was installed
/// second -- including `sdkmanager` and `avdmanager`, which nothing else
/// provides.
///
/// Shim generation and shim routing must agree, otherwise the generated shim
/// would dispatch to a different copy than the one it was written for, so both
/// sides call this function.
pub fn precedence_winner<'a>(name: &str, owner_ids: &'a BTreeSet<String>) -> Option<&'a str> {
    const ANDROID_R8_TOOLS: &[&str] = &["d8", "r8", "retrace", "resourceshrinker"];
    const ANDROID_R8_PRECEDENCE: &[&str] = &["android-build-tools", "android-cmdline-tools"];

    // This resolves contention, so a sole owner leaves nothing to decide.
    // Returning a winner there would also imply an opinion about a name no
    // other tool claims.
    if owner_ids.len() < 2 {
        return None;
    }
    if !ANDROID_R8_TOOLS.contains(&name) {
        return None;
    }
    // Only decide when every claimant is one of the known Android families;
    // an unexpected third owner is a real conflict the user must resolve.
    if !owner_ids
        .iter()
        .all(|owner_id| ANDROID_R8_PRECEDENCE.contains(&owner_id.as_str()))
    {
        return None;
    }
    ANDROID_R8_PRECEDENCE.iter().find_map(|preferred| {
        owner_ids
            .iter()
            .find(|owner_id| owner_id.as_str() == *preferred)
            .map(String::as_str)
    })
}

/// Backends whose tools run on a JVM but ship no runtime of their own.
///
/// Android distributes `sdkmanager`, `avdmanager`, `d8` and friends as thin
/// launchers around bundled jars -- 125 of them in `cmdline-tools` alone -- and
/// includes no `java` binary, so they abort before doing anything unless an
/// external JDK is visible. Maven, Gradle and Kotlin are in the same position.
///
/// A backend cannot describe another backend's install, so the JDK has to be
/// supplied by whoever launches the tool.
pub fn requires_external_jdk(backend_id: &str) -> bool {
    matches!(
        backend_id,
        "maven" | "gradle" | "kotlin" | "android-build-tools" | "android-cmdline-tools"
    )
}

/// The JDK environment osdk would activate, for launching a tool whose own
/// backend cannot describe one.
///
/// Prefers the version selected for `cwd`, falling back to the newest install,
/// which is how package-manager shims already locate a managed Node. Returns
/// the JDK's own `exec_env` rather than rebuilding `JAVA_HOME`, so quirks such
/// as macOS' `Contents/Home` layout stay in one place.
pub fn managed_jdk_env(
    ctx: &Ctx,
    registry: &crate::backend::registry::Registry,
    cwd: &Path,
) -> Option<(BTreeMap<String, String>, Vec<std::path::PathBuf>)> {
    let java = registry.get("java").ok()?;
    let installed = java.list_installed(ctx).ok()?;
    if installed.is_empty() {
        return None;
    }
    let selected = crate::version::resolver::resolve_active(
        java.id(),
        cwd,
        &ctx.config.tools,
        java.idiomatic_files(),
    )
    .and_then(|active| installed_matching(&active.spec, active.is_range, &installed))
    .or_else(|| installed.last().cloned())?;
    let version = ToolVersion::new(java.id(), selected);
    let env = java.exec_env(ctx, &version).ok()?;
    let paths = java.bin_paths(ctx, &version).ok()?;
    Some((env, paths))
}

fn installed_matching(spec: &str, is_range: bool, installed: &[String]) -> Option<String> {
    // A java spec may carry a distribution prefix (`temurin-21`) that install
    // directories do not use.
    let spec = spec.split_once('-').map_or(spec, |(left, right)| {
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_alphabetic()) && !right.is_empty() {
            right
        } else {
            spec
        }
    });
    let parsed = if is_range {
        VersionSpec::parse_range(spec).ok()?
    } else {
        VersionSpec::parse(spec)
    };
    match &parsed {
        VersionSpec::Exact(exact) => installed.iter().find(|value| *value == exact).cloned(),
        _ => {
            let infos: Vec<_> = installed
                .iter()
                .map(crate::version::VersionInfo::stable)
                .collect();
            crate::version::select_version(&parsed, &infos).map(|info| info.version.clone())
        }
    }
}

/// Scan all persisted dynamic-tool manifests under the installs tree.
pub fn scan_dynamic_installs(ctx: &Ctx) -> Result<ScanReport> {
    inventory::scan_installs(&ctx.dirs.installs, &ScanOptions::default())
}

/// Dynamic backend ids referenced by the current config. Persisted install
/// manifests intentionally never own aliases or configuration keys.
pub fn configured_dynamic_ids(ctx: &Ctx, _report: &ScanReport) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(ctx.config.tools.iter().filter_map(|(key, value)| {
        inventory::canonical_dynamic_id(key).ok().or_else(|| {
            ToolRequest::parse(value)
                .ok()
                .filter(|request| request.backend.contains(':'))
                .map(|request| request.backend)
        })
    }));

    ids.into_iter().collect()
}

/// Dynamic backend ids relevant to lifecycle operations that survive restarts:
/// configured ids plus anything found on disk through inventory scanning.
pub fn configured_and_installed_dynamic_ids(ctx: &Ctx, report: &ScanReport) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(configured_dynamic_ids(ctx, report));
    ids.extend(report.installed_ids());
    ids.into_iter().collect()
}

/// Resolve a dynamic backend request from the merged config, supporting both a
/// direct dynamic backend key (`"npm:@scope/pkg" = "1.2.3"`) and an indirection
/// key whose value is the dynamic request (`tool.ni = "npm:@scope/pkg@1.2.3"`).
pub fn dynamic_request_from_config(ctx: &Ctx, backend_id: &str) -> Option<ToolRequest> {
    if !backend_id.contains(':') {
        return None;
    }
    for (key, value) in &ctx.config.tools {
        if inventory::canonical_dynamic_id(key).ok().as_deref() == Some(backend_id) {
            return Some(ToolRequest {
                backend: backend_id.to_string(),
                spec: VersionSpec::parse(value),
                options: ctx
                    .config
                    .tool_configs
                    .get(key)
                    .map(|entry| entry.to_request_options())
                    .unwrap_or_default(),
            });
        }
        if let Ok(mut request) = ToolRequest::parse(value) {
            if request.backend == backend_id {
                if let Some(entry) = ctx.config.tool_configs.get(key) {
                    request.options.extend(entry.to_request_options());
                }
                return Some(request);
            }
        }
    }
    None
}

/// One persisted dynamic install whose complete identity has been checked
/// against the active request. Callers keep this value through path and
/// executable selection so security-sensitive inventory data is not reloaded.
#[derive(Debug)]
pub struct ValidatedDynamicInstall {
    install_root: std::path::PathBuf,
    bins: BTreeMap<String, std::path::PathBuf>,
    identity: crate::tool::InstallIdentity,
}

impl ValidatedDynamicInstall {
    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    pub fn bin_names(&self) -> Vec<String> {
        self.bins.keys().cloned().collect()
    }

    pub fn bin_paths(&self) -> Vec<std::path::PathBuf> {
        self.bins
            .values()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn executable(&self, name: &str) -> Option<std::path::PathBuf> {
        self.bins.get(name).cloned()
    }

    pub fn identity(&self) -> &crate::tool::InstallIdentity {
        &self.identity
    }
}

/// Load the selected dynamic install and require it to prove the same backend,
/// exact version, and public option identity as the configured request. Legacy
/// `.osdk-tool.json` and missing inventories are discoverable elsewhere, but
/// cannot authorize PATH exposure or execution.
pub fn validated_dynamic_install(
    ctx: &Ctx,
    report: &ScanReport,
    request: &ToolRequest,
    version: &str,
) -> Result<ValidatedDynamicInstall> {
    let backend = crate::backend::registry::Registry::load(&ctx.dirs)?.get(&request.backend)?;
    let (identity, root) =
        selected_dynamic_install_from_report(ctx, report, backend.as_ref(), request, version)?;
    let locator = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())?;
    if !std::fs::symlink_metadata(root.join(".osdk-complete"))
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has no complete selected install; reinstall it before use",
            request.backend
        )));
    }
    let manifest_path = DynamicToolManifest::manifest_path(&root);
    let install = report
        .installs
        .iter()
        .find(|install| install.install_root == root)
        .ok_or_else(|| {
            Error::other(format!(
                "dynamic tool `{}@{version}` has missing or invalid install identity at {}; reinstall it before use",
                request.backend,
                manifest_path.display()
            ))
        })?;
    install.revalidate()?;
    let manifest = &install.manifest;
    if !manifest.matches_identity(&identity) || !locator.validates_install_root(&root) {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` was installed with a different identity; reinstall it before use",
            request.backend
        )));
    }
    let mut selected = ToolVersion::new(&request.backend, version);
    selected.options = request.options.clone();
    if !backend.validate_dynamic_install(ctx, &selected, &root, &manifest.identity)? {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has invalid provider evidence; reinstall it before use",
            request.backend
        )));
    }
    let canonical_root = dunce::canonicalize(&root).map_err(|error| Error::io(&root, error))?;
    let mut bins = BTreeMap::new();
    for bin in &manifest.bins {
        let path = root.join(&bin.path);
        let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
        if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "dynamic tool `{}` inventory bin `{}` does not resolve to a file inside {}",
                request.backend,
                bin.name,
                root.display()
            )));
        }
        bins.insert(bin.name.clone(), canonical);
    }
    Ok(ValidatedDynamicInstall {
        install_root: root,
        bins,
        identity,
    })
}

/// Deterministic manifest-backed bin ownership used after a process restart,
/// even when the dynamic backend implementation itself does not expose
/// `bin_names` yet.
pub fn dynamic_bin_ownership(report: &ScanReport) -> BTreeMap<String, Vec<BinOwnerCandidate>> {
    inventory::build_bin_ownership_candidates(&report.installs)
}

pub fn selected_dynamic_install_identity(
    ctx: &Ctx,
    backend: &dyn Backend,
    request: &ToolRequest,
    version: &str,
) -> Result<crate::tool::InstallIdentity> {
    let mut tv = ToolVersion::new(&request.backend, version);
    tv.options = request.options.clone();
    backend.dynamic_install_identity(ctx, &tv)?.ok_or_else(|| {
        Error::other(format!(
            "dynamic tool `{}@{version}` requires installed identity discovery",
            request.backend
        ))
    })
}

fn selected_dynamic_install_from_report(
    ctx: &Ctx,
    report: &ScanReport,
    backend: &dyn Backend,
    request: &ToolRequest,
    version: &str,
) -> Result<(crate::tool::InstallIdentity, std::path::PathBuf)> {
    let mut selected = ToolVersion::new(&request.backend, version);
    selected.options = request.options.clone();
    if backend.dynamic_install_identity(ctx, &selected)?.is_none() {
        let scope = crate::tool::InstallScope::Isolated;
        let expected_options = crate::tool::dynamic_identity_options(
            &crate::tool::ToolId::parse(&request.backend)?,
            &request.options,
        )?
        .into_map();
        let mut matching = report.installs.iter().filter(|install| {
            let identity = &install.manifest.identity;
            identity.tool == request.backend
                && identity.version == version
                && identity.platform == ctx.platform.to_string()
                && identity.scope == scope
                && identity.material_options == expected_options
                && std::fs::symlink_metadata(install.install_root.join(".osdk-complete"))
                    .is_ok_and(|metadata| metadata.file_type().is_file())
                && backend
                    .validate_dynamic_install(ctx, &selected, &install.install_root, identity)
                    .unwrap_or(false)
        });
        let first = matching.next();
        if matching.next().is_some() {
            return Err(Error::other(format!(
                "dynamic tool `{}@{version}` has multiple complete installs matching its unlocked request; use a lockfile with exact artifact identity or reinstall it",
                request.backend
            )));
        }
        if let Some(install) = first {
            return Ok((
                install.manifest.identity.clone(),
                install.install_root.clone(),
            ));
        }
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has no complete install matching its unlocked request; reinstall it before use",
            request.backend
        )));
    }
    let identity = selected_dynamic_install_identity(ctx, backend, request, version)?;
    let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())?
        .install_root()
        .to_path_buf();
    Ok((identity, root))
}

pub(crate) fn configured_npm_scope(ctx: &Ctx, request: &ToolRequest) -> Result<Option<ToolScope>> {
    if let Some(scope) = request.options.get(LOCKED_NPM_SCOPE_OPTION) {
        return scope.parse().map(Some);
    }
    let mut project = false;
    let mut global = false;
    for (key, value) in &ctx.config.tools {
        let matches = key == &request.backend
            || ToolRequest::parse(value)
                .is_ok_and(|candidate| candidate.backend == request.backend);
        if !matches {
            continue;
        }
        match ctx.config.tool_origins.get(key) {
            Some(
                crate::config::ToolConfigOrigin::ProjectConfig(_)
                | crate::config::ToolConfigOrigin::ToolVersions(_),
            ) => project = true,
            Some(crate::config::ToolConfigOrigin::GlobalConfig(_)) => global = true,
            None if ctx.config.global_tool_configs.contains_key(key) => global = true,
            None => {}
        }
    }
    Ok(if project {
        Some(ToolScope::Project)
    } else if global {
        Some(ToolScope::Global)
    } else {
        None
    })
}

/// Generate a shim named `name` in the shims dir pointing at `osdk_shim_bin`.
pub fn generate_shim(dirs: &Dirs, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    let shims = dirs.shims();
    create_dir_all(&shims)?;
    generate_shim_in(&shims, name, osdk_shim_bin)
}

#[cfg(unix)]
fn generate_shim_in(shims: &Path, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    create_dir_all(shims)?;
    let link = shims.join(name);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(osdk_shim_bin, &link).map_err(|e| Error::io(&link, e))?;
    Ok(())
}

#[cfg(windows)]
fn generate_shim_in(shims: &Path, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    create_dir_all(shims)?;
    // .cmd wrapper for cmd.exe / PowerShell
    let cmd_path = shims.join(format!("{name}.cmd"));
    let cmd = format!("@echo off\r\n\"{}\" %~n0 %*\r\n", osdk_shim_bin.display());
    std::fs::write(&cmd_path, cmd).map_err(|e| Error::io(&cmd_path, e))?;

    // extension-less bash wrapper for Git-Bash / MSYS
    let sh_path = shims.join(name);
    let sh = format!(
        "#!/bin/sh\nexec \"{}\" \"$(basename \"$0\")\" \"$@\"\n",
        osdk_shim_bin.display().to_string().replace('\\', "/")
    );
    std::fs::write(&sh_path, sh).map_err(|e| Error::io(&sh_path, e))?;
    Ok(())
}

/// Remove a shim by name (all its platform variants).
pub fn remove_shim(dirs: &Dirs, name: &str) -> Result<()> {
    let shims = dirs.shims();
    let _ = std::fs::remove_file(shims.join(name));
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(shims.join(format!("{name}.cmd")));
    }
    Ok(())
}

/// Remove a shim only when it has osdk's generated shape. This lets `reshim`
/// reconcile obsolete routing aliases without deleting unrelated files that a
/// user may have placed in the shims directory.
pub fn remove_managed_shim(dirs: &Dirs, name: &str) -> Result<bool> {
    let shims = dirs.shims();
    let removed = remove_managed_shim_path(&shims.join(name))?;
    #[cfg(windows)]
    {
        let cmd_removed = remove_managed_shim_path(&shims.join(format!("{name}.cmd")))?;
        Ok(removed || cmd_removed)
    }
    #[cfg(not(windows))]
    Ok(removed)
}

#[cfg(unix)]
fn remove_managed_shim_path(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(path, error)),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let target = std::fs::read_link(path).map_err(|error| Error::io(path, error))?;
    if target.file_name().and_then(|name| name.to_str()) != Some("osdk-shim") {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
    Ok(true)
}

#[cfg(windows)]
fn remove_managed_shim_path(path: &Path) -> Result<bool> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(path, error)),
    };
    let lower = contents.to_ascii_lowercase();
    let generated_cmd =
        lower.starts_with("@echo off\r\n\"") && lower.contains("osdk-shim.exe\" %~n0 %*");
    let generated_shell = lower.starts_with("#!/bin/sh\nexec \"")
        && lower.contains("osdk-shim.exe\" \"$(basename \"$0\")\" \"$@\"");
    if !generated_cmd && !generated_shell {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
    Ok(true)
}

/// Locate the installed `osdk-shim` binary. It is expected to sit next to the
/// `osdk` binary (same dir). Falls back to the shims dir.
pub fn find_shim_binary(dirs: &Dirs) -> Option<std::path::PathBuf> {
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let name = format!("osdk-shim{exe_suffix}");
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let candidate = parent.join(&name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let candidate = dirs.data.join("bin").join(&name);
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn jvm_tools_without_a_bundled_runtime_are_flagged() {
        // Android ships these as launchers around jars with no `java` binary,
        // and the JVM build tools are in the same position.
        for id in [
            "android-build-tools",
            "android-cmdline-tools",
            "maven",
            "gradle",
            "kotlin",
        ] {
            assert!(super::requires_external_jdk(id), "{id}");
        }
        // Tools that carry their own runtime, or need none, must not be
        // handed someone else's JDK.
        for id in [
            "java",
            "node",
            "python",
            "android-ndk",
            "android-platform-tools",
        ] {
            assert!(!super::requires_external_jdk(id), "{id}");
        }
    }

    #[test]
    fn managed_jdk_prefers_the_version_selected_for_the_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let registry = crate::backend::registry::Registry::new();
        let java = registry.get("java").unwrap();

        // With nothing installed there is nothing to offer.
        let mut ctx = jdk_test_ctx(temporary.path(), &[]);
        assert!(super::managed_jdk_env(&ctx, &registry, &project).is_none());

        // Two installs present: the newest is the fallback.
        for version in ["17.0.1+9", "21.0.2+13"] {
            let home = ctx.dirs.install_path(java.id(), version);
            std::fs::create_dir_all(home.join("bin")).unwrap();
            std::fs::write(home.join(".osdk-complete"), b"").unwrap();
        }
        let (env, paths) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some(
                ctx.dirs
                    .install_path(java.id(), "21.0.2+13")
                    .to_str()
                    .unwrap()
            )
        );
        assert!(!paths.is_empty());

        // An explicit selection wins over the newest install, so a project
        // pinned to an older JDK builds against that one.
        ctx.config.tools.insert("java".into(), "17.0.1+9".into());
        let (env, _) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some(
                ctx.dirs
                    .install_path(java.id(), "17.0.1+9")
                    .to_str()
                    .unwrap()
            )
        );

        // A distribution-prefixed spec addresses the same install.
        ctx.config
            .tools
            .insert("java".into(), "temurin-17.0.1+9".into());
        let (env, _) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert!(env
            .get("JAVA_HOME")
            .is_some_and(|home| home.ends_with("17.0.1+9")));
    }

    fn jdk_test_ctx(root: &std::path::Path, tools: &[(&str, &str)]) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: tools
                    .iter()
                    .map(|(tool, version)| (tool.to_string(), version.to_string()))
                    .collect(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }
    // Generation and routing must agree on the owner of a shared launcher.
    // If they diverge, the shim dispatches to a copy other than the one it
    // was generated for, which is invisible until a build misbehaves.
    #[test]
    fn shared_android_r8_launchers_resolve_to_build_tools() {
        let both = super::BTreeSet::from([
            "android-cmdline-tools".to_string(),
            "android-build-tools".to_string(),
        ]);
        for name in ["d8", "r8", "retrace", "resourceshrinker"] {
            assert_eq!(
                super::precedence_winner(name, &both),
                Some("android-build-tools"),
                "{name}"
            );
        }
    }

    #[test]
    fn precedence_declines_single_owners_and_unknown_claimants() {
        // Sole owner: nothing to decide, so ordinary handling applies.
        let only_cmdline = super::BTreeSet::from(["android-cmdline-tools".to_string()]);
        assert_eq!(super::precedence_winner("d8", &only_cmdline), None);
        // Names outside the curated set stay real conflicts.
        let both = super::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
        ]);
        assert_eq!(super::precedence_winner("aapt2", &both), None);
        // An unexpected third claimant must not be silently overridden.
        let with_outsider = super::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
            "npm:d8-lookalike".to_string(),
        ]);
        assert_eq!(super::precedence_winner("d8", &with_outsider), None);
    }
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::backend::npm_package::NpmPackageBackend;
    use crate::config::{Config, Settings, SourcesConfig, ToolConfigEntry, ToolConfigOrigin};
    use crate::inventory::DynamicToolBin;
    use crate::platform::Platform;
    use crate::store::Cas;
    use crate::tool::{InstallDependency, InstallDependencyKind, InstallScope};

    fn npm_scope_test_ctx(
        root: &Path,
        origin: Option<ToolConfigOrigin>,
    ) -> (Ctx, NpmPackageBackend) {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        let key = "npm:fixture-cli".to_string();
        let entry = ToolConfigEntry::structured(
            "1.2.3",
            BTreeMap::from([(
                crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
                crate::config::ToolConfigValue::String("1.0.0".into()),
            )]),
        );
        let mut tool_origins = BTreeMap::new();
        let mut global_tool_configs = BTreeMap::new();
        if let Some(origin) = origin {
            if matches!(origin, ToolConfigOrigin::GlobalConfig(_)) {
                global_tool_configs.insert(key.clone(), entry.clone());
            }
            tool_origins.insert(key.clone(), origin);
        }
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: BTreeMap::from([(key.clone(), "1.2.3".into())]),
                tool_configs: BTreeMap::from([(key, entry)]),
                global_tools: Default::default(),
                global_tool_configs,
                tool_origins,
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        (ctx, backend)
    }

    fn dynamic_fixture(
        ctx: &Ctx,
        backend: &NpmPackageBackend,
        scope: ToolScope,
        options: BTreeMap<String, String>,
        bin_name: &str,
    ) -> (PathBuf, DynamicToolManifest) {
        let mut options = options;
        options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let identity = crate::tool::InstallIdentity::new(
            backend.id(),
            "1.2.3",
            ctx.platform.to_string(),
            match scope {
                ToolScope::Project => InstallScope::Isolated,
                ToolScope::Global => InstallScope::Global,
            },
            &options,
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "1.0.0".into(),
                identity: None,
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let mut manifest = DynamicToolManifest::from_identity(identity.clone()).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
        }];
        (root, manifest)
    }

    fn write_dynamic_fixture(
        ctx: &Ctx,
        backend: &NpmPackageBackend,
        scope: ToolScope,
        bin_name: &str,
    ) -> PathBuf {
        let (root, manifest) = dynamic_fixture(ctx, backend, scope, BTreeMap::new(), bin_name);
        let bin = root.join(format!("bin/{bin_name}"));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    #[test]
    fn npm_manifest_lookup_is_scope_strict_and_compatibility_falls_back() {
        let temporary = tempfile::tempdir().unwrap();
        let project_config = temporary.path().join("project/osdk.toml");
        let (project_ctx, backend) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::ProjectConfig(project_config)),
        );
        let global = write_dynamic_fixture(&project_ctx, &backend, ToolScope::Global, "global-bin");
        let isolated =
            write_dynamic_fixture(&project_ctx, &backend, ToolScope::Project, "isolated-bin");
        let global_config = temporary.path().join("config/config.toml");
        let (global_ctx, global_backend) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::GlobalConfig(global_config)),
        );
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        assert_eq!(
            global_backend
                .global_install_root_for(&global_ctx, &version)
                .unwrap(),
            global
        );
        assert_eq!(
            backend
                .isolated_install_root_for(&project_ctx, &version)
                .unwrap(),
            isolated
        );
    }

    #[test]
    fn indirect_dynamic_request_preserves_structured_options() {
        use crate::config::ToolConfigValue;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(td.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(td.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let options = BTreeMap::from([(
            "allow_builds".into(),
            ToolConfigValue::Array(vec!["esbuild".into()]),
        )]);
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: BTreeMap::from([("ni".into(), "npm:@antfu/ni@0.21.12".into())]),
                tool_configs: BTreeMap::from([(
                    "ni".into(),
                    ToolConfigEntry::structured("npm:@antfu/ni@0.21.12", options),
                )]),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };

        let request = dynamic_request_from_config(&ctx, "npm:@antfu/ni").unwrap();
        assert_eq!(request.spec.to_string(), "0.21.12");
        assert_eq!(request.options["allow_builds"], "esbuild");
    }

    #[test]
    fn validated_dynamic_install_requires_schema_one_matching_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let root = backend.isolated_install_root_for(&ctx, &version).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture-cli"), b"fixture").unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();

        let report = scan_dynamic_installs(&ctx).unwrap();
        let missing = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("missing or invalid install identity"),
            "{missing}"
        );

        std::fs::write(
            root.join(crate::inventory::LEGACY_INVENTORY_FILE),
            r#"{"schema":2,"id":"npm:fixture-cli","version":"1.2.3","bins":[{"name":"fixture-cli","path":"bin/fixture-cli"}]}"#,
        )
        .unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let legacy = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            legacy
                .to_string()
                .contains("missing or invalid install identity"),
            "{legacy}"
        );
        assert_eq!(report.legacy_installs.len(), 1);

        let (mismatch_root, mismatched) = dynamic_fixture(
            &ctx,
            &backend,
            ToolScope::Project,
            BTreeMap::from([("installer".into(), "aube".into())]),
            "fixture-cli",
        );
        std::fs::create_dir_all(mismatch_root.join("bin")).unwrap();
        std::fs::write(mismatch_root.join("bin/fixture-cli"), b"fixture").unwrap();
        mismatched.write_atomic(&mismatch_root).unwrap();
        std::fs::write(mismatch_root.join(".osdk-complete"), b"").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let mismatch = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            mismatch
                .to_string()
                .contains("missing or invalid install identity"),
            "{mismatch}"
        );
    }

    #[test]
    fn validated_dynamic_install_requires_completion_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let root = write_dynamic_fixture(&ctx, &backend, ToolScope::Project, "fixture-cli");
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        std::fs::remove_file(root.join(".osdk-complete")).unwrap();
        let incomplete = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(incomplete
            .to_string()
            .contains("no complete selected install"));
        assert!(incomplete.to_string().contains("reinstall"));

        std::fs::create_dir(root.join(".osdk-complete")).unwrap();
        let invalid = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(invalid.to_string().contains("no complete selected install"));
        assert!(invalid.to_string().contains("reinstall"));
    }

    #[cfg(unix)]
    #[test]
    fn validated_dynamic_install_rejects_symlink_completion_marker() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let root = write_dynamic_fixture(&ctx, &backend, ToolScope::Project, "fixture-cli");
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        std::fs::remove_file(root.join(".osdk-complete")).unwrap();
        std::fs::write(root.join("real-complete"), b"").unwrap();
        symlink("real-complete", root.join(".osdk-complete")).unwrap();

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("no complete selected install"));
    }

    fn write_github_fixture(
        ctx: &Ctx,
        request_options: &BTreeMap<String, String>,
        materials: BTreeMap<String, String>,
        contents: &[u8],
    ) -> PathBuf {
        let identity = crate::tool::InstallIdentity::new(
            "github:example/tool",
            "1.2.3",
            ctx.platform.to_string(),
            InstallScope::Isolated,
            request_options,
            Vec::new(),
            materials,
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/tool"), contents).unwrap();
        let mut manifest = DynamicToolManifest::from_identity(identity.clone()).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
        }];
        manifest.write_atomic(&root).unwrap();
        let artifact_file = identity.materials["artifact-file"].clone();
        let checksum = identity
            .materials
            .get("artifact-checksum")
            .map(|value| format!(r#","checksum":{value:?}"#))
            .unwrap_or_default();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            format!(
                r#"{{"url":"https://example.test/tool.tar.gz","file_name":{artifact_file:?}{checksum},"evidence":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    fn write_http_fixture(ctx: &Ctx, receipt_url: &str) -> (ToolRequest, PathBuf) {
        let backend = "http:https://downloads.example.test/tool-{version}";
        let digest = "a".repeat(64);
        let options = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "file".into()),
            ("rename".into(), "fixture".into()),
        ]);
        let identity = crate::tool::InstallIdentity::new(
            backend,
            "1.2.3",
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &options,
            Vec::new(),
            BTreeMap::from([
                ("artifact-file".into(), "tool-1.2.3".into()),
                ("artifact-checksum".into(), format!("sha256:{digest}")),
                ("artifact-url-blake3".into(), {
                    let mut hasher = blake3::Hasher::new_derive_key("osdk-http-artifact-url-v1");
                    hasher.update(receipt_url.as_bytes());
                    hasher.finalize().to_hex().to_string()
                }),
            ]),
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture"), b"fixture").unwrap();
        let mut manifest = DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: "fixture".into(),
            path: "bin/fixture".into(),
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&crate::pipeline::ArtifactReceipt {
                url: receipt_url.into(),
                file_name: "tool-1.2.3".into(),
                checksum: Some(format!("sha256:{digest}")),
                evidence: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        (
            ToolRequest {
                backend: backend.into(),
                spec: VersionSpec::Exact("1.2.3".into()),
                options,
            },
            root,
        )
    }

    #[test]
    fn http_restart_selection_requires_matching_receipt_url_fingerprint() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        let receipt_url = "https://downloads.example.test/tool-1.2.3";
        let (request, root) = write_http_fixture(&ctx, receipt_url);
        let report = scan_dynamic_installs(&ctx).unwrap();
        let selected = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap();
        assert_eq!(selected.install_root(), root);
        assert_eq!(selected.bin_names(), vec!["fixture"]);

        let mut receipt = crate::pipeline::artifact_receipt_at(&root).unwrap();
        receipt.url = "https://downloads.example.test/substitute-1.2.3".into();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("receipt"), "{error}");
    }

    #[test]
    fn unlocked_github_request_recovers_one_complete_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        let materials = BTreeMap::from([
            ("artifact-file".into(), "tool.tar.gz".into()),
            (
                "artifact-checksum".into(),
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
        ]);
        let root = write_github_fixture(&ctx, &BTreeMap::new(), materials, b"one");
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let selected = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap();
        assert_eq!(selected.install_root(), root);
        assert_eq!(
            std::fs::read(selected.executable("tool").unwrap()).unwrap(),
            b"one"
        );
    }

    #[test]
    fn validated_dynamic_install_rejects_manifest_replacement_before_using_cached_bins() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        let materials = BTreeMap::from([
            ("artifact-file".into(), "tool.tar.gz".into()),
            (
                "artifact-checksum".into(),
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
        ]);
        let root = write_github_fixture(&ctx, &BTreeMap::new(), materials, b"one");
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let mut replacement = DynamicToolManifest::load(&root).unwrap();
        replacement.bins.clear();
        replacement.write_atomic(&root).unwrap();
        assert_eq!(report.installs[0].manifest.bins[0].name, "tool");
        assert!(DynamicToolManifest::load(&root).unwrap().bins.is_empty());

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            error.to_string().contains("changed after inventory scan"),
            "{error}"
        );
    }

    #[test]
    fn unlocked_github_request_rejects_ambiguous_complete_identities() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        for (file, digest) in [
            (
                "tool-a.tar.gz",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                "tool-b.tar.gz",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ] {
            write_github_fixture(
                &ctx,
                &BTreeMap::new(),
                BTreeMap::from([
                    ("artifact-file".into(), file.into()),
                    ("artifact-checksum".into(), digest.into()),
                ]),
                file.as_bytes(),
            );
        }
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("multiple complete installs"));
        assert!(error.to_string().contains("lockfile"));
    }

    #[test]
    fn configured_dynamic_ids_include_indirect_requests_without_inventory() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("tool.ni".into(), "npm:@antfu/ni@1.2.3".into())]);
        ctx.config.tool_configs.clear();

        assert_eq!(
            configured_dynamic_ids(&ctx, &ScanReport::default()),
            vec!["npm:@antfu/ni"]
        );
    }

    #[test]
    fn configured_dynamic_ids_and_requests_support_http_templates() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        let backend = "http:https://downloads.example.test/tool-{version}";
        let digest = "a".repeat(64);
        ctx.config.tools = BTreeMap::from([(backend.into(), "1.2.3".into())]);
        ctx.config.tool_configs = BTreeMap::from([(
            backend.into(),
            ToolConfigEntry::structured(
                "1.2.3",
                BTreeMap::from([
                    (
                        "sha256".into(),
                        crate::config::ToolConfigValue::String(digest),
                    ),
                    (
                        "kind".into(),
                        crate::config::ToolConfigValue::String("file".into()),
                    ),
                    (
                        "rename".into(),
                        crate::config::ToolConfigValue::String("fixture".into()),
                    ),
                ]),
            ),
        )]);

        assert_eq!(
            configured_dynamic_ids(&ctx, &ScanReport::default()),
            vec![backend]
        );
        let request = dynamic_request_from_config(&ctx, backend).unwrap();
        assert_eq!(request.backend, backend);
        assert_eq!(request.spec, VersionSpec::Exact("1.2.3".into()));
        assert_eq!(request.options["rename"], "fixture");
    }

    #[cfg(unix)]
    #[test]
    fn unix_shim_is_symlink() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let shims = td.path().join("shims");
        let fake_bin = td.path().join("osdk-shim");
        std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();
        generate_shim_in(&shims, "node", &fake_bin).unwrap();
        let link = shims.join("node");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), fake_bin);
    }

    #[cfg(unix)]
    #[test]
    fn managed_shim_cleanup_removes_generated_links_but_preserves_regular_files() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            _ => None,
        })
        .unwrap();
        let fake_bin = td.path().join("bin/osdk-shim");
        std::fs::create_dir_all(fake_bin.parent().unwrap()).unwrap();
        std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();

        generate_shim(&dirs, "npm", &fake_bin).unwrap();
        assert!(remove_managed_shim(&dirs, "npm").unwrap());
        assert!(!dirs.shims().join("npm").exists());

        std::fs::write(dirs.shims().join("npx"), b"user-owned").unwrap();
        assert!(!remove_managed_shim(&dirs, "npx").unwrap());
        assert_eq!(
            std::fs::read(dirs.shims().join("npx")).unwrap(),
            b"user-owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn node_routing_names_include_bundled_npm_without_changing_backend_ownership() {
        use std::sync::Arc;

        use super::*;
        use crate::backend::node::NodeBackend;
        use crate::config::{Config, Settings, SourcesConfig};
        use crate::platform::Platform;
        use crate::store::Cas;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(td.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(td.path().join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(td.path().join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(td.path().join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let version = ToolVersion::new("node", "20.0.0");
        let bin = NodeBackend.bin_paths(&ctx, &version).unwrap().remove(0);
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["node", "npm", "npx"] {
            let path = bin.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let owned = NodeBackend.bin_names(&ctx, &version).unwrap();
        assert!(owned.contains(&"node".to_string()));
        assert!(!owned.contains(&"npm".to_string()));
        assert!(!owned.contains(&"npx".to_string()));

        let routed = routed_bin_names(&ctx, &NodeBackend, &version).unwrap();
        assert!(routed.contains(&"npm".to_string()));
        assert!(routed.contains(&"npx".to_string()));
    }
}
