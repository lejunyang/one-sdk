//! `pypi:` backend -- install Python CLIs from a PEP 503 index.
//!
//! # Why this delegates
//!
//! Resolving Python dependencies correctly means implementing PubGrub, PEP 440
//! version semantics, PEP 425 wheel-tag selection, PEP 517 source builds, and
//! the wheel installer. uv already does all of that, and the consequence of
//! getting it wrong is not slowness but installing the *wrong package*. So this
//! backend owns what osdk is actually better placed to own -- index mirror
//! selection, cache placement, fail-closed gating, and install layout -- and
//! delegates resolution and installation to a subprocess.
//!
//! # One venv per tool, shared dependencies
//!
//! Each tool gets its own virtual environment, like `uv tool` and pipx: two CLIs
//! that need incompatible versions of one library must not fight over a shared
//! environment.
//!
//! Isolation would normally mean paying for a full copy of every shared
//! dependency, but under uv it does not. uv hard-links unpacked files from its
//! own `archive-v0` cache into each environment, so N environments sharing a
//! dependency hold one copy of its bytes. Measured on Windows x64 with two
//! environments that both installed `certifi`: uv produced three paths sharing a
//! single inode (both environments plus the cache) at 762,964 B per
//! environment, while pip produced independent copies at 6,812,960 B -- an 8.9x
//! difference for identical content.
//!
//! **osdk deliberately does not add a wheel-level CAS of its own.** osdk's
//! `store` keys unpacked SDK *archives*; uv's `archive-v0` keys unpacked
//! *site-packages* trees. The key semantics and lifetimes differ, so a second
//! implementation would produce two incomplete caches rather than one good one.
//! What osdk contributes is making uv's cache land under the managed root (see
//! `crate::cache`), so this reuse is osdk-managed rather than scattered.
//!
//! On the pip fallback path that reuse is simply unavailable, which is a real
//! capability difference and is reported to the user rather than papered over.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

/// The installer that produced an environment.
///
/// Recorded because the two paths do not produce equivalent environments, and
/// later operations have to know which one they are looking at. `uv venv` does
/// not seed pip unless asked; `python -m venv` always has pip but never
/// setuptools or wheel. Measured: a stdlib environment carries pip 26.2.1 with
/// `setuptools`/`wheel` absent, and its `pyvenv.cfg` has no `uv =` key at all,
/// which is what makes the creator detectable after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvCreator {
    /// `uv venv` + `uv pip install`. Shares unpacked files via hard links.
    Uv,
    /// `python -m venv` + that environment's own pip. No cross-env sharing.
    Stdlib,
}

impl EnvCreator {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uv => "uv",
            Self::Stdlib => "stdlib",
        }
    }

    /// Whether this creator shares unpacked dependencies between environments.
    pub fn shares_dependencies(self) -> bool {
        matches!(self, Self::Uv)
    }
}

/// What osdk records about an environment it created.
///
/// Kept in osdk's own state rather than written into the environment: a venv is
/// a directory the user may delete, recreate, or have built themselves, so osdk
/// does not modify its contents. [`creator_from_pyvenv_cfg`] recovers the
/// creator when this record is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvReceipt {
    pub schema: u32,
    pub creator: EnvCreator,
    /// Interpreter the environment was built against, as an absolute path.
    pub interpreter: String,
    /// Whether `pip`, `setuptools`, and `wheel` are present.
    pub seed: SeedState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedState {
    pub pip: bool,
    pub setuptools: bool,
    pub wheel: bool,
}

pub const ENV_RECEIPT_SCHEMA: u32 = 1;
pub const ENV_RECEIPT_FILE: &str = ".osdk-pypi-env.json";

/// Recover the creator from an existing environment.
///
/// uv writes a `uv = <version>` key into `pyvenv.cfg`; the stdlib `venv` module
/// never does. Verified against both paths on Windows x64: the uv environment's
/// config carried `uv = 0.12.13`, and the stdlib one had only the five standard
/// keys. This is the fallback for an environment osdk has no record of.
pub fn creator_from_pyvenv_cfg(venv: &Path) -> Option<EnvCreator> {
    let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    let created_by_uv = text.lines().any(|line| {
        line.split_once('=')
            .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("uv"))
    });
    Some(if created_by_uv {
        EnvCreator::Uv
    } else {
        EnvCreator::Stdlib
    })
}

/// The directory holding executables inside a virtual environment.
pub fn venv_bin_dir(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

/// The interpreter inside a virtual environment.
pub fn venv_python(venv: &Path) -> PathBuf {
    venv_bin_dir(venv).join(if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    })
}

/// Environment variables that point a delegated installer at osdk's choices.
///
/// # Gating, not convenience
///
/// Every entry here is load-bearing:
///
/// - The index is mapped to the *default* index only. A mirror is a full copy of
///   PyPI and therefore carries upstream's package names, malicious ones
///   included, so ranking it above the default index would be a
///   dependency-confusion vector. Note `UV_INDEX` is exactly that
///   higher-precedence form and is never produced here.
/// - `PIP_CONFIG_FILE` is pinned on the pip path. uv ignores `pip.conf` by
///   design, but pip reads it, so a stale user-level `pip.conf` pointing at an
///   untrusted index would otherwise silently override everything below.
/// - `UV_PYTHON_DOWNLOADS=never` keeps uv from fetching an interpreter behind
///   osdk's back; the interpreter is always the one osdk resolved.
/// - Offline is propagated so a cache-only install stays cache-only instead of
///   quietly reaching the network.
pub fn installer_env(
    creator: EnvCreator,
    index_url: Option<&str>,
    offline: bool,
    require_hashes: bool,
    config_file: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match creator {
        EnvCreator::Uv => {
            if let Some(url) = index_url {
                // `UV_DEFAULT_INDEX`, never `UV_INDEX`: see above.
                env.insert("UV_DEFAULT_INDEX".to_string(), url.to_string());
            }
            env.insert("UV_PYTHON_DOWNLOADS".to_string(), "never".to_string());
            if offline {
                env.insert("UV_OFFLINE".to_string(), "1".to_string());
            }
            if require_hashes {
                env.insert("UV_REQUIRE_HASHES".to_string(), "true".to_string());
            }
        }
        EnvCreator::Stdlib => {
            if let Some(url) = index_url {
                // pip's `--index-url` is the default index; its
                // `--extra-index-url` is the confusable one and is never set.
                env.insert("PIP_INDEX_URL".to_string(), url.to_string());
            }
            if let Some(path) = config_file {
                env.insert("PIP_CONFIG_FILE".to_string(), path.display().to_string());
            }
            if offline {
                env.insert("PIP_NO_INDEX".to_string(), "1".to_string());
            }
            if require_hashes {
                env.insert("PIP_REQUIRE_HASHES".to_string(), "1".to_string());
            }
            // Keep pip from phoning home about upgrades on every invocation.
            env.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
        }
    }
    env
}

/// Arguments that must never reach a delegated installer.
///
/// Each of these switches off a check osdk is relying on, so they are refused
/// rather than forwarded -- including when a user passes them through as extra
/// installer arguments.
pub const REFUSED_INSTALLER_ARGS: &[&str] = &[
    // Would disable the hash verification that makes mirror use safe.
    "--no-verify-hashes",
    // Would disable TLS verification for a host.
    "--allow-insecure-host",
    "--trusted-host",
    // Would add an index that outranks the default one (confusion vector).
    "--extra-index-url",
    "--index",
];

/// Reject any argument that would weaken a gate, with the reason.
pub fn reject_unsafe_installer_args<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<()> {
    for arg in args {
        // `--flag=value` has to be caught as well as the bare form.
        let name = arg.split('=').next().unwrap_or(arg);
        if let Some(found) = REFUSED_INSTALLER_ARGS
            .iter()
            .find(|refused| **refused == name)
        {
            return Err(Error::config(format!(
                "`{found}` cannot be forwarded to the Python installer: it disables a \
                 check osdk relies on. Index mirrors are configured with `osdk config` \
                 instead, and are mapped onto the default index only."
            )));
        }
    }
    Ok(())
}

/// A `pypi:<project>` backend instance.
pub struct PypiBackend {
    id: String,
    project: String,
}

impl PypiBackend {
    pub fn from_id(id: &str) -> Option<Self> {
        let id = crate::tool::canonical_dynamic_id(id).ok()?;
        let project = id.strip_prefix("pypi:")?.to_string();
        Some(Self { id, project })
    }

    /// The PEP 503 normalized project name.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// The PEP 508 requirement string for a concrete version, including extras.
    ///
    /// `==` is deliberate: the version is already resolved, and a looser
    /// specifier would let the installer pick something else.
    pub fn requirement(&self, version: &str, options: &BTreeMap<String, String>) -> String {
        match options.get("extras") {
            Some(extras) if !extras.is_empty() => {
                format!("{}[{}]=={}", self.project, extras, version)
            }
            _ => format!("{}=={}", self.project, version),
        }
    }
}

#[async_trait]
impl Backend for PypiBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        // The index is not an SDK download source: it is consumed by a delegated
        // installer, and its candidates live in `[registries.python]` so that one
        // configured mirror serves every Python operation. Returning nothing here
        // keeps the two from being conflated.
        Vec::new()
    }

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        None
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        // Deliberately unimplemented for now: listing versions means reading the
        // index, and the index client belongs to the same change that performs
        // installs. Returning an explicit error beats an empty list, which would
        // read as "this project has no releases".
        Err(Error::other(format!(
            "listing versions for `{}` is not implemented yet",
            self.id
        )))
    }

    #[cfg(feature = "install")]
    async fn resolve_version(&self, _ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        // An exact request resolves without touching the network, which is what
        // `osdk install pypi:uv@0.12.13` needs.
        if let crate::version::VersionSpec::Exact(version) = &req.spec {
            let mut tv = ToolVersion::new(self.id(), version.clone());
            tv.options = req.options.clone();
            return Ok(tv);
        }
        Err(Error::VersionResolve {
            tool: self.id.clone(),
            spec: req.spec.to_string(),
            hint: Some(
                "pypi requests need an exact version for now, e.g. `pypi:ruff@0.6.9`".into(),
            ),
        })
    }

    #[cfg(feature = "install")]
    async fn install(&self, _ctx: &InstallCtx<'_>, _tv: &ToolVersion) -> Result<()> {
        Err(Error::other(format!(
            "installing `{}` is not implemented yet",
            self.id
        )))
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // One environment per tool: executables live in its own bin directory.
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        Ok(vec![venv_bin_dir(&root)])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        // Discover what is actually on disk rather than assuming the console
        // scripts match the project name -- they frequently do not.
        let paths = self.bin_paths(ctx, tv)?;
        let mut names = crate::backend::bin_names_in_dirs(&paths);
        // The interpreter and pip belong to the environment's plumbing, not to
        // the tool the user asked for; exposing them would let `pypi:ruff` shadow
        // the managed `python`.
        names.retain(|name| {
            let stem = name
                .rsplit_once('.')
                .map(|(stem, _)| stem)
                .unwrap_or(name)
                .to_ascii_lowercase();
            !matches!(
                stem.as_str(),
                "python" | "pythonw" | "python3" | "pip" | "pip3" | "activate" | "deactivate"
            ) && !stem.starts_with("pip3.")
                && !stem.starts_with("python3.")
                && !stem.starts_with("activate")
                && !stem.starts_with("deactivate")
        });
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_normalize_through_the_namespace_schema() {
        let backend = PypiBackend::from_id("pypi:Zope.Interface").unwrap();
        assert_eq!(backend.id(), "pypi:zope-interface");
        assert_eq!(backend.project(), "zope-interface");

        // Not a pypi id, or not a valid one.
        assert!(PypiBackend::from_id("npm:prettier").is_none());
        assert!(PypiBackend::from_id("pypi:").is_none());
        assert!(PypiBackend::from_id("pypi:a/b").is_none());
    }

    #[test]
    fn requirements_pin_exactly_and_carry_extras() {
        let backend = PypiBackend::from_id("pypi:httpx").unwrap();
        let plain = BTreeMap::new();
        assert_eq!(backend.requirement("0.27.0", &plain), "httpx==0.27.0");

        let mut with_extras = BTreeMap::new();
        with_extras.insert("extras".to_string(), "http2,socks".to_string());
        assert_eq!(
            backend.requirement("0.27.0", &with_extras),
            "httpx[http2,socks]==0.27.0"
        );
    }

    /// The mirror must reach the installer as the *default* index only.
    #[test]
    fn mirrors_never_become_a_higher_precedence_index() {
        let uv = installer_env(
            EnvCreator::Uv,
            Some("https://mirror.test/simple/"),
            false,
            false,
            None,
        );
        assert_eq!(
            uv.get("UV_DEFAULT_INDEX").map(String::as_str),
            Some("https://mirror.test/simple/")
        );
        // `UV_INDEX` is the higher-precedence form; producing it would make a
        // mirror able to shadow the default index.
        assert!(!uv.contains_key("UV_INDEX"));
        assert!(!uv.contains_key("UV_EXTRA_INDEX_URL"));
        // The interpreter is always osdk's, never one uv downloads.
        assert_eq!(
            uv.get("UV_PYTHON_DOWNLOADS").map(String::as_str),
            Some("never")
        );

        let pip = installer_env(
            EnvCreator::Stdlib,
            Some("https://mirror.test/simple/"),
            false,
            false,
            None,
        );
        assert_eq!(
            pip.get("PIP_INDEX_URL").map(String::as_str),
            Some("https://mirror.test/simple/")
        );
        assert!(!pip.contains_key("PIP_EXTRA_INDEX_URL"));
    }

    /// pip reads `pip.conf`; uv does not. The pip path must therefore pin the
    /// config file, or a stale user-level config could redirect the index.
    #[test]
    fn the_pip_path_pins_its_config_file() {
        let config = PathBuf::from("/tmp/osdk/pip.conf");
        let pip = installer_env(EnvCreator::Stdlib, None, false, false, Some(&config));
        assert_eq!(
            pip.get("PIP_CONFIG_FILE").map(String::as_str),
            Some("/tmp/osdk/pip.conf")
        );

        // uv ignores pip.conf by design, so pinning it there would be noise that
        // implies a protection that is not actually in play.
        let uv = installer_env(EnvCreator::Uv, None, false, false, Some(&config));
        assert!(!uv.contains_key("PIP_CONFIG_FILE"));
    }

    #[test]
    fn offline_and_hash_enforcement_reach_both_installers() {
        let uv = installer_env(EnvCreator::Uv, None, true, true, None);
        assert_eq!(uv.get("UV_OFFLINE").map(String::as_str), Some("1"));
        assert_eq!(
            uv.get("UV_REQUIRE_HASHES").map(String::as_str),
            Some("true")
        );

        let pip = installer_env(EnvCreator::Stdlib, None, true, true, None);
        assert_eq!(pip.get("PIP_NO_INDEX").map(String::as_str), Some("1"));
        assert_eq!(pip.get("PIP_REQUIRE_HASHES").map(String::as_str), Some("1"));
    }

    #[test]
    fn gate_weakening_arguments_are_refused_in_both_spellings() {
        for refused in [
            "--no-verify-hashes",
            "--trusted-host",
            "--trusted-host=mirror.test",
            "--extra-index-url",
            "--extra-index-url=https://evil.test/simple",
            "--index",
            "--allow-insecure-host=mirror.test",
        ] {
            let error = reject_unsafe_installer_args([refused]).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("disables a check osdk relies on"),
                "`{refused}` should be refused with an explanation, got: {error}"
            );
        }

        // Ordinary arguments still pass.
        reject_unsafe_installer_args(["--no-cache", "--upgrade", "ruff==0.6.9"]).unwrap();
    }

    /// The creator is detectable from an environment osdk has no record of.
    #[test]
    fn creator_is_recovered_from_pyvenv_cfg() {
        let temp = tempfile::tempdir().unwrap();

        let uv_env = temp.path().join("uv-env");
        std::fs::create_dir_all(&uv_env).unwrap();
        std::fs::write(
            uv_env.join("pyvenv.cfg"),
            "home = /x\nversion = 3.14.7\nuv = 0.12.13\nseed = true\n",
        )
        .unwrap();
        assert_eq!(creator_from_pyvenv_cfg(&uv_env), Some(EnvCreator::Uv));

        // The stdlib module writes no `uv` key -- this is what distinguishes them.
        let stdlib_env = temp.path().join("stdlib-env");
        std::fs::create_dir_all(&stdlib_env).unwrap();
        std::fs::write(
            stdlib_env.join("pyvenv.cfg"),
            "home = /x\ninclude-system-site-packages = false\nversion = 3.14.7\n",
        )
        .unwrap();
        assert_eq!(
            creator_from_pyvenv_cfg(&stdlib_env),
            Some(EnvCreator::Stdlib)
        );

        // No config at all means no answer, rather than a wrong default.
        assert_eq!(creator_from_pyvenv_cfg(temp.path()), None);
    }

    /// Only uv shares unpacked dependencies between environments; the pip path
    /// is a real capability downgrade that callers must be able to detect.
    #[test]
    fn only_the_uv_path_shares_dependencies() {
        assert!(EnvCreator::Uv.shares_dependencies());
        assert!(!EnvCreator::Stdlib.shares_dependencies());
    }

    fn context(root: &Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[test]
    fn environment_plumbing_is_not_exposed_as_tool_commands() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path());

        let backend = PypiBackend::from_id("pypi:ruff").unwrap();
        let tv = ToolVersion::new(backend.id(), "0.6.9");
        let bin_dir = venv_bin_dir(&ctx.dirs.install_path(backend.id(), &tv.version));
        std::fs::create_dir_all(&bin_dir).unwrap();
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        for name in [
            format!("ruff{suffix}"),
            format!("python{suffix}"),
            format!("pythonw{suffix}"),
            format!("pip{suffix}"),
            format!("pip3.14{suffix}"),
            "activate.bat".to_string(),
            "deactivate.bat".to_string(),
        ] {
            std::fs::write(bin_dir.join(&name), b"x").unwrap();
        }

        let names = backend.bin_names(&ctx, &tv).unwrap();
        assert!(
            names.iter().any(|name| name.starts_with("ruff")),
            "the tool's own command must be exposed: {names:?}"
        );
        // Exposing these would let `pypi:ruff` shadow the managed `python`.
        for hidden in [
            "python",
            "pythonw",
            "pip",
            "pip3.14",
            "activate",
            "deactivate",
        ] {
            assert!(
                !names.iter().any(|name| {
                    name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name) == hidden
                }),
                "`{hidden}` must not become a shim: {names:?}"
            );
        }
    }
}
