//! Shell activation.
//!
//! Two mechanisms (both offered, like mise):
//! - Shims (default): the shims dir on PATH; robust in IDEs/CI. Set up by
//!   `osdk` itself when tools are installed.
//! - Shell activation (`osdk activate <shell>`): injects a hook that runs
//!   `osdk hook-env` on each prompt / dir change, putting a project's installed
//!   commands before global shims and active versions' real bin dirs, and
//!   exporting their env (GOROOT/JAVA_HOME/...).
//!
//! This module renders the per-shell snippets and computes the env delta.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::backend::registry::Registry;
use crate::backend::Ctx;
use crate::version::resolver::resolve_active;
use crate::version::{select_version, ToolVersion, VersionInfo, VersionSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

impl std::str::FromStr for Shell {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> crate::Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "bash" => Shell::Bash,
            "zsh" => Shell::Zsh,
            "fish" => Shell::Fish,
            "powershell" | "pwsh" => Shell::Powershell,
            other => {
                return Err(crate::error::Error::other(format!(
                    "unsupported shell `{other}` (expected bash|zsh|fish|powershell)"
                )))
            }
        })
    }
}

/// Render the shell-integration snippet for `osdk activate <shell>`. The snippet
/// wires a prompt/dir-change hook that evals `osdk hook-env`.
pub fn activation_script(shell: Shell, osdk_bin: &str) -> String {
    match shell {
        Shell::Bash => format!(
            r#"# osdk shell integration (bash)
_osdk_hook() {{
  local out
  out="$({bin} hook-env --shell bash 2>/dev/null)" && eval "$out"
}}
if [[ ";${{PROMPT_COMMAND:-}};" != *";_osdk_hook;"* ]]; then
  PROMPT_COMMAND="_osdk_hook${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}"
fi
_osdk_hook
"#,
            bin = shell_quote(Shell::Bash, osdk_bin)
        ),
        Shell::Zsh => format!(
            r#"# osdk shell integration (zsh)
_osdk_hook() {{
  local out
  out="$({bin} hook-env --shell zsh 2>/dev/null)" && eval "$out"
}}
typeset -ag precmd_functions
if [[ -z ${{precmd_functions[(r)_osdk_hook]}} ]]; then
  precmd_functions+=(_osdk_hook)
fi
_osdk_hook
"#,
            bin = shell_quote(Shell::Zsh, osdk_bin)
        ),
        Shell::Fish => format!(
            r#"# osdk shell integration (fish)
function _osdk_hook --on-variable PWD --on-event fish_prompt
  {bin} hook-env --shell fish 2>/dev/null | source
end
_osdk_hook
"#,
            bin = shell_quote(Shell::Fish, osdk_bin)
        ),
        Shell::Powershell => format!(
            r#"# osdk shell integration (powershell)
$script:OsdkHookRunning = $false
function Invoke-OsdkHook {{
  $out = & {bin} hook-env --shell powershell 2>$null
  if ($out) {{ Invoke-Expression ($out -join "`n") }}
}}
Invoke-OsdkHook
$ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = {{
  if ($script:OsdkHookRunning) {{ return }}
  $script:OsdkHookRunning = $true
  try {{ Invoke-OsdkHook }} finally {{ $script:OsdkHookRunning = $false }}
}}
"#,
            bin = powershell_quote(osdk_bin)
        ),
    }
}

/// Render shell code that removes osdk's hook and restores the environment
/// captured by [`render_hook_env`].
pub fn deactivation_script(shell: Shell) -> String {
    match shell {
        Shell::Bash => r#"# osdk shell deactivation (bash)
if [ -n "${OSDK_MANAGED_ENV:-}" ]; then
  _osdk_saved_ifs=$IFS
  IFS=,
  for _osdk_key in $OSDK_MANAGED_ENV; do
    case "$_osdk_key" in
      ''|*[!A-Za-z0-9_]*) continue ;;
    esac
    eval "_osdk_present=\${OSDK_ORIG_${_osdk_key}_PRESENT:-0}"
    if [ "$_osdk_present" = 1 ]; then
      eval "export ${_osdk_key}=\${OSDK_ORIG_${_osdk_key}}"
    else
      unset "$_osdk_key"
    fi
    unset "OSDK_ORIG_${_osdk_key}" "OSDK_ORIG_${_osdk_key}_PRESENT" "OSDK_ORIG_${_osdk_key}_SET"
  done
  IFS=$_osdk_saved_ifs
  unset _osdk_saved_ifs _osdk_key _osdk_present
fi
if [ -n "${OSDK_ORIGINAL_PATH_SET+x}" ]; then export PATH="$OSDK_ORIGINAL_PATH"; fi
PROMPT_COMMAND=";${PROMPT_COMMAND:-};"
PROMPT_COMMAND="${PROMPT_COMMAND//;_osdk_hook;/;}"
PROMPT_COMMAND="${PROMPT_COMMAND#;}"
PROMPT_COMMAND="${PROMPT_COMMAND%;}"
unset -f _osdk_hook 2>/dev/null || true
unset OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET
"#
        .to_string(),
        Shell::Zsh => r#"# osdk shell deactivation (zsh)
if [[ -n ${OSDK_MANAGED_ENV:-} ]]; then
  local _osdk_key
  for _osdk_key in ${(s:,:)OSDK_MANAGED_ENV}; do
    [[ $_osdk_key == [A-Za-z_][A-Za-z0-9_]# ]] || continue
    local _osdk_present_var="OSDK_ORIG_${_osdk_key}_PRESENT"
    local _osdk_original_var="OSDK_ORIG_${_osdk_key}"
    if [[ ${(P)_osdk_present_var:-0} == 1 ]]; then
      export "$_osdk_key=${(P)_osdk_original_var}"
    else
      unset "$_osdk_key"
    fi
    unset "OSDK_ORIG_${_osdk_key}" "OSDK_ORIG_${_osdk_key}_PRESENT" "OSDK_ORIG_${_osdk_key}_SET"
  done
fi
if [[ -n ${OSDK_ORIGINAL_PATH_SET+x} ]]; then export PATH="$OSDK_ORIGINAL_PATH"; fi
precmd_functions=(${precmd_functions:#_osdk_hook})
unfunction _osdk_hook 2>/dev/null || true
unset OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET
"#
        .to_string(),
        Shell::Fish => r#"# osdk shell deactivation (fish)
if set -q OSDK_MANAGED_ENV
  for _osdk_key in (string split , -- $OSDK_MANAGED_ENV)
    string match -rq '^[A-Za-z_][A-Za-z0-9_]*$' -- $_osdk_key; or continue
    set _osdk_original OSDK_ORIG_$_osdk_key
    set _osdk_present $_osdk_original"_PRESENT"
    if test "$$_osdk_present" = 1
      set -gx $_osdk_key "$$_osdk_original"
    else
      set -e $_osdk_key
    end
    set -e $_osdk_original $_osdk_present $_osdk_original"_SET"
  end
  set -e _osdk_key _osdk_original _osdk_present
end
if set -q OSDK_ORIGINAL_PATH_SET
  set -gx PATH $OSDK_ORIGINAL_PATH
end
functions -e _osdk_hook
set -e OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET
"#
        .to_string(),
        Shell::Powershell => r#"# osdk shell deactivation (powershell)
if (Test-Path Env:OSDK_MANAGED_ENV) {
  foreach ($osdkKey in ($env:OSDK_MANAGED_ENV -split ',')) {
    if ($osdkKey -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') { continue }
    $original = "OSDK_ORIG_${osdkKey}"
    $present = "${original}_PRESENT"
    if ([Environment]::GetEnvironmentVariable($present) -eq '1') {
      [Environment]::SetEnvironmentVariable($osdkKey, [Environment]::GetEnvironmentVariable($original))
    } else {
      Remove-Item "Env:$osdkKey" -ErrorAction SilentlyContinue
    }
    Remove-Item "Env:$original","Env:$present","Env:${original}_SET" -ErrorAction SilentlyContinue
  }
}
if (Test-Path Env:OSDK_ORIGINAL_PATH_SET) { $env:PATH = $env:OSDK_ORIGINAL_PATH }
$ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = $null
Remove-Item Function:Invoke-OsdkHook -ErrorAction SilentlyContinue
Remove-Variable OsdkHookRunning -Scope Script -ErrorAction SilentlyContinue
Remove-Item Env:OSDK_MANAGED_ENV,Env:OSDK_ORIGINAL_PATH,Env:OSDK_ORIGINAL_PATH_SET -ErrorAction SilentlyContinue
"#
        .to_string(),
    }
}

/// The env changes to apply for the active toolset in `cwd`.
pub struct EnvDelta {
    /// Directories to prepend to PATH (project bins, shims, and active tools'
    /// bin dirs, with a managed Node guard when required).
    pub path_prepend: Vec<PathBuf>,
    /// Variables to set (GOROOT, JAVA_HOME, ...).
    pub set_vars: BTreeMap<String, String>,
    /// Variables managed by the previous hook invocation but no longer active.
    pub unset_vars: Vec<String>,
}

/// Compute the env delta for the directory `cwd`: for each backend with an
/// active + installed version, collect its bin dirs and exec env.
pub fn compute_env_delta(ctx: &Ctx, registry: &Registry, cwd: &std::path::Path) -> EnvDelta {
    let mut path_prepend = Vec::new();
    let mut set_vars = BTreeMap::new();
    let mut has_generated_shim = false;
    let dynamic_report = crate::shim::scan_dynamic_installs(ctx).ok();
    let mut backend_ids = registry
        .all()
        .iter()
        .map(|backend| backend.id().to_string())
        .collect::<Vec<_>>();
    if let Some(report) = &dynamic_report {
        backend_ids.extend(crate::shim::configured_and_installed_dynamic_ids(
            ctx, report,
        ));
        backend_ids.sort();
        backend_ids.dedup();
    }

    for backend_id in backend_ids {
        let Ok(backend) = registry.get(&backend_id) else {
            continue;
        };
        let dynamic_request = crate::shim::dynamic_request_from_config(ctx, backend.id());
        let active = match resolve_active(
            backend.id(),
            cwd,
            &ctx.config.tools,
            backend.idiomatic_files(),
        ) {
            Some(a) => Some((a.spec, a.is_range)),
            None => dynamic_request.map(|request| (request.spec.to_string(), false)),
        };
        let Some((active_spec, active_is_range)) = active else {
            continue;
        };
        // Resolve to an installed version.
        let installed = backend.list_installed(ctx).unwrap_or_default();
        if installed.is_empty() {
            continue;
        }
        let expanded = ctx
            .config
            .expand_alias(backend.id(), &active_spec)
            .unwrap_or(active_spec);
        let spec = strip_distribution_prefix(&expanded);
        let version = if backend.id() == "python" {
            crate::backend::python::select_installed(spec, &installed)
        } else {
            let parsed = if active_is_range {
                VersionSpec::parse_range(spec).unwrap_or_else(|_| VersionSpec::parse(spec))
            } else {
                VersionSpec::parse(spec)
            };
            match &parsed {
                VersionSpec::Exact(v) if installed.iter().any(|i| i == v) => Some(v.clone()),
                _ => {
                    let infos: Vec<VersionInfo> =
                        installed.iter().map(VersionInfo::stable).collect();
                    select_version(&parsed, &infos).map(|vi| vi.version.clone())
                }
            }
        };
        let version = match version {
            Some(v) => v,
            None => continue,
        };
        let tv = ToolVersion::new(backend.id(), &version);
        if crate::shim::routed_bin_names(ctx, backend.as_ref(), &tv).is_ok_and(|names| {
            names
                .into_iter()
                .any(|name| shim_exists(&ctx.dirs.shims(), &name))
        }) {
            has_generated_shim = true;
        }
        let backend_bins = backend
            .bin_paths(ctx, &tv)
            .ok()
            .into_iter()
            .flatten()
            .collect::<std::collections::BTreeSet<_>>();
        let bins = backend_bins
            .into_iter()
            .chain(
                crate::shim::dynamic_manifest_bin_paths(ctx, backend.id(), &version)
                    .ok()
                    .into_iter()
                    .flatten(),
            )
            .collect::<std::collections::BTreeSet<_>>();
        for b in bins {
            if b.exists() {
                path_prepend.push(b);
            }
        }
        if let Ok(env) = backend.exec_env(ctx, &tv) {
            for (k, v) in env {
                set_vars.insert(k, v);
            }
        }
    }

    prioritize_managed_paths(&mut path_prepend, &ctx.dirs.shims(), has_generated_shim);
    if project_npm_bins_are_trusted(ctx, cwd) {
        if let Some(project_bin) = project_bin_for(cwd) {
            prioritize_project_bin(&mut path_prepend, project_bin);
        }
    }

    let previous = std::env::var("OSDK_MANAGED_ENV").unwrap_or_default();
    let unset_vars = previous
        .split(',')
        .filter(|key| !key.is_empty() && valid_env_name(key) && !set_vars.contains_key(*key))
        .map(str::to_string)
        .collect();

    EnvDelta {
        path_prepend,
        set_vars,
        unset_vars,
    }
}

fn project_npm_bins_are_trusted(ctx: &Ctx, cwd: &std::path::Path) -> bool {
    let Some(config_path) = ctx.config.tool_origins.iter().find_map(|(key, origin)| {
        let configured = key.starts_with("npm:")
            || ctx
                .config
                .tools
                .get(key)
                .is_some_and(|value| value.starts_with("npm:"));
        match (configured, origin) {
            (true, crate::config::ToolConfigOrigin::ProjectConfig(path)) => Some(path),
            _ => None,
        }
    }) else {
        return false;
    };
    let Some(config_root) = config_path.parent() else {
        return false;
    };
    let Ok(config_root) = dunce::canonicalize(config_root) else {
        return false;
    };
    let Ok(cwd) = dunce::canonicalize(cwd) else {
        return false;
    };
    cwd.starts_with(&config_root)
        && crate::trust::is_trusted(
            &ctx.dirs.config,
            config_path,
            std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS").as_ref(),
        )
        .unwrap_or(false)
}

/// Return the nearest Node project's installed command directory. The nearest
/// `package.json` is a hard boundary: if its `.bin` is absent or unsafe, an
/// outer project must not leak commands into the current working directory.
fn project_bin_for(cwd: &std::path::Path) -> Option<PathBuf> {
    let project_root = cwd.ancestors().find_map(|directory| {
        let package_json = directory.join("package.json");
        match std::fs::symlink_metadata(&package_json) {
            Ok(metadata) => Some(metadata.file_type().is_file().then_some(directory)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            // Fail closed when a possible nearer boundary cannot be inspected.
            Err(_) => Some(None),
        }
    })??;

    let canonical_root = dunce::canonicalize(project_root).ok()?;
    if !canonical_root.is_dir() {
        return None;
    }
    let canonical_package = dunce::canonicalize(project_root.join("package.json")).ok()?;
    if !canonical_package.is_file()
        || canonical_package
            .parent()
            .is_none_or(|parent| !same_existing_path(parent, &canonical_root))
    {
        return None;
    }

    let node_modules = project_root.join("node_modules");
    let bin = node_modules.join(".bin");
    if std::fs::symlink_metadata(&node_modules)
        .ok()
        .is_none_or(|metadata| !metadata.file_type().is_dir())
        || std::fs::symlink_metadata(&bin)
            .ok()
            .is_none_or(|metadata| !metadata.file_type().is_dir())
    {
        return None;
    }

    let canonical_node_modules = dunce::canonicalize(&node_modules).ok()?;
    let canonical_bin = dunce::canonicalize(&bin).ok()?;
    (canonical_node_modules
        .parent()
        .is_some_and(|parent| same_existing_path(parent, &canonical_root))
        && canonical_bin
            .parent()
            .is_some_and(|parent| same_existing_path(parent, &canonical_node_modules)))
    .then_some(canonical_bin)
}

fn prioritize_project_bin(paths: &mut Vec<PathBuf>, project_bin: PathBuf) {
    let node_collision = directory_provides_command(&project_bin, "node").unwrap_or(true);
    // Never put a directory that can replace `node` on PATH. Elevating the
    // complete managed Node bin would also elevate bundled npm/npx ahead of
    // osdk's registry-aware shims, so fail closed for this unusual project.
    if node_collision {
        return;
    }
    paths.retain(|path| !same_existing_path(path, &project_bin));
    paths.insert(0, project_bin);
}

fn same_existing_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right || same_file::is_same_file(left, right).unwrap_or(false)
}

fn directory_provides_command(directory: &std::path::Path, command: &str) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(directory)? {
        let file_name = entry?.file_name();
        if command_name_matches(&file_name, command) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn command_name_matches(file_name: &std::ffi::OsStr, command: &str) -> bool {
    #[cfg(windows)]
    {
        let file_name = file_name.to_string_lossy();
        file_name.eq_ignore_ascii_case(command)
            || file_name
                .get(..command.len() + 1)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&format!("{command}.")))
    }
    #[cfg(not(windows))]
    {
        file_name == std::ffi::OsStr::new(command)
    }
}

fn prioritize_managed_paths(
    paths: &mut Vec<PathBuf>,
    shims: &std::path::Path,
    has_generated_shim: bool,
) {
    // Shell activation must not bypass the shim launchers: package-manager
    // shims perform the registry preflight before entering the real binary.
    // Keep the real backend dirs behind shims so executables without a
    // generated shim remain available. Package managers still precede Node's
    // bin dir there, preventing Node's bundled npm/corepack launchers from
    // shadowing independently managed package managers. Once a shim starts,
    // it removes the shim dir and constructs its own lifecycle-safe PATH.
    paths.retain(|path| path != shims);
    paths.sort_by_key(|path| managed_runtime_path_priority(path));
    if has_generated_shim && !paths.is_empty() {
        paths.insert(0, shims.to_path_buf());
    }
}

fn shim_exists(shims: &std::path::Path, name: &str) -> bool {
    shims.join(name).is_file() || (cfg!(windows) && shims.join(format!("{name}.cmd")).is_file())
}

fn managed_runtime_path_priority(path: &std::path::Path) -> u8 {
    let components: std::collections::BTreeSet<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if ["npm", "pnpm", "yarn"]
        .iter()
        .any(|backend| components.contains(backend))
    {
        0
    } else if components.contains("node") {
        1
    } else {
        2
    }
}

/// Render `hook-env` output: shell commands that prepend PATH and set vars.
pub fn render_hook_env(shell: Shell, delta: &EnvDelta) -> String {
    let mut out = String::new();
    render_path_reset(shell, &delta.path_prepend, &mut out);

    for key in &delta.unset_vars {
        if valid_env_name(key) {
            render_restore_var(shell, key, &mut out);
        }
    }

    for (k, v) in &delta.set_vars {
        if valid_env_name(k) {
            render_capture_var(shell, k, &mut out);
            match shell {
                Shell::Fish => out.push_str(&format!("set -gx {} {}\n", k, shell_quote(shell, v))),
                Shell::Powershell => {
                    out.push_str(&format!("$env:{} = {}\n", k, powershell_quote(v)))
                }
                _ => out.push_str(&format!("export {}={}\n", k, shell_quote(shell, v))),
            }
        }
    }
    let managed = delta
        .set_vars
        .keys()
        .filter(|key| valid_env_name(key))
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    match shell {
        Shell::Fish if managed.is_empty() => out.push_str("set -e OSDK_MANAGED_ENV\n"),
        Shell::Fish => out.push_str(&format!(
            "set -gx OSDK_MANAGED_ENV {}\n",
            shell_quote(shell, &managed)
        )),
        Shell::Powershell if managed.is_empty() => {
            out.push_str("Remove-Item Env:OSDK_MANAGED_ENV -ErrorAction SilentlyContinue\n")
        }
        Shell::Powershell => out.push_str(&format!(
            "$env:OSDK_MANAGED_ENV = {}\n",
            powershell_quote(&managed)
        )),
        _ if managed.is_empty() => out.push_str("unset OSDK_MANAGED_ENV\n"),
        _ => out.push_str(&format!(
            "export OSDK_MANAGED_ENV={}\n",
            shell_quote(shell, &managed)
        )),
    }
    out
}

fn render_path_reset(shell: Shell, paths: &[PathBuf], out: &mut String) {
    match shell {
        Shell::Fish => {
            out.push_str(
                "if not set -q OSDK_ORIGINAL_PATH_SET\n  set -gx OSDK_ORIGINAL_PATH $PATH\n  set -gx OSDK_ORIGINAL_PATH_SET 1\nend\n",
            );
            if paths.is_empty() {
                out.push_str("set -gx PATH $OSDK_ORIGINAL_PATH\n");
            } else {
                out.push_str(&format!(
                    "set -gx PATH {} $OSDK_ORIGINAL_PATH\n",
                    paths
                        .iter()
                        .map(|path| shell_quote(shell, &path.display().to_string()))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
        }
        Shell::Powershell => {
            out.push_str(
                "if (-not (Test-Path Env:OSDK_ORIGINAL_PATH_SET)) { $env:OSDK_ORIGINAL_PATH = $env:PATH; $env:OSDK_ORIGINAL_PATH_SET = '1' }\n",
            );
            if paths.is_empty() {
                out.push_str("$env:PATH = $env:OSDK_ORIGINAL_PATH\n");
            } else {
                let joined = std::env::join_paths(paths)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                out.push_str(&format!(
                    "$env:PATH = {} + [IO.Path]::PathSeparator + $env:OSDK_ORIGINAL_PATH\n",
                    powershell_quote(&joined)
                ));
            }
        }
        _ => {
            out.push_str(
                "if [ -z \"${OSDK_ORIGINAL_PATH_SET+x}\" ]; then export OSDK_ORIGINAL_PATH=\"$PATH\"; export OSDK_ORIGINAL_PATH_SET=1; fi\n",
            );
            if paths.is_empty() {
                out.push_str("export PATH=\"$OSDK_ORIGINAL_PATH\"\n");
            } else {
                let joined = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":");
                out.push_str(&format!(
                    "export PATH={}:\"$OSDK_ORIGINAL_PATH\"\n",
                    shell_quote(shell, &joined)
                ));
            }
        }
    }
}

fn render_capture_var(shell: Shell, key: &str, out: &mut String) {
    let original = format!("OSDK_ORIG_{key}");
    let present = format!("{original}_PRESENT");
    let set = format!("{original}_SET");
    match shell {
        Shell::Fish => out.push_str(&format!(
            "if not set -q {set}\n  if set -q {key}\n    set -gx {original} \"${key}\"\n    set -gx {present} 1\n  else\n    set -e {original}\n    set -gx {present} 0\n  end\n  set -gx {set} 1\nend\n"
        )),
        Shell::Powershell => out.push_str(&format!(
            "if (-not (Test-Path Env:{set})) {{ if (Test-Path Env:{key}) {{ $env:{original} = $env:{key}; $env:{present} = '1' }} else {{ Remove-Item Env:{original} -ErrorAction SilentlyContinue; $env:{present} = '0' }}; $env:{set} = '1' }}\n"
        )),
        _ => out.push_str(&format!(
            "if [ -z \"${{{set}+x}}\" ]; then if [ -n \"${{{key}+x}}\" ]; then export {original}=\"${key}\"; export {present}=1; else unset {original}; export {present}=0; fi; export {set}=1; fi\n"
        )),
    }
}

fn render_restore_var(shell: Shell, key: &str, out: &mut String) {
    let original = format!("OSDK_ORIG_{key}");
    let present = format!("{original}_PRESENT");
    let set = format!("{original}_SET");
    match shell {
        Shell::Fish => out.push_str(&format!(
            "if set -q {set}\n  if test \"${present}\" = 1\n    set -gx {key} \"${original}\"\n  else\n    set -e {key}\n  end\n  set -e {original} {present} {set}\nend\n"
        )),
        Shell::Powershell => out.push_str(&format!(
            "if (Test-Path Env:{set}) {{ if ($env:{present} -eq '1') {{ $env:{key} = $env:{original} }} else {{ Remove-Item Env:{key} -ErrorAction SilentlyContinue }}; Remove-Item Env:{original},Env:{present},Env:{set} -ErrorAction SilentlyContinue }}\n"
        )),
        _ => out.push_str(&format!(
            "if [ -n \"${{{set}+x}}\" ]; then if [ \"${{{present}:-0}}\" = 1 ]; then export {key}=\"${original}\"; else unset {key}; fi; unset {original} {present} {set}; fi\n"
        )),
    }
}

fn valid_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn shell_quote(shell: Shell, s: &str) -> String {
    match shell {
        Shell::Powershell => powershell_quote(s),
        _ => {
            // single-quote for POSIX/fish, escaping embedded quotes
            let escaped = s.replace('\'', r"'\''");
            format!("'{escaped}'")
        }
    }
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Strip a leading `<word>-` distribution prefix (java's temurin-17).
fn strip_distribution_prefix(spec: &str) -> &str {
    if let Some((left, right)) = spec.split_once('-') {
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_alphabetic()) && !right.is_empty() {
            return right;
        }
    }
    spec
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::store::Cas;

    fn test_ctx(root: &std::path::Path, tools: &[(&str, &str)]) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
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
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig::default(),
                tools: tools
                    .iter()
                    .map(|(tool, version)| (tool.to_string(), version.to_string()))
                    .collect(),
                tool_configs: BTreeMap::new(),
                global_tools: BTreeMap::new(),
                global_tool_configs: BTreeMap::new(),
                tool_origins: BTreeMap::new(),
                aliases: BTreeMap::new(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    fn mark_installed(ctx: &Ctx, tool: &str, version: &str, bin_dir: &str) -> PathBuf {
        let install = ctx.dirs.install_path(tool, version);
        let bin = install.join(bin_dir);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();
        bin
    }

    #[test]
    fn parse_shell() {
        assert_eq!("bash".parse::<Shell>().unwrap(), Shell::Bash);
        assert_eq!("pwsh".parse::<Shell>().unwrap(), Shell::Powershell);
        assert!("tcsh".parse::<Shell>().is_err());
    }

    #[test]
    fn activation_snippet_mentions_hook_env() {
        let s = activation_script(Shell::Bash, "osdk");
        assert!(s.contains("hook-env"));
        assert!(s.contains("PROMPT_COMMAND"));

        let powershell =
            activation_script(Shell::Powershell, r"C:\Program Files\中文 osdk\osdk.exe");
        assert!(powershell.contains(r"& 'C:\Program Files\中文 osdk\osdk.exe' hook-env"));
        assert!(powershell.contains("$script:OsdkHookRunning"));
        assert!(powershell.contains("finally"));
        assert!(
            powershell.find("Invoke-OsdkHook\n").unwrap()
                < powershell.find("PostCommandLookupAction").unwrap()
        );
    }

    #[test]
    fn deactivation_snippets_remove_hooks_and_restore_path() {
        let bash = deactivation_script(Shell::Bash);
        assert!(bash.contains("unset -f _osdk_hook"));
        assert!(bash.contains("PATH=\"$OSDK_ORIGINAL_PATH\""));

        let zsh = deactivation_script(Shell::Zsh);
        assert!(zsh.contains("precmd_functions"));
        assert!(zsh.contains("unfunction _osdk_hook"));

        let fish = deactivation_script(Shell::Fish);
        assert!(fish.contains("functions -e _osdk_hook"));

        let powershell = deactivation_script(Shell::Powershell);
        assert!(powershell.contains("PostCommandLookupAction = $null"));
        assert!(powershell.contains("Remove-Variable OsdkHookRunning"));
    }

    #[test]
    fn hook_env_renders_path_and_vars() {
        let mut set_vars = BTreeMap::new();
        set_vars.insert("GOROOT".to_string(), "/x/go".to_string());
        let delta = EnvDelta {
            path_prepend: vec![PathBuf::from("/x/go/bin")],
            set_vars,
            unset_vars: vec!["JAVA_HOME".into()],
        };
        let out = render_hook_env(Shell::Bash, &delta);
        assert!(out.contains("export PATH='/x/go/bin':\"$OSDK_ORIGINAL_PATH\""));
        assert!(out.contains("export GOROOT='/x/go'"));
        assert!(out.contains("unset JAVA_HOME"));
        assert!(out.contains("export OSDK_MANAGED_ENV='GOROOT'"));

        let fish = render_hook_env(Shell::Fish, &delta);
        assert!(fish.contains("set -gx PATH"));
        assert!(fish.contains("set -gx GOROOT"));
    }

    #[test]
    fn activation_path_prefers_shims_and_keeps_real_bin_fallbacks() {
        let shims = PathBuf::from("/osdk/shims");
        let npm = PathBuf::from("/osdk/installs/npm/10.9.0/bin");
        let pnpm = PathBuf::from("/osdk/installs/pnpm/9.15.0");
        let node = PathBuf::from("/osdk/installs/node/22.14.0/bin");
        let go = PathBuf::from("/osdk/installs/go/1.24.0/bin");
        let mut paths = vec![
            node.clone(),
            go.clone(),
            npm.clone(),
            shims.clone(),
            pnpm.clone(),
        ];

        prioritize_managed_paths(&mut paths, &shims, true);

        assert_eq!(paths, vec![shims, npm, pnpm, node, go]);
    }

    #[test]
    fn project_bin_precedes_shims_without_shadowing_managed_node() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let project_bin = project.join("node_modules").join(".bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        write_test_command(&project_bin, "eslint");

        let shims = temporary.path().join("osdk/shims");
        let node = temporary.path().join("osdk/installs/node/22.14.0/bin");
        let npm = temporary.path().join("osdk/installs/npm/10.9.0/bin");
        let mut paths = vec![shims.clone(), npm.clone(), node.clone()];

        let discovered = project_bin_for(&project).unwrap();
        prioritize_project_bin(&mut paths, discovered.clone());
        assert_eq!(
            paths,
            vec![discovered.clone(), shims.clone(), npm, node.clone()]
        );

        write_test_command(&project_bin, "node");
        let mut guarded_paths = vec![shims.clone(), node.clone()];
        prioritize_project_bin(&mut guarded_paths, project_bin_for(&project).unwrap());
        assert_eq!(guarded_paths, vec![shims, node]);
    }

    #[test]
    fn project_bin_with_node_is_omitted_without_managed_node() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let project_bin = project.join("node_modules").join(".bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        write_test_command(&project_bin, "node");

        let mut paths = vec![temporary.path().join("osdk/shims")];
        let discovered = project_bin_for(&project).unwrap();
        prioritize_project_bin(&mut paths, discovered);

        assert_eq!(paths, vec![temporary.path().join("osdk/shims")]);
    }

    #[test]
    fn project_bin_discovery_stops_at_nearest_package_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let outer = temporary.path().join("outer");
        let outer_bin = outer.join("node_modules").join(".bin");
        let nested = outer.join("packages/app/src");
        std::fs::create_dir_all(&outer_bin).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(outer.join("package.json"), b"{}").unwrap();
        std::fs::write(outer.join("packages/app/package.json"), b"{}").unwrap();

        assert_eq!(project_bin_for(&nested), None);
        assert!(!outer.join("packages/app/node_modules/.bin").exists());

        std::fs::remove_file(outer.join("packages/app/package.json")).unwrap();
        assert_eq!(
            project_bin_for(&nested),
            Some(dunce::canonicalize(outer_bin).unwrap())
        );
    }

    #[test]
    fn missing_project_bin_is_not_created() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/src");
        let project_bin = temporary.path().join("project/node_modules/.bin");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(temporary.path().join("project/package.json"), b"{}").unwrap();

        assert_eq!(project_bin_for(&project), None);
        assert!(!project_bin.exists());
    }

    #[cfg(unix)]
    #[test]
    fn project_bin_discovery_rejects_paths_outside_project() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let outside = temporary.path().join("outside-bin");
        std::fs::create_dir_all(project.join("node_modules")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        symlink(&outside, project.join("node_modules/.bin")).unwrap();

        assert_eq!(project_bin_for(&project), None);
    }

    #[cfg(unix)]
    #[test]
    fn project_bin_discovery_rejects_symlinked_package_marker() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let nested = project.join("src");
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(temporary.path().join("package.json"), b"{}").unwrap();
        symlink(
            temporary.path().join("package.json"),
            project.join("package.json"),
        )
        .unwrap();

        assert_eq!(project_bin_for(&nested), None);
    }

    #[cfg(unix)]
    #[test]
    fn project_bin_discovery_rejects_symlinked_node_modules() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let outside_modules = temporary.path().join("outside-modules");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(outside_modules.join(".bin")).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        symlink(&outside_modules, project.join("node_modules")).unwrap();

        assert_eq!(project_bin_for(&project), None);
    }

    #[test]
    fn command_collision_names_follow_target_rules() {
        #[cfg(windows)]
        for name in [
            "node", "node.com", "NODE.EXE", "NoDe.BaT", "node.CMD", "NODE.ps1",
        ] {
            assert!(command_name_matches(std::ffi::OsStr::new(name), "node"));
        }
        #[cfg(windows)]
        assert!(command_name_matches(
            std::ffi::OsStr::new("node.js"),
            "node"
        ));

        #[cfg(not(windows))]
        {
            assert!(command_name_matches(std::ffi::OsStr::new("node"), "node"));
            assert!(!command_name_matches(std::ffi::OsStr::new("Node"), "node"));
            assert!(!command_name_matches(
                std::ffi::OsStr::new("node.cmd"),
                "node"
            ));
        }
    }

    #[test]
    fn project_path_is_deduplicated_against_existing_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let project_bin = temporary.path().join("node_modules/.bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        let canonical = dunce::canonicalize(&project_bin).unwrap();
        let mut paths = vec![project_bin, PathBuf::from("/osdk/shims")];

        prioritize_project_bin(&mut paths, canonical.clone());

        assert_eq!(paths, vec![canonical, PathBuf::from("/osdk/shims")]);
    }

    #[cfg(unix)]
    #[test]
    fn project_path_is_deduplicated_against_a_symlink_alias() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project_bin = temporary.path().join("node_modules/.bin");
        let alias = temporary.path().join("project-bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        symlink(&project_bin, &alias).unwrap();
        let canonical = dunce::canonicalize(&project_bin).unwrap();
        let mut paths = vec![alias, PathBuf::from("/osdk/shims")];

        prioritize_project_bin(&mut paths, canonical.clone());

        assert_eq!(paths, vec![canonical, PathBuf::from("/osdk/shims")]);
    }

    #[test]
    fn computed_activation_path_routes_node_bundled_npm_through_shims() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), &[("node", "22.14.0")]);
        let project = temporary.path().join("project/src");
        let project_bin = temporary.path().join("project/node_modules/.bin");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::write(temporary.path().join("project/package.json"), b"{}").unwrap();
        trust_project_npm(&mut ctx, &temporary.path().join("project"));
        write_test_command(&project_bin, "eslint");
        let node = mark_installed(
            &ctx,
            "node",
            "22.14.0",
            if cfg!(windows) { "" } else { "bin" },
        );
        let (node_name, npm_name, shim_name) = if cfg!(windows) {
            ("node.exe", "npm.cmd", "npm.cmd")
        } else {
            ("node", "npm", "npm")
        };
        let node_executable = node.join(node_name);
        let npm_executable = node.join(npm_name);
        std::fs::write(&node_executable, b"node").unwrap();
        std::fs::write(&npm_executable, b"npm").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for executable in [&node_executable, &npm_executable] {
                std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
        let shim = ctx.dirs.shims().join(shim_name);
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(&shim, b"shim").unwrap();

        let delta = compute_env_delta(&ctx, &Registry::new(), &project);

        assert_eq!(
            delta.path_prepend,
            vec![
                dunce::canonicalize(project_bin).unwrap(),
                ctx.dirs.shims(),
                node
            ]
        );
    }

    #[test]
    fn computed_activation_path_guards_node_from_project_bin() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), &[]);
        let project = temporary.path().join("project");
        let project_bin = project.join("node_modules/.bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::write(
            project.join("package.json"),
            br#"{"engines":{"node":">=20 <23"}}"#,
        )
        .unwrap();
        trust_project_npm(&mut ctx, &project);
        write_test_command(&project_bin, "node");

        let older_node = mark_installed(
            &ctx,
            "node",
            "20.0.0",
            if cfg!(windows) { "" } else { "bin" },
        );
        write_test_command(&older_node, "node");
        let node = mark_installed(
            &ctx,
            "node",
            "22.14.0",
            if cfg!(windows) { "" } else { "bin" },
        );
        write_test_command(&node, "node");
        let shim_name = if cfg!(windows) { "node.cmd" } else { "node" };
        let shim = ctx.dirs.shims().join(shim_name);
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(&shim, b"shim").unwrap();

        let delta = compute_env_delta(&ctx, &Registry::new(), &project);

        assert_eq!(delta.path_prepend, vec![ctx.dirs.shims(), node]);
    }

    #[test]
    fn untrusted_project_npm_bins_are_not_activated() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), &[]);
        let project = temporary.path().join("project");
        let bin = project.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"aube\"\n",
        )
        .unwrap();
        ctx.config.tools.insert("npm:prettier".into(), "3".into());
        ctx.config.tool_origins.insert(
            "npm:prettier".into(),
            crate::config::ToolConfigOrigin::ProjectConfig(project.join("osdk.toml")),
        );

        assert!(!project_npm_bins_are_trusted(&ctx, &project));
        assert!(!compute_env_delta(&ctx, &Registry::new(), &project)
            .path_prepend
            .contains(&dunce::canonicalize(bin).unwrap()));
    }

    #[test]
    fn activation_path_does_not_add_shims_without_an_active_runtime() {
        let mut no_active_bins = Vec::new();
        prioritize_managed_paths(
            &mut no_active_bins,
            std::path::Path::new("/osdk/shims"),
            false,
        );
        assert!(no_active_bins.is_empty());
    }

    #[test]
    fn activation_path_does_not_add_missing_shims() {
        let node = PathBuf::from("/osdk/installs/node/22.14.0/bin");
        let mut paths = vec![node.clone()];
        prioritize_managed_paths(&mut paths, std::path::Path::new("/osdk/shims"), false);
        assert_eq!(paths, vec![node]);
    }

    #[test]
    fn empty_delta_restores_original_path() {
        let delta = EnvDelta {
            path_prepend: Vec::new(),
            set_vars: BTreeMap::new(),
            unset_vars: vec!["GOROOT".into()],
        };
        let out = render_hook_env(Shell::Bash, &delta);
        assert!(out.contains("export PATH=\"$OSDK_ORIGINAL_PATH\""));
        assert!(out.contains("unset GOROOT"));
        assert!(out.contains("unset OSDK_MANAGED_ENV"));
    }

    fn write_test_command(directory: &std::path::Path, name: &str) {
        #[cfg(windows)]
        let path = directory.join(format!("{name}.cmd"));
        #[cfg(not(windows))]
        let path = directory.join(name);
        std::fs::write(path, b"test command").unwrap();
    }

    fn trust_project_npm(ctx: &mut Ctx, project: &std::path::Path) {
        let config = project.join("osdk.toml");
        std::fs::write(
            &config,
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"aube\"\n",
        )
        .unwrap();
        crate::trust::trust(&ctx.dirs.config, &config).unwrap();
        ctx.config.tools.insert("npm:prettier".into(), "3".into());
        ctx.config.tool_origins.insert(
            "npm:prettier".into(),
            crate::config::ToolConfigOrigin::ProjectConfig(config.clone()),
        );
        ctx.config.project_config_path = Some(config);
    }
}
