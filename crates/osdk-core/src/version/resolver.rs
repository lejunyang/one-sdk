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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManagerRequest {
    pub manager: String,
    pub version: String,
    pub source: PathBuf,
}

/// Read the package-manager declaration from one specific `package.json`.
///
/// Unlike [`resolve_package_manager`], this helper does not walk ancestors and
/// does not restrict the manager name. Callers that treat the nearest manifest
/// as a hard project boundary can therefore inspect that exact declaration and
/// apply their own supported-manager policy. `packageManager` wins over
/// `devEngines.packageManager`, matching Node/Corepack conventions.
pub fn package_manager_from_package_json(
    path: &Path,
) -> Result<Option<PackageManagerRequest>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        crate::t!(
            "err.package_manager_manifest_read",
            path = path.display(),
            error = error
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        crate::t!(
            "err.package_manager_manifest_parse",
            path = path.display(),
            error = error
        )
    })?;

    if let Some(value) = value.get("packageManager") {
        let raw = value
            .as_str()
            .ok_or_else(|| crate::t!("err.package_manager_field_type", path = path.display()))?;
        return parse_package_manager_declaration(raw, path.to_path_buf()).map(Some);
    }

    let Some(package_manager) = value
        .get("devEngines")
        .and_then(|value| value.get("packageManager"))
    else {
        return Ok(None);
    };
    let package_manager = if let Some(items) = package_manager.as_array() {
        items.first().ok_or_else(|| {
            crate::t!(
                "err.package_manager_dev_engines_empty",
                path = path.display()
            )
        })?
    } else {
        package_manager
    };
    let manager = package_manager
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| crate::t!("err.package_manager_name_missing", path = path.display()))?;
    let version = package_manager
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| crate::t!("err.package_manager_version_missing", path = path.display()))?;
    parse_package_manager_declaration_parts(manager, version, path.to_path_buf()).map(Some)
}

pub fn resolve_package_manager(start_dir: &Path) -> Result<Option<PackageManagerRequest>, String> {
    for directory in start_dir.ancestors() {
        for name in PROJECT_CONFIG_NAMES {
            let path = directory.join(name);
            if path.is_file() {
                if let Some((manager, version)) = read_project_package_manager(&path) {
                    return configured_package_manager(&manager, &version, path).map(Some);
                }
            }
        }
    }
    for directory in start_dir.ancestors() {
        let path = directory.join("package.json");
        if !path.is_file() {
            continue;
        }
        if let Some(request) = package_manager_from_package_json(&path)? {
            return validate_supported_package_manager(request).map(Some);
        }
    }
    Ok(None)
}

fn read_project_package_manager(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let tools = value.get("tools")?;
    for manager in ["npm", "pnpm", "yarn"] {
        if let Some(value) = tools.get(manager) {
            if let Some(version) = read_tool_value(value) {
                return Some((manager.into(), version));
            }
        }
    }
    None
}

fn parse_package_manager_declaration(
    raw: &str,
    source: PathBuf,
) -> Result<PackageManagerRequest, String> {
    let Some((manager, version)) = raw.split_once('@') else {
        return Err(crate::t!(
            "err.package_manager_declaration_invalid",
            path = source.display()
        ));
    };
    parse_package_manager_declaration_parts(manager, version, source)
}

fn configured_package_manager(
    manager: &str,
    version: &str,
    source: PathBuf,
) -> Result<PackageManagerRequest, String> {
    let manager = manager.trim();
    let version = version.trim();
    validate_package_manager_name(manager, &source)?;
    if version.is_empty() {
        return Err(crate::t!(
            "err.package_manager_version_missing",
            path = source.display()
        ));
    }
    Ok(PackageManagerRequest {
        manager: manager.to_ascii_lowercase(),
        version: version.to_string(),
        source,
    })
}

fn validate_supported_package_manager(
    request: PackageManagerRequest,
) -> Result<PackageManagerRequest, String> {
    if matches!(request.manager.as_str(), "npm" | "pnpm" | "yarn") {
        Ok(request)
    } else {
        Err(crate::t!(
            "err.package_manager_unsupported",
            path = request.source.display(),
            manager = request.manager
        ))
    }
}

fn validate_package_manager_name(manager: &str, source: &Path) -> Result<(), String> {
    if manager.is_empty()
        || manager.contains(char::is_whitespace)
        || manager.contains(['/', '\\', '#', '+'])
    {
        return Err(crate::t!(
            "err.package_manager_name_invalid",
            path = source.display(),
            manager = manager
        ));
    }
    Ok(())
}

fn parse_package_manager_declaration_parts(
    manager: &str,
    version: &str,
    source: PathBuf,
) -> Result<PackageManagerRequest, String> {
    let manager = manager.trim();
    let version = version.trim();
    validate_package_manager_name(manager, &source)?;
    if version.contains(['#', '+', '/', '\\']) || semver::Version::parse(version).is_err() {
        return Err(crate::t!(
            "err.package_manager_version_not_exact",
            path = source.display(),
            manager = manager,
            version = version
        ));
    }
    Ok(PackageManagerRequest {
        manager: manager.to_ascii_lowercase(),
        version: version.to_string(),
        source,
    })
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
    if matches!(tool, "npm" | "pnpm" | "yarn") {
        if let Ok(Some(package_manager)) = resolve_package_manager(start_dir) {
            if package_manager.manager == tool {
                return Some(ActiveVersion {
                    tool: tool.into(),
                    spec: package_manager.version,
                    source: VersionOrigin::ProjectMetadata(package_manager.source),
                    is_range: false,
                });
            }
        }
    }
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

/// Apply one shared policy to tool selection. Explicit CLI requests and lock
/// replay bypass this resolver and retain their authority.
pub fn resolve_active_config(
    tool: &str,
    start_dir: &Path,
    config: &crate::config::Config,
    idiomatic_files: &[&str],
) -> crate::Result<Option<ActiveVersion>> {
    use crate::config::{SourcePrecedence, ToolConfigOrigin};
    let native = native_version(tool, start_dir, idiomatic_files)?;
    let configured = config.tools.get(tool).map(|spec| ActiveVersion {
        tool: tool.into(),
        spec: spec.clone(),
        is_range: false,
        source: match config.tool_origins.get(tool) {
            Some(ToolConfigOrigin::ProjectConfig(path)) => {
                VersionOrigin::ProjectConfig(path.clone())
            }
            Some(ToolConfigOrigin::ToolVersions(path)) => VersionOrigin::ToolVersions(path.clone()),
            _ => VersionOrigin::GlobalConfig,
        },
    });
    let project = configured
        .as_ref()
        .is_some_and(|v| matches!(v.source, VersionOrigin::ProjectConfig(_)));
    let winner = if config.settings.precedence.tools == SourcePrecedence::Osdk && project {
        configured
    } else {
        native.or(configured)
    };
    if let Some(selected) = &winner {
        tracing::debug!(tool, source = ?selected.source, policy = ?config.settings.precedence.tools,
            "tool selection: explicit command and lock selection take precedence; native and osdk declarations ordered by policy");
    }
    Ok(winner)
}

fn native_version(
    tool: &str,
    start_dir: &Path,
    files: &[&str],
) -> crate::Result<Option<ActiveVersion>> {
    use crate::error::Error;
    let make = |spec: String, source, is_range| {
        Some(ActiveVersion {
            tool: tool.into(),
            spec,
            source,
            is_range,
        })
    };
    for dir in start_dir.ancestors() {
        let path = dir.join(".tool-versions");
        if path.is_file() {
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            for line in text.lines() {
                let mut words = line.split('#').next().unwrap_or("").split_whitespace();
                if words.next() == Some(tool) && words.next().is_none() {
                    return Err(Error::config(format!(
                        "missing version for {tool} in {}",
                        path.display()
                    )));
                }
            }
            if let Some(spec) = parse_tool_versions(&text).get(tool) {
                return Ok(make(spec.clone(), VersionOrigin::ToolVersions(path), false));
            }
        }
    }
    for name in files {
        for dir in start_dir.ancestors() {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            if *name == "rust-toolchain.toml" {
                let value: toml::Value = toml::from_str(&text)
                    .map_err(|_| Error::config("invalid rust-toolchain.toml"))?;
                if value
                    .get("toolchain")
                    .and_then(|v| v.get("channel"))
                    .and_then(toml::Value::as_str)
                    .is_none()
                {
                    return Err(Error::config("rust-toolchain.toml needs toolchain.channel"));
                }
            }
            if let Some(spec) = read_idiomatic(&path) {
                if spec.contains(char::is_whitespace) {
                    return Err(Error::config(format!(
                        "invalid native version file {}",
                        path.display()
                    )));
                }
                return Ok(make(spec, VersionOrigin::IdiomaticFile(path), false));
            }
            if *name != "go.mod" {
                return Err(Error::config(format!(
                    "empty native version file {}",
                    path.display()
                )));
            }
        }
    }
    if matches!(tool, "node" | "npm" | "pnpm" | "yarn") {
        for dir in start_dir.ancestors() {
            let path = dir.join("package.json");
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|_| Error::config("invalid native package.json"))?;
            if tool == "node" {
                if value
                    .get("engines")
                    .and_then(|v| v.get("node"))
                    .is_some_and(|v| !v.is_string())
                {
                    return Err(Error::config(
                        "package.json engines.node must be a version string",
                    ));
                }
                if let Some(spec) = read_node_package(&path) {
                    crate::version::VersionSpec::parse_range(&spec)?;
                    return Ok(make(spec, VersionOrigin::ProjectMetadata(path), true));
                }
            } else if let Some(manager) =
                package_manager_from_package_json(&path).map_err(Error::config)?
            {
                if manager.manager == tool {
                    return Ok(make(
                        manager.version,
                        VersionOrigin::ProjectMetadata(path),
                        false,
                    ));
                }
            }
        }
    }
    Ok(None)
}

fn read_project_tool(path: &Path, tool: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    read_tool_value(value.get("tools")?.get(tool)?)
}

fn read_tool_value(value: &toml::Value) -> Option<String> {
    value.as_str().map(str::to_string).or_else(|| {
        value
            .get("version")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    })
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
    fn package_manager_field_and_dev_engines_parse_exact_versions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.15.0","devEngines":{"packageManager":{"name":"yarn","version":"4.10.3"}}}"#,
        )
        .unwrap();
        let selected = resolve_package_manager(temp.path()).unwrap().unwrap();
        assert_eq!(selected.manager, "pnpm");
        assert_eq!(selected.version, "9.15.0");

        std::fs::write(
            temp.path().join("package.json"),
            r#"{"devEngines":{"packageManager":{"name":"yarn","version":"4.10.3"}}}"#,
        )
        .unwrap();
        let selected = resolve_package_manager(temp.path()).unwrap().unwrap();
        assert_eq!(selected.manager, "yarn");
        assert_eq!(selected.version, "4.10.3");
    }

    #[test]
    fn exact_package_json_helper_stays_at_one_manifest_and_preserves_unknown_manager() {
        let temporary = tempfile::tempdir().unwrap();
        let package = temporary.path().join("package.json");
        std::fs::write(&package, r#"{"packageManager":"bun@1.2.3"}"#).unwrap();

        let selected = package_manager_from_package_json(&package)
            .unwrap()
            .unwrap();
        assert_eq!(selected.manager, "bun");
        assert_eq!(selected.version, "1.2.3");
        assert_eq!(selected.source, package);
    }

    #[test]
    fn project_package_manager_uses_normal_tool_version_selectors() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("osdk.toml"), "[tools]\npnpm = \"9\"\n").unwrap();

        let selected = resolve_package_manager(temp.path()).unwrap().unwrap();
        assert_eq!(selected.manager, "pnpm");
        assert_eq!(selected.version, "9");

        std::fs::write(
            temp.path().join("osdk.toml"),
            "[tools]\nnpm = { version = \"11.5\" }\n",
        )
        .unwrap();
        let selected = resolve_package_manager(temp.path()).unwrap().unwrap();
        assert_eq!(selected.manager, "npm");
        assert_eq!(selected.version, "11.5");

        std::fs::remove_file(temp.path().join("osdk.toml")).unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"packageManager":"pnpm@9"}"#,
        )
        .unwrap();
        assert!(resolve_package_manager(temp.path()).is_err());
    }

    #[test]
    fn project_tools_beat_package_manager_and_invalid_values_fail() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("osdk.toml"), "[tools]\nnpm = \"11.5.2\"\n").unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.15.0"}"#,
        )
        .unwrap();
        let selected = resolve_package_manager(temp.path()).unwrap().unwrap();
        assert_eq!(selected.manager, "npm");
        assert_eq!(selected.version, "11.5.2");

        std::fs::remove_file(temp.path().join("osdk.toml")).unwrap();
        for value in [
            "pnpm",
            "bun@1.2.3",
            "npm@https://example.test/npm.tgz",
            "yarn@4.0.0+sha",
        ] {
            std::fs::write(
                temp.path().join("package.json"),
                format!(r#"{{"packageManager":"{value}"}}"#),
            )
            .unwrap();
            assert!(resolve_package_manager(temp.path()).is_err(), "{value}");
        }
    }

    #[test]
    fn package_manager_errors_render_in_chinese() {
        let invalid = crate::i18n::interpolate(
            &crate::i18n::trl(
                crate::i18n::Lang::Zh,
                "err.package_manager_version_not_exact",
            ),
            &[
                ("path", "/repo/package.json"),
                ("manager", "npm"),
                ("version", "latest"),
            ],
        );
        assert_eq!(
            invalid,
            "/repo/package.json 中的包管理器 `npm` 必须使用不含 URL/hash 后缀的精确 semver：`latest`"
        );
    }

    #[test]
    fn structured_project_tool_version_is_resolved() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("osdk.toml"),
            "[tools]\nnode = { version = \"20.11.1\", engine = \"node\" }\nnpm = { version = \"11.5.2\", allow_builds = [\"esbuild\"] }\n",
        )
        .unwrap();

        let global = BTreeMap::new();
        let active = resolve_active("node", &dir, &global, &[".nvmrc"]).unwrap();
        assert_eq!(active.spec, "20.11.1");

        let package_manager = resolve_package_manager(&dir).unwrap().unwrap();
        assert_eq!(package_manager.manager, "npm");
        assert_eq!(package_manager.version, "11.5.2");
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

#[cfg(test)]
mod precedence_tests {
    use super::*;
    use crate::config::{Config, SourcePrecedence};
    #[test]
    fn precedence_preserves_origins_and_explicit_inheritance() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let child = root.join("app");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            root.join("osdk.toml"),
            "[tools]\nnode='22'\n[settings.precedence]\ntools='native'\n",
        )
        .unwrap();
        std::fs::write(child.join("osdk.toml"), "extends='../osdk.toml'\n").unwrap();
        std::fs::write(child.join(".nvmrc"), "20.11.1\n").unwrap();
        let mut config = Config::load(&temp.path().join("absent"), &child).unwrap();
        let native = resolve_active_config("node", &child, &config, &[".nvmrc"])
            .unwrap()
            .unwrap();
        assert_eq!(native.spec, "20.11.1");
        assert!(matches!(native.source, VersionOrigin::IdiomaticFile(_)));
        config.settings.precedence.tools = SourcePrecedence::Osdk;
        let osdk = resolve_active_config("node", &child, &config, &[".nvmrc"])
            .unwrap()
            .unwrap();
        assert_eq!(osdk.spec, "22");
        assert!(matches!(osdk.source, VersionOrigin::ProjectConfig(_)));
    }
    #[test]
    fn node_runtime_and_package_manager_are_separate_native_candidates() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"engines":{"node":">=20 <23"},"packageManager":"pnpm@10.5.0"}"#,
        )
        .unwrap();
        let config = Config::default();
        let node = resolve_active_config("node", temp.path(), &config, &[])
            .unwrap()
            .unwrap();
        assert!(node.is_range);
        assert_eq!(node.spec, ">=20 <23");
        let pnpm = resolve_active_config("pnpm", temp.path(), &config, &[])
            .unwrap()
            .unwrap();
        assert_eq!(pnpm.spec, "10.5.0");
        assert!(!pnpm.is_range);
    }
    #[test]
    fn invalid_native_declarations_are_errors_under_both_policies() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".nvmrc"), "two versions\n").unwrap();
        let mut config = Config::default();
        for policy in [SourcePrecedence::Native, SourcePrecedence::Osdk] {
            config.settings.precedence.tools = policy;
            assert!(resolve_active_config("node", temp.path(), &config, &[".nvmrc"]).is_err());
        }
        std::fs::remove_file(temp.path().join(".nvmrc")).unwrap();
        std::fs::write(temp.path().join("package.json"), "{invalid").unwrap();
        assert!(resolve_active_config("node", temp.path(), &config, &[]).is_err());
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"engines":{"node":22}}"#,
        )
        .unwrap();
        assert!(resolve_active_config("node", temp.path(), &config, &[]).is_err());
        std::fs::remove_file(temp.path().join("package.json")).unwrap();
        std::fs::write(temp.path().join(".tool-versions"), "node # missing version").unwrap();
        assert!(resolve_active_config("node", temp.path(), &config, &[]).is_err());
    }
}

pub fn explain_selection(
    tool: &str,
    cwd: &Path,
    config: &crate::config::Config,
    files: &[&str],
) -> crate::Result<String> {
    let native = native_version(tool, cwd, files)?;
    let selected = resolve_active_config(tool, cwd, config, files)?;
    Ok(format!("tools={:?}; selected={:?}; osdk declaration={:?}; native declaration={:?}; explicit CLI and lock replay remain authoritative",
        config.settings.precedence.tools,
        selected.as_ref().map(|v| (&v.spec, &v.source)),
        config.tools.get(tool),
        native.as_ref().map(|v| (&v.spec, &v.source))))
}
