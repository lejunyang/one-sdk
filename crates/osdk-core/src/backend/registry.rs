//! Backend registry: maps tool ids / aliases to backend instances.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};

use super::Backend;

pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
    by_name: HashMap<String, usize>,
}

impl Registry {
    /// Build the registry with all compiled-in backends.
    pub fn new() -> Registry {
        let backends: Vec<Arc<dyn Backend>> = vec![
            Arc::new(crate::backend::node::NodeBackend),
            Arc::new(crate::backend::go::GoBackend),
            Arc::new(crate::backend::python::PythonBackend),
            Arc::new(crate::backend::java::JavaBackend),
            Arc::new(crate::backend::rust::RustBackend),
            Arc::new(crate::backend::pnpm::PnpmBackend),
            Arc::new(crate::backend::yarn::YarnBackend),
        ];
        let mut by_name = HashMap::new();
        for (i, b) in backends.iter().enumerate() {
            by_name.insert(b.id().to_string(), i);
            for alias in b.aliases() {
                by_name.insert((*alias).to_string(), i);
            }
        }
        Registry { backends, by_name }
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
