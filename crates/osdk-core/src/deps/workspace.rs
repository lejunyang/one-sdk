//! Native workspace membership, shared installations and lockfile ownership.
use super::DetectedProject;
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) const CARGO_PROGRAM_CANDIDATES: [&str; 2] = ["cargo.exe", "cargo"];
pub const CARGO_LOCKFILE_ENV: &str = "CARGO_RESOLVER_LOCKFILE_PATH";
const CARGO_WORKSPACE_QUERY: &[&str] = &[
    "metadata",
    "--no-deps",
    "--format-version",
    "1",
    "--offline",
];

pub struct NativeLockOwner {
    pub root: PathBuf,
    pub lock: Option<PathBuf>,
    /// Absolute paths relevant to freshness when a lock belongs to a workspace.
    pub inputs: Vec<PathBuf>,
    /// Cargo can validate a locked fetch even when metadata cannot prove freshness.
    pub inputs_complete: bool,
    pub status: NativeLockStatus,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeLockStatus {
    /// The tool's answer is conclusive, including membership errors.
    Checked,
    /// The configured tool is not available for a read-only query yet.
    CargoUnavailable,
    /// Cargo confirmed membership, but the effective lock path is unresolved.
    CargoLockPathUnknown,
}

pub struct NativeLockQuery {
    pub output: std::process::Output,
    pub cargo_home: Option<PathBuf>,
    pub cargo_lockfile_override: Option<PathBuf>,
}

/// A read-only native query using a tool that is already available.
pub trait NativeLockProbe {
    fn query(
        &self,
        program_candidates: &[String],
        args: &[&str],
        cwd: &Path,
    ) -> std::io::Result<Option<NativeLockQuery>>;
}

#[derive(serde::Deserialize)]
struct CargoMetadata {
    workspace_root: PathBuf,
    packages: Vec<CargoPackage>,
}

#[derive(serde::Deserialize)]
struct CargoPackage {
    manifest_path: PathBuf,
    dependencies: Vec<CargoDependency>,
}

#[derive(serde::Deserialize)]
struct CargoDependency {
    path: Option<PathBuf>,
}

pub fn native_lock_owner(
    project: &DetectedProject,
    cwd: &Path,
    probe: &dyn NativeLockProbe,
) -> Result<NativeLockOwner> {
    let mut owner = NativeLockOwner {
        root: project.root.clone(),
        lock: project.native_lock.clone(),
        inputs: Vec::new(),
        inputs_complete: true,
        status: NativeLockStatus::Checked,
        reason: None,
    };
    if project.provider != "cargo" {
        return Ok(owner);
    }
    // Cargo alone decides whether the lock beside the selected manifest is used.
    owner.lock = None;
    owner.inputs_complete = false;

    let candidates = CARGO_PROGRAM_CANDIDATES.map(str::to_owned);
    let query = match probe.query(&candidates, CARGO_WORKSPACE_QUERY, cwd) {
        Ok(Some(query)) if query.output.status.success() => Ok(query),
        Ok(Some(query)) => {
            let stderr = String::from_utf8_lossy(&query.output.stderr);
            let first_line = stderr.lines().find(|line| !line.trim().is_empty());
            Err(match first_line {
                Some(line) => format!(
                    "Cargo could not determine workspace membership: {}",
                    crate::package_sources::display_value(line.trim())
                ),
                None => format!(
                    "Cargo could not determine workspace membership ({})",
                    query.output.status
                ),
            })
        }
        Ok(None) => {
            owner.status = NativeLockStatus::CargoUnavailable;
            Err("cannot determine Cargo workspace membership without an installed or linked Cargo; run `osdk install rust`".into())
        }
        Err(error) => Err(format!("cannot query Cargo workspace membership: {error}")),
    };
    let result = query.and_then(|query| {
        owner
            .apply_cargo_metadata(project, cwd, query, probe)
            .map_err(|error| format!("cannot use Cargo workspace metadata: {error}"))
    });
    if let Err(reason) = result {
        owner.reason = Some(reason);
    }
    Ok(owner)
}

impl NativeLockOwner {
    fn apply_cargo_metadata(
        &mut self,
        project: &DetectedProject,
        cwd: &Path,
        query: NativeLockQuery,
        probe: &dyn NativeLockProbe,
    ) -> Result<()> {
        let metadata: CargoMetadata = serde_json::from_slice(&query.output.stdout)
            .map_err(|_| Error::config("Cargo returned invalid workspace metadata"))?;
        let canonicalize = |path: &Path| dunce::canonicalize(path).map_err(|e| Error::io(path, e));
        let root = canonicalize(&metadata.workspace_root)?;
        let manifests = metadata
            .packages
            .iter()
            .map(|package| canonicalize(&package.manifest_path))
            .collect::<Result<Vec<_>>>()?;
        let is_root = root == canonicalize(cwd)?;
        // With an explicit deps.dir Cargo finds the manifest from that cwd.
        // Otherwise require Cargo's answer to include the selected package.
        if !is_root && cwd == project.root && !manifests.contains(&canonicalize(&project.manifest)?)
        {
            self.reason = Some(
                "Cargo metadata does not list the selected manifest as a workspace member".into(),
            );
            return Ok(());
        }

        self.root.clone_from(&root);
        let (config, configured) = match cargo_lock_configuration(cwd, &query, probe) {
            Ok(configuration) => configuration,
            Err(error) => {
                self.status = NativeLockStatus::CargoLockPathUnknown;
                self.reason = Some(format!(
                    "cannot establish the effective Cargo lockfile: {error}"
                ));
                return Ok(());
            }
        };
        let lock = configured
            .clone()
            .unwrap_or_else(|| root.join("Cargo.lock"));
        let root_manifest = root.join("Cargo.toml");
        // --no-deps can describe direct path dependencies of every member,
        // but cannot see the dependency graph behind nonmembers or overrides.
        self.inputs_complete = !config.local_overrides
            && read_cargo_toml(&root_manifest).is_ok_and(|manifest| {
                !["patch", "replace"]
                    .iter()
                    .any(|key| manifest.contains_key(*key))
            })
            && metadata
                .packages
                .iter()
                .flat_map(|package| &package.dependencies)
                .all(|dependency| {
                    dependency.path.as_ref().is_none_or(|path| {
                        dunce::canonicalize(path.join("Cargo.toml"))
                            .is_ok_and(|path| manifests.contains(&path))
                    })
                });
        self.inputs = manifests;
        self.inputs.extend([root_manifest, lock.clone()]);
        self.inputs.extend(config.files);
        if lock.is_file() {
            self.lock = Some(lock);
        } else if configured.is_some() {
            self.reason = Some(format!(
                "configured Cargo lockfile {} is missing",
                lock.display()
            ));
        } else if is_root {
            self.reason = Some(format!(
                "no Cargo.lock in {}; Cargo uses this directory as its workspace root",
                root.display()
            ));
        } else {
            self.reason = Some(format!(
                "no Cargo.lock in Cargo workspace root {}",
                root.display()
            ));
        }
        Ok(())
    }
}

fn cargo_lock_configuration(
    cwd: &Path,
    query: &NativeLockQuery,
    probe: &dyn NativeLockProbe,
) -> Result<(CargoConfigInputs, Option<PathBuf>)> {
    let cargo_home = query.cargo_home.as_deref().ok_or_else(|| {
        Error::config("the fetch environment leaves Cargo's home to the operating system")
    })?;
    let mut config = CargoConfigInputs::load(cwd, cargo_home)?;
    let override_path = query
        .cargo_lockfile_override
        .as_ref()
        .map(|path| cwd.join(path))
        .or_else(|| config.lockfile.take());
    let configured = match override_path {
        Some(path) if cargo_supports_lockfile_override(cwd, probe)? => Some(path),
        _ => None,
    };
    Ok((config, configured))
}

fn cargo_supports_lockfile_override(cwd: &Path, probe: &dyn NativeLockProbe) -> Result<bool> {
    let candidates = CARGO_PROGRAM_CANDIDATES.map(str::to_owned);
    let version = probe
        .query(&candidates, &["--version"], cwd)
        .map_err(|error| Error::io(cwd, error))?
        .filter(|query| query.output.status.success())
        .and_then(|query| {
            String::from_utf8_lossy(&query.output.stdout)
                .split_whitespace()
                .nth(1)
                .and_then(|version| semver::Version::parse(version).ok())
        })
        .ok_or_else(|| Error::config("could not determine Cargo lockfile-path support"))?;
    Ok((version.major, version.minor) >= (1, 97))
}

#[derive(Default)]
struct CargoConfigInputs {
    lockfile: Option<PathBuf>,
    files: Vec<PathBuf>,
    local_overrides: bool,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum CargoConfigInclude {
    Path(PathBuf),
    Table { path: PathBuf },
}

impl CargoConfigInputs {
    fn load(cwd: &Path, cargo_home: &Path) -> Result<Self> {
        // Stack order mirrors Cargo's scalar precedence: cwd before parents,
        // then CARGO_HOME. The extensionless filename wins at each location.
        let mut pending: Vec<_> = cwd
            .ancestors()
            .map(|dir| dir.join(".cargo"))
            .chain(std::iter::once(cargo_home.to_path_buf()))
            .filter_map(|dir| {
                ["config", "config.toml"]
                    .map(|name| dir.join(name))
                    .into_iter()
                    .find(|path| path.is_file())
            })
            .collect();
        pending.reverse();
        let mut visited = HashSet::new();
        let mut result = Self::default();
        while let Some(path) = pending.pop() {
            // Cargo already validated the include graph during metadata.
            if !path.is_file() || !visited.insert(path.clone()) {
                continue;
            }
            let config = read_cargo_toml(&path)?;
            let directory = path.parent().unwrap();
            if result.lockfile.is_none() {
                result.lockfile = config
                    .get("resolver")
                    .and_then(|resolver| resolver.get("lockfile-path"))
                    .and_then(toml::Value::as_str)
                    .map(|lockfile| directory.parent().unwrap_or(directory).join(lockfile));
            }
            result.local_overrides |= ["patch", "paths", "source"]
                .iter()
                .any(|key| config.contains_key(*key));
            if let Some(include) = config.get("include") {
                let includes: Vec<CargoConfigInclude> = include
                    .clone()
                    .try_into()
                    .map_err(|error| Error::config(format!("{}: {error}", path.display())))?;
                // Later includes take precedence over earlier ones, below
                // the file that includes them.
                pending.extend(includes.into_iter().map(|include| {
                    let include = match include {
                        CargoConfigInclude::Path(path) | CargoConfigInclude::Table { path } => path,
                    };
                    // Cargo normalizes the include before interpreting any
                    // relative paths defined by that file.
                    crate::fs::normalize_lexically(&directory.join(include))
                }));
            }
            result.files.push(path);
        }
        Ok(result)
    }
}

fn read_cargo_toml(path: &Path) -> Result<toml::Table> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    toml::from_str(&text)
        .map_err(|error| Error::config(format!("{}: {}", path.display(), error.message())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceUnit {
    pub root: PathBuf,
    pub provider: String,
    pub definition: PathBuf,
    pub members: Vec<PathBuf>,
}
impl WorkspaceUnit {
    pub fn sources(&self) -> Vec<PathBuf> {
        let manifest = if self.provider == "uv" {
            "pyproject.toml"
        } else {
            "package.json"
        };
        let lock = if self.provider == "uv" {
            "uv.lock"
        } else {
            "pnpm-lock.yaml"
        };
        let mut paths = vec![
            self.definition.clone(),
            self.root.join(manifest),
            self.root.join(lock),
        ];
        paths.extend(self.members.iter().map(|member| member.join(manifest)));
        paths.sort();
        paths.dedup();
        paths
    }
    pub fn coverage(&self) -> Vec<String> {
        std::iter::once(&self.root)
            .chain(&self.members)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect()
    }
}

pub fn detect(root: &Path) -> Result<Vec<WorkspaceUnit>> {
    let canonical = root
        .canonicalize()
        .map_err(|error| Error::io(root, error))?;
    let root = canonical.as_path();
    let mut result = Vec::new();
    let pnpm = root.join("pnpm-workspace.yaml");
    if pnpm.is_file() {
        let text = std::fs::read_to_string(&pnpm).map_err(|e| Error::io(&pnpm, e))?;
        let value: serde_yaml::Value = serde_yaml::from_str(&text)
            .map_err(|e| Error::config(format!("{}: {e}", pnpm.display())))?;
        let patterns = yaml_strings(value.get("packages"), &pnpm)?;
        let (exclude, include): (Vec<_>, Vec<_>) =
            patterns.into_iter().partition(|p| p.starts_with('!'));
        let exclude: Vec<_> = exclude.into_iter().map(|p| p[1..].to_string()).collect();
        result.push(WorkspaceUnit {
            root: root.into(),
            provider: "pnpm".into(),
            definition: pnpm,
            members: expand_members(root, &include, &exclude, "package.json")?,
        });
    }
    let pyproject = root.join("pyproject.toml");
    if pyproject.is_file() {
        let text = std::fs::read_to_string(&pyproject).map_err(|e| Error::io(&pyproject, e))?;
        let value: toml::Value = toml::from_str(&text)
            .map_err(|e| Error::config(format!("{}: {}", pyproject.display(), e.message())))?;
        if let Some(workspace) = value
            .get("tool")
            .and_then(|v| v.get("uv"))
            .and_then(|v| v.get("workspace"))
        {
            let include = toml_strings(workspace.get("members"), &pyproject)?;
            let exclude = toml_strings(workspace.get("exclude"), &pyproject)?;
            result.push(WorkspaceUnit {
                root: root.into(),
                provider: "uv".into(),
                definition: pyproject,
                members: expand_members(root, &include, &exclude, "pyproject.toml")?,
            });
        }
    }
    Ok(result)
}
fn yaml_strings(value: Option<&serde_yaml::Value>, path: &Path) -> Result<Vec<String>> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => value
            .as_sequence()
            .ok_or_else(|| Error::config(format!("{}: packages must be a list", path.display())))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Error::config("workspace patterns must be strings"))
            })
            .collect(),
    }
}
fn toml_strings(value: Option<&toml::Value>, path: &Path) -> Result<Vec<String>> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => value
            .as_array()
            .ok_or_else(|| {
                Error::config(format!(
                    "{}: workspace patterns must be lists",
                    path.display()
                ))
            })?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Error::config("workspace patterns must be strings"))
            })
            .collect(),
    }
}
fn matcher(pattern: &str) -> Result<globset::GlobMatcher> {
    if Path::new(pattern).is_absolute()
        || pattern.contains(['\\', ':'])
        || pattern.split('/').any(|p| p == "..")
    {
        return Err(Error::config(format!(
            "workspace pattern must remain inside its root: {pattern}"
        )));
    }
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| Error::config(e.to_string()))
}

fn expand_members(
    root: &Path,
    includes: &[String],
    excludes: &[String],
    manifest: &str,
) -> Result<Vec<PathBuf>> {
    let exclude = excludes
        .iter()
        .map(|p| matcher(p))
        .collect::<Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for pattern in includes {
        matcher(pattern)?; // Validate before touching any directory.
        let parts: Vec<_> = pattern
            .trim_start_matches("./")
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.first() == Some(&"**") {
            return Err(Error::config(
                "workspace `**` requires a bounded directory prefix, such as packages/**",
            ));
        }
        expand(root, root, &parts, &exclude, &mut result)?;
    }
    result = result
        .into_iter()
        .map(|path| path.canonicalize().map_err(|e| Error::io(&path, e)))
        .collect::<Result<_>>()?;
    result.sort();
    result.dedup();
    result.retain(|path| path != root && path.join(manifest).is_file());
    Ok(result)
}
fn expand(
    root: &Path,
    directory: &Path,
    parts: &[&str],
    excludes: &[globset::GlobMatcher],
    result: &mut Vec<PathBuf>,
) -> Result<()> {
    let relative = directory
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    if excludes.iter().any(|g| g.is_match(&relative)) {
        return Ok(());
    }
    let Some((first, rest)) = parts.split_first() else {
        result.push(directory.into());
        return Ok(());
    };
    if *first == "**" {
        expand(root, directory, rest, excludes, result)?;
    }
    if !first.contains(['*', '?', '[', '{']) {
        let next = directory.join(first);
        if next.is_dir() {
            check_boundary(root, &next)?;
            expand(root, &next, rest, excludes, result)?;
        }
        return Ok(());
    }
    let glob = matcher(first)?;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(directory, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(directory, e))?;
        let kind = entry.file_type().map_err(|e| Error::io(entry.path(), e))?;
        if !kind.is_dir() {
            continue;
        } // Never traverse symlinks.
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ["node_modules", ".venv", ".git", "target"].contains(&&*name) {
            continue;
        }
        if *first == "**" || glob.is_match(&*name) {
            check_boundary(root, &entry.path())?;
            expand(
                root,
                &entry.path(),
                if *first == "**" { parts } else { rest },
                excludes,
                result,
            )?;
        }
    }
    Ok(())
}
fn check_boundary(root: &Path, directory: &Path) -> Result<()> {
    let canonical = directory
        .canonicalize()
        .map_err(|e| Error::io(directory, e))?;
    if !canonical.starts_with(root) {
        return Err(Error::config("workspace member escapes its root"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CargoProbe {
        metadata: serde_json::Value,
        cargo_home: Option<PathBuf>,
    }

    impl NativeLockProbe for CargoProbe {
        fn query(
            &self,
            candidates: &[String],
            args: &[&str],
            cwd: &Path,
        ) -> std::io::Result<Option<NativeLockQuery>> {
            assert!(candidates.iter().any(|name| name == "cargo"));
            assert_eq!(
                args,
                [
                    "metadata",
                    "--no-deps",
                    "--format-version",
                    "1",
                    "--offline"
                ]
            );
            assert!(cwd.join("Cargo.toml").is_file());
            Ok(Some(NativeLockQuery {
                output: std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: serde_json::to_vec(&self.metadata).unwrap(),
                    stderr: Vec::new(),
                },
                cargo_home: self.cargo_home.clone(),
                cargo_lockfile_override: None,
            }))
        }
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn cargo_metadata_binds_the_root_lock_to_listed_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "demo/Cargo.toml",
            "other/Cargo.toml",
        ] {
            write(&root, file, "");
        }
        let member = DetectedProject {
            provider: "cargo".into(),
            ecosystem: super::super::Ecosystem::Rust,
            root: root.join("demo"),
            manifest: root.join("demo/Cargo.toml"),
            native_lock: None,
            declared_manager: None,
        };
        let other = root.join("other/Cargo.toml");
        let probe = |manifests: &[&Path]| CargoProbe {
            metadata: serde_json::json!({
                "workspace_root": root,
                "packages": manifests.iter().map(|path| serde_json::json!({"manifest_path": path, "dependencies": []})).collect::<Vec<_>>(),
            }),
            cargo_home: Some(root.join("cargo-home")),
        };
        let owner =
            native_lock_owner(&member, &member.root, &probe(&[&member.manifest, &other])).unwrap();
        let lock = root.join("Cargo.lock");
        assert_eq!(owner.root, root);
        assert_eq!(owner.lock.as_ref(), Some(&lock));
        assert_eq!(
            owner.inputs,
            [
                member.manifest.clone(),
                other.clone(),
                root.join("Cargo.toml"),
                lock
            ]
        );
        assert!(owner.reason.is_none());
        assert!(owner.inputs_complete);
        assert_eq!(owner.status, NativeLockStatus::Checked);

        let mut unknown_home = probe(&[&member.manifest, &other]);
        unknown_home.cargo_home = None;
        let owner = native_lock_owner(&member, &member.root, &unknown_home).unwrap();
        assert_eq!(owner.root, root);
        assert!(owner.lock.is_none());
        assert!(!owner.inputs_complete);
        assert_eq!(owner.status, NativeLockStatus::CargoLockPathUnknown);

        let owner = native_lock_owner(&member, &member.root, &probe(&[&other])).unwrap();
        assert!(owner.lock.is_none());
        assert!(owner.inputs.is_empty());
        assert_eq!(owner.status, NativeLockStatus::Checked);
        assert!(owner
            .reason
            .unwrap()
            .contains("does not list the selected manifest"));
    }

    #[test]
    fn native_members_exclusions_and_changes_are_freshness_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical_root = tmp.path().canonicalize().unwrap();
        let root = canonical_root.as_path();
        write(
            root,
            "pnpm-workspace.yaml",
            "packages: ['packages/*', '!packages/excluded']\n",
        );
        for member in ["a", "b", "excluded"] {
            write(root, &format!("packages/{member}/package.json"), "{}");
        }
        write(root, "unrelated/package.json", "{}");
        let first = detect(root).unwrap().remove(0);
        assert_eq!(first.members.len(), 2);
        let existing: Vec<_> = first
            .sources()
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
        let hash = crate::deps::state::input_hash(&existing, "plan").unwrap();
        write(
            root,
            "packages/a/package.json",
            r#"{"dependencies":{"is-number":"7.0.0"}}"#,
        );
        assert_ne!(
            hash,
            crate::deps::state::input_hash(&existing, "plan").unwrap()
        );
        write(root, "packages/c/package.json", "{}");
        assert_eq!(detect(root).unwrap()[0].members.len(), 3);
        std::fs::remove_dir_all(root.join("packages/b")).unwrap();
        assert_eq!(detect(root).unwrap()[0].members.len(), 2);
        write(
            root,
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers=['python/*']\nexclude=['python/excluded']",
        );
        write(
            root,
            "python/a/pyproject.toml",
            "[project]\nname='a'\nversion='0.1.0'",
        );
        write(root, "python/excluded/pyproject.toml", "invalid");
        assert_eq!(detect(root).unwrap()[1].members, [root.join("python/a")]);
    }
}
