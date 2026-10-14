//! Bounded project discovery and ownership, before CLI selection.
use super::*;
use crate::config::{DepsConfig, DepsProviderEntry};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProjectEntry {
    pub relative: String,
    pub source: String,
    pub project: DetectedProject,
    pub config: ProviderConfig,
    pub unit: Option<super::workspace::WorkspaceUnit>,
}
impl ProjectEntry {
    pub fn id(&self) -> String {
        format!("//{}:{}", self.relative, self.project.provider)
    }
}

impl TryFrom<&DepsProviderEntry> for ProviderConfig {
    type Error = Error;
    fn try_from(entry: &DepsProviderEntry) -> Result<Self> {
        if entry.extra_index.is_some() {
            return Err(Error::config("deps.extra_index is unsupported; use native registry configuration; uv supports native named indexes and package bindings in pyproject.toml"));
        }
        let timeout = entry
            .timeout
            .as_deref()
            .map(|text| {
                crate::tasks::tree::parse_duration(text)
                    .ok_or_else(|| Error::config("invalid deps timeout"))
            })
            .transpose()?;
        Ok(Self {
            auto: entry.auto,
            sources: entry.sources.clone().unwrap_or_default(),
            sources_explicit: entry.sources.is_some(),
            outputs: entry.outputs.clone().unwrap_or_default(),
            outputs_explicit: entry.outputs.is_some(),
            run: entry.run.clone(),
            timeout,
            env: entry.env.clone(),
            dir: entry.dir.clone(),
            depends: entry.depends.clone(),
            installer: entry.installer.clone(),
            index: entry.index.clone(),
            allow_build_from_source: entry.allow_build_from_source,
        })
    }
}

/// The root configuration supplies defaults, while project declarations override
/// individual fields. Arrays replace, env merges, and false remains an override.
pub fn project_providers(
    defaults: &DepsConfig,
    declarations: &[&crate::config::DepsProject],
) -> Result<BTreeMap<String, DepsProviderEntry>> {
    let mut entries = defaults.providers.clone();
    let mut overrides: BTreeMap<String, toml::Value> = BTreeMap::new();
    for declaration in declarations {
        for (id, value) in &declaration.providers {
            if let Some(prior) = overrides.insert(id.clone(), value.clone()) {
                if prior != *value {
                    return Err(Error::config(format!(
                        "overlapping deps.projects declarations conflict for `{id}`"
                    )));
                }
            }
        }
    }
    for (id, value) in overrides {
        let mut base = toml::Value::try_from(entries.get(&id).cloned().unwrap_or_default())
            .map_err(|e| Error::config(e.to_string()))?;
        merge(&mut base, value);
        let entry = base.try_into().map_err(|e: toml::de::Error| {
            Error::config(format!("deps.projects.{id}: {}", e.message()))
        })?;
        entries.insert(id, entry);
    }
    for disabled in defaults
        .disable
        .iter()
        .chain(declarations.iter().flat_map(|d| &d.disable))
    {
        entries.remove(disabled);
    }
    Ok(entries)
}

fn merge(base: &mut toml::Value, value: toml::Value) {
    match (base, value) {
        (toml::Value::Table(base), toml::Value::Table(value)) => {
            for (key, value) in value {
                if let Some(prior) = base.get_mut(&key) {
                    merge(prior, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, value) => *base = value,
    }
}

pub fn catalog(root: &Path, config_path: &Path, config: &DepsConfig) -> Result<Vec<ProjectEntry>> {
    let root = root.canonicalize().map_err(|e| Error::io(root, e))?;
    let mut directories: BTreeMap<PathBuf, (String, String)> = BTreeMap::new();
    directories.insert(root.clone(), (String::new(), "configuration root".into()));
    let patterns: Vec<String> = config
        .roots
        .iter()
        .chain(config.projects.keys())
        .cloned()
        .filter(|p| p != ".")
        .collect();
    for expanded in expand_roots(&root, &patterns, "deps roots/projects")? {
        let canonical = expanded
            .directory
            .canonicalize()
            .map_err(|e| Error::io(&expanded.directory, e))?;
        if !canonical.starts_with(&root) {
            return Err(Error::config("deps project escapes configuration root"));
        }
        directories
            .entry(canonical)
            .or_insert((expanded.relative, expanded.pattern));
    }
    // Compatibility for a provider's declared nested directory: discovery and
    // execution agree about the manifest it owns.
    for entry in config.providers.values() {
        if let Some(dir) = &entry.dir {
            let path = effective_cwd(&root, Some(dir));
            let canonical = path.canonicalize().map_err(|e| Error::io(&path, e))?;
            if !canonical.starts_with(&root) {
                return Err(Error::config("deps.dir escapes configuration root"));
            }
            let relative = canonical
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            directories
                .entry(canonical)
                .or_insert((relative, "provider dir".into()));
        }
    }
    let mut units = Vec::new();
    for directory in directories.keys() {
        units.extend(super::workspace::detect(directory)?);
    }
    for unit in &units {
        for member in &unit.members {
            let relative = member
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            directories.entry(member.clone()).or_insert((
                relative,
                format!("native {} workspace {}", unit.provider, unit.root.display()),
            ));
        }
    }
    let mut result = Vec::new();
    for (directory, (relative, source)) in directories {
        let declarations: Vec<_> = config
            .projects
            .iter()
            .filter(|(pattern, _)| path_matches(pattern, &relative))
            .map(|(_, d)| d)
            .collect();
        let mut defaults = config.clone();
        for unit in units
            .iter()
            .filter(|unit| unit.members.contains(&directory))
        {
            let relative = unit
                .root
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let root_declarations: Vec<_> = config
                .projects
                .iter()
                .filter(|(pattern, _)| path_matches(pattern, &relative))
                .map(|(_, d)| d)
                .collect();
            let root_entries = project_providers(config, &root_declarations)?;
            if let Some(entry) = root_entries.get(&unit.provider) {
                defaults
                    .providers
                    .insert(unit.provider.clone(), entry.clone());
            }
        }
        let entries = project_providers(&defaults, &declarations)?;
        let configs: BTreeMap<String, ProviderConfig> = entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .dir
                    .as_deref()
                    .is_none_or(|dir| effective_cwd(&root, Some(dir)) == directory)
            })
            .map(|(id, entry)| {
                let mut config = ProviderConfig::try_from(entry)?;
                if entry.dir.is_some() {
                    config.dir = None;
                }
                Ok((id.clone(), config))
            })
            .collect::<Result<_>>()?;
        let mut projects = owned_in(&directory, &configs)?;
        for unit in units
            .iter()
            .filter(|unit| unit.root == directory && unit.provider == "pnpm")
        {
            if configs.contains_key("pnpm") && !projects.iter().any(|p| p.provider == "pnpm") {
                projects.push(DetectedProject {
                    provider: "pnpm".into(),
                    ecosystem: Ecosystem::Node,
                    root: directory.clone(),
                    manifest: unit.definition.clone(),
                    native_lock: directory
                        .join("pnpm-lock.yaml")
                        .is_file()
                        .then(|| directory.join("pnpm-lock.yaml")),
                    declared_manager: None,
                });
            }
        }
        for project in projects {
            let matches: Vec<_> = units
                .iter()
                .filter(|unit| {
                    unit.provider == project.provider
                        && (unit.root == directory || unit.members.contains(&directory))
                })
                .collect();
            if matches.len() > 1 {
                return Err(Error::config(
                    "overlapping native workspaces claim the same project",
                ));
            }
            let unit = matches.first().map(|unit| (*unit).clone());
            if project.ecosystem == Ecosystem::Node
                && units
                    .iter()
                    .any(|unit| unit.provider == "pnpm" && unit.members.contains(&directory))
                && project.provider != "pnpm"
            {
                return Err(Error::config(
                    "pnpm workspace member has a conflicting package manager",
                ));
            }
            if let Some(unit) = &unit {
                if unit.root != directory && project.native_lock.is_some() {
                    return Err(Error::config("workspace member has an independent lockfile; remove it or exclude the project from the native workspace"));
                }
            }
            let config = configs.get(&*project.provider).unwrap().clone();
            let choice = select_installer(&project, &config, &[])?;
            let _ = choice; // Validate even when CLI filters omit this project.
            result.push(ProjectEntry {
                relative: relative.clone(),
                source: source.clone(),
                project,
                config,
                unit,
            });
        }
        for (id, config) in &configs {
            if provider_schema(id).is_none()
                && (directory == root || declarations.iter().any(|d| d.providers.contains_key(id)))
            {
                result.push(ProjectEntry {
                    relative: relative.clone(),
                    source: source.clone(),
                    project: detect_custom(id, &directory, config_path),
                    config: config.clone(),
                    unit: None,
                });
            }
        }
    }
    for unit in &units {
        let entries: Vec<_> = result
            .iter()
            .filter(|entry| entry.unit.as_ref() == Some(unit))
            .collect();
        if let Some(first) = entries.first() {
            if entries.iter().any(|entry| entry.config != first.config)
                || entries.len() != unit.members.len() + 1
            {
                return Err(Error::config(format!("{}: shared {} workspace requires identical provider options and enabled members; auto/disable/run/index conflicts cannot be honored by one installation", unit.root.display(), unit.provider)));
            }
        }
    }
    result.sort_by_key(ProjectEntry::id);
    Ok(result)
}

/// Lock and manifest ownership must be determined using all known providers,
/// including disabled ones. A filter must never hide a contradictory lockfile.
pub fn owned_in(
    directory: &Path,
    configs: &BTreeMap<String, ProviderConfig>,
) -> Result<Vec<DetectedProject>> {
    let all = discover(directory, Some(directory), PROVIDERS)?;
    let mut result = Vec::new();
    for ecosystem in [
        Ecosystem::Node,
        Ecosystem::Python,
        Ecosystem::Go,
        Ecosystem::Rust,
        Ecosystem::Deno,
    ] {
        let peers: Vec<_> = all.iter().filter(|p| p.ecosystem == ecosystem).collect();
        let locked: Vec<_> = peers.iter().filter(|p| p.native_lock.is_some()).collect();
        if locked.len() > 1 {
            return Err(Error::config(format!(
                "{} has conflicting lockfiles for {}",
                directory.display(),
                locked
                    .iter()
                    .map(|p| &*p.provider)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let declared = peers.iter().find_map(|p| p.declared_manager.as_ref());
        if let (Some(declared), Some(locked)) = (declared, locked.first()) {
            if declared.name != locked.provider {
                return Err(Error::config(format!(
                    "{} declares `{}` but its lockfile belongs to `{}`",
                    directory.display(),
                    declared.name,
                    locked.provider
                )));
            }
        }
        let owner = declared
            .map(|d| d.name.as_str())
            .or_else(|| locked.first().map(|p| &*p.provider));
        let eligible: Vec<_> = peers
            .iter()
            .filter(|p| {
                configs.contains_key(&*p.provider)
                    && owner.is_none_or(|owner| owner == &*p.provider)
            })
            .collect();
        if eligible.len() > 1 {
            return Err(Error::config(format!("{} has ambiguous {} providers; declare packageManager, retain one lockfile, or select one provider per project", directory.display(), ecosystem.as_str())));
        }
        if let Some(project) = eligible.first() {
            result.push((***project).clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    #[test]
    fn mixed_managers_route_once_without_child_configs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "apps/a/package.json",
            r#"{"packageManager":"pnpm@10"}"#,
        );
        write(
            root,
            "apps/b/package.json",
            r#"{"packageManager":"npm@11"}"#,
        );
        write(root, "apps/c/requirements.txt", "idna==3.10");
        let config: DepsConfig=toml::from_str("roots=['apps/*','apps/a']\n[pnpm]\n[npm]\n[projects.'apps/c'.pip-requirements]\nauto=false\n").unwrap();
        let found = catalog(root, &root.join("osdk.toml"), &config).unwrap();
        assert_eq!(
            found.iter().map(ProjectEntry::id).collect::<Vec<_>>(),
            ["//apps/a:pnpm", "//apps/b:npm", "//apps/c:pip-requirements"]
        );
        assert!(!found[2].config.auto);
        write(root, "apps/a/package-lock.json", "{}");
        assert!(catalog(root, &root.join("osdk.toml"), &config).is_err());
    }
    #[test]
    fn overrides_keep_false_and_empty_and_merge_env() {
        let config: DepsConfig=toml::from_str("[uv]\nenv={A='1',B='1'}\n[projects.'api'.uv]\nauto=false\nsources=[]\noutputs=[]\nenv={B='2'}\n").unwrap();
        let entries = project_providers(&config, &[&config.projects["api"]]).unwrap();
        let cfg = ProviderConfig::try_from(&entries["uv"]).unwrap();
        assert!(!cfg.auto);
        assert!(cfg.sources_explicit);
        assert!(cfg.outputs_explicit);
        assert_eq!(cfg.env["A"], "1");
        assert_eq!(cfg.env["B"], "2");
    }
}
