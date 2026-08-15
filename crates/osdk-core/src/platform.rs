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

    /// go's arch token, e.g. `amd64`, `arm64`.
    pub fn go_token(self) -> &'static str {
        match self {
            Arch::X64 => "amd64",
            Arch::Arm64 => "arm64",
            Arch::X86 => "386",
            Arch::Arm => "armv6l",
        }
    }

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
    /// Detect libc flavor. Only meaningful on Linux; elsewhere returns `None`.
    ///
    /// We detect musl by checking whether the dynamic loader path or ldd output
    /// mentions musl. This is best-effort; backends can override.
    pub fn current() -> Libc {
        #[cfg(target_os = "linux")]
        {
            if cfg!(target_env = "musl") {
                return Libc::Musl;
            }
            // Best-effort runtime detection: musl systems ship `ld-musl-*.so`.
            if std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
                || std::path::Path::new("/lib/ld-musl-aarch64.so.1").exists()
            {
                return Libc::Musl;
            }
            Libc::Glibc
        }
        #[cfg(not(target_os = "linux"))]
        {
            Libc::None
        }
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
