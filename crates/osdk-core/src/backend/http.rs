//! Inline HTTPS artifacts addressed as `http:https://host/path-{version}`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures_util::StreamExt as _;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::dirs::InstallLocator;
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::source::Source;
use crate::tool::{InstallIdentity, InstallScope};
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 16 * 1024;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const DNS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug)]
pub struct HttpBackend {
    id: String,
    template: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpArtifactKind {
    TarGz,
    TarXz,
    Zip,
    File,
}

#[derive(Debug)]
struct HttpArtifact {
    url: String,
    file_name: String,
    kind: HttpArtifactKind,
    checksum: Checksum,
}

impl HttpBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let id = crate::tool::canonical_dynamic_id(id).ok()?;
        let template = id.strip_prefix("http:")?.to_string();
        Some(Self { id, template })
    }

    fn artifact(&self, tv: &ToolVersion) -> Result<HttpArtifact> {
        if let Some(locked) = pipeline::locked_artifact(tv)? {
            validate_rendered_url(&locked.url)?;
            let checksum = locked
                .checksum
                .as_deref()
                .map(pipeline::parse_checksum)
                .transpose()?
                .ok_or_else(|| Error::config("locked HTTP artifact is missing its checksum"))?;
            if checksum.algo != HashAlgo::Sha256
                || checksum.hex.len() != 64
                || !checksum.hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                || checksum.hex != checksum.hex.to_ascii_lowercase()
            {
                return Err(Error::config(
                    "locked HTTP artifact requires a SHA-256 checksum",
                ));
            }
            let canonical_options =
                crate::backend::dynamic::identity_options(self.id(), &tv.options)?;
            let public_checksum = canonical_options
                .get("sha256")
                .ok_or_else(|| Error::config("sha256 is required for HTTP artifacts"))?;
            if &checksum.hex != public_checksum {
                return Err(Error::config(
                    "locked HTTP artifact checksum does not match the public sha256 option",
                ));
            }
            return Ok(HttpArtifact {
                kind: kind_from_options_or_name(&tv.options, &self.template)?,
                url: locked.url,
                file_name: locked.file_name,
                checksum,
            });
        }

        let url = render_template(&self.template, &tv.version)?;
        validate_rendered_url(&url)?;
        let file_name = download_file_name(&url)?;
        let checksum = Checksum {
            algo: HashAlgo::Sha256,
            hex: tv
                .options
                .get("sha256")
                .ok_or_else(|| Error::config("sha256 is required for HTTP artifacts"))?
                .clone(),
        };
        Ok(HttpArtifact {
            kind: kind_from_options_or_name(&tv.options, &file_name)?,
            url,
            file_name,
            checksum,
        })
    }

    fn locator(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        artifact: &HttpArtifact,
    ) -> Result<InstallLocator> {
        let materials = BTreeMap::from([
            ("artifact-file".into(), artifact.file_name.clone()),
            (
                "artifact-checksum".into(),
                format!("sha256:{}", artifact.checksum.hex),
            ),
            (
                "artifact-url-blake3".into(),
                artifact_url_hash(&artifact.url),
            ),
        ]);
        let identity = InstallIdentity::new(
            self.id(),
            &tv.version,
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &tv.options,
            Vec::new(),
            materials,
        )?;
        InstallLocator::new(&ctx.dirs, identity)
    }

    /// Derive an exact fingerprinted locator from public and lock-replay
    /// options without performing DNS or I/O.
    pub fn install_locator_for(
        dirs: &crate::dirs::Dirs,
        platform: crate::platform::Platform,
        backend_id: &str,
        tv: &ToolVersion,
    ) -> Result<InstallLocator> {
        let backend = Self::from_id(backend_id)
            .ok_or_else(|| Error::UnknownBackend(backend_id.to_string()))?;
        let artifact = backend.artifact(tv)?;
        let materials = BTreeMap::from([
            ("artifact-file".into(), artifact.file_name),
            (
                "artifact-checksum".into(),
                format!("sha256:{}", artifact.checksum.hex),
            ),
            (
                "artifact-url-blake3".into(),
                artifact_url_hash(&artifact.url),
            ),
        ]);
        let identity = InstallIdentity::new(
            backend.id(),
            &tv.version,
            platform.to_string(),
            InstallScope::Isolated,
            &tv.options,
            Vec::new(),
            materials,
        )?;
        InstallLocator::new(dirs, identity)
    }

    pub(crate) fn installed_locator(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<InstallLocator> {
        let artifact = self.artifact(tv)?;
        self.locator(ctx, tv, &artifact)
    }

    /// Validate an inventory candidate without network access. Lifecycle and
    /// lockfile callers use this narrow HTTP-specific boundary.
    pub fn install_candidate_is_valid(
        dirs: &crate::dirs::Dirs,
        install_root: &Path,
        identity: &InstallIdentity,
    ) -> Result<bool> {
        if !crate::backend::dynamic::artifact_install_candidate_is_valid(
            dirs,
            install_root,
            identity,
        )? {
            return Ok(false);
        }
        if !identity.tool.starts_with("http:") {
            return Ok(false);
        }
        let receipt = crate::pipeline::artifact_receipt_at(install_root)
            .ok_or_else(|| Error::other("HTTP artifact receipt is missing or invalid"))?;
        validate_rendered_url(&receipt.url)?;
        let expected_url = identity
            .materials
            .get("artifact-url-blake3")
            .ok_or_else(|| Error::other("HTTP install identity is missing its URL fingerprint"))?;
        let expected_checksum = identity
            .materials
            .get("artifact-checksum")
            .ok_or_else(|| Error::other("HTTP install identity is missing its checksum"))?;
        if artifact_url_hash(&receipt.url) != *expected_url
            || receipt.checksum.as_deref() != Some(expected_checksum.as_str())
            || !receipt.evidence.is_empty()
        {
            return Err(Error::other(format!(
                "HTTP artifact receipt does not match install identity at {}",
                install_root.display()
            )));
        }
        Ok(true)
    }
}

#[async_trait]
impl Backend for HttpBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        Vec::new()
    }

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        None
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        Err(Error::other(
            "HTTP artifacts require an exact semantic version selector",
        ))
    }

    #[cfg(feature = "install")]
    async fn resolve_version(&self, _ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let VersionSpec::Exact(version) = &req.spec else {
            return Err(Error::VersionResolve {
                tool: self.id.clone(),
                spec: req.spec.to_string(),
                hint: Some("HTTP artifacts require an exact semantic version selector".into()),
            });
        };
        crate::backend::dynamic::validate_options(self.id(), &req.options)?;
        let mut resolved = ToolVersion::new(self.id(), version);
        resolved.options = req.options.clone();
        Ok(resolved)
    }

    #[cfg(feature = "install")]
    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        crate::backend::dynamic::validate_options(self.id(), &tv.options)?;
        if !matches!(VersionSpec::parse(&tv.version), VersionSpec::Exact(ref exact) if exact == &tv.version)
        {
            return Err(Error::config(
                "HTTP artifacts require an exact semantic version selector",
            ));
        }
        let artifact = self.artifact(tv)?;
        let locator = self.locator(ctx, tv, &artifact)?;
        let _lock = crate::backend::dynamic::acquire_install_lock(&locator, "HTTP").await?;
        let root = locator.install_root();
        if root.join(".osdk-complete").exists() {
            if Self::install_candidate_is_valid(&ctx.dirs, root, locator.identity())? {
                return Ok(());
            }
            return Err(Error::other(format!(
                "refusing to reuse incomplete or invalid HTTP artifact install at {}",
                root.display()
            )));
        }

        match artifact.kind {
            HttpArtifactKind::File => {
                let name = tv
                    .options
                    .get("rename")
                    .map(String::as_str)
                    .unwrap_or_else(|| artifact.file_name.as_str());
                let name = executable_stem(name, ctx.platform.os)?;
                let cached = pipeline::dynamic_artifact_cache_path(
                    &ctx.dirs,
                    &locator,
                    &artifact.file_name,
                )?;
                prepare_cached_artifact(
                    ctx,
                    &artifact.url,
                    &artifact.file_name,
                    &artifact.checksum,
                    &cached,
                )
                .await?;
                let client = offline_client()?;
                pipeline::install_single_binary_unfinalized_at(
                    &client,
                    &ctx.dirs,
                    &locator,
                    std::slice::from_ref(&artifact.url),
                    &name,
                    &artifact.file_name,
                    ctx.platform.os,
                    Some(&artifact.checksum),
                    ctx.show_progress,
                    true,
                    true,
                    None,
                )
                .await?;
            }
            kind => {
                let plan = InstallPlan {
                    tool: self.id.clone(),
                    version: tv.version.clone(),
                    urls: vec![artifact.url],
                    file_name: artifact.file_name,
                    kind: kind.archive_kind().expect("archive branch"),
                    checksum: Some(artifact.checksum),
                    strip_root: false,
                    subdir: tv.options.get("subdir").map(PathBuf::from),
                };
                let cached =
                    pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, &plan.file_name)?;
                prepare_cached_artifact(
                    ctx,
                    &plan.urls[0],
                    &plan.file_name,
                    plan.checksum
                        .as_ref()
                        .expect("HTTP plans always have checksums"),
                    &cached,
                )
                .await?;
                let client = offline_client()?;
                validate_archive_entries(&cached, kind)?;
                let pipeline_ctx = PipelineCtx {
                    client: &client,
                    dirs: &ctx.dirs,
                    cas: &ctx.cas,
                    // Arbitrary archives may contain links. Materialize real
                    // files so the final no-symlink validation is meaningful.
                    link_mode: crate::store::link::LinkMode::Copy,
                    show_progress: ctx.show_progress,
                    // The artifact is already downloaded and security-checked
                    // above. Force the shared pipeline to consume only that
                    // exact cache entry and never perform a second request.
                    offline: true,
                    require_checksums: true,
                };
                pipeline::run_with_attestation_unfinalized_at(&plan, &pipeline_ctx, None, &locator)
                    .await?;
                postprocess_archive(ctx, &locator, &tv.options)?;
            }
        }
        crate::backend::dynamic::finalize_artifact_install(&locator)
    }

    #[cfg(feature = "install")]
    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let locator = self.installed_locator(ctx, tv)?;
        let _lock = crate::backend::dynamic::acquire_install_lock(&locator, "HTTP").await?;
        match std::fs::symlink_metadata(locator.install_root()) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(locator.install_root())
                    .map_err(|error| Error::io(locator.install_root(), error))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(Error::other(
                "refusing to remove non-directory HTTP install root",
            )),
            Err(error) => Err(Error::io(locator.install_root(), error)),
        }
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        let report = crate::inventory::scan_installs(
            &ctx.dirs.installs,
            &crate::inventory::ScanOptions::default(),
        )?;
        let mut versions = std::collections::BTreeSet::new();
        for install in report.installs {
            if install.manifest.identity.tool != self.id
                || install.manifest.identity.platform != ctx.platform.to_string()
                || install.manifest.identity.scope != InstallScope::Isolated
            {
                continue;
            }
            if Self::install_candidate_is_valid(
                &ctx.dirs,
                &install.install_root,
                &install.manifest.identity,
            )? {
                versions.insert(install.manifest.identity.version);
            }
        }
        Ok(versions.into_iter().collect())
    }

    fn ensure_post_install(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let locator = self.installed_locator(ctx, tv)?;
        if Self::install_candidate_is_valid(&ctx.dirs, locator.install_root(), locator.identity())?
        {
            Ok(())
        } else {
            Err(Error::other("HTTP artifact install is incomplete"))
        }
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = self
            .installed_locator(ctx, tv)?
            .install_root()
            .to_path_buf();
        let bin = root.join("bin");
        Ok(if bin.is_dir() {
            vec![bin, root]
        } else {
            vec![root]
        })
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let locator = self.installed_locator(ctx, tv)?;
        Ok(
            crate::inventory::DynamicToolManifest::load(locator.install_root())?
                .bins
                .into_iter()
                .map(|bin| bin.name)
                .collect(),
        )
    }

    fn dynamic_install_identity(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<InstallIdentity>> {
        self.installed_locator(ctx, tv)
            .map(|locator| Some(locator.identity().clone()))
    }

    fn validate_dynamic_install(
        &self,
        ctx: &Ctx,
        _tv: &ToolVersion,
        install_root: &Path,
        identity: &InstallIdentity,
    ) -> Result<bool> {
        Self::install_candidate_is_valid(&ctx.dirs, install_root, identity)
    }
}

impl HttpArtifactKind {
    fn archive_kind(self) -> Option<ArchiveKind> {
        match self {
            Self::TarGz => Some(ArchiveKind::TarGz),
            Self::TarXz => Some(ArchiveKind::TarXz),
            Self::Zip => Some(ArchiveKind::Zip),
            Self::File => None,
        }
    }
}

fn kind_from_options_or_name(
    options: &BTreeMap<String, String>,
    file_name: &str,
) -> Result<HttpArtifactKind> {
    match options.get("kind").map(String::as_str) {
        Some("tar.gz") => Ok(HttpArtifactKind::TarGz),
        Some("tar.xz") => Ok(HttpArtifactKind::TarXz),
        Some("zip") => Ok(HttpArtifactKind::Zip),
        Some("file") => Ok(HttpArtifactKind::File),
        Some(other) => Err(Error::config(format!(
            "invalid HTTP artifact kind `{other}`"
        ))),
        None => match ArchiveKind::from_name(file_name) {
            Ok(ArchiveKind::TarGz) => Ok(HttpArtifactKind::TarGz),
            Ok(ArchiveKind::TarXz) => Ok(HttpArtifactKind::TarXz),
            Ok(ArchiveKind::Zip) => Ok(HttpArtifactKind::Zip),
            Ok(ArchiveKind::TarZst) => Err(Error::config(
                "HTTP artifacts support tar.gz, tar.xz, zip, or file",
            )),
            // `.7z` is extractable, but the inline `http:` identity deliberately
            // exposes a narrower archive set; declarative plugins cover it.
            #[cfg(feature = "install")]
            Ok(ArchiveKind::SevenZ) => Err(Error::config(
                "HTTP artifacts support tar.gz, tar.xz, zip, or file",
            )),
            Err(_) => Ok(HttpArtifactKind::File),
        },
    }
}

fn render_template(template: &str, version: &str) -> Result<String> {
    if version.is_empty()
        || version.len() > 128
        || version.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '_' | '+')
        })
    {
        return Err(Error::config("unsafe HTTP artifact version"));
    }
    Ok(template.replace("{version}", version))
}

fn validate_rendered_url(value: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|error| Error::config(format!("invalid HTTP artifact URL: {error}")))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::config(
            "HTTP artifact URL must remain absolute HTTPS without credentials, query, or fragment",
        ));
    }
    let host = parsed.host_str().expect("host checked above");
    let literal = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if literal
        .parse::<IpAddr>()
        .is_ok_and(|address| !crate::tool::is_public_ip(address))
    {
        return Err(Error::config(
            "HTTP artifact URL must not target a non-public IP address",
        ));
    }
    Ok(())
}

fn artifact_url_hash(url: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-http-artifact-url-v1");
    hasher.update(url.as_bytes());
    hasher.finalize().to_hex().to_string()
}

async fn secure_client(url: &str) -> Result<reqwest::Client> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| Error::config(format!("invalid HTTP artifact URL: {error}")))?;
    let url_host = parsed
        .host_str()
        .ok_or_else(|| Error::config("HTTP artifact URL requires a host"))?;
    let host = url_host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(url_host);
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| Error::config("HTTP artifact URL requires a known port"))?;
    let addresses = resolve_public_addresses(host, port).await?;
    reqwest::Client::builder()
        .user_agent(concat!(
            "osdk/",
            env!("CARGO_PKG_VERSION"),
            " http-artifact"
        ))
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(HTTP_REQUEST_TIMEOUT)
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .resolve_to_addrs(host, &addresses)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if let Err(error) = validate_redirect(attempt.url(), attempt.previous()) {
                return attempt.error(error);
            }
            attempt.follow()
        }))
        .build()
        .map_err(Error::from)
}

fn offline_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Error::from)
}

async fn resolve_public_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        let endpoint = (host.to_string(), port);
        tokio::time::timeout(
            DNS_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                endpoint
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect::<Vec<_>>())
            }),
        )
        .await
        .map_err(|_| Error::other(format!("HTTP artifact DNS lookup timed out for `{host}`")))?
        .map_err(|error| {
            Error::other(format!(
                "HTTP artifact DNS task failed for `{host}`: {error}"
            ))
        })?
        .map_err(|error| {
            Error::other(format!(
                "HTTP artifact DNS lookup failed for `{host}`: {error}"
            ))
        })?
    };
    if addresses.is_empty() {
        return Err(Error::other(format!(
            "HTTP artifact DNS lookup returned no addresses for `{host}`"
        )));
    }
    if let Some(address) = addresses
        .iter()
        .map(SocketAddr::ip)
        .find(|address| !crate::tool::is_public_ip(*address))
    {
        return Err(Error::other(format!(
            "HTTP artifact destination `{host}` resolved to forbidden address {address}"
        )));
    }
    let mut addresses = addresses;
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(Error::other(format!(
            "HTTP artifact DNS lookup returned no usable addresses for `{host}`"
        )));
    }
    Ok(addresses)
}

fn validate_redirect(
    next: &reqwest::Url,
    previous: &[reqwest::Url],
) -> std::result::Result<(), &'static str> {
    let Some(initial) = previous.first() else {
        return Err("HTTP artifact redirect has no origin");
    };
    if previous.len() >= 10 {
        return Err("HTTP artifact redirect limit exceeded");
    }
    if next.scheme() != "https" {
        return Err("HTTP artifact redirects must remain on HTTPS");
    }
    if !next.username().is_empty()
        || next.password().is_some()
        || next.query().is_some()
        || next.fragment().is_some()
    {
        return Err("HTTP artifact redirect must not contain credentials, query, or fragment");
    }
    if initial.host_str() != next.host_str()
        || initial.port_or_known_default() != next.port_or_known_default()
    {
        return Err("HTTP artifact redirect must remain on the original origin");
    }
    if previous.iter().any(|url| url == next) {
        return Err("HTTP artifact redirect loop detected");
    }
    Ok(())
}

async fn prepare_cached_artifact(
    ctx: &Ctx,
    url: &str,
    file_name: &str,
    checksum: &Checksum,
    cached: &Path,
) -> Result<()> {
    if cached.exists() {
        let validation = validate_cached_artifact(cached).and_then(|()| {
            pipeline::verify::verify_file(cached, &checksum.hex, checksum.algo, file_name)
        });
        match validation {
            Ok(()) => return Ok(()),
            Err(error) if ctx.config.settings.offline => return Err(error),
            Err(error) => match std::fs::symlink_metadata(cached) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    std::fs::remove_file(cached).map_err(|remove| Error::io(cached, remove))?;
                }
                _ => return Err(error),
            },
        }
    } else if ctx.config.settings.offline {
        return Err(Error::other(format!(
            "offline artifact cache miss for {}",
            cached.display()
        )));
    }
    let client = secure_client(url).await?;
    download_bounded(&client, url, cached).await?;
    validate_cached_artifact(cached)?;
    pipeline::verify::verify_file(cached, &checksum.hex, checksum.algo, file_name)
}

fn validate_cached_artifact(cached: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(cached).map_err(|error| Error::io(cached, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::other(
            "HTTP artifact cache entry must be a regular non-symlink file",
        ));
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::other(format!(
            "HTTP artifact exceeds the {MAX_ARTIFACT_BYTES} byte download limit"
        )));
    }
    Ok(())
}

async fn download_bounded(client: &reqwest::Client, url: &str, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        crate::dirs::create_dir_all(parent)?;
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| Error::network(url, error))?
        .error_for_status()
        .map_err(|error| Error::network(url, error))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARTIFACT_BYTES)
    {
        return Err(Error::other(format!(
            "HTTP artifact exceeds the {MAX_ARTIFACT_BYTES} byte download limit"
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::other("HTTP artifact cache path has no parent"))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".osdk-http-download.")
        .tempfile_in(parent)
        .map_err(|error| Error::io(parent, error))?;
    let result = async {
        let mut stream = response.bytes_stream();
        let mut downloaded = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Error::network(url, error))?;
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| Error::other("HTTP artifact download size overflow"))?;
            if downloaded > MAX_ARTIFACT_BYTES {
                return Err(Error::other(format!(
                    "HTTP artifact exceeds the {MAX_ARTIFACT_BYTES} byte download limit"
                )));
            }
            temporary
                .write_all(&chunk)
                .map_err(|error| Error::io(temporary.path(), error))?;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| Error::io(temporary.path(), error))?;
        temporary
            .persist(destination)
            .map_err(|error| Error::io(destination, error.error))?;
        Ok(())
    }
    .await;
    result
}

fn validate_archive_entries(path: &Path, kind: HttpArtifactKind) -> Result<()> {
    match kind {
        HttpArtifactKind::TarGz => {
            let file = std::fs::File::open(path).map_err(|error| Error::io(path, error))?;
            validate_tar_entries(flate2::read::GzDecoder::new(std::io::BufReader::new(file)))
        }
        HttpArtifactKind::TarXz => {
            let file = std::fs::File::open(path).map_err(|error| Error::io(path, error))?;
            validate_tar_entries(xz2::read::XzDecoder::new(std::io::BufReader::new(file)))
        }
        HttpArtifactKind::Zip => validate_zip_entries(path),
        HttpArtifactKind::File => Ok(()),
    }
}

fn validate_tar_entries(reader: impl std::io::Read) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut count = 0_usize;
    let mut expanded = 0_u64;
    let mut paths = std::collections::BTreeSet::new();
    for entry in archive
        .entries()
        .map_err(|error| Error::other(format!("invalid HTTP tar archive: {error}")))?
    {
        let entry =
            entry.map_err(|error| Error::other(format!("invalid HTTP tar entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| Error::other(format!("invalid HTTP tar path: {error}")))?;
        validate_archive_relative_path(&path)?;
        register_archive_path(&mut paths, &path)?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(Error::other(format!(
                "HTTP archives may contain only regular files and directories: {}",
                path.display()
            )));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| Error::other("HTTP archive entry count overflow"))?;
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| Error::other("HTTP archive expanded size overflow"))?;
        validate_archive_limits(count, expanded)?;
    }
    Ok(())
}

fn validate_zip_entries(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path).map_err(|error| Error::io(path, error))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|error| Error::other(format!("invalid HTTP zip archive: {error}")))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(Error::other(format!(
            "HTTP archive exceeds the {MAX_ARCHIVE_ENTRIES} entry limit"
        )));
    }
    let mut expanded = 0_u64;
    let mut paths = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| Error::other(format!("invalid HTTP zip entry: {error}")))?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| Error::other(format!("unsafe HTTP zip entry `{}`", entry.name())))?;
        validate_archive_relative_path(&enclosed)?;
        register_archive_path(&mut paths, &enclosed)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(Error::other(format!(
                "HTTP archives may not contain symlinks: {}",
                entry.name()
            )));
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| Error::other("HTTP archive expanded size overflow"))?;
        validate_archive_limits(index + 1, expanded)?;
    }
    Ok(())
}

fn validate_archive_limits(entries: usize, expanded_bytes: u64) -> Result<()> {
    if entries > MAX_ARCHIVE_ENTRIES {
        return Err(Error::other(format!(
            "HTTP archive exceeds the {MAX_ARCHIVE_ENTRIES} entry limit"
        )));
    }
    if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
        return Err(Error::other(format!(
            "HTTP archive exceeds the {MAX_ARCHIVE_EXPANDED_BYTES} byte expanded-size limit"
        )));
    }
    Ok(())
}

fn validate_archive_relative_path(path: &Path) -> Result<()> {
    crate::tool::canonical_safe_relative_path("HTTP archive entry", &path.to_string_lossy())
        .map(|_| ())
}

fn register_archive_path(
    paths: &mut std::collections::BTreeSet<String>,
    path: &Path,
) -> Result<()> {
    let canonical =
        crate::tool::canonical_safe_relative_path("HTTP archive entry", &path.to_string_lossy())?;
    let windows_key = canonical.to_ascii_lowercase();
    if !paths.insert(windows_key) {
        return Err(Error::other(format!(
            "HTTP archive contains a cross-platform path collision: {}",
            path.display()
        )));
    }
    Ok(())
}

fn download_file_name(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| Error::config(format!("invalid HTTP artifact URL: {error}")))?;
    let name = parsed
        .path_segments()
        .and_then(Iterator::last)
        .ok_or_else(|| Error::config("HTTP artifact URL requires a filename"))?;
    pipeline::validate_safe_filename("HTTP artifact filename", name)?;
    Ok(name.to_string())
}

fn postprocess_archive(
    ctx: &Ctx,
    locator: &InstallLocator,
    options: &BTreeMap<String, String>,
) -> Result<()> {
    let root = locator.install_root();
    crate::backend::dynamic::reject_symlinks(root)?;
    let count = options
        .get("strip-components")
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|error| Error::config(format!("invalid strip-components: {error}")))?
        .unwrap_or(0);
    let base = descend_unique(root, count)?;
    let Some(bins) = options.get("bins") else {
        return Ok(());
    };
    let sources = bins.split(',').map(PathBuf::from).collect::<Vec<_>>();
    if options.get("rename").is_some() && sources.len() != 1 {
        return Err(Error::config(
            "rename requires exactly one HTTP archive bin",
        ));
    }
    let bin_dir = root.join("bin");
    crate::dirs::create_dir_all(&bin_dir)?;
    let canonical_root = dunce::canonicalize(root).map_err(|error| Error::io(root, error))?;
    for source in sources {
        let source_path = base.join(&source);
        let metadata = std::fs::symlink_metadata(&source_path)
            .map_err(|error| Error::io(&source_path, error))?;
        let canonical =
            dunce::canonicalize(&source_path).map_err(|error| Error::io(&source_path, error))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !canonical.starts_with(&canonical_root)
        {
            return Err(Error::other(format!(
                "configured HTTP binary is unsafe: {}",
                source.display()
            )));
        }
        if ctx.platform.os == crate::platform::Os::Windows {
            let source_name = source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::config("configured HTTP bin has no safe filename"))?;
            executable_stem(source_name, ctx.platform.os)?;
        }
        let name = options
            .get("rename")
            .cloned()
            .or_else(|| {
                source
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or_else(|| Error::config("configured HTTP bin has no filename"))?;
        let destination = bin_dir.join(executable_name(&name, ctx.platform.os)?);
        if destination.exists() {
            return Err(Error::other(format!(
                "HTTP archive maps multiple executables to `{}`",
                destination.display()
            )));
        }
        std::fs::copy(&source_path, &destination)
            .map_err(|error| Error::io(&destination, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| Error::io(&destination, error))?;
        }
    }
    Ok(())
}

fn descend_unique(root: &Path, count: u32) -> Result<PathBuf> {
    let mut selected = root.to_path_buf();
    for _ in 0..count {
        let mut children = std::fs::read_dir(&selected)
            .map_err(|error| Error::io(&selected, error))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.retain(|entry| !entry.file_name().to_string_lossy().starts_with(".osdk-"));
        if children.len() != 1 || !children[0].path().is_dir() {
            return Err(Error::other(format!(
                "strip-components cannot descend through {}",
                selected.display()
            )));
        }
        selected = children.remove(0).path();
    }
    Ok(selected)
}

fn executable_name(name: &str, os: crate::platform::Os) -> Result<String> {
    if os == crate::platform::Os::Windows {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return Err(Error::config(
                "HTTP artifacts support only native .exe executables on Windows",
            ));
        }
        if !lower.ends_with(".exe") {
            return Ok(format!("{name}.exe"));
        }
    }
    Ok(name.to_string())
}

fn executable_stem(name: &str, os: crate::platform::Os) -> Result<String> {
    if os == crate::platform::Os::Windows
        && matches!(
            Path::new(name)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("cmd") | Some("bat")
        )
    {
        return Err(Error::config(
            "HTTP artifacts support only native .exe executables on Windows",
        ));
    }
    if os == crate::platform::Os::Windows && name.to_ascii_lowercase().ends_with(".exe") {
        return Ok(name[..name.len() - ".exe".len()].to_string());
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::inventory::DynamicToolManifest;
    use crate::platform::{Arch, Libc, Os, Platform};
    use crate::source::Selection;
    use crate::store::Cas;

    #[test]
    fn parses_only_strict_https_templates() {
        assert!(HttpBackend::from_id("http:https://example.test/tool-{version}.tar.gz").is_some());
        for invalid in [
            "http:http://example.test/tool-{version}.tar.gz",
            "http:https://user@example.test/tool-{version}.tar.gz",
            "http:https://example.test/tool-{version}.tar.gz?token=x",
            "http:https://example.test/tool.tar.gz",
        ] {
            assert!(HttpBackend::from_id(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn redirect_policy_rejects_cross_origin_downgrade_and_loops() {
        let initial = reqwest::Url::parse("https://example.test/start").unwrap();
        let same = reqwest::Url::parse("https://example.test/final").unwrap();
        assert_eq!(
            validate_redirect(&same, std::slice::from_ref(&initial)),
            Ok(())
        );
        let downgrade = reqwest::Url::parse("http://example.test/final").unwrap();
        assert!(validate_redirect(&downgrade, std::slice::from_ref(&initial)).is_err());
        let cross = reqwest::Url::parse("https://other.test/final").unwrap();
        assert!(validate_redirect(&cross, std::slice::from_ref(&initial)).is_err());
        assert!(validate_redirect(&initial, std::slice::from_ref(&initial)).is_err());
    }

    #[test]
    fn archive_validation_rejects_traversal_and_links() {
        for path in [
            "../escape",
            "/absolute",
            "safe/../../escape",
            r"bin\tool",
            "C:/tool",
            "bin/tool.",
            "bin/AUX",
        ] {
            assert!(validate_archive_relative_path(Path::new(path)).is_err());
        }

        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("link.tar.gz");
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("../../outside").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/tool", std::io::empty())
            .unwrap();
        builder.finish().unwrap();
        assert!(validate_archive_entries(&archive, HttpArtifactKind::TarGz).is_err());

        let mut paths = std::collections::BTreeSet::new();
        register_archive_path(&mut paths, Path::new("bin/Tool")).unwrap();
        assert!(register_archive_path(&mut paths, Path::new("BIN/tool")).is_err());
    }

    #[test]
    fn public_address_policy_rejects_local_metadata_and_mapped_addresses() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(
                !crate::tool::is_public_ip(address.parse().unwrap()),
                "{address}"
            );
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                crate::tool::is_public_ip(address.parse().unwrap()),
                "{address}"
            );
        }
    }

    #[test]
    fn archive_limits_are_fail_closed() {
        assert!(validate_archive_limits(MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_EXPANDED_BYTES).is_ok());
        assert!(validate_archive_limits(MAX_ARCHIVE_ENTRIES + 1, 0)
            .unwrap_err()
            .to_string()
            .contains("entry limit"));
        assert!(validate_archive_limits(1, MAX_ARCHIVE_EXPANDED_BYTES + 1)
            .unwrap_err()
            .to_string()
            .contains("expanded-size limit"));
    }

    #[test]
    fn windows_batch_executables_fail_closed() {
        for name in ["tool.cmd", "TOOL.BAT"] {
            assert!(executable_stem(name, Os::Windows).is_err(), "{name}");
        }
        assert_eq!(executable_stem("tool.exe", Os::Windows).unwrap(), "tool");
        assert_eq!(executable_stem("Tool.EXE", Os::Windows).unwrap(), "Tool");
        assert_eq!(executable_name("tool", Os::Windows).unwrap(), "tool.exe");
        assert_eq!(
            executable_name("Tool.EXE", Os::Windows).unwrap(),
            "Tool.EXE"
        );
        assert!(executable_name("tool.cmd", Os::Windows).is_err());
    }

    #[test]
    fn locked_checksum_must_match_public_sha256() {
        let backend =
            HttpBackend::from_id("http:https://downloads.example.test/tool-{version}").unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = BTreeMap::from([
            ("sha256".into(), "a".repeat(64)),
            ("kind".into(), "file".into()),
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://downloads.example.test/tool-1.2.3".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "tool-1.2.3".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{}", "b".repeat(64)),
            ),
        ]);
        let error = backend.artifact(&version).unwrap_err();
        assert!(error.to_string().contains("public sha256"), "{error}");
    }

    #[tokio::test]
    async fn offline_cache_hit_never_resolves_the_artifact_host() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let cached = temp.path().join("cached-tool");
        let bytes = b"fixture";
        std::fs::write(&cached, bytes).unwrap();
        let checksum = Checksum {
            algo: HashAlgo::Sha256,
            hex: pipeline::verify::hash_bytes(bytes, HashAlgo::Sha256),
        };
        prepare_cached_artifact(
            &ctx,
            "https://does-not-resolve.invalid/tool",
            "tool",
            &checksum,
            &cached,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dns_resolution_rejects_non_public_results() {
        let error = resolve_public_addresses("localhost", 443)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("forbidden address"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn locked_bare_file_replays_offline_and_concurrent_installs_serialize() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let backend =
            Arc::new(HttpBackend::from_id("http:https://changed.invalid/tool-{version}").unwrap());
        let bytes = b"fixture executable";
        let digest = pipeline::verify::hash_bytes(bytes, HashAlgo::Sha256);
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "file".into()),
            (
                "rename".into(),
                if cfg!(windows) {
                    "fixture.exe"
                } else {
                    "fixture"
                }
                .into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://unreachable.invalid/original".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "original".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]);
        let artifact = backend.artifact(&version).unwrap();
        assert_eq!(artifact.url, "https://unreachable.invalid/original");
        let locator = backend.locator(&ctx, &version, &artifact).unwrap();
        let cached =
            pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, "original").unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, bytes).unwrap();

        let ctx = Arc::new(ctx);
        let first = {
            let backend = backend.clone();
            let ctx = ctx.clone();
            let version = version.clone();
            tokio::spawn(async move { backend.install(&InstallCtx { ctx: &ctx }, &version).await })
        };
        let second = {
            let backend = backend.clone();
            let ctx = ctx.clone();
            let version = version.clone();
            tokio::spawn(async move { backend.install(&InstallCtx { ctx: &ctx }, &version).await })
        };
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(
            std::fs::read(locator.install_root().join("bin").join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }))
            .unwrap(),
            bytes
        );
        assert!(locator.install_root().join(".osdk-complete").is_file());
        assert!(
            crate::backend::dynamic::artifact_install_candidate_is_valid(
                &ctx.dirs,
                locator.install_root(),
                locator.identity()
            )
            .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninstall_waits_for_the_identity_lock() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = Arc::new(test_ctx(temp.path()));
        let backend =
            Arc::new(HttpBackend::from_id("http:https://example.test/tool-{version}").unwrap());
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = BTreeMap::from([
            ("sha256".into(), "a".repeat(64)),
            ("kind".into(), "file".into()),
            (
                "rename".into(),
                if cfg!(windows) {
                    "fixture.exe"
                } else {
                    "fixture"
                }
                .into(),
            ),
        ]);
        let artifact = backend.artifact(&version).unwrap();
        let locator = backend.locator(&ctx, &version, &artifact).unwrap();
        std::fs::create_dir_all(locator.install_root()).unwrap();
        let held = crate::backend::dynamic::acquire_install_lock(&locator, "test")
            .await
            .unwrap();
        let root = locator.install_root().to_path_buf();
        let uninstall = {
            let backend = backend.clone();
            let ctx = ctx.clone();
            let version = version.clone();
            tokio::spawn(async move { backend.uninstall(&ctx, &version).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!uninstall.is_finished());
        assert!(root.exists());
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), uninstall)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn checksum_mismatch_never_publishes_completion() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let backend = HttpBackend::from_id("http:https://example.test/tool-{version}.zip").unwrap();
        let digest = "a".repeat(64);
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "zip".into()),
            ("bins".into(), "bin/tool".into()),
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://example.test/tool.zip".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "tool.zip".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]);
        let artifact = backend.artifact(&version).unwrap();
        let locator = backend.locator(&ctx, &version, &artifact).unwrap();
        let cached =
            pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, "tool.zip").unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"wrong bytes").unwrap();
        assert!(backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .is_err());
        assert!(!locator.install_root().join(".osdk-complete").exists());
    }

    #[tokio::test]
    async fn locked_archive_replays_offline_with_safe_layout() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let backend =
            HttpBackend::from_id("http:https://changed.invalid/tool-{version}.tar.gz").unwrap();
        let archive = temp.path().join("fixture.tar.gz");
        let archived_name = if cfg!(windows) {
            "package/dist/tool.exe"
        } else {
            "package/dist/tool"
        };
        write_archive(&archive, archived_name, b"archive executable");
        let digest = pipeline::verify::hash_file(&archive, HashAlgo::Sha256).unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "tar.gz".into()),
            (
                "bins".into(),
                if cfg!(windows) {
                    "dist/tool.exe"
                } else {
                    "dist/tool"
                }
                .into(),
            ),
            ("strip-components".into(), "1".into()),
            (
                "rename".into(),
                if cfg!(windows) {
                    "fixture.exe"
                } else {
                    "fixture"
                }
                .into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://unreachable.invalid/original.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "original.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]);
        let artifact = backend.artifact(&version).unwrap();
        let locator = backend.locator(&ctx, &version, &artifact).unwrap();
        let cached =
            pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, "original.tar.gz").unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::copy(&archive, &cached).unwrap();

        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(locator.install_root().join("bin").join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            }))
            .unwrap(),
            b"archive executable"
        );
        let manifest = DynamicToolManifest::load(locator.install_root()).unwrap();
        assert_eq!(manifest.bins[0].name, "fixture");
    }

    fn write_archive(path: &Path, name: &str, bytes: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
        builder.finish().unwrap();
    }

    fn test_ctx(root: &Path) -> Ctx {
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
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            },
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig {
                    selection: Selection::Ordered,
                    ..Default::default()
                },
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
}
