//! Factories for backends whose complete id includes a namespace-specific value.

use std::sync::Arc;

use super::Backend;

/// Constructs a backend for a registered dynamic namespace.
pub(super) trait DynamicBackendFactory: Send + Sync {
    /// Namespace before the `:` in a dynamic backend id.
    fn prefix(&self) -> &'static str;

    /// Parse and construct a backend from its complete namespaced id.
    fn create(&self, id: &str) -> Option<Arc<dyn Backend>>;
}

pub(super) fn builtin_factories() -> Vec<Arc<dyn DynamicBackendFactory>> {
    vec![Arc::new(GithubBackendFactory), Arc::new(NpmBackendFactory)]
}

struct GithubBackendFactory;

impl DynamicBackendFactory for GithubBackendFactory {
    fn prefix(&self) -> &'static str {
        "github"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::github::GithubBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}

struct NpmBackendFactory;

impl DynamicBackendFactory for NpmBackendFactory {
    fn prefix(&self) -> &'static str {
        "npm"
    }

    fn create(&self, id: &str) -> Option<Arc<dyn Backend>> {
        crate::backend::npm_package::NpmPackageBackend::from_id(id)
            .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
    }
}
