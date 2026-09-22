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
//! * Node: `node_modules/.package-lock.json`, which carries a per-package
//!   `version` and `integrity`.
//!
//! osdk reads those rather than keeping a second dependency graph of its own.

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

/// Verify a Node environment against `node_modules/.package-lock.json`.
///
/// That file is npm's own record of what it placed where, so it stays correct
/// without osdk maintaining a parallel graph. pnpm and yarn write different
/// receipts; when the expected one is absent this returns `ReceiptMissing`
/// rather than an empty clean report, because "could not check" must not look
/// like "checked and fine".
pub fn verify_node(project_root: &Path) -> Result<Report> {
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
}
