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
    cache_dir: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match creator {
        EnvCreator::Uv => {
            if let Some(url) = index_url {
                // `UV_DEFAULT_INDEX`, never `UV_INDEX`: see above.
                env.insert("UV_DEFAULT_INDEX".to_string(), url.to_string());
            }
            // Point uv at the managed cache explicitly. `crate::cache` sets this
            // for interactive shells, but a subprocess osdk spawns itself does not
            // inherit that, so without it uv used its own default location -- and
            // the hard-link sharing between environments, which is the whole
            // reason to prefer uv, happens *through* that cache.
            if let Some(dir) = cache_dir {
                env.insert("UV_CACHE_DIR".to_string(), dir.display().to_string());
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
            // Same reasoning as the uv branch: a spawned subprocess does not
            // inherit the shell hook's cache redirect. pip gains no cross-env
            // sharing from this, but the downloads still belong under the managed
            // root rather than in pip's own default location.
            if let Some(dir) = cache_dir {
                env.insert("PIP_CACHE_DIR".to_string(), dir.display().to_string());
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

    /// Create the environment and install the requirement into it.
    #[cfg(feature = "install")]
    fn build_environment(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        choice: &InstallerChoice,
        interpreter: &Path,
        venv: &Path,
    ) -> Result<()> {
        // A user-supplied pip.conf must not be able to redirect the index, so
        // the pip path always runs against a config file osdk controls.
        let pip_config = venv.parent().map(|parent| parent.join(".osdk-pip.conf"));
        if let (EnvCreator::Stdlib, Some(path)) = (choice.creator, pip_config.as_deref()) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
            }
            std::fs::write(path, b"# managed by osdk\\n")
                .map_err(|error| Error::io(path, error))?;
        }

        let index = ctx
            .config
            .registries()
            .python
            .urls
            .first()
            .map(String::as_str);
        // Reuse the same directory the shell hook redirects to, so an install
        // performed by osdk and one performed in an activated shell share a cache
        // rather than filling two.
        let cache_root = crate::cache::downstream_root(&ctx.dirs.cache);
        let cache_dir = match choice.creator {
            EnvCreator::Uv => cache_root.join("uv"),
            EnvCreator::Stdlib => cache_root.join("pip"),
        };
        let env = installer_env(
            choice.creator,
            index,
            ctx.config.settings.offline,
            ctx.config.settings.require_checksums,
            pip_config.as_deref(),
            Some(&cache_dir),
        );

        let (program, args) = venv_command(choice, interpreter, venv);
        run_installer(&program, &args, &env)?;

        let requirement = self.requirement(&tv.version, &tv.options);
        let (program, args) = install_command(choice, venv, &requirement);
        run_installer(&program, &args, &env)?;

        // Record what was built. Kept beside the environment rather than inside
        // it, so osdk never modifies a directory the user may manage.
        let receipt = EnvReceipt {
            schema: ENV_RECEIPT_SCHEMA,
            creator: choice.creator,
            interpreter: interpreter.display().to_string(),
            seed: detect_seed(venv),
        };
        let receipt_path = venv.join(ENV_RECEIPT_FILE);
        std::fs::write(
            &receipt_path,
            serde_json::to_vec_pretty(&receipt).map_err(|error| {
                Error::other(format!("could not serialize the pypi env receipt: {error}"))
            })?,
        )
        .map_err(|error| Error::io(&receipt_path, error))?;
        Ok(())
    }

    /// The index to read for version discovery.
    ///
    /// Uses the same ranking as installs, so `latest` and the install that
    /// follows it read the same index -- resolving against upstream and then
    /// installing from a lagging mirror is how a version that "exists" turns out
    /// to be unavailable moments later.
    #[cfg(feature = "install")]
    async fn resolved_index(&self, ctx: &Ctx) -> Result<String> {
        let configured = &ctx.config.registries().python;
        match crate::python_index::plan(&configured.urls, configured.probe_timeout_ms).await {
            crate::python_index::IndexPlan::Selected { url, .. } => Ok(url),
            // No mirror configured: upstream is the default index.
            crate::python_index::IndexPlan::PassThrough { .. } => {
                Ok(crate::python_index::PYPI.to_string())
            }
            // Configured mirrors all failed. Falling back to upstream here would
            // silently ignore the configuration, so this fails closed.
            crate::python_index::IndexPlan::Unavailable { probes } => {
                let detail = probes
                    .iter()
                    .map(|probe| {
                        format!(
                            "{} ({})",
                            probe.url,
                            probe.error.as_deref().unwrap_or("unavailable")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(Error::other(format!(
                    "no configured Python index is reachable: {detail}"
                )))
            }
        }
    }

    /// The directory holding this tool's environment.
    ///
    /// Derived from the install identity, so it matches what the shim resolves.
    pub fn install_root(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<PathBuf> {
        let identity = pypi_install_identity(ctx, self.id(), tv)?;
        Ok(crate::dirs::InstallLocator::new(&ctx.dirs, identity)?
            .install_root()
            .to_path_buf())
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

/// Which installer to drive, and why that choice was made.
///
/// The reason travels with the decision because it has to be shown to the
/// user: the two paths differ in dependency sharing and in supported options,
/// so silently taking the slower, less capable one would turn a capability
/// difference into an unexplained mystery.
#[cfg(feature = "install")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerChoice {
    pub creator: EnvCreator,
    /// Absolute path to the uv executable, when the uv path was chosen.
    pub uv: Option<PathBuf>,
    /// User-facing explanation, empty when uv was available as expected.
    pub notice: Option<String>,
}

/// Decide between uv and the stdlib fallback.
///
/// # Why "can it spawn" rather than "does it exist"
///
/// A plain path lookup is not enough on Windows: a `uv.ps1` shim or a file
/// carrying only a shebang resolves fine yet cannot be started as a process.
/// mise hit this in both its venv and pipx paths and settled on a spawn check;
/// this does the same by actually running `uv --version`.
///
/// `require_uv` makes the absence of uv fail closed instead of falling back --
/// for callers that need uv-only behaviour such as `--relocatable`.
#[cfg(feature = "install")]
pub fn choose_installer(uv_candidate: Option<&Path>, require_uv: bool) -> Result<InstallerChoice> {
    if let Some(candidate) = uv_candidate {
        if uv_is_spawnable(candidate) {
            return Ok(InstallerChoice {
                creator: EnvCreator::Uv,
                uv: Some(candidate.to_path_buf()),
                notice: None,
            });
        }
        if require_uv {
            return Err(Error::other(format!(
                "`{}` was found but could not be started, so uv-only behaviour is unavailable; reinstall uv or drop --require-uv",
                candidate.display()
            )));
        }
        // Found but unusable is worth saying out loud: the file exists, so
        // "uv is not installed" would send the user looking in the wrong place.
        return Ok(InstallerChoice {
            creator: EnvCreator::Stdlib,
            uv: None,
            notice: Some(format!(
                "`{}` exists but could not be started; using python -m venv with pip instead. Dependencies will not be shared between environments.",
                candidate.display()
            )),
        });
    }
    if require_uv {
        return Err(Error::other(
            "uv is required for this operation but is not installed; run `osdk install pypi:uv` first",
        ));
    }
    Ok(InstallerChoice {
        creator: EnvCreator::Stdlib,
        uv: None,
        notice: Some(
            "uv is not installed; using python -m venv with pip. Installing uv (`osdk install pypi:uv`) makes resolution faster and lets environments share dependencies instead of each keeping its own copy."
                .to_string(),
        ),
    })
}

/// Whether this uv can actually be started, not merely located.
#[cfg(feature = "install")]
fn uv_is_spawnable(uv: &Path) -> bool {
    use crate::process::{
        CaptureLimits, CommandOutcome, CommandRunner, CommandSpec, SystemCommandRunner,
    };
    let command = CommandSpec::new(uv).arg("--version");
    matches!(
        SystemCommandRunner.run_captured(&command, CaptureLimits::default()),
        CommandOutcome::Exited { status, .. } if status.success()
    )
}

/// Build the argument list that creates the environment.
#[cfg(feature = "install")]
pub fn venv_command(
    choice: &InstallerChoice,
    interpreter: &Path,
    venv: &Path,
) -> (PathBuf, Vec<String>) {
    match (choice.creator, choice.uv.as_deref()) {
        (EnvCreator::Uv, Some(uv)) => (
            uv.to_path_buf(),
            vec![
                "venv".to_string(),
                // The interpreter is named explicitly rather than left to uv:
                // the whole point is that the environment is built against the
                // version osdk resolved. mise passes `--python <abs path>` here
                // for the same reason.
                "--python".to_string(),
                interpreter.display().to_string(),
                venv.display().to_string(),
            ],
        ),
        _ => (
            interpreter.to_path_buf(),
            vec![
                "-m".to_string(),
                "venv".to_string(),
                venv.display().to_string(),
            ],
        ),
    }
}

/// Build the argument list that installs `requirement` into `venv`.
#[cfg(feature = "install")]
pub fn install_command(
    choice: &InstallerChoice,
    venv: &Path,
    requirement: &str,
) -> (PathBuf, Vec<String>) {
    match (choice.creator, choice.uv.as_deref()) {
        (EnvCreator::Uv, Some(uv)) => (
            uv.to_path_buf(),
            vec![
                "pip".to_string(),
                "install".to_string(),
                // Target the environment explicitly. Relying on an ambient
                // VIRTUAL_ENV would make the destination depend on how the
                // caller was invoked.
                "--python".to_string(),
                venv_python(venv).display().to_string(),
                requirement.to_string(),
            ],
        ),
        _ => (
            // The environment's own pip, never a global one: a global pip would
            // install into whichever interpreter owns it, which is how packages
            // end up in a system Python nobody asked for.
            venv_python(venv),
            vec![
                "-m".to_string(),
                "pip".to_string(),
                "install".to_string(),
                requirement.to_string(),
            ],
        ),
    }
}

/// Inspect which seed packages an environment ended up with.
#[cfg(feature = "install")]
pub fn detect_seed(venv: &Path) -> SeedState {
    let site = site_packages_dirs(venv);
    let present = |name: &str| {
        site.iter().any(|dir| {
            dir.join(name).is_dir()
                || std::fs::read_dir(dir).is_ok_and(|entries| {
                    entries.filter_map(|entry| entry.ok()).any(|entry| {
                        let file = entry.file_name();
                        let file = file.to_string_lossy();
                        file.starts_with(&format!("{name}-")) && file.ends_with(".dist-info")
                    })
                })
        })
    };
    SeedState {
        pip: present("pip"),
        setuptools: present("setuptools"),
        wheel: present("wheel"),
    }
}

#[cfg(feature = "install")]
fn site_packages_dirs(venv: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![venv.join("Lib").join("site-packages")];
    }
    // Unix nests site-packages under the minor version, which is not known
    // here, so enumerate rather than guess.
    let lib = venv.join("lib");
    let Ok(entries) = std::fs::read_dir(&lib) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("site-packages"))
        .filter(|path| path.is_dir())
        .collect()
}

/// Run one installer step, refusing arguments that would weaken a gate.
#[cfg(feature = "install")]
fn run_installer(program: &Path, args: &[String], env: &BTreeMap<String, String>) -> Result<()> {
    // Belt and braces: the argument lists are built above rather than taken
    // from the user, but this is the single choke point every install passes
    // through, so the check lives here too.
    reject_unsafe_installer_args(args.iter().map(String::as_str))?;
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::process::run(&program.display().to_string(), &borrowed, env, None)
}

/// The `python` option names which managed interpreter to build against.
#[cfg(feature = "install")]
pub const PYTHON_OPTION: &str = "python";


/// Whether the caller demanded uv-only behaviour.
#[cfg(feature = "install")]
fn require_uv(options: &BTreeMap<String, String>) -> bool {
    options
        .get("require-uv")
        .is_some_and(|value| value == "true" || value == "1")
}

/// Resolve the managed interpreter to build the environment against.
///
/// Requiring an osdk-managed interpreter is deliberate. Building against
/// whatever `python` happens to be on PATH would make the environment depend on
/// machine state osdk does not control -- on this machine that PATH entry is a
/// system 3.11 while osdk manages 3.14.7.
#[cfg(feature = "install")]
fn managed_interpreter(ctx: &Ctx, options: &BTreeMap<String, String>) -> Result<PathBuf> {
    let installed = crate::backend::python::PythonBackend.list_installed(ctx)?;
    if installed.is_empty() {
        return Err(Error::other(
            "no osdk-managed Python is installed; run `osdk install python` first",
        ));
    }
    let version = match options.get(PYTHON_OPTION) {
        Some(requested) => crate::backend::python::select_installed(requested, &installed)
            .ok_or_else(|| Error::NotInstalled {
                tool: "python".into(),
                version: requested.clone(),
            })?,
        // Newest installed wins when unspecified. `list_installed` sorts
        // lexically, which is not version order, so pick by comparing properly.
        None => installed
            .iter()
            .max_by(|left, right| compare_python_versions(left, right))
            .cloned()
            .expect("the list is not empty"),
    };

    let root = ctx.dirs.install_path("python", &version);
    if !root.join(".osdk-complete").is_file() {
        return Err(Error::NotInstalled {
            tool: "python".into(),
            version,
        });
    }
    let interpreter = if cfg!(windows) {
        root.join("python.exe")
    } else {
        root.join("bin").join("python3")
    };
    if !interpreter.is_file() {
        return Err(Error::other(format!(
            "managed Python {version} has no interpreter at {}",
            interpreter.display()
        )));
    }
    Ok(interpreter)
}

/// Order two version strings numerically, segment by segment.
#[cfg(feature = "install")]
fn compare_python_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split(['.', '+', '-'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    parse(left).cmp(&parse(right))
}

/// Find an osdk-managed uv, if one is installed.
///
/// Only osdk's own installs are consulted. A uv from PATH would be an
/// unpinned version whose behaviour could change under the user without any
/// record of it in the install.
#[cfg(feature = "install")]
fn locate_uv(ctx: &Ctx) -> Option<PathBuf> {
    let backend = PypiBackend::from_id("pypi:uv")?;
    let versions = backend.list_installed(ctx).ok()?;
    let newest = versions
        .iter()
        .max_by(|left, right| compare_python_versions(left, right))?;
    // Resolve through the install identity, not `install_path`: a dynamic install
    // sits one level deeper. Using the shallower path here meant a freshly
    // installed uv was never found, so every later install silently took the pip
    // fallback and reported "uv is not installed" while uv sat right there.
    let tv = ToolVersion::new(backend.id(), newest);
    let candidate = venv_bin_dir(&backend.install_root(ctx, &tv).ok()?).join(if cfg!(windows) {
        "uv.exe"
    } else {
        "uv"
    });
    candidate.is_file().then_some(candidate)
}

/// The install identity for one `pypi:` environment.
///
/// Not install-gated: the shim resolves an installed environment through the
/// same identity, and it reads only local state.
pub fn pypi_install_identity(
    ctx: &Ctx,
    backend_id: &str,
    tv: &ToolVersion,
) -> Result<crate::tool::InstallIdentity> {
    crate::tool::InstallIdentity::new(
        backend_id,
        &tv.version,
        ctx.platform.to_string(),
        crate::tool::InstallScope::Isolated,
        &tv.options,
        Vec::new(),
        // An environment is built by an installer rather than fetched from one
        // address, so there is no artifact file or checksum to record. The
        // requirement and its options are already part of the identity.
        std::collections::BTreeMap::new(),
    )
}

/// Resolve a command stem to the real file inside `directory`.
///
/// Needed because executable discovery reports stems while the file on Windows
/// carries an extension. Getting this wrong is quiet rather than loud: the
/// manifest simply records nothing.
#[cfg(feature = "install")]
fn executable_in_dir(directory: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.to_string(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    candidates
        .into_iter()
        .map(|candidate| directory.join(candidate))
        .find(|candidate| candidate.is_file())
}

/// Publish the inventory manifest that makes the environment usable.
#[cfg(feature = "install")]
fn finalize_pypi_install(
    backend: &PypiBackend,
    ctx: &Ctx,
    tv: &ToolVersion,
    root: &Path,
) -> Result<()> {
    use crate::inventory::{DynamicToolBin, DynamicToolManifest};

    let result = (|| -> Result<()> {
        let identity = pypi_install_identity(ctx, backend.id(), tv)?;
        let mut manifest = DynamicToolManifest::from_identity(identity)?;
        let bin_dir = venv_bin_dir(root);
        let canonical_root = dunce::canonicalize(root).map_err(|error| Error::io(root, error))?;

        // Record only the commands this tool actually owns, resolved against the
        // real root rather than a re-derived path.
        for name in tool_bin_names(root) {
            // `tool_bin_names` yields executable *stems* (`cowsay`), while the file
            // on Windows is `cowsay.exe`. Joining the stem directly finds nothing
            // there, which is how the manifest ended up with an empty `bins` list
            // even though the environment was correct and the command runnable.
            let Some(path) = executable_in_dir(&bin_dir, &name) else {
                continue;
            };
            let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
            // A console script must not point outside its own environment.
            let Ok(relative) = canonical.strip_prefix(&canonical_root) else {
                return Err(Error::other(format!(
                    "pypi command `{name}` resolves outside {}",
                    root.display()
                )));
            };
            manifest.bins.push(DynamicToolBin {
                name,
                path: relative.to_string_lossy().replace('\\', "/"),
                owned: true,
            });
        }
        manifest
            .bins
            .sort_by(|left, right| left.name.cmp(&right.name));
        manifest
            .bins
            .dedup_by(|left, right| left.name == right.name);

        manifest.write_atomic(root)?;
        // Written last: it is what marks the install usable, so it must not
        // appear before the inventory it depends on.
        std::fs::write(root.join(".osdk-complete"), b"")
            .map_err(|error| Error::io(root.join(".osdk-complete"), error))?;
        Ok(())
    })();
    if result.is_err() {
        // A published-but-unusable environment is worse than none: it would
        // satisfy a completeness check while failing every command.
        let _ = std::fs::remove_dir_all(root);
    }
    result
}

/// Whether a PEP 440 version string is a pre-release.
///
/// PEP 440 spells these with a letter marker after the release segment (`1.0rc1`,
/// `2.0b3`, `3.0.dev1`, `1.0a2`), unlike semver's `-` suffix, so the shared
/// semver-based check does not recognize them. Marking them unstable is what
/// keeps `latest` from resolving to a release candidate while still allowing an
/// exact request for one.
#[cfg(feature = "install")]
pub fn is_pep440_prerelease(version: &str) -> bool {
    let lowered = version.to_ascii_lowercase();
    // `.devN` and `.postN` may follow a separator; the pre-release markers may
    // not, which is why simple substring checks are not enough on their own.
    if lowered.contains(".dev") || lowered.starts_with("dev") {
        return true;
    }
    // Walk the string and look for a marker that directly follows a digit, so
    // `1.0rc1` matches while a project named like `beta-tool` does not.
    let bytes = lowered.as_bytes();
    for marker in ["a", "b", "c", "rc", "alpha", "beta", "pre", "preview"] {
        let mut from = 0usize;
        while let Some(found) = lowered[from..].find(marker) {
            let at = from + found;
            let after = at + marker.len();
            let preceded_by_digit = at
                .checked_sub(1)
                .is_some_and(|index| bytes[index].is_ascii_digit());
            let followed_by_digit_or_end = after >= bytes.len() || bytes[after].is_ascii_digit();
            if preceded_by_digit && followed_by_digit_or_end {
                return true;
            }
            from = at + 1;
            if from >= lowered.len() {
                break;
            }
        }
    }
    false
}

/// Order two PEP 440 versions.
///
/// Compares the numeric release segments field by field, so `0.10.0` sorts above
/// `0.9.0` -- a lexical sort puts them the other way round and would make
/// `latest` resolve to an older release. A pre-release sorts below the
/// corresponding final release, and anything unparseable falls back to a string
/// comparison rather than being dropped.
#[cfg(feature = "install")]
pub fn compare_pep440(left: &str, right: &str) -> std::cmp::Ordering {
    let release = |value: &str| -> Vec<u64> {
        value
            .split(['.', '+', '!'])
            .map(|part| {
                // Stop at the first non-digit so `0rc1` contributes 0 rather than
                // being discarded entirely.
                let digits: String = part
                    .chars()
                    .take_while(|character| character.is_ascii_digit())
                    .collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect()
    };
    let ordering = release(left).cmp(&release(right));
    if ordering != std::cmp::Ordering::Equal {
        return ordering;
    }
    // Same release numbers: a pre-release is older than the final release.
    match (is_pep440_prerelease(left), is_pep440_prerelease(right)) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => left.cmp(right),
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
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let index = self.resolved_index(ctx).await?;
        let versions = crate::python_index::list_versions(
            &ctx.client,
            &index,
            &self.project,
            ctx.config.settings.offline,
        )
        .await?;

        // Sort by PEP 440 order rather than lexically, or `0.10.0` would sort
        // below `0.9.0` and `latest` would resolve to an older release.
        let mut sorted = versions;
        sorted.sort_by(|left, right| compare_pep440(left, right));
        sorted.dedup();
        Ok(sorted
            .into_iter()
            .map(|version| {
                // Pre-releases are marked unstable so `latest` skips them, while an
                // exact request for one still resolves. PEP 440 spells them with a
                // letter in the release segment (`1.0rc1`, `2.0b3`, `3.0.dev1`).
                let stable = !is_pep440_prerelease(&version);
                VersionInfo {
                    version,
                    stable,
                    lts: None,
                }
            })
            .collect())
    }

    #[cfg(feature = "install")]
    async fn resolve_version(&self, _ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        // A literal version resolves without touching the network, which is what
        // `osdk install pypi:uv@0.12.13` needs.
        //
        // `Prefix` counts as literal here, which looks wrong until you notice
        // that the shared parser only calls a version `Exact` when it is a full
        // three-part semver. Python versions are not semver and have no fixed
        // segment count -- `cowsay@6.1`, `certifi@2026.7.22` -- so treating
        // `Prefix` as "not a real version" rejects the majority of genuine PyPI
        // releases. Verified end to end: `pypi:cowsay@6.1` parses to
        // `Prefix("6.1")` and was refused before this.
        //
        // The requirement built from this is `==`-pinned, so the installer still
        // treats it as one exact release rather than a range.
        let literal = match &req.spec {
            crate::version::VersionSpec::Exact(version)
            | crate::version::VersionSpec::Prefix(version)
            | crate::version::VersionSpec::Pinned(version) => Some(version.clone()),
            _ => None,
        };
        if let Some(version) = literal {
            let mut tv = ToolVersion::new(self.id(), version);
            tv.options = req.options.clone();
            return Ok(tv);
        }
        // Anything else -- `latest`, a range -- needs the index. Resolution goes
        // through the shared selector so prerelease policy and range semantics
        // behave the same here as for every other backend.
        let versions = self.list_remote_versions(_ctx).await?;
        let chosen = crate::version::select_version(&req.spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id.clone(),
                spec: req.spec.to_string(),
                hint: Some(format!(
                    "the index lists {} release(s) for `{}`, none matching",
                    versions.len(),
                    self.project
                )),
            }
        })?;
        let mut tv = ToolVersion::new(self.id(), chosen.version.clone());
        tv.options = req.options.clone();
        Ok(tv)
    }

    #[cfg(feature = "install")]
    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let interpreter = managed_interpreter(ctx, &tv.options)?;
        let choice = choose_installer(locate_uv(ctx).as_deref(), require_uv(&tv.options))?;
        if let Some(notice) = &choice.notice {
            // Printed rather than logged: the capability difference has to reach
            // the person running the command, not only a log file.
            eprintln!("osdk: {notice}");
        }

        // A dynamic install lives at `install_path/<install_id>`, not at
        // `install_path` itself. Writing to the latter produced an environment
        // that was complete on disk yet invisible to the shim, which reported
        // `has no complete selected install` right after a successful install.
        let identity = pypi_install_identity(ctx, self.id(), tv)?;
        let locator = crate::dirs::InstallLocator::new(&ctx.dirs, identity)?;
        let _lock = crate::backend::dynamic::acquire_install_lock(&locator, "pypi").await?;
        let root = locator.install_root().to_path_buf();

        // Build directly at the final location. A virtual environment cannot be
        // relocated after creation, so the usual "build in scratch, then rename"
        // pattern does not apply here.
        //
        // This is not a style preference. Windows console scripts are launcher
        // executables with the interpreter's absolute path embedded in them, and
        // `pyvenv.cfg` records absolute paths as well. Building in
        // `cache/tmp/...` and moving the tree produced exactly that failure:
        // `cowsay.exe` shipped with a shebang pointing into the scratch
        // directory, so after the move it exited 1 with no output at all -- the
        // hardest kind of failure to attribute, because the install reported
        // success and the file was present and executable.
        //
        // Atomicity is preserved a different way: the completion marker and the
        // inventory manifest are written only after the install succeeds, and a
        // failure removes the whole directory. An interrupted run therefore
        // leaves a directory with no marker, which every reader treats as absent.
        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|error| Error::io(&root, error))?;
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }

        let result = self.build_environment(ctx, tv, &choice, &interpreter, &root);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return result;
        }

        // The completion marker alone is not enough for a dynamic namespace.
        // `osdk exec` and the shim locate a dynamic install through the
        // inventory manifest, so without it the tool installs "successfully"
        // and is then unusable -- which is exactly what the first end-to-end
        // run produced: `has no complete install matching its unlocked request`
        // immediately after `installed pypi:cowsay@6.1`.
        finalize_pypi_install(self, ctx, tv, &root)
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        // A dynamic install cannot use the default implementation. That one
        // looks for `.osdk-complete` directly under `install_path/<version>`,
        // while a dynamic install keeps it one level deeper, under the
        // identity-derived `<install_id>` directory. The result was an install
        // that worked through `osdk exec` yet never appeared in `osdk list`, and
        // whose commands `reshim` therefore never generated -- the tool was
        // usable only if you already knew its full id.
        //
        // Scan only this backend's own subtree: `reshim` reaches this once per
        // backend per version, so walking every unrelated tool's installs would
        // be paid on every call.
        //
        // Tolerant: listing what is installed must not fail wholesale because
        // one environment is damaged, or the user cannot see the rest well
        // enough to remove the broken one.
        let report = crate::inventory::scan_installs_for_tool(
            &ctx.dirs.installs,
            self.id(),
            &crate::inventory::ScanOptions::tolerant(),
        )?;
        let mut versions: Vec<String> = report
            .installs
            .iter()
            .filter(|install| {
                let identity = &install.manifest.identity;
                identity.tool == self.id()
                    && identity.platform == ctx.platform.to_string()
                    && identity.scope == crate::tool::InstallScope::Isolated
                    && install.install_root.join(".osdk-complete").is_file()
            })
            .map(|install| install.manifest.identity.version.clone())
            .collect();
        versions.sort();
        versions.dedup();
        Ok(versions)
    }

    fn dynamic_install_identity(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<Option<crate::tool::InstallIdentity>> {
        // The identity is knowable without solving anything, unlike conda's,
        // because the environment is keyed by the request rather than by a
        // resolved closure.
        Ok(Some(pypi_install_identity(ctx, self.id(), tv)?))
    }

    fn validate_dynamic_install(
        &self,
        _ctx: &Ctx,
        _tv: &ToolVersion,
        install_root: &Path,
        identity: &crate::tool::InstallIdentity,
    ) -> Result<bool> {
        if identity.scope != crate::tool::InstallScope::Isolated
            || !install_root.join(".osdk-complete").is_file()
            || !crate::inventory::DynamicToolManifest::manifest_path(install_root).is_file()
        {
            return Ok(false);
        }
        // The environment has to still have an interpreter: a venv whose
        // interpreter was removed (or whose managed Python was uninstalled)
        // looks complete on disk yet cannot run anything.
        Ok(venv_python(install_root).is_file())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // One environment per tool: executables live in its own bin directory,
        // under the identity-derived install root rather than beside it.
        Ok(vec![venv_bin_dir(&self.install_root(ctx, tv)?)])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(tool_bin_names(&self.install_root(ctx, tv)?))
    }
}

/// The commands an environment exposes, given its root.
///
/// The root is a parameter rather than something derived inside, because the
/// caller that matters most -- publishing the inventory just after the
/// environment is moved into place -- already knows the real path. Deriving it
/// independently is how the first attempt wrote an empty `bins` list while
/// `cowsay.exe` sat in `Scripts/`: at that moment the derived path did not exist
/// yet, so the scan found nothing and reported success.
///
/// Discovery reads the directory instead of assuming the console script matches
/// the project name, which frequently it does not.
pub fn tool_bin_names(root: &Path) -> Vec<String> {
    let bin_dir = venv_bin_dir(root);
    let mut names = crate::backend::bin_names_in_dirs(std::slice::from_ref(&bin_dir));
    // Only what the requested tool itself provides. Everything the environment
    // comes with belongs to the interpreter, and claiming it has two costs: it
    // would let `pypi:ruff` shadow the managed `python`, and any two pypi tools
    // would collide over the same name.
    //
    // That collision is not hypothetical. Installing `pypi:cowsay` and
    // `pypi:requests` together failed with "refusing to generate managed shim
    // `pydoc` because it is provided by multiple installed tools" -- `pydoc` is a
    // stdlib script that every venv carries, so both environments honestly
    // claimed it and shim generation had to refuse. The install itself had
    // already succeeded, which made the failure look unrelated to either tool.
    names.retain(|name| !is_interpreter_plumbing(name));
    names
}

/// Whether a command in a venv's bin directory comes from the interpreter
/// rather than from the installed tool.
///
/// The list is what a bare `python -m venv` / `uv venv` produces before anything
/// is installed into it, so none of it identifies the tool the user asked for.
fn is_interpreter_plumbing(name: &str) -> bool {
    let stem = name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(name)
        .to_ascii_lowercase();

    // The interpreter and its installer.
    if matches!(
        stem.as_str(),
        "python" | "pythonw" | "python3" | "pip" | "pip3" | "pipx"
    ) {
        return true;
    }
    // Version-suffixed variants: `pip3.14`, `python3.14`.
    if stem.starts_with("pip3.") || stem.starts_with("python3.") {
        return true;
    }
    // Activation scripts, which are shell fragments rather than commands.
    if stem.starts_with("activate") || stem.starts_with("deactivate") {
        return true;
    }
    // stdlib console scripts a venv inherits. `pydoc` is the one that actually
    // broke a two-tool install; the rest are here because they arrive by the
    // same route and would break the same way.
    matches!(
        stem.as_str(),
        "pydoc" | "pydoc3" | "idle" | "idle3" | "2to3" | "wheel" | "easy_install" | "easy-install"
    ) || stem.starts_with("pydoc3.")
        || stem.starts_with("idle3.")
        || stem.starts_with("2to3-")
        || stem.starts_with("easy_install-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version ordering must be numeric, not lexical.
    ///
    /// A lexical sort puts `0.9.0` above `0.10.0`, which would make `latest`
    /// resolve to an older release -- a silent wrong answer rather than an error.
    #[cfg(feature = "install")]
    #[test]
    fn versions_order_numerically_so_latest_is_actually_latest() {
        let mut versions = vec![
            "0.9.0".to_string(),
            "0.10.0".to_string(),
            "0.12.13".to_string(),
            "0.2.0".to_string(),
            "1.0.0".to_string(),
        ];
        versions.sort_by(|left, right| compare_pep440(left, right));
        assert_eq!(versions, ["0.2.0", "0.9.0", "0.10.0", "0.12.13", "1.0.0"]);
        // The specific pair a lexical sort gets wrong.
        assert_eq!(
            compare_pep440("0.10.0", "0.9.0"),
            std::cmp::Ordering::Greater
        );

        // Non-semver shapes PyPI actually uses must order sensibly too.
        assert_eq!(
            compare_pep440("2026.7.22", "2026.1.4"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_pep440("6.1", "6.0"), std::cmp::Ordering::Greater);
        assert_eq!(compare_pep440("6.1", "6.1"), std::cmp::Ordering::Equal);
    }

    /// PEP 440 pre-releases carry no `-`, so the semver check does not see them.
    #[cfg(feature = "install")]
    #[test]
    fn pep440_prereleases_are_recognized_without_a_dash() {
        for prerelease in ["1.0rc1", "2.0b3", "1.0a2", "3.0.dev1", "1.0c1", "1.0.0rc2"] {
            assert!(
                is_pep440_prerelease(prerelease),
                "`{prerelease}` is a pre-release"
            );
        }
        for final_release in ["1.0", "2.0.0", "0.12.13", "2026.7.22", "1.0.post1"] {
            assert!(
                !is_pep440_prerelease(final_release),
                "`{final_release}` is a final release"
            );
        }

        // A pre-release sorts below the final release with the same numbers, so
        // `latest` prefers the final one.
        assert_eq!(compare_pep440("1.0rc1", "1.0"), std::cmp::Ordering::Less);
    }

    /// Filenames are where versions come from when an index omits PEP 700's
    /// `versions` key, which several mirrors do.
    #[test]
    fn versions_are_extracted_from_distribution_filenames() {
        use crate::python_index::version_from_filename;

        assert_eq!(
            version_from_filename("uv-0.12.13-py3-none-win_amd64.whl").as_deref(),
            Some("0.12.13")
        );
        assert_eq!(
            version_from_filename("cowsay-6.1.tar.gz").as_deref(),
            Some("6.1")
        );
        // A project name containing a dash: splitting from the left would yield
        // the wrong field.
        assert_eq!(
            version_from_filename("typing-extensions-4.12.2.tar.gz").as_deref(),
            Some("4.12.2")
        );
        assert_eq!(
            version_from_filename("typing_extensions-4.12.2-py3-none-any.whl").as_deref(),
            Some("4.12.2")
        );

        // Anything unrecognized is skipped rather than guessed at: a wrong
        // version here would be installed as though it had been requested.
        assert_eq!(version_from_filename("index.html"), None);
        assert_eq!(version_from_filename("uv.whl"), None);
        assert_eq!(version_from_filename(""), None);
    }

    /// User-facing notices must not carry stray whitespace or backslashes.
    ///
    /// Both spellings that produce this are valid Rust and look right in review:
    /// `\` before a newline is a continuation, `\\` is a literal backslash
    /// followed by a real newline and every space of the source indentation. The
    /// second shipped twice here -- once as `Installing uv \` plus a block of
    /// spaces, then again as a double space after collapsing it -- because
    /// nothing fails at compile time and the strings read correctly in the file.
    /// This asserts on the value rather than the source, which is the only place
    /// the difference is visible.
    #[cfg(feature = "install")]
    #[test]
    fn installer_notices_contain_no_stray_whitespace_or_backslashes() {
        // `None` means "no uv candidate", which is the fallback path.
        let messages = [
            choose_installer(None, false)
                .expect("stdlib fallback is available")
                .notice
                .expect("fallback carries a notice"),
            match choose_installer(None, true) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("--require-uv must fail when uv is absent"),
            },
        ];

        for message in messages {
            for (needle, description) in [
                ("\\", "a backslash"),
                ("  ", "a double space"),
                ("\n", "a newline"),
                ("\t", "a tab"),
            ] {
                assert!(
                    !message.contains(needle),
                    "message must not contain {description}: {message}"
                );
            }
        }
    }

    /// A venv's inherited stdlib scripts must not be claimed as tool commands.
    ///
    /// Every venv carries `pydoc`, so two pypi tools both claiming it made shim
    /// generation refuse the whole batch: "refusing to generate managed shim
    /// `pydoc` because it is provided by multiple installed tools". Reproduced by
    /// installing `pypi:cowsay` and `pypi:requests` together -- the installs
    /// succeeded and then the command failed, so the error pointed at neither
    /// tool. A single-tool test cannot see this, which is why it survived until a
    /// two-tool lockfile replay.
    #[test]
    fn inherited_interpreter_commands_are_not_claimed_by_the_tool() {
        // What a bare venv provides, on either platform.
        for inherited in [
            "python",
            "python.exe",
            "pythonw.exe",
            "python3",
            "python3.14",
            "pip",
            "pip.exe",
            "pip3",
            "pip3.14",
            "pydoc",
            "pydoc.exe",
            "pydoc3",
            "pydoc3.14",
            "idle",
            "idle3",
            "2to3",
            "2to3-3.14",
            "activate",
            "activate.bat",
            "deactivate.bat",
            "wheel",
            "easy_install",
            "easy_install-3.14",
        ] {
            assert!(
                is_interpreter_plumbing(inherited),
                "`{inherited}` comes from the interpreter, not the tool"
            );
        }

        // Real tool commands must still come through, including ones whose names
        // start like a filtered entry.
        for owned in [
            "cowsay",
            "cowsay.exe",
            "ruff",
            "http",
            "httpie",
            "uv",
            "uvx",
            "pytest",
            "python-dotenv",
            "pipdeptree",
            "idlemer",
        ] {
            assert!(
                !is_interpreter_plumbing(owned),
                "`{owned}` belongs to the tool and must be exposed"
            );
        }
    }

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
            None,
        );
        assert_eq!(
            pip.get("PIP_INDEX_URL").map(String::as_str),
            Some("https://mirror.test/simple/")
        );
        assert!(!pip.contains_key("PIP_EXTRA_INDEX_URL"));
    }

    /// The managed cache must be named explicitly for a spawned installer.
    ///
    /// `crate::cache` redirects these for interactive shells, but a subprocess
    /// osdk spawns does not inherit that. Without this, uv used its own default
    /// location -- and since the hard-link sharing between environments happens
    /// *through* that cache, the whole reason to prefer uv quietly stopped
    /// applying. Verified end to end: two environments shared no inode until the
    /// cache was passed here.
    #[test]
    fn the_managed_cache_is_passed_to_the_installer_explicitly() {
        let cache = PathBuf::from("/osdk/cache/pkg/uv");
        let uv = installer_env(EnvCreator::Uv, None, false, false, None, Some(&cache));
        assert_eq!(
            uv.get("UV_CACHE_DIR").map(String::as_str),
            Some("/osdk/cache/pkg/uv")
        );

        let pip_cache = PathBuf::from("/osdk/cache/pkg/pip");
        let pip = installer_env(
            EnvCreator::Stdlib,
            None,
            false,
            false,
            None,
            Some(&pip_cache),
        );
        assert_eq!(
            pip.get("PIP_CACHE_DIR").map(String::as_str),
            Some("/osdk/cache/pkg/pip")
        );

        // Each installer gets only its own variable, so a pip run cannot be
        // pointed at uv's cache layout or the other way round.
        assert!(!uv.contains_key("PIP_CACHE_DIR"));
        assert!(!pip.contains_key("UV_CACHE_DIR"));
    }

    /// pip reads `pip.conf`; uv does not. The pip path must therefore pin the
    /// config file, or a stale user-level config could redirect the index.
    #[test]
    fn the_pip_path_pins_its_config_file() {
        let config = PathBuf::from("/tmp/osdk/pip.conf");
        let pip = installer_env(EnvCreator::Stdlib, None, false, false, Some(&config), None);
        assert_eq!(
            pip.get("PIP_CONFIG_FILE").map(String::as_str),
            Some("/tmp/osdk/pip.conf")
        );

        // uv ignores pip.conf by design, so pinning it there would be noise that
        // implies a protection that is not actually in play.
        let uv = installer_env(EnvCreator::Uv, None, false, false, Some(&config), None);
        assert!(!uv.contains_key("PIP_CONFIG_FILE"));
    }

    #[test]
    fn offline_and_hash_enforcement_reach_both_installers() {
        let uv = installer_env(EnvCreator::Uv, None, true, true, None, None);
        assert_eq!(uv.get("UV_OFFLINE").map(String::as_str), Some("1"));
        assert_eq!(
            uv.get("UV_REQUIRE_HASHES").map(String::as_str),
            Some("true")
        );

        let pip = installer_env(EnvCreator::Stdlib, None, true, true, None, None);
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

    /// uv gets `--python <abs path>`; the fallback runs the interpreter itself.
    /// Both must name the interpreter explicitly rather than inherit one.
    #[cfg(feature = "install")]
    #[test]
    fn both_paths_build_against_the_interpreter_osdk_resolved() {
        let interpreter = PathBuf::from("/managed/python/3.14.7/python.exe");
        let venv = PathBuf::from("/installs/pypi/ruff/0.6.9");

        let uv_choice = InstallerChoice {
            creator: EnvCreator::Uv,
            uv: Some(PathBuf::from("/tools/uv.exe")),
            notice: None,
        };
        let (program, args) = venv_command(&uv_choice, &interpreter, &venv);
        assert_eq!(program, PathBuf::from("/tools/uv.exe"));
        assert_eq!(args[0], "venv");
        let python_flag = args.iter().position(|arg| arg == "--python").unwrap();
        assert_eq!(args[python_flag + 1], interpreter.display().to_string());

        let stdlib_choice = InstallerChoice {
            creator: EnvCreator::Stdlib,
            uv: None,
            notice: None,
        };
        let (program, args) = venv_command(&stdlib_choice, &interpreter, &venv);
        // The interpreter *is* the program here, so the environment cannot be
        // built against a different Python than the one osdk chose.
        assert_eq!(program, interpreter);
        assert_eq!(args[0], "-m");
        assert_eq!(args[1], "venv");
    }

    /// Installs must target the environment explicitly on both paths.
    #[cfg(feature = "install")]
    #[test]
    fn installs_target_the_environment_and_never_a_global_pip() {
        let venv = PathBuf::from("/installs/pypi/ruff/0.6.9");

        let uv_choice = InstallerChoice {
            creator: EnvCreator::Uv,
            uv: Some(PathBuf::from("/tools/uv.exe")),
            notice: None,
        };
        let (program, args) = install_command(&uv_choice, &venv, "ruff==0.6.9");
        assert_eq!(program, PathBuf::from("/tools/uv.exe"));
        assert_eq!(&args[0..2], &["pip".to_string(), "install".to_string()]);
        // Explicit target: relying on an ambient VIRTUAL_ENV would make the
        // destination depend on how osdk itself was invoked.
        let python_flag = args.iter().position(|arg| arg == "--python").unwrap();
        assert_eq!(PathBuf::from(&args[python_flag + 1]), venv_python(&venv));
        assert!(args.contains(&"ruff==0.6.9".to_string()));

        let stdlib_choice = InstallerChoice {
            creator: EnvCreator::Stdlib,
            uv: None,
            notice: None,
        };
        let (program, args) = install_command(&stdlib_choice, &venv, "ruff==0.6.9");
        // The environment's own interpreter runs its own pip. A bare `pip` on
        // PATH would install into whichever Python owns it -- which is exactly
        // how packages end up in a system Python nobody asked for.
        assert_eq!(program, venv_python(&venv));
        assert_eq!(
            &args[0..3],
            &["-m".to_string(), "pip".to_string(), "install".to_string()]
        );
    }

    /// Absent uv falls back with an explanation; `require_uv` fails closed.
    #[cfg(feature = "install")]
    #[test]
    fn missing_uv_falls_back_loudly_unless_uv_was_required() {
        let choice = choose_installer(None, false).unwrap();
        assert_eq!(choice.creator, EnvCreator::Stdlib);
        let notice = choice.notice.expect("a fallback must be explained");
        // The user has to learn three things: which path ran, what it costs,
        // and how to get the better one.
        assert!(notice.contains("python -m venv"), "{notice}");
        assert!(notice.contains("share dependencies"), "{notice}");
        assert!(notice.contains("osdk install pypi:uv"), "{notice}");

        let error = choose_installer(None, true).unwrap_err();
        assert!(error.to_string().contains("uv is required"));
    }

    /// A path that resolves but cannot be started must not be treated as uv.
    ///
    /// This is the Windows trap mise handles in two places: a `uv.ps1` or a
    /// shebang-only file passes a path lookup yet fails to spawn.
    #[cfg(feature = "install")]
    #[test]
    fn a_present_but_unspawnable_uv_is_not_used() {
        let temp = tempfile::tempdir().unwrap();
        // A text file named like an executable: findable, not runnable.
        let fake = temp
            .path()
            .join(if cfg!(windows) { "uv.exe" } else { "uv" });
        std::fs::write(&fake, b"#!/bin/sh\\necho not really uv\\n").unwrap();

        assert!(!uv_is_spawnable(&fake));

        let choice = choose_installer(Some(&fake), false).unwrap();
        assert_eq!(choice.creator, EnvCreator::Stdlib);
        let notice = choice.notice.expect("an unusable uv must be explained");
        // "not installed" would send the user looking in the wrong place, since
        // the file is right there.
        assert!(notice.contains("could not be started"), "{notice}");

        // With uv required, this is an error rather than a silent downgrade.
        let error = choose_installer(Some(&fake), true).unwrap_err();
        assert!(error.to_string().contains("could not be started"));
    }

    #[test]
    fn environment_plumbing_is_not_exposed_as_tool_commands() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path());

        let backend = PypiBackend::from_id("pypi:ruff").unwrap();
        let tv = ToolVersion::new(backend.id(), "0.6.9");
        // The identity-derived root, not `install_path` itself: a dynamic install
        // lives one level deeper, and writing to the shallower path is what made
        // an earlier version of this look correct while discovering nothing.
        let bin_dir = venv_bin_dir(&backend.install_root(&ctx, &tv).unwrap());
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
