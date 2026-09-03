//! Writes the `package.xml` index Google's own tools read.
//!
//! ## Why osdk has to write this at all
//!
//! osdk records what it installed in its own metadata, but Google's tools do not
//! ask a manager what is available -- they walk the SDK root and parse a
//! `package.xml` inside each package directory. A package installed by osdk is
//! therefore invisible to them, however correct the directory layout is.
//!
//! Measured, not inferred. With the file absent, `avdmanager create avd` reports
//!
//! ```text
//! Error: Package path is not valid. Valid system image paths are:
//! ```
//!
//! and lists nothing, even though the image is present and `sdkmanager
//! --list_installed` shows it -- sdkmanager is satisfied by `source.properties`,
//! avdmanager is not. Writing this file is what moves avdmanager on to its next
//! check.
//!
//! ## Fidelity
//!
//! The schema is versioned per package type and the shapes here were copied from
//! a file `sdkmanager` itself wrote, not composed from documentation. Only the
//! two shapes osdk's curated families actually use are emitted:
//! `genericDetailsType` for tools, and `sysImgDetailsType` for system images,
//! which carries the api level, tag, vendor and abi that avdmanager matches an
//! AVD against.
//!
//! Everything written comes from `source.properties`, which ships inside the
//! archive. Nothing is guessed: a field that is absent is omitted rather than
//! defaulted, because a wrong api level or abi would make avdmanager offer an
//! AVD that cannot boot.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::Result;

/// The file Google's tools look for inside a package directory.
pub const PACKAGE_XML: &str = "package.xml";

/// The metadata file shipped inside every Android SDK archive.
pub const SOURCE_PROPERTIES: &str = "source.properties";

/// Parsed `source.properties` contents.
///
/// A plain key/value map: the format is `key=value` per line with `#` comments,
/// and no section headers.
pub fn read_source_properties(dir: &Path) -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(dir.join(SOURCE_PROPERTIES)).ok()?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Some(fields)
}

/// Escape text for inclusion in an XML text node or attribute value.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render `<revision>` from a dotted revision string.
///
/// The schema wants separate `major`/`minor`/`micro` elements rather than the
/// dotted form, and omits the trailing ones when they are absent -- a revision
/// of `37` must not become `37.0.0`, because the tools compare these numerically
/// against a dependency's `min-revision`.
fn revision_element(revision: &str) -> String {
    let mut out = String::from("<revision>");
    for (index, part) in revision.split('.').take(3).enumerate() {
        let number: u32 = part.trim().parse().unwrap_or(0);
        let name = match index {
            0 => "major",
            1 => "minor",
            _ => "micro",
        };
        out.push_str(&format!("<{name}>{number}</{name}>"));
    }
    out.push_str("</revision>");
    out
}

/// The `type-details` element for a package.
///
/// System images are the only curated family with a non-generic shape: their
/// details carry the fields avdmanager matches on, so a generic element would
/// index the package but leave it unusable for `create avd`.
fn type_details(manifest_path: &str, fields: &BTreeMap<String, String>) -> String {
    let generic = "<type-details xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
                   xsi:type=\"ns5:genericDetailsType\"/>";
    if !manifest_path.starts_with("system-images;") {
        return generic.to_string();
    }
    // Without an api level and abi the element would be malformed, and a
    // malformed one is worse than a generic one: the tools reject the whole
    // package rather than skipping the details.
    let (Some(api), Some(abi)) = (
        fields.get("AndroidVersion.ApiLevel"),
        fields.get("SystemImage.Abi"),
    ) else {
        return generic.to_string();
    };
    let mut out = String::from(
        "<type-details xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xsi:type=\"ns6:sysImgDetailsType\">",
    );
    out.push_str(&format!("<api-level>{}</api-level>", escape(api)));
    if let Some(extension) = fields.get("AndroidVersion.ExtensionLevel") {
        out.push_str(&format!(
            "<extension-level>{}</extension-level>",
            escape(extension)
        ));
    }
    if let Some(base) = fields.get("AndroidVersion.IsBaseSdk") {
        out.push_str(&format!(
            "<base-extension>{}</base-extension>",
            escape(&base.to_ascii_lowercase())
        ));
    }
    if let Some(tag) = fields.get("SystemImage.TagId") {
        let display = fields
            .get("SystemImage.TagDisplay")
            .cloned()
            .unwrap_or_else(|| tag.clone());
        out.push_str(&format!(
            "<tag><id>{}</id><display>{}</display></tag>",
            escape(tag),
            escape(&display)
        ));
    }
    // The vendor is spelled `Addon.VendorId` in the shipped properties but
    // `SystemImage.VendorId` in some older images; accept either.
    let vendor = fields
        .get("SystemImage.VendorId")
        .or_else(|| fields.get("Addon.VendorId"));
    if let Some(vendor) = vendor {
        let display = fields
            .get("SystemImage.VendorDisplay")
            .or_else(|| fields.get("Addon.VendorDisplay"))
            .cloned()
            .unwrap_or_else(|| vendor.clone());
        out.push_str(&format!(
            "<vendor><id>{}</id><display>{}</display></vendor>",
            escape(vendor),
            escape(&display)
        ));
    }
    out.push_str(&format!("<abi>{}</abi>", escape(abi)));
    out.push_str("</type-details>");
    out
}

/// Render the whole `package.xml` document for one installed package.
///
/// `manifest_path` is the id Google's tools know the package by (`platform-tools`,
/// `system-images;android-35;google_apis;x86_64`), which is what they match
/// against; it is not the on-disk path.
pub fn render(
    manifest_path: &str,
    display_name: &str,
    revision: &str,
    license_id: Option<&str>,
    fields: &BTreeMap<String, String>,
) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    // Namespace prefixes match the sample sdkmanager writes. Both the generic
    // and sys-img namespaces are declared up front so `type_details` can pick
    // either without the caller knowing which.
    out.push_str(
        "<ns2:repository \
         xmlns:ns2=\"http://schemas.android.com/repository/android/common/02\" \
         xmlns:ns5=\"http://schemas.android.com/repository/android/generic/02\" \
         xmlns:ns6=\"http://schemas.android.com/sdk/android/repo/sys-img2/03\">",
    );
    // The agreement text itself is not duplicated here: osdk records consent in
    // the SDK root's `licenses/` directory, which is the file Gradle and the
    // command line tools actually read. Only the reference is needed for the
    // package to be well formed.
    if let Some(id) = license_id {
        out.push_str(&format!("<license id=\"{}\" type=\"text\"/>", escape(id)));
    }
    out.push_str(&format!(
        "<localPackage path=\"{}\" obsolete=\"false\">",
        escape(manifest_path)
    ));
    out.push_str(&type_details(manifest_path, fields));
    out.push_str(&revision_element(revision));
    out.push_str(&format!(
        "<display-name>{}</display-name>",
        escape(display_name)
    ));
    if let Some(id) = license_id {
        out.push_str(&format!("<uses-license ref=\"{}\"/>", escape(id)));
    }
    out.push_str("</localPackage></ns2:repository>");
    out
}

/// Write `package.xml` into an installed package directory.
///
/// Returns `Ok(false)` when the directory does not exist or carries no
/// `source.properties` to describe it. A missing index only costs interop with
/// Google's tools, so this never fails an install: refusing to install a
/// perfectly good NDK because an index could not be written would be worse than
/// the interop gap it prevents.
pub fn write_into(
    dir: &Path,
    manifest_path: &str,
    display_name: &str,
    revision: &str,
    license_id: Option<&str>,
) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let Some(fields) = read_source_properties(dir) else {
        return Ok(false);
    };
    // Prefer the revision the archive states over the one the caller passed:
    // for families whose osdk version is not the revision (a system image is
    // addressed by api/tag/abi but revisioned separately) they differ, and the
    // tools compare the revision against dependency minimums.
    let revision = fields
        .get("Pkg.Revision")
        .map(String::as_str)
        .unwrap_or(revision);
    let document = render(manifest_path, display_name, revision, license_id, &fields);
    let path = dir.join(PACKAGE_XML);
    std::fs::write(&path, document.as_bytes())
        .map_err(|error| crate::error::Error::io(&path, error))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn revision_keeps_the_component_count_it_was_given() {
        // `37` must not become `37.0.0`: the tools compare these against a
        // dependency's min-revision, so inventing components changes meaning.
        assert_eq!(
            revision_element("37"),
            "<revision><major>37</major></revision>"
        );
        assert_eq!(
            revision_element("37.0.1"),
            "<revision><major>37</major><minor>0</minor><micro>1</micro></revision>"
        );
        // A four-part NDK revision is truncated to what the schema allows.
        assert_eq!(
            revision_element("29.0.14206865.1"),
            "<revision><major>29</major><minor>0</minor><micro>14206865</micro></revision>"
        );
    }

    #[test]
    fn tools_get_generic_details_and_images_get_the_sys_img_shape() {
        let empty = props(&[]);
        assert!(type_details("platform-tools", &empty).contains("genericDetailsType"));
        // The system-image shape needs api level and abi; without them a generic
        // element is emitted rather than a malformed sys-img one, because the
        // tools reject the whole package on a malformed details element.
        assert!(
            type_details("system-images;android-35;google_apis;x86_64", &empty)
                .contains("genericDetailsType")
        );
        let full = props(&[
            ("AndroidVersion.ApiLevel", "35"),
            ("SystemImage.Abi", "x86_64"),
            ("SystemImage.TagId", "google_apis"),
            ("SystemImage.TagDisplay", "Google APIs"),
            ("Addon.VendorId", "google"),
            ("Addon.VendorDisplay", "Google Inc."),
        ]);
        let details = type_details("system-images;android-35;google_apis;x86_64", &full);
        assert!(details.contains("sysImgDetailsType"));
        assert!(details.contains("<api-level>35</api-level>"));
        assert!(details.contains("<abi>x86_64</abi>"));
        // avdmanager matches an AVD on tag and vendor, so both must survive.
        assert!(details.contains("<id>google_apis</id>"));
        assert!(details.contains("<display>Google APIs</display>"));
        assert!(details.contains("<id>google</id>"));
    }

    #[test]
    fn the_vendor_is_read_from_either_spelling() {
        // Shipped images use `Addon.VendorId`; some older ones use the
        // `SystemImage.` prefix. Missing the vendor entirely would make
        // avdmanager treat the image as a different variant.
        let addon = props(&[
            ("AndroidVersion.ApiLevel", "35"),
            ("SystemImage.Abi", "x86_64"),
            ("Addon.VendorId", "google"),
        ]);
        assert!(type_details("system-images;a;b;c", &addon).contains("<id>google</id>"));
        let sysimg = props(&[
            ("AndroidVersion.ApiLevel", "35"),
            ("SystemImage.Abi", "x86_64"),
            ("SystemImage.VendorId", "google"),
        ]);
        assert!(type_details("system-images;a;b;c", &sysimg).contains("<id>google</id>"));
    }

    #[test]
    fn rendered_document_is_well_formed_and_carries_the_manifest_path() {
        let fields = props(&[("Pkg.Revision", "37.0.1")]);
        let xml = render(
            "platform-tools",
            "Android SDK Platform-Tools",
            "37.0.1",
            Some("android-sdk-license"),
            &fields,
        );
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.ends_with("</localPackage></ns2:repository>"));
        // The path is the id Google's tools match on, not a filesystem path.
        assert!(xml.contains("<localPackage path=\"platform-tools\" obsolete=\"false\">"));
        assert!(xml.contains("<uses-license ref=\"android-sdk-license\"/>"));
        // Tags must balance, or the tools skip the package silently.
        assert_eq!(xml.matches("<localPackage").count(), 1);
        assert_eq!(xml.matches("</localPackage>").count(), 1);
    }

    #[test]
    fn text_is_escaped_so_a_display_name_cannot_break_the_document() {
        let xml = render(
            "build-tools;37.0.0",
            "Tools & <friends>",
            "37",
            None,
            &props(&[]),
        );
        assert!(xml.contains("Tools &amp; &lt;friends&gt;"));
        assert!(!xml.contains("<friends>"));
        // With no license the reference is omitted rather than left empty.
        assert!(!xml.contains("uses-license"));
    }

    #[test]
    fn source_properties_parsing_ignores_comments_and_blank_lines() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(SOURCE_PROPERTIES),
            "# a comment\n\nPkg.Revision=9\nSystemImage.Abi = x86_64 \n",
        )
        .unwrap();
        let fields = read_source_properties(temp.path()).unwrap();
        assert_eq!(fields.get("Pkg.Revision").unwrap(), "9");
        // Surrounding whitespace is not part of the value.
        assert_eq!(fields.get("SystemImage.Abi").unwrap(), "x86_64");
        assert!(!fields.contains_key("# a comment"));
    }

    #[test]
    fn writing_is_skipped_rather_than_failing_when_there_is_nothing_to_describe() {
        let temp = tempfile::tempdir().unwrap();
        // No such directory: not an error, just nothing to index.
        assert!(!write_into(
            &temp.path().join("absent"),
            "platform-tools",
            "Platform Tools",
            "37",
            None
        )
        .unwrap());
        // Directory with no source.properties: same.
        let bare = temp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(!write_into(&bare, "platform-tools", "Platform Tools", "37", None).unwrap());
        assert!(!bare.join(PACKAGE_XML).exists());
    }

    #[test]
    fn the_archives_own_revision_wins_over_the_osdk_version() {
        // A system image is addressed by api/tag/abi but revisioned separately,
        // so the version osdk uses as its id is not the revision the tools
        // compare against a dependency minimum.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(SOURCE_PROPERTIES),
            "Pkg.Revision=9\nAndroidVersion.ApiLevel=35\nSystemImage.Abi=x86_64\n",
        )
        .unwrap();
        assert!(write_into(
            temp.path(),
            "system-images;android-35;google_apis;x86_64",
            "Google APIs Intel x86_64 Atom System Image",
            "android-35;google_apis;x86_64",
            Some("android-sdk-license"),
        )
        .unwrap());
        let written = std::fs::read_to_string(temp.path().join(PACKAGE_XML)).unwrap();
        assert!(written.contains("<revision><major>9</major></revision>"));
        // The osdk version string must not leak into the revision.
        assert!(!written.contains("<major>0</major>"));
    }
}
