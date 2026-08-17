//! GitHub artifact-attestation acquisition, caching, and Sigstore verification.
//!
//! Verification includes the Fulcio chain and SCT, certificate policy,
//! artifact signature, DSSE digest, Rekor body consistency, Signed Entry
//! Timestamp, checkpoint signature, Merkle inclusion proof, and signing time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sigstore::bundle::verify::{
    policy::{
        GitHubWorkflowRepository, OIDCIssuer, PolicyError, SingleX509ExtPolicy, VerificationPolicy,
    },
    Verifier,
};
use sigstore::bundle::Bundle;
use sigstore::crypto::{CosignVerificationKey, Signature};
use sigstore::rekor::models::{
    checkpoint::SignedCheckpoint, inclusion_proof::InclusionProof as RekorInclusionProof,
};
use sigstore::trust::sigstore::SigstoreTrustRoot;
use sigstore::trust::TrustRoot;
use sigstore_verify::trust_root::{SigstoreInstance, TrustedRoot};
use sigstore_verify::types::{Bundle as VerifiedBundle, Sha256Hash, SignatureContent};
use sigstore_verify::VerificationPolicy as SigstoreVerificationPolicy;

use crate::config::AttestationPolicy;
use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::source::Source;

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
    pub sources: Vec<Source>,
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
        let response = fetch_attestations(
            client,
            &request.owner,
            &request.repo,
            &digest,
            &request.sources,
        )
        .await?;
        let bundles = materialize_bundles(client, response.attestations, &request.sources).await?;
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
    let rekor_keys = load_rekor_keys(&trust_root)?;
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
        if bundle
            .verification_material
            .as_ref()
            .is_some_and(|material| material.tlog_entries.is_empty())
        {
            match verify_github_timestamp_bundle(&bundle_json, &repository, digest) {
                Ok(()) => {
                    return Ok(Some(VerificationEvidence {
                        kind: "sigstore-bundle+github-tsa".into(),
                        repository,
                        issuer: "https://github.com".into(),
                        digest: format!("sha256:{digest}"),
                    }));
                }
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            }
        }
        if let Err(error) = verify_rekor_transparency(&bundle, &rekor_keys) {
            errors.push(error.to_string());
            continue;
        }
        let file = tokio::fs::File::open(artifact)
            .await
            .map_err(|error| Error::io(artifact, error))?;
        match verifier.verify(file, bundle, &policy, true).await {
            Ok(()) => {
                return Ok(Some(VerificationEvidence {
                    kind: "sigstore-bundle+rekor".into(),
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

fn verify_github_timestamp_bundle(
    bundle_json: &[u8],
    repository: &str,
    digest: &str,
) -> Result<()> {
    let bundle_json = std::str::from_utf8(bundle_json)
        .map_err(|error| Error::other(format!("invalid UTF-8 Sigstore bundle: {error}")))?;
    let bundle = VerifiedBundle::from_json(bundle_json)
        .map_err(|error| Error::other(format!("invalid GitHub Sigstore bundle: {error}")))?;
    verify_signed_repository_claim(&bundle, repository)?;
    let digest = Sha256Hash::from_hex(digest)
        .map_err(|error| Error::other(format!("invalid artifact SHA-256 digest: {error}")))?;
    let trust_root = TrustedRoot::from_embedded(SigstoreInstance::GitHub)
        .map_err(|error| Error::other(format!("invalid embedded GitHub trust root: {error}")))?;
    let policy = SigstoreVerificationPolicy::default().skip_tlog().skip_sct();
    sigstore_verify::verify(digest, &bundle, &policy, &trust_root).map_err(|error| {
        Error::other(format!(
            "GitHub timestamp bundle verification failed: {error}"
        ))
    })?;
    Ok(())
}

fn verify_signed_repository_claim(bundle: &VerifiedBundle, repository: &str) -> Result<()> {
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Err(Error::other(
            "GitHub timestamp bundle must contain a DSSE statement",
        ));
    };
    let payload = envelope.decode_payload();
    let statement: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|error| Error::other(format!("invalid GitHub attestation statement: {error}")))?;
    let claimed = statement
        .pointer("/predicate/repository")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            statement
                .pointer("/predicate/buildDefinition/externalParameters/workflow/repository")
                .and_then(serde_json::Value::as_str)
        });
    match claimed {
        Some(claimed) if claimed.eq_ignore_ascii_case(repository) => Ok(()),
        Some(claimed) => Err(Error::other(format!(
            "GitHub attestation repository mismatch: expected `{repository}`, got `{claimed}`"
        ))),
        None => Err(Error::other(
            "GitHub attestation statement is missing a supported repository claim",
        )),
    }
}

fn load_rekor_keys(trust_root: &impl TrustRoot) -> Result<BTreeMap<String, CosignVerificationKey>> {
    trust_root
        .rekor_keys()
        .map_err(|error| Error::other(format!("loading Rekor trust root keys: {error}")))?
        .into_iter()
        .map(|(id, bytes)| {
            CosignVerificationKey::try_from_der(bytes)
                .map_err(|error| Error::other(format!("invalid Rekor public key `{id}`: {error}")))
                .map(|key| (id, key))
        })
        .collect()
}

fn verify_rekor_transparency(
    bundle: &Bundle,
    rekor_keys: &BTreeMap<String, CosignVerificationKey>,
) -> Result<()> {
    let material = bundle
        .verification_material
        .as_ref()
        .ok_or_else(|| Error::other("Sigstore bundle missing verification material"))?;
    let [entry] = material.tlog_entries.as_slice() else {
        return Err(Error::other(format!(
            "Sigstore bundle requires exactly one transparency log entry, got {}",
            material.tlog_entries.len()
        )));
    };
    let log_id = entry
        .log_id
        .as_ref()
        .ok_or_else(|| Error::other("Sigstore bundle transparency entry missing log ID"))?;
    let key_id = hex::encode(&log_id.key_id);
    if key_id.is_empty() {
        return Err(Error::other(
            "Sigstore bundle transparency entry has empty log ID",
        ));
    }
    let rekor_key = rekor_keys
        .get(&key_id)
        .ok_or_else(|| Error::other(format!("untrusted Rekor log ID `{key_id}`")))?;
    if entry.log_index < 0 || entry.integrated_time < 0 {
        return Err(Error::other(
            "Sigstore bundle transparency entry has negative index or time",
        ));
    }
    if entry.canonicalized_body.is_empty() {
        return Err(Error::other(
            "Sigstore bundle transparency entry has empty canonical body",
        ));
    }

    let promise = entry
        .inclusion_promise
        .as_ref()
        .ok_or_else(|| Error::other("Sigstore bundle missing Rekor Signed Entry Timestamp"))?;
    if promise.signed_entry_timestamp.is_empty() {
        return Err(Error::other(
            "Sigstore bundle has empty Rekor Signed Entry Timestamp",
        ));
    }
    let set_payload = serde_json::json!({
        "body": base64::engine::general_purpose::STANDARD.encode(&entry.canonicalized_body),
        "integratedTime": entry.integrated_time,
        "logIndex": entry.log_index,
        "logID": key_id,
    });
    let canonical_set = serde_json_canonicalizer::to_vec(&set_payload)
        .map_err(|error| Error::other(format!("canonicalizing Rekor SET payload: {error}")))?;
    rekor_key
        .verify_signature(
            Signature::Raw(&promise.signed_entry_timestamp),
            &canonical_set,
        )
        .map_err(|error| Error::other(format!("Rekor SET verification failed: {error}")))?;

    let proof = entry
        .inclusion_proof
        .as_ref()
        .ok_or_else(|| Error::other("Sigstore bundle missing Rekor inclusion proof"))?;
    if proof.log_index < 0 {
        return Err(Error::other(format!(
            "invalid Rekor proof index {}",
            proof.log_index
        )));
    }
    let root_hash = fixed_sha256(&proof.root_hash, "root hash")?;
    let hashes = proof
        .hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| fixed_sha256(hash, &format!("path hash {index}")))
        .collect::<Result<Vec<_>>>()?;
    let checkpoint = proof
        .checkpoint
        .as_ref()
        .ok_or_else(|| Error::other("Sigstore bundle inclusion proof missing checkpoint"))?;
    let signed_checkpoint: SignedCheckpoint =
        serde_json::from_value(serde_json::Value::String(checkpoint.envelope.clone()))
            .map_err(|error| Error::other(format!("invalid Rekor checkpoint: {error}")))?;
    let tree_size = u64::try_from(proof.tree_size)
        .map_err(|_| Error::other(format!("invalid Rekor tree size {}", proof.tree_size)))?;
    let proof = RekorInclusionProof::new(
        proof.log_index,
        root_hash,
        tree_size,
        hashes,
        Some(signed_checkpoint),
    );
    proof
        .verify(&entry.canonicalized_body, rekor_key)
        .map_err(|error| {
            Error::other(format!(
                "Rekor inclusion proof verification failed: {error}"
            ))
        })
}

fn fixed_sha256(bytes: &[u8], field: &str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| Error::other(format!("Rekor {field} must contain exactly 32 bytes")))
}

async fn fetch_attestations(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    digest: &str,
    sources: &[Source],
) -> Result<AttestationResponse> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/attestations/sha256:{digest}?per_page=30"
    );
    let urls = crate::http::github_url_candidates(sources, &url);
    crate::http::get_github_json_from_urls(client, &urls).await
}

async fn materialize_bundles(
    client: &reqwest::Client,
    entries: Vec<AttestationEntry>,
    sources: &[Source],
) -> Result<Vec<Vec<u8>>> {
    let mut bundles = Vec::new();
    for entry in entries {
        if let Some(url) = entry.bundle_url {
            let urls = crate::http::github_url_candidates(sources, &url);
            bundles.push(fetch_bundle_urls(client, &urls).await?);
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

async fn fetch_bundle_urls(client: &reqwest::Client, urls: &[String]) -> Result<Vec<u8>> {
    let mut last_error = None;
    for url in urls {
        match fetch_bundle_url(client, url).await {
            Ok(bundle) => return Ok(bundle),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::other("no attestation bundle URL candidates")))
}

async fn fetch_bundle_url(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| Error::other(format!("invalid attestation bundle URL: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(Error::other("attestation bundle URL must use HTTPS"));
    }
    let response = crate::http::github_request(client, parsed.as_str())
        .send()
        .await?
        .error_for_status()?;
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

    fn fixture_bundle_value() -> serde_json::Value {
        serde_json::from_slice(&fixture_bundle()).unwrap()
    }

    fn github_timestamp_bundle() -> Vec<u8> {
        let encoded: String =
            include_str!("../../tests/fixtures/attestation/github-tsa-bundle.json.b64")
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
    }

    fn fixture_rekor_keys() -> BTreeMap<String, CosignVerificationKey> {
        let trust_root = SigstoreTrustRoot::from_trusted_root_json_unchecked(TRUSTED_ROOT).unwrap();
        load_rekor_keys(&trust_root).unwrap()
    }

    fn verify_fixture_transparency(bundle: serde_json::Value) -> Result<()> {
        let bundle: Bundle = serde_json::from_value(bundle)?;
        verify_rekor_transparency(&bundle, &fixture_rekor_keys())
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
            sources: Vec::new(),
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
            &[],
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
                kind: "sigstore-bundle+rekor".into(),
                repository: "kubewarden/kubewarden-controller".into(),
                issuer: GITHUB_OIDC_ISSUER.into(),
                digest: "sha256:c811d58de79c92f03214e63aa339484e488d694ae8a6283b5f3f17a9faf50172"
                    .into(),
            }
        );
    }

    #[test]
    fn verifies_github_timestamp_bundle_without_rekor_entry() {
        verify_github_timestamp_bundle(
            &github_timestamp_bundle(),
            "jdx/communique",
            "b958c6046bab52febf958c94974e1ffcc450bff78c28d7233e179bfd73828912",
        )
        .unwrap();
    }

    #[test]
    fn rejects_wrong_repository_for_github_timestamp_bundle() {
        let error = verify_github_timestamp_bundle(
            &github_timestamp_bundle(),
            "jdx/other",
            "b958c6046bab52febf958c94974e1ffcc450bff78c28d7233e179bfd73828912",
        )
        .unwrap_err();
        assert!(error.to_string().contains("repository mismatch"));
    }

    #[test]
    fn rejects_bundle_without_rekor_inclusion_proof() {
        let mut bundle = fixture_bundle_value();
        bundle["verificationMaterial"]["tlogEntries"][0]
            .as_object_mut()
            .unwrap()
            .remove("inclusionProof");

        let error = verify_fixture_transparency(bundle).unwrap_err();
        assert!(error.to_string().contains("missing Rekor inclusion proof"));
    }

    #[test]
    fn rejects_tampered_rekor_inclusion_proof() {
        let mut bundle = fixture_bundle_value();
        let encoded = bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["hashes"]
            [0]
        .as_str()
        .unwrap();
        let mut hash = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        hash[0] ^= 0xff;
        bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["hashes"][0] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(hash));

        let error = verify_fixture_transparency(bundle).unwrap_err();
        assert!(error
            .to_string()
            .contains("Rekor inclusion proof verification failed"));
    }

    #[test]
    fn rejects_tampered_rekor_checkpoint() {
        let mut bundle = fixture_bundle_value();
        let envelope = bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]
            ["checkpoint"]["envelope"]
            .as_str()
            .unwrap();
        let (note, signatures) = envelope.split_once("\n\n").unwrap();
        let mut parts = signatures.trim().splitn(3, ' ');
        let dash = parts.next().unwrap();
        let name = parts.next().unwrap();
        let encoded = parts.next().unwrap();
        let mut signature = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        signature[4] ^= 0xff;
        let checkpoint = format!(
            "{note}\n\n{dash} {name} {}\n",
            base64::engine::general_purpose::STANDARD.encode(signature)
        );
        bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["checkpoint"]
            ["envelope"] = serde_json::Value::String(checkpoint);

        let error = verify_fixture_transparency(bundle).unwrap_err();
        assert!(error
            .to_string()
            .contains("Rekor inclusion proof verification failed"));
    }

    #[test]
    fn rejects_invalid_rekor_signed_entry_timestamp() {
        let mut bundle = fixture_bundle_value();
        let encoded = bundle["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"]
            ["signedEntryTimestamp"]
            .as_str()
            .unwrap();
        let mut signature = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        signature[0] ^= 0xff;
        bundle["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"]
            ["signedEntryTimestamp"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(signature));

        let error = verify_fixture_transparency(bundle).unwrap_err();
        assert!(error.to_string().contains("Rekor SET verification failed"));
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
            kind: "sigstore-bundle+rekor".into(),
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
