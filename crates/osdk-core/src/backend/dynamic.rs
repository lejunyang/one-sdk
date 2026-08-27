//! Factories for backends whose complete id includes a namespace-specific value.

use std::sync::Arc;
use std::{collections::BTreeMap, fmt::Write as _};

use super::Backend;
use crate::error::{Error, Result};

const FINGERPRINT_DOMAIN: &[u8] = b"osdk-dynamic-options-v1";

/// Canonical public options that contribute to a dynamic tool's install
/// identity. Internal lock replay metadata is deliberately kept separate.
pub fn identity_options(
    id: &str,
    options: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let id = crate::inventory::canonical_dynamic_id(id)?;
    let (prefix, _) = id
        .split_once(':')
        .ok_or_else(|| Error::config(format!("dynamic tool id must be namespaced: `{id}`")))?;
    let allowed: &[&str] = match prefix {
        "npm" => &["allow_builds", "installer"],
        "github" => &[
            "arch",
            "asset-regex",
            "asset-template",
            "bin",
            "bins",
            "catalog-sha256",
            "catalog-subdir",
            "catalog-url",
            "libc",
            "os",
            "rename",
            "strip-components",
        ],
        _ => return Err(Error::UnknownBackend(id.to_string())),
    };
    let mut identity = BTreeMap::new();
    if prefix == "github" && options.contains_key("bin") && options.contains_key("bins") {
        return Err(Error::config("bin and bins are mutually exclusive"));
    }
    for (raw_key, value) in options {
        if raw_key.starts_with("__osdk_") {
            continue;
        }
        let key = match (prefix, raw_key.as_str()) {
            ("github", "bin") => "bins",
            _ => raw_key.as_str(),
        };
        if !allowed.contains(&key) {
            return Err(Error::config(format!(
                "unsupported option `{raw_key}` for dynamic backend `{id}`"
            )));
        }
        if let Some((key, value)) = normalize_option(prefix, key, value)? {
            identity.insert(key, value);
        }
    }
    if prefix == "github"
        && options.keys().any(|key| key == "catalog-url")
        && !identity.contains_key("catalog-sha256")
    {
        return Err(Error::config("catalog-sha256 is required with catalog-url"));
    }
    Ok(identity)
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
    let id = crate::inventory::canonical_dynamic_id(id)?;
    if identity_options(&id, options)? != *options {
        return Err(Error::config(
            "dynamic tool inventory contains non-canonical identity options",
        ));
    }
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

fn normalize_option(prefix: &str, key: &str, value: &str) -> Result<Option<(String, String)>> {
    if prefix == "npm" && key == "allow_builds" {
        let value = value.trim();
        if value.is_empty()
            || matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            )
        {
            // Missing and an explicit false/empty value both select the
            // backend's deny-by-default behavior.
            return Ok(None);
        }
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ) {
            return Ok(Some((key.into(), "true".into())));
        }
        let mut packages = value
            .split(',')
            .map(str::trim)
            .filter(|package| !package.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        packages.sort();
        packages.dedup();
        if packages.is_empty() {
            return Err(Error::config("allow_builds must not be empty"));
        }
        return Ok(Some((key.into(), packages.join(","))));
    }
    if prefix == "npm" && key == "installer" {
        let installer = crate::npm_tools::installer_from_request_options(&BTreeMap::from([(
            key.to_string(),
            value.to_string(),
        )]))?;
        return Ok((installer != crate::npm_tools::NpmInstaller::Auto)
            .then(|| (key.into(), installer.as_str().to_string())));
    }
    if prefix == "github" {
        let normalized = match key {
            // The required catalog digest defines its content identity. The
            // location is acquisition metadata and must never be copied into
            // an install inventory. Userinfo credentials are rejected here;
            // callers that persist request options separately must apply their
            // own URL-redaction policy.
            "catalog-url" => {
                let lower = value.to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    let parsed = reqwest::Url::parse(value).map_err(|error| {
                        Error::config(format!("invalid GitHub catalog URL: {error}"))
                    })?;
                    if !parsed.username().is_empty() || parsed.password().is_some() {
                        return Err(Error::config(
                            "GitHub catalog URL must not contain credentials",
                        ));
                    }
                    if parsed.query().is_some() || parsed.fragment().is_some() {
                        return Err(Error::config(
                            "GitHub catalog URL must not contain a query or fragment",
                        ));
                    }
                }
                return Ok(None);
            }
            "catalog-sha256" => {
                let value = value.trim().to_ascii_lowercase();
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(Error::config(
                        "catalog-sha256 must be a 64-character hexadecimal SHA-256 digest",
                    ));
                }
                value
            }
            "os" if value == "darwin" => "macos".into(),
            "arch" if matches!(value, "x86_64" | "amd64") => "x64".into(),
            "arch" if value == "aarch64" => "arm64".into(),
            "arch" if value == "i686" => "x86".into(),
            "arch" if value == "armv7" => "arm".into(),
            "bins" => {
                let bins = value
                    .split(',')
                    .map(str::trim)
                    .filter(|bin| !bin.is_empty())
                    .collect::<Vec<_>>();
                bins.join(",")
            }
            "strip-components" => value
                .parse::<usize>()
                .map_err(|error| {
                    Error::config(format!("invalid strip-components `{value}`: {error}"))
                })?
                .to_string(),
            // Whitespace can be significant in regexes, templates, paths, and
            // executable names. Keep it exactly as the backend consumes it.
            _ => value.to_string(),
        };
        return Ok(Some((key.into(), normalized)));
    }
    Ok(Some((key.into(), value.to_string())))
}

fn update_length_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Constructs a backend for a registered dynamic namespace.
pub(super) trait DynamicBackendFactory: Send + Sync {
    /// Namespace before the `:` in a dynamic backend id.
    fn prefix(&self) -> &'static str;

    /// Parse and construct a backend from its complete namespaced id.
    fn create(&self, id: &str) -> Option<Arc<dyn Backend>>;
}

pub(super) fn builtin_factories() -> Vec<Arc<dyn DynamicBackendFactory>> {
    vec![Arc::new(GithubBackendFactory), Arc::new(NpmBackendFactory)]
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
