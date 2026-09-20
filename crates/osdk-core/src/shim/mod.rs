//! Shim generation.
//!
//! A shim is a stand-in for a tool's executable, placed in the shims dir (which
//! the user puts on PATH). Invoking it dispatches to the active version via the
//! `osdk-shim` launcher.
//!
//! - Unix: a symlink from `shims/<name>` to the `osdk-shim` binary. The launcher
//!   inspects argv[0] to learn which tool to run.
//! - Windows: no symlink (privilege). We emit `shims/<name>.cmd` for
//!   cmd.exe/PowerShell and an extension-less **copy of the launcher binary**
//!   at `shims/<name>` for Git-Bash/MSYS, which resolves argv[0] the same way
//!   the Unix symlink does. That copy must never be a `#!/bin/sh` script: a
//!   shim named `sh` or `bash` would then need itself as its own interpreter
//!   and recurse until the machine dies (docs/bugs/005).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::backend::{Backend, Ctx};
use crate::config::ShimSettings;
use crate::dirs::{create_dir_all, Dirs};
use crate::error::{Error, Result};
use crate::inventory::{self, BinOwnerCandidate, DynamicToolManifest, ScanOptions, ScanReport};
use crate::npm_tools::{ToolScope, LOCKED_NPM_SCOPE_OPTION};
use crate::version::ToolVersion;
use crate::version::{ToolRequest, VersionSpec};

/// Executable names that should route through the shim for an installed
/// backend version. These are deliberately separate from backend ownership:
/// Node does not own npm/npx, but its bundled launchers still need routing
/// shims so Node-only activations cannot bypass package-registry preflight.
pub fn routed_bin_names(
    ctx: &Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
) -> Result<Vec<String>> {
    let mut names = backend
        .bin_names(ctx, version)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if backend.id() == "node" {
        for name in crate::backend::bin_names_in_dirs(&backend.bin_paths(ctx, version)?) {
            if matches!(name.as_str(), "npm" | "npx") {
                names.insert(name);
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// The curated owner for an executable name that several tools of one
/// ecosystem ship, or `None` when no rule applies.
///
/// Two Android SDK families legitimately ship the same R8 launchers: the
/// `build-tools` copy is the one a build invokes, while `cmdline-tools` bundles
/// them alongside `sdkmanager`. Without a rule this is an unresolvable
/// conflict, which would refuse every shim of whichever family was installed
/// second -- including `sdkmanager` and `avdmanager`, which nothing else
/// provides.
///
/// Shim generation and shim routing must agree, otherwise the generated shim
/// would dispatch to a different copy than the one it was written for, so both
/// sides call this function.
pub fn precedence_winner<'a>(name: &str, owner_ids: &'a BTreeSet<String>) -> Option<&'a str> {
    const ANDROID_R8_TOOLS: &[&str] = &["d8", "r8", "retrace", "resourceshrinker"];
    const ANDROID_R8_PRECEDENCE: &[&str] = &["android-build-tools", "android-cmdline-tools"];

    // This resolves contention, so a sole owner leaves nothing to decide.
    // Returning a winner there would also imply an opinion about a name no
    // other tool claims.
    if owner_ids.len() < 2 {
        return None;
    }
    if !ANDROID_R8_TOOLS.contains(&name) {
        return None;
    }
    // Only decide when every claimant is one of the known Android families;
    // an unexpected third owner is a real conflict the user must resolve.
    if !owner_ids
        .iter()
        .all(|owner_id| ANDROID_R8_PRECEDENCE.contains(&owner_id.as_str()))
    {
        return None;
    }
    ANDROID_R8_PRECEDENCE.iter().find_map(|preferred| {
        owner_ids
            .iter()
            .find(|owner_id| owner_id.as_str() == *preferred)
            .map(String::as_str)
    })
}

/// Whether `name`, exposed by `backend_id`, should get a shim.
///
/// This is the one place the decision is made, so generation and the
/// reconciliation pass that deletes unexpected shims cannot disagree and
/// fight each other. Excluding a name only withholds the shim: the tool is
/// still installed and still on PATH under an activated shell.
pub fn shim_is_enabled(settings: &ShimSettings, backend_id: &str, name: &str) -> bool {
    shim_is_enabled_for(settings, backend_id, name, true)
}

/// As [`shim_is_enabled`], but for a command whose install root is shared with
/// a dependency closure.
///
/// `owned` is only a *default*. A closure's command is withheld unless the user
/// names it -- via `expose` (additive, and the right tool for the job) or
/// `include` (an allowlist, which also withholds everything it does not name).
/// `exclude` is applied last either way, so it can still trim an owned command.
///
/// All three lists may be scoped to one tool through `shims.tools.<id>`, which is
/// what makes a narrow `include` safe: globally it silences every tool it omits.
pub fn shim_is_enabled_for(
    settings: &ShimSettings,
    backend_id: &str,
    name: &str,
    owned: bool,
) -> bool {
    let qualified = format!("{backend_id}:{name}");
    let matches_any = |patterns: &[String]| {
        patterns.iter().any(|pattern| {
            // A pattern naming a backend is matched against the qualified
            // form so one tool can be narrowed without listing its binaries.
            let subject = if pattern.contains(':') {
                qualified.as_str()
            } else {
                name
            };
            glob_matches(pattern, subject)
        })
    };

    // A tool's own lists replace the global ones for that tool, field by field.
    // Scoping matters for `include`: globally it is an allowlist over every
    // tool, so narrowing one SDK through it silences everything else. Per tool
    // that blast radius is gone -- an `include` under `conda:m2-base` cannot
    // withhold `cargo`.
    let overrides = settings.tools.get(backend_id);
    let include = overrides
        .and_then(|tool| tool.include.as_deref())
        .unwrap_or(&settings.include);
    let exclude = overrides
        .and_then(|tool| tool.exclude.as_deref())
        .unwrap_or(&settings.exclude);
    let expose = overrides
        .and_then(|tool| tool.expose.as_deref())
        .unwrap_or(&settings.expose);

    let included = matches_any(include);
    let exposed = matches_any(expose);

    // `expose` is additive, so it survives the allowlist: naming a command is
    // an instruction to shim it, and letting a narrow `include` elsewhere
    // cancel that would make the two settings fight over the same name.
    if !include.is_empty() && !included && !exposed {
        return false;
    }
    // A dependency's command needs to be asked for by name; being left in the
    // manifest is what makes asking possible. `expose` is the additive way to
    // ask -- `include` also works, but only at the cost of withholding
    // everything it does not list (docs/bugs/008).
    if !owned && !included && !exposed {
        return false;
    }
    // Exclude is applied last so a broad include or expose can be trimmed.
    !matches_any(exclude)
}

/// Case-insensitive glob over `*` and `?`.
fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let value: Vec<char> = value.to_lowercase().chars().collect();
    let (mut p, mut v) = (0usize, 0usize);
    // Position of the last `*` and the input it had consumed, so a failed
    // match can resume from there instead of recursing.
    let mut star: Option<(usize, usize)> = None;
    while v < value.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, v));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if let Some((star_p, star_v)) = star {
            p = star_p + 1;
            v = star_v + 1;
            star = Some((star_p, star_v + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|character| *character == '*')
}
/// Backends whose tools run on a JVM but ship no runtime of their own.
///
/// Android distributes `sdkmanager`, `avdmanager`, `d8` and friends as thin
/// launchers around bundled jars -- 125 of them in `cmdline-tools` alone -- and
/// includes no `java` binary, so they abort before doing anything unless an
/// external JDK is visible. Maven, Gradle and Kotlin are in the same position.
///
/// A backend cannot describe another backend's install, so the JDK has to be
/// supplied by whoever launches the tool.
pub fn requires_external_jdk(backend_id: &str) -> bool {
    matches!(
        backend_id,
        "maven" | "gradle" | "kotlin" | "android-build-tools" | "android-cmdline-tools"
    )
}

/// Whether an ambient `JAVA_HOME` is a deliberate user choice, and so must be
/// left alone, rather than osdk's own leftover output.
///
/// Both `osdk exec` and the shim fill in a JDK for backends that bundle none,
/// and both used to stand aside whenever `JAVA_HOME` was set at all. The comment
/// justifying that named the two sources it meant to respect -- "an activated
/// shell already exports JAVA_HOME, and a JAVA_HOME the user set themselves is a
/// deliberate choice" -- but the check could not tell them apart, and the two
/// want opposite handling. A value osdk exported is not an instruction; it is a
/// snapshot of whichever directory was current when the last prompt fired, so
/// honouring it lets a stale JDK outlive the project it was computed for.
///
/// Observed: with a shell activated where java resolves to 26, running
/// `osdk exec -t android-build-tools` inside a project pinning 21 launched the
/// tool against 26, silently ignoring that project's pin. Naming java in the
/// same command masked the bug, because the JDK then arrived through the
/// backend's own `exec_env` instead of this fallback.
///
/// `OSDK_MANAGED_ENV` is the discriminator, and it already exists: activation
/// writes into it the exact list of variables it manages, so `deactivate` knows
/// what to restore. A key listed there is osdk's own output and may be
/// recomputed; anything else is the user's and is preserved. When the variable
/// is absent -- a plain shell, CI, a hand-built `Command` -- there is no managed
/// environment to distrust, so an ambient value is the user's.
pub fn ambient_java_home_is_user_owned<F>(lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let Some(java_home) = lookup("JAVA_HOME") else {
        return false;
    };
    if java_home.trim().is_empty() {
        return false;
    }
    !lookup("OSDK_MANAGED_ENV")
        .unwrap_or_default()
        .split(',')
        .any(|key| key.trim() == "JAVA_HOME")
}

/// [`ambient_java_home_is_user_owned`] against the real process environment.
pub fn process_java_home_is_user_owned() -> bool {
    ambient_java_home_is_user_owned(|key| std::env::var(key).ok())
}

/// The JDK environment osdk would activate, for launching a tool whose own
/// backend cannot describe one.
///
/// Prefers the version selected for `cwd`, falling back to the newest install,
/// which is how package-manager shims already locate a managed Node. Returns
/// the JDK's own `exec_env` rather than rebuilding `JAVA_HOME`, so quirks such
/// as macOS' `Contents/Home` layout stay in one place.
pub fn managed_jdk_env(
    ctx: &Ctx,
    registry: &crate::backend::registry::Registry,
    cwd: &Path,
) -> Option<(BTreeMap<String, String>, Vec<std::path::PathBuf>)> {
    let java = registry.get("java").ok()?;
    let installed = java.list_installed(ctx).ok()?;
    if installed.is_empty() {
        return None;
    }
    let selected = crate::version::resolver::resolve_active(
        java.id(),
        cwd,
        &ctx.config.tools,
        java.idiomatic_files(),
    )
    .and_then(|active| installed_matching(&active.spec, active.is_range, &installed))
    .or_else(|| installed.last().cloned())?;
    let version = ToolVersion::new(java.id(), selected);
    let env = java.exec_env(ctx, &version).ok()?;
    let paths = java.bin_paths(ctx, &version).ok()?;
    Some((env, paths))
}

fn installed_matching(spec: &str, is_range: bool, installed: &[String]) -> Option<String> {
    // A java spec may carry a distribution prefix (`temurin-21`) that install
    // directories do not use.
    let spec = spec.split_once('-').map_or(spec, |(left, right)| {
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_alphabetic()) && !right.is_empty() {
            right
        } else {
            spec
        }
    });
    let parsed = if is_range {
        VersionSpec::parse_range(spec).ok()?
    } else {
        VersionSpec::parse(spec)
    };
    match &parsed {
        VersionSpec::Exact(exact) => installed.iter().find(|value| *value == exact).cloned(),
        _ => {
            let infos: Vec<_> = installed
                .iter()
                .map(crate::version::VersionInfo::stable)
                .collect();
            crate::version::select_version(&parsed, &infos).map(|info| info.version.clone())
        }
    }
}

/// Scan all persisted dynamic-tool manifests under the installs tree.
///
/// Fail-closed: use this where the result decides what gets executed.
pub fn scan_dynamic_installs(ctx: &Ctx) -> Result<ScanReport> {
    inventory::scan_installs(&ctx.dirs.installs, &ScanOptions::default())
}

/// Same scan, but skipping damaged manifests instead of refusing outright.
///
/// For commands that enumerate or reconcile the tree -- `list`, shim ownership,
/// `reshim`, `uninstall` -- where one broken install must not deny service to
/// every other tool, or block the command needed to remove it.
pub fn scan_dynamic_installs_tolerant(ctx: &Ctx) -> Result<ScanReport> {
    inventory::scan_installs(&ctx.dirs.installs, &ScanOptions::tolerant())
}

/// Dynamic backend ids referenced by the current config. Persisted install
/// manifests intentionally never own aliases or configuration keys.
pub fn configured_dynamic_ids(ctx: &Ctx, _report: &ScanReport) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(ctx.config.tools.iter().filter_map(|(key, value)| {
        inventory::canonical_dynamic_id(key).ok().or_else(|| {
            ToolRequest::parse(value)
                .ok()
                .filter(|request| request.backend.contains(':'))
                .map(|request| request.backend)
        })
    }));

    ids.into_iter().collect()
}

/// Dynamic backend ids relevant to lifecycle operations that survive restarts:
/// configured ids plus anything found on disk through inventory scanning.
pub fn configured_and_installed_dynamic_ids(ctx: &Ctx, report: &ScanReport) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(configured_dynamic_ids(ctx, report));
    ids.extend(report.installed_ids());
    ids.into_iter().collect()
}

/// Resolve a dynamic backend request from the merged config, supporting both a
/// direct dynamic backend key (`"npm:@scope/pkg" = "1.2.3"`) and an indirection
/// key whose value is the dynamic request (`tool.ni = "npm:@scope/pkg@1.2.3"`).
pub fn dynamic_request_from_config(ctx: &Ctx, backend_id: &str) -> Option<ToolRequest> {
    if !backend_id.contains(':') {
        return None;
    }
    for (key, value) in &ctx.config.tools {
        if inventory::canonical_dynamic_id(key).ok().as_deref() == Some(backend_id) {
            return Some(ToolRequest {
                backend: backend_id.to_string(),
                spec: VersionSpec::parse(value),
                options: ctx
                    .config
                    .tool_configs
                    .get(key)
                    .map(|entry| entry.to_request_options())
                    .unwrap_or_default(),
            });
        }
        if let Ok(mut request) = ToolRequest::parse(value) {
            if request.backend == backend_id {
                if let Some(entry) = ctx.config.tool_configs.get(key) {
                    request.options.extend(entry.to_request_options());
                }
                return Some(request);
            }
        }
    }
    None
}

/// One persisted dynamic install whose complete identity has been checked
/// against the active request. Callers keep this value through path and
/// executable selection so security-sensitive inventory data is not reloaded.
#[derive(Debug)]
pub struct ValidatedDynamicInstall {
    install_root: std::path::PathBuf,
    bins: BTreeMap<String, std::path::PathBuf>,
    /// Commands the requested package did not install itself.
    unowned: BTreeSet<String>,
    identity: crate::tool::InstallIdentity,
}

impl ValidatedDynamicInstall {
    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    pub fn bin_names(&self) -> Vec<String> {
        self.bins.keys().cloned().collect()
    }

    /// Whether the requested package installed `name` itself.
    ///
    /// Backends sharing a prefix with a dependency closure report false for the
    /// closure's commands, which shims withhold unless the user includes them.
    pub fn owns(&self, name: &str) -> bool {
        !self.unowned.contains(name)
    }

    pub fn bin_paths(&self) -> Vec<std::path::PathBuf> {
        self.bins
            .values()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn executable(&self, name: &str) -> Option<std::path::PathBuf> {
        self.bins.get(name).cloned()
    }

    pub fn identity(&self) -> &crate::tool::InstallIdentity {
        &self.identity
    }
}

/// Load the selected dynamic install and require it to prove the same backend,
/// exact version, and public option identity as the configured request. Legacy
/// `.osdk-tool.json` and missing inventories are discoverable elsewhere, but
/// cannot authorize PATH exposure or execution.
pub fn validated_dynamic_install(
    ctx: &Ctx,
    report: &ScanReport,
    request: &ToolRequest,
    version: &str,
) -> Result<ValidatedDynamicInstall> {
    let backend = crate::backend::registry::Registry::load(&ctx.dirs)?.get(&request.backend)?;
    let (identity, root) =
        selected_dynamic_install_from_report(ctx, report, backend.as_ref(), request, version)?;
    let locator = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())?;
    if !std::fs::symlink_metadata(root.join(".osdk-complete"))
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has no complete selected install; reinstall it before use",
            request.backend
        )));
    }
    let manifest_path = DynamicToolManifest::manifest_path(&root);
    let install = report
        .installs
        .iter()
        .find(|install| install.install_root == root)
        .ok_or_else(|| {
            Error::other(format!(
                "dynamic tool `{}@{version}` has missing or invalid install identity at {}; reinstall it before use",
                request.backend,
                manifest_path.display()
            ))
        })?;
    install.revalidate()?;
    let manifest = &install.manifest;
    if !manifest.matches_identity(&identity) || !locator.validates_install_root(&root) {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` was installed with a different identity; reinstall it before use",
            request.backend
        )));
    }
    let mut selected = ToolVersion::new(&request.backend, version);
    selected.options = request.options.clone();
    if !backend.validate_dynamic_install(ctx, &selected, &root, &manifest.identity)? {
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has invalid provider evidence; reinstall it before use",
            request.backend
        )));
    }
    let canonical_root = dunce::canonicalize(&root).map_err(|error| Error::io(&root, error))?;
    let mut bins = BTreeMap::new();
    let mut unowned = BTreeSet::new();
    for bin in &manifest.bins {
        let path = root.join(&bin.path);
        let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
        if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "dynamic tool `{}` inventory bin `{}` does not resolve to a file inside {}",
                request.backend,
                bin.name,
                root.display()
            )));
        }
        if !bin.owned {
            unowned.insert(bin.name.clone());
        }
        bins.insert(bin.name.clone(), canonical);
    }
    Ok(ValidatedDynamicInstall {
        install_root: root,
        bins,
        unowned,
        identity,
    })
}

/// Deterministic manifest-backed bin ownership used after a process restart,
/// even when the dynamic backend implementation itself does not expose
/// `bin_names` yet.
pub fn dynamic_bin_ownership(report: &ScanReport) -> BTreeMap<String, Vec<BinOwnerCandidate>> {
    inventory::build_bin_ownership_candidates(&report.installs)
}

/// Ownership as the shim layer sees it, honouring the shim settings.
///
/// Routing and reconciliation must apply the *same* predicate as generation.
/// Filtering on `owned` alone made them disagree whenever a user asked for an
/// unowned command: generation wrote the shim, reconciliation did not expect it
/// and removed it, so `expose` looked like it did nothing (docs/bugs/008).
pub fn dynamic_bin_ownership_with_settings(
    report: &ScanReport,
    settings: &ShimSettings,
) -> BTreeMap<String, Vec<BinOwnerCandidate>> {
    inventory::build_bin_ownership_candidates_with(&report.installs, &|id, name, owned| {
        shim_is_enabled_for(settings, id, name, owned)
    })
}

pub fn selected_dynamic_install_identity(
    ctx: &Ctx,
    backend: &dyn Backend,
    request: &ToolRequest,
    version: &str,
) -> Result<crate::tool::InstallIdentity> {
    let mut tv = ToolVersion::new(&request.backend, version);
    tv.options = request.options.clone();
    backend.dynamic_install_identity(ctx, &tv)?.ok_or_else(|| {
        Error::other(format!(
            "dynamic tool `{}@{version}` requires installed identity discovery",
            request.backend
        ))
    })
}

fn selected_dynamic_install_from_report(
    ctx: &Ctx,
    report: &ScanReport,
    backend: &dyn Backend,
    request: &ToolRequest,
    version: &str,
) -> Result<(crate::tool::InstallIdentity, std::path::PathBuf)> {
    let mut selected = ToolVersion::new(&request.backend, version);
    selected.options = request.options.clone();
    if backend.dynamic_install_identity(ctx, &selected)?.is_none() {
        let scope = crate::tool::InstallScope::Isolated;
        let expected_options = crate::tool::dynamic_identity_options(
            &crate::tool::ToolId::parse(&request.backend)?,
            &request.options,
        )?
        .into_map();
        let mut matching = report.installs.iter().filter(|install| {
            let identity = &install.manifest.identity;
            identity.tool == request.backend
                && identity.version == version
                && identity.platform == ctx.platform.to_string()
                && identity.scope == scope
                && identity.material_options == expected_options
                && std::fs::symlink_metadata(install.install_root.join(".osdk-complete"))
                    .is_ok_and(|metadata| metadata.file_type().is_file())
                && backend
                    .validate_dynamic_install(ctx, &selected, &install.install_root, identity)
                    .unwrap_or(false)
        });
        let first = matching.next();
        if matching.next().is_some() {
            return Err(Error::other(format!(
                "dynamic tool `{}@{version}` has multiple complete installs matching its unlocked request; use a lockfile with exact artifact identity or reinstall it",
                request.backend
            )));
        }
        if let Some(install) = first {
            return Ok((
                install.manifest.identity.clone(),
                install.install_root.clone(),
            ));
        }
        return Err(Error::other(format!(
            "dynamic tool `{}@{version}` has no complete install matching its unlocked request; reinstall it before use",
            request.backend
        )));
    }
    let identity = selected_dynamic_install_identity(ctx, backend, request, version)?;
    let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())?
        .install_root()
        .to_path_buf();
    Ok((identity, root))
}

pub(crate) fn configured_npm_scope(ctx: &Ctx, request: &ToolRequest) -> Result<Option<ToolScope>> {
    if let Some(scope) = request.options.get(LOCKED_NPM_SCOPE_OPTION) {
        return scope.parse().map(Some);
    }
    let mut project = false;
    let mut global = false;
    for (key, value) in &ctx.config.tools {
        let matches = key == &request.backend
            || ToolRequest::parse(value)
                .is_ok_and(|candidate| candidate.backend == request.backend);
        if !matches {
            continue;
        }
        match ctx.config.tool_origins.get(key) {
            Some(
                crate::config::ToolConfigOrigin::ProjectConfig(_)
                | crate::config::ToolConfigOrigin::ToolVersions(_),
            ) => project = true,
            Some(crate::config::ToolConfigOrigin::GlobalConfig(_)) => global = true,
            None if ctx.config.global_tool_configs.contains_key(key) => global = true,
            None => {}
        }
    }
    Ok(if project {
        Some(ToolScope::Project)
    } else if global {
        Some(ToolScope::Global)
    } else {
        None
    })
}

/// Generate a shim named `name` in the shims dir pointing at `osdk_shim_bin`.
pub fn generate_shim(dirs: &Dirs, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    let shims = dirs.shims();
    create_dir_all(&shims)?;
    generate_shim_in(&shims, name, osdk_shim_bin)
}

#[cfg(unix)]
fn generate_shim_in(shims: &Path, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    create_dir_all(shims)?;
    let link = shims.join(name);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(osdk_shim_bin, &link).map_err(|e| Error::io(&link, e))?;
    Ok(())
}

#[cfg(windows)]
fn generate_shim_in(shims: &Path, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    create_dir_all(shims)?;
    // .cmd wrapper for cmd.exe / PowerShell
    let cmd_path = shims.join(format!("{name}.cmd"));
    let cmd = windows_cmd_wrapper_bytes(osdk_shim_bin);
    std::fs::write(&cmd_path, cmd).map_err(|e| Error::io(&cmd_path, e))?;

    // Extension-less launcher for Git-Bash / MSYS.
    //
    // This MUST NOT be a `#!/bin/sh` script. A POSIX shell locates `/bin/sh`
    // through PATH, so a shim *named* `sh` or `bash` — which `conda:m2-bash`
    // and any other msys/cygwin package publishing a shell will produce —
    // becomes an interpreter whose own interpreter is itself:
    //
    //     make (SHELL := /bin/sh) -> shims/sh -> needs /bin/sh -> shims/sh -> ...
    //
    // Every level is a real msys process (emulated fork, POSIX signal and pty
    // layers), so this is exponential process growth, not bounded recursion:
    // it exhausts CPU, handles and non-paged pool within seconds and has been
    // observed to bugcheck the machine. See docs/bugs/005.
    //
    // A copy of the launcher binary avoids the whole class: msys executes a PE
    // image directly, and `osdk-shim` already derives the tool name from
    // argv[0] exactly as it does for the Unix symlink.
    let launcher_path = shims.join(name);
    write_windows_posix_launcher(&launcher_path, osdk_shim_bin)
}

/// Place an extension-less copy of the launcher binary at `launcher_path`.
///
/// Tries a hard link first so the shims dir does not carry one full copy of the
/// binary per tool, then falls back to a byte copy when linking is unavailable
/// (different volume, or a filesystem without hard-link support).
#[cfg(windows)]
fn write_windows_posix_launcher(launcher_path: &Path, osdk_shim_bin: &Path) -> Result<()> {
    let _ = std::fs::remove_file(launcher_path);
    match std::fs::hard_link(osdk_shim_bin, launcher_path) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(osdk_shim_bin, launcher_path)
            .map(|_| ())
            .map_err(|e| Error::io(launcher_path, e)),
    }
}

/// Serialize the Windows `.cmd` wrapper so cmd.exe can resolve the shim binary
/// regardless of its active code page.
///
/// cmd.exe decodes a batch file with its active console code page; when the
/// process has no console (a service, redirected CI output, `CreateNoWindow`),
/// that falls back to the system OEM code page. Writing the wrapper as UTF-8
/// therefore breaks as soon as the install path contains characters outside
/// that code page — for example a non-ASCII user name on a localized Windows.
/// Encode the quoted path in the OEM code page when it is fully representable
/// (the common case, with no console side effects); otherwise switch cmd to
/// UTF-8 with `chcp 65001` before the quoted line and keep UTF-8 bytes.
#[cfg(windows)]
fn windows_cmd_wrapper_bytes(osdk_shim_bin: &Path) -> Vec<u8> {
    let path = osdk_shim_bin.display().to_string();
    cmd_wrapper_bytes_for_oem(&path, encode_system_oem(&path))
}

#[cfg(windows)]
fn cmd_wrapper_bytes_for_oem(path: &str, oem: Option<Vec<u8>>) -> Vec<u8> {
    const HEADER: &[u8] = b"@echo off\r\n";
    const TAIL: &[u8] = b"\" %~n0 %*\r\n";
    // ASCII paths are identical in every OEM code page and need no chcp.
    if path.is_ascii() {
        let mut out = Vec::with_capacity(HEADER.len() + 1 + path.len() + TAIL.len());
        out.extend_from_slice(HEADER);
        out.push(b'"');
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(TAIL);
        return out;
    }
    match oem {
        // The whole path round-trips through the OEM code page: emit it encoded
        // that way so a default-codepage cmd.exe decodes it correctly.
        Some(oem_path) => {
            let mut out = Vec::with_capacity(HEADER.len() + 1 + oem_path.len() + TAIL.len());
            out.extend_from_slice(HEADER);
            out.push(b'"');
            out.extend_from_slice(&oem_path);
            out.extend_from_slice(TAIL);
            out
        }
        // Characters the OEM code page cannot express: switch cmd to UTF-8
        // before parsing the quoted path. `chcp` is ASCII and parses under any
        // code page; `>nul` suppresses its banner.
        None => {
            let mut out = Vec::new();
            out.extend_from_slice(b"@echo off\r\nchcp 65001>nul\r\n");
            out.push(b'"');
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(TAIL);
            out
        }
    }
}

/// Encode `text` in the system OEM code page (CP_OEMCP). Returns None when any
/// character cannot be represented, so the caller can fall back to UTF-8.
#[cfg(windows)]
fn encode_system_oem(text: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::Globalization::WideCharToMultiByte;
    const CP_OEMCP: u32 = 1;
    if text.is_ascii() {
        return Some(text.as_bytes().to_vec());
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let needed = WideCharToMultiByte(
            CP_OEMCP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if needed <= 0 {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        let mut used_default: BOOL = 0;
        let written = WideCharToMultiByte(
            CP_OEMCP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            std::ptr::null(),
            &mut used_default,
        );
        // used_default != 0 means at least one character was replaced by the
        // codepage default char, i.e. the path is lossy in this OEM codepage.
        if written <= 0 || written as usize > buffer.len() || used_default != 0 {
            return None;
        }
        buffer.truncate(written as usize);
        Some(buffer)
    }
}

/// Remove a shim by name (all its platform variants).
pub fn remove_shim(dirs: &Dirs, name: &str) -> Result<()> {
    let shims = dirs.shims();
    let _ = std::fs::remove_file(shims.join(name));
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(shims.join(format!("{name}.cmd")));
    }
    Ok(())
}

/// Remove a shim only when it has osdk's generated shape. This lets `reshim`
/// reconcile obsolete routing aliases without deleting unrelated files that a
/// user may have placed in the shims directory.
pub fn remove_managed_shim(dirs: &Dirs, name: &str) -> Result<bool> {
    let shims = dirs.shims();
    let removed = remove_managed_shim_path(&shims.join(name))?;
    #[cfg(windows)]
    {
        let cmd_removed = remove_managed_shim_path(&shims.join(format!("{name}.cmd")))?;
        Ok(removed || cmd_removed)
    }
    #[cfg(not(windows))]
    Ok(removed)
}

#[cfg(unix)]
fn remove_managed_shim_path(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(path, error)),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let target = std::fs::read_link(path).map_err(|error| Error::io(path, error))?;
    if target.file_name().and_then(|name| name.to_str()) != Some("osdk-shim") {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
    Ok(true)
}

#[cfg(windows)]
fn remove_managed_shim_path(path: &Path) -> Result<bool> {
    // Read bytes rather than a UTF-8 String: the .cmd wrapper encodes its
    // install path in the system OEM code page (or UTF-8 behind `chcp 65001`),
    // which is not always valid UTF-8. Only the ASCII template markers matter.
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(path, error)),
    };
    let lower: Vec<u8> = contents
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    let generated_cmd =
        lower.starts_with(b"@echo off\r\n") && contains_bytes(&lower, b"osdk-shim.exe\" %~n0 %*");
    // Extension-less launchers are copies (or hard links) of osdk-shim.exe.
    let generated_launcher = is_pe_image(&contents);
    // Recognize the pre-005 `#!/bin/sh` wrapper too, so `uninstall` / `reshim`
    // clean up shims written by an older osdk instead of leaving the recursive
    // `sh` / `bash` wrappers behind forever.
    let legacy_shell = lower.starts_with(b"#!/bin/sh\nexec \"")
        && contains_bytes(&lower, b"osdk-shim.exe\" \"$(basename \"$0\")\" \"$@\"");
    if !generated_cmd && !generated_launcher && !legacy_shell {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|error| Error::io(path, error))?;
    Ok(true)
}

/// Whether `contents` starts with an MZ/PE header, i.e. is a Windows binary
/// rather than a user-authored text file that happens to sit in the shims dir.
#[cfg(windows)]
fn is_pe_image(contents: &[u8]) -> bool {
    contents.starts_with(b"MZ")
}

#[cfg(windows)]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Locate the installed `osdk-shim` binary. It is expected to sit next to the
/// `osdk` binary (same dir). Falls back to the shims dir.
pub fn find_shim_binary(dirs: &Dirs) -> Option<std::path::PathBuf> {
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let name = format!("osdk-shim{exe_suffix}");
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let candidate = parent.join(&name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let candidate = dirs.data.join("bin").join(&name);
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_dependency_command_is_withheld_but_reachable_by_name() {
        // Regression: filtering the closure out of the install manifest made
        // the withheld commands unrecoverable, so `include` silently did
        // nothing. They must stay listed and be withheld here instead.
        let default = ShimSettings::default();
        assert!(
            super::shim_is_enabled_for(&default, "conda:clang", "clang", true),
            "the requested package's own command is published"
        );
        assert!(
            !super::shim_is_enabled_for(&default, "conda:clang", "xmllint", false),
            "a dependency's command is withheld by default"
        );

        let included = ShimSettings {
            include: vec!["conda:clang:xmllint".into()],
            exclude: Vec::new(),
            ..Default::default()
        };
        assert!(
            super::shim_is_enabled_for(&included, "conda:clang", "xmllint", false),
            "naming it explicitly must bring it back"
        );

        // A non-empty include still restricts everything else, ownership or not.
        assert!(!super::shim_is_enabled_for(
            &included,
            "conda:clang",
            "clang",
            true
        ));

        // Exclude is applied last and still wins over an explicit include.
        let both = ShimSettings {
            include: vec!["conda:clang:*".into()],
            exclude: vec!["conda:clang:xmllint".into()],
            ..Default::default()
        };
        assert!(!super::shim_is_enabled_for(
            &both,
            "conda:clang",
            "xmllint",
            false
        ));
        assert!(super::shim_is_enabled_for(
            &both,
            "conda:clang",
            "clang",
            true
        ));
    }

    #[test]
    fn ownership_does_not_change_behaviour_for_ordinary_backends() {
        // Every other backend passes owned = true, so the added parameter must
        // leave their behaviour identical.
        let settings = ShimSettings::default();
        for name in ["node", "d8", "aarch64-linux-android21-clang", "sdkmanager"] {
            assert_eq!(
                super::shim_is_enabled(&settings, "android-ndk", name),
                super::shim_is_enabled_for(&settings, "android-ndk", name, true),
                "{name}"
            );
        }
    }

    #[test]
    fn excluding_a_family_also_stops_it_winning_a_shared_name() {
        // Regression: filtering only generation left `d8` generated by
        // cmdline-tools but still executing the excluded build-tools copy,
        // which is the same generation-vs-routing split that misrouted the
        // R8 launchers before. Both sides consult this function, so the
        // excluded family must be invisible to routing too.
        let settings = ShimSettings {
            include: Vec::new(),
            exclude: vec!["android-build-tools:*".into()],
            ..Default::default()
        };
        assert!(!super::shim_is_enabled(
            &settings,
            "android-build-tools",
            "d8"
        ));
        assert!(super::shim_is_enabled(
            &settings,
            "android-cmdline-tools",
            "d8"
        ));
    }
    #[test]
    fn every_shim_is_enabled_by_default() {
        // The default must not change behaviour: an NDK legitimately exposes
        // 172 executables and hiding any of them would break API-level
        // compiler selection.
        let settings = ShimSettings::default();
        for name in ["node", "d8", "aarch64-linux-android21-clang", "sdkmanager"] {
            assert!(
                super::shim_is_enabled(&settings, "android-ndk", name),
                "{name}"
            );
        }
    }

    #[test]
    fn exclude_skips_names_and_include_restricts_to_them() {
        let exclude = ShimSettings {
            include: Vec::new(),
            exclude: vec!["*-clang".into(), "lint".into()],
            ..Default::default()
        };
        assert!(!super::shim_is_enabled(
            &exclude,
            "android-ndk",
            "aarch64-linux-android21-clang"
        ));
        assert!(!super::shim_is_enabled(
            &exclude,
            "android-cmdline-tools",
            "lint"
        ));
        assert!(super::shim_is_enabled(&exclude, "android-ndk", "clang"));
        assert!(super::shim_is_enabled(&exclude, "node", "node"));

        let include = ShimSettings {
            include: vec!["adb".into(), "sdk*".into()],
            exclude: Vec::new(),
            ..Default::default()
        };
        assert!(super::shim_is_enabled(
            &include,
            "android-platform-tools",
            "adb"
        ));
        assert!(super::shim_is_enabled(
            &include,
            "android-cmdline-tools",
            "sdkmanager"
        ));
        assert!(!super::shim_is_enabled(
            &include,
            "android-platform-tools",
            "fastboot"
        ));

        // Exclude is applied last, so it trims a broad include.
        let both = ShimSettings {
            include: vec!["*".into()],
            exclude: vec!["fastboot".into()],
            ..Default::default()
        };
        assert!(super::shim_is_enabled(
            &both,
            "android-platform-tools",
            "adb"
        ));
        assert!(!super::shim_is_enabled(
            &both,
            "android-platform-tools",
            "fastboot"
        ));
    }

    #[test]
    fn a_backend_qualified_pattern_narrows_only_that_tool() {
        // The point of qualifying: silence one noisy SDK without having to
        // enumerate its binaries, and without touching a same-named tool
        // from elsewhere.
        let settings = ShimSettings {
            include: Vec::new(),
            exclude: vec!["android-ndk:*".into()],
            ..Default::default()
        };
        assert!(!super::shim_is_enabled(&settings, "android-ndk", "clang"));
        assert!(!super::shim_is_enabled(
            &settings,
            "android-ndk",
            "llvm-strip"
        ));
        assert!(super::shim_is_enabled(
            &settings,
            "android-build-tools",
            "aapt2"
        ));
        assert!(super::shim_is_enabled(&settings, "node", "clang"));
    }

    #[test]
    fn ownership_honours_expose_so_reconciliation_agrees_with_generation() {
        // 008 的第二层，也是「配置读到了、published 也对了，但 shim 不落盘」的
        // 真因：归属候选曾经只按 `owned` 硬过滤，完全不看配置。于是生成侧写出
        // shim，回收侧不认它，随即删掉 —— expose 看起来毫无作用。
        //
        // 这条测试锁住的是「两侧用同一个判据」这个不变量，而不是某个具体名单。
        let settings = ShimSettings {
            expose: vec!["conda:m2-base:make".into()],
            ..Default::default()
        };
        let keep = |id: &str, name: &str, owned: bool| {
            super::shim_is_enabled_for(&settings, id, name, owned)
        };

        // owned=false 且被 expose 点名 —— 两侧都必须认。
        assert!(
            keep("conda:m2-base", "make", false),
            "被 expose 的命令必须能进入归属候选，否则回收会删掉刚生成的 shim"
        );
        // owned=false 且没被点名 —— 两侧都不认。
        assert!(!keep("conda:m2-base", "ls", false));
        // owned=true 的正常命令不受影响。
        assert!(keep("go", "go", true));
    }

    #[test]
    fn a_global_include_is_an_allowlist_over_every_tool() {
        // 008 的成因，作为既有行为固定下来：全局 include 是**全体**工具的
        // 白名单，不是「额外加回一个」。实测按 006 的告警只列了 13 个 conda
        // 命令，646 个 shim 直接归零，cargo / go 一并消失。
        //
        // 保留这个语义（它对「只要这几个」是正确的），但要有测试写明它的
        // 作用域，否则下一个人还会把它当成增量设置来用。
        let settings = ShimSettings {
            include: vec!["conda:m2-base:make".into()],
            ..Default::default()
        };
        assert!(super::shim_is_enabled_for(
            &settings,
            "conda:m2-base",
            "make",
            false
        ));
        assert!(
            !super::shim_is_enabled(&settings, "go", "go"),
            "全局 include 会牵连无关工具 —— 这正是 008，故这里断言现状而非期望"
        );
    }

    #[test]
    fn expose_adds_without_withholding_anything_else() {
        // 008 的修复：expose 是增量的，取回一个被 withheld 的命令时，
        // 不会顺手禁掉其他任何工具。
        let settings = ShimSettings {
            expose: vec!["conda:m2-base:make".into()],
            ..Default::default()
        };
        assert!(
            super::shim_is_enabled_for(&settings, "conda:m2-base", "make", false),
            "expose 必须能把元包里 owned=false 的命令取回来"
        );
        assert!(
            super::shim_is_enabled(&settings, "go", "go"),
            "expose 绝不能牵连无关工具"
        );
        assert!(
            super::shim_is_enabled(&settings, "cargo", "cargo"),
            "expose 绝不能牵连无关工具"
        );
    }

    #[test]
    fn a_per_tool_include_cannot_withhold_another_tool() {
        // per-tool 列表的核心价值：include 的白名单语义被限制在这个 tool 内，
        // 于是「收窄一个吵闹的 SDK」不再等于「禁掉全世界」。
        let mut tools = std::collections::BTreeMap::new();
        tools.insert(
            "conda:m2-base".to_string(),
            crate::config::ToolShimSettings {
                include: Some(vec!["make".into(), "sh".into()]),
                ..Default::default()
            },
        );
        let settings = ShimSettings {
            tools,
            ..Default::default()
        };
        // 该 tool 内：白名单生效。
        assert!(super::shim_is_enabled_for(
            &settings,
            "conda:m2-base",
            "make",
            false
        ));
        assert!(
            !super::shim_is_enabled_for(&settings, "conda:m2-base", "ls", true),
            "tool 内的 include 应当挡住未列出的命令"
        );
        // 其他 tool：完全不受影响 —— 与上面的全局 include 形成对照。
        assert!(
            super::shim_is_enabled(&settings, "go", "go"),
            "per-tool include 泄漏到了别的 tool"
        );
        assert!(
            super::shim_is_enabled(&settings, "cargo", "cargo"),
            "per-tool include 泄漏到了别的 tool"
        );
    }

    #[test]
    fn a_per_tool_list_overrides_the_global_one_field_by_field() {
        // 未指定的字段继承全局，指定的字段整体替换。混淆这两者会让
        // 「只调一个维度」意外清掉另一个维度。
        let mut tools = std::collections::BTreeMap::new();
        tools.insert(
            "conda:m2-base".to_string(),
            crate::config::ToolShimSettings {
                expose: Some(vec!["make".into()]),
                ..Default::default()
            },
        );
        let settings = ShimSettings {
            exclude: vec!["ls".into()],
            tools,
            ..Default::default()
        };
        // expose 被该 tool 覆盖。
        assert!(super::shim_is_enabled_for(
            &settings,
            "conda:m2-base",
            "make",
            false
        ));
        // exclude 未被覆盖，继承全局 —— 对这个 tool 同样生效。
        assert!(
            !super::shim_is_enabled_for(&settings, "conda:m2-base", "ls", true),
            "未指定的字段应当继承全局列表"
        );
    }

    #[test]
    fn exclude_still_wins_over_expose() {
        // expose 是增量，但不是特权：仍要能用 exclude 修剪，否则
        // 「放开一批再排除个别」这个常见组合就没法表达。
        let settings = ShimSettings {
            expose: vec!["conda:m2-base:*".into()],
            exclude: vec!["conda:m2-base:ls".into()],
            ..Default::default()
        };
        assert!(super::shim_is_enabled_for(
            &settings,
            "conda:m2-base",
            "make",
            false
        ));
        assert!(!super::shim_is_enabled_for(
            &settings,
            "conda:m2-base",
            "ls",
            false
        ));
    }

    #[test]
    fn expose_survives_a_narrow_global_include() {
        // 两者都命中同一个名字时，expose 说「要」，include 说「不在名单里」。
        // 让 include 赢就等于「显式要求的东西被静默丢弃」，所以 expose 优先。
        let settings = ShimSettings {
            include: vec!["go".into()],
            expose: vec!["conda:m2-base:make".into()],
            ..Default::default()
        };
        assert!(
            super::shim_is_enabled_for(&settings, "conda:m2-base", "make", false),
            "被 expose 点名的命令不应被别处的窄 include 取消"
        );
        assert!(super::shim_is_enabled(&settings, "go", "go"));
    }

    #[test]
    fn patterns_are_case_insensitive_and_match_whole_names() {
        let settings = ShimSettings {
            include: Vec::new(),
            exclude: vec!["ADB".into(), "d?".into()],
            ..Default::default()
        };
        // Windows executables are case-insensitive, so the config must be too.
        assert!(!super::shim_is_enabled(
            &settings,
            "android-platform-tools",
            "adb"
        ));
        assert!(!super::shim_is_enabled(
            &settings,
            "android-build-tools",
            "d8"
        ));
        // A pattern is anchored: `d?` must not swallow `dexdump`.
        assert!(super::shim_is_enabled(
            &settings,
            "android-build-tools",
            "dexdump"
        ));
        // Nor may a bare name match a longer one.
        assert!(super::shim_is_enabled(
            &settings,
            "android-platform-tools",
            "adbx"
        ));
    }

    #[test]
    fn glob_backtracks_correctly_on_repeated_literals() {
        // A single-pass matcher gets these wrong without resuming from the
        // last star, so they are worth pinning down directly.
        assert!(super::glob_matches("*b", "abab"));
        assert!(super::glob_matches("*ab", "aaab"));
        assert!(super::glob_matches("a*b*c", "axxbyyc"));
        assert!(!super::glob_matches("*ab", "aba"));
        assert!(!super::glob_matches("a*b", "ab_"));
        assert!(super::glob_matches("*", ""));
        assert!(super::glob_matches("**", "anything"));
        assert!(!super::glob_matches("?", ""));
        assert!(super::glob_matches("", ""));
        assert!(!super::glob_matches("", "x"));
    }
    #[test]
    fn jvm_tools_without_a_bundled_runtime_are_flagged() {
        // Android ships these as launchers around jars with no `java` binary,
        // and the JVM build tools are in the same position.
        for id in [
            "android-build-tools",
            "android-cmdline-tools",
            "maven",
            "gradle",
            "kotlin",
        ] {
            assert!(super::requires_external_jdk(id), "{id}");
        }
        // Tools that carry their own runtime, or need none, must not be
        // handed someone else's JDK.
        for id in [
            "java",
            "node",
            "python",
            "android-ndk",
            "android-platform-tools",
        ] {
            assert!(!super::requires_external_jdk(id), "{id}");
        }
    }

    #[test]
    fn only_a_java_home_osdk_did_not_export_counts_as_the_users_own() {
        // Nothing set: there is no choice to respect, so the caller fills it in.
        assert!(!super::ambient_java_home_is_user_owned(|_| None));

        // Set, with no managed-environment marker at all: a plain shell, CI, or
        // a hand-built Command. Nothing here says osdk produced it, so it is
        // the user's and must survive.
        assert!(super::ambient_java_home_is_user_owned(|key| (key
            == "JAVA_HOME")
            .then(|| "/opt/user-jdk".to_string())));

        // Set, and named in OSDK_MANAGED_ENV: this is osdk's own output from a
        // previous prompt, describing whichever directory was current then. It
        // is a stale snapshot rather than an instruction, so the caller must
        // recompute it -- this is the case the defect got wrong, and honouring
        // it let a JDK from one project leak into another.
        assert!(!super::ambient_java_home_is_user_owned(|key| match key {
            "JAVA_HOME" => Some("/managed/jdk-26".to_string()),
            "OSDK_MANAGED_ENV" => Some("GOROOT,JAVA_HOME,CARGO_HOME".to_string()),
            _ => None,
        }));

        // Managed, but managing something else: a user-set JAVA_HOME inside an
        // activated shell is still the user's. Matching on substring rather
        // than on a whole entry would get this wrong.
        assert!(super::ambient_java_home_is_user_owned(|key| match key {
            "JAVA_HOME" => Some("/opt/user-jdk".to_string()),
            "OSDK_MANAGED_ENV" => Some("GOROOT,CARGO_HOME".to_string()),
            _ => None,
        }));

        // A key that merely contains the name is not the name.
        assert!(super::ambient_java_home_is_user_owned(|key| match key {
            "JAVA_HOME" => Some("/opt/user-jdk".to_string()),
            "OSDK_MANAGED_ENV" => Some("JAVA_HOME_OVERRIDE,MY_JAVA_HOME".to_string()),
            _ => None,
        }));

        // An empty value cannot launch anything, so it is not a usable choice.
        assert!(!super::ambient_java_home_is_user_owned(|key| (key
            == "JAVA_HOME")
            .then(String::new)));
    }

    #[test]
    fn managed_jdk_prefers_the_version_selected_for_the_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let registry = crate::backend::registry::Registry::new();
        let java = registry.get("java").unwrap();

        // With nothing installed there is nothing to offer.
        let mut ctx = jdk_test_ctx(temporary.path(), &[]);
        assert!(super::managed_jdk_env(&ctx, &registry, &project).is_none());

        // Two installs present: the newest is the fallback.
        for version in ["17.0.1+9", "21.0.2+13"] {
            let home = ctx.dirs.install_path(java.id(), version);
            std::fs::create_dir_all(home.join("bin")).unwrap();
            std::fs::write(home.join(".osdk-complete"), b"").unwrap();
        }
        let (env, paths) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some(
                ctx.dirs
                    .install_path(java.id(), "21.0.2+13")
                    .to_str()
                    .unwrap()
            )
        );
        assert!(!paths.is_empty());

        // An explicit selection wins over the newest install, so a project
        // pinned to an older JDK builds against that one.
        ctx.config.tools.insert("java".into(), "17.0.1+9".into());
        let (env, _) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some(
                ctx.dirs
                    .install_path(java.id(), "17.0.1+9")
                    .to_str()
                    .unwrap()
            )
        );

        // A distribution-prefixed spec addresses the same install.
        ctx.config
            .tools
            .insert("java".into(), "temurin-17.0.1+9".into());
        let (env, _) = super::managed_jdk_env(&ctx, &registry, &project).unwrap();
        assert!(env
            .get("JAVA_HOME")
            .is_some_and(|home| home.ends_with("17.0.1+9")));
    }

    fn jdk_test_ctx(root: &std::path::Path, tools: &[(&str, &str)]) -> Ctx {
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
                tools: tools
                    .iter()
                    .map(|(tool, version)| (tool.to_string(), version.to_string()))
                    .collect(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }
    // Generation and routing must agree on the owner of a shared launcher.
    // If they diverge, the shim dispatches to a copy other than the one it
    // was generated for, which is invisible until a build misbehaves.
    #[test]
    fn shared_android_r8_launchers_resolve_to_build_tools() {
        let both = super::BTreeSet::from([
            "android-cmdline-tools".to_string(),
            "android-build-tools".to_string(),
        ]);
        for name in ["d8", "r8", "retrace", "resourceshrinker"] {
            assert_eq!(
                super::precedence_winner(name, &both),
                Some("android-build-tools"),
                "{name}"
            );
        }
    }

    #[test]
    fn precedence_declines_single_owners_and_unknown_claimants() {
        // Sole owner: nothing to decide, so ordinary handling applies.
        let only_cmdline = super::BTreeSet::from(["android-cmdline-tools".to_string()]);
        assert_eq!(super::precedence_winner("d8", &only_cmdline), None);
        // Names outside the curated set stay real conflicts.
        let both = super::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
        ]);
        assert_eq!(super::precedence_winner("aapt2", &both), None);
        // An unexpected third claimant must not be silently overridden.
        let with_outsider = super::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
            "npm:d8-lookalike".to_string(),
        ]);
        assert_eq!(super::precedence_winner("d8", &with_outsider), None);
    }
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::backend::npm_package::NpmPackageBackend;
    use crate::config::{Config, Settings, SourcesConfig, ToolConfigEntry, ToolConfigOrigin};
    use crate::inventory::DynamicToolBin;
    use crate::platform::Platform;
    use crate::store::Cas;
    use crate::tool::{InstallDependency, InstallDependencyKind, InstallScope};

    fn npm_scope_test_ctx(
        root: &Path,
        origin: Option<ToolConfigOrigin>,
    ) -> (Ctx, NpmPackageBackend) {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        let key = "npm:fixture-cli".to_string();
        let entry = ToolConfigEntry::structured(
            "1.2.3",
            BTreeMap::from([(
                crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
                crate::config::ToolConfigValue::String("1.0.0".into()),
            )]),
        );
        let mut tool_origins = BTreeMap::new();
        let mut global_tool_configs = BTreeMap::new();
        if let Some(origin) = origin {
            if matches!(origin, ToolConfigOrigin::GlobalConfig(_)) {
                global_tool_configs.insert(key.clone(), entry.clone());
            }
            tool_origins.insert(key.clone(), origin);
        }
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: BTreeMap::from([(key.clone(), "1.2.3".into())]),
                tool_configs: BTreeMap::from([(key, entry)]),
                global_tools: Default::default(),
                global_tool_configs,
                tool_origins,
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        (ctx, backend)
    }

    fn dynamic_fixture(
        ctx: &Ctx,
        backend: &NpmPackageBackend,
        scope: ToolScope,
        options: BTreeMap<String, String>,
        bin_name: &str,
    ) -> (PathBuf, DynamicToolManifest) {
        let mut options = options;
        options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let identity = crate::tool::InstallIdentity::new(
            backend.id(),
            "1.2.3",
            ctx.platform.to_string(),
            match scope {
                ToolScope::Project => InstallScope::Isolated,
                ToolScope::Global => InstallScope::Global,
            },
            &options,
            vec![InstallDependency {
                kind: InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "1.0.0".into(),
                identity: None,
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let mut manifest = DynamicToolManifest::from_identity(identity.clone()).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
            ..Default::default()
        }];
        (root, manifest)
    }

    fn write_dynamic_fixture(
        ctx: &Ctx,
        backend: &NpmPackageBackend,
        scope: ToolScope,
        bin_name: &str,
    ) -> PathBuf {
        let (root, manifest) = dynamic_fixture(ctx, backend, scope, BTreeMap::new(), bin_name);
        let bin = root.join(format!("bin/{bin_name}"));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    #[test]
    fn npm_manifest_lookup_is_scope_strict_and_compatibility_falls_back() {
        let temporary = tempfile::tempdir().unwrap();
        let project_config = temporary.path().join("project/osdk.toml");
        let (project_ctx, backend) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::ProjectConfig(project_config)),
        );
        let global = write_dynamic_fixture(&project_ctx, &backend, ToolScope::Global, "global-bin");
        let isolated =
            write_dynamic_fixture(&project_ctx, &backend, ToolScope::Project, "isolated-bin");
        let global_config = temporary.path().join("config/config.toml");
        let (global_ctx, global_backend) = npm_scope_test_ctx(
            temporary.path(),
            Some(ToolConfigOrigin::GlobalConfig(global_config)),
        );
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        assert_eq!(
            global_backend
                .global_install_root_for(&global_ctx, &version)
                .unwrap(),
            global
        );
        assert_eq!(
            backend
                .isolated_install_root_for(&project_ctx, &version)
                .unwrap(),
            isolated
        );
    }

    #[test]
    fn indirect_dynamic_request_preserves_structured_options() {
        use crate::config::ToolConfigValue;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(td.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(td.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let options = BTreeMap::from([(
            "allow_builds".into(),
            ToolConfigValue::Array(vec!["esbuild".into()]),
        )]);
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: BTreeMap::from([("ni".into(), "npm:@antfu/ni@0.21.12".into())]),
                tool_configs: BTreeMap::from([(
                    "ni".into(),
                    ToolConfigEntry::structured("npm:@antfu/ni@0.21.12", options),
                )]),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };

        let request = dynamic_request_from_config(&ctx, "npm:@antfu/ni").unwrap();
        assert_eq!(request.spec.to_string(), "0.21.12");
        assert_eq!(request.options["allow_builds"], "esbuild");
    }

    #[test]
    fn validated_dynamic_install_requires_schema_one_matching_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let root = backend.isolated_install_root_for(&ctx, &version).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture-cli"), b"fixture").unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();

        let report = scan_dynamic_installs(&ctx).unwrap();
        let missing = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("missing or invalid install identity"),
            "{missing}"
        );

        std::fs::write(
            root.join(crate::inventory::LEGACY_INVENTORY_FILE),
            r#"{"schema":2,"id":"npm:fixture-cli","version":"1.2.3","bins":[{"name":"fixture-cli","path":"bin/fixture-cli"}]}"#,
        )
        .unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let legacy = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            legacy
                .to_string()
                .contains("missing or invalid install identity"),
            "{legacy}"
        );
        assert_eq!(report.legacy_installs.len(), 1);

        let (mismatch_root, mismatched) = dynamic_fixture(
            &ctx,
            &backend,
            ToolScope::Project,
            BTreeMap::from([("installer".into(), "pnpm".into())]),
            "fixture-cli",
        );
        std::fs::create_dir_all(mismatch_root.join("bin")).unwrap();
        std::fs::write(mismatch_root.join("bin/fixture-cli"), b"fixture").unwrap();
        mismatched.write_atomic(&mismatch_root).unwrap();
        std::fs::write(mismatch_root.join(".osdk-complete"), b"").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let mismatch = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            mismatch
                .to_string()
                .contains("missing or invalid install identity"),
            "{mismatch}"
        );
    }

    #[test]
    fn validated_dynamic_install_requires_completion_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let root = write_dynamic_fixture(&ctx, &backend, ToolScope::Project, "fixture-cli");
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        std::fs::remove_file(root.join(".osdk-complete")).unwrap();
        let incomplete = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(incomplete
            .to_string()
            .contains("no complete selected install"));
        assert!(incomplete.to_string().contains("reinstall"));

        std::fs::create_dir(root.join(".osdk-complete")).unwrap();
        let invalid = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(invalid.to_string().contains("no complete selected install"));
        assert!(invalid.to_string().contains("reinstall"));
    }

    #[cfg(unix)]
    #[test]
    fn validated_dynamic_install_rejects_symlink_completion_marker() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let (ctx, backend) = npm_scope_test_ctx(temporary.path(), None);
        let root = write_dynamic_fixture(&ctx, &backend, ToolScope::Project, "fixture-cli");
        let request = dynamic_request_from_config(&ctx, backend.id()).unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        std::fs::remove_file(root.join(".osdk-complete")).unwrap();
        std::fs::write(root.join("real-complete"), b"").unwrap();
        symlink("real-complete", root.join(".osdk-complete")).unwrap();

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("no complete selected install"));
    }

    fn write_github_fixture(
        ctx: &Ctx,
        request_options: &BTreeMap<String, String>,
        materials: BTreeMap<String, String>,
        contents: &[u8],
    ) -> PathBuf {
        let identity = crate::tool::InstallIdentity::new(
            "github:example/tool",
            "1.2.3",
            ctx.platform.to_string(),
            InstallScope::Isolated,
            request_options,
            Vec::new(),
            materials,
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/tool"), contents).unwrap();
        let mut manifest = DynamicToolManifest::from_identity(identity.clone()).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
            ..Default::default()
        }];
        manifest.write_atomic(&root).unwrap();
        let artifact_file = identity.materials["artifact-file"].clone();
        let checksum = identity
            .materials
            .get("artifact-checksum")
            .map(|value| format!(r#","checksum":{value:?}"#))
            .unwrap_or_default();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            format!(
                r#"{{"url":"https://example.test/tool.tar.gz","file_name":{artifact_file:?}{checksum},"evidence":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    fn write_http_fixture(ctx: &Ctx, receipt_url: &str) -> (ToolRequest, PathBuf) {
        let backend = "http:https://downloads.example.test/tool-{version}";
        let digest = "a".repeat(64);
        let options = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "file".into()),
            ("rename".into(), "fixture".into()),
        ]);
        let identity = crate::tool::InstallIdentity::new(
            backend,
            "1.2.3",
            ctx.platform.to_string(),
            InstallScope::Isolated,
            &options,
            Vec::new(),
            BTreeMap::from([
                ("artifact-file".into(), "tool-1.2.3".into()),
                ("artifact-checksum".into(), format!("sha256:{digest}")),
                ("artifact-url-blake3".into(), {
                    let mut hasher = blake3::Hasher::new_derive_key("osdk-http-artifact-url-v1");
                    hasher.update(receipt_url.as_bytes());
                    hasher.finalize().to_hex().to_string()
                }),
            ]),
        )
        .unwrap();
        let root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture"), b"fixture").unwrap();
        let mut manifest = DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![DynamicToolBin {
            name: "fixture".into(),
            path: "bin/fixture".into(),
            ..Default::default()
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&crate::pipeline::ArtifactReceipt {
                url: receipt_url.into(),
                file_name: "tool-1.2.3".into(),
                checksum: Some(format!("sha256:{digest}")),
                evidence: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        (
            ToolRequest {
                backend: backend.into(),
                spec: VersionSpec::Exact("1.2.3".into()),
                options,
            },
            root,
        )
    }

    #[test]
    fn http_restart_selection_requires_matching_receipt_url_fingerprint() {
        let temporary = tempfile::tempdir().unwrap();
        let (ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        let receipt_url = "https://downloads.example.test/tool-1.2.3";
        let (request, root) = write_http_fixture(&ctx, receipt_url);
        let report = scan_dynamic_installs(&ctx).unwrap();
        let selected = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap();
        assert_eq!(selected.install_root(), root);
        assert_eq!(selected.bin_names(), vec!["fixture"]);

        let mut receipt = crate::pipeline::artifact_receipt_at(&root).unwrap();
        receipt.url = "https://downloads.example.test/substitute-1.2.3".into();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();
        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("receipt"), "{error}");
    }

    #[test]
    fn unlocked_github_request_recovers_one_complete_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        let materials = BTreeMap::from([
            ("artifact-file".into(), "tool.tar.gz".into()),
            (
                "artifact-checksum".into(),
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
        ]);
        let root = write_github_fixture(&ctx, &BTreeMap::new(), materials, b"one");
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let selected = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap();
        assert_eq!(selected.install_root(), root);
        assert_eq!(
            std::fs::read(selected.executable("tool").unwrap()).unwrap(),
            b"one"
        );
    }

    #[test]
    fn validated_dynamic_install_rejects_manifest_replacement_before_using_cached_bins() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        let materials = BTreeMap::from([
            ("artifact-file".into(), "tool.tar.gz".into()),
            (
                "artifact-checksum".into(),
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
        ]);
        let root = write_github_fixture(&ctx, &BTreeMap::new(), materials, b"one");
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let mut replacement = DynamicToolManifest::load(&root).unwrap();
        replacement.bins.clear();
        replacement.write_atomic(&root).unwrap();
        assert_eq!(report.installs[0].manifest.bins[0].name, "tool");
        assert!(DynamicToolManifest::load(&root).unwrap().bins.is_empty());

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(
            error.to_string().contains("changed after inventory scan"),
            "{error}"
        );
    }

    #[test]
    fn unlocked_github_request_rejects_ambiguous_complete_identities() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("github:example/tool".into(), "1.2.3".into())]);
        ctx.config.tool_configs.clear();
        for (file, digest) in [
            (
                "tool-a.tar.gz",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                "tool-b.tar.gz",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ] {
            write_github_fixture(
                &ctx,
                &BTreeMap::new(),
                BTreeMap::from([
                    ("artifact-file".into(), file.into()),
                    ("artifact-checksum".into(), digest.into()),
                ]),
                file.as_bytes(),
            );
        }
        let request = dynamic_request_from_config(&ctx, "github:example/tool").unwrap();
        let report = scan_dynamic_installs(&ctx).unwrap();

        let error = validated_dynamic_install(&ctx, &report, &request, "1.2.3").unwrap_err();
        assert!(error.to_string().contains("multiple complete installs"));
        assert!(error.to_string().contains("lockfile"));
    }

    #[test]
    fn configured_dynamic_ids_include_indirect_requests_without_inventory() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        ctx.config.tools = BTreeMap::from([("tool.ni".into(), "npm:@antfu/ni@1.2.3".into())]);
        ctx.config.tool_configs.clear();

        assert_eq!(
            configured_dynamic_ids(&ctx, &ScanReport::default()),
            vec!["npm:@antfu/ni"]
        );
    }

    #[test]
    fn configured_dynamic_ids_and_requests_support_http_templates() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut ctx, _) = npm_scope_test_ctx(temporary.path(), None);
        let backend = "http:https://downloads.example.test/tool-{version}";
        let digest = "a".repeat(64);
        ctx.config.tools = BTreeMap::from([(backend.into(), "1.2.3".into())]);
        ctx.config.tool_configs = BTreeMap::from([(
            backend.into(),
            ToolConfigEntry::structured(
                "1.2.3",
                BTreeMap::from([
                    (
                        "sha256".into(),
                        crate::config::ToolConfigValue::String(digest),
                    ),
                    (
                        "kind".into(),
                        crate::config::ToolConfigValue::String("file".into()),
                    ),
                    (
                        "rename".into(),
                        crate::config::ToolConfigValue::String("fixture".into()),
                    ),
                ]),
            ),
        )]);

        assert_eq!(
            configured_dynamic_ids(&ctx, &ScanReport::default()),
            vec![backend]
        );
        let request = dynamic_request_from_config(&ctx, backend).unwrap();
        assert_eq!(request.backend, backend);
        assert_eq!(request.spec, VersionSpec::Exact("1.2.3".into()));
        assert_eq!(request.options["rename"], "fixture");
    }

    #[cfg(unix)]
    #[test]
    fn unix_shim_is_symlink() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let shims = td.path().join("shims");
        let fake_bin = td.path().join("osdk-shim");
        std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();
        generate_shim_in(&shims, "node", &fake_bin).unwrap();
        let link = shims.join("node");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), fake_bin);
    }

    #[cfg(unix)]
    #[test]
    fn managed_shim_cleanup_removes_generated_links_but_preserves_regular_files() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            _ => None,
        })
        .unwrap();
        let fake_bin = td.path().join("bin/osdk-shim");
        std::fs::create_dir_all(fake_bin.parent().unwrap()).unwrap();
        std::fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();

        generate_shim(&dirs, "npm", &fake_bin).unwrap();
        assert!(remove_managed_shim(&dirs, "npm").unwrap());
        assert!(!dirs.shims().join("npm").exists());

        std::fs::write(dirs.shims().join("npx"), b"user-owned").unwrap();
        assert!(!remove_managed_shim(&dirs, "npx").unwrap());
        assert_eq!(
            std::fs::read(dirs.shims().join("npx")).unwrap(),
            b"user-owned"
        );
    }

    #[cfg(windows)]
    #[test]
    fn ascii_cmd_wrapper_needs_no_codepage_switch() {
        use super::*;
        let bytes = cmd_wrapper_bytes_for_oem(
            r"C:\tools\osdk-shim.exe",
            Some(br"C:\tools\osdk-shim.exe".to_vec()),
        );
        assert_eq!(
            bytes,
            b"@echo off\r\n\"C:\\tools\\osdk-shim.exe\" %~n0 %*\r\n"
        );
        assert!(!bytes.windows(4).any(|window| window == b"chcp"));
    }

    /// The extension-less launcher must never be a script whose interpreter is
    /// resolved through PATH. A shim named `sh` or `bash` would otherwise need
    /// itself to interpret itself and recurse until the machine dies; msys
    /// process creation makes that growth exponential rather than bounded.
    ///
    /// Asserting on `sh` and `bash` specifically is the point: every other tool
    /// name tolerated the old `#!/bin/sh` wrapper, which is exactly why the
    /// defect survived. See docs/bugs/005.
    #[cfg(windows)]
    #[test]
    fn windows_posix_launcher_is_never_a_shell_script() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let shims = td.path().join("shims");
        let fake_bin = td.path().join("osdk-shim.exe");
        // An MZ header stands in for the real launcher: generation must copy
        // these bytes through rather than author a script around the path.
        std::fs::write(&fake_bin, b"MZ\x90\x00fake-launcher").unwrap();

        for name in ["sh", "bash", "node"] {
            generate_shim_in(&shims, name, &fake_bin).unwrap();
            let launcher = shims.join(name);
            let contents = std::fs::read(&launcher).unwrap();
            assert!(
                !contents.starts_with(b"#!"),
                "`{name}` launcher is a script with a shebang; a shell shim would recurse"
            );
            assert!(
                contents.starts_with(b"MZ"),
                "`{name}` launcher is not a PE image, so msys cannot exec it directly"
            );
            assert_eq!(
                contents,
                std::fs::read(&fake_bin).unwrap(),
                "`{name}` launcher must be a faithful copy of osdk-shim.exe"
            );
        }
    }

    /// `uninstall` / `reshim` must still recognize the pre-005 `#!/bin/sh`
    /// wrapper, otherwise machines upgraded from an older osdk keep the
    /// recursive `sh` / `bash` shims forever. User-authored files stay put.
    #[cfg(windows)]
    #[test]
    fn cleanup_removes_legacy_shell_wrappers_but_preserves_user_files() {
        use super::*;

        let td = tempfile::tempdir().unwrap();
        let legacy = td.path().join("sh");
        std::fs::write(
            &legacy,
            b"#!/bin/sh\nexec \"E:/osdk-bin/osdk-shim.exe\" \"$(basename \"$0\")\" \"$@\"\n",
        )
        .unwrap();
        assert!(remove_managed_shim_path(&legacy).unwrap());
        assert!(!legacy.exists());

        let user_owned = td.path().join("my-script");
        std::fs::write(&user_owned, b"#!/bin/sh\necho mine\n").unwrap();
        assert!(!remove_managed_shim_path(&user_owned).unwrap());
        assert!(user_owned.exists());
    }

    #[cfg(windows)]
    #[test]
    fn oem_representable_path_is_encoded_without_chcp() {
        use super::*;
        // Injected OEM bytes stand in for a localized code page encoding of a
        // non-ASCII path: they must be embedded verbatim with no chcp line.
        let oem = vec![0x80u8, 0x81];
        let bytes = cmd_wrapper_bytes_for_oem("中文\\osdk-shim.exe", Some(oem.clone()));
        let mut expected = b"@echo off\r\n\"".to_vec();
        expected.extend_from_slice(&oem);
        expected.extend_from_slice(b"\" %~n0 %*\r\n");
        assert_eq!(bytes, expected);
        assert!(!bytes.windows(4).any(|window| window == b"chcp"));
    }

    #[cfg(windows)]
    #[test]
    fn non_representable_path_falls_back_to_utf8_and_chcp() {
        use super::*;
        let path = "中文\\osdk-shim.exe";
        let bytes = cmd_wrapper_bytes_for_oem(path, None);
        let mut expected = b"@echo off\r\nchcp 65001>nul\r\n\"".to_vec();
        expected.extend_from_slice(path.as_bytes());
        expected.extend_from_slice(b"\" %~n0 %*\r\n");
        assert_eq!(bytes, expected);
    }

    #[cfg(windows)]
    #[test]
    fn oem_encoding_is_lossless_for_ascii() {
        use super::*;
        assert_eq!(
            encode_system_oem(r"C:\x\osdk-shim.exe").unwrap(),
            br"C:\x\osdk-shim.exe"
        );
    }

    #[cfg(windows)]
    #[test]
    fn generated_cmd_wrapper_roundtrips_cleanup_with_a_non_ascii_dir() {
        use super::*;
        let td = tempfile::tempdir().unwrap();
        // A non-ASCII program directory, as a localized install path would be.
        let program = td.path().join("程序");
        std::fs::create_dir_all(&program).unwrap();
        let shim_exe = program.join("osdk-shim.exe");
        std::fs::write(&shim_exe, b"bin").unwrap();
        let shims = td.path().join("shims");
        generate_shim_in(&shims, "node", &shim_exe).unwrap();
        let wrapper = shims.join("node.cmd");
        let raw = std::fs::read(&wrapper).unwrap();
        assert!(raw.starts_with(b"@echo off\r\n"));
        let tail = b"osdk-shim.exe\" %~n0 %*";
        assert!(raw.windows(tail.len()).any(|window| window == tail));
        // Cleanup must recognize the OEM/UTF-8 wrapper as osdk-managed even
        // though its path bytes are not UTF-8 (or sit behind `chcp 65001`).
        assert!(remove_managed_shim_path(&wrapper).unwrap());
        assert!(!wrapper.exists());
    }

    #[cfg(unix)]
    #[test]
    fn node_routing_names_include_bundled_npm_without_changing_backend_ownership() {
        use std::sync::Arc;

        use super::*;
        use crate::backend::node::NodeBackend;
        use crate::config::{Config, Settings, SourcesConfig};
        use crate::platform::Platform;
        use crate::store::Cas;

        let td = tempfile::tempdir().unwrap();
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(td.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(td.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(td.path().join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(td.path().join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(td.path().join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let ctx = Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                // Gated exactly like the field: `tasks` is behind the `install`
                // feature so the shim's build never carries task parsing.
                #[cfg(feature = "install")]
                tasks: Default::default(),
            },
            client: reqwest::Client::new(),
            show_progress: false,
        };
        let version = ToolVersion::new("node", "20.0.0");
        let bin = NodeBackend.bin_paths(&ctx, &version).unwrap().remove(0);
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["node", "npm", "npx"] {
            let path = bin.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let owned = NodeBackend.bin_names(&ctx, &version).unwrap();
        assert!(owned.contains(&"node".to_string()));
        assert!(!owned.contains(&"npm".to_string()));
        assert!(!owned.contains(&"npx".to_string()));

        let routed = routed_bin_names(&ctx, &NodeBackend, &version).unwrap();
        assert!(routed.contains(&"npm".to_string()));
        assert!(routed.contains(&"npx".to_string()));
    }
}
