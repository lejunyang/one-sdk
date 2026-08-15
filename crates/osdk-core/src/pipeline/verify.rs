//! Checksum verification for downloaded archives.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
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
        let name = name.trim_start_matches('*');
        if name == filename {
            return Some(hash);
        }
    }
    None
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
}
