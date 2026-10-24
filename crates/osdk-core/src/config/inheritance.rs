//! Explicit configuration inheritance. Nesting alone grants no inheritance or trust.
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Resolve the `extends` chain for `path`.
///
/// `rel_base` is the directory relative paths inside the leaf config are
/// written against. For a checked-in config it is `None` (the file's own
/// directory); for an external project binding the config file lives outside
/// the project tree, so it is the recorded real project root.
pub fn sources(path: &Path, rel_base: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut chain = Vec::new();
    let mut next = path.canonicalize().map_err(|e| Error::io(path, e))?;
    // The repository boundary is anchored at the logical project root, not at
    // the physical config file.
    let boundary_from = rel_base.unwrap_or_else(|| next.parent().unwrap_or(&next));
    let boundary = boundary_from
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf);
    loop {
        let canonical = next.canonicalize().map_err(|e| Error::io(&next, e))?;
        if chain.contains(&canonical) {
            return Err(Error::config("config extends forms a cycle"));
        }
        if boundary
            .as_ref()
            .is_some_and(|root| !canonical.starts_with(root))
        {
            return Err(Error::config("config extends escapes repository boundary"));
        }
        let value = raw(&canonical)?;
        chain.push(canonical.clone());
        let Some(extends) = value.get("extends") else {
            break;
        };
        let extends = extends
            .as_str()
            .ok_or_else(|| Error::config("extends must name one ancestor config file"))?;
        // Relative extends resolve against the logical base, not the physical
        // file location.
        let base = if chain.len() == 1 {
            rel_base.unwrap_or_else(|| canonical.parent().unwrap())
        } else {
            canonical.parent().unwrap()
        };
        let target = base.join(extends.replace('\\', "/"));
        let target = target.canonicalize().map_err(|e| Error::io(&target, e))?;
        if target != canonical
            && (!base.starts_with(target.parent().unwrap()) || target.parent() == Some(base))
        {
            return Err(Error::config(
                "extends must refer to a config in a strict ancestor directory",
            ));
        }
        next = target;
        if chain.len() > 64 {
            return Err(Error::config("config extends exceeds 64 sources"));
        }
    }
    chain.reverse();
    Ok(chain)
}
pub fn raw(path: &Path) -> Result<toml::Value> {
    if !std::fs::symlink_metadata(path)
        .map_err(|e| Error::io(path, e))?
        .is_file()
    {
        return Err(Error::config("config source must be a regular file"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    toml::from_str(&text)
        .map_err(|e: toml::de::Error| Error::config(format!("{}: {}", path.display(), e.message())))
}
pub fn load(path: &Path, rel_base: Option<&Path>) -> Result<toml::Value> {
    let direct = raw(path)?;
    if direct.get("extends").is_none() {
        return Ok(direct);
    }
    let chain = sources(path, rel_base)?;
    let inherited = chain.len() > 1;
    let mut value = toml::Value::Table(Default::default());
    let anchor = chain[0].parent().unwrap().to_path_buf();
    let leaf = chain[chain.len() - 1].clone();
    for source in chain {
        let mut layer = raw(&source)?;
        layer.as_table_mut().unwrap().remove("extends");
        if inherited {
            if let Some(tools) = layer.get_mut("tools").and_then(toml::Value::as_table_mut) {
                for (_, entry) in tools.iter_mut() {
                    if let Some(version) = entry.as_str() {
                        *entry = toml::Value::Table(
                            [("version".into(), toml::Value::String(version.into()))]
                                .into_iter()
                                .collect(),
                        );
                    }
                }
            }
            // The leaf layer's relative paths are written against the logical
            // base (real project root), not the bundle's physical directory.
            let layer_base = if source == leaf {
                rel_base
                    .map(|b| b.to_path_buf())
                    .unwrap_or_else(|| source.parent().unwrap().to_path_buf())
            } else {
                source.parent().unwrap().to_path_buf()
            };
            rebase(&mut layer, &layer_base);
            if let Some(roots) = layer
                .get_mut("task")
                .and_then(|task| task.get_mut("roots"))
                .and_then(toml::Value::as_array_mut)
            {
                let prefix = source
                    .parent()
                    .unwrap()
                    .strip_prefix(&anchor)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if !prefix.is_empty() {
                    for root in roots {
                        if let Some(relative) = root.as_str() {
                            *root = toml::Value::String(format!("{prefix}/{relative}"));
                        }
                    }
                }
            }
        }
        merge(&mut value, layer);
    }
    Ok(value)
}
pub fn merge(base: &mut toml::Value, value: toml::Value) {
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
fn path_value(value: &mut toml::Value, base: &Path) {
    if let Some(path) = value.as_str() {
        if Path::new(path).is_absolute() {
            return;
        }
        let path = PathBuf::from(path.replace('\\', "/"));
        if !path.is_absolute() {
            *value = toml::Value::String(base.join(path).to_string_lossy().into_owned());
        }
    }
}
fn provider_paths(provider: &mut toml::Value, base: &Path) {
    if let Some(table) = provider.as_table_mut() {
        if let Some(dir) = table.get_mut("dir") {
            path_value(dir, base);
        }
        if let Some(run) = table.get_mut("run") {
            let mut argv = match run {
                toml::Value::String(text) => text
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
                toml::Value::Array(values) => values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
            if argv.first().is_some_and(|program| {
                program.contains(['/', '\\']) && !Path::new(program).is_absolute()
            }) {
                argv[0] = base
                    .join(argv[0].replace('\\', "/"))
                    .to_string_lossy()
                    .into_owned();
                *run = toml::Value::Array(argv.into_iter().map(toml::Value::String).collect());
            }
        }
        for name in ["sources", "outputs"] {
            if let Some(paths) = table.get_mut(name).and_then(toml::Value::as_array_mut) {
                for path in paths {
                    path_value(path, base);
                }
            }
        }
    }
}
pub fn rebase(value: &mut toml::Value, base: &Path) {
    if let Some(deps) = value.get_mut("deps").and_then(toml::Value::as_table_mut) {
        for (name, provider) in deps {
            if name == "projects" {
                if let Some(projects) = provider.as_table_mut() {
                    for project in projects
                        .iter_mut()
                        .map(|(_, value)| value)
                        .filter_map(toml::Value::as_table_mut)
                    {
                        for provider in project.iter_mut().map(|(_, value)| value) {
                            provider_paths(provider, base);
                        }
                    }
                }
            } else {
                provider_paths(provider, base);
            }
        }
    }
    if let Some(tasks) = value.get_mut("tasks").and_then(toml::Value::as_table_mut) {
        for task in tasks.iter_mut().map(|(_, value)| value) {
            if task.is_str() || task.is_array() {
                let run = task.clone();
                *task = toml::Value::Table([(String::from("run"), run)].into_iter().collect());
            }
            if let Some(table) = task.as_table_mut() {
                if !table.contains_key("dir")
                    && ["run", "lua", "file"]
                        .iter()
                        .any(|name| table.contains_key(*name))
                {
                    table.insert(
                        "dir".into(),
                        toml::Value::String(base.to_string_lossy().into_owned()),
                    );
                }
                for name in ["dir", "file"] {
                    if let Some(path) = table.get_mut(name) {
                        path_value(path, base);
                    }
                }
                for name in ["sources", "outputs"] {
                    if let Some(paths) = table.get_mut(name).and_then(toml::Value::as_array_mut) {
                        for path in paths {
                            path_value(path, base);
                        }
                    }
                }
            }
        }
    }
    if let Some(task) = value.get_mut("task").and_then(toml::Value::as_table_mut) {
        if let Some(dir) = task.get_mut("dir") {
            path_value(dir, base);
        }
        if let Some(paths) = task.get_mut("includes").and_then(toml::Value::as_array_mut) {
            for path in paths {
                path_value(path, base);
            }
        }
    }
}
pub fn deps_anchor(path: &Path, rel_base: Option<&Path>) -> Result<PathBuf> {
    let chain = sources(path, rel_base)?;
    for source in &chain {
        if raw(source)?.get("deps").is_some() {
            return Ok(source.clone());
        }
    }
    Ok(chain.last().unwrap().clone())
}

#[cfg(feature = "install")]
pub fn deps_file(path: &Path, rel_base: Option<&Path>) -> Result<super::DepsConfig> {
    match load(path, rel_base)?.get("deps") {
        Some(value) => value.clone().try_into().map_err(|e: toml::de::Error| {
            Error::config(format!("{}: {}", path.display(), e.message()))
        }),
        None => Ok(Default::default()),
    }
}

#[cfg(feature = "install")]
pub fn local_deps(
    path: &Path,
    anchor: &Path,
    defaults: &std::collections::BTreeMap<String, super::DepsProviderEntry>,
    default_disable: &[String],
    rel_base: Option<&Path>,
) -> Result<(
    std::collections::BTreeMap<String, super::DepsProviderEntry>,
    Vec<String>,
)> {
    let chain = sources(path, rel_base)?;
    let inherits = chain.iter().any(|source| source == anchor);
    let mut value = if inherits {
        toml::Value::try_from(defaults).map_err(|e| Error::config(e.to_string()))?
    } else {
        toml::Value::Table(Default::default())
    };
    if inherits {
        if let Some(providers) = value.as_table_mut() {
            for (_, provider) in providers.iter_mut() {
                provider_paths(provider, anchor.parent().unwrap());
            }
        }
    }
    let mut disable = if inherits {
        default_disable.to_vec()
    } else {
        Vec::new()
    };
    for source in chain.iter().filter(|source| source.as_path() != anchor) {
        let mut layer = raw(source)?;
        let layer_base = if source == chain.last().unwrap() {
            rel_base
                .map(|b| b.to_path_buf())
                .unwrap_or_else(|| source.parent().unwrap().to_path_buf())
        } else {
            source.parent().unwrap().to_path_buf()
        };
        rebase(&mut layer, &layer_base);
        if let Some(deps) = layer.get("deps").and_then(toml::Value::as_table) {
            if deps.contains_key("roots") || deps.contains_key("projects") {
                return Err(Error::config("a discovered child config must keep dependency discovery roots/projects in the catalog root"));
            }
            for (id, provider) in deps {
                if id == "disable" {
                    let list = provider
                        .as_array()
                        .ok_or_else(|| Error::config("deps.disable must be a list"))?;
                    disable = list
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| Error::config("deps.disable names must be strings"))
                        })
                        .collect::<Result<_>>()?;
                    continue;
                }
                if let Some(prior) = value.get_mut(id) {
                    merge(prior, provider.clone());
                } else {
                    value
                        .as_table_mut()
                        .unwrap()
                        .insert(id.clone(), provider.clone());
                }
            }
        }
    }
    let mut entries: std::collections::BTreeMap<String, super::DepsProviderEntry> = value
        .try_into()
        .map_err(|e: toml::de::Error| Error::config(e.message()))?;
    for disabled in &disable {
        entries.remove(disabled);
    }
    Ok((entries, disable))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_tool_object_inherits_a_shorthand_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("child")).unwrap();
        std::fs::write(root.join("osdk.toml"), "[tools]\nnode='24.19.0'\n").unwrap();
        std::fs::write(
            root.join("child/osdk.toml"),
            "extends='../osdk.toml'\n[tools.node]\nlazy=false\n",
        )
        .unwrap();
        let config =
            super::super::Config::load(&root.join("user-missing.toml"), &root.join("child"))
                .unwrap();
        assert_eq!(config.tools["node"], "24.19.0");
        assert!(!config.tool_configs["node"].is_lazy());
    }

    #[cfg(feature = "install")]
    #[test]
    fn inherited_empty_disable_clears_parent_disabled_providers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("child")).unwrap();
        std::fs::write(
            root.join("osdk.toml"),
            "[deps]\ndisable=['npm']\n[deps.npm]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("child/osdk.toml"),
            "extends='../osdk.toml'\n[deps]\ndisable=[]\n",
        )
        .unwrap();
        let anchor = root.join("osdk.toml").canonicalize().unwrap();
        let defaults = deps_file(&anchor, None).unwrap().providers;
        let (entries, disabled) = local_deps(
            &root.join("child/osdk.toml"),
            &anchor,
            &defaults,
            &["npm".into()],
            None,
        )
        .unwrap();
        assert!(disabled.is_empty());
        assert!(entries.contains_key("npm"));
    }

    #[test]
    fn explicit_chain_keeps_false_empty_env_and_source_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(root.join("osdk.toml"),"[deps.uv]\nauto=true\nsources=['pyproject.toml']\noutputs=['.venv']\nenv={A='1',B='1'}").unwrap();
        std::fs::write(
            child.join("osdk.toml"),
            "extends='../osdk.toml'\n[deps.uv]\nauto=false\noutputs=[]\nenv={B='2'}",
        )
        .unwrap();
        let value = load(&child.join("osdk.toml"), None).unwrap();
        assert_eq!(value["deps"]["uv"]["auto"].as_bool(), Some(false));
        assert!(value["deps"]["uv"]["outputs"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(value["deps"]["uv"]["env"]["A"].as_str(), Some("1"));
        assert_eq!(value["deps"]["uv"]["env"]["B"].as_str(), Some("2"));
        assert_eq!(
            value["deps"]["uv"]["sources"][0].as_str(),
            Some(
                root.canonicalize()
                    .unwrap()
                    .join("pyproject.toml")
                    .to_str()
                    .unwrap()
            )
        );
        std::fs::write(child.join("osdk.toml"), "extends='osdk.toml'").unwrap();
        assert!(load(&child.join("osdk.toml"), None)
            .unwrap_err()
            .to_string()
            .contains("cycle"));
        std::fs::write(child.join("osdk.toml"), "extends='../other/osdk.toml'").unwrap();
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(root.join("other/osdk.toml"), "").unwrap();
        assert!(load(&child.join("osdk.toml"), None).is_err());
    }
}
