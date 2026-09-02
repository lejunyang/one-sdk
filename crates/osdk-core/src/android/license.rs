//! License acceptance for Android SDK packages.
//!
//! Most Android packages are gated behind an agreement that the user must
//! accept before use. Acceptance is recorded the way the official tooling
//! records it: a file at `<sdk-root>/licenses/<license-id>` containing the
//! SHA-1 of the agreement text. Writing those files makes the acceptance
//! visible to Gradle and the Android Gradle Plugin, so a project build does not
//! prompt again.
//!
//! ## Two rules this module exists to enforce
//!
//! 1. **Hashes are computed live, never hardcoded.** The digests circulated in
//!    CI recipes (`24333f8a…`, `84831b94…`) are snapshots of *older* agreement
//!    texts. Verified 2026-09: none of them match the current manifest's
//!    licenses under any plausible normalisation. A hardcoded table would
//!    silently write wrong files the next time Google edits the wording, so the
//!    hash always comes from the license text in the manifest at hand.
//!
//! 2. **osdk never accepts on the user's behalf.** The agreement requires the
//!    user to accept before use, so acceptance must be an explicit act:
//!    `-o accept-licenses=true`, `-o accept-license=<id>`, or a recorded prior
//!    acceptance. There is no implicit or default-on path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::repo::{License, Manifest, RemotePackage};

/// Directory under an SDK root where acceptances are recorded.
pub const LICENSES_DIR: &str = "licenses";

/// What the caller authorised on this invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Acceptance {
    /// Nothing authorised. Any gated package must fail with instructions.
    #[default]
    None,
    /// Only these license ids are authorised.
    Ids(BTreeSet<String>),
    /// Every license required by the requested packages is authorised.
    All,
}

impl Acceptance {
    /// Build from CLI inputs: `-o accept-licenses=true` (all) and
    /// `-o accept-license=<id>` (comma-separated ids).
    pub fn from_flags(accept_all: bool, ids: &[String]) -> Acceptance {
        if accept_all {
            return Acceptance::All;
        }
        if ids.is_empty() {
            return Acceptance::None;
        }
        Acceptance::Ids(ids.iter().map(|id| id.trim().to_string()).collect())
    }

    /// Whether `license_id` is authorised by this acceptance.
    pub fn covers(&self, license_id: &str) -> bool {
        match self {
            Acceptance::None => false,
            Acceptance::All => true,
            Acceptance::Ids(ids) => ids.contains(license_id),
        }
    }
}

/// Path of the acceptance record for `license_id` under `sdk_root`.
pub fn record_path(sdk_root: &Path, license_id: &str) -> PathBuf {
    sdk_root.join(LICENSES_DIR).join(license_id)
}

/// Whether a prior acceptance of `license` is already recorded under
/// `sdk_root`.
///
/// The record must contain the hash of the *current* agreement text; a record
/// left over from an older wording does not count, matching the official
/// tooling's behaviour of re-prompting when the text changes.
pub fn is_recorded(sdk_root: &Path, license: &License) -> bool {
    let path = record_path(sdk_root, &license.id);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let expected = license.hash();
    contents
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case(&expected))
}

/// Record acceptance of `license` under `sdk_root`.
///
/// Writes the hash of the current agreement text, appending to any existing
/// record so that hashes of previously accepted wordings are preserved (the
/// official tooling tolerates multiple lines).
pub fn record(sdk_root: &Path, license: &License) -> Result<PathBuf> {
    let path = record_path(sdk_root, &license.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let hash = license.hash();
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .map(|body| {
            body.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !lines.iter().any(|line| line.eq_ignore_ascii_case(&hash)) {
        lines.push(hash);
    }
    let body = format!("{}\n", lines.join("\n"));
    std::fs::write(&path, body).map_err(|e| Error::io(&path, e))?;
    Ok(path)
}

/// A license that a requested package needs but the caller has not authorised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLicense {
    pub license_id: String,
    /// Packages in this request that require it.
    pub packages: Vec<String>,
}

/// Decide what still needs accepting for `packages`.
///
/// Returns the licenses that are neither covered by `acceptance` nor already
/// recorded under `sdk_root`. An empty result means installation may proceed.
pub fn pending(
    manifest: &Manifest,
    packages: &[&RemotePackage],
    acceptance: &Acceptance,
    sdk_root: &Path,
) -> Vec<PendingLicense> {
    let mut blocked: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for package in packages {
        let Some(license) = manifest.license_for(package) else {
            continue;
        };
        if acceptance.covers(&license.id) || is_recorded(sdk_root, license) {
            continue;
        }
        blocked
            .entry(license.id.clone())
            .or_default()
            .push(package.path.clone());
    }
    blocked
        .into_iter()
        .map(|(license_id, packages)| PendingLicense {
            license_id,
            packages,
        })
        .collect()
}

/// Persist acceptance for every license required by `packages` that
/// `acceptance` authorises.
///
/// Returns the license ids actually recorded. Licenses already on disk are not
/// rewritten.
pub fn record_accepted(
    manifest: &Manifest,
    packages: &[&RemotePackage],
    acceptance: &Acceptance,
    sdk_root: &Path,
) -> Result<Vec<String>> {
    let mut recorded = BTreeSet::new();
    for package in packages {
        let Some(license) = manifest.license_for(package) else {
            continue;
        };
        if !acceptance.covers(&license.id) {
            continue;
        }
        if is_recorded(sdk_root, license) {
            continue;
        }
        record(sdk_root, license)?;
        recorded.insert(license.id.clone());
    }
    Ok(recorded.into_iter().collect())
}

/// The error shown when a gated package is requested without acceptance.
///
/// Names the exact flags, so the message doubles as the instructions.
pub fn blocked_error(pending: &[PendingLicense]) -> Error {
    let mut lines =
        vec!["these Android packages require accepting a license agreement first:".to_string()];
    for entry in pending {
        lines.push(format!(
            "  {} (required by: {})",
            entry.license_id,
            entry.packages.join(", ")
        ));
    }
    lines.push(String::new());
    lines.push("review the full text with:".into());
    lines.push("  osdk android licenses show <package>".into());
    lines.push("then accept explicitly with either:".into());
    lines.push("  -o accept-licenses=true            (all licenses this request needs)".into());
    for entry in pending {
        lines.push(format!(
            "  -o accept-license={}  (just this one)",
            entry.license_id
        ));
    }
    Error::other(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::android::repo::{Archive, Channel};

    fn license(id: &str, text: &str) -> License {
        License {
            id: id.to_string(),
            text: text.to_string(),
        }
    }

    fn package(path: &str, license_ref: Option<&str>) -> RemotePackage {
        RemotePackage {
            path: path.to_string(),
            display_name: path.to_string(),
            revision: "1.0.0".into(),
            license_ref: license_ref.map(str::to_string),
            channel: Channel::Stable,
            dependencies: Vec::new(),
            archives: vec![Archive {
                url: "a.zip".into(),
                size: 1,
                checksum: "ab".into(),
                host_os: None,
                host_bits: None,
            }],
            obsolete: false,
        }
    }

    fn manifest(packages: Vec<RemotePackage>, licenses: Vec<License>) -> Manifest {
        Manifest {
            packages,
            licenses: licenses.into_iter().map(|l| (l.id.clone(), l)).collect(),
        }
    }

    #[test]
    fn nothing_is_accepted_by_default() {
        let acceptance = Acceptance::from_flags(false, &[]);
        assert_eq!(acceptance, Acceptance::None);
        assert!(!acceptance.covers("android-sdk-license"));
    }

    #[test]
    fn accept_all_flag_covers_every_license() {
        let acceptance = Acceptance::from_flags(true, &[]);
        assert!(acceptance.covers("android-sdk-license"));
        assert!(acceptance.covers("android-sdk-preview-license"));
    }

    #[test]
    fn per_id_acceptance_does_not_leak_to_other_licenses() {
        let acceptance = Acceptance::from_flags(false, &["android-sdk-license".to_string()]);
        assert!(acceptance.covers("android-sdk-license"));
        // A stable-license approval must not silently authorise preview terms.
        assert!(!acceptance.covers("android-sdk-preview-license"));
    }

    #[test]
    fn gated_package_is_blocked_without_acceptance() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(
            vec![package("ndk;29.0.1", Some("android-sdk-license"))],
            vec![license("android-sdk-license", "terms")],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        let blocked = pending(&m, &packages, &Acceptance::None, root.path());
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].license_id, "android-sdk-license");
        assert_eq!(blocked[0].packages, vec!["ndk;29.0.1".to_string()]);
    }

    #[test]
    fn accepting_unblocks_and_records_the_live_hash() {
        let root = tempfile::tempdir().unwrap();
        let text = "terms and conditions";
        let m = manifest(
            vec![package("ndk;29.0.1", Some("android-sdk-license"))],
            vec![license("android-sdk-license", text)],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        let acceptance = Acceptance::All;
        assert!(pending(&m, &packages, &acceptance, root.path()).is_empty());

        let recorded = record_accepted(&m, &packages, &acceptance, root.path()).unwrap();
        assert_eq!(recorded, vec!["android-sdk-license".to_string()]);

        // The file holds the SHA-1 of the exact text, computed live.
        let expected =
            crate::pipeline::verify::hash_bytes(text.as_bytes(), crate::pipeline::HashAlgo::Sha1);
        let body =
            std::fs::read_to_string(record_path(root.path(), "android-sdk-license")).unwrap();
        assert_eq!(body.trim(), expected);
    }

    #[test]
    fn recorded_acceptance_survives_a_later_run_without_flags() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(
            vec![package("ndk;29.0.1", Some("android-sdk-license"))],
            vec![license("android-sdk-license", "terms")],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        record_accepted(&m, &packages, &Acceptance::All, root.path()).unwrap();
        // No flags this time, but the record stands.
        assert!(pending(&m, &packages, &Acceptance::None, root.path()).is_empty());
    }

    #[test]
    fn a_stale_record_from_older_terms_does_not_count() {
        let root = tempfile::tempdir().unwrap();
        let path = record_path(root.path(), "android-sdk-license");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // The widely-circulated historical hash, which no longer matches.
        std::fs::write(&path, "24333f8a63b6825ea9c5514f83c2829b004d1fee\n").unwrap();
        let current = license("android-sdk-license", "current terms");
        assert!(!is_recorded(root.path(), &current));

        let m = manifest(
            vec![package("ndk;29.0.1", Some("android-sdk-license"))],
            vec![current],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        assert_eq!(
            pending(&m, &packages, &Acceptance::None, root.path()).len(),
            1
        );
    }

    #[test]
    fn recording_appends_and_preserves_prior_hashes() {
        let root = tempfile::tempdir().unwrap();
        let path = record_path(root.path(), "android-sdk-license");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "0000000000000000000000000000000000000000\n").unwrap();
        let current = license("android-sdk-license", "current terms");
        record(root.path(), &current).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("0000000000000000000000000000000000000000"));
        assert!(body.contains(&current.hash()));
        assert!(is_recorded(root.path(), &current));
    }

    #[test]
    fn recording_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let l = license("android-sdk-license", "terms");
        record(root.path(), &l).unwrap();
        record(root.path(), &l).unwrap();
        let body = std::fs::read_to_string(record_path(root.path(), &l.id)).unwrap();
        assert_eq!(body.lines().filter(|line| !line.is_empty()).count(), 1);
    }

    #[test]
    fn multiple_packages_group_under_their_shared_license() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(
            vec![
                package("ndk;29.0.1", Some("android-sdk-license")),
                package("build-tools;36.0.0", Some("android-sdk-license")),
                package("ndk;30.0.1", Some("android-sdk-preview-license")),
            ],
            vec![
                license("android-sdk-license", "terms"),
                license("android-sdk-preview-license", "preview terms"),
            ],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        let blocked = pending(&m, &packages, &Acceptance::None, root.path());
        assert_eq!(blocked.len(), 2);
        let stable = blocked
            .iter()
            .find(|b| b.license_id == "android-sdk-license")
            .unwrap();
        assert_eq!(stable.packages.len(), 2);
    }

    #[test]
    fn ungated_package_needs_no_acceptance() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(vec![package("cmake;1.0", None)], vec![]);
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        assert!(pending(&m, &packages, &Acceptance::None, root.path()).is_empty());
    }

    #[test]
    fn partial_acceptance_blocks_only_the_unapproved_license() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(
            vec![
                package("ndk;29.0.1", Some("android-sdk-license")),
                package("ndk;30.0.1", Some("android-sdk-preview-license")),
            ],
            vec![
                license("android-sdk-license", "terms"),
                license("android-sdk-preview-license", "preview terms"),
            ],
        );
        let packages: Vec<&RemotePackage> = m.packages.iter().collect();
        let acceptance = Acceptance::from_flags(false, &["android-sdk-license".to_string()]);
        let blocked = pending(&m, &packages, &acceptance, root.path());
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].license_id, "android-sdk-preview-license");

        // Only the approved one gets written.
        let recorded = record_accepted(&m, &packages, &acceptance, root.path()).unwrap();
        assert_eq!(recorded, vec!["android-sdk-license".to_string()]);
        assert!(!record_path(root.path(), "android-sdk-preview-license").exists());
    }

    #[test]
    fn blocked_error_names_the_flags_and_packages() {
        let message = blocked_error(&[PendingLicense {
            license_id: "android-sdk-license".into(),
            packages: vec!["ndk;29.0.1".into()],
        }])
        .to_string();
        assert!(message.contains("-o accept-licenses=true"));
        assert!(message.contains("-o accept-license=android-sdk-license"));
        assert!(message.contains("ndk;29.0.1"));
        assert!(message.contains("osdk android licenses show"));
    }
}
