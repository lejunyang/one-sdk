//! Active-version resolution for a working directory.
//!
//! Resolution order for a tool, walking up from the start dir:
//! 1. project config (`osdk.toml`) `[tools]` entry
//! 2. `.tool-versions` entry
//! 3. idiomatic version files (`.nvmrc`, `.node-version`, ...) — per backend
//! 4. user global config `[tools]` entry
//!
//! This module is intentionally synchronous and dependency-light so the
//! `osdk-shim` launcher can use it on the hot path without a tokio runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{parse_tool_versions, PROJECT_CONFIG_NAMES};

/// A resolved active version for a tool, plus where it came from.
#[derive(Debug, Clone)]
pub struct ActiveVersion {
    pub tool: String,
    /// The raw version spec string (e.g. "20", "lts", "20.11.1").
    pub spec: String,
    pub source: VersionOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionOrigin {
    ProjectConfig(PathBuf),
    ToolVersions(PathBuf),
    IdiomaticFile(PathBuf),
    GlobalConfig,
}

/// Resolve the active spec for `tool`, walking up from `start_dir`. Global
/// config pins are consulted last. `idiomatic` maps a tool to its idiomatic
/// filenames.
pub fn resolve_active(
    tool: &str,
    start_dir: &Path,
    global_tools: &BTreeMap<String, String>,
    idiomatic_files: &[&str],
) -> Option<ActiveVersion> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        // 1. project config
        for name in PROJECT_CONFIG_NAMES {
            let p = dir.join(name);
            if p.is_file() {
                if let Some(spec) = read_project_tool(&p, tool) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec,
                        source: VersionOrigin::ProjectConfig(p),
                    });
                }
            }
        }
        // 2. .tool-versions
        let tv = dir.join(".tool-versions");
        if tv.is_file() {
            if let Ok(text) = std::fs::read_to_string(&tv) {
                let map = parse_tool_versions(&text);
                if let Some(spec) = map.get(tool) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec: spec.clone(),
                        source: VersionOrigin::ToolVersions(tv),
                    });
                }
            }
        }
        // 3. idiomatic files
        for name in idiomatic_files {
            let p = dir.join(name);
            if p.is_file() {
                if let Some(spec) = read_idiomatic(&p) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec,
                        source: VersionOrigin::IdiomaticFile(p),
                    });
                }
            }
        }
        cur = dir.parent();
    }

    // 4. global config
    global_tools.get(tool).map(|spec| ActiveVersion {
        tool: tool.to_string(),
        spec: spec.clone(),
        source: VersionOrigin::GlobalConfig,
    })
}

fn read_project_tool(path: &Path, tool: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("tools")?
        .get(tool)?
        .as_str()
        .map(|s| s.to_string())
}

/// Read a simple idiomatic version file (`.nvmrc`, `.python-version`, ...).
/// Takes the first non-empty, non-comment line and trims a leading `v`.
fn read_idiomatic(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let v = line.trim_start_matches('v').trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn project_config_beats_global() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("osdk.toml")).unwrap();
        writeln!(f, "[tools]\nnode = \"20.11.1\"").unwrap();

        let mut global = BTreeMap::new();
        global.insert("node".to_string(), "18".to_string());

        let av = resolve_active("node", &dir, &global, &[".nvmrc"]).unwrap();
        assert_eq!(av.spec, "20.11.1");
        assert!(matches!(av.source, VersionOrigin::ProjectConfig(_)));
    }

    #[test]
    fn nvmrc_resolved_when_no_config() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".nvmrc"), "v20.11.1\n").unwrap();

        let global = BTreeMap::new();
        let av = resolve_active("node", &dir, &global, &[".nvmrc", ".node-version"]).unwrap();
        assert_eq!(av.spec, "20.11.1");
        assert!(matches!(av.source, VersionOrigin::IdiomaticFile(_)));
    }

    #[test]
    fn walks_up_to_parent() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join(".tool-versions"), "go 1.22.5\n").unwrap();
        let nested = td.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        let global = BTreeMap::new();
        let av = resolve_active("go", &nested, &global, &[]).unwrap();
        assert_eq!(av.spec, "1.22.5");
        assert!(matches!(av.source, VersionOrigin::ToolVersions(_)));
    }

    #[test]
    fn falls_back_to_global() {
        let td = tempfile::tempdir().unwrap();
        let mut global = BTreeMap::new();
        global.insert("node".to_string(), "18".to_string());
        let av = resolve_active("node", td.path(), &global, &[]).unwrap();
        assert_eq!(av.spec, "18");
        assert_eq!(av.source, VersionOrigin::GlobalConfig);
    }
}
