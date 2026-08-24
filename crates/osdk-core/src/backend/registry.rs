//! Backend registry: maps tool ids / aliases to backend instances.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

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
        // Dynamic namespaced backends: `github:owner/repo`, `npm:package`.
        if name.starts_with("github:") {
            if let Some(gh) = crate::backend::github::GithubBackend::from_id(name) {
                return Ok(Arc::new(gh));
            }
            return Err(Error::UnknownBackend(name.to_string()));
        }
        if name.starts_with("npm:") {
            if let Some(package) = NpmPackageBackend::from_id(name) {
                return Ok(Arc::new(package));
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

struct NpmPackageBackend {
    id: String,
    package: String,
}

impl NpmPackageBackend {
    fn from_id(id: &str) -> Option<Self> {
        let package = id.strip_prefix("npm:")?;
        validate_npm_package_name(package)?;
        Some(Self {
            id: format!("npm:{package}"),
            package: package.to_string(),
        })
    }
}

#[async_trait]
impl Backend for NpmPackageBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        crate::backend::npm_cli::NpmBackend.default_sources()
    }

    fn probe_url(&self, ctx: &super::Ctx, source: &Source) -> Option<String> {
        crate::backend::npm_cli::NpmBackend.probe_url(ctx, source)
    }

    async fn list_remote_versions(&self, ctx: &super::Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, &self.package).await?;
        Ok(versions
            .into_iter()
            .map(|version| VersionInfo {
                stable: !version.contains('-'),
                version,
                lts: None,
            })
            .collect())
    }

    async fn resolve_version(&self, ctx: &super::Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        crate::npm::resolve_package_version(ctx, &sources, &self.package, self.id(), req).await
    }

    async fn install(&self, _ctx: &super::InstallCtx<'_>, _tv: &ToolVersion) -> Result<()> {
        Err(Error::other(format!(
            "dynamic npm package installs are not implemented yet for `{}`",
            self.id
        )))
    }

    fn bin_paths(&self, _ctx: &super::Ctx, _tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![])
    }

    fn bin_names(&self, _ctx: &super::Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

fn validate_npm_package_name(package: &str) -> Option<()> {
    if let Some(rest) = package.strip_prefix('@') {
        let (scope, name) = rest.split_once('/')?;
        if !valid_npm_segment(scope) || !valid_npm_segment(name) || name.contains('/') {
            return None;
        }
        return Some(());
    }
    if !valid_npm_segment(package) {
        return None;
    }
    Some(())
}

fn valid_npm_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('@')
        && !value.chars().any(char::is_whitespace)
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
    fn rejects_invalid_dynamic_namespaced_backends() {
        let registry = Registry::new();

        assert!(registry.get("npm:").is_err());
        assert!(registry.get("npm:@antfu").is_err());
        assert!(registry.get("npm:@antfu/ni/extra").is_err());
    }
}
