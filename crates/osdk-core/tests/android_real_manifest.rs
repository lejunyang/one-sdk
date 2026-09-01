//! Parses the real Android repository manifest that was captured from
//! `dl.google.com`, so the hand-written parser is validated against actual
//! upstream bytes rather than only a hand-built fixture.
//!
//! The manifest is not vendored into the repo (it is ~400 KB and is Google's
//! content), so these tests skip when the capture is absent. Set
//! `OSDK_ANDROID_MANIFEST` to a downloaded `repository2-4.xml` to run them.

use osdk_core::android::repo::{self, Channel};

/// Load the manifest capture, or return `None` when it is not available.
fn manifest_xml() -> Option<String> {
    let path = std::env::var_os("OSDK_ANDROID_MANIFEST")?;
    std::fs::read_to_string(path).ok()
}

#[test]
fn parses_the_real_google_manifest() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: set OSDK_ANDROID_MANIFEST to a repository2-N.xml capture");
        return;
    };
    let manifest = repo::parse_manifest(&xml).expect("real manifest parses");

    // Observed in repository2-4.xml (2026-09): 313 packages, 2 licenses.
    assert!(
        manifest.packages.len() > 250,
        "expected the full inventory, got {}",
        manifest.packages.len()
    );
    assert!(
        manifest.licenses.contains_key("android-sdk-license"),
        "the standard SDK license must be present"
    );

    // Every package must carry a usable path and at least one archive.
    for package in &manifest.packages {
        assert!(!package.path.is_empty());
        assert!(
            !package.archives.is_empty(),
            "{} has no archives",
            package.path
        );
    }
}

#[test]
fn real_manifest_exposes_platform_tools_with_a_sha1_archive() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: OSDK_ANDROID_MANIFEST not set");
        return;
    };
    let manifest = repo::parse_manifest(&xml).unwrap();
    let package = manifest
        .package("platform-tools")
        .expect("platform-tools is always published");

    assert_eq!(package.channel, Channel::Stable);
    assert_eq!(
        package.license_ref.as_deref(),
        Some("android-sdk-license"),
        "platform-tools is license-gated"
    );

    // A Windows archive with a well-formed 40-hex SHA-1 and a real size.
    let platform = osdk_core::platform::Platform {
        os: osdk_core::platform::Os::Windows,
        arch: osdk_core::platform::Arch::X64,
        libc: osdk_core::platform::Libc::Glibc,
    };
    let archive = package
        .archive_for(&platform)
        .expect("a windows archive exists");
    assert!(archive.url.ends_with(".zip"), "url = {}", archive.url);
    assert_eq!(
        archive.checksum.len(),
        40,
        "manifest publishes sha1 only: {}",
        archive.checksum
    );
    assert!(archive.checksum.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(archive.size > 1_000_000, "size = {}", archive.size);
}

#[test]
fn real_manifest_ndk_family_sorts_numerically() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: OSDK_ANDROID_MANIFEST not set");
        return;
    };
    let manifest = repo::parse_manifest(&xml).unwrap();
    let ndks = manifest.family("ndk");
    assert!(
        ndks.len() > 20,
        "expected many NDK packages, got {}",
        ndks.len()
    );

    // The manifest is not version-ordered, so the sorted tail must be the
    // numerically highest revision -- not whatever appears last in the file.
    let highest = ndks.last().unwrap();
    for package in &ndks {
        assert!(
            repo::compare_versions(&package.version(), &highest.version())
                != std::cmp::Ordering::Greater,
            "{} sorted below {} but is numerically greater",
            package.path,
            highest.path
        );
    }

    // The newest NDK is a preview, so a stable-only filter must pick a
    // different, lower package.
    let newest_stable = ndks
        .iter()
        .rfind(|p| p.channel == Channel::Stable)
        .expect("stable NDKs exist");
    assert!(
        repo::compare_versions(&newest_stable.version(), &highest.version())
            != std::cmp::Ordering::Greater
    );
}

#[test]
fn real_manifest_licenses_hash_to_forty_hex_chars() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: OSDK_ANDROID_MANIFEST not set");
        return;
    };
    let manifest = repo::parse_manifest(&xml).unwrap();
    for (id, license) in &manifest.licenses {
        assert!(
            license.text.len() > 1_000,
            "{id} text looks truncated ({} chars)",
            license.text.len()
        );
        let hash = license.hash();
        assert_eq!(hash.len(), 40, "{id}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }

    // The hashes that circulate in CI recipes are snapshots of older agreement
    // texts. If one of them ever matches again it means Google reverted the
    // wording, and the live-computation rule still holds -- but a hardcoded
    // table would have been wrong in between.
    if let Some(license) = manifest.licenses.get("android-sdk-license") {
        assert_ne!(
            license.hash(),
            "24333f8a63b6825ea9c5514f83c2829b004d1fee",
            "historical hash unexpectedly matches; do not hardcode it regardless"
        );
    }
}

/// The preview license contains three non-ASCII characters (curly quotes and
/// an apostrophe), so its byte length and character length differ by 6. The
/// acceptance hash is taken over BYTES; measuring characters instead would
/// produce a different digest and a license file the Android tooling rejects.
#[test]
fn license_hash_is_taken_over_bytes_not_characters() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: OSDK_ANDROID_MANIFEST not set");
        return;
    };
    let manifest = repo::parse_manifest(&xml).unwrap();
    let preview = manifest
        .licenses
        .get("android-sdk-preview-license")
        .expect("preview license present");

    let chars = preview.text.chars().count();
    let bytes = preview.text.len();
    assert!(
        bytes > chars,
        "expected multi-byte characters in the preview license (bytes={bytes}, chars={chars})"
    );

    // The hash must equal the digest of the UTF-8 bytes.
    let over_bytes = osdk_core::pipeline::verify::hash_bytes(
        preview.text.as_bytes(),
        osdk_core::pipeline::HashAlgo::Sha1,
    );
    assert_eq!(preview.hash(), over_bytes);
}

#[test]
fn real_manifest_dependencies_stay_rare_and_resolvable() {
    let Some(xml) = manifest_xml() else {
        eprintln!("skipping: OSDK_ANDROID_MANIFEST not set");
        return;
    };
    let manifest = repo::parse_manifest(&xml).unwrap();
    let with_deps: Vec<_> = manifest
        .packages
        .iter()
        .filter(|p| !p.dependencies.is_empty())
        .collect();

    // Only a handful of packages declare dependencies (4 of 313 in 2026-09), so
    // a trivial resolver suffices. A large jump here means the assumption broke.
    assert!(
        with_deps.len() < 25,
        "dependency count jumped to {}; revisit the resolver",
        with_deps.len()
    );

    // Every declared dependency must name a package that actually exists.
    for package in with_deps {
        for dependency in &package.dependencies {
            assert!(
                manifest.package(dependency).is_some()
                    || manifest
                        .packages
                        .iter()
                        .any(|p| p.family() == dependency.as_str()),
                "{} depends on unknown {}",
                package.path,
                dependency
            );
        }
    }
}
