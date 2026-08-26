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

use crate::backend::npm_package::NpmPackageBackend;
use crate::backend::{Backend, Ctx};
use crate::dirs::{create_dir_all, Dirs};
use crate::error::{Error, Result};
use crate::inventory::{self, BinOwnerCandidate, DynamicToolManifest, ScanOptions, ScanReport};
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

/// Scan all persisted dynamic-tool manifests under the installs tree.
pub fn scan_dynamic_installs(ctx: &Ctx) -> Result<ScanReport> {
    inventory::scan_installs(&ctx.dirs.installs, &ScanOptions::default())
}

/// Dynamic backend ids referenced by the current config, plus any ids that an
/// installed manifest says can be addressed through one of its recorded
/// `config_keys`.
pub fn configured_dynamic_ids(ctx: &Ctx, report: &ScanReport) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(
        ctx.config
            .tools
            .keys()
            .filter_map(|key| inventory::canonical_dynamic_id(key).ok()),
    );

    let configured_values = ctx
        .config
        .tools
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let config_keys = report
        .installs
        .iter()
        .flat_map(|install| install.manifest.config_keys.iter().map(String::as_str))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !config_keys.is_empty() {
        ids.extend(inventory::configured_dynamic_ids(
            configured_values.iter().copied(),
            &config_keys,
        ));
    }
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

/// Deterministic manifest-backed bin ownership used after a process restart,
/// even when the dynamic backend implementation itself does not expose
/// `bin_names` yet.
pub fn dynamic_bin_ownership(report: &ScanReport) -> BTreeMap<String, Vec<BinOwnerCandidate>> {
    inventory::build_bin_ownership_candidates(&report.installs)
}

/// Load the dynamic manifest recorded for one installed backend version.
pub fn dynamic_manifest_for_version(
    ctx: &Ctx,
    backend_id: &str,
    version: &str,
) -> Result<Option<DynamicToolManifest>> {
    if !backend_id.contains(':') {
        return Ok(None);
    }
    for install_root in dynamic_install_roots(ctx, backend_id, version)? {
        let manifest_path = DynamicToolManifest::manifest_path(&install_root);
        if manifest_path.is_file() {
            return Ok(Some(DynamicToolManifest::load(&install_root)?));
        }
    }
    Ok(None)
}

/// Bin names exported by a manifest-backed dynamic install.
pub fn dynamic_manifest_bin_names(
    ctx: &Ctx,
    backend_id: &str,
    version: &str,
) -> Result<Vec<String>> {
    let Some(manifest) = dynamic_manifest_for_version(ctx, backend_id, version)? else {
        return Ok(Vec::new());
    };
    Ok(manifest
        .bins
        .into_iter()
        .map(|bin| bin.name)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// PATH directories contributed by a manifest-backed dynamic install.
pub fn dynamic_manifest_bin_paths(
    ctx: &Ctx,
    backend_id: &str,
    version: &str,
) -> Result<Vec<std::path::PathBuf>> {
    for install_root in dynamic_install_roots(ctx, backend_id, version)? {
        let manifest_path = DynamicToolManifest::manifest_path(&install_root);
        if manifest_path.is_file() {
            let manifest = DynamicToolManifest::load(&install_root)?;
            return Ok(manifest_bin_paths(&install_root, &manifest));
        }
    }
    Ok(Vec::new())
}

/// Resolve one executable path directly from the manifest instead of relying on
/// backend `bin_paths`.
pub fn dynamic_manifest_executable(
    ctx: &Ctx,
    backend_id: &str,
    version: &str,
    executable_name: &str,
) -> Result<Option<std::path::PathBuf>> {
    for install_root in dynamic_install_roots(ctx, backend_id, version)? {
        let manifest_path = DynamicToolManifest::manifest_path(&install_root);
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = DynamicToolManifest::load(&install_root)?;
        return Ok(manifest
            .bins
            .into_iter()
            .find(|bin| bin.name == executable_name)
            .map(|bin| install_root.join(bin.path)));
    }
    Ok(None)
}

fn dynamic_install_roots(
    ctx: &Ctx,
    backend_id: &str,
    version: &str,
) -> Result<Vec<std::path::PathBuf>> {
    if let Some(backend) = NpmPackageBackend::from_id(backend_id) {
        let tv = ToolVersion::new(backend_id, version);
        return Ok(backend
            .selected_install_root(ctx, &tv)?
            .into_iter()
            .collect());
    }
    Ok(vec![ctx.dirs.install_path(backend_id, version)])
}

fn manifest_bin_paths(
    install_root: &std::path::Path,
    manifest: &DynamicToolManifest,
) -> Vec<std::path::PathBuf> {
    manifest
        .bins
        .iter()
        .filter_map(|bin| {
            install_root
                .join(&bin.path)
                .parent()
                .map(|path| path.to_path_buf())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig, ToolConfigEntry, ToolConfigOrigin};
    use crate::inventory::DynamicToolBin;
    use crate::platform::Platform;
    use crate::store::Cas;

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
        let entry = ToolConfigEntry::legacy("1.2.3");
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

    fn write_dynamic_fixture(root: &Path, bin_name: &str, scope: Option<&str>) {
        let bin = root.join(format!("bin/{bin_name}"));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        let mut manifest = DynamicToolManifest::new("npm:fixture-cli").unwrap();
        manifest.version = Some("1.2.3".into());
        manifest.bins = vec![DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
        }];
        if let Some(scope) = scope {
            manifest.metadata.insert("scope".into(), scope.into());
        }
        manifest.write_atomic(root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    #[test]
    fn npm_manifest_lookup_is_scope_strict_and_compatibility_falls_back() {
        let temporary = tempfile::tempdir().unwrap();
        let project_config = temporary.path().join("project/osdk.toml");
        let (project_ctx, backend) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::ProjectConfig(project_config)),
        );
        let isolated = backend.isolated_install_root(&project_ctx, "1.2.3");
        let global = backend.global_install_root(&project_ctx, "1.2.3");
        write_dynamic_fixture(&global, "global-bin", Some("global"));
        assert!(
            dynamic_manifest_for_version(&project_ctx, backend.id(), "1.2.3")
                .unwrap()
                .is_none()
        );

        write_dynamic_fixture(&isolated, "isolated-bin", None);
        let global_config = temporary.path().join("config/config.toml");
        let (global_ctx, _) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::GlobalConfig(global_config)),
        );
        let manifest = dynamic_manifest_for_version(&global_ctx, backend.id(), "1.2.3")
            .unwrap()
            .unwrap();
        assert_eq!(manifest.bins[0].name, "global-bin");

        std::fs::remove_dir_all(&global).unwrap();
        assert!(
            dynamic_manifest_for_version(&global_ctx, backend.id(), "1.2.3")
                .unwrap()
                .is_none()
        );
        write_dynamic_fixture(&global, "global-bin", Some("global"));

        let (compat_ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        let manifest = dynamic_manifest_for_version(&compat_ctx, backend.id(), "1.2.3")
            .unwrap()
            .unwrap();
        assert_eq!(manifest.bins[0].name, "isolated-bin");
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
