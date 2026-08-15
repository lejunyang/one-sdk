//! GitHub artifact-attestation acquisition, caching, and Sigstore verification.
//!
//! The upstream `sigstore` 0.14 bundle verifier validates the Fulcio chain and
//! SCT, certificate policy, artifact signature, DSSE digest, Rekor body
//! consistency, and signing time. Its bundle verifier does not yet validate
//! Rekor Merkle inclusion proofs or Signed Entry Timestamps.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sigstore::bundle::verify::{
    policy::{
        GitHubWorkflowRepository, OIDCIssuer, PolicyError, SingleX509ExtPolicy, VerificationPolicy,
    },
    Verifier,
};
use sigstore::bundle::Bundle;
use sigstore::trust::sigstore::SigstoreTrustRoot;

use crate::config::AttestationPolicy;
use crate::dirs::Dirs;
use crate::error::{Error, Result};

const GITHUB_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPRESSED_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;
const TRUSTED_ROOT: &[u8] = include_bytes!("data/sigstore-public-good-trusted-root.json");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationEvidence {
    pub kind: String,
    pub repository: String,
    pub issuer: String,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct GithubAttestation {
    pub owner: String,
    pub repo: String,
    pub policy: AttestationPolicy,
}

struct GithubRepositoryPolicy {
    issuer: OIDCIssuer,
    repository: GitHubWorkflowRepository,
}

impl VerificationPolicy for GithubRepositoryPolicy {
    fn verify(&self, certificate: &x509_cert::Certificate) -> std::result::Result<(), PolicyError> {
        let mut errors = Vec::new();
        if let Err(error) = self.issuer.verify(certificate) {
            errors.push(error);
        }
        if let Err(error) = self.repository.verify(certificate) {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(PolicyError::AllOf { total: 2, errors })
        }
    }
}

#[derive(Debug, Deserialize)]
struct AttestationResponse {
    #[serde(default)]
    attestations: Vec<AttestationEntry>,
}

#[derive(Debug, Deserialize)]
struct AttestationEntry {
    #[serde(default)]
    bundle: Option<serde_json::Value>,
    #[serde(default)]
    bundle_url: Option<String>,
}

pub async fn verify_github_attestation(
    client: &reqwest::Client,
    dirs: &Dirs,
    offline: bool,
    artifact: &Path,
    request: &GithubAttestation,
) -> Result<Option<VerificationEvidence>> {
    if request.policy == AttestationPolicy::Off {
        return Ok(None);
    }
    if !artifact.is_file() {
        if request.policy == AttestationPolicy::Required {
            return Err(Error::other(format!(
                "required GitHub artifact attestation cannot be verified: cached artifact missing at {}",
                artifact.display()
            )));
        }
        return Ok(None);
    }
    let digest = crate::pipeline::verify::hash_file(artifact, crate::pipeline::HashAlgo::Sha256)?;
    let cache_path = bundle_cache_path(dirs, &request.owner, &request.repo, &digest);
    let bundles = if cache_path.is_file() {
        read_cached_bundles(&cache_path)?
    } else {
        if offline {
            return missing_attestation(request, &digest, true);
        }
        let response = fetch_attestations(client, &request.owner, &request.repo, &digest).await?;
        let bundles = materialize_bundles(client, response.attestations).await?;
        if !bundles.is_empty() {
            write_cached_bundles(&cache_path, &bundles)?;
        }
        bundles
    };

    if bundles.is_empty() {
        return missing_attestation(request, &digest, false);
    }
    verify_bundles_for_repository(artifact, &request.owner, &request.repo, &digest, bundles).await
}

fn missing_attestation(
    request: &GithubAttestation,
    digest: &str,
    offline: bool,
) -> Result<Option<VerificationEvidence>> {
    if request.policy == AttestationPolicy::Required {
        let qualifier = if offline { "cached " } else { "" };
        return Err(Error::other(format!(
            "required GitHub artifact attestation unavailable: no {qualifier}bundle for {}/{} sha256:{digest}",
            request.owner, request.repo
        )));
    }
    Ok(None)
}

async fn verify_bundles_for_repository(
    artifact: &Path,
    owner: &str,
    repo: &str,
    digest: &str,
    bundles: Vec<Vec<u8>>,
) -> Result<Option<VerificationEvidence>> {
    let trust_root = SigstoreTrustRoot::from_trusted_root_json_unchecked(TRUSTED_ROOT)
        .map_err(|error| Error::other(format!("invalid embedded Sigstore trust root: {error}")))?;
    let verifier = Verifier::new(Default::default(), trust_root)
        .map_err(|error| Error::other(format!("building Sigstore verifier: {error}")))?;
    let repository = format!("{owner}/{repo}");
    let policy = GithubRepositoryPolicy {
        issuer: OIDCIssuer::new(GITHUB_OIDC_ISSUER),
        repository: GitHubWorkflowRepository::new(&repository),
    };

    let mut errors = Vec::new();
    for bundle_json in bundles {
        let bundle: Bundle = match serde_json::from_slice(&bundle_json) {
            Ok(bundle) => bundle,
            Err(error) => {
                errors.push(format!("invalid Sigstore bundle: {error}"));
                continue;
            }
        };
        let file = tokio::fs::File::open(artifact)
            .await
            .map_err(|error| Error::io(artifact, error))?;
        match verifier.verify(file, bundle, &policy, true).await {
            Ok(()) => {
                return Ok(Some(VerificationEvidence {
                    kind: "sigstore-bundle".into(),
                    repository,
                    issuer: GITHUB_OIDC_ISSUER.into(),
                    digest: format!("sha256:{digest}"),
                }));
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    Err(Error::other(format!(
        "GitHub artifact attestation verification failed: {}",
        errors.join("; ")
    )))
}

async fn fetch_attestations(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    digest: &str,
) -> Result<AttestationResponse> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/attestations/sha256:{digest}?per_page=30"
    );
    crate::http::get_github_json(client, &url).await
}

async fn materialize_bundles(
    client: &reqwest::Client,
    entries: Vec<AttestationEntry>,
) -> Result<Vec<Vec<u8>>> {
    let mut bundles = Vec::new();
    for entry in entries {
        if let Some(url) = entry.bundle_url {
            bundles.push(fetch_bundle_url(client, &url).await?);
        } else if let Some(bundle) = entry.bundle {
            bundles.push(serde_json::to_vec(&bundle)?);
        } else {
            return Err(Error::other(
                "GitHub attestation entry has neither bundle nor bundle_url",
            ));
        }
    }
    Ok(bundles)
}

async fn fetch_bundle_url(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| Error::other(format!("invalid attestation bundle URL: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(Error::other("attestation bundle URL must use HTTPS"));
    }
    let response = client.get(parsed).send().await?.error_for_status()?;
    if response.url().scheme() != "https" {
        return Err(Error::other(
            "attestation bundle redirect must remain on HTTPS",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_COMPRESSED_BUNDLE_BYTES)
    {
        return Err(Error::other(format!(
            "compressed attestation bundle exceeds {MAX_COMPRESSED_BUNDLE_BYTES} bytes"
        )));
    }
    let mut compressed = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if compressed.len() + chunk.len() > MAX_COMPRESSED_BUNDLE_BYTES as usize {
            return Err(Error::other(format!(
                "compressed attestation bundle exceeds {MAX_COMPRESSED_BUNDLE_BYTES} bytes"
            )));
        }
        compressed.extend_from_slice(&chunk);
    }
    decode_snappy_bundle(&compressed)
}

fn decode_snappy_bundle(compressed: &[u8]) -> Result<Vec<u8>> {
    let decoded_len = snap::raw::decompress_len(compressed)
        .map_err(|error| Error::other(format!("invalid Snappy attestation bundle: {error}")))?;
    if decoded_len > MAX_BUNDLE_BYTES {
        return Err(Error::other(format!(
            "attestation bundle exceeds {MAX_BUNDLE_BYTES} bytes"
        )));
    }
    snap::raw::Decoder::new()
        .decompress_vec(compressed)
        .map_err(|error| Error::other(format!("invalid Snappy attestation bundle: {error}")))
}

fn bundle_cache_path(dirs: &Dirs, owner: &str, repo: &str, digest: &str) -> PathBuf {
    dirs.remote_cache()
        .join("attestations")
        .join(crate::dirs::sanitize_tool_id(&format!("{owner}/{repo}")))
        .join(format!("{digest}.json"))
}

fn write_cached_bundles(path: &Path, bundles: &[Vec<u8>]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let values: Vec<serde_json::Value> = bundles
        .iter()
        .map(|bundle| serde_json::from_slice(bundle))
        .collect::<std::result::Result<_, _>>()?;
    let bytes = serde_json::to_vec_pretty(&values)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) if path.is_file() => {
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => Err(Error::io(path, error)),
    }
}

fn read_cached_bundles(path: &Path) -> Result<Vec<Vec<u8>>> {
    let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
    let values: Vec<serde_json::Value> = serde_json::from_slice(&bytes)?;
    values
        .iter()
        .map(|value| serde_json::to_vec(value).map_err(Error::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn fixture_artifact() -> Vec<u8> {
        use base64::Engine as _;

        let encoded: String =
            include_str!("../../tests/fixtures/attestation/kubewarden-manifest.json.b64")
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
    }

    fn fixture_bundle() -> Vec<u8> {
        use base64::Engine as _;

        let encoded: String =
            include_str!("../../tests/fixtures/attestation/kubewarden.sigstore.json.gz.b64")
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
        let mut bundle = Vec::new();
        decoder.read_to_end(&mut bundle).unwrap();
        bundle
    }

    fn test_dirs(root: &Path) -> Dirs {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        dirs
    }

    async fn verify_fixture(repository: &str, artifact: &[u8]) -> Result<VerificationEvidence> {
        let temp = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temp.path());
        let path = temp.path().join("manifest.json");
        std::fs::write(&path, artifact).unwrap();
        let (owner, repo) = repository.split_once('/').unwrap();
        let digest =
            crate::pipeline::verify::hash_file(&path, crate::pipeline::HashAlgo::Sha256).unwrap();
        let bundle_path = bundle_cache_path(&dirs, owner, repo, &digest);
        write_cached_bundles(&bundle_path, &[fixture_bundle()]).unwrap();
        let request = GithubAttestation {
            owner: owner.into(),
            repo: repo.into(),
            policy: AttestationPolicy::Required,
        };
        verify_github_attestation(&reqwest::Client::new(), &dirs, true, &path, &request)
            .await?
            .ok_or_else(|| Error::other("fixture produced no evidence"))
    }

    #[test]
    fn identifies_non_https_bundle_urls() {
        let parsed = reqwest::Url::parse("http://example.test/bundle").unwrap();
        assert_ne!(parsed.scheme(), "https");
    }

    #[test]
    fn decodes_github_snappy_bundle_payload() {
        let bundle = fixture_bundle();
        let compressed = snap::raw::Encoder::new().compress_vec(&bundle).unwrap();
        assert_eq!(decode_snappy_bundle(&compressed).unwrap(), bundle);
        assert!(decode_snappy_bundle(b"not-snappy").is_err());
    }

    #[tokio::test]
    async fn malformed_api_entry_is_not_treated_as_unavailable() {
        let error = materialize_bundles(
            &reqwest::Client::new(),
            vec![AttestationEntry {
                bundle: None,
                bundle_url: None,
            }],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("neither bundle nor bundle_url"));
    }

    #[test]
    fn embedded_trust_root_parses() {
        SigstoreTrustRoot::from_trusted_root_json_unchecked(TRUSTED_ROOT).unwrap();
    }

    #[tokio::test]
    async fn verifies_offline_github_actions_bundle() {
        let evidence = verify_fixture("kubewarden/kubewarden-controller", &fixture_artifact())
            .await
            .unwrap();
        assert_eq!(
            evidence,
            VerificationEvidence {
                kind: "sigstore-bundle".into(),
                repository: "kubewarden/kubewarden-controller".into(),
                issuer: GITHUB_OIDC_ISSUER.into(),
                digest: "sha256:c811d58de79c92f03214e63aa339484e488d694ae8a6283b5f3f17a9faf50172"
                    .into(),
            }
        );
    }

    #[tokio::test]
    async fn attestation_supplies_required_checksum_and_receipt_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temp.path());
        let tool = "github:kubewarden/kubewarden-controller";
        let version = "1.34.0";
        let file_name = "manifest.json";
        let artifact = crate::pipeline::artifact_cache_path(&dirs, tool, version, file_name);
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, fixture_artifact()).unwrap();
        let digest =
            crate::pipeline::verify::hash_file(&artifact, crate::pipeline::HashAlgo::Sha256)
                .unwrap();
        let bundle_path = bundle_cache_path(&dirs, "kubewarden", "kubewarden-controller", &digest);
        write_cached_bundles(&bundle_path, &[fixture_bundle()]).unwrap();
        let request = GithubAttestation {
            owner: "kubewarden".into(),
            repo: "kubewarden-controller".into(),
            policy: AttestationPolicy::Required,
        };

        crate::pipeline::install_single_binary(
            &reqwest::Client::new(),
            &dirs,
            tool,
            version,
            &["https://invalid.example/manifest.json".into()],
            "kubewarden-controller",
            file_name,
            crate::platform::Os::Linux,
            None,
            false,
            true,
            true,
            Some(&request),
        )
        .await
        .unwrap();

        let receipt = crate::pipeline::artifact_receipt(&dirs, tool, version).unwrap();
        assert_eq!(receipt.checksum, Some(format!("sha256:{digest}")));
        assert_eq!(receipt.evidence.len(), 1);
        assert_eq!(receipt.evidence[0].digest, format!("sha256:{digest}"));
        assert!(crate::pipeline::is_installed(&dirs, tool, version));
    }

    #[tokio::test]
    async fn rejects_wrong_repository_identity() {
        let error = verify_fixture("kubewarden/other", &fixture_artifact())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("verification failed"));
    }

    #[tokio::test]
    async fn rejects_tampered_artifact() {
        let mut artifact = fixture_artifact();
        artifact[0] ^= 0xff;
        let error = verify_fixture("kubewarden/kubewarden-controller", &artifact)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("verification failed"));
    }

    #[test]
    fn required_policy_rejects_missing_cached_bundle() {
        let request = GithubAttestation {
            owner: "prefix-dev".into(),
            repo: "sigstore-example".into(),
            policy: AttestationPolicy::Required,
        };
        let error = missing_attestation(&request, "00", true).unwrap_err();
        assert!(error.to_string().contains("no cached bundle"));

        let permissive = GithubAttestation {
            policy: AttestationPolicy::IfAvailable,
            ..request
        };
        assert_eq!(missing_attestation(&permissive, "00", true).unwrap(), None);
    }

    #[test]
    fn receipt_evidence_round_trips() {
        let evidence = VerificationEvidence {
            kind: "sigstore-bundle".into(),
            repository: "cli/cli".into(),
            issuer: GITHUB_OIDC_ISSUER.into(),
            digest: "sha256:00".into(),
        };
        let encoded = serde_json::to_vec(&evidence).unwrap();
        assert_eq!(
            serde_json::from_slice::<VerificationEvidence>(&encoded).unwrap(),
            evidence
        );
    }
}
