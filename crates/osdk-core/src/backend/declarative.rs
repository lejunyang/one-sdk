//! Data-only external backends loaded from TOML.
//!
//! Declarative backends can list versions, select a platform archive, verify
//! its checksum, expose installed binaries, and describe the environment their
//! toolchain needs. They intentionally cannot run hooks or arbitrary commands.
//! Installation always goes through the shared download, verification,
//! extraction, and CAS pipeline.
//!
//! The optional `[env]` table exists because a C/C++ toolchain is not usable
//! from `PATH` alone: build systems locate a cross compiler through `CC`,
//! `SYSROOT` and similar variables. Values may only interpolate paths that stay
//! inside the install root, so a definition can point a build at its own
//! toolchain but cannot inject an arbitrary host path into a child process.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Libc, Os, Platform};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

/// The only declarative backend schema currently accepted.
pub const SCHEMA_VERSION: u32 = 1;

const MAX_DEFINITION_BYTES: u64 = 1024 * 1024;
const MAX_VERSIONS: usize = 10_000;

/// A validated data-only backend.
///
/// The stable external interface is its schema-1 TOML representation. Use
/// [`DeclarativeBackend::from_toml`] for an in-memory definition or
/// [`load_dir`] to load a directory containing one backend per `.toml` file.
pub struct DeclarativeBackend {
    id: String,
    versions: VersionSource,
    archive: ArchiveDefinition,
    bin_paths: Vec<PathBuf>,
    bin_names: Vec<String>,
    env: BTreeMap<String, String>,
    idiomatic_files: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendDefinition {
    schema: u32,
    id: String,
    versions: VersionDefinition,
    archive: ArchiveDefinition,
    bin_paths: Vec<String>,
    bin_names: Vec<String>,
    /// Environment a build system needs in order to find this toolchain.
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    idiomatic_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionDefinition {
    #[serde(default)]
    values: Vec<String>,
    url: Option<String>,
}

enum VersionSource {
    Static(Vec<String>),
    Url(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveDefinition {
    url: String,
    file: String,
    kind: ArchiveKindDefinition,
    #[serde(default)]
    strip_root: bool,
    checksum: ChecksumDefinition,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum ArchiveKindDefinition {
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.zst")]
    TarZst,
    #[serde(rename = "zip")]
    Zip,
    /// Windows GCC toolchains are commonly published as `.7z` only. Only the
    /// install path can unpack one, so the variant follows that feature.
    #[cfg(feature = "install")]
    #[serde(rename = "7z")]
    SevenZ,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksumDefinition {
    algorithm: ChecksumAlgorithm,
    value: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChecksumAlgorithm {
    Sha256,
    Sha512,
    Blake3,
}

impl DeclarativeBackend {
    /// Parse and validate one schema-1 backend definition.
    pub fn from_toml(input: &str) -> Result<Self> {
        let definition: BackendDefinition = toml::from_str(input)?;
        Self::from_definition(definition)
    }

    /// Load and validate one schema-1 backend definition from a file.
    pub fn load_file(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))?;
        if !metadata.file_type().is_file() {
            return Err(Error::config(format!(
                "declarative backend definition must be a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_DEFINITION_BYTES {
            return Err(Error::config(format!(
                "declarative backend definition exceeds {MAX_DEFINITION_BYTES} bytes: {}",
                path.display()
            )));
        }
        let input = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
        Self::from_toml(&input).map_err(|error| {
            Error::config(format!(
                "invalid declarative backend {}: {error}",
                path.display()
            ))
        })
    }

    fn from_definition(mut definition: BackendDefinition) -> Result<Self> {
        if definition.schema != SCHEMA_VERSION {
            return Err(Error::config(format!(
                "unsupported declarative backend schema {}; expected {SCHEMA_VERSION}",
                definition.schema
            )));
        }
        validate_id(&definition.id)?;

        let has_values = !definition.versions.values.is_empty();
        let has_url = definition.versions.url.is_some();
        if has_values == has_url {
            return Err(Error::config(
                "`versions` must set exactly one of `values` or `url`",
            ));
        }
        let versions = if let Some(url) = definition.versions.url {
            validate_url_template(
                "versions.url",
                &url,
                &["id", "os", "arch", "arch_llvm", "libc"],
                false,
            )?;
            VersionSource::Url(url)
        } else {
            validate_versions(&mut definition.versions.values)?;
            VersionSource::Static(definition.versions.values)
        };

        validate_url_template(
            "archive.url",
            &definition.archive.url,
            &["id", "version", "os", "arch", "arch_llvm", "libc", "file"],
            false,
        )?;
        validate_file_template(&definition.archive.file)?;
        if !definition.archive.url.contains("{version}")
            && !definition.archive.url.contains("{file}")
            && !definition.archive.file.contains("{version}")
        {
            return Err(Error::config(
                "`archive.url` or `archive.file` must vary by `{version}`",
            ));
        }
        definition.archive.checksum.validate()?;

        if definition.bin_paths.is_empty() {
            return Err(Error::config("`bin_paths` must not be empty"));
        }
        let bin_paths = definition
            .bin_paths
            .iter()
            .map(|path| validate_relative_path("bin path", path))
            .collect::<Result<Vec<_>>>()?;

        if definition.bin_names.is_empty() {
            return Err(Error::config("`bin_names` must not be empty"));
        }
        for name in &definition.bin_names {
            validate_basename("bin name", name)?;
        }
        for name in &definition.idiomatic_files {
            validate_basename("idiomatic file", name)?;
        }

        let env = validate_env(&definition.env)?;

        // Backend definitions are process-lifetime registry data. Interning the
        // small idiomatic filename list satisfies the existing Backend trait's
        // borrowed-slice contract without allowing executable plugin code.
        let idiomatic_files = definition
            .idiomatic_files
            .into_iter()
            .map(|name| -> &'static str { Box::leak(name.into_boxed_str()) })
            .collect();

        Ok(Self {
            id: definition.id,
            versions,
            archive: definition.archive,
            bin_paths,
            bin_names: definition.bin_names,
            env,
            idiomatic_files,
        })
    }

    fn rendered_file(&self, platform: Platform, version: &str) -> Result<String> {
        validate_version(version)?;
        let rendered = render_template(
            &self.archive.file,
            &self.id,
            Some(version),
            platform,
            None,
            None,
        );
        validate_basename("rendered archive file", &rendered)?;
        Ok(rendered)
    }

    fn rendered_url(
        &self,
        template: &str,
        platform: Platform,
        version: Option<&str>,
        file: Option<&str>,
        archive_url: Option<&str>,
    ) -> Result<String> {
        let rendered = render_template(template, &self.id, version, platform, file, archive_url);
        validate_rendered_url(&rendered)?;
        Ok(rendered)
    }

    async fn remote_versions(&self, ctx: &Ctx, template: &str) -> Result<Vec<VersionInfo>> {
        let url = self.rendered_url(template, ctx.platform, None, None, None)?;
        let body = crate::http::get_cached_text(ctx, &url).await?;
        let mut values = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect::<Vec<_>>();
        validate_versions(&mut values)?;
        Ok(values.into_iter().map(VersionInfo::stable).collect())
    }

    async fn checksum(
        &self,
        ctx: &Ctx,
        version: &str,
        file: &str,
        archive_urls: &[String],
    ) -> Result<Checksum> {
        let definition = &self.archive.checksum;
        let algo = definition.algorithm.into();
        if let Some(value) = &definition.value {
            validate_checksum(value, definition.algorithm)?;
            return Ok(Checksum {
                algo,
                hex: value.clone(),
            });
        }

        let template = definition
            .url
            .as_deref()
            .ok_or_else(|| Error::config("checksum URL is missing"))?;
        let mut last_error = None;
        for archive_url in archive_urls {
            let url = self.rendered_url(
                template,
                ctx.platform,
                Some(version),
                Some(file),
                Some(archive_url),
            )?;
            match crate::http::get_cached_text(ctx, &url).await {
                Ok(body) => {
                    let value = body.split_whitespace().next().ok_or_else(|| {
                        Error::other(format!("empty checksum response from {url}"))
                    })?;
                    validate_checksum(value, definition.algorithm)?;
                    return Ok(Checksum {
                        algo,
                        hex: value.to_string(),
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id.clone(),
            tried: archive_urls.len(),
        }))
    }
}

impl ChecksumDefinition {
    fn validate(&self) -> Result<()> {
        if self.value.is_some() == self.url.is_some() {
            return Err(Error::config(
                "`archive.checksum` must set exactly one of `value` or `url`",
            ));
        }
        if let Some(value) = &self.value {
            validate_checksum(value, self.algorithm)?;
        }
        if let Some(url) = &self.url {
            validate_url_template(
                "archive.checksum.url",
                url,
                &[
                    "id",
                    "version",
                    "os",
                    "arch",
                    "arch_llvm",
                    "libc",
                    "file",
                    "archive_url",
                ],
                true,
            )?;
        }
        Ok(())
    }
}

impl From<ArchiveKindDefinition> for ArchiveKind {
    fn from(value: ArchiveKindDefinition) -> Self {
        match value {
            ArchiveKindDefinition::TarGz => ArchiveKind::TarGz,
            ArchiveKindDefinition::TarXz => ArchiveKind::TarXz,
            ArchiveKindDefinition::TarZst => ArchiveKind::TarZst,
            ArchiveKindDefinition::Zip => ArchiveKind::Zip,
            #[cfg(feature = "install")]
            ArchiveKindDefinition::SevenZ => ArchiveKind::SevenZ,
        }
    }
}

impl From<ChecksumAlgorithm> for HashAlgo {
    fn from(value: ChecksumAlgorithm) -> Self {
        match value {
            ChecksumAlgorithm::Sha256 => HashAlgo::Sha256,
            ChecksumAlgorithm::Sha512 => HashAlgo::Sha512,
            ChecksumAlgorithm::Blake3 => HashAlgo::Blake3,
        }
    }
}

#[async_trait]
impl Backend for DeclarativeBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        let mut source = Source::official("declarative", &self.archive.url);
        if let VersionSource::Url(url) = &self.versions {
            source = source.with_index(url);
        }
        vec![source]
    }

    fn probe_url(&self, ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.as_deref().and_then(|template| {
            self.rendered_url(template, ctx.platform, None, None, None)
                .ok()
        })
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        match &self.versions {
            VersionSource::Static(values) => {
                Ok(values.iter().cloned().map(VersionInfo::stable).collect())
            }
            VersionSource::Url(default_url) => {
                let sources = crate::source::select::ranked_source_list(ctx, self).await?;
                let mut last_error = None;
                for source in &sources {
                    let template = source.index_url.as_deref().unwrap_or(default_url);
                    match self.remote_versions(ctx, template).await {
                        Ok(versions) => return Ok(versions),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
                    tool: self.id.clone(),
                    tried: sources.len(),
                }))
            }
        }
    }

    #[cfg(feature = "install")]
    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        validate_version(&tv.version)?;
        let plan = if let Some(plan) =
            pipeline::locked_install_plan(self.id(), tv, self.archive.strip_root)?
        {
            plan
        } else {
            let file = self.rendered_file(ctx.platform, &tv.version)?;
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let urls = sources
                .iter()
                .map(|source| {
                    self.rendered_url(
                        &source.download_url,
                        ctx.platform,
                        Some(&tv.version),
                        Some(&file),
                        None,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            let checksum = self.checksum(ctx, &tv.version, &file, &urls).await?;
            InstallPlan {
                tool: self.id.clone(),
                version: tv.version.clone(),
                urls,
                file_name: file,
                kind: self.archive.kind.into(),
                checksum: Some(checksum),
                strip_root: self.archive.strip_root,
                subdir: None,
            }
        };
        let pipeline_ctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
            require_checksums: ctx.config.settings.require_checksums,
        };
        pipeline::run(&plan, &pipeline_ctx).await?;
        Ok(())
    }

    #[cfg(feature = "install")]
    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        validate_version(&tv.version)?;
        let directory = ctx.dirs.install_path(self.id(), &tv.version);
        if directory.exists() {
            std::fs::remove_dir_all(&directory).map_err(|error| Error::io(&directory, error))?;
        }
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        validate_version(&tv.version)?;
        let install = ctx.dirs.install_path(self.id(), &tv.version);
        Ok(self
            .bin_paths
            .iter()
            .map(|path| install.join(path))
            .collect())
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(self.bin_names.clone())
    }

    /// The environment this toolchain needs, with `{install_path}` resolved to
    /// the version's own install root.
    fn exec_env(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        if self.env.is_empty() {
            return Ok(BTreeMap::new());
        }
        validate_version(&tv.version)?;
        let install = ctx.dirs.install_path(self.id(), &tv.version);
        let install = install.display().to_string();
        let mut rendered = BTreeMap::new();
        for (name, value) in &self.env {
            let value = value
                .replace("{install_path}", &install)
                .replace("{version}", &tv.version)
                .replace("{id}", &self.id);
            if value.contains('{') || value.contains('}') {
                return Err(Error::config(format!(
                    "env variable `{name}` left an unresolved placeholder: `{value}`"
                )));
            }
            rendered.insert(name.clone(), value);
        }
        Ok(rendered)
    }

    fn idiomatic_files(&self) -> &[&str] {
        &self.idiomatic_files
    }
}

/// Load all regular `.toml` definitions directly inside `directory`.
///
/// Files are loaded in lexical path order. A missing directory is an empty
/// plugin set; malformed definitions fail the whole load.
pub fn load_dir(directory: &Path) -> Result<Vec<DeclarativeBackend>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = std::fs::read_dir(directory)
        .map_err(|error| Error::io(directory, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| Error::io(directory, error))
        })
        .collect::<Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"));
    paths.sort();
    paths
        .iter()
        .map(|path| DeclarativeBackend::load_file(path))
        .collect()
}

fn validate_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    if !chars
        .next()
        .map(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        .unwrap_or(false)
        || !chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(Error::config(format!(
            "invalid declarative backend id `{id}`; use lowercase ASCII letters, digits, `-`, or `_`"
        )));
    }
    if id == "github" || id.starts_with("github:") {
        return Err(Error::config(
            "declarative backend ids cannot use the reserved `github` namespace",
        ));
    }
    Ok(())
}

fn validate_versions(versions: &mut Vec<String>) -> Result<()> {
    if versions.is_empty() {
        return Err(Error::config("version list must not be empty"));
    }
    if versions.len() > MAX_VERSIONS {
        return Err(Error::config(format!(
            "version list exceeds {MAX_VERSIONS} entries"
        )));
    }
    for version in versions.iter() {
        validate_version(version)?;
    }
    versions.sort_by(|left, right| {
        match (semver::Version::parse(left), semver::Version::parse(right)) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        }
    });
    versions.dedup();
    Ok(())
}

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version == "."
        || version == ".."
        || !version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
        })
    {
        return Err(Error::config(format!(
            "invalid declarative backend version `{version}`"
        )));
    }
    Ok(())
}

fn validate_relative_path(label: &str, value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\\') || value.contains(':') {
        return Err(Error::config(format!("invalid {label} `{value}`")));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::config(format!(
            "{label} must stay inside the install root: `{value}`"
        )));
    }
    Ok(path.to_path_buf())
}

fn validate_basename(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err(Error::config(format!(
            "{label} must be a single safe filename: `{value}`"
        )));
    }
    Ok(())
}

/// Validate the optional `[env]` table.
///
/// A declarative backend may describe the environment its toolchain needs, but
/// it must not be able to point a child process at arbitrary host state. Names
/// are restricted to conventional environment identifiers, and every value is
/// built only from literal text and install-root-relative placeholders, so a
/// definition can never smuggle in an absolute path or a reference to another
/// tool's directory.
fn validate_env(env: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut validated = BTreeMap::new();
    for (name, value) in env {
        validate_env_name(name)?;
        validate_env_value(name, value)?;
        validated.insert(name.clone(), value.clone());
    }
    Ok(validated)
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_');
    if !valid_start
        || !characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(Error::config(format!(
            "invalid env variable name `{name}`; use ASCII letters, digits, or `_`"
        )));
    }
    // PATH is composed from `bin_paths`, and these steer the process or the
    // dynamic loader at libraries outside the install root.
    const RESERVED: &[&str] = &[
        "PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
    ];
    if RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(Error::config(format!(
            "env variable `{name}` is reserved; declare directories through `bin_paths`"
        )));
    }
    Ok(())
}

fn validate_env_value(name: &str, value: &str) -> Result<()> {
    let label = format!("env.{name}");
    validate_template(&label, value, &["id", "version", "install_path"])?;
    if value.is_empty() {
        return Err(Error::config(format!("`{label}` must not be empty")));
    }
    // An absolute path or a parent traversal would escape the install root that
    // `{install_path}` anchors, which is the whole point of restricting values.
    if value.contains("..") {
        return Err(Error::config(format!(
            "`{label}` must not contain `..`; values stay inside the install root"
        )));
    }
    // Test the literal prefix, not the placeholder-stripped text: a value that
    // legitimately starts with `{install_path}` continues with a separator, and
    // stripping the placeholder first would make it look absolute.
    let leading_literal = match value.find('{') {
        Some(0) => "",
        Some(open) => &value[..open],
        None => value,
    };
    if Path::new(leading_literal).is_absolute()
        || leading_literal.starts_with('/')
        || leading_literal.starts_with('\\')
    {
        return Err(Error::config(format!(
            "`{label}` must not use an absolute path; anchor it at `{{install_path}}`"
        )));
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(Error::config(format!(
            "`{label}` must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_file_template(template: &str) -> Result<()> {
    validate_template(
        "archive.file",
        template,
        &["id", "version", "os", "arch", "arch_llvm", "libc"],
    )?;
    if template.contains('/') || template.contains('\\') || template.contains(':') {
        return Err(Error::config(
            "`archive.file` must render to a single filename",
        ));
    }
    Ok(())
}

fn validate_url_template(
    label: &str,
    template: &str,
    placeholders: &[&str],
    allow_archive_url_prefix: bool,
) -> Result<()> {
    validate_template(label, template, placeholders)?;
    if !(template.starts_with("https://")
        || template.starts_with("http://")
        || (allow_archive_url_prefix && template.starts_with("{archive_url}")))
    {
        return Err(Error::config(format!(
            "`{label}` must use an HTTP or HTTPS URL"
        )));
    }
    Ok(())
}

fn validate_template(label: &str, template: &str, placeholders: &[&str]) -> Result<()> {
    if template.is_empty() {
        return Err(Error::config(format!("`{label}` must not be empty")));
    }
    let mut remainder = template;
    while let Some(open) = remainder.find('{') {
        if remainder[..open].contains('}') {
            return Err(Error::config(format!(
                "`{label}` contains an unmatched `}}`"
            )));
        }
        let after_open = &remainder[open + 1..];
        let close = after_open
            .find('}')
            .ok_or_else(|| Error::config(format!("`{label}` contains an unmatched `{{`")))?;
        let placeholder = &after_open[..close];
        if !placeholders.contains(&placeholder) {
            return Err(Error::config(format!(
                "`{label}` uses unsupported placeholder `{{{placeholder}}}`"
            )));
        }
        remainder = &after_open[close + 1..];
    }
    if remainder.contains('}') {
        return Err(Error::config(format!(
            "`{label}` contains an unmatched `}}`"
        )));
    }
    Ok(())
}

fn validate_rendered_url(url: &str) -> Result<()> {
    if url.contains('{') || url.contains('}') {
        return Err(Error::config(format!(
            "URL template left an unresolved placeholder: `{url}`"
        )));
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| Error::config(format!("invalid rendered URL `{url}`: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::config(format!(
            "rendered URL must use HTTP or HTTPS with a host: `{url}`"
        )));
    }
    Ok(())
}

fn validate_checksum(value: &str, algorithm: ChecksumAlgorithm) -> Result<()> {
    let expected_length = match algorithm {
        ChecksumAlgorithm::Sha256 | ChecksumAlgorithm::Blake3 => 64,
        ChecksumAlgorithm::Sha512 => 128,
    };
    if value.len() != expected_length
        || !value.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(Error::config(format!(
            "invalid {:?} checksum; expected {expected_length} hexadecimal characters",
            algorithm
        )));
    }
    Ok(())
}

fn render_template(
    template: &str,
    id: &str,
    version: Option<&str>,
    platform: Platform,
    file: Option<&str>,
    archive_url: Option<&str>,
) -> String {
    let os = match platform.os {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
        Arch::X86 => "x86",
        Arch::Arm => "arm",
    };
    let libc = match platform.libc {
        Libc::Glibc => "glibc",
        Libc::Musl => "musl",
        Libc::None => "none",
    };
    let mut rendered = template
        .replace("{id}", id)
        .replace("{os}", os)
        .replace("{arch}", arch)
        .replace("{arch_llvm}", platform.arch.llvm_token())
        .replace("{libc}", libc);
    if let Some(version) = version {
        rendered = rendered.replace("{version}", version);
    }
    if let Some(file) = file {
        rendered = rendered.replace("{file}", file);
    }
    if let Some(archive_url) = archive_url {
        rendered = rendered.replace("{archive_url}", archive_url);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::source::Selection;
    use crate::store::link::LinkMode;
    use crate::store::Cas;

    const STATIC_FIXTURE: &str =
        include_str!("../../tests/fixtures/declarative/static-backend.toml");

    #[test]
    fn parses_static_fixture() {
        let backend = DeclarativeBackend::from_toml(STATIC_FIXTURE).unwrap();
        assert_eq!(backend.id(), "acme");
        assert_eq!(backend.bin_names, ["acme", "acmectl"]);
        assert_eq!(backend.idiomatic_files(), [".acme-version"]);
    }

    #[test]
    fn rejects_executable_hooks_and_unsafe_paths() {
        let with_script = STATIC_FIXTURE.replace(
            "id = \"acme\"",
            "id = \"acme\"\ninstall_script = \"curl example.test | sh\"",
        );
        assert!(DeclarativeBackend::from_toml(&with_script).is_err());

        let unsafe_path =
            STATIC_FIXTURE.replace("bin_paths = [\"bin\"]", "bin_paths = [\"../bin\"]");
        assert!(DeclarativeBackend::from_toml(&unsafe_path).is_err());
    }

    /// A C/C++ toolchain is only usable once a build system can find it, so an
    /// `[env]` table must survive into the activated environment with
    /// `{install_path}` resolved against the version's own root.
    #[test]
    fn env_table_renders_against_the_install_root() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = isolated_dirs(temp.path());
        let definition = format!(
            "{STATIC_FIXTURE}\n[env]\nCC = \"{{install_path}}/bin/acme-gcc\"\n\
             SYSROOT = \"{{install_path}}/sysroot\"\nACME_RELEASE = \"{{version}}\"\n"
        );
        let backend = DeclarativeBackend::from_toml(&definition).unwrap();
        let ctx = test_ctx(dirs);
        let tv = ToolVersion::new("acme", "1.2.3");

        let env = backend.exec_env(&ctx, &tv).unwrap();
        let root = ctx.dirs.install_path("acme", "1.2.3");
        assert_eq!(
            env.get("CC").unwrap(),
            &format!("{}/bin/acme-gcc", root.display())
        );
        assert_eq!(
            env.get("SYSROOT").unwrap(),
            &format!("{}/sysroot", root.display())
        );
        assert_eq!(env.get("ACME_RELEASE").unwrap(), "1.2.3");
        // No placeholder may survive into a child process.
        for value in env.values() {
            assert!(!value.contains('{') && !value.contains('}'), "{value}");
        }
    }

    /// Absent `[env]` must stay absent rather than becoming an empty export set,
    /// so existing definitions keep their current activation behavior.
    #[test]
    fn absent_env_table_exports_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let backend = DeclarativeBackend::from_toml(STATIC_FIXTURE).unwrap();
        let ctx = test_ctx(isolated_dirs(temp.path()));
        let env = backend
            .exec_env(&ctx, &ToolVersion::new("acme", "1.2.3"))
            .unwrap();
        assert!(env.is_empty());
    }

    /// An `[env]` value is the one place a data-only definition could otherwise
    /// aim a child process at arbitrary host state, so each escape must be
    /// refused when the definition is parsed rather than when it is activated.
    #[test]
    fn env_table_rejects_escapes_from_the_install_root() {
        let cases = [
            // Absolute paths and parent traversal leave the install root.
            ("CC", "/usr/bin/gcc"),
            ("CC", "C:\\\\Program Files\\\\gcc.exe"),
            ("SYSROOT", "{install_path}/../../other-tool/sysroot"),
            // PATH is owned by bin_paths; loader hooks bypass the install root.
            ("PATH", "{install_path}/bin"),
            ("LD_PRELOAD", "{install_path}/lib/evil.so"),
            ("LD_LIBRARY_PATH", "{install_path}/lib"),
            ("DYLD_INSERT_LIBRARIES", "{install_path}/lib/evil.dylib"),
            // Unknown placeholders must not reach a rendered value.
            ("CC", "{home}/bin/gcc"),
            ("CC", "{archive_url}"),
            // Malformed names and values.
            ("2CC", "{install_path}/bin/gcc"),
            ("CC-BAD", "{install_path}/bin/gcc"),
            ("CC", ""),
        ];
        for (name, value) in cases {
            let definition = format!("{STATIC_FIXTURE}\n[env]\n{name} = \"{value}\"\n");
            assert!(
                DeclarativeBackend::from_toml(&definition).is_err(),
                "expected `{name} = {value}` to be rejected"
            );
        }
    }

    /// Case-insensitive spellings must not slip past the reserved-name check.
    #[test]
    fn env_table_rejects_reserved_names_case_insensitively() {
        for name in ["path", "Path", "ld_preload", "Dyld_Library_Path"] {
            let definition =
                format!("{STATIC_FIXTURE}\n[env]\n{name} = \"{{install_path}}/bin\"\n");
            assert!(
                DeclarativeBackend::from_toml(&definition).is_err(),
                "expected reserved name `{name}` to be rejected"
            );
        }
    }

    /// Toolchain archives are overwhelmingly named with LLVM triple tokens
    /// (`x86_64`, `aarch64`) rather than osdk's own short tokens, so a
    /// definition must be able to ask for either spelling.
    #[test]
    fn arch_llvm_renders_triple_tokens_independently_of_arch() {
        let expectations = [
            (Arch::X64, "x64", "x86_64"),
            (Arch::Arm64, "arm64", "aarch64"),
            (Arch::X86, "x86", "i686"),
            (Arch::Arm, "arm", "armv7"),
        ];
        for (arch, short, llvm) in expectations {
            let platform = Platform {
                os: Os::Linux,
                arch,
                libc: Libc::Glibc,
            };
            // Both tokens must survive in one template: `{arch}` and
            // `{arch_llvm}` are distinct whole placeholders, so neither
            // substitution may consume part of the other.
            let rendered = render_template(
                "tool-{arch_llvm}-{arch}.tar.gz",
                "acme",
                Some("1.0.0"),
                platform,
                None,
                None,
            );
            assert_eq!(rendered, format!("tool-{llvm}-{short}.tar.gz"));
            assert!(!rendered.contains('{'), "{rendered}");
        }
    }

    /// The new token has to be accepted everywhere a platform token already is,
    /// and still rejected where no platform token belongs.
    #[test]
    fn arch_llvm_is_accepted_in_platform_template_positions() {
        let definition = STATIC_FIXTURE
            .replace(
                "file = \"acme-{version}-{os}-{arch}.tar.gz\"",
                "file = \"acme-{version}-{os}-{arch_llvm}.tar.gz\"",
            )
            .replace(
                "url = \"https://example.test/{version}/{file}\"",
                "url = \"https://example.test/{arch_llvm}/{version}/{file}\"",
            );
        assert!(DeclarativeBackend::from_toml(&definition).is_ok());

        // `[env]` values are not platform templates.
        let in_env = format!("{STATIC_FIXTURE}\n[env]\nCC = \"{{arch_llvm}}/gcc\"\n");
        assert!(DeclarativeBackend::from_toml(&in_env).is_err());
    }

    /// The extractor supporting `.7z` is not enough on its own: the TOML `kind`
    /// field has its own variant list, so a definition must be able to *say*
    /// `7z` and have it reach the pipeline.
    #[cfg(feature = "install")]
    #[test]
    fn archive_kind_7z_round_trips_from_toml() {
        let definition = STATIC_FIXTURE
            .replace("kind = \"tar.gz\"", "kind = \"7z\"")
            .replace(
                "file = \"acme-{version}-{os}-{arch}.tar.gz\"",
                "file = \"acme-{version}-{os}-{arch_llvm}.7z\"",
            );
        let backend = DeclarativeBackend::from_toml(&definition).expect("`kind = \"7z\"` accepted");
        assert_eq!(ArchiveKind::from(backend.archive.kind), ArchiveKind::SevenZ);
    }

    #[tokio::test]
    async fn loads_lists_and_installs_from_local_fixtures() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = isolated_dirs(temp.path());
        dirs.ensure().unwrap();

        let archive_path = temp.path().join("acme.tar.gz");
        write_fixture_archive(&archive_path);
        let archive = std::fs::read(&archive_path).unwrap();
        let checksum = crate::pipeline::verify::hash_file(&archive_path, HashAlgo::Sha256).unwrap();
        let versions = include_bytes!("../../tests/fixtures/declarative/versions.txt").to_vec();

        let mut routes = HashMap::new();
        routes.insert("/versions.txt".to_string(), versions);
        routes.insert(
            "/downloads/acme-1.2.3-linux-x64.tar.gz".to_string(),
            archive,
        );
        routes.insert(
            "/downloads/acme-1.2.3-linux-x64.tar.gz.sha256".to_string(),
            format!("{checksum}  acme-1.2.3-linux-x64.tar.gz\n").into_bytes(),
        );
        let (base_url, server) = serve(routes, 3);

        let plugin_dir = dirs.config.join("plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("acme.toml"),
            format!(
                r#"
schema = 1
id = "acme"
bin_paths = ["bin"]
bin_names = ["acme"]
idiomatic_files = [".acme-version"]

[versions]
url = "{base_url}/versions.txt"

[archive]
url = "{base_url}/downloads/{{file}}"
file = "acme-{{version}}-{{os}}-{{arch}}.tar.gz"
kind = "tar.gz"
strip_root = true

[archive.checksum]
algorithm = "sha256"
url = "{{archive_url}}.sha256"
"#
            ),
        )
        .unwrap();

        let registry = crate::backend::registry::Registry::load(&dirs).unwrap();
        let backend = registry.get("acme").unwrap();
        let ctx = test_ctx(dirs);
        let versions = backend.list_remote_versions(&ctx).await.unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|version| version.version.as_str())
                .collect::<Vec<_>>(),
            ["1.2.3"]
        );

        let tool_version = ToolVersion::new("acme", "1.2.3");
        backend
            .install(&InstallCtx { ctx: &ctx }, &tool_version)
            .await
            .unwrap();
        let installed = ctx.dirs.install_path("acme", "1.2.3");
        assert_eq!(
            std::fs::read_to_string(installed.join("bin/acme")).unwrap(),
            "fixture executable\n"
        );
        assert!(installed.join(".osdk-complete").is_file());
        assert_eq!(
            backend.bin_paths(&ctx, &tool_version).unwrap(),
            [installed.join("bin")]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn locked_artifact_reinstalls_offline_without_rendering_current_templates() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = isolated_dirs(temp.path());
        dirs.ensure().unwrap();

        let file_name = "locked-acme.tar.gz";
        let archive_path =
            pipeline::artifact_cache_path(&dirs, "acme", "1.2.3", file_name).unwrap();
        std::fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        write_fixture_archive(&archive_path);
        let checksum = pipeline::verify::hash_file(&archive_path, HashAlgo::Sha256).unwrap();

        // These templates deliberately cannot produce the locked artifact. A
        // lock-restored request must use its recorded URL, filename, and
        // checksum without consulting current plugin metadata.
        let backend = DeclarativeBackend::from_toml(
            &STATIC_FIXTURE
                .replace("acme-{version}-{os}-{arch}.tar.gz", "changed-{version}.zip")
                .replace("kind = \"tar.gz\"", "kind = \"zip\""),
        )
        .unwrap();
        let mut ctx = test_ctx(dirs);
        ctx.config.settings.offline = true;
        ctx.config.settings.require_checksums = true;
        let mut tool_version = ToolVersion::new("acme", "1.2.3");
        tool_version.options.extend(BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://unreachable.invalid/locked-acme.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
        ]));

        backend
            .install(&InstallCtx { ctx: &ctx }, &tool_version)
            .await
            .unwrap();

        let installed = ctx.dirs.install_path("acme", "1.2.3");
        assert_eq!(
            std::fs::read_to_string(installed.join("bin/acme")).unwrap(),
            "fixture executable\n"
        );
        assert!(installed.join(".osdk-complete").is_file());
    }

    fn isolated_dirs(root: &Path) -> Dirs {
        Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap()
    }

    fn test_ctx(dirs: Dirs) -> Ctx {
        let settings = Settings {
            link_mode: LinkMode::Copy,
            ..Default::default()
        };
        let sources = SourcesConfig {
            selection: Selection::Ordered,
            ..Default::default()
        };
        Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            },
            config: Config {
                settings,
                sources,
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

    fn write_fixture_archive(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let contents = b"fixture executable\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "acme/bin/acme", &contents[..])
            .unwrap();
        archive.finish().unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    fn serve(
        routes: HashMap<String, Vec<u8>>,
        request_count: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                match routes.get(path) {
                    Some(body) => {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .unwrap();
                        stream.write_all(body).unwrap();
                    }
                    None => {
                        write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .unwrap();
                    }
                }
            }
        });
        (format!("http://{address}"), server)
    }
}
