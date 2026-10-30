//! Choosing a GitHub release asset for a platform from its file name alone.
//!
//! Names are split into tokens rather than searched for substrings: `win` is a
//! substring of `darwin`, `x86` of `x86_64` and `arm` of `arm64`, and each of
//! those once picked a binary for the wrong platform. Candidates are then
//! ranked on an explicit key, and a tie between genuinely different assets is
//! reported instead of being settled by the order the listing happened to use.

use crate::pipeline::ArchiveKind;
use crate::platform::{Arch, Libc, Os, Platform};

/// The platform an asset has to run on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetTarget {
    pub platform: Platform,
    /// `arch=` was given explicitly, so an emulated architecture is not an
    /// acceptable substitute.
    pub arch_pinned: bool,
    /// `libc=` was given explicitly, so a Linux asset naming the other libc is
    /// rejected rather than ranked lower.
    pub libc_pinned: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AssetChoice {
    Selected(usize),
    NoMatch,
    /// The best candidates rank equally and are not the same build in
    /// different archive formats.
    Ambiguous(Vec<String>),
}

/// Pick the asset in `names` that best fits `target`.
pub(crate) fn choose_asset(names: &[&str], target: &AssetTarget) -> AssetChoice {
    let traits: Vec<Option<Traits>> = names.iter().map(|name| Traits::parse(name)).collect();
    // A release that labels any asset with an OS labels its platform builds;
    // an unlabelled file beside them is a script, a source bundle or docs.
    let release_names_os = traits
        .iter()
        .flatten()
        .any(|traits| !traits.oses.is_empty());
    let mut ranked: Vec<(Rank, usize)> = traits
        .iter()
        .enumerate()
        .filter_map(|(index, traits)| {
            let traits = traits.as_ref()?;
            if release_names_os && traits.oses.is_empty() {
                return None;
            }
            Some((traits.rank(target)?, index))
        })
        .collect();
    let Some(best) = ranked.iter().map(|(rank, _)| *rank).max() else {
        return AssetChoice::NoMatch;
    };
    ranked.retain(|(rank, _)| *rank == best);
    // The same build offered as both `.tar.gz` and `.zip` is a real tie with
    // an arbitrary but stable answer; anything else needs the user to choose.
    ranked.sort_by_key(|(_, index)| names[*index]);
    let stem = build_stem(names[ranked[0].1]);
    if ranked
        .iter()
        .all(|(_, index)| build_stem(names[*index]) == stem)
    {
        AssetChoice::Selected(ranked[0].1)
    } else {
        AssetChoice::Ambiguous(
            ranked
                .into_iter()
                .map(|(_, index)| names[index].to_string())
                .collect(),
        )
    }
}

/// Ordering key; fields are compared in declaration order, higher is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    os: u8,
    arch: u8,
    libc: u8,
    release_build: bool,
    archive: bool,
}

#[derive(Debug, Default)]
struct Traits {
    oses: Vec<Os>,
    arches: Vec<Arch>,
    universal: bool,
    libc: Option<Libc>,
    msvc: bool,
    mingw: bool,
    debug: bool,
    archive: bool,
}

impl Traits {
    /// `None` for files that are never the platform binary: checksums,
    /// signatures, metadata, OS packages, sources, foreign platforms, and
    /// compressed formats osdk would otherwise install as a raw executable.
    fn parse(name: &str) -> Option<Traits> {
        let lower = name.to_ascii_lowercase();
        let archive = ArchiveKind::from_name(&lower).is_ok();
        if !archive
            && NON_BINARY_SUFFIXES
                .iter()
                .any(|suffix| lower.ends_with(suffix))
        {
            return None;
        }
        let mut traits = Traits {
            archive,
            ..Traits::default()
        };
        let mut all = false;
        let mut source = false;
        for token in tokens(&lower) {
            match token.as_str() {
                "linux" => traits.os(Os::Linux),
                "linux64" => {
                    traits.os(Os::Linux);
                    traits.arch(Arch::X64);
                }
                "linux32" => {
                    traits.os(Os::Linux);
                    traits.arch(Arch::X86);
                }
                "darwin" | "macos" | "macosx" | "osx" | "mac" | "apple" => traits.os(Os::Macos),
                "windows" | "win" | "win32" | "exe" => traits.os(Os::Windows),
                "win64" => {
                    traits.os(Os::Windows);
                    traits.arch(Arch::X64);
                }
                "msvc" => {
                    traits.msvc = true;
                    traits.os(Os::Windows);
                }
                "mingw" | "mingw32" | "mingw64" => {
                    traits.mingw = true;
                    traits.os(Os::Windows);
                }
                "x86_64" | "x64" | "amd64" | "x8664" | "64bit" => traits.arch(Arch::X64),
                "aarch64" | "arm64" | "armv8" => traits.arch(Arch::Arm64),
                "x86" | "i386" | "i486" | "i586" | "i686" | "386" | "ia32" | "32bit" => {
                    traits.arch(Arch::X86)
                }
                "arm" | "armhf" | "armel" => traits.arch(Arch::Arm),
                "universal" | "universal2" => traits.universal = true,
                "all" => all = true,
                "debug" | "dbg" | "symbols" => traits.debug = true,
                "src" | "source" | "sources" => source = true,
                "checksum" | "checksums" | "sha256sums" | "sbom" => return None,
                token if FOREIGN_TOKENS.contains(&token) => return None,
                token if token.starts_with("armv") => traits.arch(Arch::Arm),
                token if token.starts_with("gnu") => {
                    traits.libc = Some(Libc::Glibc);
                    traits.arm_eabi(token);
                }
                token if token.starts_with("musl") => {
                    traits.libc = Some(Libc::Musl);
                    traits.arm_eabi(token);
                }
                token
                    if ["macos", "osx"].iter().any(|prefix| {
                        token
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
                    }) =>
                {
                    traits.os(Os::Macos)
                }
                _ => {}
            }
        }
        // `tool-1.0-src.tar.gz` is a source bundle, but `src_linux_amd64` (the
        // Sourcegraph CLI) is a binary: only a name without a platform counts.
        if source && traits.oses.is_empty() && traits.arches.is_empty() {
            return None;
        }
        // goreleaser names its universal macOS binary `darwin_all`.
        if all && traits.oses == [Os::Macos] && traits.arches.is_empty() {
            traits.universal = true;
        }
        Some(traits)
    }

    fn os(&mut self, os: Os) {
        if !self.oses.contains(&os) {
            self.oses.push(os);
        }
    }

    fn arch(&mut self, arch: Arch) {
        if !self.arches.contains(&arch) {
            self.arches.push(arch);
        }
    }

    /// `gnueabihf` / `musleabi` also say the build is 32-bit ARM.
    fn arm_eabi(&mut self, token: &str) {
        if token.contains("eabi") {
            self.arch(Arch::Arm);
        }
    }

    fn rank(&self, target: &AssetTarget) -> Option<Rank> {
        let platform = target.platform;
        let os = if self.oses.is_empty() {
            0
        } else if self.oses.contains(&platform.os) {
            1
        } else {
            return None;
        };
        let arch = if self.arches.contains(&platform.arch) {
            4
        } else if self.universal && platform.os == Os::Macos {
            3
        } else if self.arches.is_empty() {
            2
        } else if target.arch_pinned {
            return None;
        } else {
            self.arches
                .iter()
                .filter_map(|arch| emulated_rank(platform.os, platform.arch, *arch))
                .max()?
        };
        let libc = match platform.os {
            Os::Linux => linux_libc_rank(self.libc, platform.libc, target.libc_pinned)?,
            // Both toolchains produce working binaries; MSVC is the platform's
            // native ABI and the one most projects mean as their main build.
            Os::Windows => u8::from(self.msvc || (self.libc.is_none() && !self.mingw)),
            Os::Macos => 0,
        };
        Some(Rank {
            os,
            arch,
            libc,
            release_build: !self.debug,
            archive: self.archive,
        })
    }
}

fn linux_libc_rank(asset: Option<Libc>, host: Libc, pinned: bool) -> Option<u8> {
    match (asset, host, pinned) {
        (None, _, _) => Some(1),
        (Some(asset), host, true) => (asset == host).then_some(2),
        // A glibc binary cannot start on a musl system.
        (Some(Libc::Glibc), Libc::Musl, false) => None,
        // A static musl build runs anywhere, while a glibc build carries its
        // builder's glibc version as a floor.
        (Some(Libc::Musl), _, false) => Some(2),
        (Some(_), _, false) => Some(1),
    }
}

/// Architectures the OS runs through built-in emulation (Windows on Arm,
/// WOW64, Rosetta 2), ranked below every native or unlabelled build so they
/// are used only when the release has nothing better. 64-bit beats 32-bit.
fn emulated_rank(os: Os, host: Arch, asset: Arch) -> Option<u8> {
    match (os, host, asset) {
        (Os::Windows, Arch::Arm64, Arch::X64)
        | (Os::Windows, Arch::X64, Arch::X86)
        | (Os::Macos, Arch::Arm64, Arch::X64) => Some(1),
        (Os::Windows, Arch::Arm64, Arch::X86) => Some(0),
        _ => None,
    }
}

/// Lower-case alphanumeric runs, with `x86_64` / `x86-64` kept as one token.
fn tokens(lower: &str) -> Vec<String> {
    let raw: Vec<&str> = lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let mut tokens = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "x86" && raw.get(index + 1) == Some(&"64") {
            tokens.push("x86_64".to_string());
            index += 2;
        } else {
            tokens.push(raw[index].to_string());
            index += 1;
        }
    }
    tokens
}

/// The name without its archive or executable extension, so the same build in
/// two formats compares equal.
fn build_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".zip", ".7z", ".exe",
    ] {
        if let Some(stem) = lower.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    lower
}

const NON_BINARY_SUFFIXES: &[&str] = &[
    // checksums and signatures
    ".sha256",
    ".sha256sum",
    ".sha512",
    ".sha512sum",
    ".sha1",
    ".md5",
    ".b3",
    ".asc",
    ".sig",
    ".minisig",
    ".pem",
    ".crt",
    ".cert",
    ".pub",
    ".sigstore",
    ".bundle",
    // metadata and documents
    ".sbom",
    ".spdx",
    ".json",
    ".jsonl",
    ".txt",
    ".yml",
    ".yaml",
    ".xml",
    ".html",
    ".md",
    // installers, OS packages and scripts
    ".deb",
    ".rpm",
    ".apk",
    ".msi",
    ".msix",
    ".pkg",
    ".dmg",
    ".snap",
    ".flatpak",
    ".nupkg",
    ".vsix",
    ".whl",
    ".jar",
    ".sh",
    ".ps1",
    // debug symbols
    ".pdb",
    ".dsym",
    // compressed or packed formats osdk does not extract
    ".gz",
    ".bz2",
    ".tbz",
    ".tbz2",
    ".xz",
    ".zst",
    ".lz",
    ".lzma",
    ".tar",
    ".rar",
];

/// Platforms osdk never runs on; naming one rules the asset out even when the
/// name also contains a token osdk recognises.
const FOREIGN_TOKENS: &[&str] = &[
    "freebsd",
    "openbsd",
    "netbsd",
    "dragonfly",
    "android",
    "illumos",
    "solaris",
    "sunos",
    "aix",
    "ios",
    "wasi",
    "plan9",
    "haiku",
    "riscv64",
    "riscv64gc",
    "ppc",
    "ppc64",
    "ppc64le",
    "powerpc",
    "powerpc64",
    "powerpc64le",
    "s390x",
    "mips",
    "mipsel",
    "mips64",
    "mips64el",
    "loong64",
    "loongarch64",
    "sparc64",
    "wasm",
    "wasm32",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn target(os: Os, arch: Arch, libc: Libc) -> AssetTarget {
        AssetTarget {
            platform: Platform { os, arch, libc },
            arch_pinned: false,
            libc_pinned: false,
        }
    }

    fn pick(names: &[&str], target: AssetTarget) -> String {
        match choose_asset(names, &target) {
            AssetChoice::Selected(index) => names[index].to_string(),
            other => format!("{other:?}"),
        }
    }

    /// ripgrep 14.1.1's asset list, which names platforms by Rust triple.
    const RIPGREP: &[&str] = &[
        "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz",
        "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz.sha256",
        "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz",
        "ripgrep-14.1.1-armv7-unknown-linux-gnueabihf.tar.gz",
        "ripgrep-14.1.1-armv7-unknown-linux-musleabihf.tar.gz",
        "ripgrep-14.1.1-i686-pc-windows-msvc.zip",
        "ripgrep-14.1.1-i686-unknown-linux-gnu.tar.gz",
        "ripgrep-14.1.1-s390x-unknown-linux-gnu.tar.gz",
        "ripgrep-14.1.1-x86_64-apple-darwin.tar.gz",
        "ripgrep-14.1.1-x86_64-pc-windows-gnu.zip",
        "ripgrep-14.1.1-x86_64-pc-windows-msvc.zip",
        "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
        "ripgrep_14.1.1-1_amd64.deb",
    ];

    /// gh 2.62.0's asset list, which uses goreleaser's naming.
    const GH: &[&str] = &[
        "gh_2.62.0_checksums.txt",
        "gh_2.62.0_linux_386.tar.gz",
        "gh_2.62.0_linux_amd64.deb",
        "gh_2.62.0_linux_amd64.tar.gz",
        "gh_2.62.0_linux_arm64.tar.gz",
        "gh_2.62.0_linux_armv6.tar.gz",
        "gh_2.62.0_macOS_amd64.zip",
        "gh_2.62.0_macOS_arm64.zip",
        "gh_2.62.0_windows_386.zip",
        "gh_2.62.0_windows_amd64.msi",
        "gh_2.62.0_windows_amd64.zip",
        "gh_2.62.0_windows_arm64.zip",
    ];

    #[test]
    fn rust_triple_names_select_the_matching_build_in_any_listing_order() {
        let cases = [
            (
                Os::Windows,
                Arch::X64,
                Libc::None,
                "x86_64-pc-windows-msvc.zip",
            ),
            (
                Os::Windows,
                Arch::X86,
                Libc::None,
                "i686-pc-windows-msvc.zip",
            ),
            // No native arm64 Windows build: the x64 one runs emulated, and
            // `aarch64-apple-darwin` must not win because "darwin" contains "win".
            (
                Os::Windows,
                Arch::Arm64,
                Libc::None,
                "x86_64-pc-windows-msvc.zip",
            ),
            (
                Os::Linux,
                Arch::X64,
                Libc::Glibc,
                "x86_64-unknown-linux-musl.tar.gz",
            ),
            (
                Os::Linux,
                Arch::X86,
                Libc::Glibc,
                "i686-unknown-linux-gnu.tar.gz",
            ),
            (
                Os::Linux,
                Arch::Arm,
                Libc::Glibc,
                "armv7-unknown-linux-musleabihf.tar.gz",
            ),
            (
                Os::Linux,
                Arch::Arm,
                Libc::Musl,
                "armv7-unknown-linux-musleabihf.tar.gz",
            ),
            (
                Os::Macos,
                Arch::Arm64,
                Libc::None,
                "aarch64-apple-darwin.tar.gz",
            ),
            (
                Os::Macos,
                Arch::X64,
                Libc::None,
                "x86_64-apple-darwin.tar.gz",
            ),
        ];
        let mut reversed = RIPGREP.to_vec();
        reversed.reverse();
        for names in [RIPGREP.to_vec(), reversed] {
            for (os, arch, libc, expected) in cases {
                assert_eq!(
                    pick(&names, target(os, arch, libc)),
                    format!("ripgrep-14.1.1-{expected}"),
                    "{os:?}/{arch:?}/{libc:?}"
                );
            }
        }
    }

    #[test]
    fn a_glibc_only_build_is_never_offered_to_a_musl_host() {
        assert_eq!(
            pick(RIPGREP, target(Os::Linux, Arch::Arm64, Libc::Musl)),
            "NoMatch"
        );
        assert_eq!(
            pick(RIPGREP, target(Os::Linux, Arch::Arm64, Libc::Glibc)),
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn goreleaser_names_select_the_matching_archive() {
        let cases = [
            (Os::Windows, Arch::X64, "windows_amd64.zip"),
            (Os::Windows, Arch::Arm64, "windows_arm64.zip"),
            (Os::Windows, Arch::X86, "windows_386.zip"),
            (Os::Linux, Arch::X64, "linux_amd64.tar.gz"),
            (Os::Linux, Arch::X86, "linux_386.tar.gz"),
            (Os::Linux, Arch::Arm, "linux_armv6.tar.gz"),
            (Os::Linux, Arch::Arm64, "linux_arm64.tar.gz"),
            (Os::Macos, Arch::Arm64, "macOS_arm64.zip"),
        ];
        for (os, arch, expected) in cases {
            assert_eq!(
                pick(GH, target(os, arch, Libc::Glibc)),
                format!("gh_2.62.0_{expected}"),
                "{os:?}/{arch:?}"
            );
        }
    }

    #[test]
    fn a_build_for_another_os_is_never_a_fallback() {
        let names = [
            "tool-1.0-win64.zip",
            "tool-1.0-linux-x86_64.tar.gz",
            "tool-1.0-darwin-x86_64.tar.gz",
        ];
        assert_eq!(
            pick(&names, target(Os::Windows, Arch::X64, Libc::None)),
            "tool-1.0-win64.zip"
        );
        assert_eq!(
            pick(&names, target(Os::Macos, Arch::Arm64, Libc::None)),
            "tool-1.0-darwin-x86_64.tar.gz"
        );
        assert_eq!(
            pick(&names, target(Os::Linux, Arch::Arm64, Libc::Glibc)),
            "NoMatch"
        );
        assert_eq!(
            pick(
                &["tool-1.0-win64.zip"],
                target(Os::Linux, Arch::X64, Libc::Glibc)
            ),
            "NoMatch"
        );
    }

    #[test]
    fn emulation_is_a_last_resort_and_never_overrides_a_pinned_arch() {
        let names = ["tool-macos-x64.tar.gz", "tool-macos-universal.tar.gz"];
        assert_eq!(
            pick(&names, target(Os::Macos, Arch::Arm64, Libc::None)),
            "tool-macos-universal.tar.gz"
        );
        let mut pinned = target(Os::Macos, Arch::Arm64, Libc::None);
        pinned.arch_pinned = true;
        assert_eq!(pick(&["tool-macos-x64.tar.gz"], pinned), "NoMatch");
        // Linux has no built-in emulation to fall back on.
        assert_eq!(
            pick(
                &["tool-linux-x64.tar.gz"],
                target(Os::Linux, Arch::Arm64, Libc::Glibc)
            ),
            "NoMatch"
        );
        assert_eq!(
            pick(
                &["tool_darwin_all.tar.gz"],
                target(Os::Macos, Arch::Arm64, Libc::None)
            ),
            "tool_darwin_all.tar.gz"
        );
    }

    #[test]
    fn a_pinned_libc_rejects_the_other_libc() {
        let names = ["tool-linux-x64-gnu.tar.gz", "tool-linux-x64-musl.tar.gz"];
        let mut gnu = target(Os::Linux, Arch::X64, Libc::Glibc);
        assert_eq!(pick(&names, gnu), "tool-linux-x64-musl.tar.gz");
        gnu.libc_pinned = true;
        assert_eq!(pick(&names, gnu), "tool-linux-x64-gnu.tar.gz");
    }

    #[test]
    fn non_binaries_and_foreign_platforms_are_excluded() {
        let linux = target(Os::Linux, Arch::X64, Libc::Glibc);
        for name in [
            "tool-linux-amd64.tar.gz.sha256",
            "tool-linux-amd64.deb",
            "tool-linux-amd64.sbom.json",
            "tool-linux-amd64.gz",
            "tool-1.0-src.tar.gz",
            "tool-freebsd-amd64.tar.gz",
            "tool-linux-riscv64.tar.gz",
            "install.sh",
        ] {
            assert_eq!(pick(&[name], linux), "NoMatch", "{name}");
        }
        // An unlabelled file is only a candidate when no asset names an OS.
        assert_eq!(
            pick(&["tool.tar.gz", "tool-windows-x64.zip"], linux),
            "NoMatch"
        );
        assert_eq!(pick(&["tool-x86_64.tar.gz"], linux), "tool-x86_64.tar.gz");
        // A platform-labelled `src` is a program name, not a source bundle.
        assert_eq!(pick(&["src_linux_amd64"], linux), "src_linux_amd64");
    }

    #[test]
    fn ties_resolve_only_between_formats_of_the_same_build() {
        let linux = target(Os::Linux, Arch::X64, Libc::Glibc);
        assert_eq!(
            pick(&["tool-linux-x64.zip", "tool-linux-x64.tar.gz"], linux),
            "tool-linux-x64.tar.gz"
        );
        assert_eq!(
            pick(
                &["tool-linux-x64.tar.gz", "tool-linux-x64-debug.tar.gz"],
                linux
            ),
            "tool-linux-x64.tar.gz"
        );
        assert_eq!(
            choose_asset(
                &["toolctl-linux-x64.tar.gz", "tool-linux-x64.tar.gz"],
                &linux
            ),
            AssetChoice::Ambiguous(vec![
                "tool-linux-x64.tar.gz".into(),
                "toolctl-linux-x64.tar.gz".into()
            ])
        );
    }
}
