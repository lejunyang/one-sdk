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

pub mod native;
pub mod node;
pub mod python;
pub mod state;
pub mod verify;

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
    Deno,
    Custom,
}

impl Ecosystem {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::Deno => "deno",
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
pub static PROVIDERS: &[&DepsProviderSchema] = &[
    &node::NPM,
    &node::PNPM,
    &node::YARN,
    &node::BUN,
    &python::UV,
    &python::PIP_REQUIREMENTS,
    &native::GO,
    &native::CARGO,
    &native::DENO,
];

pub fn provider_schema(id: &str) -> Option<&'static DepsProviderSchema> {
    PROVIDERS.iter().copied().find(|schema| schema.id == id)
}

/// Build a `DetectedProject` for a provider that exists only in the project's
/// config.
///
/// A custom provider has no manifest to find and no lockfile to own: it *is* a
/// declaration. So discovery does not apply -- the project root is where the
/// config that declared it lives, and that is the whole detection step.
///
/// Note what this deliberately does not do: it does not go looking for files to
/// decide whether the provider applies. `sources` still drives freshness, but an
/// empty `sources` means "cannot establish freshness" (so it always runs), not
/// "not applicable". Guessing applicability from the filesystem is how a custom
/// step would silently stop running after a refactor.
pub fn detect_custom(id: &str, config_root: &Path, config_path: &Path) -> DetectedProject {
    DetectedProject {
        provider: std::borrow::Cow::Owned(id.to_string()),
        ecosystem: Ecosystem::Custom,
        root: config_root.to_path_buf(),
        manifest: config_path.to_path_buf(),
        native_lock: None,
        declared_manager: None,
    }
}

/// Plan a custom provider's command.
///
/// The command is split on whitespace rather than run through a shell. Handing it
/// to `cmd.exe` or `sh` would make the same `run` string mean different things on
/// different machines, and would turn quoting into a portability hazard for
/// something that gets committed. Anything needing shell features belongs in a
/// script the `run` line invokes.
pub fn plan_custom(project: &DetectedProject, config: &ProviderConfig) -> Result<RunPlan> {
    // One check, deliberately. An earlier version also tested `is_empty()` before
    // this, and the redundancy made the guard untestable: removing either one left
    // the other still rejecting, so injecting a fault produced no observable
    // change. A guard whose removal is invisible cannot be trusted to be there.
    let command = config.run.as_deref().unwrap_or_default();
    let mut parts = command.split_whitespace();
    let program = parts.next().ok_or_else(|| {
        Error::config(format!(
            "custom deps provider `{}` needs a `run` command",
            project.provider
        ))
    })?;
    let args: Vec<String> = parts.map(str::to_string).collect();

    Ok(RunPlan {
        tool: std::borrow::Cow::Owned(project.provider.to_string()),
        // Resolved from PATH, which by then has the bin directories of whatever
        // `depends` brought in. A custom step usually calls a tool another
        // provider installed.
        program_candidates: vec![program.to_string()],
        args,
        env: config.env.clone(),
        cwd: effective_cwd(&project.root, config.dir.as_deref()),
        // Nothing to freeze: there is no lockfile in this model. Reporting it as
        // frozen would make `--frozen` pass on a provider it cannot check.
        frozen: false,
        downgraded_reason: None,
        prelude: Vec::new(),
    })
}

/// One provider matched against a real directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedProject {
    /// Provider id. `Borrowed` for the built-in table, `Owned` for a custom
    /// provider whose name only exists in the project's config. Only this field,
    /// `InstallerChoice::provider` and `RunPlan::tool` needed widening: the static
    /// schemas keep `&'static str`, so the built-in path still allocates nothing.
    pub provider: std::borrow::Cow<'static, str>,
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
    pub tool: std::borrow::Cow<'static, str>,
    /// Program file name to look for inside that bin directory, best first.
    pub program_candidates: Vec<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Whether this plan is the frozen (lockfile-respecting) form.
    pub frozen: bool,
    /// Commands to run, in order, before the main one -- same program, same
    /// cwd, same env.
    ///
    /// This exists for one measured asymmetry: `uv sync` creates the project
    /// environment itself, while `uv pip sync` refuses without one ("No
    /// virtual environment found"). Expressing it as a prelude keeps the step
    /// visible in `--dry-run` and inside the freshness hash, instead of hiding
    /// an implicit `uv venv` in the runner where neither would show it.
    pub prelude: Vec<Vec<String>>,
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
    pub provider: std::borrow::Cow<'static, str>,
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
    found.sort_by(|left, right| left.provider.cmp(&right.provider));
    Ok(found)
}

/// A sub-project found by expanding a declared root pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedProject {
    /// The `[deps].roots` entry this came from, `/`-normalized. Reported so
    /// `--list` can say which declaration produced a project rather than leaving
    /// the user to guess.
    pub root_pattern: String,
    /// Path of the sub-project relative to the config root, `/`-normalized.
    pub relative: String,
    pub project: DetectedProject,
}

impl RootedProject {
    /// Addressable id: `//apps/api:uv`.
    ///
    /// The `//` prefix is what keeps a rooted id from colliding with a plain
    /// provider name, so `osdk deps //apps/api:uv` and `osdk deps uv` cannot be
    /// confused for one another.
    pub fn id(&self) -> String {
        format!("//{}:{}", self.relative, self.project.provider)
    }
}

/// Parse a rooted provider id back into its parts.
///
/// Returns `None` for a plain provider name, which is how the CLI tells the two
/// forms apart without a second flag.
pub fn parse_rooted_id(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("//")?;
    let (relative, provider) = rest.rsplit_once(':')?;
    if relative.is_empty() || provider.is_empty() {
        return None;
    }
    Some((relative, provider))
}

/// Discover sub-projects inside the directories named by `[deps].roots`.
///
/// The invariant this function exists to hold: **only directories matching a
/// declared pattern are looked at.** There is no walk of arbitrary subtrees, and
/// a pattern is matched segment by segment rather than by scanning and filtering
/// afterwards -- the difference matters, because a scan-then-filter version would
/// still have to read every directory to decide, and one forgotten filter would
/// silently turn it into a full crawl.
///
/// The reasoning is the same one AGENTS.md records for the model inventory scan:
/// the set of things that can be found automatically must be the set that was
/// declared. A monorepo where `osdk deps` quietly picked up a package nobody
/// listed would install dependencies for a project the user did not ask about.
pub fn discover_in_roots(
    config_root: &Path,
    roots: &[String],
    enabled: &[&'static DepsProviderSchema],
) -> Result<Vec<RootedProject>> {
    let mut found = Vec::new();
    for expanded in expand_roots(config_root, roots, "[deps].roots")? {
        for schema in enabled {
            // `detect_in` is reused unchanged, so a root sub-project is
            // fail-closed on a broken manifest exactly like a top-level one.
            if let Some(project) = detect_in(&expanded.directory, schema)? {
                found.push(RootedProject {
                    root_pattern: expanded.pattern.clone(),
                    relative: expanded.relative.clone(),
                    project,
                });
            }
        }
    }
    found.sort_by_key(RootedProject::id);
    Ok(found)
}

/// One directory a `roots` pattern named.
#[derive(Debug, Clone)]
pub struct ExpandedRoot {
    /// The directory itself, with this machine's separators: it is a location on
    /// this machine, not something written into a portable artifact.
    pub directory: PathBuf,
    /// Its path relative to the config root, `/`-normalized because it goes into
    /// ids that are printed, compared, and written down.
    pub relative: String,
    /// The pattern that produced it, reported so a listing can say where a
    /// sub-project came from rather than printing the same name repeatedly.
    pub pattern: String,
}

/// Expand `roots` patterns into the directories they name.
///
/// Shared by `[deps].roots` and `[tasks].roots`, which differ only in what they
/// then look for inside each directory -- a dependency manifest for one, an
/// `osdk.toml` for the other. Keeping the expansion in one place is the point: two
/// copies of this logic would drift, and the thing that would drift is precisely
/// the guarantee that nothing outside the declared set is ever reached.
///
/// Patterns descend **segment by segment**: a literal segment is joined, and only a
/// wildcard segment causes `read_dir` of that one level. A scan-then-filter version
/// would have to read every directory to decide, and one forgotten filter would
/// silently turn it into a full crawl.
///
/// `label` names the configuration key in error messages, so `[tasks].roots` does
/// not get told about `[deps].roots`.
pub fn expand_roots(
    config_root: &Path,
    roots: &[String],
    label: &str,
) -> Result<Vec<ExpandedRoot>> {
    let mut found = Vec::new();
    for pattern in roots {
        let normalized = normalize_relative(pattern);
        let segments: Vec<&str> = relative_segments(&normalized);
        if segments.is_empty() {
            return Err(Error::config(format!(
                "`{label}` entry `{pattern}` does not name a directory"
            )));
        }
        // A root must stay inside the project: a pattern escaping upwards would
        // let a committed config reach parts of the machine the project has no
        // business touching.
        if segments.contains(&"..") {
            return Err(Error::config(format!(
                "`{label}` entry `{pattern}` must not contain `..`"
            )));
        }

        let mut directories = vec![config_root.to_path_buf()];
        for segment in &segments {
            let mut next = Vec::new();
            for directory in &directories {
                if segment.contains('*') || segment.contains('?') {
                    // Only this one level is enumerated, and only because a
                    // wildcard was written for it.
                    //
                    // Worth knowing if you are auditing this: routing a *literal*
                    // segment through this branch too would not actually widen
                    // anything, because `glob_matches("apps", name)` still only
                    // accepts `apps`. It would just read a directory to learn what
                    // a `join` already knew. The branch that does the load-bearing
                    // work is the `glob_matches` call below -- replacing it with
                    // `true` is what turns this into a subtree crawl, and that is
                    // the variant the tests are built to catch.
                    let entries = match std::fs::read_dir(directory) {
                        Ok(entries) => entries,
                        // A pattern matching nothing is not an error: `apps/*` in
                        // a repo with no `apps` yet is a forward-looking
                        // declaration, not a mistake.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(Error::io(directory, error)),
                    };
                    let mut matched: Vec<PathBuf> = Vec::new();
                    for entry in entries {
                        let entry = entry.map_err(|error| Error::io(directory, error))?;
                        if !entry
                            .file_type()
                            .map_err(|e| Error::io(directory, e))?
                            .is_dir()
                        {
                            continue;
                        }
                        let name = entry.file_name();
                        let Some(name) = name.to_str() else { continue };
                        if glob_matches(segment, name) {
                            matched.push(entry.path());
                        }
                    }
                    // Sorted so the order does not depend on readdir, which would
                    // make listings and `depends` resolution vary by machine.
                    matched.sort();
                    next.extend(matched);
                } else {
                    let candidate = directory.join(segment);
                    if candidate.is_dir() {
                        next.push(candidate);
                    }
                }
            }
            directories = next;
        }

        for directory in directories {
            let relative = directory
                .strip_prefix(config_root)
                .unwrap_or(&directory)
                .to_string_lossy()
                .replace('\\', "/");
            found.push(ExpandedRoot {
                directory,
                relative,
                pattern: normalized.clone(),
            });
        }
    }
    Ok(found)
}

/// Match one path segment against one pattern segment.
///
/// Supports `*` (any run of characters) and `?` (one character). Deliberately not
/// a full glob library: `**` is the one thing that would turn a declared root
/// into an arbitrary crawl, which is exactly what this feature exists to prevent,
/// so it is not supported rather than supported-and-restricted.
fn glob_matches(pattern: &str, name: &str) -> bool {
    // A separator is never matchable here, by either side. Today every caller
    // passes a single directory name, so this changes nothing -- but a matcher
    // that *can* span a separator means the next caller to hand it a
    // multi-segment string silently gets the subtree crawl this whole feature
    // exists to prevent. The guarantee belongs in the function, not in every
    // caller remembering.
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    // Classic two-pointer wildcard match with backtracking on `*`.
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(position) = star {
            p = position + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
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

    // Routed by ecosystem: Python manifests carry no \packageManager\ equivalent,
    // but must still be parsed so an unreadable one is an error rather than a
    // silent "no declaration" that falls through to a different installer.
    // Exhaustive on purpose -- no `_` arm. A catch-all would route a newly added
    // ecosystem to the Node parser, which reads `packageManager` out of JSON: on a
    // `go.mod` that is an error about invalid JSON, and on a TOML manifest that
    // happens to parse it would be a declaration the file never made. Making the
    // compiler demand an arm is cheaper than finding that out from a user.
    let declared_manager = match schema.ecosystem {
        Ecosystem::Node => node::declared_manager(schema, &manifest)?,
        Ecosystem::Python => python::declared_manager(schema, &manifest)?,
        Ecosystem::Go | Ecosystem::Rust | Ecosystem::Deno => {
            native::declared_manager(schema, &manifest)?
        }
        Ecosystem::Custom => None,
    };
    let native_lock = schema
        .native_locks
        .iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file());

    Ok(Some(DetectedProject {
        provider: schema.id.into(),
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
        let mut names: Vec<&str> = rivals.iter().map(|peer| &*peer.provider).collect();
        names.push(&project.provider);
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
            provider: project.provider.clone(),
            origin: InstallerOrigin::Manifest,
            version: declared.version.clone(),
        });
    }

    if project.native_lock.is_some() {
        return Ok(InstallerChoice {
            provider: project.provider.clone(),
            origin: InstallerOrigin::NativeLock,
            version: None,
        });
    }

    if let Some(installer) = &config.installer {
        if installer.as_str() != &*project.provider {
            return Err(Error::config(format!(
                "`[deps.{}].installer = \"{}\"` does not match the provider; \
                 enable `[deps.{}]` instead",
                project.provider, installer, installer
            )));
        }
        return Ok(InstallerChoice {
            provider: project.provider.clone(),
            origin: InstallerOrigin::ProjectConfig,
            version: None,
        });
    }

    Ok(InstallerChoice {
        provider: project.provider.clone(),
        origin: InstallerOrigin::Default,
        version: None,
    })
}

/// Which installer owns a native lockfile, by file name.
fn lock_owner(ecosystem: Ecosystem, lock: &Path) -> Option<String> {
    let name = lock.file_name()?.to_str()?;
    match ecosystem {
        Ecosystem::Node => node::lock_owner(name).map(str::to_string),
        Ecosystem::Python => python::lock_owner(name).map(str::to_string),
        Ecosystem::Go | Ecosystem::Rust | Ecosystem::Deno => {
            native::lock_owner(name).map(str::to_string)
        }
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
        Ecosystem::Python => python::plan(project, choice, config, tool_versions),
        Ecosystem::Go | Ecosystem::Rust | Ecosystem::Deno => {
            native::plan(project, choice, config, tool_versions)
        }
        Ecosystem::Custom => plan_custom(project, config),
        // No catch-all: every ecosystem has an implementation, so adding one
        // should fail to compile here rather than fail at runtime with a message
        // announcing that it is not implemented.
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

/// Working directory for a provider's command: the project root, with
/// `[deps.<p>].dir` applied when set.
///
/// This exists because `dir` is a *declared* setting -- documented, in the
/// schema, and carried through config -- and a declared setting that silently
/// does nothing is worse than an absent one: the project looks configured and
/// the command runs somewhere else.
///
/// `dir` is read accepting both separators. It arrives from a committed
/// `osdk.toml`, so the machine that wrote it is not necessarily the one reading
/// it, and `Path::components()` only understands the host's own separator (see
/// [`relative_segments`]).
pub fn effective_cwd(root: &Path, dir: Option<&str>) -> PathBuf {
    match dir {
        Some(dir) => {
            let mut path = root.to_path_buf();
            for segment in relative_segments(dir) {
                path.push(segment);
            }
            path
        }
        None => root.to_path_buf(),
    }
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
    /// A partial wildcard rejects the directories it does not match.
    ///
    /// Every other fixture here uses `apps/*`, whose only wildcard segment is a
    /// bare `*` -- which correctly matches every directory. That makes "accept any
    /// name" and "match the pattern" produce identical results, so the filter
    /// itself was never under test: injecting `if true` in place of the match left
    /// the suite green.
    ///
    /// `api-*` is the shape that distinguishes them: `api-v1` must be found and
    /// `web-v1`, sitting right beside it with its own manifest, must not.
    #[test]
    fn a_partial_wildcard_rejects_what_it_does_not_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["api-v1", "api-v2", "web-v1"] {
            let directory = root.join("apps").join(name);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("package.json"), "{}").unwrap();
        }

        let enabled = [&node::NPM];
        let found = discover_in_roots(root, &["apps/api-*".to_string()], &enabled).unwrap();
        let ids: Vec<String> = found.iter().map(RootedProject::id).collect();
        assert_eq!(
            ids,
            vec![
                "//apps/api-v1:npm".to_string(),
                "//apps/api-v2:npm".to_string(),
            ],
            "`api-*` must not pick up `web-v1`"
        );
    }

    /// A literal path segment is matched literally, never enumerated.
    ///
    /// This is the invariant that separates "look inside what was declared" from
    /// "walk the repository", and it needs a fixture that can actually tell the
    /// two apart. `other/pkg` sits at the same depth as `apps/api` and has its own
    /// manifest: matching `apps` literally makes it unreachable, while enumerating
    /// that level makes it appear at once.
    ///
    /// The earlier version of this test used an undeclared directory under
    /// `vendor/`, which could not distinguish the two behaviours -- treating
    /// segments as wildcards left it green. A test that stays green when the
    /// invariant is broken is not protecting it.
    #[test]
    fn a_literal_root_segment_is_not_enumerated() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for relative in ["apps/api", "other/pkg"] {
            let directory = root.join(relative);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("package.json"), "{}").unwrap();
        }

        let enabled = [&node::NPM];
        let found = discover_in_roots(root, &["apps/*".to_string()], &enabled).unwrap();
        let ids: Vec<String> = found.iter().map(RootedProject::id).collect();
        assert_eq!(
            ids,
            vec!["//apps/api:npm".to_string()],
            "`apps` is a literal segment: `other/pkg` must be unreachable"
        );
    }

    /// Only directories matching a declared root are looked at.
    ///
    /// This is the invariant the whole feature exists for, so it is asserted from
    /// both sides: the declared sub-projects are found, and an undeclared one
    /// sitting right next to them is **not** -- even though it has a perfectly
    /// good manifest and a subtree walk would have found it immediately.
    ///
    /// The same judgement AGENTS.md records for the model inventory scan: what can
    /// be discovered automatically must be what was declared. A monorepo where
    /// `osdk deps` quietly picked up an unlisted package would install
    /// dependencies for a project nobody asked about.
    #[test]
    fn only_declared_roots_are_discovered() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for relative in ["apps/api", "apps/web", "packages/ui", "vendor/thirdparty"] {
            let directory = root.join(relative);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("package.json"), "{}").unwrap();
        }

        let enabled = [&node::NPM];
        let found = discover_in_roots(
            root,
            &["apps/*".to_string(), "packages/*".to_string()],
            &enabled,
        )
        .unwrap();
        let ids: Vec<String> = found.iter().map(RootedProject::id).collect();
        assert_eq!(
            ids,
            vec![
                "//apps/api:npm".to_string(),
                "//apps/web:npm".to_string(),
                "//packages/ui:npm".to_string(),
            ]
        );
        assert!(
            !ids.iter().any(|id| id.contains("vendor")),
            "`vendor/thirdparty` was never declared and must not be found: {ids:?}"
        );

        // Each result says which declaration produced it, so `--list` does not
        // leave the user guessing.
        assert_eq!(found[0].root_pattern, "apps/*");
        assert_eq!(found[2].root_pattern, "packages/*");

        // No roots means no sub-projects at all, not "scan everything".
        assert!(discover_in_roots(root, &[], &enabled).unwrap().is_empty());
    }

    /// A root pattern is matched one segment at a time, so a wildcard never
    /// becomes a recursive crawl.
    ///
    /// `apps/*` must not reach `apps/group/nested`. Supporting `**` would hand
    /// back exactly the arbitrary walk this feature refuses, so it is absent
    /// rather than present-and-limited.
    #[test]
    fn a_wildcard_matches_one_level_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("apps/group/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("package.json"), "{}").unwrap();
        // A sibling at the level the pattern does name, as a control: without it,
        // an empty result would not distinguish "did not recurse" from "matched
        // nothing at all".
        let shallow = root.join("apps/api");
        std::fs::create_dir_all(&shallow).unwrap();
        std::fs::write(shallow.join("package.json"), "{}").unwrap();

        let enabled = [&node::NPM];
        let found = discover_in_roots(root, &["apps/*".to_string()], &enabled).unwrap();
        let ids: Vec<String> = found.iter().map(RootedProject::id).collect();
        assert_eq!(ids, vec!["//apps/api:npm".to_string()]);

        // The nested one is reachable only by naming its level explicitly.
        let found = discover_in_roots(root, &["apps/*/*".to_string()], &enabled).unwrap();
        let ids: Vec<String> = found.iter().map(RootedProject::id).collect();
        assert_eq!(ids, vec!["//apps/group/nested:npm".to_string()]);
    }

    /// A broken manifest inside a root is an error, exactly as at the top level.
    ///
    /// Skipping it would mean a monorepo silently installs some of its packages
    /// and reports success -- the fail-closed rule does not get weaker because the
    /// project was found through a root.
    #[test]
    fn a_broken_manifest_in_a_root_is_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let good = root.join("apps/api");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("package.json"), "{}").unwrap();

        let enabled = [&node::NPM];
        assert!(discover_in_roots(root, &["apps/*".to_string()], &enabled).is_ok());

        // A directory where a manifest belongs cannot be shown harmless.
        let bad = root.join("apps/web");
        std::fs::create_dir_all(bad.join("package.json")).unwrap();
        assert!(
            discover_in_roots(root, &["apps/*".to_string()], &enabled).is_err(),
            "a root sub-project must be fail-closed like any other"
        );
    }

    /// Root patterns accept either separator, on every platform, and cannot
    /// escape the project.
    ///
    /// Not `#[cfg(windows)]`-gated: `roots` is written in a committed
    /// `osdk.toml`, so a Windows author can write `apps\api` and a Linux machine
    /// still has to find it. Gating the test would declare that half unverified,
    /// which is where this class of bug lives.
    #[test]
    fn root_patterns_accept_either_separator_and_cannot_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directory = root.join("apps/api");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("package.json"), "{}").unwrap();

        let enabled = [&node::NPM];
        let forward = discover_in_roots(root, &["apps/api".to_string()], &enabled).unwrap();
        let backward = discover_in_roots(root, &["apps\\api".to_string()], &enabled).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(forward.len(), 1);
        // The recorded pattern and relative path are `/`-normalized regardless of
        // how they were written, because both end up in reports and state files.
        assert_eq!(forward[0].root_pattern, "apps/api");
        assert_eq!(forward[0].relative, "apps/api");

        for escaping in ["../outside", "apps/../../outside"] {
            assert!(
                discover_in_roots(root, &[escaping.to_string()], &enabled).is_err(),
                "`{escaping}` escapes the project and must be refused"
            );
        }
        assert!(discover_in_roots(root, &[".".to_string()], &enabled).is_err());
    }

    /// Rooted ids round-trip, and a plain provider name is not mistaken for one.
    #[test]
    fn rooted_ids_round_trip() {
        assert_eq!(parse_rooted_id("//apps/api:uv"), Some(("apps/api", "uv")));
        assert_eq!(
            parse_rooted_id("//packages/ui:pnpm"),
            Some(("packages/ui", "pnpm"))
        );
        // A plain name has no `//`, which is how the two forms stay distinct.
        assert_eq!(parse_rooted_id("uv"), None);
        assert_eq!(parse_rooted_id("//:uv"), None);
        assert_eq!(parse_rooted_id("//apps/api:"), None);
        assert_eq!(parse_rooted_id("//apps/api"), None);
    }

    /// The wildcard matcher itself, including the cases that decide whether a
    /// pattern can widen unexpectedly.
    #[test]
    fn glob_matching_is_bounded() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("api", "api"));
        assert!(glob_matches("api-*", "api-v2"));
        assert!(glob_matches("*-service", "auth-service"));
        assert!(glob_matches("a*c", "abbbc"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("api", "api-v2"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(!glob_matches("a*c", "abd"));
        // A separator is never matched by a wildcard: segments are matched
        // individually, so a pattern cannot reach into a deeper level.
        assert!(!glob_matches("*", "apps/api"));
    }

    /// A custom provider with no `run` is an error, not a silent no-op.
    ///
    /// Accepting it would make `osdk deps` report success for a step that never
    /// executed -- the shape this subsystem keeps refusing elsewhere ("could not
    /// check" must not read as "checked and fine").
    #[test]
    fn a_custom_provider_without_a_command_is_refused() {
        let project = detect_custom("codegen", Path::new("/p"), Path::new("/p/osdk.toml"));

        for run in [None, Some(String::new()), Some("   ".to_string())] {
            let config = ProviderConfig {
                run,
                ..ProviderConfig::default()
            };
            assert!(
                plan_custom(&project, &config).is_err(),
                "an absent or blank `run` must not be accepted"
            );
        }

        // And a real command plans, so the check above is discriminating rather
        // than "always fails".
        let config = ProviderConfig {
            run: Some("pnpm run codegen".into()),
            ..ProviderConfig::default()
        };
        let plan = plan_custom(&project, &config).unwrap();
        assert_eq!(plan.program_candidates, vec!["pnpm".to_string()]);
        assert_eq!(plan.args, vec!["run".to_string(), "codegen".to_string()]);
        // Never reported as frozen: there is no lockfile in this model, so
        // `--frozen` must not pass on the strength of a custom step.
        assert!(!plan.frozen);
        assert!(plan.prelude.is_empty());
    }

    /// `[deps.<p>].dir` is applied, and applied for both separator spellings on
    /// every platform.
    ///
    /// Not `#[cfg(windows)]`-gated on purpose. AGENTS.md records that gating a
    /// separator test is a declaration that the other half is never verified,
    /// and that is exactly where the bug hides: `dir` arrives from a committed
    /// `osdk.toml`, so a Windows author can write `apps\api` and a Linux
    /// machine must still find it.
    #[test]
    fn a_configured_dir_is_applied_for_either_separator() {
        let root = Path::new("/projects/app");

        assert_eq!(effective_cwd(root, None), root.to_path_buf());

        let forward = effective_cwd(root, Some("apps/api"));
        let backward = effective_cwd(root, Some("apps\\api"));
        assert_eq!(
            forward, backward,
            "the separator the value was written with must not change where the command runs"
        );
        assert!(forward.ends_with("api"), "{forward:?}");
        assert_eq!(
            forward.strip_prefix(root).unwrap(),
            Path::new("apps").join("api")
        );

        // A value that normalizes to nothing must not move the command out of the
        // project; `relative_segments` drops `.` and empty components.
        assert_eq!(effective_cwd(root, Some("./")), root.to_path_buf());
    }

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
