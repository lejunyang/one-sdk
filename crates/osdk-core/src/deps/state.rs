//! Persisted freshness state for application dependency providers.
//!
//! The model is the one `tasks::freshness` already established: hash the inputs
//! *and* the effective command, keep the state in osdk's managed cache keyed by
//! project, and require declared outputs to exist. Two deliberate properties:
//!
//! - **Nothing is written inside the project.** Derived data beside a manifest
//!   would oblige every consumer to add a `.gitignore` entry, and nobody edits
//!   or commits it. Same reasoning as `tasks::freshness::state_path`.
//! - **The command participates in the hash.** Changing the installer, the tool
//!   version, the registry or the args must re-run the install even when no
//!   source file changed -- otherwise switching registries would silently reuse
//!   a tree fetched from somewhere else.
//!
//! What this layer deliberately does *not* claim: that the installed tree is
//! intact. A hash match only says "the inputs and command are unchanged since
//! the last success". Detecting an externally modified `node_modules` is the
//! job of the deeper verification pass, which is why that exists as a separate
//! level rather than being folded in here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// All providers' last successful runs for one project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DepsState {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderState>,
}

/// One provider's last successful run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderState {
    /// Content hash of sources plus the effective command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Outputs observed after a successful run. An optional output is only
    /// checked for disappearance once it has been seen, so a package manager
    /// that installs outside the project does not look permanently stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen_outputs: Vec<String>,
}

impl DepsState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| Error::other(format!("invalid {}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::io(path, error)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::dirs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| Error::other(format!("cannot serialize deps state: {error}")))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes()).map_err(|error| Error::io(&tmp, error))?;
        std::fs::rename(&tmp, path).map_err(|error| Error::io(path, error))
    }

    /// Record a successful run so the next one has something to compare against.
    ///
    /// Only to be called after the command exited successfully. Recording a hash
    /// for a failed install would make the *next* run report "up to date" for a
    /// tree that was never populated -- worse than recording nothing, because it
    /// turns one visible failure into a silently broken working tree.
    pub fn record(&mut self, provider: &str, inputs: &Inputs<'_>) -> Result<()> {
        let hash = input_hash(inputs.sources, inputs.command)?;
        let seen_outputs = inputs
            .outputs
            .iter()
            .filter(|(path, _)| path.exists())
            // The same normalization `decide` applies on the read side.
            // Calling the shared helper rather than repeating the separator
            // rule is deliberate: two copies drift, and the symptom would be
            // a recorded output that silently never matches the one looked up.
            .map(|(path, _)| super::normalize_relative(&path.to_string_lossy()))
            .collect();
        self.providers.insert(
            provider.to_string(),
            ProviderState {
                hash: Some(hash),
                seen_outputs,
            },
        );
        Ok(())
    }
}

/// Where a project's deps state belongs inside the managed cache.
///
/// Keyed by a hash of the project root so two projects cannot collide, and
/// placed next to the task freshness state for the same reason it is: it is
/// derived data that only osdk reads and writes.
pub fn state_path(cache_dir: &Path, project_root: &Path) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(project_root.to_string_lossy().as_bytes());
    let key = hasher.finalize().to_hex()[..16].to_string();
    cache_dir.join("deps").join(format!("{key}.toml"))
}

/// Hash the freshness inputs: every existing source file's path and contents,
/// plus the effective command.
///
/// A source pattern that matches nothing is *not* silently treated as fresh:
/// the caller passes the resolved list and [`decide`] refuses to call an empty
/// input set fresh. `tasks::freshness` records the same trap -- an empty
/// `sources` makes freshness vacuously true, which looks exactly like a correct
/// cache hit.
pub fn input_hash(sources: &[PathBuf], command: &str) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(command.as_bytes());
    for path in sources {
        let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
        // The path participates so renaming a source is a change.
        hasher.update(super::normalize_relative(&path.to_string_lossy()).as_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Why a provider is going to run (or not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Fresh,
    Stale(String),
}

impl Decision {
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Fresh => None,
            Self::Stale(reason) => Some(reason),
        }
    }
}

/// Inputs to a freshness decision, all already resolved against the filesystem.
pub struct Inputs<'a> {
    /// Source files that actually exist.
    pub sources: &'a [PathBuf],
    /// Whether any source pattern was declared at all.
    pub declared_sources: bool,
    /// Effective command string (program, args, env, tool versions).
    pub command: &'a str,
    /// Declared outputs with their required flag, absolute.
    pub outputs: &'a [(PathBuf, bool)],
}

/// Decide whether a provider needs to run.
pub fn decide(inputs: &Inputs<'_>, state: Option<&ProviderState>) -> Result<Decision> {
    // No recorded success: always run. This also covers a first run.
    let Some(state) = state else {
        return Ok(Decision::Stale("no recorded successful run".into()));
    };
    let Some(recorded) = &state.hash else {
        return Ok(Decision::Stale("no recorded input hash".into()));
    };

    if !inputs.declared_sources {
        // A provider with no declared sources cannot prove anything about its
        // inputs, so it must not be called fresh on that basis.
        return Ok(Decision::Stale(
            "no sources declared, so freshness cannot be established".into(),
        ));
    }
    if inputs.sources.is_empty() {
        // Declared but matched nothing: this is the vacuous-truth trap. Refuse
        // rather than report fresh.
        return Ok(Decision::Stale("declared sources matched no files".into()));
    }

    for (path, required) in inputs.outputs {
        let name = super::normalize_relative(&path.to_string_lossy());
        if path.exists() {
            continue;
        }
        if *required {
            return Ok(Decision::Stale(format!("missing output {name}")));
        }
        // Optional outputs are only enforced once observed.
        if state
            .seen_outputs
            .iter()
            .any(|seen| seen == &name || Path::new(seen) == path)
        {
            return Ok(Decision::Stale(format!(
                "previously present output {name} disappeared"
            )));
        }
    }

    let current = input_hash(inputs.sources, inputs.command)?;
    if &current == recorded {
        Ok(Decision::Fresh)
    } else {
        Ok(Decision::Stale(
            "sources or the effective command changed".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    /// `record` is the only thing that makes a provider fresh, so the ordering in
    /// the caller is load-bearing: it must run *after* a successful command.
    ///
    /// This test pins the property that makes that ordering observable -- a
    /// state file whose hash was never updated still reports stale. Without it,
    /// moving `record` ahead of the run would look harmless: the command still
    /// fails loudly once, and only the *next* run silently claims a tree that was
    /// never populated is up to date.
    #[test]
    fn a_hash_recorded_for_different_inputs_does_not_make_a_provider_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("package.json");
        std::fs::write(&manifest, b"{}").unwrap();
        let sources = vec![manifest.clone()];

        let mut state = DepsState::default();
        state
            .record(
                "npm",
                &Inputs {
                    sources: &sources,
                    declared_sources: true,
                    command: "npm install",
                    outputs: &[],
                },
            )
            .unwrap();

        // Same sources, different command: the run that would have produced this
        // state never happened, so it cannot be fresh.
        let decision = decide(
            &Inputs {
                sources: &sources,
                declared_sources: true,
                command: "npm ci",
                outputs: &[],
            },
            state.providers.get("npm"),
        )
        .unwrap();
        assert!(!decision.is_fresh(), "{decision:?}");

        // And the recorded command *is* fresh, which is what proves the check
        // above is discriminating rather than always-stale.
        let decision = decide(
            &Inputs {
                sources: &sources,
                declared_sources: true,
                command: "npm install",
                outputs: &[],
            },
            state.providers.get("npm"),
        )
        .unwrap();
        assert!(decision.is_fresh(), "{decision:?}");
    }

    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn state_round_trips_and_is_keyed_per_project() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let one = state_path(&cache, Path::new("/projects/a"));
        let two = state_path(&cache, Path::new("/projects/b"));
        assert_ne!(one, two, "two projects must not share a state file");
        assert!(
            one.starts_with(cache.join("deps")),
            "state belongs in the managed cache, never in the project"
        );

        let mut state = DepsState::default();
        state.providers.insert(
            "pnpm".into(),
            ProviderState {
                hash: Some("abc".into()),
                seen_outputs: vec!["node_modules".into()],
            },
        );
        state.save(&one).unwrap();
        assert_eq!(DepsState::load(&one).unwrap(), state);

        // A missing file is an empty state, not an error.
        assert_eq!(
            DepsState::load(&cache.join("deps").join("missing.toml")).unwrap(),
            DepsState::default()
        );
    }

    #[test]
    fn a_changed_source_or_command_makes_it_stale() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("package.json");
        write(&manifest, r#"{"name":"p"}"#);
        let modules = temp.path().join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();

        let sources = vec![manifest.clone()];
        let outputs = vec![(modules.clone(), true)];
        let hash = input_hash(&sources, "pnpm install --frozen-lockfile").unwrap();
        let state = ProviderState {
            hash: Some(hash),
            seen_outputs: vec!["node_modules".into()],
        };
        let inputs = Inputs {
            sources: &sources,
            declared_sources: true,
            command: "pnpm install --frozen-lockfile",
            outputs: &outputs,
        };
        assert!(decide(&inputs, Some(&state)).unwrap().is_fresh());

        // Same sources, different command: must re-run. Switching registry or
        // installer lands here.
        let changed = Inputs {
            command: "pnpm install --frozen-lockfile --registry=https://evil",
            ..Inputs {
                sources: &sources,
                declared_sources: true,
                command: "",
                outputs: &outputs,
            }
        };
        assert!(!decide(&changed, Some(&state)).unwrap().is_fresh());

        // Changed source content.
        write(&manifest, r#"{"name":"p","dependencies":{"x":"1"}}"#);
        assert!(!decide(&inputs, Some(&state)).unwrap().is_fresh());
    }

    #[test]
    fn a_missing_required_output_is_stale_even_when_the_hash_matches() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("package.json");
        write(&manifest, r#"{"name":"p"}"#);
        let sources = vec![manifest];
        let command = "npm ci --ignore-scripts";
        let state = ProviderState {
            hash: Some(input_hash(&sources, command).unwrap()),
            seen_outputs: vec!["node_modules".into()],
        };
        let outputs = vec![(temp.path().join("node_modules"), true)];
        let inputs = Inputs {
            sources: &sources,
            declared_sources: true,
            command,
            outputs: &outputs,
        };
        let decision = decide(&inputs, Some(&state)).unwrap();
        assert_eq!(
            decision.reason(),
            Some(
                format!(
                    "missing output {}",
                    super::super::normalize_relative(
                        &temp.path().join("node_modules").to_string_lossy()
                    )
                )
                .as_str()
            )
        );
    }

    /// An empty or undeclared source set must never be reported fresh. A
    /// pattern matching zero files is the trap `tasks::freshness` documents:
    /// "nothing changed" and "nothing was checked" look identical from the
    /// outside.
    #[test]
    fn empty_or_undeclared_sources_are_never_fresh() {
        let state = ProviderState {
            hash: Some("whatever".into()),
            seen_outputs: Vec::new(),
        };
        let outputs: Vec<(PathBuf, bool)> = Vec::new();

        let undeclared = Inputs {
            sources: &[],
            declared_sources: false,
            command: "npm ci",
            outputs: &outputs,
        };
        assert!(!decide(&undeclared, Some(&state)).unwrap().is_fresh());

        let matched_nothing = Inputs {
            sources: &[],
            declared_sources: true,
            command: "npm ci",
            outputs: &outputs,
        };
        let decision = decide(&matched_nothing, Some(&state)).unwrap();
        assert_eq!(decision.reason(), Some("declared sources matched no files"));
    }

    #[test]
    fn an_optional_output_is_only_enforced_once_it_has_been_seen() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("go.mod");
        write(&manifest, "module x\n");
        let sources = vec![manifest];
        let command = "go mod download";
        let hash = input_hash(&sources, command).unwrap();
        let vendor = temp.path().join("vendor");
        let outputs = vec![(vendor.clone(), false)];
        let name = super::super::normalize_relative(&vendor.to_string_lossy());

        // Never seen: absence is fine (the tool installs elsewhere).
        let never_seen = ProviderState {
            hash: Some(hash.clone()),
            seen_outputs: Vec::new(),
        };
        let inputs = Inputs {
            sources: &sources,
            declared_sources: true,
            command,
            outputs: &outputs,
        };
        assert!(decide(&inputs, Some(&never_seen)).unwrap().is_fresh());

        // Seen once, now gone: that is a real regression.
        let seen = ProviderState {
            hash: Some(hash),
            seen_outputs: vec![name],
        };
        assert!(!decide(&inputs, Some(&seen)).unwrap().is_fresh());
    }
}
