//! Go command tools installed with an exact osdk-managed Go runtime.
//!
//! The dynamic `go:` namespace addresses a module or nested command path.
//! Version discovery uses bounded Go proxy metadata, while installation invokes
//! the selected managed `go` binary once in a cleared, osdk-controlled
//! environment and publishes only lifecycle-validated staged binaries.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::backend::native_tool::{
    self, NativeToolFamily, NativeToolLifecycle, NativeToolPreparation, NativeToolProvider,
    LOCKED_NATIVE_REPLAY_OPTION, LOCKED_NATIVE_RUNTIME_OPTION,
    LOCKED_NATIVE_RUNTIME_VERSION_OPTION,
};
use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::process::{
    CaptureLimits, CommandOutcome, CommandRunner, CommandSpec, SystemCommandRunner,
};
use crate::source::Source;
use crate::tool::{InstallDependency, InstallDependencyKind, InstallIdentity, ToolId};
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

const GO_PROXY_METADATA_LIMIT: usize = 4 * 1024 * 1024;
const GO_PROXY_TIMEOUT: Duration = Duration::from_secs(30);
const GO_RESOLUTION_FILE: &str = "go-resolution.json";
const GO_RESOLUTION_SCHEMA: u32 = 1;
const PROVIDER_OUTPUT_LIMIT: usize = 1024 * 1024;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const LOCKED_GO_PROXY_OPTION: &str = "__osdk_go_proxy";
pub const LOCKED_GO_MODULE_OPTION: &str = "__osdk_go_module";
static NEXT_METADATA_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoResolution {
    schema: u32,
    backend: String,
    version: String,
    proxy: String,
    module_root: String,
    replay: String,
}

#[derive(Debug)]
struct GoProxySelection {
    source: Source,
    module_root: String,
    versions: Vec<VersionInfo>,
}

/// Backend bound to one canonical Go module or nested command path.
pub struct GoPackageBackend {
    id: String,
    command_path: String,
}

impl GoPackageBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let id = ToolId::parse(id).ok()?;
        if id.namespace() != Some("go") {
            return None;
        }
        Some(Self {
            command_path: id.subject().to_string(),
            id: id.to_string(),
        })
    }

    fn runtime_version<'a>(&self, options: &'a BTreeMap<String, String>) -> Result<&'a str> {
        match (
            options.get(LOCKED_NATIVE_RUNTIME_OPTION),
            options.get(LOCKED_NATIVE_RUNTIME_VERSION_OPTION),
        ) {
            (Some(runtime), Some(version))
                if runtime == "go"
                    && matches!(VersionSpec::parse(version), VersionSpec::Exact(exact) if exact == *version) =>
            {
                Ok(version)
            }
            (Some(runtime), _) if runtime != "go" => Err(Error::config(format!(
                "Go tool `{}` requires managed runtime `go`, got `{runtime}`",
                self.id
            ))),
            _ => Err(Error::config(format!(
                "Go tool `{}` requires an exact managed Go selection; add an exact `go@<version>` request or configuration",
                self.id
            ))),
        }
    }

    fn canonical_options(
        &self,
        options: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let id = ToolId::parse(&self.id)?;
        let mut canonical = options
            .iter()
            .filter(|(name, _)| name.starts_with("__osdk_"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        canonical.extend(crate::tool::canonicalize_dynamic_options(&id, options)?.into_map());
        Ok(canonical)
    }

    fn runtime_dependency(
        &self,
        ctx: &Ctx,
        options: &BTreeMap<String, String>,
    ) -> Result<InstallDependency> {
        let version = self.runtime_version(options)?;
        let root = ctx.dirs.install_path("go", version);
        if !regular_file(&root.join(".osdk-complete")) {
            return Err(Error::NotInstalled {
                tool: "go".into(),
                version: version.into(),
            });
        }
        Ok(InstallDependency {
            kind: InstallDependencyKind::Runtime,
            id: "go".into(),
            version: version.into(),
            identity: Some(native_tool::go_runtime_identity(
                &ctx.dirs,
                ctx.platform,
                version,
            )?),
        })
    }

    fn proxy<'a>(&self, options: &'a BTreeMap<String, String>) -> Result<&'a str> {
        let proxy = options
            .get(LOCKED_GO_PROXY_OPTION)
            .map(String::as_str)
            .unwrap_or("https://proxy.golang.org");
        validate_go_proxy(proxy)?;
        Ok(proxy)
    }

    fn module_root<'a>(&'a self, options: &'a BTreeMap<String, String>) -> Result<&'a str> {
        let module = options
            .get(LOCKED_GO_MODULE_OPTION)
            .map(String::as_str)
            .unwrap_or(&self.command_path);
        let id = ToolId::parse(&format!("go:{module}"))?;
        let suffix = self.command_path.strip_prefix(module).unwrap_or("!");
        if id.subject() != module || (!suffix.is_empty() && !suffix.starts_with('/')) {
            return Err(Error::config("locked Go module root is invalid"));
        }
        Ok(module)
    }

    fn has_locked_resolution(options: &BTreeMap<String, String>) -> bool {
        options.get(LOCKED_NATIVE_REPLAY_OPTION).map(String::as_str) == Some("version-only")
            && options.contains_key(LOCKED_GO_PROXY_OPTION)
            && options.contains_key(LOCKED_GO_MODULE_OPTION)
    }

    fn materials(&self, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::from([
            ("source-kind".into(), "go-proxy".into()),
            ("command-path".into(), self.command_path.clone()),
            ("proxy".into(), self.proxy(&tv.options)?.into()),
            ("module-root".into(), self.module_root(&tv.options)?.into()),
        ]))
    }

    fn materials_match(&self, tv: &ToolVersion, actual: &BTreeMap<String, String>) -> bool {
        if actual.get("source-kind").map(String::as_str) != Some("go-proxy")
            || actual.get("command-path").map(String::as_str) != Some(self.command_path.as_str())
            || actual.len() != 4
        {
            return false;
        }
        let Some(proxy) = actual.get("proxy") else {
            return false;
        };
        let Some(module_root) = actual.get("module-root") else {
            return false;
        };
        validate_go_proxy(proxy).is_ok()
            && ToolId::parse(&format!("go:{module_root}"))
                .is_ok_and(|id| id.subject() == module_root)
            && self
                .command_path
                .strip_prefix(module_root)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
            && tv
                .options
                .get(LOCKED_GO_PROXY_OPTION)
                .is_none_or(|expected| expected == proxy)
            && tv
                .options
                .get(LOCKED_GO_MODULE_OPTION)
                .is_none_or(|expected| expected == module_root)
    }

    fn lifecycle(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<NativeToolLifecycle> {
        NativeToolLifecycle::new(
            &ctx.dirs,
            ctx.platform,
            &self.id,
            &tv.version,
            &tv.options,
            NativeToolFamily::Go,
            self.runtime_dependency(ctx, &tv.options)?,
            self.materials(tv)?,
        )
    }

    fn selected_lifecycle(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<NativeToolLifecycle>> {
        if tv
            .options
            .contains_key(LOCKED_NATIVE_RUNTIME_VERSION_OPTION)
            && tv.options.contains_key(LOCKED_GO_PROXY_OPTION)
        {
            return self.lifecycle(ctx, tv).map(Some);
        }
        let expected_options = crate::backend::dynamic::identity_options(&self.id, &tv.options)?;
        let report = crate::inventory::scan_installs(
            &ctx.dirs.installs,
            &crate::inventory::ScanOptions::default(),
        )?;
        let mut matching = report.installs.into_iter().filter(|install| {
            let identity = &install.manifest.identity;
            identity.tool == self.id
                && identity.version == tv.version
                && identity.platform == ctx.platform.to_string()
                && identity.scope == crate::tool::InstallScope::Isolated
                && identity.material_options == expected_options
                && self.materials_match(tv, &identity.materials)
                && native_tool::validate_install_candidate(
                    &ctx.dirs,
                    NativeToolFamily::Go,
                    &install.install_root,
                    identity,
                )
                .unwrap_or(false)
        });
        let first = matching.next();
        if matching.next().is_some() {
            return Err(Error::other(format!(
                "Go tool `{}@{}` has multiple matching managed Go identities; select it through a lockfile",
                self.id, tv.version
            )));
        }
        first
            .map(|install| {
                NativeToolLifecycle::from_identity(
                    &ctx.dirs,
                    NativeToolFamily::Go,
                    install.manifest.identity,
                )
            })
            .transpose()
    }

    async fn proxy_selection(&self, ctx: &Ctx) -> Result<GoProxySelection> {
        let sources = self.ranked_proxy_sources(ctx).await?;
        let tried = sources.len();
        let candidates = module_path_candidates(&self.command_path);
        let mut last_error = None;
        if !ctx.config.settings.offline {
            for module in &candidates {
                for source in &sources {
                    let url = proxy_list_url(source, module);
                    match fetch_live_proxy_versions(ctx, source, &url).await {
                        Ok(versions) if !versions.is_empty() => {
                            return Ok(GoProxySelection {
                                source: source.clone(),
                                module_root: module.clone(),
                                versions,
                            });
                        }
                        Ok(_) => {}
                        Err(error) => last_error = Some(error),
                    }
                }
            }
        }
        for module in &candidates {
            for source in &sources {
                let url = proxy_list_url(source, module);
                match read_cached_proxy_versions(ctx, source, &url) {
                    Ok(versions) if !versions.is_empty() => {
                        tracing::warn!(
                            source = %source.id,
                            url,
                            "using stale cached Go proxy metadata after all live sources failed"
                        );
                        return Ok(GoProxySelection {
                            source: source.clone(),
                            module_root: module.clone(),
                            versions,
                        });
                    }
                    Ok(_) => {}
                    Err(_) if !ctx.config.settings.offline => {}
                    Err(_) => {
                        last_error = Some(Error::other(format!(
                            "offline Go proxy metadata cache miss for {url}"
                        )));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id.clone(),
            tried,
        }))
    }

    async fn exact_proxy_selection(&self, ctx: &Ctx, version: &str) -> Result<GoProxySelection> {
        let sources = self.ranked_proxy_sources(ctx).await?;
        let tried = sources.len();
        let candidates = module_path_candidates(&self.command_path);
        let mut last_error = None;
        if !ctx.config.settings.offline {
            for module in &candidates {
                for source in &sources {
                    let url = proxy_info_url(source, module, version);
                    match fetch_live_proxy_info(ctx, source, &url, version).await {
                        Ok(()) => {
                            return Ok(GoProxySelection {
                                source: source.clone(),
                                module_root: module.clone(),
                                versions: vec![version_info(version)],
                            });
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
            }
        }
        for module in &candidates {
            for source in &sources {
                let url = proxy_info_url(source, module, version);
                match read_cached_proxy_info(ctx, source, &url, version) {
                    Ok(()) => {
                        tracing::warn!(
                            source = %source.id,
                            url,
                            "using stale cached Go proxy version evidence after all live sources failed"
                        );
                        return Ok(GoProxySelection {
                            source: source.clone(),
                            module_root: module.clone(),
                            versions: vec![version_info(version)],
                        });
                    }
                    Err(_) if !ctx.config.settings.offline => {}
                    Err(_) => {
                        last_error = Some(Error::other(format!(
                            "offline Go proxy metadata cache miss for {url}"
                        )));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id.clone(),
            tried,
        }))
    }

    async fn latest_proxy_selection(&self, ctx: &Ctx) -> Result<GoProxySelection> {
        let sources = self.ranked_proxy_sources(ctx).await?;
        let tried = sources.len();
        let candidates = module_path_candidates(&self.command_path);
        let mut last_error = None;
        if !ctx.config.settings.offline {
            for module in &candidates {
                for source in &sources {
                    let url = proxy_latest_url(source, module);
                    match fetch_live_proxy_latest(ctx, source, &url).await {
                        Ok(version) => {
                            return Ok(GoProxySelection {
                                source: source.clone(),
                                module_root: module.clone(),
                                versions: vec![version_info(&version)],
                            });
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
            }
        }
        for module in &candidates {
            for source in &sources {
                let url = proxy_latest_url(source, module);
                match read_cached_proxy_latest(ctx, source, &url) {
                    Ok(version) => {
                        tracing::warn!(
                            source = %source.id,
                            url,
                            "using stale cached Go proxy latest metadata after all live sources failed"
                        );
                        return Ok(GoProxySelection {
                            source: source.clone(),
                            module_root: module.clone(),
                            versions: vec![version_info(&version)],
                        });
                    }
                    Err(_) if !ctx.config.settings.offline => {}
                    Err(_) => {
                        last_error = Some(Error::other(format!(
                            "offline Go proxy metadata cache miss for {url}"
                        )));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id.clone(),
            tried,
        }))
    }

    async fn ranked_proxy_sources(&self, ctx: &Ctx) -> Result<Vec<Source>> {
        let mut sources = crate::source::select::effective_sources(ctx, self);
        if let Some(config) = ctx.config.tool_sources(self.id()) {
            if !config.custom.is_empty() {
                let mut allowed_ids = config
                    .custom
                    .iter()
                    .map(|source| source.id.as_str())
                    .collect::<std::collections::BTreeSet<_>>();
                if let Some(pin) = config.pin.as_deref() {
                    allowed_ids.insert(pin);
                }
                sources.retain(|source| allowed_ids.contains(source.id.as_str()));
            }
        }
        for source in &sources {
            validate_go_source(source)?;
        }
        crate::source::select::ranked_source_candidates(ctx, self, sources).await
    }

    fn managed_go(
        &self,
        ctx: &Ctx,
        options: &BTreeMap<String, String>,
    ) -> Result<(PathBuf, PathBuf)> {
        let version = self.runtime_version(options)?;
        let root = ctx.dirs.install_path("go", version);
        let go = root
            .join("bin")
            .join(format!("go{}", ctx.platform.os.exe_suffix()));
        let root_metadata =
            std::fs::symlink_metadata(&root).map_err(|error| Error::io(&root, error))?;
        let go_metadata = std::fs::symlink_metadata(&go).map_err(|error| Error::io(&go, error))?;
        let canonical_root = dunce::canonicalize(&root).map_err(|error| Error::io(&root, error))?;
        let canonical_go = dunce::canonicalize(&go).map_err(|error| Error::io(&go, error))?;
        let canonical_store = dunce::canonicalize(&ctx.dirs.store).ok();
        if root_metadata.file_type().is_symlink()
            || !root_metadata.is_dir()
            || (!go_metadata.is_file() && !go_metadata.file_type().is_symlink())
            || (!canonical_go.starts_with(&canonical_root)
                && canonical_store
                    .as_ref()
                    .is_none_or(|store| !canonical_go.starts_with(store)))
        {
            return Err(Error::other(format!(
                "managed Go runtime `{version}` has an unsafe or missing go executable"
            )));
        }
        Ok((root, canonical_go))
    }

    fn command_env(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        stage: &Path,
    ) -> Result<BTreeMap<OsString, OsString>> {
        let (go_root, _) = self.managed_go(ctx, &tv.options)?;
        let home = stage.join("home");
        let go_path = stage.join("gopath");
        let tmp = stage.join("tmp");
        let module_cache = crate::cache::downstream_root(&ctx.dirs.cache).join("go-mod");
        let build_cache = crate::cache::downstream_root(&ctx.dirs.cache).join("go-build");
        for path in [&home, &go_path, &tmp, &module_cache, &build_cache] {
            std::fs::create_dir_all(path).map_err(|error| Error::io(path, error))?;
        }
        let mut env = BTreeMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("USERPROFILE"), home.into_os_string()),
            (OsString::from("GOROOT"), go_root.clone().into_os_string()),
            (OsString::from("GOBIN"), stage.join("bin").into_os_string()),
            (OsString::from("GOPATH"), go_path.into_os_string()),
            (OsString::from("GOMODCACHE"), module_cache.into_os_string()),
            (OsString::from("GOCACHE"), build_cache.into_os_string()),
            (OsString::from("GOENV"), OsString::from("off")),
            (OsString::from("GOTOOLCHAIN"), OsString::from("local")),
            (OsString::from("CGO_ENABLED"), OsString::from("0")),
            (OsString::from("GOSUMDB"), OsString::from("off")),
            (
                OsString::from("GONOSUMDB"),
                OsString::from(self.module_root(&tv.options)?),
            ),
            (OsString::from("GONOPROXY"), OsString::from("none")),
            (
                OsString::from("GOPROXY"),
                OsString::from(self.proxy(&tv.options)?),
            ),
            (
                OsString::from("PATH"),
                sanitized_provider_path(ctx, &go_root.join("bin"), std::env::var_os("PATH"))?,
            ),
            (OsString::from("TMPDIR"), tmp.clone().into_os_string()),
            (OsString::from("TEMP"), tmp.clone().into_os_string()),
            (OsString::from("TMP"), tmp.into_os_string()),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ]);
        if let Some(values) = tv.options.get("env") {
            for assignment in values.split(';') {
                let (name, value) = assignment
                    .split_once('=')
                    .ok_or_else(|| Error::config("invalid canonical Go install env"))?;
                env.insert(OsString::from(name), OsString::from(value));
            }
        }
        if ctx.platform.os == crate::platform::Os::Windows {
            for name in ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
                if let Some(value) = std::env::var_os(name) {
                    env.insert(OsString::from(name), value);
                }
            }
        }
        Ok(env)
    }

    fn install_args(&self, tv: &ToolVersion) -> Vec<OsString> {
        let mut args = vec![OsString::from("install")];
        if let Some(tags) = tv.options.get("tags") {
            args.push(OsString::from("-tags"));
            args.push(OsString::from(tags));
        }
        args.push(OsString::from(format!(
            "{}@{}",
            self.command_path,
            go_provider_version(&tv.version)
        )));
        args
    }

    async fn install_with_runner(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        runner: &dyn CommandRunner,
    ) -> Result<()> {
        let canonical = self.canonical_options(&tv.options)?;
        if canonical != tv.options {
            return Err(Error::config(format!(
                "Go tool `{}` contains non-canonical install options",
                self.id
            )));
        }
        if ctx.config.settings.offline {
            if let Some(lifecycle) = self.selected_lifecycle(ctx, tv)? {
                if lifecycle.validate_complete(&ctx.dirs)? {
                    return Ok(());
                }
            }
            return Err(Error::other(format!(
                "offline Go install requires an already complete matching install for `{}`; Go native locks do not contain a complete module graph",
                self.id
            )));
        }
        let lifecycle = self.lifecycle(ctx, tv)?;
        let NativeToolPreparation::Staged(stage) = lifecycle.prepare(&ctx.dirs).await? else {
            return Ok(());
        };
        let (_, go) = self.managed_go(ctx, &tv.options)?;
        let env = self.command_env(ctx, tv, stage.path())?;
        let command = CommandSpec::new(go.as_os_str().to_owned())
            .args(self.install_args(tv))
            .envs(env)
            .current_dir(stage.path())
            .clear_env();
        match runner.run_captured(
            &command,
            CaptureLimits::new(
                PROVIDER_TIMEOUT,
                PROVIDER_OUTPUT_LIMIT,
                PROVIDER_OUTPUT_LIMIT,
            ),
        ) {
            CommandOutcome::Exited { status, output: _ } if status.success() => {}
            CommandOutcome::Exited { status, output } => {
                return Err(provider_error(status.to_string(), &output.stderr));
            }
            outcome => {
                return Err(Error::other(format!(
                    "go install could not run: {outcome:?}"
                )))
            }
        }
        clean_provider_workspace(stage.path())?;
        write_resolution(stage.path(), &self.resolution(tv)?)?;
        stage.publish(NativeToolProvider::GoInstall)?;
        Ok(())
    }

    fn resolution(&self, tv: &ToolVersion) -> Result<GoResolution> {
        Ok(GoResolution {
            schema: GO_RESOLUTION_SCHEMA,
            backend: self.id.clone(),
            version: tv.version.clone(),
            proxy: self.proxy(&tv.options)?.into(),
            module_root: self.module_root(&tv.options)?.into(),
            replay: "version-only".into(),
        })
    }

    fn resolution_matches(&self, tv: &ToolVersion, actual: &GoResolution) -> bool {
        let valid_module = ToolId::parse(&format!("go:{}", actual.module_root))
            .is_ok_and(|id| id.subject() == actual.module_root)
            && self
                .command_path
                .strip_prefix(&actual.module_root)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'));
        actual.schema == GO_RESOLUTION_SCHEMA
            && actual.backend == self.id
            && actual.version == tv.version
            && actual.replay == "version-only"
            && validate_go_proxy(&actual.proxy).is_ok()
            && valid_module
            && tv
                .options
                .get(LOCKED_GO_PROXY_OPTION)
                .is_none_or(|proxy| proxy == &actual.proxy)
            && tv
                .options
                .get(LOCKED_GO_MODULE_OPTION)
                .is_none_or(|module| module == &actual.module_root)
    }
}

fn go_provider_version(version: &str) -> String {
    if let Some(base) = version.strip_suffix("+incompatible") {
        format!("v{base}+incompatible")
    } else {
        format!("v{version}")
    }
}

#[async_trait]
impl Backend for GoPackageBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("proxy.golang.org", "https://proxy.golang.org"),
            Source::mirror("goproxy.cn", "https://goproxy.cn", 10),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        validate_go_source(source)
            .ok()
            .map(|()| format!("{}/", source.download_url.trim_end_matches('/')))
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        Ok(self.proxy_selection(ctx).await?.versions)
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let id = ToolId::parse(&req.backend)?;
        crate::tool::validate_dynamic_selector(&id, Some(&req.spec.to_string()))?;
        let mut options = req
            .options
            .iter()
            .filter(|(name, _)| name.starts_with("__osdk_"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        options.extend(crate::tool::canonicalize_dynamic_options(&id, &req.options)?.into_map());
        if ctx.config.settings.offline
            && matches!(&req.spec, VersionSpec::Exact(version) if crate::tool::is_canonical_go_module_version(version))
            && !options.contains_key(LOCKED_GO_PROXY_OPTION)
            && !options.contains_key(LOCKED_GO_MODULE_OPTION)
        {
            let mut resolved = ToolVersion::new(&self.id, req.spec.to_string());
            resolved.options = options;
            resolved
                .options
                .insert(LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into());
            return Ok(resolved);
        }
        let (version, source, module_root) = match &req.spec {
            VersionSpec::Exact(version) if crate::tool::is_canonical_go_module_version(version) => {
                let selection = if Self::has_locked_resolution(&options) {
                    let proxy = options
                        .get(LOCKED_GO_PROXY_OPTION)
                        .expect("locked resolution checked proxy");
                    let module = options
                        .get(LOCKED_GO_MODULE_OPTION)
                        .expect("locked resolution checked module");
                    validate_go_proxy(proxy)?;
                    self.module_root(&options)?;
                    GoProxySelection {
                        source: Source::official("locked", proxy),
                        module_root: module.clone(),
                        versions: vec![version_info(version)],
                    }
                } else {
                    self.exact_proxy_selection(ctx, version).await?
                };
                (
                    version.clone(),
                    selection.source.download_url,
                    selection.module_root,
                )
            }
            VersionSpec::Latest => {
                let selection = self.latest_proxy_selection(ctx).await?;
                let selected = selection
                    .versions
                    .first()
                    .ok_or_else(|| Error::VersionResolve {
                        tool: self.id.clone(),
                        spec: req.spec.to_string(),
                        hint: Some("Go proxy returned no latest module version".into()),
                    })?;
                (
                    selected.version.clone(),
                    selection.source.download_url,
                    selection.module_root,
                )
            }
            VersionSpec::Prefix(_) => {
                let selection = self.proxy_selection(ctx).await?;
                let stable = selection
                    .versions
                    .iter()
                    .filter(|version| version.stable)
                    .cloned()
                    .collect::<Vec<_>>();
                let selected =
                    crate::version::select_version(&req.spec, &stable).ok_or_else(|| {
                        Error::VersionResolve {
                        tool: self.id.clone(),
                        spec: req.spec.to_string(),
                        hint: Some(
                            "no matching Go module release found through the configured proxies"
                                .into(),
                        ),
                    }
                    })?;
                (
                    selected.version.clone(),
                    selection.source.download_url,
                    selection.module_root,
                )
            }
            _ => {
                return Err(Error::VersionResolve {
                    tool: self.id.clone(),
                    spec: req.spec.to_string(),
                    hint: Some("Go tools require latest, an exact semantic or pseudo-version, or a numeric prefix".into()),
                });
            }
        };
        validate_go_proxy(&source)?;
        let mut resolved = ToolVersion::new(&self.id, version);
        resolved.options = options;
        resolved
            .options
            .insert(LOCKED_GO_PROXY_OPTION.into(), source);
        resolved
            .options
            .insert(LOCKED_GO_MODULE_OPTION.into(), module_root);
        resolved
            .options
            .insert(LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into());
        Ok(resolved)
    }

    async fn install(&self, ctx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        self.install_with_runner(ctx.ctx, tv, &SystemCommandRunner)
            .await
    }

    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        if let Some(lifecycle) = self.selected_lifecycle(ctx, tv)? {
            lifecycle.uninstall().await?;
        }
        Ok(())
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        native_tool::list_installed(&ctx.dirs, ctx.platform, NativeToolFamily::Go, &self.id)
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let Some(lifecycle) = self.selected_lifecycle(ctx, tv)? else {
            return Ok(Vec::new());
        };
        Ok(lifecycle
            .validate_complete(&ctx.dirs)?
            .then(|| lifecycle.install_root().join("bin"))
            .into_iter()
            .collect())
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let Some(lifecycle) = self.selected_lifecycle(ctx, tv)? else {
            return Err(Error::NotInstalled {
                tool: self.id.clone(),
                version: tv.version.clone(),
            });
        };
        if !lifecycle.validate_complete(&ctx.dirs)? {
            return Err(Error::NotInstalled {
                tool: self.id.clone(),
                version: tv.version.clone(),
            });
        }
        Ok(native_tool::load_receipt(lifecycle.install_root())?
            .bins
            .into_iter()
            .filter_map(|bin| {
                Path::new(&bin.path)
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .map(str::to_string)
            })
            .collect())
    }

    fn dynamic_install_identity(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<InstallIdentity>> {
        if !tv
            .options
            .contains_key(LOCKED_NATIVE_RUNTIME_VERSION_OPTION)
        {
            return Ok(None);
        }
        self.lifecycle(ctx, tv)
            .map(|lifecycle| Some(lifecycle.identity().clone()))
    }

    fn validate_dynamic_install(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        install_root: &Path,
        identity: &InstallIdentity,
    ) -> Result<bool> {
        if identity.tool != self.id
            || identity.version != tv.version
            || !self.materials_match(tv, &identity.materials)
            || identity.material_options
                != crate::backend::dynamic::identity_options(&self.id, &tv.options)?
            || !self.resolution_matches(tv, &load_resolution(install_root)?)
        {
            return Ok(false);
        }
        NativeToolLifecycle::from_identity(&ctx.dirs, NativeToolFamily::Go, identity.clone())?
            .validate_dynamic_install(&ctx.dirs, install_root, identity)
    }
}

fn module_path_candidates(path: &str) -> Vec<String> {
    let components = path.split('/').collect::<Vec<_>>();
    (2..=components.len())
        .rev()
        .map(|length| components[..length].join("/"))
        .collect()
}

fn escape_module_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_ascii_uppercase() {
            escaped.push('!');
            escaped.push(character.to_ascii_lowercase());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn proxy_list_url(source: &Source, module: &str) -> String {
    format!(
        "{}/{}/@v/list",
        source.download_url.trim_end_matches('/'),
        escape_module_path(module)
    )
}

fn proxy_info_url(source: &Source, module: &str, version: &str) -> String {
    format!(
        "{}/{}/@v/{}.info",
        source.download_url.trim_end_matches('/'),
        escape_module_path(module),
        escape_go_token(&format!("v{version}"))
    )
}

fn proxy_latest_url(source: &Source, module: &str) -> String {
    format!(
        "{}/{}/@latest",
        source.download_url.trim_end_matches('/'),
        escape_module_path(module)
    )
}

fn escape_go_token(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            escaped.push('!');
            escaped.push(character.to_ascii_lowercase());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn validate_go_source(source: &Source) -> Result<()> {
    validate_go_proxy(&source.download_url)?;
    if !source.headers.is_empty() {
        return Err(Error::config(format!(
            "Go proxy source `{}` cannot use custom HTTP headers because the go command cannot enforce their forwarding boundary",
            source.id
        )));
    }
    Ok(())
}

pub fn validate_go_proxy(value: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(value).map_err(|_| Error::config("Go proxy URL is invalid"))?;
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
    let canonical = parsed.as_str().trim_end_matches('/');
    if (parsed.scheme() != "https" && !loopback_http)
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || (parsed.path() != "/" && parsed.path().ends_with('/'))
        || canonical != value
    {
        return Err(Error::config(
            "Go proxy must be a canonical HTTPS URL without credentials, query, fragment, or trailing slash",
        ));
    }
    Ok(())
}

fn parse_proxy_versions(bytes: &[u8]) -> Result<Vec<VersionInfo>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::config("Go proxy version list is not valid UTF-8"))?;
    let mut versions = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let canonical = line.strip_prefix('v')?;
            let parsed = semver::Version::parse(canonical).ok()?;
            if !crate::tool::is_canonical_go_module_version(canonical) {
                return None;
            }
            Some(VersionInfo {
                version: canonical.to_string(),
                stable: parsed.pre.is_empty(),
                lts: None,
            })
        })
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| {
        semver::Version::parse(&left.version)
            .expect("filtered canonical Go version")
            .cmp(&semver::Version::parse(&right.version).expect("filtered canonical Go version"))
    });
    versions.dedup_by(|left, right| left.version == right.version);
    Ok(versions)
}

fn version_info(version: &str) -> VersionInfo {
    VersionInfo {
        version: version.into(),
        stable: semver::Version::parse(version).is_ok_and(|version| version.pre.is_empty()),
        lts: None,
    }
}

#[derive(Deserialize)]
struct GoProxyInfo {
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Time")]
    _time: String,
}

fn parse_proxy_info(bytes: &[u8], expected: &str) -> Result<()> {
    let info: GoProxyInfo = serde_json::from_slice(bytes)?;
    if info.version != format!("v{expected}") {
        return Err(Error::config(format!(
            "Go proxy returned version `{}` while resolving `v{expected}`",
            info.version
        )));
    }
    Ok(())
}

fn parse_proxy_latest(bytes: &[u8]) -> Result<String> {
    let info: GoProxyInfo = serde_json::from_slice(bytes)?;
    let version = info
        .version
        .strip_prefix('v')
        .ok_or_else(|| Error::config("Go proxy latest version must start with `v`"))?;
    if !crate::tool::is_canonical_go_module_version(version) {
        return Err(Error::config(
            "Go proxy latest metadata has an invalid version",
        ));
    }
    Ok(version.to_string())
}

async fn fetch_live_proxy_info(
    ctx: &Ctx,
    source: &Source,
    url: &str,
    expected: &str,
) -> Result<()> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = fetch_live_proxy_bytes(
        ctx,
        source,
        url,
        64 * 1024,
        "Go proxy version metadata exceeds 64 KiB",
    )
    .await?;
    parse_proxy_info(&bytes, expected)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let serial = NEXT_METADATA_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = cache.with_extension(format!("tmp-{}-{serial}", std::process::id()));
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &cache);
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(())
}

fn read_cached_proxy_info(ctx: &Ctx, source: &Source, url: &str, expected: &str) -> Result<()> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = crate::inventory::read_stable_regular_file(&cache, 64 * 1024)
        .map_err(|error| Error::io(&cache, error))?;
    parse_proxy_info(&bytes, expected)
}

async fn fetch_live_proxy_latest(ctx: &Ctx, source: &Source, url: &str) -> Result<String> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = fetch_live_proxy_bytes(
        ctx,
        source,
        url,
        64 * 1024,
        "Go proxy latest metadata exceeds 64 KiB",
    )
    .await?;
    let version = parse_proxy_latest(&bytes)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let serial = NEXT_METADATA_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = cache.with_extension(format!("tmp-{}-{serial}", std::process::id()));
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &cache);
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(version)
}

fn read_cached_proxy_latest(ctx: &Ctx, source: &Source, url: &str) -> Result<String> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = crate::inventory::read_stable_regular_file(&cache, 64 * 1024)
        .map_err(|error| Error::io(&cache, error))?;
    parse_proxy_latest(&bytes)
}

async fn fetch_live_proxy_versions(
    ctx: &Ctx,
    source: &Source,
    url: &str,
) -> Result<Vec<VersionInfo>> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = fetch_live_proxy_bytes(
        ctx,
        source,
        url,
        GO_PROXY_METADATA_LIMIT,
        "Go proxy metadata exceeds the 4 MiB limit",
    )
    .await?;
    let versions = parse_proxy_versions(&bytes)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let serial = NEXT_METADATA_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = cache.with_extension(format!("tmp-{}-{serial}", std::process::id()));
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &cache);
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(versions)
}

async fn fetch_live_proxy_bytes(
    ctx: &Ctx,
    source: &Source,
    url: &str,
    limit: usize,
    limit_error: &'static str,
) -> Result<Vec<u8>> {
    let fetch = async {
        let response = crate::http::get_source_response(&ctx.client, source, url)
            .await?
            .error_for_status()
            .map_err(|error| Error::network(url, error))?;
        if response
            .content_length()
            .is_some_and(|size| size > limit as u64)
        {
            return Err(Error::other(limit_error));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Error::network(url, error))?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(Error::other(limit_error));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, Error>(bytes)
    };
    tokio::time::timeout(GO_PROXY_TIMEOUT, fetch)
        .await
        .map_err(|_| Error::other("Go proxy metadata exceeded the 30 second timeout"))?
}

fn read_cached_proxy_versions(ctx: &Ctx, source: &Source, url: &str) -> Result<Vec<VersionInfo>> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let bytes = crate::inventory::read_stable_regular_file(&cache, GO_PROXY_METADATA_LIMIT as u64)
        .map_err(|error| Error::io(&cache, error))?;
    parse_proxy_versions(&bytes)
}

fn sanitized_provider_path(
    ctx: &Ctx,
    managed_bin: &Path,
    inherited: Option<OsString>,
) -> Result<OsString> {
    let shims = ctx.dirs.shims();
    let managed_go_base = ctx.dirs.installs.join("go");
    let mut paths = vec![managed_bin.to_path_buf()];
    if let Some(inherited) = inherited {
        for path in std::env::split_paths(&inherited) {
            if path.as_os_str().is_empty()
                || path == managed_bin
                || path == shims
                || path.starts_with(&managed_go_base)
                || paths.iter().any(|existing| existing == &path)
            {
                continue;
            }
            paths.push(path);
        }
    }
    std::env::join_paths(paths)
        .map_err(|error| Error::config(format!("invalid sanitized Go provider PATH: {error}")))
}

fn clean_provider_workspace(stage: &Path) -> Result<()> {
    for name in ["home", "gopath", "tmp"] {
        let path = stage.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::other(format!(
                    "Go provider workspace is unsafe: {}",
                    path.display()
                )));
            }
            Ok(_) => std::fs::remove_dir_all(&path).map_err(|error| Error::io(&path, error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
    }
    Ok(())
}

fn write_resolution(root: &Path, resolution: &GoResolution) -> Result<()> {
    let path = root.join(GO_RESOLUTION_FILE);
    let bytes = serde_json::to_vec_pretty(resolution)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write as _;
    let mut file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::other(format!(
                "Go provider wrote reserved metadata path {}",
                path.display()
            ))
        } else {
            Error::io(&path, error)
        }
    })?;
    file.write_all(&bytes)
        .map_err(|error| Error::io(&path, error))
}

fn load_resolution(root: &Path) -> Result<GoResolution> {
    let path = root.join(GO_RESOLUTION_FILE);
    let bytes = crate::inventory::read_stable_regular_file(&path, 64 * 1024)
        .map_err(|error| Error::io(&path, error))?;
    let resolution: GoResolution = serde_json::from_slice(&bytes)?;
    if resolution.schema != GO_RESOLUTION_SCHEMA {
        return Err(Error::config("unsupported Go resolution schema"));
    }
    Ok(resolution)
}

fn provider_error(status: String, stderr: &[u8]) -> Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    Error::Command {
        cmd: "go install".into(),
        status,
        stderr: (!stderr.is_empty()).then(|| stderr.to_string()),
    }
}

fn regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crate::config::{Config, Settings};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::process::CapturedOutput;
    use crate::store::Cas;

    fn context(root: &Path, offline: bool) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
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
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings {
                    offline,
                    ..Default::default()
                },
                sources: Default::default(),
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
        }
    }

    fn write_executable(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn managed_go(ctx: &Ctx, version: &str) {
        let root = ctx.dirs.install_path("go", version);
        for directory in ["bin", "pkg/tool", "src/runtime"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for name in ["go", "gofmt"] {
            write_executable(
                &root
                    .join("bin")
                    .join(format!("{name}{}", ctx.platform.os.exe_suffix())),
                name.as_bytes(),
            );
        }
        write_executable(
            &root
                .join("pkg/tool")
                .join(format!("compile{}", ctx.platform.os.exe_suffix())),
            b"compile",
        );
        std::fs::write(root.join("src/runtime/runtime.go"), b"package runtime").unwrap();
        std::fs::write(root.join("VERSION"), format!("go{version}\n")).unwrap();
        std::fs::write(root.join("go.env"), b"GOTOOLCHAIN=local\n").unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    fn selected(backend: &GoPackageBackend, runtime: &str) -> ToolVersion {
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.extend(BTreeMap::from([
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), runtime.into()),
            (LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into()),
            (
                LOCKED_GO_PROXY_OPTION.into(),
                "https://proxy.golang.org".into(),
            ),
            (LOCKED_GO_MODULE_OPTION.into(), backend.command_path.clone()),
        ]));
        version
    }

    #[derive(Clone)]
    struct FixtureRunner {
        calls: Arc<Mutex<Vec<CommandSpec>>>,
        statuses: Arc<Mutex<Vec<i32>>>,
        forge_resolution: bool,
    }

    impl FixtureRunner {
        fn new(statuses: impl IntoIterator<Item = i32>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                statuses: Arc::new(Mutex::new(statuses.into_iter().collect())),
                forge_resolution: false,
            }
        }
    }

    impl CommandRunner for FixtureRunner {
        fn run_captured(&self, command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            self.calls.lock().unwrap().push(command.clone());
            let stage = command.working_directory().unwrap();
            std::fs::create_dir_all(stage.join("bin")).unwrap();
            let code = self.statuses.lock().unwrap().remove(0);
            if code == 0 {
                write_executable(
                    &stage
                        .join("bin")
                        .join(if cfg!(windows) { "tool.exe" } else { "tool" }),
                    b"fixture binary",
                );
                if self.forge_resolution {
                    std::fs::write(stage.join(GO_RESOLUTION_FILE), b"{}").unwrap();
                }
            }
            exited(code)
        }

        fn run_foreground(
            &self,
            _command: &CommandSpec,
        ) -> std::io::Result<std::process::ExitStatus> {
            unreachable!()
        }
    }

    #[cfg(unix)]
    fn exited(code: i32) -> CommandOutcome {
        use std::os::unix::process::ExitStatusExt;
        CommandOutcome::Exited {
            status: std::process::ExitStatus::from_raw(code << 8),
            output: CapturedOutput::default(),
        }
    }

    #[cfg(windows)]
    fn exited(code: i32) -> CommandOutcome {
        use std::os::windows::process::ExitStatusExt;
        CommandOutcome::Exited {
            status: std::process::ExitStatus::from_raw(code as u32),
            output: CapturedOutput::default(),
        }
    }

    struct ProxyServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        shutdown: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl ProxyServer {
        fn start(responses: Vec<(String, String, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = Arc::clone(&requests);
            let (shutdown, shutdown_rx) = mpsc::channel();
            let handle = thread::spawn(move || loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets inherit the listener's nonblocking mode on
                        // Windows. Switch back to blocking I/O before reading the request
                        // so the fixture behaves consistently across platforms.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 1024];
                        while !request.ends_with(b"\r\n\r\n") {
                            let read = stream.read(&mut buffer).unwrap();
                            if read == 0 {
                                break;
                            }
                            request.extend_from_slice(&buffer[..read]);
                        }
                        let path = String::from_utf8(request)
                            .unwrap()
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        server_requests.lock().unwrap().push(path.clone());
                        let (status, body) = responses
                            .iter()
                            .find(|(expected, _, _)| expected == &path)
                            .map(|(_, status, body)| (status.as_str(), body.as_str()))
                            .unwrap_or(("404 Not Found", ""));
                        write!(
                            stream,
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("proxy server failed: {error}"),
                }
            });
            Self {
                base_url,
                requests,
                shutdown: Some(shutdown),
                handle: Some(handle),
            }
        }
    }

    impl Drop for ProxyServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
        }
    }

    fn configure_proxy(ctx: &mut Ctx, backend: &GoPackageBackend, server: &ProxyServer) {
        ctx.config.sources.selection = crate::source::Selection::Ordered;
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                disable: vec!["proxy.golang.org".into(), "goproxy.cn".into()],
                custom: vec![Source::mirror("fixture", &server.base_url, 0)],
                ..Default::default()
            },
        );
    }

    #[test]
    fn factory_and_proxy_helpers_are_strict() {
        assert!(GoPackageBackend::from_id("go:example.com/acme/tool/cmd/tool").is_some());
        assert!(GoPackageBackend::from_id("go:Example.com/acme/tool").is_none());
        assert_eq!(
            module_path_candidates("example.com/acme/tool/cmd/tool"),
            vec![
                "example.com/acme/tool/cmd/tool",
                "example.com/acme/tool/cmd",
                "example.com/acme/tool",
                "example.com/acme",
            ]
        );
        assert!(validate_go_proxy("https://user:secret@example.com").is_err());
        assert!(validate_go_proxy("http://example.com").is_err());
        assert!(crate::tool::is_canonical_go_module_version("1.2.3"));
        assert!(crate::tool::is_canonical_go_module_version("1.2.3-beta.1"));
        assert!(crate::tool::is_canonical_go_module_version(
            "1.2.3+incompatible"
        ));
        assert!(crate::tool::is_canonical_go_module_version(
            "0.0.0-20240801123456-0123456789ab"
        ));
        assert!(crate::tool::is_canonical_go_module_version(
            "1.2.4-0.20240801123456-0123456789ab"
        ));
        assert!(crate::tool::is_canonical_go_module_version(
            "1.2.3-beta.0.20240801123456-0123456789ab"
        ));
        assert!(crate::tool::is_canonical_go_module_version(
            "2.0.0-20240801123456-0123456789ab+incompatible"
        ));
        assert!(crate::tool::is_canonical_go_module_version(
            "1.2.3-arbitrary"
        ));
        assert!(!crate::tool::is_canonical_go_module_version(
            "1.2.3+metadata"
        ));
        assert!(!crate::tool::is_canonical_go_module_version(
            "0.0.0-2024080112345-0123456789ab"
        ));
        assert_eq!(escape_go_token("v1.2.3-RC1"), "v1.2.3-!r!c1");
        assert_eq!(go_provider_version("1.2.3"), "v1.2.3");
        assert_eq!(
            go_provider_version("1.2.3+incompatible"),
            "v1.2.3+incompatible"
        );
        assert_eq!(
            proxy_info_url(
                &Source::official("fixture", "https://proxy.example.test"),
                "example.com/Acme/Tool",
                "1.2.3+incompatible",
            ),
            "https://proxy.example.test/example.com/!acme/!tool/@v/v1.2.3+incompatible.info"
        );
    }

    #[tokio::test]
    async fn exact_resolution_requires_proxy_evidence_and_discovers_nested_module_root() {
        let server = ProxyServer::start(vec![
            (
                "/example.com/acme/tool/cmd/tool/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/example.com/acme/tool/cmd/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/example.com/acme/tool/@v/v1.2.3.info".into(),
                "200 OK".into(),
                r#"{"Version":"v1.2.3","Time":"2026-01-01T00:00:00Z"}"#.into(),
            ),
        ]);
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), false);
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool/cmd/tool").unwrap();
        configure_proxy(&mut ctx, &backend, &server);

        let resolved = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool/cmd/tool@1.2.3").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resolved.options[LOCKED_GO_PROXY_OPTION], server.base_url);
        assert_eq!(
            resolved.options[LOCKED_GO_MODULE_OPTION],
            "example.com/acme/tool"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 3);

        let missing = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool/cmd/tool@9.9.9").unwrap(),
            )
            .await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn exact_resolution_falls_back_to_later_live_proxy_before_stale_cache() {
        let server = ProxyServer::start(vec![
            (
                "/first/example.com/acme/tool/@v/v1.2.3.info".into(),
                "503 Service Unavailable".into(),
                String::new(),
            ),
            (
                "/second/example.com/acme/tool/@v/v1.2.3.info".into(),
                "200 OK".into(),
                r#"{"Version":"v1.2.3","Time":"2026-01-01T00:00:00Z"}"#.into(),
            ),
        ]);
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), false);
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        ctx.config.sources.selection = crate::source::Selection::Ordered;
        let first = Source::mirror("first", &format!("{}/first", server.base_url), 0);
        let second = Source::mirror("second", &format!("{}/second", server.base_url), 10);
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                disable: vec!["proxy.golang.org".into(), "goproxy.cn".into()],
                custom: vec![first.clone(), second.clone()],
                ..Default::default()
            },
        );
        let stale_url = proxy_info_url(&first, "example.com/acme/tool", "1.2.3");
        let stale_cache =
            crate::http::source_metadata_cache_path(&ctx, &first, &stale_url).unwrap();
        std::fs::create_dir_all(stale_cache.parent().unwrap()).unwrap();
        std::fs::write(
            stale_cache,
            br#"{"Version":"v1.2.3","Time":"2025-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let resolved = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resolved.options[LOCKED_GO_PROXY_OPTION],
            format!("{}/second", server.base_url)
        );
        assert_eq!(
            server.requests.lock().unwrap().last().unwrap(),
            "/second/example.com/acme/tool/@v/v1.2.3.info"
        );
    }

    #[tokio::test]
    async fn longest_module_candidate_wins_across_source_priorities() {
        let server = ProxyServer::start(vec![
            (
                "/first/example.com/acme/tool/cmd/x/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/second/example.com/acme/tool/cmd/x/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/first/example.com/acme/tool/cmd/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/second/example.com/acme/tool/cmd/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/first/example.com/acme/tool/@v/v1.2.3.info".into(),
                "404 Not Found".into(),
                String::new(),
            ),
            (
                "/second/example.com/acme/tool/@v/v1.2.3.info".into(),
                "200 OK".into(),
                r#"{"Version":"v1.2.3","Time":"2026-01-01T00:00:00Z"}"#.into(),
            ),
            (
                "/first/example.com/acme/@v/v1.2.3.info".into(),
                "200 OK".into(),
                r#"{"Version":"v1.2.3","Time":"2026-01-01T00:00:00Z"}"#.into(),
            ),
        ]);
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), false);
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool/cmd/x").unwrap();
        ctx.config.sources.selection = crate::source::Selection::Ordered;
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                disable: vec!["proxy.golang.org".into(), "goproxy.cn".into()],
                custom: vec![
                    Source::mirror("first", &format!("{}/first", server.base_url), 0),
                    Source::mirror("second", &format!("{}/second", server.base_url), 10),
                ],
                ..Default::default()
            },
        );
        let resolved = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool/cmd/x@1.2.3").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resolved.options[LOCKED_GO_MODULE_OPTION],
            "example.com/acme/tool"
        );
        assert_eq!(
            resolved.options[LOCKED_GO_PROXY_OPTION],
            format!("{}/second", server.base_url)
        );
    }

    #[tokio::test]
    async fn proxy_sources_with_custom_headers_are_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), false);
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        let mut source = Source::official("private", "https://proxy.example.test");
        source
            .headers
            .push(("Authorization".into(), "secret".into()));
        ctx.config.sources.selection = crate::source::Selection::Ordered;
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                disable: vec!["proxy.golang.org".into(), "goproxy.cn".into()],
                custom: vec![source],
                ..Default::default()
            },
        );
        let error = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("custom HTTP headers"));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn unpinned_custom_proxies_exclude_public_defaults_before_probing() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), true);
        let backend = GoPackageBackend::from_id("go:private.example/acme/tool").unwrap();
        ctx.config.sources.selection = crate::source::Selection::Auto;
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                custom: vec![Source::mirror(
                    "private",
                    "https://proxy.private.example",
                    0,
                )],
                ..Default::default()
            },
        );

        let sources = backend.ranked_proxy_sources(&ctx).await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, "private");
    }

    #[tokio::test]
    async fn latest_and_prefix_resolve_through_proxy_list() {
        let server = ProxyServer::start(vec![
            (
                "/example.com/acme/tool/@latest".into(),
                "200 OK".into(),
                r#"{"Version":"v2.0.0","Time":"2026-01-01T00:00:00Z"}"#.into(),
            ),
            (
                "/example.com/acme/tool/@v/list".into(),
                "200 OK".into(),
                "v1.2.1\nv1.2.4-beta.1\nv2.0.0\nv1.2.3\n".into(),
            ),
        ]);
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = context(temporary.path(), false);
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        configure_proxy(&mut ctx, &backend, &server);

        let latest = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool@latest").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(latest.version, "2.0.0");
        let prefix = backend
            .resolve_version(
                &ctx,
                &ToolRequest::parse("go:example.com/acme/tool@1.2").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prefix.version, "1.2.3");
    }

    #[test]
    fn proxy_version_lists_use_semver_order_and_mark_prereleases() {
        let versions = parse_proxy_versions(
            b"v1.9.0\nv1.10.0\nv1.11.0-beta.1\nv0.0.0-20240801123456-0123456789ab\n",
        )
        .unwrap();

        assert_eq!(
            versions
                .iter()
                .map(|version| version.version.as_str())
                .collect::<Vec<_>>(),
            [
                "0.0.0-20240801123456-0123456789ab",
                "1.9.0",
                "1.10.0",
                "1.11.0-beta.1",
            ]
        );
        assert!(!versions.last().unwrap().stable);
    }

    #[tokio::test]
    async fn provider_failure_and_reserved_metadata_never_publish() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path(), false);
        managed_go(&ctx, "1.24.1");
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        let version = selected(&backend, "1.24.1");
        let failed = FixtureRunner::new([1]);
        assert!(backend
            .install_with_runner(&ctx, &version, &failed)
            .await
            .is_err());
        assert!(!backend
            .lifecycle(&ctx, &version)
            .unwrap()
            .install_root()
            .exists());

        let mut forged = FixtureRunner::new([0]);
        forged.forge_resolution = true;
        let error = backend
            .install_with_runner(&ctx, &version, &forged)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reserved metadata path"));
        assert!(!backend
            .lifecycle(&ctx, &version)
            .unwrap()
            .install_root()
            .exists());
    }

    #[tokio::test]
    async fn uninstall_removes_only_selected_go_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path(), false);
        managed_go(&ctx, "1.24.1");
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        let first = selected(&backend, "1.24.1");
        let mut second = first.clone();
        second.options.insert("tags".into(), "netgo".into());
        backend
            .install_with_runner(&ctx, &first, &FixtureRunner::new([0]))
            .await
            .unwrap();
        backend
            .install_with_runner(&ctx, &second, &FixtureRunner::new([0]))
            .await
            .unwrap();
        let first_root = backend
            .lifecycle(&ctx, &first)
            .unwrap()
            .install_root()
            .to_path_buf();
        let second_root = backend
            .lifecycle(&ctx, &second)
            .unwrap()
            .install_root()
            .to_path_buf();
        assert_ne!(first_root, second_root);
        backend.uninstall(&ctx, &first).await.unwrap();
        assert!(!first_root.exists());
        assert!(second_root.exists());
    }

    #[tokio::test]
    async fn provider_uses_exact_managed_go_and_private_environment_then_reuses_offline() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path(), false);
        managed_go(&ctx, "1.24.1");
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        let mut version = selected(&backend, "1.24.1");
        version.options.insert("tags".into(), "netgo,sqlite".into());
        version
            .options
            .insert("env".into(), "CGO_ENABLED=0;GOAMD64=v3".into());
        let runner = FixtureRunner::new([0]);
        let result = backend.install_with_runner(&ctx, &version, &runner).await;
        result.unwrap();
        {
            let calls = runner.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            let call = &calls[0];
            assert!(call.environment_is_cleared());
            let expected_go = ctx
                .dirs
                .install_path("go", "1.24.1")
                .join("bin")
                .join(format!("go{}", ctx.platform.os.exe_suffix()));
            assert!(
                same_file::is_same_file(call.program(), &expected_go).unwrap_or(false),
                "program={} expected={}",
                call.program().display(),
                expected_go.display()
            );
            assert_eq!(
                call.arguments(),
                [
                    OsString::from("install"),
                    OsString::from("-tags"),
                    OsString::from("netgo,sqlite"),
                    OsString::from("example.com/acme/tool@v1.2.3"),
                ]
            );
            for name in [
                "HOME",
                "USERPROFILE",
                "GOROOT",
                "GOBIN",
                "GOPATH",
                "GOMODCACHE",
                "GOCACHE",
                "GOENV",
                "GOTOOLCHAIN",
                "GOPROXY",
                "GOSUMDB",
                "GONOSUMDB",
                "GONOPROXY",
                "PATH",
                "TMPDIR",
                "TEMP",
                "TMP",
                "CGO_ENABLED",
                "GOAMD64",
            ] {
                assert!(call.environment().contains_key(OsStr::new(name)), "{name}");
            }
            assert_eq!(call.environment()[OsStr::new("GOTOOLCHAIN")], "local");
            assert_eq!(call.environment()[OsStr::new("GOSUMDB")], "off");
            assert_eq!(call.environment()[OsStr::new("GONOPROXY")], "none");
            assert_eq!(
                call.environment()[OsStr::new("GOMODCACHE")],
                crate::cache::downstream_root(&ctx.dirs.cache).join("go-mod")
            );
            assert!(!call.environment().contains_key(OsStr::new("GOFLAGS")));
        }

        let offline = context(temporary.path(), true);
        backend
            .install_with_runner(&offline, &version, &FixtureRunner::new([]))
            .await
            .unwrap();
        let other = GoPackageBackend::from_id("go:example.com/acme/other").unwrap();
        let error = other
            .install_with_runner(
                &offline,
                &selected(&other, "1.24.1"),
                &FixtureRunner::new([]),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline Go install"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_go_accepts_cas_link_and_rejects_external_link() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path(), false);
        managed_go(&ctx, "1.24.1");
        let backend = GoPackageBackend::from_id("go:example.com/acme/tool").unwrap();
        let version = selected(&backend, "1.24.1");
        let go = ctx.dirs.install_path("go", "1.24.1").join("bin").join("go");
        let cas = ctx.dirs.store.join("aa/bb/go");
        std::fs::create_dir_all(cas.parent().unwrap()).unwrap();
        std::fs::write(&cas, b"go").unwrap();
        std::fs::remove_file(&go).unwrap();
        symlink(&cas, &go).unwrap();
        assert_eq!(backend.managed_go(&ctx, &version.options).unwrap().1, cas);

        let outside = temporary.path().join("outside-go");
        std::fs::write(&outside, b"go").unwrap();
        std::fs::remove_file(&go).unwrap();
        symlink(&outside, &go).unwrap();
        assert!(backend.managed_go(&ctx, &version.options).is_err());
    }
}
