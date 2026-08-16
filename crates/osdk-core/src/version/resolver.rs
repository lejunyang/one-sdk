//! Active-version resolution for a working directory.
//!
//! Resolution order for a tool, walking up from the start dir:
//! 1. project config (`osdk.toml`) `[tools]` entry
//! 2. `.tool-versions` entry
//! 3. idiomatic version files (`.nvmrc`, `.node-version`, ...) — per backend
//! 4. structured project metadata (`package.json` for Node)
//! 5. user global config `[tools]` entry
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
    pub is_range: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionOrigin {
    ProjectConfig(PathBuf),
    ToolVersions(PathBuf),
    IdiomaticFile(PathBuf),
    ProjectMetadata(PathBuf),
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
    let ancestors: Vec<&Path> = start_dir.ancestors().collect();

    // 1. project config
    for dir in &ancestors {
        for name in PROJECT_CONFIG_NAMES {
            let p = dir.join(name);
            if p.is_file() {
                if let Some(spec) = read_project_tool(&p, tool) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec,
                        source: VersionOrigin::ProjectConfig(p),
                        is_range: false,
                    });
                }
            }
        }
    }

    // 2. .tool-versions
    for dir in &ancestors {
        let tv = dir.join(".tool-versions");
        if tv.is_file() {
            if let Ok(text) = std::fs::read_to_string(&tv) {
                let map = parse_tool_versions(&text);
                if let Some(spec) = map.get(tool) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec: spec.clone(),
                        source: VersionOrigin::ToolVersions(tv),
                        is_range: false,
                    });
                }
            }
        }
    }

    // 3. idiomatic files, preserving each backend's declared priority.
    for name in idiomatic_files {
        for dir in &ancestors {
            let p = dir.join(name);
            if p.is_file() {
                if let Some(spec) = read_idiomatic(&p) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec,
                        source: VersionOrigin::IdiomaticFile(p),
                        is_range: false,
                    });
                }
            }
        }
    }

    // 4. structured project metadata.
    if tool == "node" {
        for dir in &ancestors {
            let package = dir.join("package.json");
            if package.is_file() {
                if let Some(spec) = read_node_package(&package) {
                    return Some(ActiveVersion {
                        tool: tool.to_string(),
                        spec,
                        source: VersionOrigin::ProjectMetadata(package),
                        is_range: true,
                    });
                }
            }
        }
    }

    // 5. global config
    global_tools.get(tool).map(|spec| ActiveVersion {
        tool: tool.to_string(),
        spec: spec.clone(),
        source: VersionOrigin::GlobalConfig,
        is_range: false,
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
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let text = std::fs::read_to_string(path).ok()?;

    // Structured files need real parsing, not first-line heuristics.
    match name {
        // rust-toolchain.toml: [toolchain] channel = "1.79.0" | "stable"
        "rust-toolchain.toml" => {
            if let Ok(v) = toml::from_str::<toml::Value>(&text) {
                if let Some(ch) = v
                    .get("toolchain")
                    .and_then(|t| t.get("channel"))
                    .and_then(|c| c.as_str())
                {
                    return Some(ch.to_string());
                }
            }
            // legacy `rust-toolchain` may itself be TOML or a bare string;
            // fall through to plain handling.
        }
        // legacy `rust-toolchain` (no extension): a bare channel string, but
        // could also be TOML. Try TOML first.
        "rust-toolchain" => {
            if let Ok(v) = toml::from_str::<toml::Value>(&text) {
                if let Some(ch) = v
                    .get("toolchain")
                    .and_then(|t| t.get("channel"))
                    .and_then(|c| c.as_str())
                {
                    return Some(ch.to_string());
                }
            }
        }
        // go.mod: the `go 1.22` directive.
        "go.mod" => {
            for line in text.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("go ") {
                    let v = rest.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
            return None;
        }
        _ => {}
    }

    // Plain single-value files (.nvmrc, .python-version, .java-version, ...).
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

fn read_node_package(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("engines")
        .and_then(|engines| engines.get("node"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            let runtime = value.get("devEngines")?.get("runtime")?;
            let runtime = runtime
                .as_array()
                .and_then(|items| {
                    items.iter().find(|item| {
                        item.get("name").and_then(serde_json::Value::as_str) == Some("node")
                    })
                })
                .unwrap_or(runtime);
            let name = runtime
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("node");
            if name != "node" {
                return None;
            }
            runtime
                .get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
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
    fn node_package_engines_and_dev_engines_are_resolved_last() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"engines":{"node":">=20 <23"},"devEngines":{"runtime":{"name":"node","version":"^22.0.0"}}}"#,
        )
        .unwrap();

        let global = BTreeMap::new();
        let active = resolve_active("node", &dir, &global, &[".nvmrc", ".node-version"]).unwrap();
        assert_eq!(active.spec, ">=20 <23");
        assert!(matches!(active.source, VersionOrigin::ProjectMetadata(_)));

        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":"^22.0.0"}}}"#,
        )
        .unwrap();
        let active = resolve_active("node", &dir, &global, &[".nvmrc", ".node-version"]).unwrap();
        assert_eq!(active.spec, "^22.0.0");
    }

    #[test]
    fn node_version_files_beat_package_json_and_invalid_ranges_are_preserved_for_validation() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join(".node-version"), "21.7.3\n").unwrap();
        std::fs::write(
            td.path().join("package.json"),
            r#"{"engines":{"node":"definitely-not-semver"}}"#,
        )
        .unwrap();
        let global = BTreeMap::new();
        let active =
            resolve_active("node", td.path(), &global, &[".nvmrc", ".node-version"]).unwrap();
        assert_eq!(active.spec, "21.7.3");

        std::fs::remove_file(td.path().join(".node-version")).unwrap();
        let active =
            resolve_active("node", td.path(), &global, &[".nvmrc", ".node-version"]).unwrap();
        assert_eq!(active.spec, "definitely-not-semver");
        assert!(active.is_range);
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
    fn higher_priority_parent_file_beats_lower_priority_child_file() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("osdk.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let nested = td.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".nvmrc"), "20\n").unwrap();
        std::fs::write(nested.join("package.json"), r#"{"engines":{"node":"18"}}"#).unwrap();

        let active = resolve_active(
            "node",
            &nested,
            &BTreeMap::new(),
            &[".nvmrc", ".node-version"],
        )
        .unwrap();
        assert_eq!(active.spec, "22");
        assert!(matches!(active.source, VersionOrigin::ProjectConfig(_)));
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

    #[test]
    fn rust_toolchain_toml_channel_parsed() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.79.0\"\ncomponents = [\"clippy\"]\n",
        )
        .unwrap();
        let global = BTreeMap::new();
        let av = resolve_active("rust", td.path(), &global, &["rust-toolchain.toml"]).unwrap();
        assert_eq!(av.spec, "1.79.0");
    }

    #[test]
    fn go_mod_directive_parsed() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("go.mod"),
            "module example.com/x\n\ngo 1.22\n\nrequire foo v1.0.0\n",
        )
        .unwrap();
        let global = BTreeMap::new();
        let av = resolve_active("go", td.path(), &global, &["go.mod"]).unwrap();
        assert_eq!(av.spec, "1.22");
    }
}
