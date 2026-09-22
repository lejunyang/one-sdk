//! Node ecosystem providers: npm, pnpm, yarn, bun.
//!
//! Every command here was verified against real package managers on
//! 2026-09-22 (npm 10.9.8, pnpm 9.15.1 and 12.5.1, yarn 1.22.19 and 4.6.0,
//! bun 1.4.2) by installing a local dependency whose lifecycle scripts append
//! to a marker file -- so "were scripts disabled" is answered by the marker's
//! contents, not by an exit code. Two findings shaped this module and are worth
//! stating where the code lives:
//!
//! 1. **`--frozen-lockfile` is not a guarantee.** With no lockfile present,
//!    yarn classic and bun accept the flag, install anyway and write no lock
//!    (exit 0). npm's `ci`, pnpm and yarn berry all fail. So osdk checks for the
//!    native lock *itself* rather than delegating that promise -- see
//!    [`plan`]'s downgrade path.
//! 2. **yarn berry has no `--ignore-scripts`.** It answers `Unsupported option
//!    name`; scripts are disabled with `YARN_ENABLE_SCRIPTS=false`. That is why
//!    a plan carries env as well as args.

use std::collections::BTreeMap;
use std::path::Path;

use super::{
    DeclaredManager, DepsProviderSchema, DetectedProject, Ecosystem, InstallerChoice, OutputSpec,
    ProviderConfig, RequiredTool, RunPlan, ToolRole, DEFAULT_TRUST_PROFILE,
};
use crate::error::{Error, Result};

const NODE_MODULES: OutputSpec = OutputSpec::Required("node_modules");

pub static NPM: DepsProviderSchema = DepsProviderSchema {
    id: "npm",
    ecosystem: Ecosystem::Node,
    manifests: &["package.json"],
    native_locks: &["package-lock.json", "npm-shrinkwrap.json"],
    default_sources: &["package.json", "package-lock.json"],
    default_outputs: &[NODE_MODULES],
    // npm ships with node, so the runtime is the only tool to install.
    required_tools: &[RequiredTool {
        id: "node",
        role: ToolRole::Runtime,
    }],
    trust: DEFAULT_TRUST_PROFILE,
};

pub static PNPM: DepsProviderSchema = DepsProviderSchema {
    id: "pnpm",
    ecosystem: Ecosystem::Node,
    manifests: &["package.json"],
    native_locks: &["pnpm-lock.yaml"],
    default_sources: &["package.json", "pnpm-lock.yaml"],
    default_outputs: &[NODE_MODULES],
    required_tools: &[
        RequiredTool {
            id: "node",
            role: ToolRole::Runtime,
        },
        RequiredTool {
            id: "pnpm",
            role: ToolRole::Installer,
        },
    ],
    trust: DEFAULT_TRUST_PROFILE,
};

pub static YARN: DepsProviderSchema = DepsProviderSchema {
    id: "yarn",
    ecosystem: Ecosystem::Node,
    manifests: &["package.json"],
    native_locks: &["yarn.lock"],
    default_sources: &["package.json", "yarn.lock"],
    default_outputs: &[NODE_MODULES],
    required_tools: &[
        RequiredTool {
            id: "node",
            role: ToolRole::Runtime,
        },
        RequiredTool {
            id: "yarn",
            role: ToolRole::Installer,
        },
    ],
    trust: DEFAULT_TRUST_PROFILE,
};

pub static BUN: DepsProviderSchema = DepsProviderSchema {
    id: "bun",
    ecosystem: Ecosystem::Node,
    manifests: &["package.json"],
    // `bun.lock` is the current text format; `bun.lockb` the older binary one.
    native_locks: &["bun.lock", "bun.lockb"],
    default_sources: &["package.json", "bun.lock", "bun.lockb"],
    default_outputs: &[NODE_MODULES],
    required_tools: &[RequiredTool {
        id: "bun",
        role: ToolRole::Installer,
    }],
    trust: DEFAULT_TRUST_PROFILE,
};

/// Which installer owns a Node lockfile, by file name.
pub fn lock_owner(file_name: &str) -> Option<&'static str> {
    match file_name {
        "package-lock.json" | "npm-shrinkwrap.json" => Some("npm"),
        "pnpm-lock.yaml" => Some("pnpm"),
        "yarn.lock" => Some("yarn"),
        "bun.lock" | "bun.lockb" => Some("bun"),
        _ => None,
    }
}

/// Read `packageManager` out of a Node manifest.
///
/// A manifest that cannot be parsed is an error rather than "no declaration":
/// treating it as absent would fall through to a different installer and
/// install the tree with something the project did not ask for.
pub fn declared_manager(
    schema: &DepsProviderSchema,
    manifest: &Path,
) -> Result<Option<DeclaredManager>> {
    if schema.ecosystem != Ecosystem::Node {
        return Ok(None);
    }
    let text = std::fs::read_to_string(manifest).map_err(|error| Error::io(manifest, error))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| Error::config(format!("{}: {error}", manifest.display())))?;
    Ok(value
        .get("packageManager")
        .and_then(serde_json::Value::as_str)
        .and_then(DeclaredManager::parse))
}

/// Yarn's two major lines take different flags for the same intent, and the
/// wrong one is accepted *silently*: classic 1.22.19 takes `--immutable`
/// without complaint and neither freezes nor blocks scripts. So the major has
/// to be decided before the command is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YarnLine {
    Classic,
    Berry,
}

/// Decide the yarn line from the version the project pinned.
///
/// Unknown or absent versions resolve to berry: that is what corepack installs
/// for a modern project, and berry *fails* on a missing lockfile rather than
/// silently continuing, so guessing it is the safer default of the two.
pub fn yarn_line(version: Option<&str>) -> YarnLine {
    let Some(version) = version else {
        return YarnLine::Berry;
    };
    let major = version
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .next()
        .and_then(|major| major.parse::<u64>().ok());
    match major {
        Some(1) => YarnLine::Classic,
        _ => YarnLine::Berry,
    }
}

/// Build the install command for a Node project.
pub fn plan(
    project: &DetectedProject,
    choice: &InstallerChoice,
    config: &ProviderConfig,
    tool_versions: &BTreeMap<String, String>,
) -> Result<RunPlan> {
    let has_lock = project.native_lock.is_some();
    let mut env = BTreeMap::new();
    let mut args: Vec<String> = Vec::new();
    let mut downgraded_reason = None;

    // The version that decides yarn's flag set: what the project declared, else
    // what osdk resolved for the tool.
    let declared_version = choice
        .version
        .clone()
        .or_else(|| tool_versions.get(project.provider).cloned());

    let (tool, program_candidates) = match project.provider {
        "npm" => ("node", vec!["npm.cmd".to_string(), "npm".to_string()]),
        "pnpm" => ("pnpm", vec!["pnpm.cmd".to_string(), "pnpm".to_string()]),
        "yarn" => ("yarn", vec!["yarn.cmd".to_string(), "yarn".to_string()]),
        "bun" => ("bun", vec!["bun.exe".to_string(), "bun".to_string()]),
        other => {
            return Err(Error::other(format!(
                "`{other}` is not a Node deps provider"
            )))
        }
    };

    match project.provider {
        "npm" => {
            if has_lock {
                args.push("ci".into());
            } else {
                args.push("install".into());
                downgraded_reason = Some(format!(
                    "no package-lock.json in {}; running a non-frozen `npm install`, \
                     which will create one",
                    project.root.display()
                ));
            }
            args.push("--ignore-scripts".into());
        }
        "pnpm" => {
            args.push("install".into());
            if has_lock {
                args.push("--frozen-lockfile".into());
            } else {
                downgraded_reason = Some(format!(
                    "no pnpm-lock.yaml in {}; running a non-frozen `pnpm install`, \
                     which will create one",
                    project.root.display()
                ));
            }
            args.push("--ignore-scripts".into());
        }
        "yarn" => {
            args.push("install".into());
            match yarn_line(declared_version.as_deref()) {
                YarnLine::Berry => {
                    if has_lock {
                        args.push("--immutable".into());
                    } else {
                        downgraded_reason = Some(format!(
                            "no yarn.lock in {}; running a non-immutable `yarn install`, \
                             which will create one",
                            project.root.display()
                        ));
                    }
                    // Berry rejects `--ignore-scripts` outright; this is the
                    // supported way to keep build scripts off.
                    env.insert("YARN_ENABLE_SCRIPTS".into(), "false".into());
                }
                YarnLine::Classic => {
                    // Classic accepts `--frozen-lockfile` even with no lock and
                    // installs anyway, so the guarantee comes from osdk's own
                    // check above, not from the flag.
                    if has_lock {
                        args.push("--frozen-lockfile".into());
                    } else {
                        downgraded_reason = Some(format!(
                            "no yarn.lock in {}; yarn classic would accept \
                             --frozen-lockfile and install anyway, so osdk runs a \
                             plain install and reports it",
                            project.root.display()
                        ));
                    }
                    args.push("--ignore-scripts".into());
                    args.push("--non-interactive".into());
                }
            }
        }
        "bun" => {
            args.push("install".into());
            if has_lock {
                args.push("--frozen-lockfile".into());
            } else {
                downgraded_reason = Some(format!(
                    "no bun.lock in {}; bun would accept --frozen-lockfile and \
                     install anyway, so osdk runs a plain install and reports it",
                    project.root.display()
                ));
            }
            args.push("--ignore-scripts".into());
        }
        _ => unreachable!("provider validated above"),
    }

    // Opting into build scripts drops the flag again. This is the key that makes
    // the entry trust-requiring (`ExecutesCode`), so it must be the only way to
    // get here.
    if config.allow_build_from_source {
        args.retain(|arg| arg != "--ignore-scripts");
        env.remove("YARN_ENABLE_SCRIPTS");
    }

    if let Some(index) = &config.index {
        // Registry selection is mapped onto the *default* registry only. An
        // extra registry ranked above the default is a dependency-confusion
        // vector, so osdk never writes one.
        env.insert("NPM_CONFIG_REGISTRY".into(), index.clone());
        if project.provider == "yarn" {
            env.insert("YARN_NPM_REGISTRY_SERVER".into(), index.clone());
        }
    }
    for (key, value) in &config.env {
        env.insert(key.clone(), value.clone());
    }

    Ok(RunPlan {
        tool,
        program_candidates,
        args,
        env,
        cwd: super::effective_cwd(&project.root, config.dir.as_deref()),
        frozen: has_lock,
        downgraded_reason,
        // Node installers create whatever they need themselves.
        prelude: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::plan as build;
    use super::*;
    use std::path::PathBuf;

    fn project(provider: &'static str, lock: Option<&str>) -> DetectedProject {
        DetectedProject {
            provider,
            ecosystem: Ecosystem::Node,
            root: PathBuf::from("/p"),
            manifest: PathBuf::from("/p/package.json"),
            native_lock: lock.map(|name| PathBuf::from("/p").join(name)),
            declared_manager: None,
        }
    }

    fn choice(provider: &'static str, version: Option<&str>) -> InstallerChoice {
        InstallerChoice {
            provider,
            origin: super::super::InstallerOrigin::Default,
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn frozen_commands_are_used_when_a_lock_exists() {
        let versions = BTreeMap::new();
        let config = ProviderConfig::default();

        let got = build(
            &project("npm", Some("package-lock.json")),
            &choice("npm", None),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(got.args, vec!["ci", "--ignore-scripts"]);
        assert!(got.frozen);
        assert!(got.downgraded_reason.is_none());

        let got = build(
            &project("pnpm", Some("pnpm-lock.yaml")),
            &choice("pnpm", None),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(
            got.args,
            vec!["install", "--frozen-lockfile", "--ignore-scripts"]
        );

        let got = build(
            &project("bun", Some("bun.lock")),
            &choice("bun", None),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(
            got.args,
            vec!["install", "--frozen-lockfile", "--ignore-scripts"]
        );
    }

    /// Yarn classic and bun accept `--frozen-lockfile` with no lockfile and
    /// install anyway (verified against yarn 1.22.19 and bun 1.4.2). osdk must
    /// therefore decide from the lockfile's presence and *say* it downgraded --
    /// silently passing a flag that does nothing is the failure this guards.
    #[test]
    fn a_missing_lock_downgrades_explicitly_rather_than_passing_a_useless_flag() {
        let versions = BTreeMap::new();
        let config = ProviderConfig::default();

        for (provider, version) in [
            ("npm", None),
            ("pnpm", None),
            ("bun", None),
            ("yarn", Some("1.22.19")),
            ("yarn", Some("4.6.0")),
        ] {
            let got = build(
                &project(provider, None),
                &choice(provider, version),
                &config,
                &versions,
            )
            .unwrap();
            assert!(
                !got.frozen,
                "{provider} without a lock must not claim to be frozen"
            );
            assert!(
                got.downgraded_reason.is_some(),
                "{provider} without a lock must report the downgrade"
            );
            for flag in ["--frozen-lockfile", "--immutable"] {
                assert!(
                    !got.args.iter().any(|arg| arg == flag),
                    "{provider} must not pass {flag} when there is no lock: {:?}",
                    got.args
                );
            }
        }
    }

    /// Yarn's two lines need different flags, and classic accepts berry's
    /// `--immutable` silently without freezing, so choosing by major is not a
    /// nicety.
    #[test]
    fn yarn_dispatches_by_major_and_berry_disables_scripts_through_env() {
        let versions = BTreeMap::new();
        let config = ProviderConfig::default();

        let berry = build(
            &project("yarn", Some("yarn.lock")),
            &choice("yarn", Some("4.6.0")),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(berry.args, vec!["install", "--immutable"]);
        assert_eq!(
            berry.env.get("YARN_ENABLE_SCRIPTS").map(String::as_str),
            Some("false"),
            "berry has no --ignore-scripts; the env var is the only way"
        );
        assert!(
            !berry.args.iter().any(|arg| arg == "--ignore-scripts"),
            "berry rejects --ignore-scripts outright"
        );

        let classic = build(
            &project("yarn", Some("yarn.lock")),
            &choice("yarn", Some("1.22.19")),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(
            classic.args,
            vec![
                "install",
                "--frozen-lockfile",
                "--ignore-scripts",
                "--non-interactive"
            ]
        );
        assert!(
            !classic.env.contains_key("YARN_ENABLE_SCRIPTS"),
            "classic honours the flag, so the berry env var must not be set"
        );

        assert_eq!(yarn_line(Some("1.22.19")), YarnLine::Classic);
        assert_eq!(yarn_line(Some("4.6.0")), YarnLine::Berry);
        assert_eq!(yarn_line(Some("v1.0.0")), YarnLine::Classic);
        // Unknown resolves to berry: berry fails loudly on a missing lock,
        // classic does not, so it is the safer guess.
        assert_eq!(yarn_line(None), YarnLine::Berry);
        assert_eq!(yarn_line(Some("nonsense")), YarnLine::Berry);
    }

    /// Scripts are off unless the entry explicitly opts in. That opt-in is what
    /// makes the entry `ExecutesCode` for trust, so it must be the only path to
    /// a command that can run them.
    #[test]
    fn build_scripts_stay_off_until_explicitly_allowed() {
        let versions = BTreeMap::new();

        for (provider, version) in [
            ("npm", None),
            ("pnpm", None),
            ("bun", None),
            ("yarn", Some("1.22.19")),
        ] {
            let got = build(
                &project(provider, Some(lock_for(provider))),
                &choice(provider, version),
                &ProviderConfig::default(),
                &versions,
            )
            .unwrap();
            assert!(
                got.args.iter().any(|arg| arg == "--ignore-scripts"),
                "{provider} must disable scripts by default: {:?}",
                got.args
            );
        }

        let allowed = ProviderConfig {
            allow_build_from_source: true,
            ..Default::default()
        };
        let got = build(
            &project("pnpm", Some("pnpm-lock.yaml")),
            &choice("pnpm", None),
            &allowed,
            &versions,
        )
        .unwrap();
        assert!(!got.args.iter().any(|arg| arg == "--ignore-scripts"));

        let got = build(
            &project("yarn", Some("yarn.lock")),
            &choice("yarn", Some("4.6.0")),
            &allowed,
            &versions,
        )
        .unwrap();
        assert!(
            !got.env.contains_key("YARN_ENABLE_SCRIPTS"),
            "allowing builds must also drop berry's env-based block"
        );
    }

    fn lock_for(provider: &str) -> &'static str {
        match provider {
            "npm" => "package-lock.json",
            "pnpm" => "pnpm-lock.yaml",
            "yarn" => "yarn.lock",
            "bun" => "bun.lock",
            _ => unreachable!(),
        }
    }

    #[test]
    fn lock_ownership_maps_every_supported_lockfile() {
        assert_eq!(lock_owner("package-lock.json"), Some("npm"));
        assert_eq!(lock_owner("npm-shrinkwrap.json"), Some("npm"));
        assert_eq!(lock_owner("pnpm-lock.yaml"), Some("pnpm"));
        assert_eq!(lock_owner("yarn.lock"), Some("yarn"));
        assert_eq!(lock_owner("bun.lock"), Some("bun"));
        assert_eq!(lock_owner("bun.lockb"), Some("bun"));
        assert_eq!(lock_owner("Cargo.lock"), None);
    }

    /// A custom registry is mapped onto the *default* registry only. Writing an
    /// additional registry that outranks the default would be a
    /// dependency-confusion vector.
    #[test]
    fn a_custom_registry_maps_to_the_default_registry_only() {
        let versions = BTreeMap::new();
        let config = ProviderConfig {
            index: Some("https://registry.example.com/".into()),
            ..Default::default()
        };
        let got = build(
            &project("npm", Some("package-lock.json")),
            &choice("npm", None),
            &config,
            &versions,
        )
        .unwrap();
        assert_eq!(
            got.env.get("NPM_CONFIG_REGISTRY").map(String::as_str),
            Some("https://registry.example.com/")
        );
        // Nothing may set an *additional* registry.
        assert!(!got
            .env
            .keys()
            .any(|key| key.contains("EXTRA") || key == "NPM_CONFIG_REGISTRIES"));
    }
}
