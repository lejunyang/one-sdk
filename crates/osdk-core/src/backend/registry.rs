//! Backend registry: maps tool ids / aliases to backend instances.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dirs::Dirs;
use crate::error::{Error, Result};

use super::dynamic::{self, DynamicBackendFactory};
use super::Backend;

pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
    by_name: HashMap<String, usize>,
    dynamic_by_prefix: HashMap<&'static str, Arc<dyn DynamicBackendFactory>>,
}

impl Registry {
    /// Build the registry with all compiled-in backends.
    pub fn new() -> Registry {
        Self::from_backends(vec![
            Arc::new(crate::backend::node::NodeBackend),
            Arc::new(crate::backend::npm_cli::NpmBackend),
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
        Self::from_parts(backends, dynamic::builtin_factories())
    }

    fn from_parts(
        backends: Vec<Arc<dyn Backend>>,
        dynamic_factories: Vec<Arc<dyn DynamicBackendFactory>>,
    ) -> Result<Registry> {
        let mut by_name = HashMap::new();
        for (i, b) in backends.iter().enumerate() {
            insert_name(&mut by_name, b.id(), i)?;
            for alias in b.aliases() {
                insert_name(&mut by_name, alias, i)?;
            }
        }

        let mut dynamic_by_prefix = HashMap::new();
        for factory in dynamic_factories {
            let prefix = factory.prefix();
            if dynamic_by_prefix.insert(prefix, factory).is_some() {
                return Err(Error::config(format!(
                    "duplicate dynamic backend prefix `{prefix}`"
                )));
            }
        }

        Ok(Registry {
            backends,
            by_name,
            dynamic_by_prefix,
        })
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Backend>> {
        if let Some((prefix, _)) = name.split_once(':') {
            return self
                .dynamic_by_prefix
                .get(prefix)
                .and_then(|factory| factory.create(name))
                .ok_or_else(|| Error::UnknownBackend(name.to_string()));
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

    #[test]
    fn load_preserves_dynamic_backend_factories() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();

        let registry = Registry::load(&dirs).unwrap();
        assert_eq!(registry.get("npm:Prettier").unwrap().id(), "npm:prettier");
        assert_eq!(
            registry.get("github:cli/cli").unwrap().id(),
            "github:cli/cli"
        );
    }

    #[test]
    fn resolves_dynamic_namespaced_backends_without_shadowing_bare_npm() {
        let registry = Registry::new();

        assert_eq!(registry.get("npm").unwrap().id(), "npm");
        assert_eq!(
            registry.get("github:cli/cli").unwrap().id(),
            "github:cli/cli"
        );
        assert_eq!(registry.get("npm:prettier").unwrap().id(), "npm:prettier");
        assert_eq!(registry.get("npm:@antfu/ni").unwrap().id(), "npm:@antfu/ni");
        assert_eq!(registry.get("npm:npm").unwrap().id(), "npm:npm");
    }

    #[test]
    fn resolves_dynamic_namespaced_backends_with_lowercase_canonical_ids_and_paths() {
        let registry = Registry::new();

        let prettier = registry.get("npm:Prettier").unwrap();
        assert_eq!(prettier.id(), "npm:prettier");
        assert_eq!(
            crate::dirs::sanitize_tool_id(prettier.id()),
            std::path::PathBuf::from("npm/prettier")
        );

        let scoped = registry.get("npm:@Antfu/Ni").unwrap();
        assert_eq!(scoped.id(), "npm:@antfu/ni");
        assert_eq!(
            crate::dirs::sanitize_tool_id(scoped.id()),
            std::path::PathBuf::from("npm/@antfu/ni")
        );
    }

    #[test]
    fn rejects_invalid_dynamic_namespaced_backends() {
        let registry = Registry::new();

        for name in [
            "npm:",
            "npm:@antfu",
            "npm:@antfu/ni/extra",
            "github:",
            "github:noslash",
            "github:cli/cli/extra",
            "cargo:ripgrep",
            "NPM:prettier",
        ] {
            assert!(
                matches!(registry.get(name), Err(Error::UnknownBackend(id)) if id == name),
                "expected `{name}` to remain an unknown backend"
            );
        }
    }

    #[test]
    fn rejects_duplicate_dynamic_backend_prefixes() {
        struct TestFactory;

        impl DynamicBackendFactory for TestFactory {
            fn prefix(&self) -> &'static str {
                "test"
            }

            fn create(&self, _id: &str) -> Option<Arc<dyn Backend>> {
                None
            }
        }

        let error = match Registry::from_parts(
            Vec::new(),
            vec![Arc::new(TestFactory), Arc::new(TestFactory)],
        ) {
            Ok(_) => panic!("expected a duplicate dynamic backend prefix error"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("duplicate dynamic backend prefix `test`"));
    }
}
