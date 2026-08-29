//! Cargo package tools installed with an exact osdk-managed Rust toolchain.
//!
//! Registry packages and HTTPS Git repositories are addressed by the dynamic
//! `cargo:` namespace. Provider commands receive absolute managed tool paths
//! and a private environment, write only to a lifecycle-owned stage, and are
//! published through [`NativeToolLifecycle`].

use std::collections::BTreeMap;
use std::ffi::OsString;
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

const CRATES_IO_API: &str = "https://crates.io/api/v1/crates";
const CARGO_METADATA_LIMIT: usize = 8 * 1024 * 1024;
const CARGO_METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const CARGO_RESOLUTION_FILE: &str = "cargo-resolution.json";
const CARGO_RESOLUTION_SCHEMA: u32 = 1;
const PROVIDER_OUTPUT_LIMIT: usize = 1024 * 1024;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const LOCKED_CARGO_INDEX_OPTION: &str = "__osdk_cargo_index";
static NEXT_METADATA_TEMPORARY: AtomicU64 = AtomicU64::new(0);

pub fn validate_registry_index(value: &str) -> Result<()> {
    let parsed = value
        .strip_prefix("sparse+")
        .ok_or_else(|| Error::config("Cargo registry source must use sparse HTTPS"))?;
    let url = reqwest::Url::parse(parsed)
        .map_err(|_| Error::config("Cargo registry source is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !value.ends_with('/')
        || format!("sparse+{}", url.as_str()) != value
    {
        return Err(Error::config(
            "Cargo registry source must be canonical sparse HTTPS",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CargoSource {
    Registry { package: String },
    Git { url: String },
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    versions: Vec<CratesVersion>,
}

#[derive(Debug)]
struct CargoRegistrySelection {
    source: Source,
    metadata: CratesResponse,
}

fn version_infos(metadata: CratesResponse) -> Vec<VersionInfo> {
    let mut versions = metadata
        .versions
        .into_iter()
        .filter(|version| !version.yanked)
        .map(|version| VersionInfo {
            stable: semver::Version::parse(&version.num)
                .is_ok_and(|version| version.pre.is_empty()),
            version: version.num,
            lts: None,
        })
        .collect::<Vec<_>>();
    versions
        .sort_by(|left, right| crate::backend::python::cmp_versions(&left.version, &right.version));
    versions.dedup_by(|left, right| left.version == right.version);
    versions
}

fn registry_index(source: &Source) -> Result<String> {
    let index = source.index_url.clone().ok_or_else(|| {
        Error::config(format!(
            "Cargo source `{}` requires a sparse HTTPS index URL",
            source.id
        ))
    })?;
    validate_registry_index(&index)?;
    Ok(index)
}

#[derive(Debug, Deserialize)]
struct CratesVersion {
    num: String,
    #[serde(default)]
    yanked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoResolution {
    schema: u32,
    backend: String,
    version: String,
    source_kind: String,
    source: String,
    replay: String,
}

/// Backend bound to one registry crate or canonical HTTPS Git repository.
pub struct CargoPackageBackend {
    id: String,
    source: CargoSource,
}

impl CargoPackageBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let id = ToolId::parse(id).ok()?;
        if id.namespace() != Some("cargo") {
            return None;
        }
        let subject = id.subject().to_string();
        let source = if subject.starts_with("https://") {
            CargoSource::Git {
                url: subject.clone(),
            }
        } else {
            CargoSource::Registry {
                package: subject.clone(),
            }
        };
        Some(Self {
            id: id.to_string(),
            source,
        })
    }

    fn runtime_version<'a>(&self, options: &'a BTreeMap<String, String>) -> Result<&'a str> {
        match (
            options.get(LOCKED_NATIVE_RUNTIME_OPTION),
            options.get(LOCKED_NATIVE_RUNTIME_VERSION_OPTION),
        ) {
            (Some(runtime), Some(version)) if runtime == "rust" && !version.is_empty() => {
                Ok(version)
            }
            (Some(runtime), _) if runtime != "rust" => Err(Error::config(format!(
                "Cargo tool `{}` requires managed runtime `rust`, got `{runtime}`",
                self.id
            ))),
            _ => Err(Error::config(format!(
                "Cargo tool `{}` requires an exact managed Rust selection; add an exact `rust@<version>` request or configuration",
                self.id
            ))),
        }
    }

    fn runtime_dependency(
        &self,
        ctx: &Ctx,
        options: &BTreeMap<String, String>,
    ) -> Result<InstallDependency> {
        let version = self.runtime_version(options)?;
        let marker = ctx.dirs.install_path("rust", version);
        if !marker.join(".osdk-complete").is_file() {
            return Err(Error::NotInstalled {
                tool: "rust".into(),
                version: version.into(),
            });
        }
        if marker.join(".osdk-linked").exists() {
            return Err(Error::config(
                "Cargo tools require an osdk-managed Rust toolchain; linked Rust toolchains are not reproducible",
            ));
        }
        Ok(InstallDependency {
            kind: InstallDependencyKind::Runtime,
            id: "rust".into(),
            version: version.into(),
            identity: Some(native_tool::rust_runtime_identity(
                &ctx.dirs,
                ctx.platform,
                version,
            )?),
        })
    }

    fn materials(&self, _ctx: &Ctx, tv: &ToolVersion) -> BTreeMap<String, String> {
        match &self.source {
            CargoSource::Registry { package } => BTreeMap::from([
                ("source-kind".into(), "registry".into()),
                ("package".into(), package.clone()),
                (
                    "registry-index".into(),
                    tv.options
                        .get(LOCKED_CARGO_INDEX_OPTION)
                        .cloned()
                        .unwrap_or_else(|| "sparse+https://index.crates.io/".into()),
                ),
            ]),
            CargoSource::Git { url } => BTreeMap::from([
                ("source-kind".into(), "git".into()),
                ("git-url".into(), url.clone()),
                ("git-selector".into(), tv.version.clone()),
            ]),
        }
    }

    fn materials_match(&self, tv: &ToolVersion, actual: &BTreeMap<String, String>) -> bool {
        match &self.source {
            CargoSource::Git { url } => {
                actual
                    == &BTreeMap::from([
                        ("source-kind".into(), "git".into()),
                        ("git-url".into(), url.clone()),
                        ("git-selector".into(), tv.version.clone()),
                    ])
            }
            CargoSource::Registry { package } => {
                if actual.get("source-kind").map(String::as_str) != Some("registry")
                    || actual.get("package").map(String::as_str) != Some(package.as_str())
                    || actual.len() != 3
                {
                    return false;
                }
                let Some(index) = actual.get("registry-index") else {
                    return false;
                };
                if validate_registry_index(index).is_err() {
                    return false;
                }
                tv.options
                    .get(LOCKED_CARGO_INDEX_OPTION)
                    .is_none_or(|expected| expected == index)
            }
        }
    }

    fn lifecycle(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<NativeToolLifecycle> {
        let runtime = self.runtime_dependency(ctx, &tv.options)?;
        let materials = self.materials(ctx, tv);
        NativeToolLifecycle::new(
            &ctx.dirs,
            ctx.platform,
            &self.id,
            &tv.version,
            &tv.options,
            NativeToolFamily::Cargo,
            runtime,
            materials,
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
            && tv.options.contains_key(LOCKED_CARGO_INDEX_OPTION)
        {
            return self.lifecycle(ctx, tv).map(Some);
        }
        let expected_options = crate::backend::dynamic::identity_options(&self.id, &tv.options)?;
        let expected_materials = self.materials(ctx, tv);
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
                && (tv.options.contains_key(LOCKED_CARGO_INDEX_OPTION)
                    && identity.materials == expected_materials
                    || !tv.options.contains_key(LOCKED_CARGO_INDEX_OPTION)
                        && self.materials_match(tv, &identity.materials))
                && native_tool::validate_install_candidate(
                    &ctx.dirs,
                    NativeToolFamily::Cargo,
                    &install.install_root,
                    identity,
                )
                .unwrap_or(false)
        });
        let first = matching.next();
        if matching.next().is_some() {
            return Err(Error::other(format!(
                "Cargo tool `{}@{}` has multiple matching managed Rust identities; select it through a lockfile",
                self.id, tv.version
            )));
        }
        first
            .map(|install| {
                NativeToolLifecycle::from_identity(
                    &ctx.dirs,
                    NativeToolFamily::Cargo,
                    install.manifest.identity,
                )
            })
            .transpose()
    }

    fn replay(&self, version: &str) -> &'static str {
        match &self.source {
            CargoSource::Registry { .. } => "version-only",
            CargoSource::Git { .. } if full_revision(version).is_some() => "immutable-revision",
            CargoSource::Git { .. } => "floating-ref",
        }
    }

    async fn registry_selection(&self, ctx: &Ctx, package: &str) -> Result<CargoRegistrySelection> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let tried = sources.len();
        let mut last_error = None;
        if !ctx.config.settings.offline {
            for source in &sources {
                if let Err(error) = registry_index(source) {
                    last_error = Some(error);
                    continue;
                }
                let url = crate::http::join_url(&source.download_url, package);
                match fetch_live_crate_metadata(ctx, source, &url).await {
                    Ok(metadata) => {
                        return Ok(CargoRegistrySelection {
                            source: source.clone(),
                            metadata,
                        });
                    }
                    Err(error) => last_error = Some(error),
                }
            }
        }

        for source in sources {
            if let Err(error) = registry_index(&source) {
                if ctx.config.settings.offline {
                    last_error = Some(error);
                }
                continue;
            }
            let url = crate::http::join_url(&source.download_url, package);
            match read_source_cached_crate_metadata(ctx, &source, &url) {
                Ok(metadata) => {
                    tracing::warn!(
                        source = %source.id,
                        url,
                        "using stale cached Cargo registry metadata after all live sources failed"
                    );
                    return Ok(CargoRegistrySelection { source, metadata });
                }
                Err(_) if !ctx.config.settings.offline => {}
                Err(_) => {
                    last_error = Some(Error::other(format!(
                        "offline Cargo metadata cache miss for {url}"
                    )));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id.clone(),
            tried,
        }))
    }

    fn resolution(&self, tv: &ToolVersion) -> CargoResolution {
        let (source_kind, source) = match &self.source {
            CargoSource::Registry { package } => (
                "registry",
                tv.options
                    .get(LOCKED_CARGO_INDEX_OPTION)
                    .cloned()
                    .unwrap_or_else(|| format!("crates.io:{package}")),
            ),
            CargoSource::Git { url } => ("git", url.clone()),
        };
        CargoResolution {
            schema: CARGO_RESOLUTION_SCHEMA,
            backend: self.id.clone(),
            version: tv.version.clone(),
            source_kind: source_kind.into(),
            source,
            replay: self.replay(&tv.version).into(),
        }
    }

    fn resolution_matches(&self, tv: &ToolVersion, actual: &CargoResolution) -> bool {
        if actual.schema != CARGO_RESOLUTION_SCHEMA
            || actual.backend != self.id
            || actual.version != tv.version
            || actual.replay != self.replay(&tv.version)
        {
            return false;
        }
        match &self.source {
            CargoSource::Git { url } => actual.source_kind == "git" && actual.source == *url,
            CargoSource::Registry { .. } => {
                actual.source_kind == "registry"
                    && validate_registry_index(&actual.source).is_ok()
                    && tv
                        .options
                        .get(LOCKED_CARGO_INDEX_OPTION)
                        .is_none_or(|expected| expected == &actual.source)
            }
        }
    }

    fn toolchain_bins(
        &self,
        ctx: &Ctx,
        options: &BTreeMap<String, String>,
    ) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let version = self.runtime_version(options)?;
        let root = crate::backend::rust::RustBackend::exact_toolchain_dir_for_dirs(
            &ctx.dirs,
            ctx.platform,
            version,
        )
        .ok_or_else(|| Error::NotInstalled {
            tool: "rust".into(),
            version: version.into(),
        })?;
        let bin = root.join("bin");
        let cargo = bin.join(format!("cargo{}", ctx.platform.os.exe_suffix()));
        let rustc = bin.join(format!("rustc{}", ctx.platform.os.exe_suffix()));
        if !regular_file(&cargo) || !regular_file(&rustc) {
            return Err(Error::other(format!(
                "managed Rust toolchain `{version}` is missing Cargo or rustc"
            )));
        }
        Ok((bin, cargo, rustc))
    }

    fn command_env(
        &self,
        ctx: &Ctx,
        stage: &Path,
        toolchain_bin: &Path,
        rustc: &Path,
    ) -> Result<BTreeMap<OsString, OsString>> {
        let home = stage.join("home");
        let cargo_home = stage.join("cargo-home");
        let target = stage.join("target");
        let tmp = stage.join("tmp");
        for path in [&home, &cargo_home, &target, &tmp] {
            std::fs::create_dir_all(path).map_err(|error| Error::io(path, error))?;
        }
        let path = sanitized_provider_path(ctx, toolchain_bin, std::env::var_os("PATH"))?;
        Ok(BTreeMap::from([
            (OsString::from("HOME"), home.into_os_string()),
            (
                OsString::from("USERPROFILE"),
                stage.join("home").into_os_string(),
            ),
            (OsString::from("CARGO_HOME"), cargo_home.into_os_string()),
            (OsString::from("CARGO_TARGET_DIR"), target.into_os_string()),
            (
                OsString::from("CARGO_INSTALL_ROOT"),
                stage.as_os_str().to_owned(),
            ),
            (
                OsString::from("RUSTUP_HOME"),
                ctx.dirs.rustup_home().into_os_string(),
            ),
            (OsString::from("RUSTC"), rustc.as_os_str().to_owned()),
            (OsString::from("PATH"), path),
            (OsString::from("TMPDIR"), tmp.into_os_string()),
            (OsString::from("TEMP"), stage.join("tmp").into_os_string()),
            (OsString::from("TMP"), stage.join("tmp").into_os_string()),
            (OsString::from("CARGO_TERM_COLOR"), OsString::from("never")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ]))
    }

    fn cargo_install_args(&self, tv: &ToolVersion, stage: &Path) -> Result<Vec<OsString>> {
        let mut args = vec![
            OsString::from("install"),
            OsString::from("--root"),
            stage.as_os_str().to_owned(),
            OsString::from("--no-track"),
        ];
        match &self.source {
            CargoSource::Registry { package } => {
                args.push(OsString::from("--version"));
                args.push(OsString::from(format!("={}", tv.version)));
                args.push(OsString::from(package));
            }
            CargoSource::Git { url } => {
                args.push(OsString::from("--git"));
                args.push(OsString::from(url));
                match git_selector(&tv.version)? {
                    GitSelector::Head => {}
                    GitSelector::Tag(value) => {
                        args.push(OsString::from("--tag"));
                        args.push(OsString::from(value));
                    }
                    GitSelector::Branch(value) => {
                        args.push(OsString::from("--branch"));
                        args.push(OsString::from(value));
                    }
                    GitSelector::Revision(value) => {
                        args.push(OsString::from("--rev"));
                        args.push(OsString::from(value));
                    }
                }
                if let Some(package) = tv.options.get("crate") {
                    args.push(OsString::from(package));
                }
            }
        }
        append_build_options(&mut args, &tv.options);
        if let Some(index) = tv.options.get(LOCKED_CARGO_INDEX_OPTION) {
            args.push(OsString::from("--index"));
            args.push(OsString::from(index));
        }
        Ok(args)
    }

    fn binstall_args(&self, tv: &ToolVersion, stage: &Path) -> Vec<OsString> {
        let CargoSource::Registry { package } = &self.source else {
            unreachable!("Git sources are never binstall eligible");
        };
        let mut args = vec![
            OsString::from("--no-confirm"),
            OsString::from("--disable-telemetry"),
            OsString::from("--no-discover-github-token"),
            OsString::from("--disable-strategies"),
            OsString::from("compile,quick-install"),
            OsString::from("--no-track"),
            OsString::from("--root"),
            stage.as_os_str().to_owned(),
            OsString::from("--version"),
            OsString::from(format!("={}", tv.version)),
        ];
        if let Some(bin) = tv.options.get("bin") {
            args.push(OsString::from("--bin"));
            args.push(OsString::from(bin));
        }
        if option_enabled(&tv.options, "locked") {
            args.push(OsString::from("--locked"));
        }
        if let Some(index) = tv.options.get(LOCKED_CARGO_INDEX_OPTION) {
            args.push(OsString::from("--index"));
            args.push(OsString::from(index));
        }
        args.push(OsString::from(package));
        args
    }

    fn binstall_eligible(&self, ctx: &Ctx, tv: &ToolVersion) -> bool {
        if ctx.config.settings.offline
            || !matches!(self.source, CargoSource::Registry { .. })
            || matches!(&self.source, CargoSource::Registry { package } if package == "cargo-binstall")
            || tv.options.contains_key("features")
            || tv.options.get("default-features").map(String::as_str) == Some("false")
        {
            return false;
        }
        controlled_binstall(ctx).is_some()
    }

    fn run_provider(
        &self,
        runner: &dyn CommandRunner,
        program: &Path,
        args: Vec<OsString>,
        env: &BTreeMap<OsString, OsString>,
        cwd: &Path,
        provider: &str,
    ) -> ProviderStatus {
        let command = CommandSpec::new(program.as_os_str().to_owned())
            .args(args)
            .envs(env.clone())
            .current_dir(cwd)
            .clear_env();
        match runner.run_captured(
            &command,
            CaptureLimits::new(
                PROVIDER_TIMEOUT,
                PROVIDER_OUTPUT_LIMIT,
                PROVIDER_OUTPUT_LIMIT,
            ),
        ) {
            CommandOutcome::Exited { status, output: _ } if status.success() => {
                ProviderStatus::Success
            }
            CommandOutcome::Exited { status, output } => ProviderStatus::Exit {
                code: status.code(),
                error: provider_error(provider, status.to_string(), &output.stderr),
            },
            outcome => ProviderStatus::Failure(outcome_error(provider, &outcome)),
        }
    }

    async fn install_with_runner(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        runner: &dyn CommandRunner,
    ) -> Result<()> {
        if ctx.config.settings.offline
            && matches!(self.source, CargoSource::Registry { .. })
            && !tv.options.contains_key(LOCKED_CARGO_INDEX_OPTION)
        {
            if self.selected_lifecycle(ctx, tv)?.is_some() {
                return Ok(());
            }
            return Err(Error::other(format!(
                "offline Cargo install requires an already complete matching install for `{}`; Cargo native locks do not contain a complete source graph",
                self.id
            )));
        }
        let lifecycle = self.lifecycle(ctx, tv)?;
        let NativeToolPreparation::Staged(mut stage) = lifecycle.prepare(&ctx.dirs).await? else {
            return Ok(());
        };
        if ctx.config.settings.offline {
            return Err(Error::other(format!(
                "offline Cargo install requires an already complete matching install for `{}`; Cargo native locks do not contain a complete source graph",
                self.id
            )));
        }
        let (toolchain_bin, _cargo, rustc) = self.toolchain_bins(ctx, &tv.options)?;
        let stage_root = stage.path().to_path_buf();
        let env = self.command_env(ctx, &stage_root, &toolchain_bin, &rustc)?;

        if self.binstall_eligible(ctx, tv) {
            let binstall = controlled_binstall(ctx).expect("eligibility checked");
            match self.run_provider(
                runner,
                &binstall,
                self.binstall_args(tv, &stage_root),
                &env,
                &stage_root,
                "cargo-binstall",
            ) {
                ProviderStatus::Success => {
                    clean_provider_workspace(&stage_root)?;
                    write_resolution(&stage_root, &self.resolution(tv))?;
                    stage.publish(NativeToolProvider::CargoBinstall)?;
                    return Ok(());
                }
                ProviderStatus::Exit { code: Some(94), .. } => stage.reset()?,
                ProviderStatus::Exit { error, .. } | ProviderStatus::Failure(error) => {
                    return Err(error);
                }
            }
        }

        let (toolchain_bin, cargo, rustc) = self.toolchain_bins(ctx, &tv.options)?;
        let env = self.command_env(ctx, stage.path(), &toolchain_bin, &rustc)?;
        match self.run_provider(
            runner,
            &cargo,
            self.cargo_install_args(tv, stage.path())?,
            &env,
            stage.path(),
            "cargo install",
        ) {
            ProviderStatus::Success => {}
            ProviderStatus::Exit { error, .. } | ProviderStatus::Failure(error) => {
                return Err(error)
            }
        }
        clean_provider_workspace(stage.path())?;
        write_resolution(stage.path(), &self.resolution(tv))?;
        stage.publish(NativeToolProvider::CargoInstall)?;
        Ok(())
    }
}

enum ProviderStatus {
    Success,
    Exit { code: Option<i32>, error: Error },
    Failure(Error),
}

enum GitSelector<'a> {
    Head,
    Tag(&'a str),
    Branch(&'a str),
    Revision(&'a str),
}

fn git_selector(version: &str) -> Result<GitSelector<'_>> {
    if version == "latest" {
        Ok(GitSelector::Head)
    } else if let Some(value) = version.strip_prefix("tag:") {
        Ok(GitSelector::Tag(value))
    } else if let Some(value) = version.strip_prefix("branch:") {
        Ok(GitSelector::Branch(value))
    } else if let Some(value) = full_revision(version) {
        Ok(GitSelector::Revision(value))
    } else {
        Err(Error::config(format!(
            "unsupported Cargo Git selector `{version}`"
        )))
    }
}

fn full_revision(version: &str) -> Option<&str> {
    let value = version.strip_prefix("rev:")?;
    (value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    .then_some(value)
}

fn append_build_options(args: &mut Vec<OsString>, options: &BTreeMap<String, String>) {
    if let Some(features) = options.get("features") {
        args.push(OsString::from("--features"));
        args.push(OsString::from(features));
    }
    if options.get("default-features").map(String::as_str) == Some("false") {
        args.push(OsString::from("--no-default-features"));
    }
    if let Some(bin) = options.get("bin") {
        args.push(OsString::from("--bin"));
        args.push(OsString::from(bin));
    }
    if option_enabled(options, "locked") {
        args.push(OsString::from("--locked"));
    }
}

fn option_enabled(options: &BTreeMap<String, String>, name: &str) -> bool {
    options.get(name).map(String::as_str) == Some("true")
}

fn sanitized_provider_path(
    ctx: &Ctx,
    toolchain_bin: &Path,
    inherited: Option<OsString>,
) -> Result<OsString> {
    let cargo_bin = ctx.dirs.cargo_home().join("bin");
    let shims = ctx.dirs.shims();
    let mut paths = vec![toolchain_bin.to_path_buf()];
    if let Some(inherited) = inherited {
        for path in std::env::split_paths(&inherited) {
            if path.as_os_str().is_empty()
                || path == toolchain_bin
                || path == cargo_bin
                || path == shims
                || paths.iter().any(|existing| existing == &path)
            {
                continue;
            }
            paths.push(path);
        }
    }
    std::env::join_paths(paths)
        .map_err(|error| Error::config(format!("invalid sanitized provider PATH: {error}")))
}

fn controlled_binstall(ctx: &Ctx) -> Option<PathBuf> {
    let name = format!("cargo-binstall{}", ctx.platform.os.exe_suffix());
    let path = ctx.dirs.cargo_home().join("bin").join(name);
    regular_file(&path).then_some(path)
}

fn clean_provider_workspace(stage: &Path) -> Result<()> {
    for name in ["home", "cargo-home", "target", "tmp"] {
        let path = stage.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::other(format!(
                    "Cargo provider workspace is unsafe: {}",
                    path.display()
                )));
            }
            Ok(_) => std::fs::remove_dir_all(&path).map_err(|error| Error::io(&path, error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&path, error)),
        }
    }
    let crates_metadata = stage.join(".crates.toml");
    let crates2_metadata = stage.join(".crates2.json");
    for path in [&crates_metadata, &crates2_metadata] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
            }
            Ok(_) => {
                return Err(Error::other(format!(
                    "Cargo provider metadata is unsafe: {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path, error)),
        }
    }
    Ok(())
}

fn write_resolution(root: &Path, resolution: &CargoResolution) -> Result<()> {
    let path = root.join(CARGO_RESOLUTION_FILE);
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
                "Cargo provider wrote reserved metadata path {}",
                path.display()
            ))
        } else {
            Error::io(&path, error)
        }
    })?;
    file.write_all(&bytes)
        .map_err(|error| Error::io(&path, error))
}

fn load_resolution(root: &Path) -> Result<CargoResolution> {
    let path = root.join(CARGO_RESOLUTION_FILE);
    let bytes = crate::inventory::read_stable_regular_file(&path, 64 * 1024)
        .map_err(|error| Error::io(&path, error))?;
    let resolution: CargoResolution = serde_json::from_slice(&bytes)?;
    if resolution.schema != CARGO_RESOLUTION_SCHEMA {
        return Err(Error::config("unsupported Cargo resolution schema"));
    }
    Ok(resolution)
}

fn provider_error(provider: &str, status: String, stderr: &[u8]) -> Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    Error::Command {
        cmd: provider.into(),
        status,
        stderr: (!stderr.is_empty()).then(|| stderr.to_string()),
    }
}

fn outcome_error(provider: &str, outcome: &CommandOutcome) -> Error {
    Error::other(format!("{provider} could not run: {outcome:?}"))
}

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

#[async_trait]
impl Backend for CargoPackageBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        match self.source {
            CargoSource::Registry { .. } => vec![
                Source::official("crates-io", CRATES_IO_API)
                    .with_index("sparse+https://index.crates.io/"),
                Source::mirror("rsproxy", "https://rsproxy.cn/api/v1/crates", 10)
                    .with_index("sparse+https://rsproxy.cn/index/"),
            ],
            CargoSource::Git { .. } => Vec::new(),
        }
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        match &self.source {
            CargoSource::Registry { package } => {
                Some(crate::http::join_url(&source.download_url, package))
            }
            CargoSource::Git { .. } => None,
        }
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let CargoSource::Registry { package } = &self.source else {
            return Ok(Vec::new());
        };
        let selection = self.registry_selection(ctx, package).await?;
        Ok(version_infos(selection.metadata))
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let id = ToolId::parse(&req.backend)?;
        crate::tool::validate_dynamic_selector(&id, Some(&req.spec.to_string()))?;
        crate::backend::dynamic::validate_options(&self.id, &req.options)?;
        let (version, selected_source) = match &self.source {
            CargoSource::Registry { package } => {
                let (version, source) = match &req.spec {
                    VersionSpec::Exact(version)
                        if req.options.contains_key(LOCKED_CARGO_INDEX_OPTION) =>
                    {
                        validate_registry_index(
                            req.options
                                .get(LOCKED_CARGO_INDEX_OPTION)
                                .expect("checked above"),
                        )?;
                        (version.clone(), None)
                    }
                    VersionSpec::Exact(version) if ctx.config.settings.offline => (
                        version.clone(),
                        Some(crate::source::select::active_source(ctx, self).await?),
                    ),
                    VersionSpec::Exact(version) => {
                        let selection = self.registry_selection(ctx, package).await?;
                        let versions = version_infos(selection.metadata);
                        let exact = crate::version::select_version(
                            &VersionSpec::Exact(version.clone()),
                            &versions,
                        )
                        .ok_or_else(|| Error::VersionResolve {
                            tool: self.id.clone(),
                            spec: req.spec.to_string(),
                            hint: Some("exact Cargo registry release is missing or yanked".into()),
                        })?
                        .version
                        .clone();
                        (exact, Some(selection.source))
                    }
                    VersionSpec::Latest | VersionSpec::Prefix(_) => {
                        let selection = self.registry_selection(ctx, package).await?;
                        let versions = version_infos(selection.metadata);
                        let version = crate::version::select_version(&req.spec, &versions)
                            .ok_or_else(|| Error::VersionResolve {
                                tool: self.id.clone(),
                                spec: req.spec.to_string(),
                                hint: Some("no matching non-yanked crates.io release found".into()),
                            })?
                            .version
                            .clone();
                        (version, Some(selection.source))
                    }
                    _ => {
                        return Err(Error::VersionResolve {
                            tool: self.id.clone(),
                            spec: req.spec.to_string(),
                            hint: Some("Cargo registry tools require latest, an exact version, or a numeric prefix".into()),
                        });
                    }
                };
                (version, source)
            }
            CargoSource::Git { .. } => match &req.spec {
                VersionSpec::Latest => ("latest".into(), None),
                VersionSpec::Prefix(selector) => (selector.clone(), None),
                VersionSpec::Exact(selector) => (selector.clone(), None),
                _ => {
                    return Err(Error::VersionResolve {
                        tool: self.id.clone(),
                        spec: req.spec.to_string(),
                        hint: Some(
                            "Cargo Git tools require tag:, branch:, or rev:<40 lowercase hex>"
                                .into(),
                        ),
                    });
                }
            },
        };
        let mut resolved = ToolVersion::new(&self.id, version);
        resolved.options = req.options.clone();
        if let Some(source) = selected_source {
            resolved
                .options
                .insert(LOCKED_CARGO_INDEX_OPTION.into(), registry_index(&source)?);
        }
        resolved.options.insert(
            LOCKED_NATIVE_REPLAY_OPTION.into(),
            self.replay(&resolved.version).into(),
        );
        Ok(resolved)
    }

    async fn install(&self, ctx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        self.install_with_runner(ctx.ctx, tv, &SystemCommandRunner)
            .await
    }

    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let Some(lifecycle) = self.selected_lifecycle(ctx, tv)? else {
            return Ok(());
        };
        lifecycle.uninstall().await?;
        Ok(())
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        native_tool::list_installed(&ctx.dirs, ctx.platform, NativeToolFamily::Cargo, &self.id)
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
        let receipt = native_tool::load_receipt(lifecycle.install_root())?;
        Ok(receipt
            .bins
            .into_iter()
            .map(|bin| {
                Path::new(&bin.path)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string()
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
        {
            return Ok(false);
        }
        if !self.resolution_matches(tv, &load_resolution(install_root)?) {
            return Ok(false);
        }
        NativeToolLifecycle::from_identity(&ctx.dirs, NativeToolFamily::Cargo, identity.clone())?
            .validate_dynamic_install(&ctx.dirs, install_root, identity)
    }
}

async fn fetch_live_crate_metadata(
    ctx: &Ctx,
    source: &Source,
    url: &str,
) -> Result<CratesResponse> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    let fresh = async {
        let response = crate::http::get_source_response(&ctx.client, source, url)
            .await?
            .error_for_status()
            .map_err(|error| Error::network(url, error))?;
        if response
            .content_length()
            .is_some_and(|size| size > CARGO_METADATA_LIMIT as u64)
        {
            return Err(Error::other(
                "Cargo registry metadata exceeds the 8 MiB limit",
            ));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Error::network(url, error))?;
            if bytes.len().saturating_add(chunk.len()) > CARGO_METADATA_LIMIT {
                return Err(Error::other(
                    "Cargo registry metadata exceeds the 8 MiB limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, Error>(bytes)
    };
    let bytes = match tokio::time::timeout(CARGO_METADATA_TIMEOUT, fresh).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(Error::other(
                "Cargo registry metadata exceeded the 30 second timeout",
            ))
        }
    };
    let parsed = serde_json::from_slice(&bytes)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let serial = NEXT_METADATA_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = cache.with_extension(format!("tmp-{}-{serial}", std::process::id()));
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &cache);
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(parsed)
}

fn read_source_cached_crate_metadata(
    ctx: &Ctx,
    source: &Source,
    url: &str,
) -> Result<CratesResponse> {
    let cache = crate::http::source_metadata_cache_path(ctx, source, url)?;
    read_cached_crate_metadata(&cache)
}

fn read_cached_crate_metadata(path: &Path) -> Result<CratesResponse> {
    let bytes = crate::inventory::read_stable_regular_file(path, CARGO_METADATA_LIMIT as u64)
        .map_err(|error| Error::io(path, error))?;
    Ok(serde_json::from_slice(&bytes)?)
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

    struct MetadataServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        shutdown: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MetadataServer {
        fn start(responses: Vec<(&'static str, &'static str, &'static str)>) -> Self {
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
                        let request = String::from_utf8(request).unwrap();
                        let path = request
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        server_requests.lock().unwrap().push(path.clone());
                        let (status, body) = responses
                            .iter()
                            .find(|(expected, _, _)| *expected == path)
                            .map(|(_, status, body)| (*status, *body))
                            .unwrap_or(("404 Not Found", ""));
                        write!(
                                stream,
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("metadata server failed: {error}"),
                }
            });
            Self {
                base_url,
                requests,
                shutdown: Some(shutdown),
                handle: Some(handle),
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MetadataServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
        }
    }

    fn configure_registry_sources(
        ctx: &mut Ctx,
        backend: &CargoPackageBackend,
        base_url: &str,
    ) -> (Source, Source) {
        let preferred = Source::mirror("preferred", &format!("{base_url}/preferred"), 0)
            .with_index("sparse+https://preferred.example.test/index/");
        let fallback = Source::mirror("fallback", &format!("{base_url}/fallback"), 10)
            .with_index("sparse+https://fallback.example.test/index/");
        ctx.config.sources.selection = crate::source::Selection::Ordered;
        ctx.config.sources.per_tool.insert(
            backend.id().into(),
            crate::config::ToolSources {
                disable: vec!["crates-io".into(), "rsproxy".into()],
                custom: vec![preferred.clone(), fallback.clone()],
                ..Default::default()
            },
        );
        (preferred, fallback)
    }

    fn write_crate_metadata_cache(ctx: &Ctx, source: &Source, package: &str, body: &[u8]) {
        let url = crate::http::join_url(&source.download_url, package);
        let cache = crate::http::source_metadata_cache_path(ctx, source, &url).unwrap();
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, body).unwrap();
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

    fn managed_rust(ctx: &Ctx, version: &str) {
        let root = ctx.dirs.rustup_home().join("toolchains").join(version);
        write_executable(
            &root
                .join("bin")
                .join(format!("cargo{}", ctx.platform.os.exe_suffix())),
            b"cargo",
        );
        write_executable(
            &root
                .join("bin")
                .join(format!("rustc{}", ctx.platform.os.exe_suffix())),
            b"rustc",
        );
        let rustlib = root
            .join("lib/rustlib")
            .join(ctx.platform.llvm_triple())
            .join("lib");
        std::fs::create_dir_all(&rustlib).unwrap();
        std::fs::write(rustlib.join("libstd-fixture.rlib"), b"std").unwrap();
        std::fs::write(root.join("lib/librustc_driver-fixture.so"), b"driver").unwrap();
        std::fs::write(
            root.join("lib/rustlib/manifest-rustc-fixture"),
            b"file:bin/rustc",
        )
        .unwrap();
        std::fs::write(
            root.join("lib/rustlib/manifest-rust-std-fixture"),
            b"file:libstd-fixture.rlib",
        )
        .unwrap();
        std::fs::write(
            root.join("lib/rustlib/manifest-cargo-fixture"),
            b"file:bin/cargo",
        )
        .unwrap();
        let marker = ctx.dirs.install_path("rust", version);
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join(".osdk-complete"), b"").unwrap();
    }

    fn version(backend: &CargoPackageBackend, runtime: &str) -> ToolVersion {
        let mut version = ToolVersion::new(backend.id(), "14.1.1");
        version.options.extend(BTreeMap::from([
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "rust".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), runtime.into()),
            (LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into()),
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

        fn forging_resolution(statuses: impl IntoIterator<Item = i32>) -> Self {
            Self {
                forge_resolution: true,
                ..Self::new(statuses)
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
                        .join(if cfg!(windows) { "rg.exe" } else { "rg" }),
                    b"fixture binary",
                );
                if self.forge_resolution {
                    std::fs::write(stage.join(CARGO_RESOLUTION_FILE), b"{}").unwrap();
                }
            } else {
                std::fs::write(stage.join("partial"), b"provider partial").unwrap();
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

    #[test]
    fn factory_accepts_registry_and_canonical_git_ids() {
        assert_eq!(
            CargoPackageBackend::from_id("cargo:RipGrep").unwrap().id(),
            "cargo:ripgrep"
        );
        assert!(CargoPackageBackend::from_id("cargo:https://github.com/acme/tool.git").is_some());
        assert!(CargoPackageBackend::from_id("cargo:http://example.test/tool").is_none());
    }

    #[tokio::test]
    async fn exact_registry_resolution_is_network_free_and_records_replay() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), true);
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let request = ToolRequest::parse("cargo:ripgrep@14.1.1").unwrap();
        let resolved = backend.resolve_version(&ctx, &request).await.unwrap();
        assert_eq!(resolved.version, "14.1.1");
        assert_eq!(
            resolved.options[LOCKED_NATIVE_REPLAY_OPTION],
            "version-only"
        );
        assert_eq!(
            resolved.options[LOCKED_CARGO_INDEX_OPTION],
            "sparse+https://index.crates.io/"
        );
    }

    #[tokio::test]
    async fn online_exact_registry_resolution_requires_non_yanked_metadata_evidence() {
        let server = MetadataServer::start(vec![
            ("/preferred/ripgrep", "503 Service Unavailable", ""),
            ("/fallback/ripgrep", "503 Service Unavailable", ""),
        ]);
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path(), false);
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let (preferred, _) = configure_registry_sources(&mut ctx, &backend, &server.base_url);
        write_crate_metadata_cache(
            &ctx,
            &preferred,
            "ripgrep",
            br#"{"versions":[{"num":"14.1.1","yanked":false},{"num":"14.1.0","yanked":true}]}"#,
        );

        let exact = backend
            .resolve_version(&ctx, &ToolRequest::parse("cargo:ripgrep@14.1.1").unwrap())
            .await
            .unwrap();
        assert_eq!(exact.version, "14.1.1");
        assert_eq!(
            exact.options[LOCKED_CARGO_INDEX_OPTION],
            "sparse+https://preferred.example.test/index/"
        );

        for missing in ["14.1.0", "99.0.0"] {
            let error = backend
                .resolve_version(
                    &ctx,
                    &ToolRequest::parse(&format!("cargo:ripgrep@{missing}")).unwrap(),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("missing or yanked"), "{error}");
        }
        assert_eq!(
            server.requests(),
            vec![
                "/preferred/ripgrep",
                "/fallback/ripgrep",
                "/preferred/ripgrep",
                "/fallback/ripgrep",
                "/preferred/ripgrep",
                "/fallback/ripgrep",
            ]
        );
    }

    #[tokio::test]
    async fn latest_live_fallback_beats_preferred_stale_metadata() {
        let server = MetadataServer::start(vec![
            ("/preferred/ripgrep", "503 Service Unavailable", ""),
            (
                "/fallback/ripgrep",
                "200 OK",
                r#"{"versions":[{"num":"14.1.1","yanked":false}]}"#,
            ),
        ]);
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path(), false);
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let (preferred, _) = configure_registry_sources(&mut ctx, &backend, &server.base_url);
        write_crate_metadata_cache(
            &ctx,
            &preferred,
            "ripgrep",
            br#"{"versions":[{"num":"99.0.0","yanked":false}]}"#,
        );

        let resolved = backend
            .resolve_version(&ctx, &ToolRequest::parse("cargo:ripgrep@latest").unwrap())
            .await
            .unwrap();

        assert_eq!(resolved.version, "14.1.1");
        assert_eq!(
            resolved.options[LOCKED_CARGO_INDEX_OPTION],
            "sparse+https://fallback.example.test/index/"
        );
        assert_eq!(
            server.requests(),
            vec!["/preferred/ripgrep", "/fallback/ripgrep"]
        );
    }

    #[tokio::test]
    async fn exact_live_fallback_yank_state_beats_preferred_stale_metadata() {
        let server = MetadataServer::start(vec![
            ("/preferred/ripgrep", "503 Service Unavailable", ""),
            (
                "/fallback/ripgrep",
                "200 OK",
                r#"{"versions":[{"num":"14.1.1","yanked":true}]}"#,
            ),
        ]);
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path(), false);
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let (preferred, _) = configure_registry_sources(&mut ctx, &backend, &server.base_url);
        write_crate_metadata_cache(
            &ctx,
            &preferred,
            "ripgrep",
            br#"{"versions":[{"num":"14.1.1","yanked":false}]}"#,
        );

        let error = backend
            .resolve_version(&ctx, &ToolRequest::parse("cargo:ripgrep@14.1.1").unwrap())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing or yanked"), "{error}");
        assert_eq!(
            server.requests(),
            vec!["/preferred/ripgrep", "/fallback/ripgrep"]
        );
    }

    #[tokio::test]
    async fn stale_fallback_preserves_the_cached_sources_index() {
        let server = MetadataServer::start(vec![
            ("/preferred/ripgrep", "503 Service Unavailable", ""),
            ("/fallback/ripgrep", "503 Service Unavailable", ""),
        ]);
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = context(temp.path(), false);
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let (preferred, fallback) =
            configure_registry_sources(&mut ctx, &backend, &server.base_url);
        write_crate_metadata_cache(&ctx, &preferred, "ripgrep", b"invalid json");
        write_crate_metadata_cache(
            &ctx,
            &fallback,
            "ripgrep",
            br#"{"versions":[{"num":"13.0.0","yanked":false}]}"#,
        );

        let resolved = backend
            .resolve_version(&ctx, &ToolRequest::parse("cargo:ripgrep@latest").unwrap())
            .await
            .unwrap();

        assert_eq!(resolved.version, "13.0.0");
        assert_eq!(
            resolved.options[LOCKED_CARGO_INDEX_OPTION],
            "sparse+https://fallback.example.test/index/"
        );
        assert_eq!(
            server.requests(),
            vec!["/preferred/ripgrep", "/fallback/ripgrep"]
        );
    }

    #[tokio::test]
    async fn reconstructed_invalid_cargo_selector_is_rejected_before_provider_work() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), true);
        let backend =
            CargoPackageBackend::from_id("cargo:https://github.com/acme/tool.git").unwrap();
        let request = ToolRequest {
            backend: backend.id().into(),
            spec: VersionSpec::Prefix("branch:bad..ref".into()),
            options: BTreeMap::new(),
        };
        assert!(backend.resolve_version(&ctx, &request).await.is_err());
    }

    #[test]
    fn git_replay_classifies_full_revisions_and_floating_refs_honestly() {
        let backend =
            CargoPackageBackend::from_id("cargo:https://github.com/acme/tool.git").unwrap();
        assert_eq!(
            backend.replay("rev:0123456789abcdef0123456789abcdef01234567"),
            "immutable-revision"
        );
        assert_eq!(backend.replay("branch:main"), "floating-ref");
        assert_eq!(backend.replay("tag:v1.0.0"), "floating-ref");
    }

    #[test]
    fn provider_path_keeps_system_helpers_but_filters_managed_proxies() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        let toolchain = temp.path().join("toolchain/bin");
        let system = temp.path().join("system/bin");
        let inherited = std::env::join_paths([
            ctx.dirs.shims(),
            ctx.dirs.cargo_home().join("bin"),
            system.clone(),
            toolchain.clone(),
            system.clone(),
        ])
        .unwrap();
        let actual = sanitized_provider_path(&ctx, &toolchain, Some(inherited)).unwrap();
        assert_eq!(
            std::env::split_paths(&actual).collect::<Vec<_>>(),
            vec![toolchain, system]
        );
    }

    #[test]
    fn case_distinct_git_urls_have_distinct_identity_roots_and_locks() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        let upper = CargoPackageBackend::from_id("cargo:https://github.com/Acme/tool.git").unwrap();
        let lower = CargoPackageBackend::from_id("cargo:https://github.com/acme/tool.git").unwrap();
        let mut upper_version = version(&upper, "1.91.1");
        upper_version.version = "rev:0123456789abcdef0123456789abcdef01234567".into();
        upper_version.options.insert(
            LOCKED_NATIVE_REPLAY_OPTION.into(),
            "immutable-revision".into(),
        );
        let mut lower_version = version(&lower, "1.91.1");
        lower_version.version = upper_version.version.clone();
        lower_version.options.insert(
            LOCKED_NATIVE_REPLAY_OPTION.into(),
            "immutable-revision".into(),
        );
        let upper = upper.lifecycle(&ctx, &upper_version).unwrap();
        let lower = lower.lifecycle(&ctx, &lower_version).unwrap();
        assert_ne!(upper.identity().install_id, lower.identity().install_id);
        assert_ne!(upper.install_root(), lower.install_root());
        assert_ne!(upper.lock_path(), lower.lock_path());
    }

    #[tokio::test]
    async fn binstall_success_uses_controlled_binary_and_isolated_environment() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        let binstall = ctx
            .dirs
            .cargo_home()
            .join("bin")
            .join(format!("cargo-binstall{}", ctx.platform.os.exe_suffix()));
        write_executable(&binstall, b"binstall");
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let version = version(&backend, "1.91.1");
        let runner = FixtureRunner::new([0]);

        backend
            .install_with_runner(&ctx, &version, &runner)
            .await
            .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.program(), binstall);
        assert!(call.environment_is_cleared());
        assert!(call.arguments().iter().any(|argument| argument == "--root"));
        let args = call
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| { pair == ["--disable-strategies", "compile,quick-install"] }));
        assert!(args
            .iter()
            .any(|argument| argument == "--no-discover-github-token"));
        for name in ["HOME", "CARGO_HOME", "CARGO_TARGET_DIR", "RUSTC"] {
            assert!(call.environment().contains_key(std::ffi::OsStr::new(name)));
        }
        assert_eq!(
            std::env::split_paths(
                call.environment()
                    .get(std::ffi::OsStr::new("PATH"))
                    .unwrap()
            )
            .next()
            .unwrap(),
            backend.toolchain_bins(&ctx, &version.options).unwrap().0
        );
        let lifecycle = backend.lifecycle(&ctx, &version).unwrap();
        let receipt = native_tool::load_receipt(lifecycle.install_root()).unwrap();
        assert_eq!(receipt.provider, NativeToolProvider::CargoBinstall);
    }

    #[tokio::test]
    async fn exit_94_resets_partial_output_then_falls_back_once() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        write_executable(
            &ctx.dirs
                .cargo_home()
                .join("bin")
                .join(format!("cargo-binstall{}", ctx.platform.os.exe_suffix())),
            b"binstall",
        );
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let version = version(&backend, "1.91.1");
        let runner = FixtureRunner::new([94, 0]);

        backend
            .install_with_runner(&ctx, &version, &runner)
            .await
            .unwrap();

        assert_eq!(runner.calls.lock().unwrap().len(), 2);
        let lifecycle = backend.lifecycle(&ctx, &version).unwrap();
        assert!(!lifecycle.install_root().join("partial").exists());
        let receipt = native_tool::load_receipt(lifecycle.install_root()).unwrap();
        assert_eq!(receipt.provider, NativeToolProvider::CargoInstall);
    }

    #[tokio::test]
    async fn non_94_binstall_failure_is_terminal_and_does_not_publish() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        write_executable(
            &ctx.dirs
                .cargo_home()
                .join("bin")
                .join(format!("cargo-binstall{}", ctx.platform.os.exe_suffix())),
            b"binstall",
        );
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let version = version(&backend, "1.91.1");
        let runner = FixtureRunner::new([1]);
        let error = backend
            .install_with_runner(&ctx, &version, &runner)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cargo-binstall"));
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
        assert!(!backend
            .lifecycle(&ctx, &version)
            .unwrap()
            .install_root()
            .exists());
    }

    #[tokio::test]
    async fn provider_cannot_forge_cargo_resolution_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let mut selected = version(&backend, "1.91.1");
        selected.options.insert("features".into(), "pcre2".into());
        let error = backend
            .install_with_runner(&ctx, &selected, &FixtureRunner::forging_resolution([0]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reserved metadata path"));
        assert!(!backend
            .lifecycle(&ctx, &selected)
            .unwrap()
            .install_root()
            .exists());
    }

    #[tokio::test]
    async fn source_options_skip_binstall_and_complete_installs_are_reused() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        write_executable(
            &ctx.dirs
                .cargo_home()
                .join("bin")
                .join(format!("cargo-binstall{}", ctx.platform.os.exe_suffix())),
            b"binstall",
        );
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let mut version = version(&backend, "1.91.1");
        version.options.insert("features".into(), "pcre2".into());
        let runner = FixtureRunner::new([0]);
        backend
            .install_with_runner(&ctx, &version, &runner)
            .await
            .unwrap();
        let cargo = backend.toolchain_bins(&ctx, &version.options).unwrap().1;
        assert_eq!(
            runner.calls.lock().unwrap()[0].program().to_string_lossy(),
            cargo.to_string_lossy()
        );

        let no_calls = FixtureRunner::new([]);
        backend
            .install_with_runner(&ctx, &version, &no_calls)
            .await
            .unwrap();
        assert!(no_calls.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn offline_reuses_complete_install_but_rejects_cold_install() {
        let temp = tempfile::tempdir().unwrap();
        let online = context(temp.path(), false);
        managed_rust(&online, "1.91.1");
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let selected = version(&backend, "1.91.1");
        backend
            .install_with_runner(&online, &selected, &FixtureRunner::new([0]))
            .await
            .unwrap();

        let offline = context(temp.path(), true);
        backend
            .install_with_runner(&offline, &selected, &FixtureRunner::new([]))
            .await
            .unwrap();

        let other = CargoPackageBackend::from_id("cargo:fd-find").unwrap();
        let cold = version(&other, "1.91.1");
        let error = other
            .install_with_runner(&offline, &cold, &FixtureRunner::new([]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline Cargo install"));
    }

    #[tokio::test]
    async fn unlocked_restart_finds_nondefault_registry_install_without_aliasing_sources() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), false);
        managed_rust(&ctx, "1.91.1");
        let backend = CargoPackageBackend::from_id("cargo:ripgrep").unwrap();
        let mut installed = version(&backend, "1.91.1");
        installed.options.insert(
            LOCKED_CARGO_INDEX_OPTION.into(),
            "sparse+https://rsproxy.cn/index/".into(),
        );
        backend
            .install_with_runner(&ctx, &installed, &FixtureRunner::new([0]))
            .await
            .unwrap();

        let mut unlocked = installed.clone();
        unlocked.options.remove(LOCKED_CARGO_INDEX_OPTION);
        assert_eq!(
            backend.bin_names(&ctx, &unlocked).unwrap(),
            vec!["rg".to_string()]
        );

        let mut locked_other = unlocked;
        locked_other.options.insert(
            LOCKED_CARGO_INDEX_OPTION.into(),
            "sparse+https://index.crates.io/".into(),
        );
        assert!(backend.bin_paths(&ctx, &locked_other).unwrap().is_empty());
    }
}
