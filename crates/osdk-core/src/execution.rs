//! Host executables are execution targets, never managed installations.
//! Discovery performs no subprocesses. Only explicit verification executes a
//! bounded version probe. `system` is opt-in and is never appended by defaults.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::{Backend, Ctx};
use crate::config::{ToolConflictPolicy, ToolSource};
use crate::error::{Error, Result};
use crate::process::{
    CaptureLimits, CommandOutcome, CommandRunner, CommandSpec, SystemCommandRunner,
};
use crate::version::resolver::{ActiveVersion, SelectionEvidence};
use crate::version::{VersionInfo, VersionSpec};

#[derive(Debug, Clone)]
pub enum ExecutionTarget {
    Managed(ActiveVersion),
    System(SystemTarget),
}

#[derive(Debug, Clone)]
pub struct SystemTarget {
    pub tool: String,
    /// Absolute invocation path, retaining native symlink/launcher layout.
    pub executable: PathBuf,
    pub origin: String,
    pub version: Option<String>,
    pub env: BTreeMap<String, String>,
    canonical: PathBuf,
    identity: std::sync::Arc<same_file::Handle>,
}

pub struct Selection {
    pub target: Option<ExecutionTarget>,
    pub evidence: Vec<SelectionEvidence>,
}

pub fn enabled(ctx: &Ctx) -> bool {
    ctx.config
        .settings
        .precedence
        .tool_order
        .contains(&ToolSource::System)
}

/// Explicitly audited executable identities. Locator variables are a separate
/// allowlist: global install/cache directories are not runtime homes.
fn adapter(tool: &str) -> Option<(&'static [&'static str], Option<&'static str>)> {
    Some(match tool {
        "node" => (&["node"], None),
        "python" => (&["python3", "python"], Some("VIRTUAL_ENV")),
        "go" => (&["go"], Some("GOROOT")),
        "java" => (&["java"], Some("JAVA_HOME")),
        "npm" => (&["npm"], None),
        "pnpm" => (&["pnpm"], None),
        "yarn" => (&["yarn"], None),
        "bun" => (&["bun"], None),
        "deno" => (&["deno"], None),
        "zig" => (&["zig"], None),
        _ => return None,
    })
}

/// An osdk activation's locator is output, not an ambient system choice.
fn original_env(key: &str) -> Option<OsString> {
    if std::env::var("OSDK_MANAGED_ENV")
        .unwrap_or_default()
        .split(',')
        .any(|k| k == key)
    {
        return (std::env::var(format!("OSDK_ORIG_{key}_PRESENT"))
            .ok()
            .as_deref()
            == Some("1"))
        .then(|| std::env::var_os(format!("OSDK_ORIG_{key}")))
        .flatten();
    }
    std::env::var_os(key)
}

pub fn ambient_path() -> OsString {
    if std::env::var_os("OSDK_ORIGINAL_PATH_SET").is_some() {
        std::env::var_os("OSDK_ORIGINAL_PATH").unwrap_or_default()
    } else {
        std::env::var_os("PATH").unwrap_or_default()
    }
}

fn excluded(ctx: &Ctx, path: &Path) -> bool {
    let Ok(canonical) = dunce::canonicalize(path) else {
        return true;
    };
    if [&ctx.dirs.data, &ctx.dirs.installs, &ctx.dirs.store]
        .iter()
        .any(|root| dunce::canonicalize(root).is_ok_and(|root| canonical.starts_with(root)))
    {
        return true;
    }
    let identities = [
        std::env::current_exe().ok(),
        crate::shim::find_shim_binary(&ctx.dirs),
        Some(
            ctx.dirs
                .data
                .join("bin")
                .join(format!("osdk-shim{}", std::env::consts::EXE_SUFFIX)),
        ),
    ];
    if identities
        .into_iter()
        .flatten()
        .any(|p| same_file::is_same_file(path, &p).unwrap_or(false) || same_contents(path, &p))
    {
        return true;
    }
    // Generated wrappers can be copied outside their original directory.
    if let Ok(file) = std::fs::File::open(path) {
        let mut bytes = Vec::new();
        let _ = file.take(4096).read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        if text.contains("osdk-shim") || text.contains("OSDK_SHIM_ACTIVE") {
            return true;
        }
    }
    false
}

fn same_contents(a: &Path, b: &Path) -> bool {
    if !matches!((a.metadata(), b.metadata()), (Ok(a), Ok(b)) if a.len() == b.len()) {
        return false;
    }
    fn hash(path: &Path) -> Option<blake3::Hash> {
        let mut file = std::fs::File::open(path).ok()?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 8192];
        loop {
            let n = file.read(&mut buffer).ok()?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        Some(hash.finalize())
    }
    matches!((hash(a), hash(b)), (Some(a), Some(b)) if a == b)
}

fn executable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    #[cfg(windows)]
    {
        // Arbitrary cmd/bat/PowerShell wrappers are not audited system targets.
        if !path
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
        {
            return false;
        }
    }
    true
}

fn find_in(ctx: &Ctx, dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| {
        let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        (executable(&path) && !excluded(ctx, &path)).then_some(path)
    })
}

fn discover(ctx: &Ctx, tool: &str, cwd: &Path) -> Result<Option<SystemTarget>> {
    let Some((names, home)) = adapter(tool) else {
        return Ok(None);
    };
    let mut env = BTreeMap::new();
    let mut chosen = None;
    if let Some(key) = home {
        if let Some(value) = original_env(key).filter(|v| !v.is_empty()) {
            let root = PathBuf::from(value);
            if !root.is_absolute() {
                return Err(Error::config(format!(
                    "system {tool}: {key} must be an absolute directory"
                )));
            }
            let subdir = if cfg!(windows) && tool == "python" {
                "Scripts"
            } else {
                "bin"
            };
            let path = find_in(ctx, &root.join(subdir), names).ok_or_else(|| {
                Error::config(format!(
                    "system {tool}: {key} does not contain an eligible external executable"
                ))
            })?;
            env.insert(
                key.to_string(),
                root.to_str()
                    .ok_or_else(|| Error::config("system tool home must be valid Unicode"))?
                    .to_owned(),
            );
            chosen = Some((path, key.to_string()));
        }
    }
    if chosen.is_none() {
        for directory in std::env::split_paths(&ambient_path()) {
            // Empty/relative entries imply the working directory. Never execute
            // project-local candidates merely because '.' appeared on PATH.
            if !directory.is_absolute() {
                continue;
            }
            if let Some(path) = find_in(ctx, &directory, names) {
                chosen = Some((path, "PATH".into()));
                break;
            }
        }
    }
    let _ = cwd;
    chosen
        .map(|(mut executable, origin)| {
            let canonical =
                dunce::canonicalize(&executable).map_err(|e| Error::io(&executable, e))?;
            if tool == "java" {
                let root = canonical
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| Error::config("system Java has no JDK home"))?;
                if find_in(ctx, &root.join("bin"), &["javac"]).is_none() {
                    return Err(Error::config("system Java requires a JDK with javac"));
                }
                if env.get("JAVA_HOME").is_some_and(|declared| {
                    dunce::canonicalize(declared).ok().as_deref() != Some(root)
                }) {
                    return Err(Error::config(
                        "JAVA_HOME and its java executable identify different JDK homes",
                    ));
                }
                // Alternatives symlinks for java and javac may point at
                // different JDKs. Bind all companions to java's actual home.
                executable = canonical.clone();
                env.insert(
                    "JAVA_HOME".into(),
                    root.to_str()
                        .ok_or_else(|| Error::config("system tool home must be valid Unicode"))?
                        .to_owned(),
                );
            }
            let identity = std::sync::Arc::new(
                same_file::Handle::from_path(&executable).map_err(|e| Error::io(&executable, e))?,
            );
            Ok(SystemTarget {
                tool: tool.into(),
                executable,
                origin,
                version: None,
                env,
                canonical,
                identity,
            })
        })
        .transpose()
}

impl SystemTarget {
    pub fn bin_dir(&self) -> &Path {
        self.executable.parent().expect("absolute executable")
    }

    /// Restore only this adapter's locator; unrelated native/cache policy stays
    /// intact. Callers launch with env_clear + this complete environment so a
    /// stale osdk locator can actually be removed, not merely left unset here.
    pub fn environment(
        &self,
        overrides: &BTreeMap<String, String>,
    ) -> BTreeMap<OsString, OsString> {
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        self.restore_environment(&mut env);
        env.extend(
            overrides
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        env.extend(
            self.env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        env
    }

    pub fn restore_environment(&self, env: &mut BTreeMap<OsString, OsString>) {
        // These existing forced locations are environment outputs, not extra
        // selection adapters. Restore the user's value (or absence) before
        // launching an external target; keep cache override policy unchanged.
        let keys = crate::persistent_env::forced_var_names(&self.tool)
            .iter()
            .copied()
            .chain(adapter(&self.tool).and_then(|(_, home)| home));
        for key in keys {
            env.remove(std::ffi::OsStr::new(key));
            if let Some(value) = original_env(key) {
                env.insert(key.into(), value);
            }
        }
        env.extend(
            self.env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
    }

    pub fn command_names(&self) -> &[&str] {
        match self.tool.as_str() {
            "python" => &["python", "python3", "pip", "pip3"],
            "node" => &["node", "npm", "npx"],
            "npm" => &["npm", "npx"],
            "pnpm" => &["pnpm", "pnpx"],
            "yarn" => &["yarn", "yarnpkg"],
            "go" => &["go", "gofmt"],
            "java" => &["java", "javac", "jar", "javadoc", "jshell"],
            "bun" => &["bun", "bunx"],
            "deno" => &["deno"],
            "zig" => &["zig"],
            _ => &[],
        }
    }

    pub fn published_names(&self, ctx: &Ctx) -> Vec<String> {
        self.command_names()
            .iter()
            .filter(|name| !(self.tool == "node" && **name != "node"))
            .filter(|name| self.command(ctx, name).is_ok())
            .map(|name| name.to_string())
            .collect()
    }

    pub fn command(&self, ctx: &Ctx, name: &str) -> Result<PathBuf> {
        if !self.command_names().contains(&name) {
            return Err(Error::config("system target does not own this command"));
        }
        let primary = name == self.tool || (self.tool == "python" && name == "python3");
        let path = if primary {
            Some(self.executable.clone())
        } else {
            find_in(ctx, self.bin_dir(), &[name])
        };
        let path = path.ok_or_else(|| {
            Error::config(format!("system {} has no companion {name}", self.tool))
        })?;
        if !executable(&path)
            || excluded(ctx, &path)
            || (primary
                && (dunce::canonicalize(&path).ok().as_ref() != Some(&self.canonical)
                    || !same_file::Handle::from_path(&path)
                        .is_ok_and(|handle| handle == *self.identity)))
        {
            return Err(Error::config(
                "system executable changed or is no longer eligible",
            ));
        }
        Ok(path)
    }

    pub fn verify(&mut self, ctx: &Ctx, env: &BTreeMap<String, String>) -> Result<()> {
        let path = self.command(ctx, &self.tool)?;
        let flag = if self.tool == "java" {
            "-version"
        } else if self.tool == "go" {
            "version"
        } else {
            "--version"
        };
        let spec = CommandSpec::new(path)
            .arg(flag)
            .clear_env()
            .envs(self.environment(env));
        let outcome = SystemCommandRunner.run_captured(
            &spec,
            CaptureLimits::new(Duration::from_secs(3), 8192, 8192),
        );
        let CommandOutcome::Exited { status, output } = outcome else {
            return Err(Error::config(format!(
                "system {} version probe failed",
                self.tool
            )));
        };
        if !status.success() || output.stdout_truncated || output.stderr_truncated {
            return Err(Error::config(format!(
                "system {} version probe failed",
                self.tool
            )));
        }
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.version = Some(observed_version(&self.tool, &text).ok_or_else(|| {
            Error::config(format!(
                "system {} version identity is unrecognized",
                self.tool
            ))
        })?);
        if let Some(home) = self.env.get("VIRTUAL_ENV") {
            let spec = CommandSpec::new(&self.executable)
                .args(["-c", "import sys; print(sys.prefix)"])
                .clear_env()
                .envs(self.environment(env));
            match SystemCommandRunner.run_captured(
                &spec,
                CaptureLimits::new(Duration::from_secs(3), 8192, 8192),
            ) {
                CommandOutcome::Exited { status, output }
                    if status.success()
                        && !output.stdout_truncated
                        && matches!((dunce::canonicalize(String::from_utf8_lossy(&output.stdout).trim()), dunce::canonicalize(home)), (Ok(actual), Ok(expected)) if actual == expected) =>
                    {}
                _ => {
                    return Err(Error::config(
                        "VIRTUAL_ENV does not match the selected Python prefix",
                    ))
                }
            }
        }
        Ok(())
    }
}

// Native banners have small, explicit grammars. Avoid linking a general regex
// engine into the standalone shim merely to parse one bounded version line.
fn observed_version(tool: &str, text: &str) -> Option<String> {
    for line in text.lines().map(str::trim) {
        let raw = match tool {
            "node" => line.strip_prefix('v'),
            "python" => line.strip_prefix("Python "),
            "go" => line.strip_prefix("go version go").and_then(|s| {
                s.split_once(char::is_whitespace)
                    .map(|(version, _)| version)
            }),
            "java" => line
                .strip_prefix("openjdk version \"")
                .or_else(|| line.strip_prefix("java version \""))
                .and_then(|s| s.split_once('"').map(|(version, _)| version)),
            "deno" => line.strip_prefix("deno ").and_then(|s| {
                s.split_once(char::is_whitespace)
                    .map(|(version, _)| version)
            }),
            _ => Some(line),
        };
        let Some(raw) = raw else { continue };
        if matches!(tool, "python" | "go" | "java") {
            let parts: Vec<_> = raw.split('.').collect();
            if parts
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
                || (tool == "python" && parts.len() != 3)
                || (tool == "go" && !(2..=3).contains(&parts.len()))
            {
                continue;
            }
            let mut version = raw.to_owned();
            while version.matches('.').count() < 2 {
                version.push_str(".0");
            }
            return Some(version);
        }
        if semver::Version::parse(raw).is_ok() {
            return Some(raw.to_owned());
        }
    }
    None
}

pub fn path_with(ctx: &Ctx, directories: impl IntoIterator<Item = PathBuf>) -> Result<String> {
    let mut paths: Vec<_> = directories.into_iter().collect();
    paths.extend(std::env::split_paths(&ambient_path()).filter(|p| {
        p.is_absolute()
            && ![&ctx.dirs.data, &ctx.dirs.installs, &ctx.dirs.store]
                .iter()
                .any(
                    |root| match (dunce::canonicalize(p), dunce::canonicalize(root)) {
                        (Ok(p), Ok(root)) => p.starts_with(root),
                        _ => p.starts_with(root),
                    },
                )
    }));
    std::env::join_paths(paths)
        .map_err(|_| Error::config("invalid system PATH"))?
        .into_string()
        .map_err(|_| Error::config("system PATH must be valid Unicode"))
}

/// Resolve an execution-only runtime dependency without installing anything.
/// Managed dynamic package receipts continue using their recorded runtime path.
pub type RuntimeEnvironment = (Vec<PathBuf>, BTreeMap<String, String>);

pub fn runtime_environment(
    ctx: &Ctx,
    registry: &crate::backend::registry::Registry,
    cwd: &Path,
    tool: &str,
) -> Result<RuntimeEnvironment> {
    let backend = registry.get(tool)?;
    match resolve(ctx, backend.as_ref(), cwd)?.target {
        Some(ExecutionTarget::System(mut target)) => {
            let mut env = BTreeMap::new();
            env.insert(
                "PATH".into(),
                path_with(ctx, [target.bin_dir().to_path_buf()])?,
            );
            target.verify(ctx, &env)?;
            Ok((vec![target.bin_dir().to_path_buf()], target.env))
        }
        Some(ExecutionTarget::Managed(active)) => {
            let text = ctx.config.expand_alias(tool, &active.spec)?;
            let spec = if active.is_range {
                VersionSpec::parse_range(&text)?
            } else {
                VersionSpec::parse(&text)
            };
            let versions: Vec<_> = backend
                .list_installed(ctx)?
                .iter()
                .map(VersionInfo::stable)
                .collect();
            let version = crate::version::select_version(&spec, &versions).ok_or_else(|| {
                Error::config(format!("selected {tool} runtime is not installed"))
            })?;
            let version = crate::version::ToolVersion::new(tool, &version.version);
            Ok((
                backend.bin_paths(ctx, &version)?,
                backend.exec_env(ctx, &version)?,
            ))
        }
        None => Err(Error::config(format!(
            "no {tool} runtime selected for system execution"
        ))),
    }
}

pub fn jdk_environment(
    ctx: &Ctx,
    registry: &crate::backend::registry::Registry,
    cwd: &Path,
) -> Result<Option<RuntimeEnvironment>> {
    if enabled(ctx) {
        let java = registry.get("java")?;
        if resolve(ctx, java.as_ref(), cwd)?.target.is_some() {
            return runtime_environment(ctx, registry, cwd, "java").map(Some);
        }
    }
    Ok(crate::shim::managed_jdk_env(ctx, registry, cwd).map(|(env, paths)| (paths, env)))
}

pub fn resolve(ctx: &Ctx, backend: &dyn Backend, cwd: &Path) -> Result<Selection> {
    resolve_inner(ctx, backend, cwd, true)
}

/// Ownership reconciliation must not run external executables.
pub fn resolve_without_probes(ctx: &Ctx, backend: &dyn Backend, cwd: &Path) -> Result<Selection> {
    resolve_inner(ctx, backend, cwd, false)
}

fn resolve_inner(ctx: &Ctx, backend: &dyn Backend, cwd: &Path, probes: bool) -> Result<Selection> {
    let declarations = crate::version::resolver::resolve_selection(
        backend.id(),
        cwd,
        &ctx.config,
        backend.idiomatic_files(),
    )?;
    let mut result = Selection {
        target: declarations.winner.map(ExecutionTarget::Managed),
        evidence: declarations.evidence,
    };
    if !enabled(ctx) || adapter(backend.id()).is_none() {
        return Ok(result);
    }
    let order = ctx.config.settings.precedence.ordered_tool_sources();
    let system_rank = order
        .iter()
        .position(|k| *k == ToolSource::System)
        .expect("enabled");
    let managed_rank = result
        .evidence
        .iter()
        .find(|e| e.status == "winner")
        .and_then(|e| order.iter().position(|k| *k == e.category));
    let wins = managed_rank.is_none_or(|rank| system_rank < rank);
    let strict =
        probes && ctx.config.settings.precedence.tool_conflicts == ToolConflictPolicy::Error;
    if !wins && !strict {
        result.evidence.push(SelectionEvidence {
            tool: backend.id().into(),
            path: None,
            field: "external executable".into(),
            category: ToolSource::System,
            status: "ignored".into(),
            reason: "system discovery skipped: higher priority managed declaration".into(),
        });
        return Ok(result);
    }
    let Some(mut system) = discover(ctx, backend.id(), cwd)? else {
        result.evidence.push(SelectionEvidence {
            tool: backend.id().into(),
            path: None,
            field: "external executable".into(),
            category: ToolSource::System,
            status: "ignored".into(),
            reason: "no eligible system executable".into(),
        });
        return Ok(result);
    };
    if strict {
        let mut probe_env = BTreeMap::new();
        let mut bins = vec![system.bin_dir().to_path_buf()];
        if matches!(backend.id(), "npm" | "pnpm" | "yarn") {
            let registry = crate::backend::registry::Registry::new();
            let (runtime_bins, runtime_env) = runtime_environment(ctx, &registry, cwd, "node")?;
            bins.splice(0..0, runtime_bins);
            probe_env.extend(runtime_env);
        }
        probe_env.insert("PATH".into(), path_with(ctx, bins)?);
        system.verify(ctx, &probe_env)?;
        for declaration in crate::version::resolver::selection::candidates(
            backend.id(),
            cwd,
            &ctx.config,
            backend.idiomatic_files(),
        )?
        .into_iter()
        .filter(|c| !c.ignored && !c.shadowed_layer)
        {
            let spec = ctx
                .config
                .expand_alias(backend.id(), &declaration.active.spec)?;
            let parsed = if declaration.active.is_range {
                VersionSpec::parse_range(&spec)?
            } else {
                VersionSpec::parse(&spec)
            };
            if matches!(
                parsed,
                VersionSpec::Latest | VersionSpec::System | VersionSpec::Lts(_)
            ) || matches!(&parsed, VersionSpec::Exact(s) | VersionSpec::Pinned(s) | VersionSpec::Prefix(s)
                if !s.as_bytes().first().is_some_and(u8::is_ascii_digit))
            {
                return Err(Error::config(
                    "unverifiable system version compatibility with an opaque selector",
                ));
            }
            let versions = [VersionInfo::stable(
                system.version.as_ref().expect("verified"),
            )];
            if crate::version::select_version(&parsed, &versions).is_none() {
                return Err(Error::config(format!(
                    "incompatible system version for {}",
                    backend.id()
                )));
            }
        }
    }
    if wins {
        for e in &mut result.evidence {
            if e.status == "winner" {
                e.status = "shadowed".into();
                e.reason = "lower source priority than system".into();
            }
        }
    }
    result.evidence.push(SelectionEvidence {
        tool: backend.id().into(),
        path: Some(system.executable.clone()),
        field: system.origin.clone(),
        category: ToolSource::System,
        status: if wins { "winner" } else { "shadowed" }.into(),
        reason: if system.version.is_some() {
            "verified external executable"
        } else {
            "external executable; version not probed"
        }
        .into(),
    });
    if wins {
        result.target = Some(ExecutionTarget::System(system));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::observed_version;

    #[test]
    fn native_banners_preserve_identity_and_reject_unsupported_formats() {
        for (tool, banner, expected) in [
            ("node", "v22.3.0\n", Some("22.3.0")),
            ("node", "Python 22.3.0", None),
            ("node", "v22.3.0 trailing", None),
            ("python", "Python 3.13.5", Some("3.13.5")),
            ("python", "Python 3.14.0rc1", None),
            ("go", "go version go1.24 linux/amd64", Some("1.24.0")),
            (
                "java",
                "openjdk version \"21.0.2\" 2024-01-16",
                Some("21.0.2"),
            ),
            ("java", "java version \"1.8.0_432\"", None),
            ("java", "openjdk version \"21..2\"", None),
            ("deno", "deno 2.2.3 (stable, release)", Some("2.2.3")),
            ("pnpm", "10.2.0", Some("10.2.0")),
            ("pnpm", "pnpm unknown", None),
        ] {
            assert_eq!(
                observed_version(tool, banner).as_deref(),
                expected,
                "{tool}: {banner}"
            );
        }
    }
}
