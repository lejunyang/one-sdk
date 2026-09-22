//! Application dependency manifests (`osdk deps`).
//!
//! The unit here is **one project's whole dependency closure**, installed
//! *inside the project* (`node_modules`, `.venv`, `vendor`), whose source of
//! truth is the ecosystem's own lockfile. That is a different thing from a
//! *tool* (one executable, installed into osdk's isolated install directory and
//! pinned by `osdk.lock`), and the two are deliberately kept apart: `[tools]`
//! installs the package manager, `[deps]` installs the project's packages.
//!
//! osdk's job is the part it is actually better placed to own -- having the
//! package manager ready, choosing the index, running it with credential
//! hygiene, reading the produced artifacts back, and recording the identity so
//! another machine reproduces the same thing. Resolution and installation stay
//! with the native tool; osdk does not re-implement npm, and does not keep a
//! second copy of the dependency graph.
//!
//! # Why this module is behind `install`
//!
//! `osdk-shim` dispatches an already-installed tool on *every command
//! invocation*. It never materializes application dependencies, so its build
//! must not carry this parsing -- same reason `tasks` and `[models]` are gated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub mod node;
pub mod state;

pub use state::{DepsState, ProviderState};

/// Ecosystem a provider belongs to.
///
/// Two providers in the same ecosystem compete for the same project (only one
/// installer may own `node_modules`), which is what makes the mutual-exclusion
/// check in [`select_installer`] meaningful. Providers in *different*
/// ecosystems coexist freely: a repository may legitimately have both a
/// `package.json` and a `pyproject.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ecosystem {
    Node,
    Python,
    Go,
    Rust,
    Custom,
}

impl Ecosystem {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::Custom => "custom",
        }
    }
}

/// Whether a tracked output must exist, or only after it has been seen once.
///
/// `OptionalOnceSeen` exists because some package managers install *outside*
/// the project by default (cargo into `CARGO_HOME`, pip without a virtualenv),
/// so demanding the path up front would report a correct install as stale
/// forever. Requiring it only after osdk has observed it keeps the check honest
/// without inventing a layout the tool does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSpec {
    Required(&'static str),
    OptionalOnceSeen(&'static str),
}

impl OutputSpec {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Required(path) | Self::OptionalOnceSeen(path) => path,
        }
    }

    pub const fn required(self) -> bool {
        matches!(self, Self::Required(_))
    }
}

/// A tool this provider needs before it can run, and where its version comes
/// from.
///
/// Installing it is **not** this module's job: osdk already has backends for
/// node/pnpm/yarn/bun/python/go/rust (`backend::registry`), so the deps layer
/// only decides *which* tool at *which* version and hands that to the existing
/// install path. Building a second installer here would duplicate the whole
/// verification pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredTool {
    /// Backend id, e.g. `node` or `pnpm`.
    pub id: &'static str,
    /// Whether this tool provides the program that actually runs.
    pub role: ToolRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRole {
    /// The package manager itself (`pnpm`, `bun`).
    Installer,
    /// A runtime the installer needs on PATH (`node` for npm/pnpm/yarn).
    Runtime,
}

/// Which keys of a `[deps.<provider>]` table move it out of the "needs no
/// trust" default, and into which reason.
///
/// The default is deliberately *not* "installing packages executes code".
/// `trust.rs` states the project's own philosophy: declaring which package to
/// install is not a trust gate, because installs pass `--ignore-scripts`,
/// `http:` artifacts pin a sha256 and `go:` builds run with `CGO_ENABLED=0`.
/// Only two things change that: choosing a different byte source, and asking
/// for code to be built from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustProfile {
    /// Keys that redirect where bytes come from (`WeakensVerification`).
    pub source_keys: &'static [&'static str],
    /// Keys that opt into running build/lifecycle scripts (`ExecutesCode`).
    pub build_keys: &'static [&'static str],
}

/// Keys shared by every provider. `index`/`extra_index`/`registry` redirect the
/// byte source; `allow_build_from_source` turns lifecycle and build scripts
/// back on.
pub const COMMON_SOURCE_KEYS: &[&str] = &["index", "extra_index", "registry", "insecure"];
pub const COMMON_BUILD_KEYS: &[&str] = &["allow_build_from_source"];

pub const DEFAULT_TRUST_PROFILE: TrustProfile = TrustProfile {
    source_keys: COMMON_SOURCE_KEYS,
    build_keys: COMMON_BUILD_KEYS,
};

/// Static description of one provider. Adding an ecosystem is adding a table
/// entry, not a new `match` arm -- the same shape `DYNAMIC_NAMESPACES` and the
/// backend registry already use.
#[derive(Debug, Clone, Copy)]
pub struct DepsProviderSchema {
    /// Stable id: the `[deps.<id>]` key and the CLI name.
    pub id: &'static str,
    pub ecosystem: Ecosystem,
    /// Manifest files that make this provider applicable. The first one decides
    /// the project root.
    pub manifests: &'static [&'static str],
    /// Native lockfiles this provider owns, best first.
    pub native_locks: &'static [&'static str],
    pub default_sources: &'static [&'static str],
    pub default_outputs: &'static [OutputSpec],
    pub required_tools: &'static [RequiredTool],
    pub trust: TrustProfile,
}

impl DepsProviderSchema {
    pub fn primary_manifest(&self) -> &'static str {
        self.manifests[0]
    }
}

/// All compiled-in providers. D1 ships the Node ecosystem; further ecosystems
/// are added here.
pub static PROVIDERS: &[&DepsProviderSchema] = &[&node::NPM, &node::PNPM, &node::YARN, &node::BUN];

pub fn provider_schema(id: &str) -> Option<&'static DepsProviderSchema> {
    PROVIDERS.iter().copied().find(|schema| schema.id == id)
}

/// One provider matched against a real directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedProject {
    pub provider: &'static str,
    pub ecosystem: Ecosystem,
    /// Directory holding the primary manifest.
    pub root: PathBuf,
    pub manifest: PathBuf,
    /// Native lock found next to the manifest, if any.
    pub native_lock: Option<PathBuf>,
    /// `packageManager`-style declaration read out of the manifest.
    pub declared_manager: Option<DeclaredManager>,
}

/// A package manager the project itself names, e.g. `"packageManager":
/// "pnpm@10.4.1"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredManager {
    pub name: String,
    pub version: Option<String>,
}

impl DeclaredManager {
    /// Parse a `name@version` spec. The version is optional so a bare `"pnpm"`
    /// still names an owner.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        // A scoped name has no leading `@` here (package managers are bare
        // names), so the first `@` separates the version.
        let (name, version) = match raw.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (raw, None),
        };
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        Some(Self {
            name: name.to_ascii_lowercase(),
            // Strip a `+sha224.` integrity suffix corepack allows.
            version: version.map(|version| {
                version
                    .split('+')
                    .next()
                    .unwrap_or(version)
                    .trim()
                    .to_string()
            }),
        })
    }
}

/// What to execute, with **both** arguments and environment.
///
/// Carrying env is not a convenience: yarn berry has no `--ignore-scripts`
/// option at all (verified 2026-09-22 against yarn 4.6.0, which answers
/// `Unsupported option name`), and disabling build scripts there is
/// `YARN_ENABLE_SCRIPTS=false`. A plan that could only express a command line
/// would silently run scripts on that one package manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// Backend id whose bin directory provides the program.
    pub tool: &'static str,
    /// Program file name to look for inside that bin directory, best first.
    pub program_candidates: Vec<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Whether this plan is the frozen (lockfile-respecting) form.
    pub frozen: bool,
    /// Set when osdk deliberately fell back to a non-frozen command because no
    /// native lock existed. Callers must report it rather than proceed quietly.
    pub downgraded_reason: Option<String>,
}

/// Per-provider settings resolved from `[deps.<id>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    pub auto: bool,
    pub sources: Vec<String>,
    pub outputs: Vec<String>,
    pub run: Option<String>,
    pub env: BTreeMap<String, String>,
    pub dir: Option<String>,
    pub depends: Vec<String>,
    pub installer: Option<String>,
    pub index: Option<String>,
    pub allow_build_from_source: bool,
}

/// How the installer was chosen, kept so diagnostics can explain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerOrigin {
    /// The project's own manifest named it.
    Manifest,
    /// An existing native lockfile owns the tree.
    NativeLock,
    /// `[deps.<id>].installer`.
    ProjectConfig,
    /// Settings fallback / the provider is itself the installer.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerChoice {
    pub provider: &'static str,
    pub origin: InstallerOrigin,
    /// Version the project asked for, when it named one.
    pub version: Option<String>,
}

/// Discover which providers apply, walking up from `start` to `ceiling`.
///
/// Fail-closed at the boundary: a manifest that exists but cannot be read or
/// parsed is an error, **not** a reason to keep walking up. Skipping it would
/// silently attach the project to a grandparent directory -- the same decision
/// `npm_tools::find_nearest_package_json` already makes, and for the same
/// reason: a broken manifest is a problem to report, not to route around.
///
/// Nothing is discovered below `start`: a nested project is opted into with
/// `dir`, never found by scanning. What can be discovered automatically must be
/// what was declared.
pub fn discover(
    start: &Path,
    ceiling: Option<&Path>,
    enabled: &[&'static DepsProviderSchema],
) -> Result<Vec<DetectedProject>> {
    let mut found: Vec<DetectedProject> = Vec::new();
    let mut pending: Vec<&'static DepsProviderSchema> = enabled.to_vec();

    for directory in start.ancestors() {
        if pending.is_empty() {
            break;
        }
        let mut still_pending = Vec::new();
        for schema in pending {
            match detect_in(directory, schema)? {
                Some(project) => found.push(project),
                // Nearest wins, so a provider keeps looking up only while it
                // has not matched.
                None => still_pending.push(schema),
            }
        }
        pending = still_pending;
        if ceiling.is_some_and(|ceiling| directory == ceiling) {
            break;
        }
    }
    found.sort_by(|left, right| left.provider.cmp(right.provider));
    Ok(found)
}

fn detect_in(
    directory: &Path,
    schema: &'static DepsProviderSchema,
) -> Result<Option<DetectedProject>> {
    let manifest = directory.join(schema.primary_manifest());
    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            // A directory or symlink where a manifest belongs cannot be shown
            // harmless, and treating it as absent would attach the project
            // somewhere else entirely.
            return Err(Error::config(format!(
                "{} is not a regular file; cannot read it as a {} manifest",
                manifest.display(),
                schema.id
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(manifest, error)),
    }

    let declared_manager = node::declared_manager(schema, &manifest)?;
    let native_lock = schema
        .native_locks
        .iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file());

    Ok(Some(DetectedProject {
        provider: schema.id,
        ecosystem: schema.ecosystem,
        root: directory.to_path_buf(),
        manifest,
        native_lock,
        declared_manager,
    }))
}

/// Choose the installer for one detected project.
///
/// Priority, highest first:
/// 1. the project manifest's own declaration (`packageManager`),
/// 2. the installer that owns an existing native lockfile,
/// 3. `[deps.<id>].installer`,
/// 4. the provider itself.
///
/// Note 3 sits *below* 2: both the manifest and an incumbent lockfile are the
/// project's own statements, and osdk-side configuration must not take a tree
/// away from the installer already managing it. A declaration that disagrees
/// with the lockfile is rejected rather than guessed at.
pub fn select_installer(
    project: &DetectedProject,
    config: &ProviderConfig,
    peers: &[DetectedProject],
) -> Result<InstallerChoice> {
    // Two installers of the same ecosystem both claiming the tree is a conflict
    // the user must resolve; picking one would silently pick a lockfile.
    let rivals: Vec<&DetectedProject> = peers
        .iter()
        .filter(|peer| {
            peer.ecosystem == project.ecosystem
                && peer.provider != project.provider
                && peer.root == project.root
                && peer.native_lock.is_some()
        })
        .collect();
    if project.native_lock.is_some() && !rivals.is_empty() {
        let mut names: Vec<&str> = rivals.iter().map(|peer| peer.provider).collect();
        names.push(project.provider);
        names.sort_unstable();
        return Err(Error::config(format!(
            "{} has lockfiles for more than one installer ({}); \
             set `[deps.<provider>].installer` or remove the stale lockfile",
            project.root.display(),
            names.join(", ")
        )));
    }

    if let Some(declared) = &project.declared_manager {
        if let Some(lock) = &project.native_lock {
            if let Some(owner) = lock_owner(project.ecosystem, lock) {
                if owner != declared.name {
                    return Err(Error::config(format!(
                        "{} declares `{}` but {} is owned by `{}`; \
                         update the declaration or the lockfile",
                        project.manifest.display(),
                        declared.name,
                        lock.display(),
                        owner
                    )));
                }
            }
        }
        if declared.name != project.provider {
            return Err(Error::config(format!(
                "{} declares package manager `{}`, so provider `{}` does not own it; \
                 enable `[deps.{}]` instead",
                project.manifest.display(),
                declared.name,
                project.provider,
                declared.name
            )));
        }
        return Ok(InstallerChoice {
            provider: project.provider,
            origin: InstallerOrigin::Manifest,
            version: declared.version.clone(),
        });
    }

    if project.native_lock.is_some() {
        return Ok(InstallerChoice {
            provider: project.provider,
            origin: InstallerOrigin::NativeLock,
            version: None,
        });
    }

    if let Some(installer) = &config.installer {
        if installer != project.provider {
            return Err(Error::config(format!(
                "`[deps.{}].installer = \"{}\"` does not match the provider; \
                 enable `[deps.{}]` instead",
                project.provider, installer, installer
            )));
        }
        return Ok(InstallerChoice {
            provider: project.provider,
            origin: InstallerOrigin::ProjectConfig,
            version: None,
        });
    }

    Ok(InstallerChoice {
        provider: project.provider,
        origin: InstallerOrigin::Default,
        version: None,
    })
}

/// Which installer owns a native lockfile, by file name.
fn lock_owner(ecosystem: Ecosystem, lock: &Path) -> Option<String> {
    let name = lock.file_name()?.to_str()?;
    match ecosystem {
        Ecosystem::Node => node::lock_owner(name).map(str::to_string),
        _ => None,
    }
}

/// Build the command for one detected project.
pub fn plan(
    project: &DetectedProject,
    choice: &InstallerChoice,
    config: &ProviderConfig,
    tool_versions: &BTreeMap<String, String>,
) -> Result<RunPlan> {
    match project.ecosystem {
        Ecosystem::Node => node::plan(project, choice, config, tool_versions),
        other => Err(Error::other(format!(
            "no deps provider implementation for ecosystem `{}` yet",
            other.as_str()
        ))),
    }
}

/// Effective freshness sources for a provider: explicit config replaces the
/// built-in list rather than adding to it, so a project that narrows `sources`
/// gets exactly what it asked for.
pub fn effective_sources(schema: &DepsProviderSchema, config: &ProviderConfig) -> Vec<String> {
    if config.sources.is_empty() {
        schema
            .default_sources
            .iter()
            .map(|source| (*source).to_string())
            .collect()
    } else {
        config.sources.clone()
    }
}

/// Effective outputs, same replace-not-append rule. An explicit empty list in
/// config disables output tracking on purpose.
pub fn effective_outputs(
    schema: &DepsProviderSchema,
    config: &ProviderConfig,
    explicit_outputs_set: bool,
) -> Vec<OutputSpecOwned> {
    if explicit_outputs_set {
        return config
            .outputs
            .iter()
            .map(|path| OutputSpecOwned {
                path: path.clone(),
                required: true,
            })
            .collect();
    }
    schema
        .default_outputs
        .iter()
        .map(|spec| OutputSpecOwned {
            path: spec.path().to_string(),
            required: spec.required(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpecOwned {
    pub path: String,
    pub required: bool,
}

/// Stable label for a native lockfile, derived from its file name.
///
/// Recognition is driven off the provider table rather than a second hand-written
/// mapping: a separate list is one more place to forget when a provider is added,
/// and the symptom -- a lock entry labelled with the wrong installer -- reads as
/// plausible rather than as a bug.
pub fn native_lock_kind(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let known = PROVIDERS
        .iter()
        .any(|provider| provider.native_locks.contains(&name));
    if !known {
        return None;
    }
    // Exhaustive over the table checked above, so a provider added without a
    // label here yields None rather than a borrowed name that cannot be
    // 'static\. None means "no native lock recorded", which is visibly
    // incomplete; a wrong label would not be.
    match name {
        "package-lock.json" => Some("package-lock"),
        "npm-shrinkwrap.json" => Some("npm-shrinkwrap"),
        "pnpm-lock.yaml" => Some("pnpm-lock"),
        "yarn.lock" => Some("yarn-lock"),
        "bun.lock" => Some("bun-lock"),
        "bun.lockb" => Some("bun-lockb"),
        _ => None,
    }
}

/// SHA-256 of a file, lowercase hex.
///
/// Lives here rather than in the CLI so the lock writer needs no hashing
/// dependency of its own, and so it shares the hashing crates that
/// \[profile.release]\ pins to \opt-level = 3\.
pub fn file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Normalize a project-relative path for anything that gets written into a
/// produced artifact (lock, receipt, state).
///
/// Separators are decided by *who wrote the string*, not by the host reading
/// it: these values are committed and replayed on other platforms, so they are
/// normalized to `/` on the way out. Readers accept both (see
/// [`relative_segments`]).
pub fn normalize_relative(path: &str) -> String {
    path.replace('\\', "/")
}

/// Split a recorded relative path into segments, accepting **either**
/// separator, because the machine that wrote it may not be the one reading it.
pub fn relative_segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn all() -> Vec<&'static DepsProviderSchema> {
        PROVIDERS.to_vec()
    }

    #[test]
    fn discovery_takes_the_nearest_manifest_and_does_not_look_below() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path();
        let inner = outer.join("packages").join("app");
        write(&outer.join("package.json"), r#"{"name":"outer"}"#);
        write(&inner.join("package.json"), r#"{"name":"inner"}"#);
        let deeper = inner.join("src");
        std::fs::create_dir_all(&deeper).unwrap();

        let found = discover(&deeper, Some(outer), &all()).unwrap();
        assert_eq!(found.len(), 4, "all four Node providers share package.json");
        for project in &found {
            assert_eq!(project.root, inner, "nearest manifest must win");
        }

        // Starting at the outer directory must not descend into `packages/app`.
        let found = discover(outer, Some(outer), &all()).unwrap();
        for project in &found {
            assert_eq!(project.root, outer);
        }
    }

    /// A manifest that exists but cannot be read is an error, not a signal to
    /// keep walking up. Skipping it would attach the project to a grandparent
    /// and produce a plausible-looking install of the wrong tree.
    #[test]
    fn a_manifest_that_is_not_a_regular_file_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path();
        write(&outer.join("package.json"), r#"{"name":"outer"}"#);
        let inner = outer.join("child");
        // A *directory* named package.json: present, unreadable as a manifest.
        std::fs::create_dir_all(inner.join("package.json")).unwrap();

        let error = discover(&inner, Some(outer), &all()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("not a regular file"),
            "expected a fail-closed error, got: {message}"
        );
    }

    #[test]
    fn unparsable_manifest_is_reported_rather_than_ignored() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join("package.json"), "{ this is not json");
        let error = discover(temp.path(), Some(temp.path()), &all()).unwrap_err();
        assert!(error.to_string().contains("package.json"), "{error}");
    }

    #[test]
    fn declared_manager_parses_name_and_optional_version() {
        let parsed = DeclaredManager::parse("pnpm@10.4.1").unwrap();
        assert_eq!(parsed.name, "pnpm");
        assert_eq!(parsed.version.as_deref(), Some("10.4.1"));

        // corepack allows an integrity suffix; the version must survive it.
        let parsed = DeclaredManager::parse("yarn@4.6.0+sha224.abcdef").unwrap();
        assert_eq!(parsed.name, "yarn");
        assert_eq!(parsed.version.as_deref(), Some("4.6.0"));

        let parsed = DeclaredManager::parse("  NPM  ").unwrap();
        assert_eq!(parsed.name, "npm");
        assert!(parsed.version.is_none());

        assert!(DeclaredManager::parse("").is_none());
        assert!(DeclaredManager::parse("@1.2.3").is_none());
    }

    #[test]
    fn installer_priority_manifest_beats_lock_beats_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        // 1. manifest declaration wins.
        write(
            &root.join("package.json"),
            r#"{"name":"p","packageManager":"pnpm@10.4.1"}"#,
        );
        write(&root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n");
        let found = discover(root, Some(root), &[&node::PNPM]).unwrap();
        let choice = select_installer(&found[0], &ProviderConfig::default(), &found).unwrap();
        assert_eq!(choice.origin, InstallerOrigin::Manifest);
        assert_eq!(choice.version.as_deref(), Some("10.4.1"));

        // 2. no declaration, incumbent lock decides.
        write(&root.join("package.json"), r#"{"name":"p"}"#);
        let found = discover(root, Some(root), &[&node::PNPM]).unwrap();
        let choice = select_installer(&found[0], &ProviderConfig::default(), &found).unwrap();
        assert_eq!(choice.origin, InstallerOrigin::NativeLock);

        // 3. neither: project config, then the provider itself.
        std::fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();
        let found = discover(root, Some(root), &[&node::PNPM]).unwrap();
        let config = ProviderConfig {
            installer: Some("pnpm".into()),
            ..Default::default()
        };
        let choice = select_installer(&found[0], &config, &found).unwrap();
        assert_eq!(choice.origin, InstallerOrigin::ProjectConfig);
        let choice = select_installer(&found[0], &ProviderConfig::default(), &found).unwrap();
        assert_eq!(choice.origin, InstallerOrigin::Default);
    }

    #[test]
    fn a_declaration_that_contradicts_the_lockfile_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("package.json"),
            r#"{"name":"p","packageManager":"pnpm@10.4.1"}"#,
        );
        write(&root.join("package-lock.json"), r#"{"lockfileVersion":3}"#);
        let found = discover(root, Some(root), &[&node::NPM]).unwrap();
        let error = select_installer(&found[0], &ProviderConfig::default(), &found).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("declares"), "{message}");
        assert!(message.contains("package-lock.json"), "{message}");
    }

    #[test]
    fn two_lockfiles_in_one_ecosystem_are_a_conflict_not_a_coin_flip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("package.json"), r#"{"name":"p"}"#);
        write(&root.join("package-lock.json"), r#"{"lockfileVersion":3}"#);
        write(&root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n");
        let found = discover(root, Some(root), &[&node::NPM, &node::PNPM]).unwrap();
        let error = select_installer(&found[0], &ProviderConfig::default(), &found).unwrap_err();
        assert!(
            error.to_string().contains("more than one installer"),
            "{error}"
        );
    }

    /// Paths written into artifacts are normalized to `/`; paths read back are
    /// accepted with either separator, because the writer may have been another
    /// platform. Both directions run on every platform on purpose: gating the
    /// backslash case behind `cfg(windows)` is exactly how this class of bug
    /// survives.
    #[test]
    fn recorded_relative_paths_normalize_out_and_accept_both_separators_in() {
        assert_eq!(normalize_relative("packages\\app"), "packages/app");
        assert_eq!(normalize_relative("packages/app"), "packages/app");
        assert_eq!(
            relative_segments("packages\\app\\package.json"),
            vec!["packages", "app", "package.json"]
        );
        assert_eq!(
            relative_segments("packages/app/package.json"),
            vec!["packages", "app", "package.json"]
        );
        assert_eq!(relative_segments("./a//b"), vec!["a", "b"]);
    }

    #[test]
    fn explicit_sources_replace_the_builtin_list() {
        let schema = &node::PNPM;
        let config = ProviderConfig::default();
        assert_eq!(
            effective_sources(schema, &config),
            vec!["package.json".to_string(), "pnpm-lock.yaml".to_string()]
        );
        let config = ProviderConfig {
            sources: vec!["only-this.json".into()],
            ..Default::default()
        };
        assert_eq!(
            effective_sources(schema, &config),
            vec!["only-this.json".to_string()]
        );
    }
}
