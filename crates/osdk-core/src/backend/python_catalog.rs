use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::Ctx;
use crate::config::PrereleasePolicy;
use crate::error::{Error, Result};
use crate::pipeline::{ArchiveKind, Checksum, HashAlgo, InstallPlan};
use crate::platform::{Arch, Libc, Os};
use crate::version::{select_version, ToolRequest, ToolVersion, VersionInfo, VersionSpec};

const BUILTIN: &[u8] = include_bytes!("python_catalog.json");
const CACHE_PREFIX: &str = "python-download-catalog";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub schema: u32,
    pub source: String,
    pub source_sha256: String,
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub implementation: String,
    pub version: String,
    #[serde(default = "default_variant")]
    pub variant: String,
    pub os: String,
    pub arch: String,
    pub libc: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub subdir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonRequest {
    pub implementation: String,
    pub spec: VersionSpec,
    pub variant: String,
    pub explicit_prerelease: bool,
}

fn default_variant() -> String {
    "default".into()
}

impl PythonRequest {
    pub fn parse(request: &ToolRequest) -> Result<Self> {
        let raw = request.spec.to_string();
        let (implementation, version) = split_implementation(&raw);
        let (version, inline_variant) = match version.split_once('+') {
            Some((version, variant)) => (version, Some(variant)),
            None => (version, None),
        };
        let variant = request
            .options
            .get("variant")
            .map(String::as_str)
            .or(inline_variant)
            .unwrap_or("default");
        validate_implementation(implementation)?;
        validate_variant(implementation, variant)?;
        Ok(Self {
            implementation: implementation.into(),
            spec: VersionSpec::parse(version),
            variant: variant.into(),
            explicit_prerelease: explicit_prerelease(version),
        })
    }

    pub fn identity(&self, version: &str) -> String {
        if self.implementation == "cpython" && self.variant == "default" {
            version.to_string()
        } else {
            let variant = if self.variant != "default" {
                format!("+{}", self.variant)
            } else {
                String::new()
            };
            format!("{}-{version}{variant}", self.implementation)
        }
    }
}

pub async fn load(ctx: &Ctx) -> Result<Catalog> {
    let builtin = parse_catalog(BUILTIN, "embedded python catalog")?;
    let settings = &ctx.config.settings.python;
    let Some(source) = settings.catalog_url.as_deref() else {
        return Ok(builtin);
    };
    let Some(expected) = settings.catalog_sha256.as_deref() else {
        return Err(Error::config(
            "settings.python.catalog_sha256 is required with catalog_url",
        ));
    };
    let digest = expected.trim().to_ascii_lowercase();
    let cache = ctx
        .dirs
        .remote_cache()
        .join(format!("{CACHE_PREFIX}-{digest}.json"));
    let local_source = !source.starts_with("http://") && !source.starts_with("https://");
    if !ctx.config.settings.offline {
        match read_source(ctx, source).await {
            Ok(bytes) => match verify_catalog(&bytes, expected, source) {
                Ok(catalog) => {
                    atomic_write(&cache, &bytes)?;
                    return Ok(catalog);
                }
                Err(error) => {
                    tracing::warn!(%error, "python catalog verification failed; using last-good or built-in");
                }
            },
            Err(error) => {
                tracing::warn!(%error, "python catalog refresh failed; using last-good or built-in");
            }
        }
    }
    if ctx.config.settings.offline && local_source {
        if let Ok(bytes) = read_source(ctx, source).await {
            match verify_catalog(&bytes, expected, source) {
                Ok(catalog) => return Ok(catalog),
                Err(error) => {
                    tracing::warn!(%error, "local python catalog is invalid; using last-good or built-in");
                }
            }
        }
    }

    if let Ok(bytes) = std::fs::read(&cache) {
        match verify_catalog(&bytes, expected, source) {
            Ok(catalog) => return Ok(catalog),
            Err(error) => {
                tracing::warn!(%error, "cached python catalog is invalid; using built-in");
            }
        }
    }
    Ok(builtin)
}

pub fn resolve_catalog(
    catalog: &Catalog,
    request: &PythonRequest,
    ctx: &Ctx,
) -> Result<(ToolVersion, Option<CatalogEntry>)> {
    let matching: Vec<_> = catalog
        .entries
        .iter()
        .filter(|entry| entry_matches_platform(entry, ctx))
        .filter(|entry| {
            entry.implementation == request.implementation && entry.variant == request.variant
        })
        .filter(|entry| prerelease_allowed(&entry.version, request, ctx.config.settings.prerelease))
        .cloned()
        .collect();
    let mut versions: Vec<VersionInfo> = matching
        .iter()
        .map(|entry| VersionInfo {
            version: entry.version.clone(),
            stable: !is_prerelease(&entry.version),
            lts: None,
        })
        .collect();
    versions.sort_by(|left, right| super::python::cmp_versions(&left.version, &right.version));
    let effective_spec = match (&request.spec, ctx.config.settings.prerelease) {
        (VersionSpec::Latest, PrereleasePolicy::Allow) => versions.last(),
        _ => select_version(&request.spec, &versions),
    };
    let chosen = effective_spec.ok_or_else(|| Error::VersionResolve {
        tool: "python".into(),
        spec: request.spec.to_string(),
        hint: Some(format!(
            "no {} {} catalog entry for this platform",
            request.implementation, request.variant
        )),
    })?;
    let entry = matching
        .into_iter()
        .find(|entry| entry.version == chosen.version)
        .expect("version was built from matching catalog entries");
    let mut resolved = ToolVersion::new("python", request.identity(&entry.version));
    resolved
        .options
        .insert("implementation".into(), request.implementation.clone());
    resolved
        .options
        .insert("python-version".into(), entry.version.clone());
    resolved
        .options
        .insert("variant".into(), request.variant.clone());
    resolved.options.insert("catalog".into(), "true".into());
    let plan = install_plan(&resolved.version, &entry)?;
    resolved.options.insert(
        crate::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
        entry.url.clone(),
    );
    resolved.options.insert(
        crate::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
        plan.file_name,
    );
    resolved.options.insert(
        crate::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
        format!("sha256:{}", entry.sha256),
    );
    if let Some(subdir) = &entry.subdir {
        resolved
            .options
            .insert("catalog-subdir".into(), subdir.display().to_string());
        resolved.options.insert(
            crate::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION.into(),
            subdir.display().to_string(),
        );
    }
    Ok((resolved, Some(entry)))
}

pub fn resolved_options(
    request: &PythonRequest,
    version: &str,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("implementation".into(), request.implementation.clone()),
        ("python-version".into(), version.into()),
        ("variant".into(), request.variant.clone()),
    ])
}

pub fn select_installed(spec: &str, installed: &[String]) -> Option<String> {
    let request = ToolRequest::parse(&format!("python@{spec}")).ok()?;
    let parsed = PythonRequest::parse(&request).ok()?;
    let mut candidates = Vec::new();
    for identity in installed {
        let (implementation, version, variant) = parse_identity(identity);
        if implementation == parsed.implementation && variant == parsed.variant {
            candidates.push((
                identity,
                VersionInfo {
                    version: version.to_string(),
                    stable: !is_prerelease(version),
                    lts: None,
                },
            ));
        }
    }
    candidates
        .sort_by(|left, right| super::python::cmp_versions(&left.1.version, &right.1.version));
    let infos: Vec<_> = candidates.iter().map(|(_, info)| info.clone()).collect();
    let selected = select_version(&parsed.spec, &infos)?;
    candidates
        .into_iter()
        .find(|(_, info)| info.version == selected.version)
        .map(|(identity, _)| identity.clone())
}

pub fn install_plan(identity: &str, entry: &CatalogEntry) -> Result<InstallPlan> {
    let file_name = entry
        .url
        .split('/')
        .next_back()
        .unwrap_or("python-archive")
        .replace("%2B", "+");
    Ok(InstallPlan {
        tool: "python".into(),
        version: identity.into(),
        urls: vec![entry.url.clone()],
        file_name: file_name.clone(),
        kind: ArchiveKind::from_name(&file_name)?,
        checksum: Some(Checksum {
            algo: HashAlgo::Sha256,
            hex: entry.sha256.clone(),
        }),
        strip_root: true,
        subdir: entry.subdir.clone(),
    })
}

fn split_implementation(raw: &str) -> (&str, &str) {
    for implementation in ["cpython", "pypy", "graalpy", "pyodide"] {
        if raw == implementation {
            return (implementation, "latest");
        }
        if let Some(version) = raw.strip_prefix(&format!("{implementation}-")) {
            return (implementation, version);
        }
    }
    ("cpython", raw)
}

fn parse_identity(identity: &str) -> (&str, &str, &str) {
    let (implementation, version) = split_implementation(identity);
    match version.split_once('+') {
        Some((version, variant)) => (implementation, version, variant),
        None => (implementation, version, "default"),
    }
}

fn validate_implementation(value: &str) -> Result<()> {
    match value {
        "cpython" | "pypy" | "graalpy" | "pyodide" => Ok(()),
        _ => Err(Error::config(format!(
            "unsupported Python implementation `{value}`"
        ))),
    }
}

fn validate_variant(implementation: &str, value: &str) -> Result<()> {
    let valid = match implementation {
        "cpython" => matches!(
            value,
            "default" | "freethreaded" | "debug" | "freethreaded+debug"
        ),
        _ => value == "default",
    };
    if valid {
        Ok(())
    } else {
        Err(Error::config(format!(
            "unsupported Python variant `{value}` for {implementation}"
        )))
    }
}

fn prerelease_allowed(version: &str, request: &PythonRequest, policy: PrereleasePolicy) -> bool {
    if !is_prerelease(version) {
        return true;
    }
    match policy {
        PrereleasePolicy::Never => false,
        PrereleasePolicy::IfExplicit => request.explicit_prerelease,
        PrereleasePolicy::Allow => true,
    }
}

pub fn is_prerelease(version: &str) -> bool {
    semver::Version::parse(version)
        .map(|version| !version.pre.is_empty())
        .unwrap_or_else(|_| {
            version
                .chars()
                .any(|character| character.is_ascii_alphabetic())
        })
}

fn explicit_prerelease(version: &str) -> bool {
    semver::Version::parse(version)
        .map(|version| !version.pre.is_empty())
        .unwrap_or_else(|_| {
            let Some(patch_start) = version.rfind('.') else {
                return false;
            };
            version[patch_start + 1..]
                .chars()
                .any(|character| character.is_ascii_alphabetic())
        })
}

fn entry_matches_platform(entry: &CatalogEntry, ctx: &Ctx) -> bool {
    if entry.implementation == "pyodide" {
        return true;
    }
    entry.os == os_token(ctx.platform.os)
        && entry.arch == arch_token(ctx.platform.arch)
        && entry.libc == libc_token(ctx.platform.libc)
}

fn os_token(os: Os) -> &'static str {
    match os {
        Os::Linux => "linux",
        Os::Macos => "darwin",
        Os::Windows => "windows",
    }
}

fn arch_token(arch: Arch) -> &'static str {
    match arch {
        Arch::X64 => "x86_64",
        Arch::Arm64 => "aarch64",
        Arch::X86 => "i686",
        Arch::Arm => "armv7",
    }
}

fn libc_token(libc: Libc) -> &'static str {
    match libc {
        Libc::Glibc => "gnu",
        Libc::Musl => "musl",
        Libc::None => "none",
    }
}

fn parse_catalog(bytes: &[u8], source: &str) -> Result<Catalog> {
    let catalog: Catalog = serde_json::from_slice(bytes)
        .map_err(|error| Error::config(format!("invalid {source}: {error}")))?;
    if catalog.schema != 1 {
        return Err(Error::config(format!(
            "unsupported python catalog schema {}",
            catalog.schema
        )));
    }
    if catalog.entries.is_empty() {
        return Err(Error::config("python catalog contains no entries"));
    }
    for entry in &catalog.entries {
        validate_implementation(&entry.implementation)?;
        validate_variant(&entry.implementation, &entry.variant)?;
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(Error::config(format!(
                "python catalog entry has invalid sha256: {} {}",
                entry.implementation, entry.version
            )));
        }
    }
    Ok(catalog)
}

fn verify_catalog(bytes: &[u8], expected: &str, source: &str) -> Result<Catalog> {
    let actual = crate::pipeline::verify::hash_bytes(bytes, HashAlgo::Sha256);
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        return Err(Error::ChecksumMismatch {
            name: source.into(),
            expected: expected.into(),
            actual,
        });
    }
    parse_catalog(bytes, source)
}

async fn read_source(ctx: &Ctx, source: &str) -> Result<Vec<u8>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = ctx.client.get(source).send().await?.error_for_status()?;
        return Ok(response.bytes().await?.to_vec());
    }
    let path = source.strip_prefix("file://").unwrap_or(source);
    std::fs::read(path).map_err(|error| Error::io(path, error))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(path, error))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, PythonSettings, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::{Libc, Platform};
    use crate::store::Cas;

    #[test]
    fn parses_short_and_full_python_requests() {
        let short = PythonRequest::parse(&ToolRequest::parse("python@3.14").unwrap()).unwrap();
        assert_eq!(short.implementation, "cpython");
        assert_eq!(short.variant, "default");

        let full =
            PythonRequest::parse(&ToolRequest::parse("python@cpython-3.14+freethreaded").unwrap())
                .unwrap();
        assert_eq!(full.implementation, "cpython");
        assert_eq!(full.variant, "freethreaded");
        assert_eq!(full.identity("3.14.7"), "cpython-3.14.7+freethreaded");

        let pypy = PythonRequest::parse(&ToolRequest::parse("python@pypy-3.11").unwrap()).unwrap();
        assert_eq!(pypy.implementation, "pypy");
        assert_eq!(pypy.identity("3.11.15"), "pypy-3.11.15");
    }

    #[test]
    fn builtin_catalog_covers_implementations_variants_and_prerelease() {
        let catalog = parse_catalog(BUILTIN, "test").unwrap();
        for implementation in ["cpython", "pypy", "graalpy", "pyodide"] {
            assert!(catalog
                .entries
                .iter()
                .any(|entry| entry.implementation == implementation));
        }
        for variant in ["freethreaded", "debug", "freethreaded+debug"] {
            assert!(catalog.entries.iter().any(|entry| entry.variant == variant));
        }
        assert!(catalog
            .entries
            .iter()
            .any(|entry| is_prerelease(&entry.version)));
    }

    fn test_ctx(root: &Path, catalog_path: &Path, digest: &str) -> Ctx {
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
        let settings = Settings {
            offline: true,
            python: PythonSettings {
                catalog_url: Some(catalog_path.display().to_string()),
                catalog_sha256: Some(digest.into()),
            },
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
                sources: SourcesConfig::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[tokio::test]
    async fn verified_local_catalog_works_offline_and_bad_digest_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let bytes = BUILTIN.to_vec();
        std::fs::write(&catalog_path, &bytes).unwrap();
        let digest = crate::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256);
        let ctx = test_ctx(temp.path(), &catalog_path, &digest);
        let mut online = test_ctx(temp.path(), &catalog_path, &digest);
        online.config.settings.offline = false;
        let refreshed = load(&online).await.unwrap();
        assert_eq!(refreshed.schema, 1);
        let cache = online
            .dirs
            .remote_cache()
            .join(format!("{CACHE_PREFIX}-{digest}.json"));
        assert!(cache.is_file());

        std::fs::write(&catalog_path, b"not-json").unwrap();
        let last_good = load(&online).await.unwrap();
        assert_eq!(last_good.source, refreshed.source);
        assert_eq!(std::fs::read(&cache).unwrap(), bytes);

        let catalog = load(&ctx).await.unwrap();
        assert_eq!(catalog.schema, 1);

        let bad = test_ctx(temp.path(), &catalog_path, &"0".repeat(64));
        let fallback = load(&bad).await.unwrap();
        assert_eq!(fallback.source, catalog.source);
    }

    #[test]
    fn installed_identities_select_independently() {
        let installed = vec![
            "3.14.7".to_string(),
            "cpython-3.14.7+freethreaded".to_string(),
            "pypy-3.11.15".to_string(),
        ];
        assert_eq!(
            select_installed("3.14", &installed).as_deref(),
            Some("3.14.7")
        );
        assert_eq!(
            select_installed("cpython-3.14+freethreaded", &installed).as_deref(),
            Some("cpython-3.14.7+freethreaded")
        );
        assert_eq!(
            select_installed("pypy-3.11", &installed).as_deref(),
            Some("pypy-3.11.15")
        );
    }

    #[test]
    fn prerelease_policy_is_never_implicit_by_default() {
        let request = PythonRequest::parse(&ToolRequest::parse("python@latest").unwrap()).unwrap();
        assert!(!prerelease_allowed(
            "3.15.0rc1",
            &request,
            PrereleasePolicy::IfExplicit
        ));
        assert!(!prerelease_allowed(
            "3.15.0rc1",
            &request,
            PrereleasePolicy::Never
        ));
        assert!(prerelease_allowed(
            "3.15.0rc1",
            &request,
            PrereleasePolicy::Allow
        ));

        let explicit =
            PythonRequest::parse(&ToolRequest::parse("python@3.15.0-rc.1").unwrap()).unwrap();
        assert!(prerelease_allowed(
            "3.15.0-rc.1",
            &explicit,
            PrereleasePolicy::IfExplicit
        ));
        assert!(!prerelease_allowed(
            "3.15.0-rc.1",
            &explicit,
            PrereleasePolicy::Never
        ));
        let pep_style =
            PythonRequest::parse(&ToolRequest::parse("python@3.15.0rc1").unwrap()).unwrap();
        assert!(pep_style.explicit_prerelease);
    }
}
