//! Shared candidate collection. Source kinds are ranked before directory depth;
//! only the nearest declaration of each kind participates in conflict checking.
use super::{ActiveVersion, VersionOrigin};
use crate::config::{Config, ToolConfigOrigin, ToolConflictPolicy, ToolSource};
use crate::error::{Error, Result};
use crate::version::VersionSpec;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub active: ActiveVersion,
    pub kind: ToolSource,
    pub path: Option<PathBuf>,
    pub field: String,
    pub shadowed_layer: bool,
    pub ignored: bool,
}
impl Candidate {
    pub fn location(&self) -> String {
        format!(
            "{}#{}",
            self.path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "config".into()),
            self.field
        )
    }
}

pub fn candidates(
    tool: &str,
    cwd: &Path,
    config: &Config,
    files: &[&str],
) -> Result<Vec<Candidate>> {
    collect(tool, cwd, config, files, false)
}
pub(super) fn manager_candidates(
    tool: &str,
    cwd: &Path,
    config: &Config,
) -> Result<Vec<Candidate>> {
    collect(tool, cwd, config, &[], true)
}
fn collect(
    tool: &str,
    cwd: &Path,
    config: &Config,
    files: &[&str],
    manager_selection: bool,
) -> Result<Vec<Candidate>> {
    use ToolSource::*;
    let mut out = Vec::new();
    if let Some(spec) = config.tools.get(tool) {
        let (kind, path, source) = match config.tool_origins.get(tool) {
            Some(ToolConfigOrigin::ProjectConfig(p)) => (
                Osdk,
                Some(p.clone()),
                VersionOrigin::ProjectConfig(p.clone()),
            ),
            Some(ToolConfigOrigin::GlobalConfig(p)) => {
                (Global, Some(p.clone()), VersionOrigin::GlobalConfig)
            }
            Some(ToolConfigOrigin::ToolVersions(_)) => {
                (ToolVersions, None, VersionOrigin::GlobalConfig)
            }
            None => (Global, None, VersionOrigin::GlobalConfig),
        };
        if kind != ToolVersions {
            push(
                &mut out,
                tool,
                spec,
                kind,
                path,
                format!(
                    "tools.{tool}{}",
                    if matches!(
                        config.tool_configs.get(tool),
                        Some(crate::config::ToolConfigEntry::Structured(_))
                    ) {
                        ".version"
                    } else {
                        ""
                    }
                ),
                source,
                false,
            )?;
        }
    }
    // Preserve overridden osdk layers as evidence, without treating intentional
    // extends/user overrides as simultaneous constraints.
    if let Some(leaf) = &config.project_config_path {
        for path in crate::config::inheritance::sources(leaf)?.into_iter().rev() {
            if let Some(value) = crate::config::inheritance::raw(&path)?
                .get("tools")
                .and_then(|v| v.get(tool))
            {
                if (value.is_str() || value.get("version").is_some())
                    && !out.iter().any(|c| {
                        c.kind == Osdk
                            && c.path.as_ref().and_then(|p| p.canonicalize().ok()).as_ref()
                                == Some(&path)
                    })
                {
                    // This layer was already replaced by the effective config.
                    // Retain its location, never validate or copy its old value.
                    out.push(Candidate {
                        active: ActiveVersion {
                            tool: tool.into(),
                            spec: String::new(),
                            source: VersionOrigin::ProjectConfig(path.clone()),
                            is_range: false,
                        },
                        kind: Osdk,
                        path: Some(path),
                        field: format!(
                            "tools.{tool}{}",
                            if value.is_table() { ".version" } else { "" }
                        ),
                        shadowed_layer: true,
                        ignored: false,
                    });
                }
            }
        }
    }
    if let Some(spec) = config.global_tools.get(tool) {
        if !out.iter().any(|c| c.kind == Global) {
            push(
                &mut out,
                tool,
                spec,
                Global,
                config.global_config_path.clone(),
                format!(
                    "tools.{tool}{}",
                    if matches!(
                        config.global_tool_configs.get(tool),
                        Some(crate::config::ToolConfigEntry::Structured(_))
                    ) {
                        ".version"
                    } else {
                        ""
                    }
                ),
                VersionOrigin::GlobalConfig,
                false,
            )?;
        }
    }
    for dir in cwd.ancestors() {
        let path = dir.join(".tool-versions");
        if path.is_file() {
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            for (line, text) in text.lines().enumerate() {
                let words: Vec<_> = text
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect();
                if words.first() == Some(&tool) {
                    if shadowed(
                        &mut out,
                        tool,
                        ToolVersions,
                        &path,
                        &format!("line {}: {tool}", line + 1),
                    ) {
                        continue;
                    }
                    if words.len() != 2 {
                        return Err(invalid(
                            &path,
                            &format!("line {}: {tool} (expected one version)", line + 1),
                        ));
                    }
                    push(
                        &mut out,
                        tool,
                        words[1],
                        ToolVersions,
                        Some(path.clone()),
                        format!("line {}: {tool}", line + 1),
                        VersionOrigin::ToolVersions(path.clone()),
                        false,
                    )?;
                }
            }
        }
    }
    for name in files {
        let kind = match *name {
            ".nvmrc" => Nvmrc,
            ".node-version" => NodeVersion,
            _ => Idiomatic,
        };
        for dir in cwd.ancestors() {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            if shadowed(&mut out, tool, kind, &path, "version declaration") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            let field = if *name == "rust-toolchain.toml"
                || (*name == "rust-toolchain" && text.trim_start().starts_with('['))
            {
                let value: toml::Value =
                    toml::from_str(&text).map_err(|_| invalid(&path, "toolchain.channel"))?;
                if value
                    .get("toolchain")
                    .and_then(|v| v.get("channel"))
                    .and_then(toml::Value::as_str)
                    .is_none()
                {
                    return Err(invalid(&path, "toolchain.channel"));
                }
                "toolchain.channel".to_string()
            } else if *name == "go.mod" {
                "go".into()
            } else {
                let lines: Vec<_> = text
                    .lines()
                    .enumerate()
                    .filter(|(_, s)| !s.split('#').next().unwrap_or("").trim().is_empty())
                    .collect();
                if lines.len() != 1 {
                    return Err(invalid(&path, "expected one version line"));
                }
                format!("line {}", lines[0].0 + 1)
            };
            if let Some(spec) = super::read_idiomatic(&path) {
                if spec.contains(char::is_whitespace) {
                    return Err(invalid(&path, &field));
                }
                push(
                    &mut out,
                    tool,
                    &spec,
                    kind,
                    Some(path.clone()),
                    field,
                    VersionOrigin::IdiomaticFile(path),
                    false,
                )?;
            } else if *name != "go.mod" {
                return Err(invalid(&path, &field));
            }
        }
    }
    if matches!(tool, "node" | "npm" | "pnpm" | "yarn") {
        for dir in cwd.ancestors() {
            let path = dir.join("package.json");
            if !path.is_file() {
                continue;
            }
            let kinds = if tool == "node" {
                [Engines, DevRuntime]
            } else {
                [PackageManager, DevPackageManager]
            };
            if kinds.iter().all(|kind| out.iter().any(|c| c.kind == *kind)) {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|_| invalid(&path, "JSON"))?;
            if tool == "node" {
                if let Some(v) = value
                    .get("engines")
                    .and_then(|v| v.get("node"))
                    .filter(|_| !shadowed(&mut out, tool, Engines, &path, "engines.node"))
                {
                    json_candidate(&mut out, tool, v, Engines, &path, "engines.node", true)?;
                }
                if let Some(v) = value
                    .get("devEngines")
                    .and_then(|v| v.get("runtime"))
                    .filter(|_| !shadowed(&mut out, tool, DevRuntime, &path, "devEngines.runtime"))
                {
                    for (item, field) in entries(v, "devEngines.runtime") {
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| invalid(&path, &format!("{field}.name")))?;
                        if name == "node" {
                            json_candidate(
                                &mut out,
                                tool,
                                item.get("version")
                                    .ok_or_else(|| invalid(&path, &format!("{field}.version")))?,
                                DevRuntime,
                                &path,
                                &format!("{field}.version"),
                                true,
                            )?;
                        } else {
                            ignored(&mut out, tool, DevRuntime, &path, &field);
                        }
                    }
                }
            } else {
                if let Some(v) = value
                    .get("packageManager")
                    .filter(|_| !shadowed(&mut out, tool, PackageManager, &path, "packageManager"))
                {
                    let raw = v.as_str().ok_or_else(|| invalid(&path, "packageManager"))?;
                    let (manager, version) = raw
                        .split_once('@')
                        .ok_or_else(|| invalid(&path, "packageManager"))?;
                    if manager_selection && !matches!(manager, "npm" | "pnpm" | "yarn") {
                        return Err(invalid(
                            &path,
                            "packageManager (unsupported manager; choose an explicit provider)",
                        ));
                    }
                    if manager == tool {
                        // Validate only this tool's declaration. Another manager's
                        // integrity/URL semantics belong to that manager.
                        if semver::Version::parse(version).is_err() || version.contains('+') {
                            return Err(invalid(
                                &path,
                                "packageManager (expected exact semver without hash)",
                            ));
                        }
                        push(
                            &mut out,
                            tool,
                            version,
                            PackageManager,
                            Some(path.clone()),
                            "packageManager".into(),
                            VersionOrigin::ProjectMetadata(path.clone()),
                            false,
                        )?;
                    } else {
                        ignored(&mut out, tool, PackageManager, &path, "packageManager");
                    }
                }
                if let Some(v) = value
                    .get("devEngines")
                    .and_then(|v| v.get("packageManager"))
                    .filter(|_| {
                        !shadowed(
                            &mut out,
                            tool,
                            DevPackageManager,
                            &path,
                            "devEngines.packageManager",
                        )
                    })
                {
                    if v.as_array().is_some_and(|a| {
                        a.len() != 1
                            && a.iter()
                                .any(|item| item.get("name").and_then(|v| v.as_str()) == Some(tool))
                    }) {
                        return Err(invalid(
                            &path,
                            "devEngines.packageManager (alternatives are not supported)",
                        ));
                    }
                    for (item, field) in entries(v, "devEngines.packageManager") {
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| invalid(&path, &format!("{field}.name")))?;
                        if manager_selection && !matches!(name, "npm" | "pnpm" | "yarn") {
                            return Err(invalid(&path, &format!("{field}.name (unsupported manager; choose an explicit provider)")));
                        }
                        if name == tool {
                            json_candidate(
                                &mut out,
                                tool,
                                item.get("version")
                                    .ok_or_else(|| invalid(&path, &format!("{field}.version")))?,
                                DevPackageManager,
                                &path,
                                &format!("{field}.version"),
                                true,
                            )?;
                        } else {
                            ignored(&mut out, tool, DevPackageManager, &path, &field);
                        }
                    }
                }
            }
        }
    }
    let order = config.settings.precedence.ordered_tool_sources();
    out.sort_by_key(|c| order.iter().position(|k| *k == c.kind).unwrap());
    // Within idiomatic, preserve backend file order. Within every other kind,
    // collection order is nearest first. Older declarations remain evidence.
    for i in 0..out.len() {
        if out[..i]
            .iter()
            .any(|c| c.kind == out[i].kind && (!c.ignored || c.path != out[i].path))
        {
            out[i].shadowed_layer = true;
        }
    }
    if config.settings.precedence.tool_conflicts == ToolConflictPolicy::Error {
        let eligible: Vec<_> = out
            .iter()
            .filter(|c| !c.shadowed_layer && !c.ignored)
            .collect();
        for (i, a) in eligible.iter().enumerate() {
            for b in &eligible[i + 1..] {
                match compatible(config, tool, &a.active, &b.active)? {
                    Some(true) => {}
                    result => {
                        let reason = if result.is_some() {
                            "incompatible requirements"
                        } else {
                            "unverifiable aliases/channels without catalogue metadata"
                        };
                        return Err(Error::config(format!("{reason} for {tool}: {} and {}; choose tool_conflicts=prefer to select by order", a.location(), b.location())));
                    }
                }
            }
        }
    }
    Ok(out)
}
// Once a kind has a nearer declaration, retain the older location without
// validating its inactive contents.
fn shadowed(
    out: &mut Vec<Candidate>,
    tool: &str,
    kind: ToolSource,
    path: &Path,
    field: &str,
) -> bool {
    if !out.iter().any(|c| c.kind == kind) {
        return false;
    }
    out.push(Candidate {
        active: ActiveVersion {
            tool: tool.into(),
            spec: String::new(),
            source: VersionOrigin::IdiomaticFile(path.into()),
            is_range: false,
        },
        kind,
        path: Some(path.into()),
        field: field.into(),
        shadowed_layer: true,
        ignored: false,
    });
    true
}
// Evidence for declarations of another tool, without interpreting their values.
fn ignored(out: &mut Vec<Candidate>, tool: &str, kind: ToolSource, path: &Path, field: &str) {
    out.push(Candidate {
        active: ActiveVersion {
            tool: tool.into(),
            spec: String::new(),
            source: VersionOrigin::ProjectMetadata(path.into()),
            is_range: false,
        },
        kind,
        path: Some(path.into()),
        field: field.into(),
        shadowed_layer: false,
        ignored: true,
    });
}

fn invalid(path: &Path, field: &str) -> Error {
    Error::config(format!(
        "invalid tool declaration at {}#{field}",
        path.display()
    ))
}
fn entries<'a>(value: &'a serde_json::Value, field: &str) -> Vec<(&'a serde_json::Value, String)> {
    match value.as_array() {
        Some(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (v, format!("{field}[{i}]")))
            .collect(),
        None => vec![(value, field.into())],
    }
}
fn json_candidate(
    out: &mut Vec<Candidate>,
    tool: &str,
    value: &serde_json::Value,
    kind: ToolSource,
    path: &Path,
    field: &str,
    range: bool,
) -> Result<()> {
    let spec = value.as_str().ok_or_else(|| invalid(path, field))?;
    push(
        out,
        tool,
        spec,
        kind,
        Some(path.into()),
        field.into(),
        VersionOrigin::ProjectMetadata(path.into()),
        range,
    )
}
#[allow(clippy::too_many_arguments)]
fn push(
    out: &mut Vec<Candidate>,
    tool: &str,
    spec: &str,
    kind: ToolSource,
    path: Option<PathBuf>,
    field: String,
    source: VersionOrigin,
    is_range: bool,
) -> Result<()> {
    if spec.trim().is_empty() || spec.chars().any(char::is_control) || spec.contains("://") {
        return Err(Error::config(format!(
            "invalid tool declaration at {}#{field}",
            path.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )));
    }
    if is_range {
        VersionSpec::parse_range(spec).map_err(|_| {
            Error::config(format!(
                "invalid semver range at {}#{field}",
                path.as_ref().unwrap().display()
            ))
        })?;
    }
    out.push(Candidate {
        active: ActiveVersion {
            tool: tool.into(),
            spec: spec.trim().into(),
            source,
            is_range,
        },
        kind,
        path,
        field,
        shadowed_layer: false,
        ignored: false,
    });
    Ok(())
}

/// Prove overlap using semver boundaries, not string inequality. Opaque channels
/// need catalogue metadata: only identical expanded selectors can be proven here.
fn compatible(
    config: &Config,
    tool: &str,
    a: &ActiveVersion,
    b: &ActiveVersion,
) -> Result<Option<bool>> {
    let expand = |v: &ActiveVersion| -> Result<VersionSpec> {
        let text = config
            .expand_alias(tool, &v.spec)
            .map_err(|_| Error::config("invalid/cyclic tool alias in precedence comparison"))?;
        if v.is_range {
            VersionSpec::parse_range(&text)
        } else {
            Ok(VersionSpec::parse(&text))
        }
    };
    let a = expand(a)?;
    let b = expand(b)?;
    if a == b {
        return Ok(Some(true));
    }
    fn requirements(spec: &VersionSpec) -> Option<Vec<semver::VersionReq>> {
        use VersionSpec::*;
        let value = match spec {
            Exact(v) | Pinned(v) => format!("={v}"),
            Prefix(v)
                if v.split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) =>
            {
                format!("{v}.*")
            }
            Range(v) => v.clone(),

            _ => return None,
        };
        super::super::npm_range_requirements(&value).ok()
    }
    let (Some(a), Some(b)) = (requirements(&a), requirements(&b)) else {
        return Ok(None);
    };
    let mut probes = vec![semver::Version::new(0, 0, 0)];
    for r in a.iter().chain(&b) {
        for c in &r.comparators {
            let v = semver::Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
            probes.push(v.clone());
            if let Some(p) = v.patch.checked_add(1) {
                probes.push(semver::Version::new(v.major, v.minor, p));
            }
            if let Some(m) = v.minor.checked_add(1) {
                probes.push(semver::Version::new(v.major, m, 0));
            }
            if let Some(m) = v.major.checked_add(1) {
                probes.push(semver::Version::new(m, 0, 0));
            }
            if !c.pre.is_empty() {
                let mut pre = v;
                pre.pre = c.pre.clone();
                probes.push(pre.clone());
                pre.pre = semver::Prerelease::new(&format!("{}.0", c.pre))
                    .expect("valid prerelease successor");
                probes.push(pre);
            }
        }
    }
    Ok(Some(probes.iter().any(|v| {
        a.iter().any(|r| r.matches(v)) && b.iter().any(|r| r.matches(v))
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PrecedenceSettings, SourcePrecedence};
    use crate::version::resolver::{resolve_package_manager_config, resolve_selection};
    fn write(root: &Path, path: &str, text: &str) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn select(root: &Path, config: &Config) -> Result<super::super::ToolSelection> {
        resolve_selection("node", root, config, &[".nvmrc", ".node-version"])
    }
    #[test]
    fn custom_order_records_all_fields_and_shadowed_ancestors() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        write(root, ".nvmrc", "20.1.0");
        write(root, "child/.nvmrc", "22.1.0");
        write(root, ".node-version", "24.1.0");
        write(
            root,
            "child/package.json",
            r#"{"engines":{"node":">=22 <25"},"devEngines":{"runtime":{"name":"node","version":"^24.0.0"}}}"#,
        );
        let mut c = Config::default();
        c.settings.precedence.tool_order = vec![ToolSource::NodeVersion];
        let s = select(&root.join("child"), &c).unwrap();
        assert_eq!(s.winner.unwrap().spec, "24.1.0");
        assert_eq!(s.evidence.len(), 5);
        assert_eq!(s.evidence[0].status, "winner");
        assert!(s
            .evidence
            .iter()
            .any(|e| e.field == "devEngines.runtime.version"));
        assert!(s.evidence.iter().any(
            |e| e.path.as_deref() == Some(&root.join(".nvmrc")) && e.reason.contains("nearer")
        ));
    }
    #[test]
    fn malformed_lower_priority_candidate_is_not_silently_lost() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".nvmrc", "22");
        write(t.path(), ".node-version", "two versions");
        assert!(select(t.path(), &Config::default()).is_err());
        write(t.path(), ".node-version", "22");
        write(
            t.path(),
            "package.json",
            r#"{"devEngines":{"runtime":{"name":"node","version":false}}}"#,
        );
        assert!(select(t.path(), &Config::default()).is_err());
    }
    #[test]
    fn conflicts_use_ranges_and_aliases_not_text_inequality() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".nvmrc", "22.1.0");
        write(
            t.path(),
            "package.json",
            r#"{"engines":{"node":">=20 <23"}}"#,
        );
        let mut c = Config::default();
        c.settings.precedence.tool_conflicts = ToolConflictPolicy::Error;
        assert!(select(t.path(), &c).is_ok());
        write(t.path(), ".node-version", "team");
        c.tool_aliases
            .insert("node".into(), [("team".into(), "22.1.0".into())].into());
        assert!(select(t.path(), &c).is_ok());
        write(t.path(), ".node-version", "24");
        assert!(select(t.path(), &c).is_err());
        write(t.path(), ".node-version", "lts/iron");
        assert!(select(t.path(), &c).is_err());
    }
    #[test]
    fn range_overlap_handles_prefix_bounds_and_unions() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.settings.precedence.tool_conflicts = ToolConflictPolicy::Error;
        for (a, b, ok) in [
            ("^22.0.0", ">=22.5 <23", true),
            ("^22.0.0", ">=23", false),
            ("20 || 22", ">=21 <23", true),
            ("~22.1.0", ">=22.2", false),
        ] {
            write(
                t.path(),
                "package.json",
                &format!(
                    r#"{{"engines":{{"node":"{a}"}},"devEngines":{{"runtime":{{"name":"node","version":"{b}"}}}}}}"#
                ),
            );
            assert_eq!(select(t.path(), &c).is_ok(), ok, "{a} vs {b}");
        }
    }
    #[test]
    fn order_validation_and_clear() {
        for text in [
            r#"tool_order=["wat"]"#,
            r#"tool_order=[".nvmrc",".nvmrc"]"#,
            r#"tool_conflicts="ignore""#,
        ] {
            assert!(toml::from_str::<PrecedenceSettings>(text).is_err());
        }
        let p: PrecedenceSettings = toml::from_str("tool_order=[]\ntools='native'").unwrap();
        assert_eq!(p.ordered_tool_sources()[0], ToolSource::ToolVersions);
    }
    #[test]
    fn project_inherits_user_policy_and_extends_can_clear_order() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(
            p,
            "user.toml",
            "[settings.precedence]\ntool_order=['.node-version']\ntool_conflicts='error'\n",
        );
        write(p, "project/osdk.toml", "[settings]\njobs=2\n");
        let c = Config::load(&p.join("user.toml"), &p.join("project")).unwrap();
        assert_eq!(
            c.settings.precedence.tool_order,
            vec![ToolSource::NodeVersion]
        );
        write(
            p,
            "project/child/osdk.toml",
            "extends='../osdk.toml'\n[settings.precedence]\ntool_order=[]\ntools='native'\n",
        );
        let c = Config::load(&p.join("user.toml"), &p.join("project/child")).unwrap();
        assert!(c.settings.precedence.tool_order.is_empty());
        assert_eq!(
            c.settings.precedence.tool_conflicts,
            ToolConflictPolicy::Error
        );
    }
    #[test]
    fn osdk_inheritance_preserves_shadowed_locations() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(p, "osdk.toml", "[tools]\nnode='20'\n");
        write(
            p,
            "child/osdk.toml",
            "extends='../osdk.toml'\n[tools]\nnode='22'\n",
        );
        let c = Config::load(&p.join("missing"), &p.join("child")).unwrap();
        let s = select(&p.join("child"), &c).unwrap();
        assert_eq!(s.winner.unwrap().spec, "22");
        assert_eq!(s.evidence.len(), 2);
        assert_eq!(s.evidence[1].status, "shadowed");
    }
    #[test]
    fn overridden_osdk_layers_keep_locations_without_validating_or_copying_values() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(p, "child/osdk.toml", "extends='../osdk.toml'\n[tools]\nnode='22.1.0'\n[settings.precedence]\ntool_conflicts='error'\n");
        for declaration in [
            "node=''",
            "node='https://user:SECRET@private.invalid/version'",
            "node={version='https://user:SECRET@private.invalid/version'}",
            "node=\"SECRET\\n\"",
        ] {
            write(p, "osdk.toml", &format!("[tools]\n{declaration}\n"));
            let c = Config::load(&p.join("missing"), &p.join("child")).unwrap();
            let selection = select(&p.join("child"), &c).unwrap();
            assert_eq!(selection.winner.as_ref().unwrap().spec, "22.1.0");
            let parent = p.join("osdk.toml").canonicalize().unwrap();
            let evidence = selection
                .evidence
                .iter()
                .find(|e| e.path.as_ref() == Some(&parent))
                .unwrap();
            assert_eq!(evidence.status, "shadowed");
            assert_eq!(
                evidence.field,
                if declaration.contains('{') {
                    "tools.node.version"
                } else {
                    "tools.node"
                }
            );
            assert!(!selection.explain().contains("SECRET"));
            let candidates = candidates("node", &p.join("child"), &c, &[]).unwrap();
            assert!(candidates
                .iter()
                .filter(|c| c.shadowed_layer)
                .all(|c| c.active.spec.is_empty()));
        }
    }
    #[test]
    fn manager_fields_use_same_custom_order_as_active_version() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path();
        write(
            p,
            "package.json",
            r#"{"packageManager":"pnpm@9.1.0","devEngines":{"packageManager":{"name":"yarn","version":"4.1.0"}}}"#,
        );
        let mut c = Config::default();
        c.settings.precedence.tool_order = vec![ToolSource::DevPackageManager];
        assert_eq!(
            resolve_package_manager_config(p, &c)
                .unwrap()
                .unwrap()
                .manager,
            "yarn"
        );
        let s = resolve_selection("yarn", p, &c, &[]).unwrap();
        assert_eq!(s.evidence[0].field, "devEngines.packageManager.version");
        c.settings.precedence.tool_conflicts = ToolConflictPolicy::Error;
        assert!(resolve_package_manager_config(p, &c).is_err());
    }
    #[test]
    fn inactive_ancestor_versions_are_evidence_without_validation() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".nvmrc", "not a valid version file");
        write(t.path(), "child/.nvmrc", "22.1.0");
        write(t.path(), "package.json", r#"{"packageManager":false}"#);
        write(
            t.path(),
            "child/package.json",
            r#"{"packageManager":"pnpm@9.1.0"}"#,
        );
        let s = select(&t.path().join("child"), &Config::default()).unwrap();
        assert_eq!(s.winner.unwrap().spec, "22.1.0");
        assert!(
            s.evidence
                .iter()
                .any(|e| e.status == "shadowed"
                    && e.path.as_deref() == Some(&t.path().join(".nvmrc")))
        );
        assert_eq!(
            resolve_package_manager_config(&t.path().join("child"), &Config::default())
                .unwrap()
                .unwrap()
                .manager,
            "pnpm"
        );
    }
    #[test]
    fn unrelated_manager_values_are_ignored_with_evidence() {
        let t = tempfile::tempdir().unwrap();
        write(
            t.path(),
            "package.json",
            r#"{"packageManager":"yarn@https://SECRET.invalid/archive","devEngines":{"packageManager":{"name":"bun","version":false}}}"#,
        );
        let s = resolve_selection("npm", t.path(), &Config::default(), &[]).unwrap();
        assert!(s.winner.is_none());
        assert_eq!(s.evidence.len(), 2);
        assert!(s.evidence.iter().all(|e| e.status == "ignored"));
        assert!(!s.explain().contains("SECRET"));
        assert!(resolve_selection("yarn", t.path(), &Config::default(), &[]).is_err());
    }
    #[test]
    fn nearest_manager_identity_shadows_a_different_parent_identity() {
        let t = tempfile::tempdir().unwrap();
        write(
            t.path(),
            "package.json",
            r#"{"packageManager":"pnpm@9.1.0"}"#,
        );
        write(
            t.path(),
            "child/package.json",
            r#"{"packageManager":"yarn@4.1.0"}"#,
        );
        let mut c = Config::default();
        c.settings.precedence.tool_conflicts = ToolConflictPolicy::Error;
        assert_eq!(
            resolve_package_manager_config(&t.path().join("child"), &c)
                .unwrap()
                .unwrap()
                .manager,
            "yarn"
        );
    }
    #[test]
    fn prerelease_open_bounds_and_opaque_latest_are_not_misclassified() {
        let c = Config::default();
        let version = |s: &str, is_range| ActiveVersion {
            tool: "node".into(),
            spec: s.into(),
            source: VersionOrigin::GlobalConfig,
            is_range,
        };
        assert_eq!(
            compatible(
                &c,
                "node",
                &version(">1.0.0-alpha <1.0.0-beta", true),
                &version(">1.0.0-alpha <1.0.0-alpha.1", true)
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            compatible(
                &c,
                "node",
                &version("latest", false),
                &version("22.1.0", false)
            )
            .unwrap(),
            None
        );
    }
    #[test]
    fn package_policy_overrides_are_independent() {
        let p: PrecedenceSettings = toml::from_str("packages='native'\nnpm='osdk'").unwrap();
        assert_eq!(p.package_policy("npm"), SourcePrecedence::Osdk);
        assert_eq!(p.package_policy("python"), SourcePrecedence::Native);
        let p: PrecedenceSettings = toml::from_str("packages='osdk'\nnpm='inherit'").unwrap();
        assert_eq!(p.package_policy("npm"), SourcePrecedence::Osdk);
        assert!(toml::from_str::<PrecedenceSettings>("python='unknown'").is_err());
    }
    #[test]
    fn explanation_does_not_echo_untrusted_versions() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".nvmrc", "SECRET_CHANNEL");
        assert!(!select(t.path(), &Config::default())
            .unwrap()
            .explain()
            .contains("SECRET_CHANNEL"));
        write(t.path(), ".nvmrc", "https://user:SECRET@host");
        let e = select(t.path(), &Config::default())
            .unwrap_err()
            .to_string();
        assert!(!e.contains("SECRET"));
    }
}
