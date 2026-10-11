//! Persistent (User/Machine) environment variables, as opposed to the process
//! block.
//!
//! At install time osdk wants to warn when a tool's managed environment
//! variable already exists persistently (for example a `PNPM_HOME` left behind
//! by a standalone pnpm). The standard environment only exposes the process
//! block, which also contains values the current osdk hook injected; reading the
//! persistent store directly distinguishes a genuinely pre-existing value from
//! one the hook just set.
//!
//! Windows persists these in the registry (`HKCU\Environment` and
//! `HKLM\...\Session Manager\Environment`). On Unix the equivalent lives in
//! shell startup files, which have no single parseable location, so this module
//! reports `None` there rather than guess.

/// Where a persistent variable was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentScope {
    /// The current user's block (`HKCU\Environment`).
    User,
    /// The machine-wide block (affects every user).
    Machine,
}

/// A persistent variable that was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentVar {
    /// Whether it was set for the user or machine-wide.
    pub scope: PersistentScope,
    /// The stored value (REG_EXPAND_SZ is returned unexpanded).
    pub value: String,
}

/// Look up `name` in the persistent User block first, then the Machine block.
pub fn get(name: &str) -> Option<PersistentVar> {
    #[cfg(windows)]
    {
        if let Some(value) = registry_query(HKEY_CURRENT_USER, "Environment", name) {
            return Some(PersistentVar {
                scope: PersistentScope::User,
                value,
            });
        }
        if let Some(value) = registry_query(
            HKEY_LOCAL_MACHINE,
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
            name,
        ) {
            return Some(PersistentVar {
                scope: PersistentScope::Machine,
                value,
            });
        }
    }
    #[cfg(not(windows))]
    {
        let _ = name;
    }
    None
}

/// Location variables a backend sets unconditionally in `exec_env` (like
/// `JAVA_HOME`), so a pre-existing persistent value is always replaced while
/// the tool is active. Mirrors the unconditional inserts in each backend.
pub fn forced_var_names(tool: &str) -> &'static [&'static str] {
    match tool {
        "node" => &["npm_config_prefix"],
        "npm" => &["npm_config_prefix"],
        "pnpm" => &["PNPM_HOME"],
        "yarn" => &["YARN_GLOBAL_FOLDER"],
        "bun" => &["BUN_INSTALL"],
        "deno" => &["DENO_INSTALL_ROOT"],
        "go" => &["GOROOT", "GOPROXY", "GOBIN"],
        "rust" => &["RUSTUP_HOME", "CARGO_HOME"],
        "java" => &["JAVA_HOME"],
        _ => &[],
    }
}

/// Cache/redirect variables a backend injects through the cache layer, which
/// honor an explicit user value unless the override switch is on. Mirrors the
/// mappings in each backend's `exec_env`.
pub fn managed_var_names(tool: &str) -> &'static [&'static str] {
    match tool {
        "node" => &["npm_config_cache"],
        "npm" => &["npm_config_cache"],
        // pnpm 11 uses pnpm_config_store_dir; older versions npm_config_store_dir.
        "pnpm" => &["pnpm_config_store_dir", "npm_config_store_dir"],
        "bun" => &["BUN_INSTALL_CACHE_DIR"],
        "deno" => &["DENO_DIR"],
        "go" => &["GOMODCACHE", "GOCACHE"],
        "java" => &["GRADLE_USER_HOME"],
        "python" => &["PIP_CACHE_DIR", "UV_CACHE_DIR"],
        // YARN_GLOBAL_FOLDER is a forced location variable now; only classic's
        // disposable YARN_CACHE_FOLDER stays in the cache layer.
        "yarn" => &["YARN_CACHE_FOLDER"],
        "zig" => &["ZIG_GLOBAL_CACHE_DIR"],
        _ => &[],
    }
}

/// Inspect the persistent environment for a tool about to be installed and
/// build one warning per relevant variable that already exists.
///
/// Forced location variables are always replaced, so the warning only says so.
/// Cache variables honor the override switch: with it on the value is replaced;
/// without it osdk keeps the value (explicit choice wins), which can point the
/// tool at the wrong directory, so the warning explains how to remove it or
/// turn override on.
pub fn install_warnings(settings: &crate::config::Settings, tool: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let overriding = settings.effective_override_env(tool);

    for name in forced_var_names(tool) {
        let Some(found) = get(name) else { continue };
        warnings.push(format!(
            "persistent {} `{name}` already exists (`{}`); osdk will point this tool at its own directory while active",
            scope_label(found.scope),
            found.value
        ));
    }

    for name in managed_var_names(tool) {
        let Some(found) = get(name) else { continue };
        let scope = scope_label(found.scope);
        let line = if overriding {
            format!(
                "persistent {scope} `{name}` already exists (`{}`); osdk will override it while this tool is active",
                found.value
            )
        } else {
            format!(
                "persistent {scope} `{name}` already exists (`{}`); osdk keeps this value, which can point the managed tool at the wrong directory\n  \
                 remove the variable, or run `osdk config set {tool}.override_env true` to override it (`osdk config set override_tools_env true` for every tool)",
                found.value
            )
        };
        warnings.push(line);
    }
    warnings
}

fn scope_label(scope: PersistentScope) -> &'static str {
    match scope {
        PersistentScope::User => "user-level",
        PersistentScope::Machine => "machine-level",
    }
}

/// Decode a registry string value's bytes (UTF-16LE, possibly with trailing
/// nulls) into a Rust string. Kept as a pure function so the byte handling is
/// testable without touching registry.
#[cfg(windows)]
fn decode_sz(bytes: &[u8]) -> String {
    let words: Vec<u16> = (0..bytes.len() / 2)
        .map(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        .collect();
    String::from_utf16_lossy(&words)
        .trim_end_matches('\0')
        .to_string()
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_EXPAND_SZ, REG_SZ,
};

/// Read one value from a registry hive/subkey, returning None on any miss or
/// non-string type.
#[cfg(windows)]
fn registry_query(hive: HKEY, subkey: &str, name: &str) -> Option<String> {
    unsafe {
        let subkey_w = wide(subkey);
        let mut hkey: HKEY = std::ptr::null_mut();
        // ERROR_SUCCESS == 0.
        if RegOpenKeyExW(hive, subkey_w.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }
        let name_w = wide(name);
        let mut kind = 0;
        let mut size = 0;
        // Probe for the required buffer size.
        if RegQueryValueExW(
            hkey,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        ) != 0
        {
            RegCloseKey(hkey);
            return None;
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            RegCloseKey(hkey);
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let mut returned = size;
        let status = RegQueryValueExW(
            hkey,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buffer.as_mut_ptr(),
            &mut returned,
        );
        RegCloseKey(hkey);
        if status != 0 {
            return None;
        }
        Some(decode_sz(&buffer[..returned as usize]))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn decode_trims_trailing_nuls_and_keeps_content() {
        let mut bytes = Vec::new();
        for unit in "C:\\Tools\\pnpm".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        // One trailing wide null.
        bytes.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(decode_sz(&bytes), "C:\\Tools\\pnpm");
    }

    #[test]
    fn decode_leaves_expand_tokens_verbatim() {
        let mut bytes = Vec::new();
        for unit in "%USERPROFILE%\\bin".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
        // REG_EXPAND_SZ tokens are not expanded.
        assert_eq!(decode_sz(&bytes), "%USERPROFILE%\\bin");
    }

    #[test]
    fn missing_persistent_variable_is_none() {
        assert!(get("OSDK_DEFINITELY_NOT_A_REAL_VARIABLE_12345").is_none());
    }

    #[test]
    fn path_is_readable_from_persistent_store() {
        // Windows always has a machine Path and users commonly have a user Path
        // too; get() returns the user one first, so only assert presence.
        let found = get("Path").expect("persistent Path should be readable");
        assert!(!found.value.is_empty());
    }

    #[test]
    fn machine_environment_block_is_readable() {
        let value = registry_query(
            HKEY_LOCAL_MACHINE,
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
            "Path",
        )
        .expect("machine Path should be readable");
        assert!(!value.is_empty());
    }
}
