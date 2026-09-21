//! Windows-only WinINET registry read for proxy diagnostics.
//!
//! The Settings / Internet Options proxy toggle lives under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`. Browsers
//! and other WinINET clients read it; reqwest does not. This is read-only and is
//! used purely to explain a timeout -- nothing here is ever written.
//!
//! Kept behind `#[cfg(windows)]` in its own file so the portable module has no
//! Windows types in its signature and the dependency is target-scoped.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};

use super::WindowsProxySettings;

const SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";

/// Wide NUL-terminated encoding of a constant ASCII registry path.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read one `REG_SZ`/`REG_EXPAND_SZ` value, returning `None` when absent or empty.
///
/// The returned buffer can include a trailing NUL (REG_SZ values commonly do);
/// it is trimmed before return.
fn read_string(key: HKEY, name: &[u16]) -> Option<String> {
    let mut ty = 0u32;
    let mut len = 0u32;
    // Size the buffer first.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if status != ERROR_SUCCESS || len == 0 {
        return None;
    }
    let mut buffer = vec![0u8; len as usize];
    let mut len2 = len;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut ty,
            buffer.as_mut_ptr(),
            &mut len2,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    // Decode the UTF-16LE units that actually came back. `len2` is a byte count
    // for a wide string and is therefore even; split it into fixed `[u8; 2]` cells.
    let units = len2 as usize / 2;
    let wide: Vec<u16> = buffer[..units * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let value = String::from_utf16_lossy(&wide);
    let trimmed = value.trim_end_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Read a `REG_DWORD` value as a `u32`, `None` if absent.
fn read_dword(key: HKEY, name: &[u16]) -> Option<u32> {
    let mut ty = 0u32;
    let mut value = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut ty,
            &mut value as *mut u32 as *mut u8,
            &mut len,
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

pub(super) fn read_internet_settings() -> Option<WindowsProxySettings> {
    let subkey = wide(SUBKEY);
    let mut key: HKEY = std::ptr::null_mut();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return None;
    }
    // SAFETY: `key` is a valid open HKEY for every read below and is closed once.
    let proxy_enable = wide("ProxyEnable");
    let proxy_server = wide("ProxyServer");
    let auto_config = wide("AutoConfigURL");
    let enabled = read_dword(key, &proxy_enable).is_some_and(|v| v != 0);
    let server = read_string(key, &proxy_server);
    let pac = read_string(key, &auto_config).is_some();
    let result = Some(WindowsProxySettings {
        enabled,
        server,
        pac,
    });
    unsafe {
        RegCloseKey(key);
    }
    result
}
