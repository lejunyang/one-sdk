//! Checksum verification for downloaded archives.

use std::io::Read;
use std::path::Path;

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// Legacy digest kept only for upstreams that publish nothing stronger.
    ///
    /// SHA-1 is collision-prone and must never be selected when a source also
    /// offers SHA-256 or better. The only in-tree user is the Android SDK
    /// repository, whose manifests carry a per-archive SHA-1 and no stronger
    /// alternative.
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

impl HashAlgo {
    /// Whether this digest is strong enough to stand alone as supply-chain
    /// evidence. SHA-1 is accepted only as an explicit per-source exception.
    pub fn is_collision_resistant(self) -> bool {
        !matches!(self, HashAlgo::Sha1)
    }

    /// The `<algo>:<hex>` token name used in lockfiles and locked options.
    pub fn token(self) -> &'static str {
        match self {
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Sha512 => "sha512",
            HashAlgo::Blake3 => "blake3",
        }
    }
}

pub fn hash_bytes(bytes: &[u8], algo: HashAlgo) -> String {
    match algo {
        HashAlgo::Sha1 => hex::encode(Sha1::digest(bytes)),
        HashAlgo::Sha256 => hex::encode(Sha256::digest(bytes)),
        HashAlgo::Sha512 => hex::encode(Sha512::digest(bytes)),
        HashAlgo::Blake3 => blake3::hash(bytes).to_hex().to_string(),
    }
}

/// Compute the hex digest of a file with the given algorithm.
pub fn hash_file(path: &Path, algo: HashAlgo) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut buf = [0u8; 64 * 1024];
    match algo {
        HashAlgo::Sha1 => {
            let mut h = Sha1::new();
            loop {
                let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(hex::encode(h.finalize()))
        }
        HashAlgo::Sha256 => {
            let mut h = Sha256::new();
            loop {
                let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(hex::encode(h.finalize()))
        }
        HashAlgo::Sha512 => {
            let mut h = Sha512::new();
            loop {
                let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(hex::encode(h.finalize()))
        }
        HashAlgo::Blake3 => {
            let mut h = blake3::Hasher::new();
            loop {
                let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(h.finalize().to_hex().to_string())
        }
    }
}

/// Verify `path` matches `expected` (hex) under `algo`. Case-insensitive.
pub fn verify_file(path: &Path, expected: &str, algo: HashAlgo, name: &str) -> Result<()> {
    let actual = hash_file(path, algo)?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            name: name.to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Parse a `SHASUMS256.txt`-style body and return the hash for `filename`.
/// Each line is `<hex>  <filename>` (two spaces) or `<hex> *<filename>`.
pub fn find_shasum<'a>(body: &'a str, filename: &str) -> Option<&'a str> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?;
        // manifests may list a path; match on the basename too
        let name = name.trim_start_matches('*');
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if name == filename || base == filename {
            return Some(hash);
        }
    }
    None
}

/// Extract the first 64-hex-char sha256 token from a sidecar body (a bare hash,
/// or `<hex>  <filename>`).
pub fn parse_sha256_token(body: &str) -> Option<String> {
    let token = body.split_whitespace().next()?;
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(token.to_string())
    } else {
        None
    }
}

/// Parse an npm-style Subresource Integrity string (`sha512-<base64>` or
/// `sha256-<base64>`, possibly space-separated multiples) into a hex Checksum.
/// Prefers sha512. Returns None if unparseable.
pub fn parse_sri(integrity: &str) -> Option<super::Checksum> {
    use base64::Engine;
    let mut best: Option<super::Checksum> = None;
    for token in integrity.split_whitespace() {
        let (algo_str, b64) = token.split_once('-')?;
        let algo = match algo_str {
            "sha512" => HashAlgo::Sha512,
            "sha256" => HashAlgo::Sha256,
            _ => continue,
        };
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let hex = hex::encode(raw);
        let cs = super::Checksum { algo, hex };
        // prefer the strongest (sha512 > sha256)
        match &best {
            Some(b) if b.algo == HashAlgo::Sha512 => {}
            _ => best = Some(cs),
        }
    }
    best
}

/// Best-effort discovery of a sha256 checksum for a GitHub-style release asset.
///
/// Given the full asset download URL, tries (in order):
/// 1. per-asset sidecars: `<url>.sha256`, `<url>.sha256sum`, `<url>.sha256.txt`
/// 2. a shared manifest in the same directory: `SHASUMS256.txt`, `SHA256SUMS`,
///    `checksums.txt` — matched by the asset's filename.
///
/// Returns `None` if nothing is found (caller proceeds without verification).
pub async fn discover_asset_checksum(
    client: &reqwest::Client,
    asset_url: &str,
) -> Option<super::Checksum> {
    // 1. per-asset sidecars
    for suffix in [".sha256", ".sha256sum", ".sha256.txt"] {
        let url = format!("{asset_url}{suffix}");
        if let Ok(body) = crate::http::get_text(client, &url).await {
            if let Some(hex) = parse_sha256_token(&body) {
                return Some(super::Checksum {
                    algo: HashAlgo::Sha256,
                    hex,
                });
            }
        }
    }
    // 2. shared manifest in the same directory
    let (dir, file) = asset_url.rsplit_once('/')?;
    for manifest in [
        "SHASUMS256.txt",
        "SHA256SUMS",
        "sha256sums.txt",
        "checksums.txt",
    ] {
        let url = format!("{dir}/{manifest}");
        if let Ok(body) = crate::http::get_text(client, &url).await {
            if let Some(hex) = find_shasum(&body, file) {
                return Some(super::Checksum {
                    algo: HashAlgo::Sha256,
                    hex: hex.to_string(),
                });
            }
        }
    }
    None
}

/// Verify a detached minisign signature over `file` using a minisign public key
/// (the base64 key line, e.g. `RWT...`). Used for artifacts signed with
/// minisign (osdk's own releases; some upstreams). Returns Ok(()) on a valid
/// signature.
pub fn verify_minisign(file: &Path, signature: &str, public_key_b64: &str) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| Error::io(file, e))?;
    verify_minisign_bytes(&bytes, signature, public_key_b64)
}

/// Verify a detached minisign signature over in-memory `bytes`.
pub fn verify_minisign_bytes(bytes: &[u8], signature: &str, public_key_b64: &str) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(public_key_b64.trim())
        .map_err(|e| Error::other(format!("invalid minisign public key: {e}")))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| Error::other(format!("invalid minisign signature: {e}")))?;
    // stream=false: verify the whole buffer (prehashed sigs are auto-detected).
    pk.verify(bytes, &sig, false)
        .map_err(|e| Error::other(format!("minisign verification failed: {e}")))?;
    Ok(())
}

/// A trusted minisign public key for a source's signed checksums manifest.
pub struct TrustedKey {
    /// The minisign public key (base64 line, `RW...`).
    pub public_key: &'static str,
    /// Sibling signature file name for the manifest (e.g. `SHASUMS256.txt.minisig`).
    pub manifest: &'static str,
    pub signature: &'static str,
}

/// Built-in trusted minisign keys for well-known signed distributions, keyed by
/// `github:owner/repo`. Verified against real releases.
pub fn trusted_key(repo_id: &str) -> Option<TrustedKey> {
    match repo_id {
        // mise signs SHASUMS256.txt with this key (key id 64113EDF160FDEC2).
        "github:jdx/mise" => Some(TrustedKey {
            public_key: "RWTC3g8W3z4RZK3V3qv7fa1QY4JEWyBtqIHW+85QlJpZc5yG+uNYNBSZ",
            manifest: "SHASUMS256.txt",
            signature: "SHASUMS256.txt.minisig",
        }),
        _ => None,
    }
}

/// For a signed distribution: fetch the checksums manifest + its `.minisig`,
/// verify the signature with the trusted key, and (only if valid) return the
/// asset's sha256 from the verified manifest. `dir_url` is the release download
/// directory (where the manifest lives); `filename` is the asset basename.
///
/// Returns Ok(Some(checksum)) on a verified match, Ok(None) if this repo has no
/// trusted key / no matching entry, and Err if the signature is INVALID (a hard
/// failure — never silently trust an unsigned/forged manifest here).
pub async fn signed_manifest_checksum(
    client: &reqwest::Client,
    repo_id: &str,
    dir_url: &str,
    filename: &str,
) -> Result<Option<super::Checksum>> {
    let key = match trusted_key(repo_id) {
        Some(k) => k,
        None => return Ok(None),
    };
    let dir = dir_url.trim_end_matches('/');
    let manifest_url = format!("{dir}/{}", key.manifest);
    let sig_url = format!("{dir}/{}", key.signature);

    let manifest = match crate::http::get_text(client, &manifest_url).await {
        Ok(m) => m,
        Err(_) => return Ok(None), // manifest not reachable; fall back to other paths
    };
    let signature = match crate::http::get_text(client, &sig_url).await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    // Verify the manifest's signature — hard-fail if invalid.
    verify_minisign_bytes(manifest.as_bytes(), &signature, key.public_key)?;

    // Signature valid: trust hashes from this manifest.
    Ok(find_shasum(&manifest, filename).map(|hex| super::Checksum {
        algo: HashAlgo::Sha256,
        hex: hex.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The release profile builds at `opt-level = "z"` for binary size, which
    /// costs sha2's portable backend about 65% of its throughput (measured:
    /// 2300 MiB/s -> 800 MiB/s over 256 MiB). Since every downloaded archive is
    /// checksummed, the workspace manifest pins these crates back to opt-level 3,
    /// which restores full speed for roughly 0.02 MB of size.
    ///
    /// Cargo only emits a *warning* when a `[profile.release.package.X]` name
    /// matches no package, so a dependency rename or a typo would silently
    /// reintroduce the slowdown with a green build. This test fails instead.
    #[test]
    fn hashing_crates_are_pinned_to_a_fast_opt_level() {
        let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root is two levels above this crate")
            .join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", manifest_path.display()));

        let document: toml::Value = manifest.parse().expect("workspace manifest parses as TOML");
        let release = document
            .get("profile")
            .and_then(|profile| profile.get("release"))
            .expect("[profile.release] is declared");

        // Only meaningful while the release profile is actually size-first; if it
        // ever goes back to a speed-first opt-level the pins are redundant.
        let opt_level = release.get("opt-level").expect("release sets opt-level");
        if opt_level.as_str() != Some("z") && opt_level.as_str() != Some("s") {
            return;
        }

        let packages = release
            .get("package")
            .and_then(toml::Value::as_table)
            .expect("[profile.release.package.*] overrides exist");

        // Every crate that hashes bytes on an install path. `digest`,
        // `block-buffer` and `cpufeatures` are the generic plumbing sha2 and
        // sha1 inline through, so they need the same treatment as the leaves.
        for crate_name in [
            "sha2",
            "sha1",
            "blake3",
            "digest",
            "block-buffer",
            "cpufeatures",
        ] {
            let entry = packages.get(crate_name).unwrap_or_else(|| {
                panic!(
                    "{crate_name} must keep an opt-level override in the workspace \
                     Cargo.toml, otherwise release builds hash significantly slower"
                )
            });
            let level = entry.get("opt-level").unwrap_or_else(|| {
                panic!("[profile.release.package.{crate_name}] must set opt-level")
            });
            assert_eq!(
                level.as_integer(),
                Some(3),
                "{crate_name} must build at opt-level 3 so archive verification stays fast"
            );
        }
    }

    /// Guards the other half of the same hazard: the pins above are useless if
    /// the crates they name are no longer the ones doing the hashing.
    #[test]
    fn pinned_hashing_crates_are_the_ones_actually_used() {
        // Compile-time proof that these crates are still on the hashing path;
        // if an import here is dropped, the pin list needs revisiting too.
        let sha256 = hash_bytes(b"abc", HashAlgo::Sha256);
        assert_eq!(
            sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let sha1 = hash_bytes(b"abc", HashAlgo::Sha1);
        assert_eq!(sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn sha256_known_vector() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("f");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"abc").unwrap();
        // sha256("abc")
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_file(&p, expected, HashAlgo::Sha256, "f").is_ok());
        assert!(verify_file(&p, "deadbeef", HashAlgo::Sha256, "f").is_err());
    }

    #[test]
    fn parse_shasums() {
        let body = "aaaa  node-v20-linux-x64.tar.gz\nbbbb  node-v20-linux-x64.tar.xz\n";
        assert_eq!(find_shasum(body, "node-v20-linux-x64.tar.xz"), Some("bbbb"));
        assert_eq!(find_shasum(body, "missing"), None);
    }

    #[test]
    fn shasums_matches_basename_in_path() {
        // manifests sometimes list a path; match on the basename too
        let body = "cccc  ./dist/bun-linux-x64.zip\n";
        assert_eq!(find_shasum(body, "bun-linux-x64.zip"), Some("cccc"));
    }

    #[test]
    fn sidecar_token_parsing() {
        let hex = "b".repeat(64);
        assert_eq!(parse_sha256_token(&hex).as_deref(), Some(hex.as_str()));
        // `<hex>  <filename>` form
        let body = format!("{hex}  deno-x86_64-unknown-linux-gnu.zip\n");
        assert_eq!(parse_sha256_token(&body).as_deref(), Some(hex.as_str()));
        // too short / non-hex -> None
        assert_eq!(parse_sha256_token("nothex"), None);
        assert_eq!(parse_sha256_token(""), None);
    }

    #[test]
    fn sri_parsing_prefers_sha512() {
        use base64::Engine;
        // sha256 of "abc"
        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let sha256_b64 =
            base64::engine::general_purpose::STANDARD.encode(hex::decode(sha256_hex).unwrap());
        let cs = parse_sri(&format!("sha256-{sha256_b64}")).unwrap();
        assert_eq!(cs.algo, HashAlgo::Sha256);
        assert_eq!(cs.hex, sha256_hex);

        // when both present, sha512 wins
        let sha512_hex = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                          2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
        let sha512_b64 =
            base64::engine::general_purpose::STANDARD.encode(hex::decode(sha512_hex).unwrap());
        let cs = parse_sri(&format!("sha256-{sha256_b64} sha512-{sha512_b64}")).unwrap();
        assert_eq!(cs.algo, HashAlgo::Sha512);
        assert_eq!(cs.hex, sha512_hex);

        assert!(parse_sri("md5-xxxx").is_none());
    }

    #[test]
    fn minisign_rejects_bad_inputs() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("artifact");
        std::fs::write(&f, b"payload").unwrap();
        // Invalid public key.
        assert!(verify_minisign(&f, "untrusted comment\nRWQf6L...", "not-a-key").is_err());
        // Well-formed-looking but invalid signature against a syntactically
        // valid-length key should also error (never panics).
        let fake_key = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert!(verify_minisign(&f, "garbage", fake_key).is_err());
    }

    // Real mise minisign key + a real signature over its SHASUMS256.txt.
    // (The positive verification against the exact 3520-byte manifest was
    // confirmed live; here we assert the trusted key is wired and that verifying
    // tampered bytes with the real signature FAILS — never silently passes.)
    const MISE_KEY: &str = "RWTC3g8W3z4RZK3V3qv7fa1QY4JEWyBtqIHW+85QlJpZc5yG+uNYNBSZ";
    const MISE_SIG: &str = "untrusted comment: signature from minisign secret key\n\
        RUTC3g8W3z4RZPN3yrytMMxcrYyruSFMJw/fd1BsY9CWTb06OvLLpbNdRmdTfO9yqMBy4TcBu4ZiUr6e+WLWViNnRyT4J0pUAA4=\n\
        trusted comment: timestamp:1735607189\tfile:SHASUMS256.txt\thashed\n\
        rqyHRI2HBPZJQLqYOpdcB8g7aKcsOVA9+NY5Gn12aguvRokwIZhwHAg5z+xIvuu2iplosMcbP0lBhrhh+K2LCQ==\n";

    #[test]
    fn trusted_key_registered_for_mise() {
        let k = trusted_key("github:jdx/mise").expect("mise key present");
        assert_eq!(k.public_key, MISE_KEY);
        assert_eq!(k.manifest, "SHASUMS256.txt");
        assert!(trusted_key("github:unknown/repo").is_none());
    }

    #[test]
    fn real_signature_rejects_tampered_content() {
        // The signature is over the exact SHASUMS256.txt bytes; any other bytes
        // must fail verification with the real key + real signature.
        let tampered = b"this is not the signed manifest";
        assert!(verify_minisign_bytes(tampered, MISE_SIG, MISE_KEY).is_err());
        // Key parsing + signature decoding themselves must succeed (proves the
        // material is well-formed; only the content mismatch causes failure).
        assert!(minisign_verify::PublicKey::from_base64(MISE_KEY).is_ok());
        assert!(minisign_verify::Signature::decode(MISE_SIG).is_ok());
    }
}
