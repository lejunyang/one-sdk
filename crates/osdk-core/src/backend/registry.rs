//! Backend registry: maps tool ids / aliases to backend instances.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dirs::Dirs;
use crate::error::{Error, Result};

use super::Backend;

pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
    by_name: HashMap<String, usize>,
}

impl Registry {
    /// Build the registry with all compiled-in backends.
    pub fn new() -> Registry {
        Self::from_backends(vec![
            Arc::new(crate::backend::node::NodeBackend),
            Arc::new(crate::backend::go::GoBackend),
            Arc::new(crate::backend::python::PythonBackend),
            Arc::new(crate::backend::java::JavaBackend),
            Arc::new(crate::backend::jvm_tools::JvmToolBackend::Maven),
            Arc::new(crate::backend::jvm_tools::JvmToolBackend::Gradle),
            Arc::new(crate::backend::jvm_tools::JvmToolBackend::Kotlin),
            Arc::new(crate::backend::rust::RustBackend),
            Arc::new(crate::backend::pnpm::PnpmBackend),
            Arc::new(crate::backend::yarn::YarnBackend),
            Arc::new(crate::backend::deno::DenoBackend),
            Arc::new(crate::backend::bun::BunBackend),
        ])
        .expect("compiled-in backend ids and aliases must be unique")
    }

    /// Build the registry with compiled-in backends plus schema-1 TOML
    /// definitions from `<config>/plugins` and `<data>/plugins`.
    ///
    /// Config definitions load first. Duplicate ids, aliases, or definitions
    /// are rejected rather than allowing an external backend to shadow another.
    pub fn load(dirs: &Dirs) -> Result<Registry> {
        let mut backends = Self::new().backends;
        for directory in [dirs.config.join("plugins"), dirs.plugins()] {
            backends.extend(
                crate::backend::declarative::load_dir(&directory)?
                    .into_iter()
                    .map(|backend| Arc::new(backend) as Arc<dyn Backend>),
            );
        }
        Self::from_backends(backends)
    }

    fn from_backends(backends: Vec<Arc<dyn Backend>>) -> Result<Registry> {
        let mut by_name = HashMap::new();
        for (i, b) in backends.iter().enumerate() {
            insert_name(&mut by_name, b.id(), i)?;
            for alias in b.aliases() {
                insert_name(&mut by_name, alias, i)?;
            }
        }
        Ok(Registry { backends, by_name })
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Backend>> {
        // Dynamic namespaced backends: `github:owner/repo`.
        if name.starts_with("github:") {
            if let Some(gh) = crate::backend::github::GithubBackend::from_id(name) {
                return Ok(Arc::new(gh));
            }
            return Err(Error::UnknownBackend(name.to_string()));
        }
        self.by_name
            .get(name)
            .map(|&i| self.backends[i].clone())
            .ok_or_else(|| Error::UnknownBackend(name.to_string()))
    }

    pub fn all(&self) -> &[Arc<dyn Backend>] {
        &self.backends
    }

    pub fn ids(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.id()).collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

fn insert_name(by_name: &mut HashMap<String, usize>, name: &str, index: usize) -> Result<()> {
    if by_name.insert(name.to_string(), index).is_some() {
        return Err(Error::config(format!(
            "duplicate backend id or alias `{name}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_external_backend_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let plugins = dirs.config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let fixture = include_str!("../../tests/fixtures/declarative/static-backend.toml");
        std::fs::write(
            plugins.join("node.toml"),
            fixture.replace("id = \"acme\"", "id = \"node\""),
        )
        .unwrap();

        let error = match Registry::load(&dirs) {
            Ok(_) => panic!("expected a duplicate backend error"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("duplicate backend id"));
    }
}
