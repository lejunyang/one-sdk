//! Android SDK repository protocol: manifest discovery, parsing, and package
//! selection.
//!
//! Google publishes the SDK inventory as a versioned XML manifest at
//! `https://dl.google.com/android/repository/repository2-<N>.xml`. This module
//! discovers the newest generation, parses the `<remotePackage>` entries into a
//! flat inventory, and picks the archive matching the host platform.
//!
//! ## Why a hand-written parser
//!
//! The manifest uses several XSD generations whose namespaces change with `N`,
//! and only a small, stable subset of each document matters here: package path,
//! revision, display name, license reference, channel, dependencies, and the
//! per-archive `url`/`size`/`checksum` plus host-OS filter. A focused
//! pull-parser over that subset avoids taking a schema-bound XML dependency
//! that would need updating on every manifest generation bump.
//!
//! ## Integrity
//!
//! Archives carry a **SHA-1** checksum and nothing stronger. That is weaker
//! than every other osdk source, so it is represented as an explicit,
//! per-source exception ([`ANDROID_CHECKSUM_ALGO`]) rather than by loosening
//! the shared checksum policy.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::pipeline::{Checksum, HashAlgo};
use crate::platform::{Arch, Os, Platform};

/// Canonical Google repository root.
pub const GOOGLE_REPO_ROOT: &str = "https://dl.google.com/android/repository/";

/// The only digest the Android manifests publish for their archives.
///
/// Accepted as a deliberate per-source exception: the upstream offers no
/// stronger alternative. See the module docs.
pub const ANDROID_CHECKSUM_ALGO: HashAlgo = HashAlgo::Sha1;

/// Manifest generations to probe, newest first.
///
/// Google keeps older generations online indefinitely, so probing downward from
/// a bound above the newest known generation both finds the current one and
/// degrades gracefully when a new one appears. Verified 2026-09: `2-4` is the
/// newest; `2-5` and above return 404.
pub const MANIFEST_GENERATIONS: &[u32] = &[8, 7, 6, 5, 4, 3, 2, 1];

/// Build the manifest URL for a generation under `root`.
pub fn manifest_url(root: &str, generation: u32) -> String {
    crate::http::join_url(root, &format!("repository2-{generation}.xml"))
}

/// A distribution channel. Ordered stable-first so `<=` expresses "at most as
/// unstable as".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Channel {
    #[default]
    Stable,
    Beta,
    Dev,
    Canary,
}

impl Channel {
    /// Parse a `channel-N` id as used by `<channelRef ref="channel-0"/>`.
    pub fn from_ref(value: &str) -> Option<Channel> {
        match value.trim() {
            "channel-0" => Some(Channel::Stable),
            "channel-1" => Some(Channel::Beta),
            "channel-2" => Some(Channel::Dev),
            "channel-3" => Some(Channel::Canary),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Dev => "dev",
            Channel::Canary => "canary",
        }
    }
}

/// One downloadable archive of a package, already filtered to a host OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// Archive file name relative to the repository root.
    pub url: String,
    /// Declared size in bytes.
    pub size: u64,
    /// SHA-1 hex digest as published in the manifest.
    pub checksum: String,
    /// Host OS this archive targets; `None` means "any host".
    pub host_os: Option<String>,
    /// Host bit-width constraint, when the manifest declares one.
    pub host_bits: Option<String>,
}

impl Archive {
    /// The archive's checksum as a pipeline [`Checksum`].
    pub fn pipeline_checksum(&self) -> Checksum {
        Checksum {
            algo: ANDROID_CHECKSUM_ALGO,
            hex: self.checksum.clone(),
        }
    }
}

/// A package as published in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePackage {
    /// Full manifest path, e.g. `ndk;29.0.14206865` or `platform-tools`.
    pub path: String,
    /// Human-readable name.
    pub display_name: String,
    /// Revision string built from `<major>.<minor>.<micro>`, when present.
    pub revision: String,
    /// License id this package requires, e.g. `android-sdk-license`.
    pub license_ref: Option<String>,
    /// Channel the package is published on.
    pub channel: Channel,
    /// Other package paths this package depends on.
    pub dependencies: Vec<String>,
    /// All archives declared for the package, across host platforms.
    pub archives: Vec<Archive>,
    /// True when the manifest marks the package obsolete.
    pub obsolete: bool,
}

impl RemotePackage {
    /// The leading path segment, used as the package family (`ndk`,
    /// `platform-tools`, `build-tools`, ...).
    pub fn family(&self) -> &str {
        self.path.split(';').next().unwrap_or(&self.path)
    }

    /// The version-bearing tail of the path, if any. For `ndk;29.0.1` this is
    /// `29.0.1`; for `platform-tools` there is none and the revision is used.
    pub fn version_tail(&self) -> Option<&str> {
        let mut parts = self.path.splitn(2, ';');
        let _ = parts.next();
        parts.next().filter(|tail| !tail.is_empty())
    }

    /// The version osdk should present for this package.
    pub fn version(&self) -> String {
        self.version_tail()
            .map(str::to_string)
            .unwrap_or_else(|| self.revision.clone())
    }

    /// Pick the archive matching `platform`, preferring an exact host-OS match
    /// over a host-agnostic one.
    pub fn archive_for(&self, platform: &Platform) -> Option<&Archive> {
        let token = host_os_token(platform.os);
        let bits = host_bits_token(platform.arch);
        let matches = |archive: &&Archive| match archive.host_os.as_deref() {
            Some(os) => os.eq_ignore_ascii_case(token),
            None => true,
        };
        let bits_ok = |archive: &&Archive| match (&archive.host_bits, bits) {
            (Some(declared), Some(actual)) => declared == actual,
            // An unconstrained archive fits any width.
            (None, _) => true,
            // The manifest constrains width but the host has no token: reject
            // rather than guess.
            (Some(_), None) => false,
        };
        // Exact host match first, then host-agnostic archives.
        self.archives
            .iter()
            .find(|a| a.host_os.is_some() && matches(a) && bits_ok(a))
            .or_else(|| self.archives.iter().find(|a| a.host_os.is_none()))
    }
}

/// A license as embedded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    pub id: String,
    /// Full agreement text, exactly as published.
    pub text: String,
}

impl License {
    /// The SHA-1 of the license text, computed live from `text`.
    ///
    /// This is the value the Android tooling writes into
    /// `<sdk-root>/licenses/<id>`. It **must** be computed from the manifest
    /// currently in hand: the hashes circulated in CI recipes are snapshots of
    /// older agreement texts and stop matching when Google edits the wording.
    pub fn hash(&self) -> String {
        crate::pipeline::verify::hash_bytes(self.text.as_bytes(), HashAlgo::Sha1)
    }
}

/// A parsed manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub packages: Vec<RemotePackage>,
    pub licenses: BTreeMap<String, License>,
}

impl Manifest {
    /// All packages in a family, newest revision last.
    pub fn family(&self, family: &str) -> Vec<&RemotePackage> {
        let mut found: Vec<&RemotePackage> = self
            .packages
            .iter()
            .filter(|p| p.family() == family)
            .collect();
        // The manifest is NOT ordered by version, so sort explicitly.
        found.sort_by(|a, b| {
            compare_versions(&a.version(), &b.version()).then_with(|| a.path.cmp(&b.path))
        });
        found
    }

    /// Look up a package by its exact manifest path.
    pub fn package(&self, path: &str) -> Option<&RemotePackage> {
        self.packages.iter().find(|p| p.path == path)
    }

    /// The license a package requires, resolved against this manifest.
    pub fn license_for(&self, package: &RemotePackage) -> Option<&License> {
        package
            .license_ref
            .as_deref()
            .and_then(|id| self.licenses.get(id))
    }
}

/// Compare two dotted revision strings numerically segment by segment.
///
/// Android revisions such as `29.0.14206865` exceed what a lexical sort orders
/// correctly, and the manifest does not pre-sort them.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // A trailing `-<tag>` marks a pre-release (`1.0-rc1`). It must be split
    // off before the dotted comparison, otherwise the extra segment makes the
    // pre-release look *newer* than the release it precedes.
    let (a_release, a_pre) = split_prerelease(a);
    let (b_release, b_pre) = split_prerelease(b);

    match compare_release(a_release, b_release) {
        Ordering::Equal => {}
        other => return other,
    }

    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        // No pre-release tag is the finished release, which is the newer of
        // the two.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    }
}

/// Split `1.0-rc1` into (`1.0`, Some(`rc1`)).
fn split_prerelease(version: &str) -> (&str, Option<&str>) {
    match version.split_once('-') {
        Some((release, pre)) => (release, Some(pre)),
        None => (version, None),
    }
}

/// Compare two dotted release strings numerically, treating absent trailing
/// segments as zero so `1.2` and `1.2.0` are equal.
fn compare_release(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            // A missing segment counts as zero.
            (Some(x), None) => match numeric(x) {
                Some(0) => continue,
                _ => return Ordering::Greater,
            },
            (None, Some(y)) => match numeric(y) {
                Some(0) => continue,
                _ => return Ordering::Less,
            },
            (Some(x), Some(y)) => match (numeric(x), numeric(y)) {
                (Some(xn), Some(yn)) if xn != yn => return xn.cmp(&yn),
                (Some(_), Some(_)) => continue,
                // Mixed or non-numeric segments fall back to a lexical
                // comparison rather than guessing.
                _ => match x.cmp(y) {
                    Ordering::Equal => continue,
                    other => return other,
                },
            },
        }
    }
}

fn numeric(segment: &str) -> Option<u64> {
    segment.parse::<u64>().ok()
}

/// The `hostOs` token Google uses for a platform.
pub fn host_os_token(os: Os) -> &'static str {
    match os {
        Os::Windows => "windows",
        Os::Macos => "macosx",
        Os::Linux => "linux",
    }
}

/// The `hostBits` token for an architecture, when the manifest constrains it.
pub fn host_bits_token(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::X86 | Arch::Arm => Some("32"),
        Arch::X64 | Arch::Arm64 => Some("64"),
    }
}

/// Parse a repository manifest.
pub fn parse_manifest(xml: &str) -> Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut cursor = 0usize;
    while let Some(start) = find_element(xml, "remotePackage", cursor) {
        let (body, end) = match element_body(xml, start, "remotePackage") {
            Some(found) => found,
            None => break,
        };
        if let Some(package) = parse_remote_package(xml, start, body) {
            manifest.packages.push(package);
        }
        cursor = end;
    }
    let mut cursor = 0usize;
    while let Some(start) = find_element(xml, "license", cursor) {
        let (body, end) = match element_body(xml, start, "license") {
            Some(found) => found,
            None => break,
        };
        if let Some(id) = attribute(xml, start, "id") {
            let text = decode_entities(body);
            manifest.licenses.insert(id.clone(), License { id, text });
        }
        cursor = end;
    }
    if manifest.packages.is_empty() {
        return Err(Error::other(
            "Android repository manifest contained no <remotePackage> entries",
        ));
    }
    Ok(manifest)
}

fn parse_remote_package(xml: &str, start: usize, body: &str) -> Option<RemotePackage> {
    let path = attribute(xml, start, "path")?;
    let display_name = first_text(body, "display-name").unwrap_or_else(|| path.clone());
    let revision = parse_revision(body);
    let license_ref = find_element(body, "uses-license", 0)
        .and_then(|at| attribute(body, at, "ref"))
        .or_else(|| find_element(body, "uses-license", 0).and_then(|at| attribute(body, at, "id")));
    let channel = find_element(body, "channelRef", 0)
        .and_then(|at| attribute(body, at, "ref"))
        .and_then(|value| Channel::from_ref(&value))
        .unwrap_or_default();
    let dependencies = parse_dependencies(body);
    let archives = parse_archives(body);
    let obsolete = first_text(body, "obsolete")
        .map(|value| value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    Some(RemotePackage {
        path,
        display_name,
        revision,
        license_ref,
        channel,
        dependencies,
        archives,
        obsolete,
    })
}

fn parse_revision(body: &str) -> String {
    let Some(start) = find_element(body, "revision", 0) else {
        return String::new();
    };
    let Some((revision_body, _)) = element_body(body, start, "revision") else {
        return String::new();
    };
    let mut parts = Vec::new();
    for field in ["major", "minor", "micro"] {
        match first_text(revision_body, field) {
            Some(value) if !value.trim().is_empty() => parts.push(value.trim().to_string()),
            // Stop at the first absent field so `1.2` does not become `1.2.0`.
            _ => break,
        }
    }
    parts.join(".")
}

fn parse_dependencies(body: &str) -> Vec<String> {
    let Some(start) = find_element(body, "dependencies", 0) else {
        return Vec::new();
    };
    let Some((deps_body, _)) = element_body(body, start, "dependencies") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(at) = find_element(deps_body, "dependency", cursor) {
        if let Some(path) = attribute(deps_body, at, "path") {
            out.push(path);
        }
        cursor = at + "dependency".len();
    }
    out
}

fn parse_archives(body: &str) -> Vec<Archive> {
    let Some(start) = find_element(body, "archives", 0) else {
        return Vec::new();
    };
    let Some((archives_body, _)) = element_body(body, start, "archives") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(at) = find_element(archives_body, "archive", cursor) {
        let Some((archive_body, end)) = element_body(archives_body, at, "archive") else {
            break;
        };
        cursor = end;
        // `complete` holds the actual download; `patches` are ignored.
        let complete = find_element(archive_body, "complete", 0)
            .and_then(|at| element_body(archive_body, at, "complete"))
            .map(|(body, _)| body)
            .unwrap_or(archive_body);
        let Some(url) = first_text(complete, "url") else {
            continue;
        };
        let checksum = first_text(complete, "checksum").unwrap_or_default();
        let size = first_text(complete, "size")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        out.push(Archive {
            url: url.trim().to_string(),
            size,
            checksum: checksum.trim().to_ascii_lowercase(),
            host_os: first_text(archive_body, "host-os").map(|v| v.trim().to_string()),
            host_bits: first_text(archive_body, "host-bits").map(|v| v.trim().to_string()),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Minimal XML scanning helpers
//
// These operate on the manifest subset described in the module docs. They are
// namespace-agnostic by design: element names are matched on their local name
// so a namespace-prefix change across manifest generations does not break
// parsing.
// ---------------------------------------------------------------------------

/// Find the byte offset of the start tag for local `name` at or after `from`.
fn find_element(xml: &str, name: &str, from: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut cursor = from;
    while cursor < xml.len() {
        let at = xml.get(cursor..)?.find('<')? + cursor;
        let rest = xml.get(at + 1..)?;
        // Skip closing tags, comments, declarations.
        let trimmed = rest.trim_start_matches('/');
        let local = local_name(trimmed);
        if local == name && !rest.starts_with('/') {
            // Confirm the char after the name delimits the tag, so that
            // `<licenseX>` does not match a search for `license`.
            let name_end = at + 1 + prefix_len(trimmed) + name.len();
            match bytes.get(name_end) {
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/') => {
                    return Some(at)
                }
                _ => {}
            }
        }
        cursor = at + 1;
    }
    None
}

/// Whether `c` may appear in an XML element or prefix name.
///
/// Deliberately narrow: the manifests only use ASCII names. Keeping this
/// strict is what stops running prose (which contains `:`) from being read as
/// a namespaced tag name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// The name token at the start of `tail`, i.e. everything up to the first
/// character that cannot appear in a name.
fn name_token(tail: &str) -> &str {
    let end = tail.find(|c: char| !is_name_char(c)).unwrap_or(tail.len());
    &tail[..end]
}

/// Length of a namespace prefix (`ns:`) at the start of `tail`.
///
/// Only a leading run of valid name characters followed immediately by `:`
/// counts. A `:` further along (as in prose, or in an attribute value) is not
/// a prefix separator.
fn prefix_len(tail: &str) -> usize {
    let token = name_token(tail);
    if token.is_empty() {
        return 0;
    }
    if tail.as_bytes().get(token.len()) == Some(&b':') {
        token.len() + 1
    } else {
        0
    }
}

/// The local (namespace-stripped) element name starting `tail`.
fn local_name(tail: &str) -> &str {
    name_token(&tail[prefix_len(tail)..])
}

/// Given the offset of a start tag, return its inner body and the offset just
/// past its end tag. Handles nesting of same-named elements.
fn element_body<'x>(xml: &'x str, start: usize, name: &str) -> Option<(&'x str, usize)> {
    let tag_end = xml.get(start..)?.find('>')? + start;
    // Self-closing: empty body.
    if xml.as_bytes().get(tag_end.wrapping_sub(1)) == Some(&b'/') {
        return Some(("", tag_end + 1));
    }
    let body_start = tag_end + 1;
    let mut depth = 1usize;
    let mut cursor = body_start;
    while depth > 0 {
        let at = xml.get(cursor..)?.find('<')? + cursor;
        let rest = xml.get(at + 1..)?;
        if let Some(after_slash) = rest.strip_prefix('/') {
            if local_name(after_slash) == name {
                depth -= 1;
                if depth == 0 {
                    let close = xml.get(at..)?.find('>')? + at;
                    return Some((xml.get(body_start..at)?, close + 1));
                }
            }
        } else if local_name(rest) == name {
            // Ignore self-closing occurrences; they open no scope.
            let tag_close = xml.get(at..)?.find('>')? + at;
            if xml.as_bytes().get(tag_close - 1) != Some(&b'/') {
                depth += 1;
            }
        }
        cursor = at + 1;
    }
    None
}

/// Read attribute `key` from the start tag at `start`.
fn attribute(xml: &str, start: usize, key: &str) -> Option<String> {
    let tag_end = xml.get(start..)?.find('>')? + start;
    let tag = xml.get(start..tag_end)?;
    let mut cursor = 0usize;
    while let Some(found) = tag.get(cursor..)?.find(key) {
        let at = cursor + found;
        let before_ok = at == 0
            || tag
                .as_bytes()
                .get(at - 1)
                .is_some_and(|b| b.is_ascii_whitespace());
        let after = tag.get(at + key.len()..).unwrap_or("");
        let after_trimmed = after.trim_start();
        if before_ok && after_trimmed.starts_with('=') {
            let value = after_trimmed.trim_start_matches('=').trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let rest = &value[1..];
                let end = rest.find(quote)?;
                return Some(decode_entities(&rest[..end]));
            }
        }
        cursor = at + key.len();
    }
    None
}

/// Text content of the first `name` child anywhere within `body`.
fn first_text(body: &str, name: &str) -> Option<String> {
    let at = find_element(body, name, 0)?;
    let (inner, _) = element_body(body, at, name)?;
    Some(decode_entities(inner))
}

/// Expand the XML entities that appear in these manifests.
fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Ampersand last so the expansions above are not re-processed.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Libc;

    fn test_platform(os: Os) -> Platform {
        Platform {
            os,
            arch: Arch::X64,
            libc: Libc::Glibc,
        }
    }

    /// Shape mirrors the real `repository2-4.xml`, including the namespace
    /// prefix and the `<complete>` wrapper.
    const SAMPLE: &str = r#"<?xml version="1.0" ?>
<ns2:sdk-repository xmlns:ns2="http://schemas.android.com/sdk/android/repo/repository2/04">
  <license id="android-sdk-license" type="text">Terms and Conditions text</license>
  <license id="android-sdk-preview-license" type="text">Preview terms</license>
  <remotePackage path="platform-tools">
    <!--Generated from bid:1234-->
    <type-details xsi:type="ns5:genericDetailsType"/>
    <revision><major>37</major><minor>0</minor><micro>1</micro></revision>
    <display-name>Android SDK Platform-Tools</display-name>
    <uses-license ref="android-sdk-license"/>
    <channelRef ref="channel-0"/>
    <archives>
      <archive>
        <complete>
          <size>8044989</size>
          <checksum type="sha1">E03E78B1D80B396F1C3358E31251CB31740E1110</checksum>
          <url>platform-tools_r37.0.1-win.zip</url>
        </complete>
        <host-os>windows</host-os>
      </archive>
      <archive>
        <complete>
          <size>7000000</size>
          <checksum type="sha1">aaaa1111</checksum>
          <url>platform-tools_r37.0.1-linux.zip</url>
        </complete>
        <host-os>linux</host-os>
      </archive>
    </archives>
  </remotePackage>
  <remotePackage path="ndk;29.0.14206865">
    <revision><major>29</major><minor>0</minor><micro>14206865</micro></revision>
    <display-name>NDK (Side by side) 29.0.14206865</display-name>
    <uses-license ref="android-sdk-license"/>
    <channelRef ref="channel-0"/>
    <archives>
      <archive>
        <complete>
          <size>728000000</size>
          <checksum type="sha1">bbbb2222</checksum>
          <url>android-ndk-r29-windows.zip</url>
        </complete>
        <host-os>windows</host-os>
      </archive>
    </archives>
  </remotePackage>
  <remotePackage path="ndk;30.0.16138531">
    <revision><major>30</major><minor>0</minor><micro>16138531</micro></revision>
    <display-name>NDK (Side by side) 30.0.16138531</display-name>
    <uses-license ref="android-sdk-preview-license"/>
    <channelRef ref="channel-1"/>
    <archives>
      <archive>
        <complete>
          <size>728000001</size>
          <checksum type="sha1">cccc3333</checksum>
          <url>android-ndk-r30-beta3-windows.zip</url>
        </complete>
        <host-os>windows</host-os>
      </archive>
    </archives>
  </remotePackage>
  <remotePackage path="emulators;kvm">
    <revision><major>36</major></revision>
    <display-name>Emulator support</display-name>
    <uses-license ref="android-sdk-license"/>
    <channelRef ref="channel-0"/>
    <dependencies>
      <dependency path="emulator">
        <min-revision><major>36</major></min-revision>
      </dependency>
    </dependencies>
    <archives>
      <archive>
        <complete>
          <size>1000</size>
          <checksum type="sha1">dddd4444</checksum>
          <url>kvm.zip</url>
        </complete>
      </archive>
    </archives>
  </remotePackage>
</ns2:sdk-repository>
"#;

    fn manifest() -> Manifest {
        parse_manifest(SAMPLE).expect("sample manifest parses")
    }

    #[test]
    fn parses_packages_licenses_and_namespaced_root() {
        let m = manifest();
        assert_eq!(m.packages.len(), 4);
        assert_eq!(m.licenses.len(), 2);
        assert_eq!(
            m.licenses["android-sdk-license"].text,
            "Terms and Conditions text"
        );
    }

    #[test]
    fn platform_tools_revision_and_display_name() {
        let m = manifest();
        let p = m.package("platform-tools").expect("platform-tools present");
        assert_eq!(p.revision, "37.0.1");
        assert_eq!(p.display_name, "Android SDK Platform-Tools");
        assert_eq!(p.family(), "platform-tools");
        // No `;` tail, so the version falls back to the revision.
        assert_eq!(p.version(), "37.0.1");
        assert_eq!(p.license_ref.as_deref(), Some("android-sdk-license"));
        assert_eq!(p.channel, Channel::Stable);
        assert!(!p.obsolete);
    }

    #[test]
    fn version_comes_from_path_tail_when_present() {
        let m = manifest();
        let ndk = m.package("ndk;29.0.14206865").unwrap();
        assert_eq!(ndk.family(), "ndk");
        assert_eq!(ndk.version(), "29.0.14206865");
    }

    #[test]
    fn picks_archive_for_host_os() {
        let m = manifest();
        let p = m.package("platform-tools").unwrap();
        let win = test_platform(Os::Windows);
        let archive = p.archive_for(&win).expect("windows archive");
        assert_eq!(archive.url, "platform-tools_r37.0.1-win.zip");
        assert_eq!(archive.size, 8_044_989);
        // Checksums are normalised to lowercase hex for comparison.
        assert_eq!(archive.checksum, "e03e78b1d80b396f1c3358e31251cb31740e1110");

        let linux = test_platform(Os::Linux);
        assert_eq!(
            p.archive_for(&linux).unwrap().url,
            "platform-tools_r37.0.1-linux.zip"
        );
    }

    #[test]
    fn host_agnostic_archive_matches_any_platform() {
        let m = manifest();
        let kvm = m.package("emulators;kvm").unwrap();
        for os in [Os::Windows, Os::Linux, Os::Macos] {
            let platform = test_platform(os);
            assert_eq!(kvm.archive_for(&platform).unwrap().url, "kvm.zip");
        }
    }

    #[test]
    fn parses_the_only_dependency_shape_the_manifest_uses() {
        let m = manifest();
        let kvm = m.package("emulators;kvm").unwrap();
        assert_eq!(kvm.dependencies, vec!["emulator".to_string()]);
        // Packages without a <dependencies> block report none.
        assert!(m.package("platform-tools").unwrap().dependencies.is_empty());
    }

    #[test]
    fn family_listing_sorts_numerically_not_lexically() {
        let m = manifest();
        let ndks = m.family("ndk");
        assert_eq!(ndks.len(), 2);
        // 30.x must sort above 29.x even though "3" < "2" lexically is false;
        // the real trap is 9 vs 14206865 style segments.
        assert_eq!(ndks.last().unwrap().path, "ndk;30.0.16138531");
    }

    #[test]
    fn numeric_version_ordering_beats_lexical() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("29.0.9", "29.0.14206865"), Ordering::Less);
        assert_eq!(
            compare_versions("30.0.1", "29.0.14206865"),
            Ordering::Greater
        );
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.1", "1.2"), Ordering::Greater);
        // Pre-release sorts below the plain release it precedes.
        assert_eq!(compare_versions("1.0-rc1", "1.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0", "1.0-rc1"), Ordering::Greater);
        assert_eq!(compare_versions("1.0-rc1", "1.0-rc2"), Ordering::Less);
        assert_eq!(compare_versions("1.0-rc1", "1.0-rc1"), Ordering::Equal);
        // Trailing zeros are not significant.
        assert_eq!(compare_versions("1.2.0.0", "1.2"), Ordering::Equal);
        // A real pair from the manifest: r30 beta vs the newest stable r29.
        assert_eq!(
            compare_versions("29.0.14206865", "30.0.16138531"),
            Ordering::Less
        );
    }

    #[test]
    fn channel_and_license_distinguish_preview_packages() {
        let m = manifest();
        let preview = m.package("ndk;30.0.16138531").unwrap();
        assert_eq!(preview.channel, Channel::Beta);
        assert_eq!(
            preview.license_ref.as_deref(),
            Some("android-sdk-preview-license")
        );
        // Stable is ordered below beta so `<=` filters work.
        assert!(Channel::Stable < Channel::Beta);
        assert!(m.license_for(preview).is_some());
    }

    #[test]
    fn license_hash_is_computed_live_from_text() {
        let license = License {
            id: "android-sdk-license".into(),
            text: "abc".into(),
        };
        // sha1("abc")
        assert_eq!(license.hash(), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn manifest_url_is_built_per_generation() {
        assert_eq!(
            manifest_url(GOOGLE_REPO_ROOT, 4),
            "https://dl.google.com/android/repository/repository2-4.xml"
        );
        // Newest generation is probed first.
        assert_eq!(MANIFEST_GENERATIONS.first().copied(), Some(8));
        assert!(MANIFEST_GENERATIONS.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn channel_refs_map_to_named_channels() {
        assert_eq!(Channel::from_ref("channel-0"), Some(Channel::Stable));
        assert_eq!(Channel::from_ref("channel-3"), Some(Channel::Canary));
        assert_eq!(Channel::from_ref("channel-9"), None);
        assert_eq!(Channel::Beta.as_str(), "beta");
    }

    #[test]
    fn entities_are_decoded_in_text_and_attributes() {
        let xml = r#"<sdk>
          <license id="a&amp;b">x &lt; y &amp;&amp; z &quot;q&quot;</license>
          <remotePackage path="p"><revision><major>1</major></revision>
          <archives><archive><complete><url>u.zip</url><checksum>ab</checksum><size>1</size></complete></archive></archives>
          </remotePackage></sdk>"#;
        let m = parse_manifest(xml).unwrap();
        assert_eq!(m.licenses["a&b"].text, r#"x < y && z "q""#);
    }

    #[test]
    fn empty_or_packageless_manifest_is_an_error() {
        assert!(parse_manifest("<sdk></sdk>").is_err());
        assert!(parse_manifest("").is_err());
    }

    #[test]
    fn sha1_is_flagged_as_not_collision_resistant() {
        assert!(!ANDROID_CHECKSUM_ALGO.is_collision_resistant());
        assert!(HashAlgo::Sha256.is_collision_resistant());
        assert_eq!(ANDROID_CHECKSUM_ALGO.token(), "sha1");
    }

    #[test]
    fn archive_exposes_a_pipeline_checksum() {
        let m = manifest();
        let p = m.package("platform-tools").unwrap();
        let win = test_platform(Os::Windows);
        let checksum = p.archive_for(&win).unwrap().pipeline_checksum();
        assert_eq!(checksum.algo, HashAlgo::Sha1);
        assert_eq!(checksum.hex.len(), 40);
    }

    #[test]
    fn host_tokens_match_google_vocabulary() {
        assert_eq!(host_os_token(Os::Windows), "windows");
        assert_eq!(host_os_token(Os::Macos), "macosx");
        assert_eq!(host_os_token(Os::Linux), "linux");
        assert_eq!(host_bits_token(Arch::X64), Some("64"));
        assert_eq!(host_bits_token(Arch::X86), Some("32"));
    }
}
