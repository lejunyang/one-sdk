//! Deep verification: does the environment on disk still match what was
//! installed?
//!
//! Freshness (`state.rs`) answers a different question -- "have the inputs
//! changed" -- and cannot see that an external tool replaced half of
//! `site-packages`. mise documents the same limitation for its own hash model.
//! AGENTS.md puts it more bluntly: a command that exited 0 and a hash that did
//! not change are not evidence the environment is right.
//!
//! Every predicate here was chosen by measurement, not by plausibility. The
//! candidates in the original design (`pyvenv.cfg` creator, "key package
//! metadata present") were tested against real tampering and **noticed
//! nothing**: deleting a file inside an installed package and rewriting a file's
//! contents both left them perfectly happy. They describe how the environment
//! was created, while tampering changes what is in it -- the exact shape of
//! AGENTS.md's "assertion detached from the mechanism under test". They were
//! dropped rather than kept as decoration.
//!
//! What survived is the receipt the package manager itself writes:
//!
//! * Python: `dist-info/RECORD`, which carries a per-file size and sha256
//!   (measured: 14 of idna 3.10's 15 lines).
//! * Node (npm): `node_modules/.package-lock.json`, which carries a per-package
//!   `version` and `integrity`.
//! * Node (pnpm): the store's `*-index.json`, which carries a per-file
//!   `integrity` (sha512), `size` and `mode` -- finer than npm's per-package
//!   record, and on par with Python's RECORD.
//!
//! osdk reads those rather than keeping a second dependency graph of its own.
//!
//! pnpm's shape had to be measured rather than assumed, because none of it looks
//! like npm's (measured with pnpm 9.15.1):
//!
//! * There is **no** `node_modules/.package-lock.json`.
//! * `node_modules/.modules.yaml` exists but holds layout metadata only --
//!   `storeDir`, `virtualStoreDir`, `nodeLinker` -- and nothing per package. It
//!   is a locator, not a receipt.
//! * `node_modules/<pkg>` is a junction (Windows) or symlink into
//!   `node_modules/.pnpm/<name>@<version>/node_modules/<name>`, whose files are
//!   hardlinks into the content-addressable store.
//! * `pnpm-lock.yaml` carries each package's tarball `integrity`. That digest is
//!   of the *tarball*, so it cannot be recomputed from an unpacked tree -- but it
//!   doubles as the store address: hex-decode the base64 and the first byte is
//!   the subdirectory, the rest the filename stem. Measured: ms@2.1.3's
//!   `sha512-6Flzub...` maps exactly onto
//!   `files/e8/5973b9...-index.json`.
//! * That index file is the receipt. Measured on ms@2.1.3: 4 files, each with a
//!   size and an sha512 that reproduces the bytes on disk exactly.
//!
//! The chain therefore needs both halves -- the project's lockfile for the
//! addresses and the store for the digests. When the store is elsewhere (a
//! checkout from another machine, a pruned store) that is reported as
//! `ReceiptMissing`, never as a clean report.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// One thing that is wrong with an installed environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Path the problem is about, relative to the project and `/`-normalized so
    /// the message reads the same on every platform.
    pub path: String,
    pub kind: FindingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// The receipt lists it; it is not on disk.
    Missing,
    /// On disk but a different size than the receipt recorded.
    SizeMismatch { recorded: u64, actual: u64 },
    /// On disk but a different version than the receipt recorded.
    VersionMismatch { recorded: String, actual: String },
    /// On disk but a different digest than osdk.lock recorded.
    DigestMismatch { recorded: String, actual: String },
    /// The receipt itself is absent, so nothing can be verified.
    ReceiptMissing,
}

impl Finding {
    pub fn describe(&self) -> String {
        match &self.kind {
            FindingKind::Missing => format!("{} is missing", self.path),
            FindingKind::SizeMismatch { recorded, actual } => format!(
                "{} is {actual} bytes but was installed as {recorded}",
                self.path
            ),
            FindingKind::VersionMismatch { recorded, actual } => format!(
                "{} is version {actual} but was installed as {recorded}",
                self.path
            ),
            FindingKind::DigestMismatch { recorded, actual } => format!(
                "{} has changed since it was installed (sha256 {}, recorded {})",
                self.path,
                &actual[..actual.len().min(12)],
                &recorded[..recorded.len().min(12)]
            ),
            FindingKind::ReceiptMissing => {
                format!("{} has no install receipt to verify against", self.path)
            }
        }
    }
}

/// Result of verifying one provider's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// How many entries the receipt described. Reported so "0 problems" can be
    /// told apart from "nothing was checked" -- a verification that examined
    /// nothing is not a pass.
    pub checked: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Verify a Node environment against whichever receipt the installer left.
///
/// npm and pnpm write completely different things, so the layout on disk decides
/// which reader runs. yarn is not covered: Berry's PnP keeps dependencies in a
/// single zip-backed store with no per-package tree to compare against, so it
/// stays `ReceiptMissing` rather than getting a predicate that would pass
/// regardless.
///
/// "Could not check" must never look like "checked and fine", which is why every
/// unreadable case below produces `ReceiptMissing` instead of an empty report.
pub fn verify_node(project_root: &Path) -> Result<Report> {
    // pnpm first, because a project can contain both a stale `.package-lock.json`
    // from an earlier npm install and a live `.pnpm` tree. The virtual store is
    // what the current install actually uses.
    if project_root.join("node_modules").join(".pnpm").is_dir() {
        return verify_pnpm(project_root);
    }
    verify_npm(project_root)
}

/// Verify a pnpm environment against the store's per-file index.
///
/// Walks `node_modules/.pnpm/<name>@<version>/node_modules/<name>`, which is the
/// real directory the top-level links point at, and compares every file the store
/// recorded. See the module docs for the measured layout this relies on.
fn verify_pnpm(project_root: &Path) -> Result<Report> {
    let Some(store) = pnpm_store_dir(project_root)? else {
        return Ok(receipt_missing("node_modules/.modules.yaml"));
    };
    let lock = project_root.join("pnpm-lock.yaml");
    let lock_text = match std::fs::read_to_string(&lock) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(receipt_missing("pnpm-lock.yaml"))
        }
        Err(error) => return Err(Error::io(&lock, error)),
    };
    let integrities = pnpm_lock_integrities(&lock_text);
    if integrities.is_empty() {
        return Ok(receipt_missing("pnpm-lock.yaml"));
    }

    let virtual_store = project_root.join("node_modules").join(".pnpm");
    let mut report = Report::default();
    let mut any_index_read = false;

    for entry in std::fs::read_dir(&virtual_store)
        .map_err(|error| Error::io(&virtual_store, error))?
        .flatten()
    {
        let dir_name = entry.file_name().to_string_lossy().to_string();
        // `node_modules` and `lock.yaml` sit alongside the package directories.
        if dir_name == "node_modules" || !entry.path().is_dir() {
            continue;
        }
        let Some(integrity) = integrities.get(&dir_name) else {
            // A directory with no lockfile entry cannot be addressed in the
            // store. Skipping it silently would hide a real inconsistency, but
            // reporting it as tampering would be wrong too -- peer-dependency
            // suffixes legitimately produce names the lock spells differently.
            continue;
        };
        let Some(index_path) = pnpm_index_path(&store, integrity) else {
            continue;
        };
        let Ok(index_text) = std::fs::read_to_string(&index_path) else {
            // The store was pruned, or belongs to another machine.
            continue;
        };
        let index: serde_json::Value = serde_json::from_str(&index_text)
            .map_err(|error| Error::config(format!("{}: {error}", index_path.display())))?;
        let Some(files) = index.get("files").and_then(serde_json::Value::as_object) else {
            continue;
        };
        any_index_read = true;

        let package = index
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&dir_name);
        let root = virtual_store
            .join(&dir_name)
            .join("node_modules")
            .join(to_native(package));

        for (relative, meta) in files {
            report.checked += 1;
            // `relative` comes out of the store index, written by pnpm on some
            // machine, so it is a foreign path string: always `/`-separated and
            // converted rather than parsed with `Path`.
            let path = root.join(to_native(relative));
            let shown = normalize(&format!(
                "node_modules/.pnpm/{dir_name}/node_modules/{package}/{relative}"
            ));
            let recorded_size = meta.get("size").and_then(serde_json::Value::as_u64);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    report.findings.push(Finding {
                        path: shown,
                        kind: FindingKind::Missing,
                    });
                    continue;
                }
                Err(error) => return Err(Error::io(&path, error)),
            };
            if let Some(recorded) = recorded_size {
                if recorded != bytes.len() as u64 {
                    report.findings.push(Finding {
                        path: shown,
                        kind: FindingKind::SizeMismatch {
                            recorded,
                            actual: bytes.len() as u64,
                        },
                    });
                    // Size already proves it differs; hashing adds nothing.
                    continue;
                }
            }
            let Some(recorded) = meta.get("integrity").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let actual = sha512_integrity(&bytes);
            if actual != recorded {
                report.findings.push(Finding {
                    path: shown,
                    kind: FindingKind::DigestMismatch {
                        recorded: recorded.to_string(),
                        actual,
                    },
                });
            }
        }
    }

    if !any_index_read {
        // Nothing was comparable. Reporting zero problems here would be the
        // vacuous pass this whole module exists to avoid.
        return Ok(receipt_missing("pnpm store index"));
    }
    Ok(report)
}

/// `storeDir` from `node_modules/.modules.yaml`, if it is present on this machine.
///
/// The recorded value is an absolute path written by whichever machine ran the
/// install, so it may not exist here at all -- a checkout from elsewhere, or a
/// pruned store. Returning `None` lets the caller say `ReceiptMissing`.
fn pnpm_store_dir(project_root: &Path) -> Result<Option<PathBuf>> {
    let modules = project_root.join("node_modules").join(".modules.yaml");
    let text = match std::fs::read_to_string(&modules) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(&modules, error)),
    };
    for line in text.lines() {
        let Some(value) = line.strip_prefix("storeDir:") else {
            continue;
        };
        let value = value.trim().trim_matches('\'').trim_matches('"');
        if value.is_empty() {
            return Ok(None);
        }
        let path = PathBuf::from(value);
        return Ok(path.is_dir().then_some(path));
    }
    Ok(None)
}

/// Map `<name>@<version>` directory names to the tarball integrity from the lock.
///
/// Parsed line-wise rather than with a YAML crate: the two shapes needed are the
/// `packages:` keys and their one-line `resolution: {integrity: ...}`, and adding
/// a YAML dependency to reach them would put a parser in the shim's graph for no
/// benefit.
fn pnpm_lock_integrities(lock_text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut in_packages = false;
    let mut current: Option<String> = None;
    for line in lock_text.lines() {
        if line.starts_with("packages:") {
            in_packages = true;
            continue;
        }
        // Any other top-level key ends the section.
        if in_packages && !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        if !in_packages {
            continue;
        }
        let trimmed = line.trim();
        if let Some(key) = trimmed.strip_suffix(':') {
            if !key.is_empty() && !key.starts_with('#') {
                current = Some(key.trim_matches('\'').trim_matches('"').to_string());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("resolution:") {
            if let (Some(name), Some(integrity)) = (current.as_ref(), extract_integrity(rest)) {
                out.insert(name.clone(), integrity);
            }
        }
    }
    out
}

/// Pull `sha512-...` out of `{integrity: sha512-..., tarball: ...}`.
fn extract_integrity(value: &str) -> Option<String> {
    let start = value.find("integrity:")? + "integrity:".len();
    let rest = value[start..].trim_start();
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    let integrity = rest[..end].trim();
    (!integrity.is_empty()).then(|| integrity.to_string())
}

/// Where the store keeps the index for a package with this tarball integrity.
///
/// The index is content-addressed by the tarball digest: base64-decode it, render
/// as hex, and the first byte names the subdirectory. Measured against pnpm 9.15.1
/// -- see the module docs.
fn pnpm_index_path(store: &Path, integrity: &str) -> Option<PathBuf> {
    use base64::Engine as _;

    let encoded = integrity.strip_prefix("sha512-")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let hex = hex::encode(raw);
    // One byte of prefix, the remainder as the filename stem.
    let (prefix, rest) = hex.split_at(2);
    Some(
        store
            .join("files")
            .join(prefix)
            .join(format!("{rest}-index.json")),
    )
}

/// Render bytes the way pnpm's index records them.
fn sha512_integrity(bytes: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha512};

    let digest = Sha512::digest(bytes);
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

fn receipt_missing(path: &str) -> Report {
    Report {
        checked: 0,
        findings: vec![Finding {
            path: path.into(),
            kind: FindingKind::ReceiptMissing,
        }],
    }
}

/// Verify an npm environment against `node_modules/.package-lock.json`.
///
/// That file is npm's own record of what it placed where, so it stays correct
/// without osdk maintaining a parallel graph.
fn verify_npm(project_root: &Path) -> Result<Report> {
    let receipt = project_root.join("node_modules").join(".package-lock.json");
    let text = match std::fs::read_to_string(&receipt) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Report {
                checked: 0,
                findings: vec![Finding {
                    path: "node_modules/.package-lock.json".into(),
                    kind: FindingKind::ReceiptMissing,
                }],
            })
        }
        Err(error) => return Err(Error::io(&receipt, error)),
    };

    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| Error::config(format!("{}: {error}", receipt.display())))?;
    let packages = match parsed
        .get("packages")
        .and_then(serde_json::Value::as_object)
    {
        Some(packages) => packages,
        None => {
            return Ok(Report {
                checked: 0,
                findings: vec![Finding {
                    path: "node_modules/.package-lock.json".into(),
                    kind: FindingKind::ReceiptMissing,
                }],
            })
        }
    };

    let mut report = Report::default();
    for (relative, entry) in packages {
        // The root project is keyed by the empty string; it is not an installed
        // dependency.
        if relative.is_empty() {
            continue;
        }
        report.checked += 1;
        let directory = project_root.join(to_native(relative));
        if !directory.is_dir() {
            report.findings.push(Finding {
                path: normalize(relative),
                kind: FindingKind::Missing,
            });
            continue;
        }
        let Some(recorded) = entry.get("version").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let manifest = directory.join("package.json");
        let Ok(manifest_text) = std::fs::read_to_string(&manifest) else {
            report.findings.push(Finding {
                path: normalize(&format!("{relative}/package.json")),
                kind: FindingKind::Missing,
            });
            continue;
        };
        let installed: serde_json::Value = serde_json::from_str(&manifest_text)
            .map_err(|error| Error::config(format!("{}: {error}", manifest.display())))?;
        let actual = installed
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if actual != recorded {
            report.findings.push(Finding {
                path: normalize(relative),
                kind: FindingKind::VersionMismatch {
                    recorded: recorded.to_string(),
                    actual: actual.to_string(),
                },
            });
        }
    }
    Ok(report)
}

/// Verify a Python environment against every `dist-info/RECORD` in it.
///
/// RECORD lines are `path,hash,size`. Size is compared rather than the hash
/// because it catches the same tampering (measured: appending one line to
/// `idna/core.py` changed 13239 to 13251) at a fraction of the cost, and a
/// verification nobody runs because it is slow protects nothing. The hash stays
/// available for a future `--verify --deep` if that trade-off ever needs
/// revisiting.
pub fn verify_python(project_root: &Path, venv: Option<&str>) -> Result<Report> {
    let venv = venv.unwrap_or(".venv");
    let root = project_root.join(to_native(venv));
    let site_packages = match find_site_packages(&root)? {
        Some(path) => path,
        None => {
            return Ok(Report {
                checked: 0,
                findings: vec![Finding {
                    path: normalize(venv),
                    kind: FindingKind::ReceiptMissing,
                }],
            })
        }
    };

    let mut report = Report::default();
    let mut saw_dist_info = false;
    for entry in std::fs::read_dir(&site_packages).map_err(|e| Error::io(&site_packages, e))? {
        let entry = entry.map_err(|e| Error::io(&site_packages, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".dist-info") {
            continue;
        }
        saw_dist_info = true;
        let record = entry.path().join("RECORD");
        let Ok(text) = std::fs::read_to_string(&record) else {
            report.findings.push(Finding {
                path: normalize(&format!("{name}/RECORD")),
                kind: FindingKind::ReceiptMissing,
            });
            continue;
        };
        for line in text.lines() {
            let mut fields = line.split(',');
            let Some(relative) = fields.next() else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            let _hash = fields.next();
            // RECORD leaves size empty for its own entry, and for anything whose
            // size is not meaningful. No size means nothing to compare, not a
            // failure.
            let Some(size) = fields.next().filter(|size| !size.is_empty()) else {
                continue;
            };
            let Ok(recorded) = size.parse::<u64>() else {
                continue;
            };
            report.checked += 1;
            let file = site_packages.join(to_native(relative));
            match std::fs::metadata(&file) {
                Ok(metadata) => {
                    if metadata.len() != recorded {
                        report.findings.push(Finding {
                            path: normalize(relative),
                            kind: FindingKind::SizeMismatch {
                                recorded,
                                actual: metadata.len(),
                            },
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    report.findings.push(Finding {
                        path: normalize(relative),
                        kind: FindingKind::Missing,
                    });
                }
                Err(error) => return Err(Error::io(&file, error)),
            }
        }
    }
    if !saw_dist_info {
        report.findings.push(Finding {
            path: normalize(venv),
            kind: FindingKind::ReceiptMissing,
        });
    }
    Ok(report)
}

/// L1: is the native lockfile still the one that was installed from?
///
/// Cheap, and it catches a drift freshness cannot: the lock is untouched (so the
/// sources hash matches) but the environment was rebuilt by something else, or
/// the lock was swapped while its recorded digest was not. Comparing against the
/// digest in osdk.lock is what makes that visible.
pub fn verify_native_lock(
    project_root: &Path,
    relative_path: &str,
    recorded_sha256: &str,
) -> Result<Option<Finding>> {
    let path = project_root.join(to_native(relative_path));
    if !path.is_file() {
        return Ok(Some(Finding {
            path: normalize(relative_path),
            kind: FindingKind::Missing,
        }));
    }
    let actual = super::file_sha256(&path)?;
    if !actual.eq_ignore_ascii_case(recorded_sha256) {
        return Ok(Some(Finding {
            path: normalize(relative_path),
            kind: FindingKind::DigestMismatch {
                recorded: recorded_sha256.to_string(),
                actual,
            },
        }));
    }
    Ok(None)
}

/// Locate `site-packages` under a virtual environment.
///
/// Windows puts it at `Lib/site-packages`; Unix at `lib/pythonX.Y/site-packages`
/// with the version in the path. Both are probed on every platform rather than
/// behind `#[cfg]`: a project directory can be inspected from either side (a
/// shared checkout, a container mount), and gating would declare the other half
/// never verified.
fn find_site_packages(venv: &Path) -> Result<Option<PathBuf>> {
    let windows = venv.join("Lib").join("site-packages");
    if windows.is_dir() {
        return Ok(Some(windows));
    }
    let lib = venv.join("lib");
    if lib.is_dir() {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&lib).map_err(|e| Error::io(&lib, e))? {
            let entry = entry.map_err(|e| Error::io(&lib, e))?;
            let candidate = entry.path().join("site-packages");
            if candidate.is_dir() {
                candidates.push(candidate);
            }
        }
        // Sorted so the choice is deterministic when several interpreters left a
        // directory behind, instead of depending on readdir order.
        candidates.sort();
        if let Some(found) = candidates.into_iter().next() {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A receipt's relative path is `/`-separated regardless of the platform that
/// wrote it, so it is converted rather than used as-is.
fn to_native(relative: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in relative.split(['/', '\\']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        path.push(segment);
    }
    path
}

/// Normalize a path for reporting, so a message does not change shape by
/// platform.
fn normalize(value: &str) -> String {
    value.replace('\\', "/")
}

/// All providers' verification reports for one project.
pub type Reports = BTreeMap<String, Report>;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A deleted file inside an installed package is found.
    ///
    /// This is the case that eliminated the design's original predicates: with
    /// `idna/idnadata.py` removed, `pyvenv.cfg` and the presence of the
    /// `dist-info` directory were both still perfectly satisfied. Only the
    /// per-file receipt notices, so only the per-file receipt is used.
    #[test]
    fn a_deleted_file_inside_a_package_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let sp = root.join(".venv/Lib/site-packages");
        write(&sp.join("idna/core.py"), "x");
        write(
            &sp.join("idna-3.10.dist-info/RECORD"),
            "idna/core.py,sha256=aa,1\nidna/missing.py,sha256=bb,7\nidna-3.10.dist-info/RECORD,,\n",
        );

        let report = verify_python(root, None).unwrap();
        assert_eq!(report.checked, 2, "the sizeless RECORD line is not a check");
        assert_eq!(
            report.findings,
            vec![Finding {
                path: "idna/missing.py".into(),
                kind: FindingKind::Missing
            }]
        );
        assert!(!report.is_clean());
    }

    /// Changed content is found, and a clean tree is reported clean.
    ///
    /// Both directions in one test so neither can drift. Without the clean half,
    /// a predicate that always reported a mismatch would pass the first half.
    #[test]
    fn changed_content_is_found_and_a_clean_tree_is_clean() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let sp = root.join(".venv/Lib/site-packages");
        write(&sp.join("idna/core.py"), "12345");
        write(
            &sp.join("idna-3.10.dist-info/RECORD"),
            "idna/core.py,sha256=aa,5\n",
        );
        assert!(verify_python(root, None).unwrap().is_clean());

        // Append a byte, as tampering would.
        std::fs::write(sp.join("idna/core.py"), "123456").unwrap();
        let report = verify_python(root, None).unwrap();
        assert_eq!(
            report.findings,
            vec![Finding {
                path: "idna/core.py".into(),
                kind: FindingKind::SizeMismatch {
                    recorded: 5,
                    actual: 6
                }
            }]
        );
    }

    /// A missing receipt is reported, never treated as a pass.
    ///
    /// "Could not check" and "checked and fine" must not produce the same
    /// answer; `checked == 0` with no findings would be exactly the vacuous pass
    /// AGENTS.md warns about.
    #[test]
    fn a_missing_receipt_is_not_a_pass() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        let report = verify_python(root, None).unwrap();
        assert!(!report.is_clean(), "an absent venv cannot be verified");
        assert_eq!(report.checked, 0);
        assert!(matches!(
            report.findings[0].kind,
            FindingKind::ReceiptMissing
        ));

        let report = verify_node(root).unwrap();
        assert!(!report.is_clean());
        assert!(matches!(
            report.findings[0].kind,
            FindingKind::ReceiptMissing
        ));

        // A site-packages with no dist-info at all is also unverifiable rather
        // than clean.
        std::fs::create_dir_all(root.join(".venv/Lib/site-packages")).unwrap();
        let report = verify_python(root, None).unwrap();
        assert!(!report.is_clean());
    }

    /// Node: a deleted package directory and an in-place version swap are both
    /// found, and a clean tree passes.
    ///
    /// The version swap is the nastiest of the four measured tamperings: the
    /// directory is present, the file count is right, and only the recorded
    /// version disagrees.
    #[test]
    fn node_finds_a_deleted_package_and_a_swapped_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("node_modules/.package-lock.json"),
            r#"{"packages":{"":{},"node_modules/is-odd":{"version":"3.0.1"},"node_modules/is-number":{"version":"6.0.0"}}}"#,
        );
        write(
            &root.join("node_modules/is-odd/package.json"),
            r#"{"name":"is-odd","version":"3.0.1"}"#,
        );
        write(
            &root.join("node_modules/is-number/package.json"),
            r#"{"name":"is-number","version":"6.0.0"}"#,
        );

        let report = verify_node(root).unwrap();
        assert_eq!(report.checked, 2, "the root entry is not a dependency");
        assert!(report.is_clean(), "{:?}", report.findings);

        std::fs::remove_dir_all(root.join("node_modules/is-number")).unwrap();
        let report = verify_node(root).unwrap();
        assert_eq!(
            report.findings,
            vec![Finding {
                path: "node_modules/is-number".into(),
                kind: FindingKind::Missing
            }]
        );

        write(
            &root.join("node_modules/is-number/package.json"),
            r#"{"name":"is-number","version":"6.0.0"}"#,
        );
        std::fs::write(
            root.join("node_modules/is-odd/package.json"),
            r#"{"name":"is-odd","version":"9.9.9"}"#,
        )
        .unwrap();
        let report = verify_node(root).unwrap();
        assert_eq!(
            report.findings,
            vec![Finding {
                path: "node_modules/is-odd".into(),
                kind: FindingKind::VersionMismatch {
                    recorded: "3.0.1".into(),
                    actual: "9.9.9".into()
                }
            }]
        );
    }

    /// L1: the native lockfile's digest is compared against what was recorded.
    #[test]
    fn a_replaced_native_lock_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("uv.lock"), "version = 1\n").unwrap();
        let digest = super::super::file_sha256(&root.join("uv.lock")).unwrap();

        assert!(verify_native_lock(root, "uv.lock", &digest)
            .unwrap()
            .is_none());
        // Case must not matter: a digest read back from a file may be either.
        assert!(verify_native_lock(root, "uv.lock", &digest.to_uppercase())
            .unwrap()
            .is_none());

        std::fs::write(root.join("uv.lock"), "version = 2\n").unwrap();
        let finding = verify_native_lock(root, "uv.lock", &digest)
            .unwrap()
            .unwrap();
        assert!(matches!(finding.kind, FindingKind::DigestMismatch { .. }));
        // The wording has to say what happened. "is version <sha256>" was the
        // first attempt and reads as nonsense to whoever has to act on it.
        assert!(
            finding
                .describe()
                .contains("has changed since it was installed"),
            "{}",
            finding.describe()
        );

        std::fs::remove_file(root.join("uv.lock")).unwrap();
        assert!(matches!(
            verify_native_lock(root, "uv.lock", &digest)
                .unwrap()
                .unwrap()
                .kind,
            FindingKind::Missing
        ));
    }

    /// Receipt paths are `/`-separated wherever they were written, and both
    /// spellings resolve on every platform.
    ///
    /// Not `#[cfg(windows)]`-gated: a receipt written on one platform can be read
    /// on the other, and gating would declare that half untested -- which is
    /// where this class of bug lives.
    #[test]
    fn receipt_paths_resolve_for_either_separator() {
        assert_eq!(to_native("idna/core.py"), Path::new("idna").join("core.py"));
        assert_eq!(
            to_native("idna\\core.py"),
            Path::new("idna").join("core.py")
        );
        assert_eq!(
            to_native("./idna//core.py"),
            Path::new("idna").join("core.py")
        );
        assert_eq!(normalize("node_modules\\is-odd"), "node_modules/is-odd");
    }

    /// `site-packages` is located under either layout, on every platform.
    #[test]
    fn site_packages_is_found_under_either_layout() {
        let temp = tempfile::tempdir().unwrap();
        let unix = temp.path().join("unix");
        std::fs::create_dir_all(unix.join("lib/python3.12/site-packages")).unwrap();
        assert_eq!(
            find_site_packages(&unix).unwrap(),
            Some(unix.join("lib/python3.12/site-packages"))
        );

        let windows = temp.path().join("win");
        std::fs::create_dir_all(windows.join("Lib/site-packages")).unwrap();
        assert_eq!(
            find_site_packages(&windows).unwrap(),
            Some(windows.join("Lib").join("site-packages"))
        );

        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(find_site_packages(&empty).unwrap(), None);
    }

    /// A pnpm tree, built to the shape measured from pnpm 9.15.1.
    ///
    /// Written out by hand rather than by shelling out to pnpm, so the test runs
    /// offline and on every platform. The shape itself is not invented: the
    /// integrity below is the real sha512 of the bytes written, and the store path
    /// is derived the same way pnpm derives it -- which is exactly the mapping
    /// under test.
    struct PnpmFixture {
        root: PathBuf,
        /// The file every tampering test targets.
        target: PathBuf,
    }

    fn integrity_of(bytes: &[u8]) -> String {
        super::sha512_integrity(bytes)
    }

    fn build_pnpm_fixture(root: &Path) -> PnpmFixture {
        // Two packages, not one: a single package cannot expose a mix-up between
        // "this package's files" and "some package's files", and AGENTS.md is
        // explicit that N=1 does not surface that class of bug.
        let store = root.join("store");
        let virtual_store = root.join("node_modules").join(".pnpm");

        let mut lock = String::from("lockfileVersion: '9.0'\n\npackages:\n\n");
        let mut target = PathBuf::new();

        for (name, version, files) in [
            (
                "ms",
                "2.1.3",
                vec![
                    ("index.js", "module.exports = function ms() {}\n"),
                    ("package.json", "{\"name\":\"ms\",\"version\":\"2.1.3\"}\n"),
                ],
            ),
            (
                "is-odd",
                "3.0.1",
                vec![
                    ("index.js", "module.exports = n => n % 2 === 1;\n"),
                    (
                        "package.json",
                        "{\"name\":\"is-odd\",\"version\":\"3.0.1\"}\n",
                    ),
                ],
            ),
        ] {
            let dir_name = format!("{name}@{version}");
            let package_dir = virtual_store
                .join(&dir_name)
                .join("node_modules")
                .join(name);
            std::fs::create_dir_all(&package_dir).unwrap();

            let mut entries = Vec::new();
            for (file, contents) in &files {
                let path = package_dir.join(file);
                std::fs::write(&path, contents).unwrap();
                entries.push(format!(
                    "\"{file}\":{{\"integrity\":\"{}\",\"mode\":420,\"size\":{}}}",
                    integrity_of(contents.as_bytes()),
                    contents.len()
                ));
                if *file == "index.js" && name == "ms" {
                    target = path;
                }
            }

            // The tarball integrity is what addresses the store. Its value does not
            // have to be a real tarball digest for the test -- it has to be the
            // thing the lock says and the thing the store path is derived from,
            // which is the property being verified.
            let tarball = integrity_of(dir_name.as_bytes());
            let index = format!(
                "{{\"name\":\"{name}\",\"version\":\"{version}\",\"files\":{{{}}}}}",
                entries.join(",")
            );
            let index_path = super::pnpm_index_path(&store, &tarball).unwrap();
            std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
            std::fs::write(&index_path, index).unwrap();

            lock.push_str(&format!(
                "  {dir_name}:\n    resolution: {{integrity: {tarball}}}\n\n"
            ));
        }

        std::fs::write(root.join("pnpm-lock.yaml"), lock).unwrap();
        std::fs::write(
            root.join("node_modules").join(".modules.yaml"),
            format!(
                "nodeLinker: isolated\npackageManager: pnpm@9.15.1\nstoreDir: {}\n",
                store.display()
            ),
        )
        .unwrap();

        PnpmFixture {
            root: root.to_path_buf(),
            target,
        }
    }

    /// An untampered pnpm tree verifies clean, and checks a non-zero number of files.
    ///
    /// The `checked > 0` half is the load-bearing one: a report of "0 problems"
    /// that examined nothing is the vacuous pass this module exists to prevent,
    /// and it looks identical to success in the CLI output.
    #[test]
    fn an_untouched_pnpm_tree_verifies_clean_and_actually_checks_files() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        let report = verify_node(&fixture.root).unwrap();
        assert!(
            report.is_clean(),
            "an untouched tree must be clean: {:?}",
            report.findings
        );
        assert!(
            report.checked >= 4,
            "must examine every recorded file of both packages, examined {}",
            report.checked
        );
    }

    /// Rewriting a file's contents is caught.
    #[test]
    fn rewriting_an_installed_file_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        std::fs::write(&fixture.target, "module.exports = 'tampered';\n").unwrap();

        let report = verify_node(&fixture.root).unwrap();
        assert!(!report.is_clean(), "rewritten contents must be reported");
        assert!(
            report.findings.iter().any(|finding| matches!(
                finding.kind,
                FindingKind::SizeMismatch { .. } | FindingKind::DigestMismatch { .. }
            )),
            "expected a size or digest finding, got {:?}",
            report.findings
        );
    }

    /// A same-length rewrite must still be caught, which only the digest can do.
    ///
    /// Separate from the test above on purpose: if the size check were the only
    /// one, that test would still pass and the digest comparison could be deleted
    /// without any test noticing.
    #[test]
    fn a_same_size_rewrite_is_caught_by_the_digest() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        let original = std::fs::read(&fixture.target).unwrap();
        let mut tampered = original.clone();
        // Flip one byte, keeping the length identical.
        let last = tampered.len() - 2;
        tampered[last] ^= 0x20;
        std::fs::write(&fixture.target, &tampered).unwrap();
        assert_eq!(
            std::fs::metadata(&fixture.target).unwrap().len(),
            original.len() as u64,
            "the probe must keep the size identical, or it tests the wrong thing"
        );

        let report = verify_node(&fixture.root).unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|finding| matches!(finding.kind, FindingKind::DigestMismatch { .. })),
            "a same-size rewrite must produce a digest finding, got {:?}",
            report.findings
        );
    }

    /// Deleting a file the receipt lists is caught.
    #[test]
    fn deleting_an_installed_file_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        std::fs::remove_file(&fixture.target).unwrap();

        let report = verify_node(&fixture.root).unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == FindingKind::Missing),
            "a deleted file must be reported missing, got {:?}",
            report.findings
        );
    }

    /// With no store on this machine, say so -- do not report a clean tree.
    ///
    /// This is the case a checkout from another machine hits: `.modules.yaml`
    /// records an absolute `storeDir` that does not exist here. Nothing can be
    /// compared, and "0 problems" would be a lie.
    #[test]
    fn a_missing_store_is_reported_rather_than_passing() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());
        std::fs::remove_dir_all(temp.path().join("store")).unwrap();

        let report = verify_node(&fixture.root).unwrap();
        assert!(!report.is_clean(), "an absent store must not verify clean");
        assert_eq!(report.checked, 0);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == FindingKind::ReceiptMissing),
            "expected ReceiptMissing, got {:?}",
            report.findings
        );
    }

    /// A pnpm tree takes the pnpm path even when a stale npm receipt is present.
    ///
    /// Both can coexist -- an npm install followed by a pnpm one leaves the old
    /// `.package-lock.json` behind. Reading the stale file would verify a tree that
    /// is no longer what is installed, and would do it silently.
    #[test]
    fn a_stale_npm_receipt_does_not_win_over_a_live_pnpm_store() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());
        // A receipt claiming a package that does not exist: if this file were the
        // one consulted, the report would contain a Missing finding for it.
        std::fs::write(
            fixture.root.join("node_modules").join(".package-lock.json"),
            r#"{"packages":{"node_modules/ghost":{"version":"9.9.9"}}}"#,
        )
        .unwrap();

        let report = verify_node(&fixture.root).unwrap();
        assert!(
            report.is_clean(),
            "the pnpm store is authoritative here: {:?}",
            report.findings
        );
        assert!(report.checked >= 4);
    }

    /// The store address is derived from the integrity, not searched for.
    ///
    /// Pinned separately because it is the one piece of the chain that comes from
    /// measurement rather than documentation: hex-decode the base64 digest, take
    /// the first byte as the subdirectory. Measured against pnpm 9.15.1 with
    /// ms@2.1.3.
    #[test]
    fn the_store_index_path_is_derived_from_the_tarball_integrity() {
        let store = Path::new("/store");
        let path = super::pnpm_index_path(
            store,
            "sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA==",
        )
        .expect("a well-formed sha512 integrity must map to a path");
        let shown = normalize(&path.to_string_lossy());
        assert!(
            shown.ends_with(
                "files/e8/5973b9b4cb646dc9d9afcd542025784863ceae68c601f268253dc985ef70bb2fa1568726afece715c8ebf5d73fab73ed1f7100eb479d23bfb57b45dd645394-index.json"
            ),
            "derived path does not match what pnpm 9.15.1 wrote: {shown}"
        );
    }

    /// A malformed integrity yields no path rather than a panic or a wrong one.
    #[test]
    fn a_malformed_integrity_produces_no_store_path() {
        let store = Path::new("/store");
        assert!(super::pnpm_index_path(store, "sha1-abc").is_none());
        assert!(super::pnpm_index_path(store, "sha512-not base64!!").is_none());
        assert!(super::pnpm_index_path(store, "").is_none());
    }

    /// Lockfile parsing stops at the end of the packages section.
    ///
    /// The first version of this test used a real `snapshots:` section and could
    /// not fail: pnpm writes those entries as `ms@2.1.3: {}`, whose key does not
    /// end in a bare `:` and carries no `resolution:` line, so overrunning the
    /// boundary contributes nothing either way.
    ///
    /// This version makes the boundary decide the outcome -- a later top-level
    /// section that does contain a parseable entry. Removing the `break` makes it
    /// fail.
    #[test]
    fn lock_parsing_stops_at_the_end_of_the_packages_section() {
        let lock = "\
lockfileVersion: '9.0'

packages:

  ms@2.1.3:
    resolution: {integrity: sha512-AAAA}

patchedDependencies:

  ghost@1.0.0:
    resolution: {integrity: sha512-BBBB}
";
        let parsed = super::pnpm_lock_integrities(lock);
        assert_eq!(
            parsed.get("ms@2.1.3").map(String::as_str),
            Some("sha512-AAAA")
        );
        assert!(
            !parsed.contains_key("ghost@1.0.0"),
            "entries after the packages section must not be collected: {parsed:?}"
        );
    }

    /// The size check short-circuits; it is not a second line of defence.
    ///
    /// Mutating the size comparison away left every tampering test green, because
    /// the digest catches the same cases. That is not a gap -- it is what the size
    /// check is for: skipping a sha512 over a file whose length already disagrees.
    /// Asserting that it *catches* tampering would therefore be asserting something
    /// the digest guarantees anyway.
    ///
    /// What is actually specific to it is the shape of the finding: a
    /// length-changing edit reports the sizes, which tells the reader how far off
    /// the file is, instead of two opaque digests.
    #[test]
    fn a_length_changing_edit_reports_sizes_rather_than_digests() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        std::fs::write(&fixture.target, "x").unwrap();

        let report = verify_node(&fixture.root).unwrap();
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.path.ends_with("ms/index.js"))
            .expect("the edited file must be reported");
        match &finding.kind {
            FindingKind::SizeMismatch { actual, .. } => assert_eq!(*actual, 1),
            other => panic!("expected a size finding for a length change, got {other:?}"),
        }
    }

    /// A present store that yields no readable index must not verify clean.
    ///
    /// Distinct from the absent-store case, which returns early from
    /// `pnpm_store_dir`. Here the store directory exists, so the walk runs and
    /// finds nothing comparable -- and mutation showed the absent-store test does
    /// not cover this path at all: deleting the fail-closed guard left it green.
    #[test]
    fn a_store_without_usable_indexes_is_reported_rather_than_passing() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = build_pnpm_fixture(temp.path());

        // Keep the store directory, remove only what makes it readable.
        std::fs::remove_dir_all(temp.path().join("store").join("files")).unwrap();
        assert!(
            temp.path().join("store").is_dir(),
            "the store itself must still be present, or this tests the other path"
        );

        let report = verify_node(&fixture.root).unwrap();
        assert_eq!(
            report.checked, 0,
            "nothing was comparable, so nothing may be counted as checked"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == FindingKind::ReceiptMissing),
            "expected ReceiptMissing, got {:?}",
            report.findings
        );
    }
}
