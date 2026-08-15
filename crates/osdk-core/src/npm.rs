//! Minimal npm registry helpers: resolve a package version's tarball URL +
//! Subresource Integrity (SRI), used by the pnpm/yarn backends to install
//! verified artifacts from the registry (mirror-friendly, first-party checksum).

use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::Checksum;

/// The npm registry mirrors to try (best-first). npmmirror first for CN.
pub const REGISTRIES: &[&str] = &[
    "https://registry.npmmirror.com",
    "https://registry.npmjs.org",
];

#[derive(Debug, Deserialize)]
struct VersionDoc {
    #[serde(default)]
    dist: Dist,
}

#[derive(Debug, Deserialize, Default)]
struct Dist {
    #[serde(default)]
    tarball: String,
    #[serde(default)]
    integrity: String,
    #[serde(default)]
    shasum: String,
}

/// Resolved distribution for one package version.
pub struct NpmDist {
    pub tarball: String,
    pub checksum: Option<Checksum>,
}

/// Fetch the tarball URL + checksum for `package@version` (e.g. `yarn`,
/// `@pnpm/linux-x64`). Tries each registry mirror. The checksum comes from the
/// SRI `integrity` (sha512/sha256), falling back to the legacy `shasum` (sha1,
/// unsupported by our verifier -> None).
pub async fn resolve_dist(ctx: &Ctx, package: &str, version: &str) -> Result<NpmDist> {
    let mut last_err: Option<Error> = None;
    for reg in REGISTRIES {
        // package name may contain a scope (@pnpm/linux-x64); the registry URL
        // encodes `/` in scopes as-is: <reg>/<pkg>/<version>
        let url = format!("{reg}/{package}/{version}");
        match http::get_json::<VersionDoc>(&ctx.client, &url).await {
            Ok(doc) => {
                if doc.dist.tarball.is_empty() {
                    last_err = Some(Error::other(format!("no tarball for {package}@{version}")));
                    continue;
                }
                let checksum = crate::pipeline::verify::parse_sri(&doc.dist.integrity);
                if checksum.is_none() && !doc.dist.shasum.is_empty() {
                    tracing::debug!(
                        package,
                        "npm dist has only sha1 shasum; skipping verification"
                    );
                }
                return Ok(NpmDist {
                    tarball: doc.dist.tarball,
                    checksum,
                });
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::other(format!("cannot resolve {package}@{version}"))))
}

/// List available versions of an npm package (sorted ascending), trying mirrors.
pub async fn list_versions(ctx: &Ctx, package: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Packument {
        #[serde(default)]
        versions: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let mut last_err: Option<Error> = None;
    for reg in REGISTRIES {
        let url = format!("{reg}/{package}");
        match http::get_json::<Packument>(&ctx.client, &url).await {
            Ok(p) => {
                let mut out: Vec<String> = p.versions.into_keys().collect();
                out.sort_by(|a, b| crate::backend::python::cmp_versions(a, b));
                return Ok(out);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::other(format!("cannot list {package}"))))
}
