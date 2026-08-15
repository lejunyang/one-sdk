//! Checksum verification for downloaded archives.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256, Sha512};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
    Sha512,
    Blake3,
}

/// Compute the hex digest of a file with the given algorithm.
pub fn hash_file(path: &Path, algo: HashAlgo) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut buf = [0u8; 64 * 1024];
    match algo {
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
    let pk = minisign_verify::PublicKey::from_base64(public_key_b64.trim())
        .map_err(|e| Error::other(format!("invalid minisign public key: {e}")))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| Error::other(format!("invalid minisign signature: {e}")))?;
    let bytes = std::fs::read(file).map_err(|e| Error::io(file, e))?;
    // stream=false: verify the whole buffer (prehashed sigs are auto-detected).
    pk.verify(&bytes, &sig, false)
        .map_err(|e| Error::other(format!("minisign verification failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
}
