//! Task-level freshness: skip a task when its inputs have not changed.
//!
//! This is the "task-level increment" boundary the design settles on: enough to
//! replace what people use a Makefile's timestamp checks for, and deliberately
//! short of pattern rules, which would turn a task runner into a build system.
//!
//! Two properties here were learned by getting them wrong in a benchmark, and
//! both fail *silently* when done naively:
//!
//! - **Scanning starts at the glob's literal prefix, never at the cwd.** A
//!   `WalkDir::new(".")` next to a multi-gigabyte `target/` measured 2,827.91 ms
//!   against 61.26 ms in a clean directory -- 46x, for the same answer. The
//!   prefix of `crates/**/*.rs` is `crates`, so that is where the walk begins.
//! - **A pattern that matches nothing is an error, not "up to date".** A
//!   `sources` entry matching zero files makes freshness vacuously true, so the
//!   task is skipped forever, or -- depending on which side is empty -- runs
//!   every time. Either way nothing is reported and the config looks fine.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

use super::args::Values;
use crate::error::{Error, Result};

/// How a task decides whether its inputs changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Freshness {
    /// Compare modification times. Cheap, and wrong after a `git checkout` or a
    /// CI cache restore, both of which rewrite mtimes without changing content.
    #[default]
    Mtime,
    /// Hash file contents. Immune to touched-but-unchanged files, at the cost of
    /// reading every input.
    Hash,
    /// Never skip.
    Always,
}

/// Why a task is going to run, or why it is being skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Inputs are unchanged since the recorded run.
    UpToDate,
    /// Run, with a human-readable reason.
    Run(String),
}

impl Decision {
    pub fn should_run(&self) -> bool {
        matches!(self, Self::Run(_))
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Run(reason) => Some(reason),
            Self::UpToDate => None,
        }
    }
}

/// The literal directory prefix of a normalized glob, stopping before any
/// component that needs glob parsing.
///
/// `crates/**/*.rs` -> `crates`; `src/main.rs` -> `src`; `**/*.rs` -> `` (the
/// root). Starting the walk here is what keeps a `target/`-adjacent scan from
/// costing 46x, and it is purely an optimization: the glob still decides what
/// matches, so narrowing the start cannot change the answer.
pub fn literal_prefix(pattern: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = pattern.split('/').peekable();
    while let Some(component) = components.next() {
        if component.contains(['*', '?', '[', '{', '\\']) {
            break;
        }
        // For an obvious file name, start in its parent directory.
        if components.peek().is_some() || Path::new(component).extension().is_none() {
            prefix.push(component);
        }
    }
    prefix
}

/// One `sources`/`outputs` entry after `!` negation is split off.
struct Pattern<'a> {
    glob: &'a str,
    negated: bool,
}

fn split_patterns(patterns: &[String]) -> Vec<Pattern<'_>> {
    patterns
        .iter()
        .map(|raw| {
            if let Some(rest) = raw.strip_prefix('!') {
                Pattern {
                    glob: rest,
                    negated: true,
                }
            } else if raw.starts_with(r"\!") {
                // `\!` escapes a literal leading `!`.
                Pattern {
                    glob: &raw[1..],
                    negated: false,
                }
            } else {
                Pattern {
                    glob: raw,
                    negated: false,
                }
            }
        })
        .collect()
}

/// Match files and patterns using the same spelling of each path.
fn normalize_slash_segments(
    path: Cow<'_, str>,
    is_separator: impl FnMut(char) -> bool,
) -> Cow<'_, str> {
    if !path.starts_with("./")
        && !path.contains("/./")
        && !path.contains("//")
        && !path.ends_with("/.")
    {
        return path;
    }

    let mut normalized = String::with_capacity(path.len());
    if path.starts_with('/') {
        normalized.push('/');
    }
    for segment in path.split(is_separator).filter(|s| !matches!(*s, "" | ".")) {
        if !normalized.is_empty() && !normalized.ends_with('/') {
            normalized.push('/');
        }
        normalized.push_str(segment);
    }
    // A trailing slash or `/.` denotes a directory, even after normalization.
    if (path.ends_with('/') || path.ends_with("/."))
        && !normalized.is_empty()
        && !normalized.ends_with('/')
    {
        normalized.push('/');
    }
    Cow::Owned(normalized)
}

/// Normalize a candidate relative to `root` for glob matching.
fn normalize(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let text = relative.to_str()?;
    let text = if std::path::MAIN_SEPARATOR == '\\' {
        Cow::Owned(text.replace('\\', "/"))
    } else {
        Cow::Borrowed(text)
    };
    Some(normalize_slash_segments(text, |ch| ch == '/').into_owned())
}

/// How much of the tree a scan actually touched.
///
/// Returned alongside the matches so a test can assert on the *traversal*, not
/// merely the result. A benchmark that counts matches cannot tell a prefixed
/// walk from a whole-tree walk -- both find the same files -- which is exactly
/// how a 46x regression stays invisible. `visited` moves when the pruning
/// breaks, so an assertion on it fails for the right reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCost {
    /// Directory entries the walker yielded, files and directories alike.
    pub visited: usize,
}

fn normalize_pattern(pattern: &str) -> Cow<'_, str> {
    // A forward slash (including a leading `./`) selects glob escapes.
    if !pattern.contains('\\') || pattern.contains('/') || pattern.starts_with('\\') {
        return Cow::Borrowed(pattern);
    }

    let mut chars = pattern.chars();
    let mut escaped_opening = false;
    let mut escaped_closing = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('[') => escaped_opening = true,
                // A paired bracket escape establishes glob syntax. Any later
                // escapes are part of the same filename, including `\.`.
                Some(']') if escaped_opening => return Cow::Borrowed(pattern),
                Some(']') => escaped_closing = true,
                Some('*' | '?' | '{' | '}' | ',' | '!' | '\\') | None => {}
                // Keep reading the bracket expression before deciding whether
                // backslashes in it are escapes or Windows separators.
                Some(_) if escaped_opening => {}
                // A separator before an ordinary character selects Windows
                // syntax for the whole path, including later `\*` or `\]`.
                Some(_) => return Cow::Owned(pattern.replace('\\', "/")),
            },
            // An unescaped closing bracket can end a Windows character class.
            ']' => escaped_opening = false,
            _ => {}
        }
    }

    // Preserve literal brackets in a bare filename. A Windows character class
    // such as `src\[ab].rs` has an unescaped closing bracket.
    if escaped_closing || escaped_opening {
        Cow::Borrowed(pattern)
    } else {
        Cow::Owned(pattern.replace('\\', "/"))
    }
}

fn normalize_glob_segments(pattern: Cow<'_, str>) -> Cow<'_, str> {
    // Slashes in a character class or behind an escape belong to glob syntax.
    // After an optional class negation, the first `]` can be a literal.
    enum State {
        Text,
        Escape,
        ClassStart,
        ClassFirst,
        Class,
    }
    let mut state = State::Text;
    normalize_slash_segments(pattern, |ch| {
        state = match (&state, ch) {
            (State::Text, '/') => return true,
            (State::Text, '\\') => State::Escape,
            (State::Text, '[') => State::ClassStart,
            (State::ClassStart, '!' | '^') => State::ClassFirst,
            (State::ClassStart | State::ClassFirst, _) => State::Class,
            (State::Class, ']') => State::Text,
            (State::Class, _) => State::Class,
            (State::Text | State::Escape, _) => State::Text,
        };
        false
    })
}

fn compile_patterns(patterns: &[String], label: &str) -> Result<(Vec<Glob>, GlobSet)> {
    let mut include = Vec::new();
    let mut exclude = GlobSetBuilder::new();
    for pattern in split_patterns(patterns) {
        let normalized = normalize_glob_segments(normalize_pattern(pattern.glob));
        let glob = GlobBuilder::new(&normalized)
            .backslash_escape(true)
            .build()
            .map_err(|error| {
                Error::config(format!(
                    "{label}: invalid pattern `{}`: {error}",
                    pattern.glob
                ))
            })?;
        if pattern.negated {
            exclude.add(glob);
        } else {
            include.push(glob);
        }
    }
    let exclude = exclude
        .build()
        .map_err(|error| Error::config(format!("{label}: {error}")))?;
    Ok((include, exclude))
}

/// Visit normalized file and symlink entries until the visitor asks to stop.
fn scan_files(
    root: &Path,
    walk: WalkDir,
    label: &str,
    cost: &mut ScanCost,
    mut stop: impl FnMut(&DirEntry, &str) -> bool,
) -> Result<bool> {
    for entry in walk.follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error)
                if error.depth() == 0
                    && error
                        .io_error()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(Error::other(format!("{label}: cannot scan: {error}"))),
        };
        cost.visited += 1;
        if !entry.file_type().is_file() && !entry.path_is_symlink() {
            continue;
        }
        let Some(relative) = normalize(entry.path(), root) else {
            continue;
        };
        if stop(&entry, &relative) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn output_scan_root(root: &Path, prefix: &Path) -> Result<Option<PathBuf>> {
    // A literal prefix may start below a symlink, bypassing WalkDir's link
    // settings. Check its parents in order before handing it to the walker.
    // Build the actual walk path from the checked components, keeping trailing
    // separators away from the final component's no-follow check in WalkDir.
    let mut start = root.to_path_buf();
    let mut components = prefix.components().peekable();
    while let Some(component) = components.next() {
        start.push(component);
        if components.peek().is_none() {
            break;
        }
        match start.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io(&start, error)),
        }
    }
    Ok(Some(start))
}

/// Every positive output pattern must match a file after exclusions.
fn outputs_present(
    root: &Path,
    patterns: &[String],
    label: &str,
    cost: &mut ScanCost,
) -> Result<bool> {
    let (include, exclude) = compile_patterns(patterns, label)?;
    for glob in include {
        let Some(start) = output_scan_root(root, &literal_prefix(glob.glob()))? else {
            return Ok(false);
        };
        let walk = WalkDir::new(start).follow_root_links(false);
        let matcher = glob.compile_matcher();
        if !scan_files(root, walk, label, cost, |entry, relative| {
            matcher.is_match(relative)
                && !exclude.is_match(relative)
                && (entry.file_type().is_file() || entry.path().is_file())
        })? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every file under `root` matching `patterns`.
///
/// Returns an error when a non-negated pattern matches nothing: a typo there is
/// otherwise invisible, and its effect (a task that never reruns, or one that
/// always does) shows up far from the line that caused it.
pub fn resolve(root: &Path, patterns: &[String], label: &str) -> Result<Vec<PathBuf>> {
    resolve_measured(root, patterns, label).map(|(found, _)| found)
}

/// [`resolve`], additionally reporting how much of the tree was walked.
pub fn resolve_measured(
    root: &Path,
    patterns: &[String],
    label: &str,
) -> Result<(Vec<PathBuf>, ScanCost)> {
    if patterns.is_empty() {
        return Ok((Vec::new(), ScanCost::default()));
    }
    let (includes, exclude) = compile_patterns(patterns, label)?;

    // Walk only the literal prefixes, deduplicated, instead of the whole tree.
    let mut roots: Vec<PathBuf> = includes
        .iter()
        .map(|glob| root.join(literal_prefix(glob.glob())))
        .collect();
    let mut include = GlobSetBuilder::new();
    for glob in includes {
        include.add(glob);
    }
    let include = include
        .build()
        .map_err(|error| Error::config(format!("{label}: {error}")))?;
    roots.sort();
    roots.dedup();
    // A nested prefix is already covered by its ancestor.
    let mut pruned: Vec<PathBuf> = Vec::new();
    for candidate in roots {
        if pruned.iter().any(|kept| candidate.starts_with(kept)) {
            continue;
        }
        pruned.push(candidate);
    }

    let mut found = Vec::new();
    let mut cost = ScanCost::default();
    for start in pruned {
        scan_files(
            root,
            WalkDir::new(start),
            label,
            &mut cost,
            |entry, relative| {
                if entry.file_type().is_file()
                    && include.is_match(relative)
                    && !exclude.is_match(relative)
                {
                    found.push(entry.path().to_path_buf());
                }
                false
            },
        )?;
    }
    found.sort();
    found.dedup();

    if found.is_empty() {
        return Err(Error::config(format!(
            "{label}: {:?} matched no files; a pattern that matches nothing would make \
             this task's freshness check silently meaningless",
            patterns
        )));
    }
    Ok((found, cost))
}

/// Newest modification time among `paths`.
fn newest(paths: &[PathBuf]) -> Result<Option<SystemTime>> {
    let mut newest = None;
    for path in paths {
        let modified = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|error| Error::io(path, error))?;
        newest = Some(match newest {
            Some(current) if current >= modified => current,
            _ => modified,
        });
    }
    Ok(newest)
}

/// Oldest modification time among `paths`.
fn oldest(paths: &[PathBuf]) -> Result<Option<SystemTime>> {
    let mut oldest = None;
    for path in paths {
        let modified = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|error| Error::io(path, error))?;
        oldest = Some(match oldest {
            Some(current) if current <= modified => current,
            _ => modified,
        });
    }
    Ok(oldest)
}

/// Content hash of every input, plus the task definition itself.
pub fn input_hash(paths: &[PathBuf], definition: &str) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    // The definition participates so editing a task's commands reruns it even
    // when no input file changed.
    hasher.update(definition.as_bytes());
    for path in paths {
        let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
        if let Some(name) = path.to_str() {
            hasher.update(name.as_bytes());
        }
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn definition_hash(definition: &str) -> String {
    blake3::hash(definition.as_bytes()).to_hex().to_string()
}

fn argument_hash(arguments: Option<&Values>) -> String {
    // Include the parsed map and the ordered argv array. Joining argv with
    // spaces would make one argument containing spaces look like several.
    let empty = Values::default();
    let encoded = serde_json::to_vec(arguments.unwrap_or(&empty))
        .expect("task argument strings can always be serialized");
    blake3::hash(&encoded).to_hex().to_string()
}

/// Where a project's freshness state belongs inside the cache directory.
///
/// Derived data, so it lives under the managed cache rather than beside
/// `osdk.toml`: nobody edits or commits it, and putting it in the project would
/// oblige every consumer to add a `.gitignore` entry. Keyed by a hash of the
/// config path so two projects cannot collide.
pub fn state_path(cache_dir: &Path, project_config: Option<&Path>) -> PathBuf {
    let key = match project_config {
        Some(path) => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.finalize().to_hex()[..16].to_string()
        }
        None => "global".to_string(),
    };
    cache_dir.join("tasks").join(format!("{key}.toml"))
}

/// Persisted freshness state, keyed by task name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreshnessState {
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskState>,
}

/// One task's last successful run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskState {
    /// Content hash of inputs plus definition, for `freshness = "hash"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Task definition fingerprint for `freshness = "mtime"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
    /// Fingerprint of the effective arguments, including an empty invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hash: Option<String>,
    /// Unix **nanoseconds** of the last successful run, as a decimal string.
    ///
    /// Seconds are not enough: file mtimes carry sub-second precision, so a task
    /// finishing in the same second as its source was written compares
    /// `recorded > source` as false and reruns forever -- silently, because
    /// "it ran again" looks exactly like normal operation.
    ///
    /// Stored as a string because TOML has no `u128` and nanoseconds since the
    /// epoch overflow `i64` in 2262; the value is only ever compared, never
    /// used in arithmetic, so a string is the honest carrier rather than a
    /// lossy narrowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<String>,
}

impl FreshnessState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| Error::config(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::io(path, error)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| Error::other(format!("cannot serialize freshness state: {error}")))?;
        std::fs::write(path, text).map_err(|error| Error::io(path, error))
    }
}

/// Everything needed to judge one task.
pub struct Inputs<'a> {
    pub root: &'a Path,
    pub name: &'a str,
    pub sources: &'a [String],
    pub outputs: &'a [String],
    pub freshness: Freshness,
    /// Serialized task definition, so editing commands invalidates the result.
    pub definition: &'a str,
    /// Parsed arguments for the named task, absent for other tasks in the plan.
    pub arguments: Option<&'a Values>,
}

/// Decide whether a task needs to run.
pub fn decide(inputs: &Inputs<'_>, state: &FreshnessState) -> Result<Decision> {
    if inputs.freshness == Freshness::Always {
        return Ok(Decision::Run("freshness = \"always\"".into()));
    }
    if inputs.sources.is_empty() {
        // Without declared inputs there is nothing to compare; always running is
        // the honest answer, not a skip.
        return Ok(Decision::Run("no `sources` declared".into()));
    }

    let sources = resolve(
        inputs.root,
        inputs.sources,
        &format!("task `{}`: sources", inputs.name),
    )?;
    let previous = state.tasks.get(inputs.name);

    if inputs.freshness == Freshness::Mtime {
        let Some(recorded_definition) = previous.and_then(|state| state.definition_hash.as_deref())
        else {
            return Ok(Decision::Run("no recorded task definition".into()));
        };
        if recorded_definition != definition_hash(inputs.definition) {
            return Ok(Decision::Run("task definition changed".into()));
        }
    }

    if let Some(previous) = previous {
        let Some(recorded_arguments) = previous.argument_hash.as_deref() else {
            return Ok(Decision::Run("no recorded task arguments".into()));
        };
        if recorded_arguments != argument_hash(inputs.arguments) {
            return Ok(Decision::Run("task arguments changed".into()));
        }
    }

    if inputs.freshness == Freshness::Hash {
        let hash = input_hash(&sources, inputs.definition)?;
        return Ok(match previous.and_then(|state| state.hash.as_deref()) {
            Some(recorded) if recorded == hash => {
                if outputs_present(
                    inputs.root,
                    inputs.outputs,
                    &format!("task `{}`: outputs", inputs.name),
                    &mut ScanCost::default(),
                )? {
                    Decision::UpToDate
                } else {
                    Decision::Run("outputs missing".into())
                }
            }
            Some(_) => Decision::Run("inputs changed".into()),
            None => Decision::Run("no recorded run".into()),
        });
    }

    let newest_source = newest(&sources)?;

    // Declared outputs: compare against the oldest, so a half-written set of
    // outputs does not read as fresh.
    if !inputs.outputs.is_empty() {
        let outputs = match resolve(
            inputs.root,
            inputs.outputs,
            &format!("task `{}`: outputs", inputs.name),
        ) {
            Ok(outputs) => outputs,
            // Outputs that do not exist yet is the normal first-run state, not a
            // configuration error -- unlike sources, where it means a typo.
            Err(_) => return Ok(Decision::Run("outputs missing".into())),
        };
        let oldest_output = oldest(&outputs)?;
        return Ok(match (newest_source, oldest_output) {
            (Some(source), Some(output)) if output > source => Decision::UpToDate,
            (Some(_), Some(_)) => Decision::Run("a source is newer than an output".into()),
            _ => Decision::Run("outputs missing".into()),
        });
    }

    // No declared outputs: fall back to the recorded run time.
    let Some(ran_at) = previous
        .and_then(|state| state.ran_at.as_deref())
        .and_then(|text| text.parse::<u128>().ok())
    else {
        return Ok(Decision::Run("no recorded run".into()));
    };
    let recorded = SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(ran_at as u64);
    Ok(match newest_source {
        Some(source) if recorded > source => Decision::UpToDate,
        _ => Decision::Run("a source changed since the last run".into()),
    })
}

/// Record a successful run.
pub fn record(inputs: &Inputs<'_>, state: &mut FreshnessState) -> Result<()> {
    if inputs.freshness == Freshness::Always || inputs.sources.is_empty() {
        return Ok(());
    }
    let sources = resolve(
        inputs.root,
        inputs.sources,
        &format!("task `{}`: sources", inputs.name),
    )?;
    let (hash, definition_hash) = if inputs.freshness == Freshness::Hash {
        (Some(input_hash(&sources, inputs.definition)?), None)
    } else {
        (None, Some(definition_hash(inputs.definition)))
    };
    let ran_at = Some(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
            .to_string(),
    );
    state.tasks.insert(
        inputs.name.to_string(),
        TaskState {
            hash,
            definition_hash,
            argument_hash: Some(argument_hash(inputs.arguments)),
            ran_at,
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn inputs<'a>(
        root: &'a Path,
        sources: &'a [String],
        outputs: &'a [String],
        freshness: Freshness,
    ) -> Inputs<'a> {
        Inputs {
            root,
            name: "build",
            sources,
            outputs,
            freshness,
            definition: "run = \"cargo build\"",
            arguments: None,
        }
    }

    #[test]
    fn literal_prefix_stops_at_the_first_metacharacter() {
        assert_eq!(literal_prefix("crates/**/*.rs"), PathBuf::from("crates"));
        assert_eq!(literal_prefix("src/main.rs"), PathBuf::from("src"));
        assert_eq!(literal_prefix("**/*.rs"), PathBuf::new());
        assert_eq!(literal_prefix("a/b/c/*.txt"), PathBuf::from("a/b/c"));
        assert_eq!(
            literal_prefix(r"src/\[slug\]/index.tsx"),
            PathBuf::from("src")
        );
    }

    /// The 46x lesson, asserted on traversal rather than on the result.
    ///
    /// Counting matches would pass whether or not the prefix pruning works --
    /// both walks find the same one file. So this asserts on `visited`, which
    /// only stays small while the walk really does start at `src`. Deleting the
    /// pruning makes the count jump by the size of the decoy tree and the test
    /// fails; that is the property worth locking down.
    #[test]
    fn a_narrow_glob_does_not_walk_a_heavy_sibling_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        // Stand-in for `target/`: many entries the glob can never match.
        for index in 0..200 {
            write(root, &format!("target/debug/deps/x{index}.rs"), "");
        }

        let (found, cost) = resolve_measured(root, &["src/**/*.rs".to_string()], "test").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("a.rs"));

        // `src` holds one file plus its own directory entry. A cwd-rooted walk
        // would visit 200+ more.
        assert!(
            cost.visited <= 4,
            "walk escaped the glob's literal prefix: visited {} entries",
            cost.visited
        );
    }

    /// The counter has to move when the pruning is bypassed, or the assertion
    /// above is decoration. Scanning from the root is the "regression" case.
    #[test]
    fn the_traversal_counter_actually_detects_a_wider_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        for index in 0..200 {
            write(root, &format!("target/debug/deps/x{index}.rs"), "");
        }

        let (_, narrow) = resolve_measured(root, &["src/**/*.rs".to_string()], "test").unwrap();
        // `**/*.rs` has an empty literal prefix, so this one legitimately walks
        // everything -- the same work the narrow glob must avoid.
        let (_, wide) = resolve_measured(root, &["**/*.rs".to_string()], "test").unwrap();

        assert!(
            wide.visited > narrow.visited * 10,
            "counter cannot distinguish walk widths: narrow={} wide={}",
            narrow.visited,
            wide.visited
        );
    }

    /// Both directions, because a one-sided check is how the silent failure got
    /// through: patterns that should match, and patterns that should not.
    #[test]
    fn matching_is_checked_both_ways() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "");
        write(root, "src/b.txt", "");
        write(root, "docs/c.rs", "");

        let found = resolve(root, &["src/**/*.rs".to_string()], "test").unwrap();
        assert_eq!(found.len(), 1, "should match src/a.rs only: {found:?}");

        // Must not match: wrong extension, and right extension in a wrong dir.
        let names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into())
            .collect();
        assert!(!names.contains(&"b.txt".to_string()));
        assert!(!names.contains(&"c.rs".to_string()));
    }

    /// A pattern matching nothing must fail loudly.
    ///
    /// This is the exact shape of the bug found while benchmarking: `**/*.marker`
    /// matched zero files because the walker emitted `./`-prefixed paths, and the
    /// freshness check quietly became a no-op.
    #[test]
    fn a_pattern_matching_nothing_is_an_error_not_a_silent_skip() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "src/a.rs", "");

        let error = resolve(
            temp.path(),
            &["src/**/*.marker".to_string()],
            "task `x`: sources",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("matched no files"), "{error}");
        assert!(error.contains("silently meaningless"), "{error}");
    }

    /// `./`-prefixed walker output must still match a plain pattern.
    #[test]
    fn leading_dot_slash_does_not_break_matching() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "a.marker", "");
        for pattern in ["**/*.marker", "./**/*.marker", r".\a.marker"] {
            let found = resolve(root, &[pattern.into()], "test").unwrap();
            assert_eq!(found, [root.join("a.marker")], "{pattern}");
        }
    }

    #[test]
    fn normalizing_path_segments_preserves_glob_character_classes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let hidden = write(root, "dist/.hidden.txt", "");
        write(root, "dist/xhidden.txt", "");

        let found = resolve(root, &["dist/./[/./]hidden.txt".into()], "test").unwrap();
        assert_eq!(found, [hidden]);
    }

    #[test]
    fn negation_excludes_and_can_be_escaped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/keep.rs", "");
        write(root, "src/skip.rs", "");

        let found = resolve(
            root,
            &["src/**/*.rs".to_string(), "!src/skip.rs".to_string()],
            "test",
        )
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("keep.rs"));

        let literal = write(root, "!keep.rs", "");
        assert_eq!(
            resolve(root, &[r"\!keep.rs".into()], "test").unwrap(),
            [literal]
        );
    }

    #[test]
    fn outputs_newer_than_sources_is_up_to_date_and_touching_a_source_reruns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(root, "out/bin", "built");

        let sources = vec!["src/**/*.rs".to_string()];
        let outputs = vec!["out/**".to_string()];
        let mut state = FreshnessState::default();
        let spec = inputs(root, &sources, &outputs, Freshness::Mtime);
        record(&spec, &mut state).unwrap();

        let decision = decide(&spec, &state).unwrap();
        assert_eq!(decision, Decision::UpToDate, "{decision:?}");

        std::thread::sleep(std::time::Duration::from_millis(20));
        write(root, "src/a.rs", "fn a() { changed }");
        let decision = decide(&spec, &state).unwrap();
        assert!(decision.should_run(), "{decision:?}");
        assert!(decision.reason().unwrap().contains("newer"), "{decision:?}");
    }

    #[test]
    fn missing_outputs_mean_run_not_a_configuration_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "");

        let sources = ["src/**/*.rs".to_string()];
        let outputs = ["out/**".to_string()];
        let spec = inputs(root, &sources, &outputs, Freshness::Mtime);
        let mut state = FreshnessState::default();
        let output = write(root, "out/bin", "built");
        record(&spec, &mut state).unwrap();

        std::fs::remove_file(output).unwrap();
        let decision = decide(&spec, &state).unwrap();
        assert!(decision.should_run());
        assert!(
            decision.reason().unwrap().contains("missing"),
            "{decision:?}"
        );
    }

    #[test]
    fn a_hash_match_requires_every_output_pattern_to_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        write(root, "dist/sidecar.txt", "metadata");
        write(root, "dist/ignored/result.txt", "excluded");
        let result = write(root, "dist/actual/result.txt", "fixture");
        let sources = ["input.txt".into()];

        for pattern in ["dist/*/result.txt", r"dist\*\result.txt"] {
            let outputs = [
                "dist/sidecar.txt".into(),
                pattern.into(),
                "!dist/ignored/**".into(),
            ];
            let spec = inputs(root, &sources, &outputs, Freshness::Hash);
            let mut state = FreshnessState::default();
            record(&spec, &mut state).unwrap();
            assert_eq!(decide(&spec, &state).unwrap(), Decision::UpToDate);

            std::fs::remove_file(&result).unwrap();
            assert_eq!(
                decide(&spec, &state).unwrap(),
                Decision::Run("outputs missing".into()),
                "a sidecar and an excluded file cannot satisfy {pattern}"
            );
            write(root, "dist/actual/result.txt", "fixture");
        }
    }

    #[test]
    fn checking_output_presence_stops_at_the_first_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..200 {
            write(root, &format!("dist/result-{index}.txt"), "");
        }
        let mut cost = ScanCost::default();
        assert!(outputs_present(root, &["dist/*.txt".into()], "test", &mut cost).unwrap());
        assert_eq!(cost.visited, 2, "one directory and its first matching file");
    }

    #[test]
    fn a_hash_match_accepts_an_output_with_a_dot_slash_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        write(root, "dist/result.txt", "built");
        let sources = ["input.txt".into()];
        let outputs = ["./dist/result.txt".into()];
        let spec = inputs(root, &sources, &outputs, Freshness::Hash);
        let mut state = FreshnessState::default();
        record(&spec, &mut state).unwrap();
        assert_eq!(decide(&spec, &state).unwrap(), Decision::UpToDate);
    }

    #[test]
    fn a_hash_match_accepts_an_output_with_an_internal_dot_component() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        let sources = ["input.txt".into()];

        for pattern in [
            "dist/./libs/result.txt",
            "dist/././libs/result.txt",
            "dist/libs/./*.txt",
            "dist/*/./result.txt",
            "dist//libs//result.txt",
            r"dist\.\libs\result.txt",
        ] {
            let result = write(root, "dist/libs/result.txt", "built");
            let outputs = [pattern.into()];
            let spec = inputs(root, &sources, &outputs, Freshness::Hash);
            let mut state = FreshnessState::default();
            record(&spec, &mut state).unwrap();
            assert_eq!(
                decide(&spec, &state).unwrap(),
                Decision::UpToDate,
                "{pattern}"
            );

            std::fs::remove_file(result).unwrap();
            assert_eq!(
                decide(&spec, &state).unwrap(),
                Decision::Run("outputs missing".into()),
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_hash_match_excludes_outputs_using_a_dot_slash_pattern() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        write(root, "dist/ignored/result.txt", "excluded");
        let result = write(root, "dist/result.txt", "built");
        let sources = ["input.txt".into()];
        let outputs = ["dist/**".into(), "!./dist/ignored/**".into()];
        let spec = inputs(root, &sources, &outputs, Freshness::Hash);
        let mut state = FreshnessState::default();
        record(&spec, &mut state).unwrap();
        assert_eq!(decide(&spec, &state).unwrap(), Decision::UpToDate);

        std::fs::remove_file(result).unwrap();
        assert_eq!(
            decide(&spec, &state).unwrap(),
            Decision::Run("outputs missing".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hash_match_accepts_only_file_symlinks_with_existing_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        let target = write(root, "dist/libdemo.so.1", "built");
        let sources = ["input.txt".into()];
        let outputs = ["dist/libdemo.so".into(), "dist/libdemo".into()];
        for link in &outputs {
            std::os::unix::fs::symlink("libdemo.so.1", root.join(link)).unwrap();
        }
        let spec = inputs(root, &sources, &outputs, Freshness::Hash);
        let mut state = FreshnessState::default();
        record(&spec, &mut state).unwrap();
        assert_eq!(decide(&spec, &state).unwrap(), Decision::UpToDate);

        std::fs::remove_file(target).unwrap();
        assert_eq!(
            decide(&spec, &state).unwrap(),
            Decision::Run("outputs missing".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn checking_output_presence_does_not_walk_symlinked_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "outside/nested/result.txt", "built");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::os::unix::fs::symlink("../outside", root.join("dist/linked")).unwrap();

        for (pattern, max_entries) in [
            ("dist/**/result.txt", 2),
            ("dist/linked", 1),
            ("dist/linked/**/result.txt", 1),
            ("dist/linked/nested/result.txt", 1),
            ("dist/linked/./**/result.txt", 1),
            ("dist/linked/./nested/result.txt", 1),
            ("dist/linked//*.txt", 1),
            (r"dist\linked\\*.txt", 1),
        ] {
            let mut cost = ScanCost::default();
            assert!(
                !outputs_present(root, &[pattern.into()], "test", &mut cost).unwrap(),
                "{pattern} reached the target of a directory symlink"
            );
            assert!(
                cost.visited <= max_entries,
                "{pattern} scanned into the link: {cost:?}"
            );
        }
    }

    #[test]
    fn a_hash_match_does_not_hide_an_invalid_output_pattern() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "input.txt", "fixture");
        let sources = ["input.txt".into()];
        let outputs = ["dist/[".into()];
        let spec = inputs(root, &sources, &outputs, Freshness::Hash);
        let mut state = FreshnessState::default();
        record(&spec, &mut state).unwrap();
        let error = decide(&spec, &state).unwrap_err().to_string();
        assert!(
            error.contains("outputs: invalid pattern `dist/[`"),
            "{error}"
        );
    }

    #[test]
    fn hash_mode_ignores_a_touch_that_did_not_change_contents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");

        let sources = vec!["src/**/*.rs".to_string()];
        let no_outputs: Vec<String> = Vec::new();
        let mut state = FreshnessState::default();
        let spec = inputs(root, &sources, &no_outputs, Freshness::Hash);

        assert!(decide(&spec, &state).unwrap().should_run(), "first run");
        record(&spec, &mut state).unwrap();
        assert_eq!(decide(&spec, &state).unwrap(), Decision::UpToDate);

        // Rewrite identical contents: mtime moves, hash does not.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(root, "src/a.rs", "fn a() {}");
        assert_eq!(
            decide(&spec, &state).unwrap(),
            Decision::UpToDate,
            "a touch must not invalidate a hash-mode task"
        );

        write(root, "src/a.rs", "fn a() { different }");
        assert!(decide(&spec, &state).unwrap().should_run());
    }

    /// Editing the task itself must rerun it even when no input file moved.
    #[test]
    fn changing_the_definition_invalidates_a_hash_mode_task() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        let sources = vec!["src/**/*.rs".to_string()];
        let no_outputs: Vec<String> = Vec::new();

        let mut state = FreshnessState::default();
        let first = inputs(root, &sources, &no_outputs, Freshness::Hash);
        record(&first, &mut state).unwrap();
        assert_eq!(decide(&first, &state).unwrap(), Decision::UpToDate);

        let second = Inputs {
            definition: "run = \"cargo build --release\"",
            ..inputs(root, &sources, &no_outputs, Freshness::Hash)
        };
        assert!(
            decide(&second, &state).unwrap().should_run(),
            "editing the command must invalidate"
        );
    }

    #[test]
    fn changing_the_definition_invalidates_a_mtime_task() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = write(root, "input.txt", "fixture");
        std::fs::OpenOptions::new()
            .write(true)
            .open(source)
            .unwrap()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        write(root, "dist/result.txt", "previous output");
        let sources = ["input.txt".into()];
        let outputs = ["dist/result.txt".into()];

        for outputs in [&outputs[..], &[]] {
            let mut state = FreshnessState::default();
            let first = inputs(root, &sources, outputs, Freshness::Mtime);
            record(&first, &mut state).unwrap();
            assert_eq!(decide(&first, &state).unwrap(), Decision::UpToDate);

            let second = Inputs {
                definition: "run = \"cargo build --release\"",
                ..first
            };
            assert_eq!(
                decide(&second, &state).unwrap(),
                Decision::Run("task definition changed".into()),
                "edited command with outputs {outputs:?}"
            );
            record(&second, &mut state).unwrap();
            assert_eq!(decide(&second, &state).unwrap(), Decision::UpToDate);
        }
    }

    #[test]
    fn a_legacy_mtime_record_without_a_definition_fingerprint_reruns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = write(root, "input.txt", "fixture");
        std::fs::OpenOptions::new()
            .write(true)
            .open(source)
            .unwrap()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        write(root, "dist/result.txt", "previous output");
        let state_path = write(
            root,
            "state.toml",
            "[tasks.build]\nran_at = \"42000000000\"\n",
        );
        let state = FreshnessState::load(&state_path).unwrap();
        assert_eq!(state.tasks["build"].ran_at.as_deref(), Some("42000000000"));

        let sources = ["input.txt".into()];
        let outputs = ["dist/result.txt".into()];
        let spec = inputs(root, &sources, &outputs, Freshness::Mtime);
        for state in [state, FreshnessState::default()] {
            assert_eq!(
                decide(&spec, &state).unwrap(),
                Decision::Run("no recorded task definition".into())
            );
        }
    }

    #[test]
    fn always_never_skips_and_no_sources_never_skips() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "");
        let sources = vec!["src/**/*.rs".to_string()];
        let none: Vec<String> = Vec::new();

        assert!(decide(
            &inputs(root, &sources, &none, Freshness::Always),
            &FreshnessState::default()
        )
        .unwrap()
        .should_run());
        assert!(decide(
            &inputs(root, &none, &none, Freshness::Mtime),
            &FreshnessState::default()
        )
        .unwrap()
        .should_run());
    }

    /// The sub-second case, which a seconds-resolution timestamp gets wrong.
    ///
    /// Everything here happens inside one wall-clock second, so a record stored
    /// as whole seconds compares equal to the source mtime, `recorded > source`
    /// is false, and the task reruns forever while looking perfectly healthy.
    #[test]
    fn a_task_finishing_in_the_same_second_as_its_source_is_still_up_to_date() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");

        let sources = vec!["src/**/*.rs".to_string()];
        let none: Vec<String> = Vec::new();
        let spec = inputs(root, &sources, &none, Freshness::Mtime);
        let mut state = FreshnessState::default();

        // No sleep: record and re-check within the same second.
        record(&spec, &mut state).unwrap();
        assert_eq!(
            decide(&spec, &state).unwrap(),
            Decision::UpToDate,
            "sub-second resolution lost: a task cannot be fresh only after a second passes"
        );
    }

    #[test]
    fn state_round_trips_through_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.toml");

        assert!(FreshnessState::load(&path).unwrap().tasks.is_empty());

        let mut state = FreshnessState::default();
        state.tasks.insert(
            "build".into(),
            TaskState {
                hash: Some("abc".into()),
                definition_hash: Some("def".into()),
                argument_hash: Some("arg".into()),
                ran_at: Some("42000000000".into()),
            },
        );
        state.save(&path).unwrap();

        let loaded = FreshnessState::load(&path).unwrap();
        assert_eq!(loaded.tasks["build"].hash.as_deref(), Some("abc"));
        assert_eq!(
            loaded.tasks["build"].definition_hash.as_deref(),
            Some("def")
        );
        assert_eq!(loaded.tasks["build"].ran_at.as_deref(), Some("42000000000"));
        assert_eq!(loaded.tasks["build"].argument_hash.as_deref(), Some("arg"));
    }

    #[test]
    fn an_invalid_pattern_is_rejected_with_the_pattern_in_the_message() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve(temp.path(), &["src/[".to_string()], "task `x`: sources")
            .unwrap_err()
            .to_string();
        assert!(error.contains("src/["), "{error}");
    }
}
