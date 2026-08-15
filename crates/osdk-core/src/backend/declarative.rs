//! Data-only external backends loaded from TOML.
//!
//! Declarative backends can list versions, select a platform archive, verify
//! its checksum, and expose installed binaries. They intentionally cannot run
//! hooks or arbitrary commands. Installation always goes through the shared
//! download, verification, extraction, and CAS pipeline.

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
            validate_url_template("versions.url", &url, &["id", "os", "arch", "libc"], false)?;
            VersionSource::Url(url)
        } else {
            validate_versions(&mut definition.versions.values)?;
            VersionSource::Static(definition.versions.values)
        };

        validate_url_template(
            "archive.url",
            &definition.archive.url,
            &["id", "version", "os", "arch", "libc", "file"],
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
                &["id", "version", "os", "arch", "libc", "file", "archive_url"],
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

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        validate_version(&tv.version)?;
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
        let plan = InstallPlan {
            tool: self.id.clone(),
            version: tv.version.clone(),
            urls,
            file_name: file,
            kind: self.archive.kind.into(),
            checksum: Some(checksum),
            strip_root: self.archive.strip_root,
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

fn validate_file_template(template: &str) -> Result<()> {
    validate_template(
        "archive.file",
        template,
        &["id", "version", "os", "arch", "libc"],
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
    use std::collections::HashMap;
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
