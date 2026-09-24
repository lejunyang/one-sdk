//! Python deps providers: `uv` (pyproject + uv.lock) and `pip-requirements`.
//!
//! Two measured facts shape this module, both from the 2026-09-22 probe recorded
//! in `docs/research/osdk-deps-design-2026-09-22.zh-CN.md` §5.4.2 / §5.4.3:
//!
//! 1. **`UV_NO_BUILD` cannot be used to keep source builds off.** `uv sync`
//!    honours it, but `uv pip install` ignores it silently: the probe's
//!    sdist-only package still ran its `setup.py` with the variable set and
//!    exited 0. This is the mirror image of yarn berry, which *rejects*
//!    `--ignore-scripts` and can only be controlled through the environment. So
//!    Python passes flags and never relies on env for this.
//!
//! 2. **`uv sync --frozen` does not check that the lock is current.** With a
//!    lock that disagreed with `pyproject.toml`, it exited 0 and installed the
//!    old set; the newly added dependency simply was not there. Its meaning is
//!    "do not update the lock". Asserting the lock is unchanged is `--locked`, a
//!    different flag. `npm ci` fails in that situation, so the ecosystems are
//!    not symmetric and osdk asks for both.

use std::collections::BTreeMap;
use std::path::Path;

use super::{
    DeclaredManager, DepsProviderSchema, DetectedProject, Ecosystem, InstallerChoice, OutputSpec,
    ProviderConfig, RequiredTool, RunPlan, ToolRole, DEFAULT_TRUST_PROFILE,
};
use crate::error::{Error, Result};

/// `.venv` is `OptionalOnceSeen` rather than `Required` because the environment
/// does not have to live in the project: `UV_PROJECT_ENVIRONMENT` can move it,
/// and `uv pip` can target any interpreter. Treating it as required would report
/// a permanently stale provider for a perfectly good setup -- the same reason
/// the Node providers do not require a global store directory.
const VENV: OutputSpec = OutputSpec::OptionalOnceSeen(".venv");

pub static UV: DepsProviderSchema = DepsProviderSchema {
    id: "uv",
    ecosystem: Ecosystem::Python,
    manifests: &["pyproject.toml"],
    native_locks: &["uv.lock"],
    default_sources: &["pyproject.toml", "uv.lock"],
    default_outputs: &[VENV],
    required_tools: &[
        // Ordered runtime-first so the interpreter's bin directory is on PATH
        // before the installer runs.
        RequiredTool {
            id: "python",
            role: ToolRole::Runtime,
        },
        RequiredTool {
            id: "pypi:uv",
            role: ToolRole::Installer,
        },
    ],
    trust: DEFAULT_TRUST_PROFILE,
};

pub static PIP_REQUIREMENTS: DepsProviderSchema = DepsProviderSchema {
    id: "pip-requirements",
    ecosystem: Ecosystem::Python,
    manifests: &["requirements.txt"],
    // A requirements file is an input to resolution, not proof that it contains
    // the complete transitive closure. Even `name==version` may name only a root
    // package, so this provider has no native lock.
    native_locks: &[],
    default_sources: &["requirements.txt"],
    default_outputs: &[VENV],
    required_tools: &[
        RequiredTool {
            id: "python",
            role: ToolRole::Runtime,
        },
        RequiredTool {
            id: "pypi:uv",
            role: ToolRole::Installer,
        },
    ],
    trust: DEFAULT_TRUST_PROFILE,
};

/// Which Python provider owns a lockfile, by file name.
pub fn lock_owner(file_name: &str) -> Option<&'static str> {
    match file_name {
        "uv.lock" => Some("uv"),
        _ => None,
    }
}

/// Python manifests carry no `packageManager` equivalent.
///
/// `[tool.uv]` in `pyproject.toml` configures uv but does not *declare* that uv
/// is the installer the way `packageManager` does, and `requirements.txt` says
/// nothing at all. Returning `None` keeps the choice with the lockfile and the
/// project's `[deps]` configuration rather than inventing a declaration the file
/// does not make.
pub fn declared_manager(
    _schema: &DepsProviderSchema,
    manifest: &Path,
) -> Result<Option<DeclaredManager>> {
    // Still read it, so an unparseable manifest is an error rather than a silent
    // skip -- walking past a broken pyproject.toml would install a different
    // project's dependencies, or none, and report success.
    if manifest.file_name().and_then(|name| name.to_str()) == Some("pyproject.toml") {
        let text = std::fs::read_to_string(manifest).map_err(|error| Error::io(manifest, error))?;
        toml::from_str::<toml::Value>(&text)
            .map_err(|error| Error::config(format!("{}: {error}", manifest.display())))?;
    }
    Ok(None)
}

/// Build the install command for a Python project.
pub fn plan(
    project: &DetectedProject,
    _choice: &InstallerChoice,
    config: &ProviderConfig,
    _tool_versions: &BTreeMap<String, String>,
) -> Result<RunPlan> {
    let mut args: Vec<String> = Vec::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut downgraded_reason = None;
    let mut frozen = false;
    // Commands that must succeed before the main one. `uv sync` creates its
    // project environment itself, whereas `uv pip install` needs a target venv --
    // so the requirements provider asks for it explicitly rather than assuming
    // the project already has one.
    let mut prelude: Vec<Vec<String>> = Vec::new();

    match project.provider.as_ref() {
        "uv" => {
            args.push("sync".into());
            if project.native_lock.is_some() {
                // Both flags, deliberately. `--frozen` alone was measured to
                // accept a lock that no longer matches pyproject.toml (exit 0,
                // stale set installed); `--locked` is what asserts the lock is
                // current. Passing only one of them would give a weaker
                // guarantee than the Node providers' `npm ci`.
                args.push("--locked".into());
                frozen = true;
            } else {
                downgraded_reason = Some(format!(
                    "no uv.lock in {}; running `uv sync` without --locked, \
                     which will create one",
                    project.root.display()
                ));
            }
        }
        "pip-requirements" => {
            // A requirements file normally lists roots, not the full transitive
            // closure. `uv pip sync` treats it as the *entire* environment and
            // removes every dependency absent from the file; even
            // `aiohttp==3.14.3` therefore installs without multidict/yarl. Use the
            // resolver-backed install form so root requirements bring their
            // dependencies with them. This deliberately gives up pruning extras:
            // a plain requirements file is not a lock and cannot safely define
            // the exact environment set.
            //
            // `--allow-existing` keeps environment creation idempotent: plain
            // `uv venv` exits 2 once the environment exists.
            prelude.push(vec!["venv".into(), "--allow-existing".into()]);
            args.push("pip".into());
            args.push("install".into());
            args.push("--requirements".into());
            let manifest = project
                .manifest
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("requirements.txt");
            args.push(manifest.to_string());
            // Pinning top-level entries does not turn this into a complete lock:
            // transitive versions can still change. Report that limitation and
            // never let `--frozen` accept this provider.
            downgraded_reason = Some(format!(
                "{} is an input requirements file, not a complete dependency lock; \
                 resolving it with `uv pip install`, which installs transitive \
                 dependencies but does not prune packages absent from the file",
                project.manifest.display()
            ));
        }
        other => {
            return Err(Error::other(format!(
                "`{other}` is not a Python deps provider"
            )))
        }
    }

    if config.allow_build_from_source {
        // Nothing to remove: the deny flag is added below only when this is off.
    } else {
        // A flag, never the environment variable. `UV_NO_BUILD` is ignored by
        // `uv pip install` (measured), so relying on it would produce a switch
        // that looks enabled while every sdist still builds locally.
        args.push("--no-build".into());
    }

    // Index redirection maps to uv's own variable. Only the default index is
    // mapped; an extra index is never added implicitly, because that widens
    // where a name can resolve from and is the shape of a confusion attack.
    if let Some(index) = &config.index {
        env.insert("UV_DEFAULT_INDEX".into(), index.clone());
    }
    // uv must never reach out for an interpreter on its own: the one osdk
    // resolved is the one that should be used, and a silent download would put a
    // different runtime under the environment.
    env.insert("UV_PYTHON_DOWNLOADS".into(), "never".into());

    for (key, value) in &config.env {
        env.insert(key.clone(), value.clone());
    }

    Ok(RunPlan {
        tool: "pypi:uv".into(),
        program_candidates: vec!["uv.exe".to_string(), "uv".to_string()],
        args,
        env,
        cwd: super::effective_cwd(&project.root, config.dir.as_deref()),
        frozen,
        downgraded_reason,
        prelude,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(provider: &'static str, manifest: &Path, lock: Option<&Path>) -> DetectedProject {
        DetectedProject {
            provider: provider.into(),
            ecosystem: Ecosystem::Python,
            root: manifest.parent().unwrap().to_path_buf(),
            manifest: manifest.to_path_buf(),
            native_lock: lock.map(Path::to_path_buf),
            declared_manager: None,
        }
    }

    fn choice(provider: &'static str) -> InstallerChoice {
        InstallerChoice {
            provider: provider.into(),
            version: None,
            origin: super::super::InstallerOrigin::Default,
        }
    }

    /// With a lock, uv gets `--locked` and not merely `--frozen`.
    ///
    /// `--frozen` was measured to accept a lock that no longer matches
    /// `pyproject.toml` -- exit 0, stale set installed, the added dependency
    /// missing. A test asserting only "some freeze flag is present" would pass
    /// for the weaker command, so the flag is named explicitly.
    #[test]
    fn uv_with_a_lock_asserts_the_lock_is_current() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        let lock = temp.path().join("uv.lock");
        std::fs::write(&manifest, "[project]\nname = \"p\"\n").unwrap();
        std::fs::write(&lock, "version = 1\n").unwrap();

        let got = plan(
            &project("uv", &manifest, Some(&lock)),
            &choice("uv"),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(got.args[0], "sync");
        assert!(
            got.args.iter().any(|arg| arg == "--locked"),
            "--frozen alone does not check the lock is current: {:?}",
            got.args
        );
        assert!(got.frozen);
        assert!(got.downgraded_reason.is_none());
    }

    /// Without a lock, the downgrade is reported rather than silent.
    #[test]
    fn uv_without_a_lock_reports_the_downgrade() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        std::fs::write(&manifest, "[project]\nname = \"p\"\n").unwrap();

        let got = plan(
            &project("uv", &manifest, None),
            &choice("uv"),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(!got.frozen);
        assert!(!got.args.iter().any(|arg| arg == "--locked"));
        assert!(got.downgraded_reason.is_some());
    }

    /// Source builds are denied with a flag, never with `UV_NO_BUILD`.
    ///
    /// The environment variable is ignored by `uv pip install` (measured: the
    /// probe's sdist-only package built anyway and exited 0). Asserting its
    /// absence keeps a future change from "simplifying" the flag into an env var
    /// that does nothing on half the commands.
    #[test]
    fn source_builds_are_denied_by_flag_not_by_environment() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("requirements.txt");
        std::fs::write(&manifest, "idna==3.10\n").unwrap();

        let got = plan(
            &project("pip-requirements", &manifest, None),
            &choice("pip-requirements"),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(
            got.args.iter().any(|arg| arg == "--no-build"),
            "{:?}",
            got.args
        );
        assert!(
            !got.env.contains_key("UV_NO_BUILD"),
            "UV_NO_BUILD is silently ignored by `uv pip install`; a flag is required"
        );

        // And it comes off when the project asks for source builds.
        let allowed = ProviderConfig {
            allow_build_from_source: true,
            ..ProviderConfig::default()
        };
        let got = plan(
            &project("pip-requirements", &manifest, None),
            &choice("pip-requirements"),
            &allowed,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(!got.args.iter().any(|arg| arg == "--no-build"));
    }

    /// uv is never allowed to fetch its own interpreter.
    #[test]
    fn the_interpreter_is_never_downloaded_behind_osdks_back() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        std::fs::write(&manifest, "[project]\nname = \"p\"\n").unwrap();
        let got = plan(
            &project("uv", &manifest, None),
            &choice("uv"),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            got.env.get("UV_PYTHON_DOWNLOADS").map(String::as_str),
            Some("never")
        );
    }

    /// Only the default index is mapped; an extra index is never added silently.
    #[test]
    fn an_index_maps_to_the_default_index_only() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        std::fs::write(&manifest, "[project]\nname = \"p\"\n").unwrap();
        let config = ProviderConfig {
            index: Some("https://example.com/simple".into()),
            ..ProviderConfig::default()
        };
        let got = plan(
            &project("uv", &manifest, None),
            &choice("uv"),
            &config,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            got.env.get("UV_DEFAULT_INDEX").map(String::as_str),
            Some("https://example.com/simple")
        );
        assert!(!got.env.contains_key("UV_EXTRA_INDEX_URL"));
        assert!(!got.env.contains_key("UV_INDEX_URL"));
    }

    /// A requirements file always uses resolver-backed install rather than sync.
    ///
    /// `uv pip sync requirements.txt` interprets the file as the complete
    /// environment set. A perfectly pinned root such as `aiohttp==3.14.3` is
    /// therefore installed without multidict/yarl and can even remove those
    /// packages on a rerun. The command shape here is the regression boundary:
    /// `install -r` resolves the transitive closure and never claims frozen.
    #[test]
    fn requirements_resolve_transitive_dependencies_instead_of_syncing_roots() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("requirements.txt");
        std::fs::write(&manifest, "aiohttp==3.14.3\n").unwrap();

        let got = plan(
            &project("pip-requirements", &manifest, None),
            &choice("pip-requirements"),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(
            got.args[..3],
            ["pip", "install", "--requirements"],
            "sync silently drops every transitive dependency: {:?}",
            got.args
        );
        assert_eq!(got.args[3], "requirements.txt");
        assert!(
            !got.args.iter().any(|arg| arg == "sync"),
            "sync treats top-level requirements as the entire environment: {:?}",
            got.args
        );
        assert!(!got.frozen, "top-level pins are not a complete lock");
        assert!(got.downgraded_reason.is_some());
    }

    /// A `pyproject.toml` that will not parse is an error, not "no declaration".
    #[test]
    fn an_unparseable_manifest_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        std::fs::write(&manifest, "[project\nname = broken").unwrap();
        assert!(declared_manager(&UV, &manifest).is_err());

        std::fs::write(&manifest, "[project]\nname = \"p\"\n").unwrap();
        assert!(declared_manager(&UV, &manifest).unwrap().is_none());
    }
}
