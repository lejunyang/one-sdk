//! Running the version the lock recorded.
//!
//! `osdk install` replays `osdk.lock`, so with `node = "20"` and a lock entry
//! of 20.20.2 the machine gets 20.20.2. But everything that *runs* a tool --
//! shims, activation, `exec`, `where` -- used to re-resolve `"20"` against the
//! installs, so a machine that also had 20.21.0 (from another project, or a
//! newer install) ran that instead. The lock described one environment and the
//! shell ran another.
//!
//! The resolver therefore narrows a project declaration to its locked exact
//! version, under two conditions that keep the lock from overriding intent:
//!
//! - the declaration comes from the project (never the global config), and
//! - the lock entry was resolved from the same request text. After the user
//!   edits `node = "20"` to `"22"`, a lock still saying `20 -> 20.20.2` is
//!   stale; it is ignored here, and `osdk install` warns about the drift.
//!
//! The narrowed version must be installed like any other selection; a missing
//! one is reported, not silently replaced by another match.
//!
//! Install-side callers (gathering requests, writing the lock, `osdk use`) use
//! the `*_declared` resolver entry points instead: they need the declaration
//! as written, or the lock would record `20.20.2` as the request.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::resolver::{ActiveVersion, VersionOrigin};
use super::VersionSpec;
use crate::platform::{Arch, Libc, Os, Platform};

pub const LOCKFILE_NAME: &str = "osdk.lock";

/// The key a lock uses for one platform's entries.
pub fn platform_key(platform: Platform) -> String {
    let os = match platform.os {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
        Arch::X86 => "x86",
        Arch::Arm => "arm",
    };
    match platform.libc {
        Libc::Musl => format!("{os}-{arch}-musl"),
        _ => format!("{os}-{arch}"),
    }
}

/// The lock that governs `start`: a selected project's own lock, else the
/// nearest `osdk.lock` above it -- the same rule `osdk install` replays by.
pub fn governing_lock(config: &crate::config::Config, start: &Path) -> Option<PathBuf> {
    if let Some(project) = &config.project {
        return project
            .lock_path
            .is_file()
            .then(|| project.lock_path.clone());
    }
    start
        .ancestors()
        .map(|directory| directory.join(LOCKFILE_NAME))
        .find(|path| path.is_file())
}

#[derive(serde::Deserialize)]
struct PinFile {
    #[serde(default)]
    platforms: BTreeMap<String, PinPlatform>,
}

#[derive(serde::Deserialize)]
struct PinPlatform {
    #[serde(default)]
    tools: BTreeMap<String, Pin>,
}

#[derive(Clone, serde::Deserialize)]
struct Pin {
    request: String,
    version: String,
}

type Pins = BTreeMap<String, Pin>;

thread_local! {
    /// Activation resolves every backend in one process; read each lock once.
    static PINS: RefCell<BTreeMap<PathBuf, Option<Pins>>> = const { RefCell::new(BTreeMap::new()) };
}

fn pins(path: &Path) -> Option<Pins> {
    PINS.with(|cache| {
        cache
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                // An unreadable lock pins nothing: `osdk install` reports it,
                // and running a tool must not fail on a file it only consults.
                let text = crate::fs::read_text_without_bom(path).ok()?;
                let file: PinFile = toml::from_str(&text).ok()?;
                file.platforms
                    .get(&platform_key(Platform::current()))
                    .map(|platform| platform.tools.clone())
            })
            .clone()
    })
}

/// Narrow `active` to the lock's exact version when the lock was resolved
/// from this same declaration.
pub fn pin_to_lock(config: &crate::config::Config, start: &Path, active: &mut ActiveVersion) {
    if matches!(active.source, VersionOrigin::GlobalConfig) {
        return;
    }
    let Some(lock) = governing_lock(config, start) else {
        return;
    };
    let Some(pin) = pins(&lock).and_then(|pins| pins.get(&active.tool).cloned()) else {
        return;
    };
    if !declares(config, active, &pin.request) {
        return;
    }
    active.spec = pin.version;
    active.is_range = false;
}

/// Whether `request` (as the lock writer stored it: `VersionSpec::to_string`)
/// is what `active` declares, with or without alias expansion.
fn declares(config: &crate::config::Config, active: &ActiveVersion, request: &str) -> bool {
    let render = |text: &str| -> Option<String> {
        if active.is_range {
            VersionSpec::parse_range(text)
                .ok()
                .map(|spec| spec.to_string())
        } else {
            Some(VersionSpec::parse(text).to_string())
        }
    };
    if render(&active.spec).as_deref() == Some(request) {
        return true;
    }
    config
        .expand_alias(&active.tool, &active.spec)
        .ok()
        .and_then(|expanded| render(&expanded))
        .as_deref()
        == Some(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lock(dir: &Path, tool: &str, request: &str, version: &str) {
        std::fs::write(
            dir.join(LOCKFILE_NAME),
            format!(
                "schema = 4\n[platforms.{}.tools.\"{tool}\"]\nrequest = \"{request}\"\nversion = \"{version}\"\n",
                platform_key(Platform::current())
            ),
        )
        .unwrap();
    }

    fn active(tool: &str, spec: &str, source: VersionOrigin) -> ActiveVersion {
        ActiveVersion {
            tool: tool.into(),
            spec: spec.into(),
            source,
            is_range: false,
        }
    }

    #[test]
    fn a_project_declaration_runs_the_locked_version() {
        let temp = tempfile::tempdir().unwrap();
        write_lock(temp.path(), "node", "20", "20.20.2");
        let config = crate::config::Config::default();
        let mut node = active(
            "node",
            "20",
            VersionOrigin::ProjectConfig(temp.path().join("osdk.toml")),
        );
        pin_to_lock(&config, temp.path(), &mut node);
        assert_eq!(node.spec, "20.20.2");
    }

    /// After `node = "20"` became `"22"`, the lock's `20 -> 20.20.2` describes
    /// a declaration that no longer exists and must not win.
    #[test]
    fn a_stale_lock_entry_does_not_override_an_edited_declaration() {
        let temp = tempfile::tempdir().unwrap();
        write_lock(temp.path(), "node", "20", "20.20.2");
        let config = crate::config::Config::default();
        let mut node = active(
            "node",
            "22",
            VersionOrigin::ProjectConfig(temp.path().join("osdk.toml")),
        );
        pin_to_lock(&config, temp.path(), &mut node);
        assert_eq!(node.spec, "22");
    }

    /// The global pin is the user's own default, not the project's lock.
    #[test]
    fn a_global_declaration_is_not_pinned_by_a_project_lock() {
        let temp = tempfile::tempdir().unwrap();
        write_lock(temp.path(), "node", "20", "20.20.2");
        let config = crate::config::Config::default();
        let mut node = active("node", "20", VersionOrigin::GlobalConfig);
        pin_to_lock(&config, temp.path(), &mut node);
        assert_eq!(node.spec, "20");
    }

    #[test]
    fn a_lock_found_above_the_directory_applies_and_an_unreadable_one_pins_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("pkg/src");
        std::fs::create_dir_all(&child).unwrap();
        write_lock(temp.path(), "go", "1.22", "1.22.5");
        let config = crate::config::Config::default();
        let mut go = active(
            "go",
            "1.22",
            VersionOrigin::ToolVersions(temp.path().join(".tool-versions")),
        );
        pin_to_lock(&config, &child, &mut go);
        assert_eq!(go.spec, "1.22.5");

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(LOCKFILE_NAME), "not toml [").unwrap();
        let mut go = active(
            "go",
            "1.22",
            VersionOrigin::ToolVersions(broken.path().join(".tool-versions")),
        );
        pin_to_lock(&config, broken.path(), &mut go);
        assert_eq!(go.spec, "1.22");
    }
}
