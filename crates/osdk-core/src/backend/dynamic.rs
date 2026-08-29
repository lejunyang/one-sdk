//! Factories for backends whose complete id includes a namespace-specific value.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{collections::BTreeMap, fmt::Write as _};

use super::Backend;
use crate::dirs::{Dirs, InstallLocator};
use crate::error::{Error, Result};
use crate::inventory::{DynamicToolBin, DynamicToolManifest};
use crate::tool::{InstallDependency, InstallIdentity, InstallScope};

const FINGERPRINT_DOMAIN: &[u8] = b"osdk-dynamic-options-v1";
const INSTALL_ID_DOMAIN: &[u8] = b"osdk-install-identity-v2";

/// Canonical public options that contribute to a dynamic tool's install
/// identity. Internal lock replay metadata is deliberately kept separate.
pub fn identity_options(
    id: &str,
    options: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let id = crate::tool::ToolId::parse(id)?;
    crate::tool::dynamic_identity_options(&id, options).map(crate::tool::CanonicalOptions::into_map)
}

/// Validate namespace-specific relationships between otherwise valid public
/// options. This is kept separate from the persisted identity projection so a
/// sensitive acquisition location can be omitted while its required content
/// digest still protects reuse.
pub fn validate_options(id: &str, options: &BTreeMap<String, String>) -> Result<()> {
    identity_options(id, options).map(|_| ())
}

/// Stable, order-independent identity for dynamic backend options.
pub fn option_fingerprint(id: &str, options: &BTreeMap<String, String>) -> Result<String> {
    let identity = identity_options(id, options)?;
    fingerprint_canonical_options(id, &identity)
}

/// Fingerprint an already-canonical identity projection. This separate path is
/// important when loading an inventory: normalizing a canonical value twice
/// must never silently change the identity being verified.
pub(crate) fn fingerprint_canonical_options(
    id: &str,
    options: &BTreeMap<String, String>,
) -> Result<String> {
    let id = crate::tool::ToolId::parse(id)?;
    crate::tool::validate_canonical_identity_options(&id, options)?;
    let id = id.to_string();
    let mut hasher = blake3::Hasher::new();
    update_length_prefixed(&mut hasher, FINGERPRINT_DOMAIN);
    update_length_prefixed(&mut hasher, id.as_bytes());
    for (key, value) in options {
        update_length_prefixed(&mut hasher, key.as_bytes());
        update_length_prefixed(&mut hasher, value.as_bytes());
    }
    let mut fingerprint = String::from("b3-v1:");
    write!(&mut fingerprint, "{}", hasher.finalize().to_hex())
        .expect("writing into a String cannot fail");
    Ok(fingerprint)
}

/// Stable identity of the complete materialized install contract.
pub fn install_identity_fingerprint(identity: &InstallIdentity) -> Result<String> {
    let tool = crate::tool::ToolId::parse(&identity.tool)?;
    if !tool.is_dynamic() || tool.to_string() != identity.tool {
        return Err(Error::config(
            "install identity contains a non-canonical dynamic tool id",
        ));
    }
    crate::tool::validate_canonical_identity_options(&tool, &identity.material_options)?;

    let mut hasher = blake3::Hasher::new();
    update_length_prefixed(&mut hasher, INSTALL_ID_DOMAIN);
    update_length_prefixed(&mut hasher, identity.tool.as_bytes());
    update_length_prefixed(&mut hasher, identity.version.as_bytes());
    update_length_prefixed(&mut hasher, identity.platform.as_bytes());
    update_length_prefixed(
        &mut hasher,
        serde_json::to_string(&identity.scope)?.as_bytes(),
    );
    update_map(&mut hasher, &identity.material_options);
    update_length_prefixed(
        &mut hasher,
        &(identity.dependencies.len() as u64).to_le_bytes(),
    );
    for dependency in &identity.dependencies {
        update_dependency(&mut hasher, dependency)?;
    }
    update_map(&mut hasher, &identity.materials);
    let mut fingerprint = String::from("b3-v2:");
    write!(&mut fingerprint, "{}", hasher.finalize().to_hex())
        .expect("writing into a String cannot fail");
    Ok(fingerprint)
}

fn update_map(hasher: &mut blake3::Hasher, values: &BTreeMap<String, String>) {
    update_length_prefixed(hasher, &(values.len() as u64).to_le_bytes());
    for (key, value) in values {
        update_length_prefixed(hasher, key.as_bytes());
        update_length_prefixed(hasher, value.as_bytes());
    }
}

fn update_dependency(hasher: &mut blake3::Hasher, dependency: &InstallDependency) -> Result<()> {
    update_length_prefixed(hasher, serde_json::to_string(&dependency.kind)?.as_bytes());
    update_length_prefixed(hasher, dependency.id.as_bytes());
    update_length_prefixed(hasher, dependency.version.as_bytes());
    match dependency.identity.as_deref() {
        Some(identity) => {
            update_length_prefixed(hasher, b"some");
            update_length_prefixed(hasher, identity.as_bytes());
        }
        None => update_length_prefixed(hasher, b"none"),
    }
    Ok(())
}

fn update_length_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Hold a dynamic install's identity lock without blocking an async executor.
pub(crate) async fn acquire_install_lock(
    locator: &InstallLocator,
    namespace: &str,
) -> Result<crate::lock::FileLock> {
    let path = locator.lock_path().to_path_buf();
    let namespace = namespace.to_string();
    tokio::task::spawn_blocking(move || crate::lock::FileLock::acquire(path))
        .await
        .map_err(|error| Error::other(format!("{namespace} install lock task failed: {error}")))?
}

/// Validate the namespace-neutral durable state of a completed artifact install.
pub(crate) fn artifact_install_candidate_is_valid(
    dirs: &Dirs,
    install_root: &Path,
    identity: &InstallIdentity,
) -> Result<bool> {
    if identity.scope != InstallScope::Isolated
        || !is_regular_file(&install_root.join(".osdk-complete"))
        || !is_regular_file(&DynamicToolManifest::manifest_path(install_root))
        || !is_regular_file(&install_root.join(".osdk-artifact.json"))
    {
        return Ok(false);
    }
    let locator = InstallLocator::new(dirs, identity.clone())?;
    if !locator.validates_existing_install_root(install_root) {
        return Ok(false);
    }
    reject_symlinks(install_root)?;
    let manifest = DynamicToolManifest::load(install_root)?;
    if !manifest.matches_identity(identity) {
        return Err(Error::other(format!(
            "dynamic install identity mismatch at {}",
            DynamicToolManifest::manifest_path(install_root).display()
        )));
    }
    let receipt = crate::pipeline::artifact_receipt_at(install_root)
        .ok_or_else(|| Error::other("dynamic artifact receipt is missing or invalid"))?;
    if receipt.file_name
        != identity
            .materials
            .get("artifact-file")
            .cloned()
            .unwrap_or_default()
        || !checksum_matches(
            receipt.checksum.as_deref(),
            identity
                .materials
                .get("artifact-checksum")
                .map(String::as_str),
        )
    {
        return Err(Error::other(format!(
            "dynamic artifact receipt does not match install identity at {}",
            install_root.display()
        )));
    }
    let canonical_root =
        dunce::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    for bin in &manifest.bins {
        let path = install_root.join(&bin.path);
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| Error::io(&path, error))?;
        let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !canonical.starts_with(&canonical_root)
        {
            return Err(Error::other(format!(
                "dynamic inventory bin `{}` does not resolve inside {}",
                bin.name,
                install_root.display()
            )));
        }
    }
    Ok(true)
}

/// Reject links before publishing an install sourced from an arbitrary URL.
pub(crate) fn reject_symlinks(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| Error::other(format!("walkdir: {error}")))?;
        if entry.file_type().is_symlink() {
            return Err(Error::other(format!(
                "artifact contains a forbidden symlink: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

/// Atomically publish inventory before exposing the completion marker.
pub(crate) fn finalize_artifact_install(locator: &InstallLocator) -> Result<()> {
    let root = locator.install_root();
    let result = (|| {
        reject_symlinks(root)?;
        let mut manifest = DynamicToolManifest::from_identity(locator.identity().clone())?;
        let bin = root.join("bin");
        let directories = if bin.is_dir() {
            vec![bin, root.to_path_buf()]
        } else {
            vec![root.to_path_buf()]
        };
        let canonical_root = dunce::canonicalize(root).map_err(|error| Error::io(root, error))?;
        for directory in directories {
            for name in super::bin_names_in_dirs(std::slice::from_ref(&directory)) {
                let Some(path) = executable_in_dir(&directory, &name) else {
                    continue;
                };
                let canonical =
                    dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
                let relative = canonical.strip_prefix(&canonical_root).map_err(|_| {
                    Error::other(format!(
                        "installed dynamic binary `{name}` resolves outside {}",
                        root.display()
                    ))
                })?;
                manifest.bins.push(DynamicToolBin {
                    name,
                    path: relative.to_string_lossy().replace('\\', "/"),
                });
            }
        }
        manifest
            .bins
            .sort_by(|left, right| left.name.cmp(&right.name));
        manifest
            .bins
            .dedup_by(|left, right| left.name == right.name);
        if manifest.bins.is_empty() {
            return Err(Error::other(
                "artifact installation did not publish any executable",
            ));
        }
        manifest.write_atomic(root)?;
        std::fs::write(root.join(".osdk-complete"), b"")
            .map_err(|error| Error::io(root.join(".osdk-complete"), error))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(root);
    }
    result
}

fn checksum_matches(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            let Ok(actual) = crate::pipeline::parse_checksum(actual) else {
                return false;
            };
            let Ok(expected) = crate::pipeline::parse_checksum(expected) else {
                return false;
            };
            actual.algo == expected.algo && actual.hex.eq_ignore_ascii_case(&expected.hex)
        }
        _ => false,
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn executable_in_dir(directory: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [format!("{name}.exe")];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    candidates
        .into_iter()
        .map(|candidate| directory.join(candidate))
        .find(|candidate| candidate.is_file())
}

/// Constructs a backend for a registered dynamic namespace.
pub(super) trait DynamicBackendFactory: Send + Sync {
    /// Namespace before the `:` in a dynamic backend id.
    fn prefix(&self) -> &'static str;

    /// Parse and construct a backend from its complete namespaced id.
    fn create(&self, id: &str) -> Option<Arc<dyn Backend>>;
}

pub(super) fn builtin_factories() -> Vec<Arc<dyn DynamicBackendFactory>> {
    vec![
        Arc::new(CargoBackendFactory),
        Arc::new(GithubBackendFactory),
        Arc::new(NpmBackendFactory),
        Arc::new(HttpBackendFactory),
    ]
}

struct CargoBackendFactory;

impl DynamicBackendFactory for CargoBackendFactory {
    fn prefix(&self) -> &'static str {
        "cargo"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::cargo_package::CargoPackageBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}

struct GithubBackendFactory;

impl DynamicBackendFactory for GithubBackendFactory {
    fn prefix(&self) -> &'static str {
        "github"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::github::GithubBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}

struct NpmBackendFactory;

impl DynamicBackendFactory for NpmBackendFactory {
    fn prefix(&self) -> &'static str {
        "npm"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::npm_package::NpmPackageBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}

struct HttpBackendFactory;

impl DynamicBackendFactory for HttpBackendFactory {
    fn prefix(&self) -> &'static str {
        "http"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::http::HttpBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_order_independent_and_ignore_locked_metadata() {
        let first = BTreeMap::from([
            ("installer".into(), "aube".into()),
            ("allow_builds".into(), "Sharp, esbuild, sharp".into()),
        ]);
        let mut second = BTreeMap::new();
        second.insert("allow_builds".into(), "esbuild,sharp".into());
        second.insert("installer".into(), "aube".into());
        second.insert("__osdk_node_version".into(), "24.1.0".into());

        assert_eq!(
            option_fingerprint("npm:prettier", &first).unwrap(),
            option_fingerprint("npm:prettier", &second).unwrap()
        );
        assert_eq!(
            identity_options("npm:prettier", &first).unwrap()["allow_builds"],
            "esbuild,sharp"
        );
    }

    #[test]
    fn npm_default_spellings_share_an_identity() {
        let defaults = BTreeMap::new();
        for options in [
            BTreeMap::from([("allow_builds".into(), "false".into())]),
            BTreeMap::from([("allow_builds".into(), "  ".into())]),
            BTreeMap::from([("installer".into(), "AUTO".into())]),
        ] {
            assert_eq!(
                option_fingerprint("npm:prettier", &defaults).unwrap(),
                option_fingerprint("npm:prettier", &options).unwrap()
            );
        }
    }

    #[test]
    fn github_identity_canonicalizes_consumed_aliases_and_omits_catalog_location() {
        let first = BTreeMap::from([
            ("os".into(), "darwin".into()),
            ("arch".into(), "amd64".into()),
            ("bins".into(), "bin/a, bin/b".into()),
            (
                "catalog-url".into(),
                "https://example.test/catalog.json".into(),
            ),
            ("catalog-sha256".into(), "A".repeat(64)),
        ]);
        let second = BTreeMap::from([
            ("os".into(), "macos".into()),
            ("arch".into(), "x64".into()),
            ("bins".into(), "bin/a,bin/b".into()),
            (
                "catalog-url".into(),
                "https://mirror.example.test/catalog.json".into(),
            ),
            ("catalog-sha256".into(), "a".repeat(64)),
        ]);
        let identity = identity_options("github:owner/repo", &first).unwrap();
        assert!(!identity.contains_key("catalog-url"));
        assert_eq!(
            option_fingerprint("github:owner/repo", &first).unwrap(),
            option_fingerprint("github:owner/repo", &second).unwrap()
        );
    }

    #[test]
    fn github_bin_and_bins_spellings_share_an_identity() {
        let singular = BTreeMap::from([("bin".into(), "bin/tool".into())]);
        let plural = BTreeMap::from([("bins".into(), " bin/tool ".into())]);
        assert_eq!(
            option_fingerprint("github:owner/repo", &singular).unwrap(),
            option_fingerprint("github:owner/repo", &plural).unwrap()
        );
    }

    #[test]
    fn github_bin_and_bins_remain_mutually_exclusive() {
        let options = BTreeMap::from([
            ("bin".into(), "bin/tool".into()),
            ("bins".into(), "bin/tool".into()),
        ]);
        assert!(identity_options("github:owner/repo", &options)
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));
    }

    #[test]
    fn github_catalog_url_rejects_userinfo_credentials() {
        let options = BTreeMap::from([(
            "catalog-url".into(),
            "https://user:secret@example.test/catalog.json".into(),
        )]);
        assert!(identity_options("github:owner/repo", &options)
            .unwrap_err()
            .to_string()
            .contains("must not contain credentials"));
    }

    #[test]
    fn github_catalog_url_rejects_signed_queries() {
        let options = BTreeMap::from([
            (
                "catalog-url".into(),
                "https://example.test/catalog.json?token=secret".into(),
            ),
            ("catalog-sha256".into(), "a".repeat(64)),
        ]);
        assert!(identity_options("github:owner/repo", &options)
            .unwrap_err()
            .to_string()
            .contains("query or fragment"));
    }

    #[test]
    fn github_catalog_location_requires_a_content_digest() {
        let options = BTreeMap::from([(
            "catalog-url".into(),
            "https://example.test/catalog.json".into(),
        )]);
        assert!(validate_options("github:owner/repo", &options)
            .unwrap_err()
            .to_string()
            .contains("catalog-sha256 is required"));
    }

    #[test]
    fn canonical_identity_fingerprint_rejects_second_normalization() {
        let non_canonical = BTreeMap::from([("allow_builds".into(), "sharp,esbuild".into())]);
        assert!(fingerprint_canonical_options("npm:prettier", &non_canonical).is_err());
        let identity = identity_options("npm:prettier", &non_canonical).unwrap();
        assert!(fingerprint_canonical_options("npm:prettier", &identity).is_ok());
    }

    #[test]
    fn fingerprints_change_with_identity_and_options() {
        let options = BTreeMap::from([("rename".into(), "rg".into())]);
        assert_ne!(
            option_fingerprint("github:owner/one", &options).unwrap(),
            option_fingerprint("github:owner/two", &options).unwrap()
        );
        let changed = BTreeMap::from([("rename".into(), "ripgrep".into())]);
        assert_ne!(
            option_fingerprint("github:owner/one", &options).unwrap(),
            option_fingerprint("github:owner/one", &changed).unwrap()
        );
    }

    #[test]
    fn unknown_public_options_fail_before_installation() {
        let options = BTreeMap::from([("token".into(), "secret".into())]);
        assert!(option_fingerprint("github:owner/repo", &options).is_err());
        assert!(option_fingerprint("npm:prettier", &options).is_err());
    }
}
