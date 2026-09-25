//! OS / architecture detection and target-triple normalization.
//!
//! Different SDKs name their platform assets differently (node uses
//! `darwin-arm64`, go uses `darwin-arm64` too but `linux-amd64`, python-build-
//! standalone uses full LLVM triples like `x86_64-unknown-linux-gnu`). We detect
//! the host once here and let each backend map it to its own naming scheme.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
    X86,
    Arm,
}

/// C library flavor, relevant on Linux (glibc vs musl).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Libc {
    Glibc,
    Musl,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
    pub libc: Libc,
}

impl Os {
    pub fn current() -> Os {
        #[cfg(target_os = "linux")]
        {
            Os::Linux
        }
        #[cfg(target_os = "macos")]
        {
            Os::Macos
        }
        #[cfg(target_os = "windows")]
        {
            Os::Windows
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            compile_error!("unsupported target_os")
        }
    }

    /// Whether executables carry the `.exe` suffix on this OS.
    pub fn exe_suffix(self) -> &'static str {
        match self {
            Os::Windows => ".exe",
            _ => "",
        }
    }

    /// node's platform token, e.g. `linux`, `darwin`, `win`.
    pub fn node_token(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "darwin",
            Os::Windows => "win",
        }
    }

    /// go's platform token, e.g. `linux`, `darwin`, `windows`.
    pub fn go_token(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "darwin",
            Os::Windows => "windows",
        }
    }

    /// The token this OS is written as in configuration and lock platform keys.
    ///
    /// Deliberately the single source for both. `syspkg`'s `os` filter, the
    /// `[tools]` platform filters and `osdk.lock`'s `platforms.<os>-<arch>` keys
    /// previously each spelled these out separately, so renaming one would have
    /// silently mismatched the others instead of failing to compile.
    pub fn config_token(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }

    /// Parse an OS written in configuration, accepting the usual spellings.
    ///
    /// Returning `None` rather than falling back is what lets a typo be
    /// reported. A platform filter that silently fails to match would drop the
    /// entry on *every* machine while still looking like a deliberate
    /// restriction -- the failure would surface as "the tool is missing", far
    /// from the line that caused it.
    pub fn parse_config(value: &str) -> Option<Os> {
        match value.trim().to_ascii_lowercase().as_str() {
            "linux" => Some(Os::Linux),
            "macos" | "mac" | "darwin" | "osx" => Some(Os::Macos),
            "windows" | "win" | "win32" => Some(Os::Windows),
            _ => None,
        }
    }

    /// Every OS token accepted in configuration, for error messages.
    pub const CONFIG_TOKENS: &'static [&'static str] = &["linux", "macos", "windows"];
}

impl Arch {
    pub fn current() -> Arch {
        #[cfg(target_arch = "x86_64")]
        {
            Arch::X64
        }
        #[cfg(target_arch = "aarch64")]
        {
            Arch::Arm64
        }
        #[cfg(target_arch = "x86")]
        {
            Arch::X86
        }
        #[cfg(target_arch = "arm")]
        {
            Arch::Arm
        }
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm"
        )))]
        {
            compile_error!("unsupported target_arch")
        }
    }

    /// node's arch token, e.g. `x64`, `arm64`.
    pub fn node_token(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
            Arch::X86 => "x86",
            Arch::Arm => "armv7l",
        }
    }

    pub fn parse_node(value: &str) -> Option<Arch> {
        match value.trim().to_ascii_lowercase().as_str() {
            "x64" | "amd64" | "x86_64" => Some(Arch::X64),
            "arm64" | "aarch64" => Some(Arch::Arm64),
            "x86" | "ia32" | "i386" | "i686" => Some(Arch::X86),
            "arm" | "armv7" | "armv7l" => Some(Arch::Arm),
            _ => None,
        }
    }

    /// go's arch token, e.g. `amd64`, `arm64`.
    pub fn go_token(self) -> &'static str {
        match self {
            Arch::X64 => "amd64",
            Arch::Arm64 => "arm64",
            Arch::X86 => "386",
            Arch::Arm => "armv6l",
        }
    }

    /// The token this arch is written as in configuration and lock platform keys.
    ///
    /// Parsing accepts more spellings than this emits; see [`Arch::parse_node`],
    /// which already handles `amd64`/`x86_64`/`aarch64` and is reused rather than
    /// duplicated, so the two cannot drift apart.
    pub fn config_token(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
            Arch::X86 => "x86",
            Arch::Arm => "arm",
        }
    }

    /// Every arch token accepted in configuration, for error messages.
    pub const CONFIG_TOKENS: &'static [&'static str] = &["x64", "arm64", "x86", "arm"];

    /// The CPU part of an LLVM target triple, e.g. `x86_64`, `aarch64`.
    pub fn llvm_token(self) -> &'static str {
        match self {
            Arch::X64 => "x86_64",
            Arch::Arm64 => "aarch64",
            Arch::X86 => "i686",
            Arch::Arm => "armv7",
        }
    }
}

impl Libc {
    /// Detect the libc ABI this binary was built for.
    ///
    /// A host may have several libc loaders installed at once (for example,
    /// `musl-tools` on Ubuntu). Their presence does not change the ABI of the
    /// running executable, so filesystem probes would make platform keys and
    /// artifact selection depend on unrelated cross-compilation packages.
    pub fn current() -> Libc {
        #[cfg(all(target_os = "linux", target_env = "musl"))]
        {
            Libc::Musl
        }
        #[cfg(all(target_os = "linux", not(target_env = "musl")))]
        {
            Libc::Glibc
        }
        #[cfg(not(target_os = "linux"))]
        {
            Libc::None
        }
    }
}

/// A platform restriction written as a `when` table on a config entry.
///
/// The semantics are **filter, not assertion**: an entry whose `when` does not
/// match the host is treated as absent, exactly as `[syspkg.packages]`'s `os`
/// already behaved. It can only ever narrow where something applies, never cause
/// something to be installed that otherwise would not be, which is why it needs
/// no trust.
///
/// Values within one dimension are OR (`os = ["linux", "macos"]` means either),
/// and the dimensions are AND (`os` **and** `arch` must both match).
///
/// # Why a nested `when` rather than flat `os`/`arch` keys
///
/// `os`, `arch` and `libc` are **already** backend options on `github:` and
/// `node`, where they select *which artifact to download* -- that is what makes
/// `osdk lock node@20 -o arch=arm64` produce a lock section for another
/// architecture. Reusing those names for "where does this entry apply" gave them
/// two opposite meanings on the same key: a first attempt at this feature made
/// `[tools.node] arch = "arm64"` drop node entirely on an x64 host, silently
/// breaking cross-architecture locking. Nesting the filter keeps the two
/// vocabularies apart, and leaves room for `libc` without a third collision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformFilter {
    /// Empty means "every OS".
    pub os: Vec<Os>,
    /// Empty means "every architecture".
    pub arch: Vec<Arch>,
}

/// One or several tokens, as either `os = "linux"` or `os = ["linux", "macos"]`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum Tokens {
    One(String),
    Many(Vec<String>),
}

impl Tokens {
    fn into_vec(self) -> Vec<String> {
        match self {
            Tokens::One(value) => vec![value],
            Tokens::Many(values) => values,
        }
    }
}

/// The raw `when` table, before tokens are validated.
///
/// `deny_unknown_fields` is what makes a future dimension fail loudly instead of
/// being ignored: `when = { libc = "musl" }` on a build that does not implement
/// `libc` must not silently widen the filter to "every libc".
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWhen {
    #[serde(default)]
    os: Option<Tokens>,
    #[serde(default)]
    arch: Option<Tokens>,
}

impl<'de> serde::Deserialize<'de> for PlatformFilter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawWhen::deserialize(deserializer)?;
        let os = raw.os.map(Tokens::into_vec).unwrap_or_default();
        let arch = raw.arch.map(Tokens::into_vec).unwrap_or_default();
        PlatformFilter::parse(&os, &arch).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for PlatformFilter {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if !self.os.is_empty() {
            let tokens: Vec<_> = self.os.iter().map(|os| os.config_token()).collect();
            map.serialize_entry("os", &tokens)?;
        }
        if !self.arch.is_empty() {
            let tokens: Vec<_> = self.arch.iter().map(|arch| arch.config_token()).collect();
            map.serialize_entry("arch", &tokens)?;
        }
        map.end()
    }
}

/// A token in a platform filter that no version of osdk understands.
///
/// Reported rather than skipped. Treating an unparseable token as "does not
/// match" would drop the entry on every machine while the report still looked
/// like a deliberate platform restriction -- the entry would simply never
/// install, with nothing pointing at the typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPlatformToken {
    /// Which dimension it was written in: `os` or `arch`.
    pub dimension: &'static str,
    /// The token as written.
    pub value: String,
    /// The tokens that would have been accepted.
    pub accepted: &'static [&'static str],
}

impl std::fmt::Display for UnknownPlatformToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unknown {} `{}` (expected one of {})",
            self.dimension,
            self.value,
            self.accepted.join(", ")
        )
    }
}

impl PlatformFilter {
    /// Whether this filter applies to `platform`.
    ///
    /// An empty dimension imposes no restriction, so a default filter matches
    /// everything and an entry without `os`/`arch` behaves exactly as before.
    pub fn matches(&self, platform: &Platform) -> bool {
        let os_ok = self.os.is_empty() || self.os.contains(&platform.os);
        let arch_ok = self.arch.is_empty() || self.arch.contains(&platform.arch);
        os_ok && arch_ok
    }

    /// Whether anything at all was restricted.
    pub fn is_unrestricted(&self) -> bool {
        self.os.is_empty() && self.arch.is_empty()
    }

    /// Build a filter from raw tokens, rejecting any that is not understood.
    pub fn parse(
        os: &[String],
        arch: &[String],
    ) -> std::result::Result<Self, UnknownPlatformToken> {
        let mut filter = PlatformFilter::default();
        for value in os {
            let parsed = Os::parse_config(value).ok_or_else(|| UnknownPlatformToken {
                dimension: "os",
                value: value.clone(),
                accepted: Os::CONFIG_TOKENS,
            })?;
            if !filter.os.contains(&parsed) {
                filter.os.push(parsed);
            }
        }
        for value in arch {
            // `parse_node` already accepts amd64/x86_64/aarch64 and friends.
            let parsed = Arch::parse_node(value).ok_or_else(|| UnknownPlatformToken {
                dimension: "arch",
                value: value.clone(),
                accepted: Arch::CONFIG_TOKENS,
            })?;
            if !filter.arch.contains(&parsed) {
                filter.arch.push(parsed);
            }
        }
        Ok(filter)
    }

    /// Describe the restriction for a message, e.g. `os=windows, arch=arm64`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.os.is_empty() {
            let names: Vec<_> = self.os.iter().map(|os| os.config_token()).collect();
            parts.push(format!("os={}", names.join("|")));
        }
        if !self.arch.is_empty() {
            let names: Vec<_> = self.arch.iter().map(|arch| arch.config_token()).collect();
            parts.push(format!("arch={}", names.join("|")));
        }
        parts.join(", ")
    }
}

impl Platform {
    pub fn current() -> Platform {
        Platform {
            os: Os::current(),
            arch: Arch::current(),
            libc: Libc::current(),
        }
    }

    /// python-build-standalone / rustup style LLVM triple for this host,
    /// e.g. `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`,
    /// `x86_64-pc-windows-msvc`.
    pub fn llvm_triple(&self) -> String {
        let cpu = self.arch.llvm_token();
        match self.os {
            Os::Linux => {
                let libc = match self.libc {
                    Libc::Musl => "musl",
                    _ => "gnu",
                };
                format!("{cpu}-unknown-linux-{libc}")
            }
            Os::Macos => format!("{cpu}-apple-darwin"),
            Os::Windows => format!("{cpu}-pc-windows-msvc"),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let os = match self.os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        };
        let arch = match self.arch {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
            Arch::X86 => "x86",
            Arch::Arm => "arm",
        };
        write!(f, "{os}-{arch}")?;
        if matches!(self.libc, Libc::Musl) {
            write!(f, "-musl")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_libc_follows_the_binary_abi() {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert_eq!(Libc::current(), Libc::Glibc);
        #[cfg(all(target_os = "linux", target_env = "musl"))]
        assert_eq!(Libc::current(), Libc::Musl);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(Libc::current(), Libc::None);
    }

    #[test]
    fn llvm_triple_shape() {
        let p = Platform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        };
        assert_eq!(p.llvm_triple(), "x86_64-unknown-linux-gnu");

        let p = Platform {
            os: Os::Macos,
            arch: Arch::Arm64,
            libc: Libc::None,
        };
        assert_eq!(p.llvm_triple(), "aarch64-apple-darwin");

        let p = Platform {
            os: Os::Windows,
            arch: Arch::X64,
            libc: Libc::None,
        };
        assert_eq!(p.llvm_triple(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn tokens() {
        assert_eq!(Os::Windows.exe_suffix(), ".exe");
        assert_eq!(Os::Linux.exe_suffix(), "");
        assert_eq!(Arch::X64.go_token(), "amd64");
        assert_eq!(Arch::X64.node_token(), "x64");
    }
}
