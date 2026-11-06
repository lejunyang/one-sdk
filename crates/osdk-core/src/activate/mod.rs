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
use crate::version::resolver::resolve_active_config;
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
  # Inside `osdk run`, the task already got its environment directly from the
  # runner. Re-running the hook here would re-derive PATH from the *config*,
  # discarding the per-task `env` and any tool the task just installed.
  if [[ -n "${{OSDK_TASK:-}}" ]]; then
    # The marker is inherited by whatever the task starts, including shells
    # and editors that outlive it, so it only holds inside the task's own
    # project. Once this shell leaves it, it is an ordinary shell again.
    local _osdk_root="${{OSDK_TASK_ROOT:-}}"
    [[ -z "$_osdk_root" ]] && return 0
    local _osdk_here="$PWD"
    # Git Bash: compare both sides in one spelling. `cygpath -u` alone is not
    # enough, since mounts such as /tmp give one directory two names.
    if command -v cygpath >/dev/null 2>&1; then
      _osdk_root="$(cygpath -m "$_osdk_root")"
      _osdk_here="$(cygpath -m "$PWD")"
    fi
    case "$_osdk_here/" in "${{_osdk_root%/}}/"*) return 0 ;; esac
    unset OSDK_TASK OSDK_TASK_NAME OSDK_TASK_ROOT
  fi
  local out
  # stderr is left on the terminal: when this directory cannot be activated
  # (a pinned tool is not installed), the reason is shown instead of the
  # environment silently staying as it was.
  out="$({bin} hook-env --shell bash)" && eval "$out"
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
  # See the bash branch: a task's environment comes from the runner, not from
  # re-deriving it here.
  if [[ -n "${{OSDK_TASK:-}}" ]]; then
    # The marker is inherited by whatever the task starts, including shells
    # and editors that outlive it, so it only holds inside the task's own
    # project. Once this shell leaves it, it is an ordinary shell again.
    local _osdk_root="${{OSDK_TASK_ROOT:-}}"
    [[ -z "$_osdk_root" ]] && return 0
    local _osdk_here="$PWD"
    # Git Bash: compare both sides in one spelling. `cygpath -u` alone is not
    # enough, since mounts such as /tmp give one directory two names.
    if command -v cygpath >/dev/null 2>&1; then
      _osdk_root="$(cygpath -m "$_osdk_root")"
      _osdk_here="$(cygpath -m "$PWD")"
    fi
    case "$_osdk_here/" in "${{_osdk_root%/}}/"*) return 0 ;; esac
    unset OSDK_TASK OSDK_TASK_NAME OSDK_TASK_ROOT
  fi
  local out
  # stderr stays visible; see the bash branch.
  out="$({bin} hook-env --shell zsh)" && eval "$out"
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
  # See the bash branch, including why the marker is bounded by its root.
  if set -q OSDK_TASK
    if not set -q OSDK_TASK_ROOT
      return 0
    end
    set -l _osdk_root (string trim -r -c / -- $OSDK_TASK_ROOT)
    if test "$PWD" = "$_osdk_root"; or test (string sub -l (string length -- "$_osdk_root/") -- "$PWD/") = "$_osdk_root/"
      return 0
    end
    set -e OSDK_TASK OSDK_TASK_NAME OSDK_TASK_ROOT
  end
  # stderr stays visible; see the bash branch.
  {bin} hook-env --shell fish | source
end
_osdk_hook
"#,
            bin = shell_quote(Shell::Fish, osdk_bin)
        ),
        Shell::Powershell => format!(
            r#"# osdk shell integration (powershell)
$script:OsdkHookRunning = $false
function global:Invoke-OsdkHook {{
  # Inside `osdk run`, the task already received its environment from the
  # runner. Re-deriving it here would discard the per-task `env` and any tool
  # the task just installed -- and on a project whose config is untrusted it
  # would print a trust error into the task's own output.
  # The marker is inherited by whatever the task starts, including shells and
  # editors that outlive it, so it only holds inside the task's own project.
  if ($env:OSDK_TASK) {{
    if (-not $env:OSDK_TASK_ROOT) {{ return }}
    $osdkTaskRoot = $env:OSDK_TASK_ROOT.TrimEnd([char]92, [char]47)
    $osdkHere = (Get-Location).ProviderPath
    if ($osdkHere -ieq $osdkTaskRoot -or $osdkHere.StartsWith($osdkTaskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {{ return }}
    Remove-Item Env:OSDK_TASK,Env:OSDK_TASK_NAME,Env:OSDK_TASK_ROOT -ErrorAction SilentlyContinue
  }}
  # stderr stays visible (see the bash branch); only stdout is evaluated.
  $out = & {bin} hook-env --shell powershell
  if ($out) {{ Invoke-Expression ($out -join "`n") }}
}}
Invoke-OsdkHook
# The hook belongs on the prompt, not on PostCommandLookupAction: the latter
# fires once per *command lookup*, so a single pipeline or loop body re-ran the
# whole activation (22 subprocesses for one ten-iteration loop, ~1.1s each).
# bash/zsh/fish have always hooked their prompt; this matches them.
# `Test-Path Variable:` rather than reading the variable: under
# `Set-StrictMode -Version Latest` a bare read of a never-assigned variable is a
# terminating error, so probing it directly made activation abort in exactly the
# sessions careful enough to enable StrictMode.
if (-not (Test-Path Variable:script:OsdkOriginalPrompt)) {{
  # Wrap whatever prompt is already installed (Oh My Posh, Starship, a dotfile
  # override) instead of replacing it, and keep the builtin default when there
  # is none -- overwriting a user's prompt to install a PATH hook is not a
  # trade we get to make for them. Captured once so re-sourcing cannot nest
  # the wrapper inside itself.
  $existing = Get-Command prompt -CommandType Function -ErrorAction SilentlyContinue
  if ($existing) {{
    $script:OsdkOriginalPrompt = $existing.ScriptBlock
  }} else {{
    $script:OsdkOriginalPrompt = {{ "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }}
  }}
  function global:prompt {{
    if (-not (Test-Path Variable:script:OsdkHookRunning) -or -not $script:OsdkHookRunning) {{
      $script:OsdkHookRunning = $true
      # A failed hook must not cost the user their shell prompt, so its errors
      # stay contained here rather than propagating out of `prompt`.
      try {{ Invoke-OsdkHook }} catch {{ }} finally {{ $script:OsdkHookRunning = $false }}
    }}
    & $script:OsdkOriginalPrompt
  }}
}}
"#,
            bin = powershell_quote(osdk_bin)
        ),
    }
}

/// Render shell code that removes osdk's hook and restores the environment
/// captured by [`render_hook_env`].
pub fn deactivation_script(shell: Shell) -> String {
    let mut path = String::new();
    render_path_update(shell, &[], &mut path);
    let script = match shell {
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
@@OSDK_PATH@@
PROMPT_COMMAND=";${PROMPT_COMMAND:-};"
PROMPT_COMMAND="${PROMPT_COMMAND//;_osdk_hook;/;}"
PROMPT_COMMAND="${PROMPT_COMMAND#;}"
PROMPT_COMMAND="${PROMPT_COMMAND%;}"
unset -f _osdk_hook 2>/dev/null || true
unset OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET OSDK_ADDED_PATH OSDK_MISSING_WARNED
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
@@OSDK_PATH@@
precmd_functions=(${precmd_functions:#_osdk_hook})
unfunction _osdk_hook 2>/dev/null || true
unset OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET OSDK_ADDED_PATH OSDK_MISSING_WARNED
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
      set -e $_osdk_key; or true
    end
    set -e $_osdk_original $_osdk_present $_osdk_original"_SET"; or true
  end
  set -e _osdk_key _osdk_original _osdk_present; or true
end
@@OSDK_PATH@@
functions -e _osdk_hook
set -e OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET OSDK_ADDED_PATH OSDK_MISSING_WARNED; or true
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
@@OSDK_PATH@@
# Put the user's own prompt back. Deleting our wrapper without restoring what it
# wrapped would leave the session with no prompt function at all, silently
# discarding an Oh My Posh / Starship / dotfile prompt that we only borrowed.
# Probed with `Test-Path Variable:` for the same StrictMode reason as activation:
# deactivating a session that was never activated must be a no-op, not an error.
if ((Test-Path Variable:script:OsdkOriginalPrompt) -and $script:OsdkOriginalPrompt) {
  Set-Item Function:global:prompt $script:OsdkOriginalPrompt
  Remove-Variable OsdkOriginalPrompt -Scope Script -ErrorAction SilentlyContinue
}
# Older sessions activated before the prompt hook wired PostCommandLookupAction;
# clear it too so deactivating an already-running shell fully unhooks.
$ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = $null
Remove-Item Function:\Invoke-OsdkHook -ErrorAction SilentlyContinue
Remove-Variable OsdkHookRunning -Scope Script -ErrorAction SilentlyContinue
Remove-Item Env:OSDK_MANAGED_ENV,Env:OSDK_ORIGINAL_PATH,Env:OSDK_ORIGINAL_PATH_SET,Env:OSDK_ADDED_PATH,Env:OSDK_MISSING_WARNED -ErrorAction SilentlyContinue
"#
        .to_string(),
    };
    script.replace(
        "@@OSDK_PATH@@
",
        &path,
    )
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

/// Whether `backend_id` is declared with `lazy = true` in the effective config.
fn is_lazy_declaration(ctx: &Ctx, backend_id: &str) -> bool {
    ctx.config.tool_configs.iter().any(|(key, entry)| {
        entry.is_lazy()
            && (key == backend_id
                || crate::inventory::canonical_dynamic_id(key).ok().as_deref() == Some(backend_id))
    })
}

/// Compute the env delta for the directory `cwd`: for each backend with an
/// active + installed version, collect its bin dirs and exec env. Invalid
/// dynamic-tool inventory aborts activation instead of producing a partial
/// environment that could route around the configured tool.
///
/// A pinned tool that is not installed is an error here: `osdk env`, `exec`
/// and tasks run unattended, and must not quietly fall back to whatever else
/// is on PATH. The prompt hook uses [`compute_env_delta_allowing_missing`].
pub fn compute_env_delta(
    ctx: &Ctx,
    registry: &Registry,
    cwd: &std::path::Path,
) -> crate::Result<EnvDelta> {
    let (delta, missing) = compute_env_delta_allowing_missing(ctx, registry, cwd)?;
    match missing.into_iter().next() {
        Some(missing) => Err(crate::error::Error::other(crate::t!(
            "err.no_installed_match",
            tool = missing.tool,
            spec = missing.spec
        ))),
        None => Ok(delta),
    }
}

/// A pinned tool with no matching install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTool {
    pub tool: String,
    pub spec: String,
}

impl std::fmt::Display for MissingTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.tool, self.spec)
    }
}

/// [`compute_env_delta`] that activates what is installed and reports the
/// pinned tools that are not.
///
/// For the prompt hook. A fresh clone before `osdk install`, or a `git pull`
/// that moved `.nvmrc` to a version not yet installed, used to leave the whole
/// directory unactivated: one missing tool took every other one off PATH with
/// it. The caller warns about the missing ones instead.
pub fn compute_env_delta_allowing_missing(
    ctx: &Ctx,
    registry: &Registry,
    cwd: &std::path::Path,
) -> crate::Result<(EnvDelta, Vec<MissingTool>)> {
    let mut missing = Vec::new();
    use crate::version::strip_distribution_prefix;
    let mut path_prepend = Vec::new();
    let mut system_paths = Vec::new();
    let mut set_vars = BTreeMap::new();
    let mut has_generated_shim = false;
    let dynamic_report = crate::shim::scan_dynamic_installs(ctx)?;
    let project_npm_bin = trusted_project_npm_bin(ctx, cwd)?;
    let mut backend_ids = registry
        .all()
        .iter()
        .map(|backend| backend.id().to_string())
        .collect::<Vec<_>>();
    backend_ids.extend(crate::shim::configured_and_installed_dynamic_ids(
        ctx,
        &dynamic_report,
    ));
    backend_ids.sort();
    backend_ids.dedup();

    for backend_id in backend_ids {
        let Ok(backend) = registry.get(&backend_id) else {
            continue;
        };
        if crate::execution::enabled(ctx) {
            if let Some(crate::execution::ExecutionTarget::System(system)) =
                crate::execution::resolve(ctx, backend.as_ref(), cwd)?.target
            {
                if system
                    .published_names(ctx)
                    .iter()
                    .any(|name| shim_exists(&ctx.dirs.shims(), name))
                {
                    has_generated_shim = true;
                }
                system_paths.push(system.bin_dir().to_path_buf());
                set_vars.extend(system.env);
                continue;
            }
        }
        let dynamic_request = crate::shim::dynamic_request_from_config(ctx, backend.id());
        // Project-aware npm `use` is owned by the real project's package
        // manager and its curated `.osdk/npm-bin` generation below. It has no
        // osdk-owned install inventory to validate or expose here.
        if dynamic_request.as_ref().is_some_and(|request| {
            request.backend.starts_with("npm:")
                && crate::shim::configured_npm_scope(ctx, request)
                    .is_ok_and(|scope| scope == Some(crate::npm_tools::ToolScope::Project))
        }) {
            continue;
        }
        let active =
            match resolve_active_config(backend.id(), cwd, &ctx.config, backend.idiomatic_files())?
            {
                Some(a) => {
                    let installed = if a.fallbacks.is_empty() {
                        Vec::new()
                    } else {
                        backend.list_installed(ctx).unwrap_or_default()
                    };
                    let spec = a.choose_installed(|candidate| {
                        let candidate = ctx
                            .config
                            .expand_alias(backend.id(), candidate)
                            .unwrap_or_else(|_| candidate.to_string());
                        crate::version::installed_match(
                            ctx,
                            backend.id(),
                            &candidate,
                            a.is_range,
                            &installed,
                        )
                        .is_some()
                    });
                    Some((spec, a.is_range))
                }
                None => dynamic_request
                    .as_ref()
                    .map(|request| (request.spec.to_string(), false)),
            };
        let Some((active_spec, active_is_range)) = active else {
            continue;
        };
        // Resolve to an installed version.
        let expanded = ctx
            .config
            .expand_alias(backend.id(), &active_spec)
            .unwrap_or(active_spec);
        let spec = strip_distribution_prefix(&expanded);
        let parsed = if active_is_range {
            VersionSpec::parse_range(spec).unwrap_or_else(|_| VersionSpec::parse(spec))
        } else {
            VersionSpec::parse(spec)
        };
        // Exact configured dynamic requests are validated at their requested
        // root even when a backend omits legacy/missing inventories from its
        // installed list. This turns stale complete roots into explicit errors.
        let version = match (&dynamic_request, &parsed) {
            (Some(_), VersionSpec::Exact(version)) => Some(version.clone()),
            _ => {
                let installed = if dynamic_request.is_some() {
                    backend.list_installed(ctx)?
                } else {
                    backend.list_installed(ctx).unwrap_or_default()
                };
                if backend.id() == "python" {
                    crate::backend::python::select_installed(spec, &installed)
                } else if backend.id() == "java" {
                    // Within the requested distribution, not by version alone.
                    crate::backend::java::select_installed(
                        ctx,
                        &expanded,
                        active_is_range,
                        &installed,
                    )
                } else {
                    match &parsed {
                        VersionSpec::Exact(v) if installed.iter().any(|i| i == v) => {
                            Some(v.clone())
                        }
                        _ => {
                            let infos: Vec<VersionInfo> =
                                installed.iter().map(VersionInfo::stable).collect();
                            select_version(&parsed, &infos).map(|vi| vi.version.clone())
                        }
                    }
                }
            }
        };
        let version = match version {
            Some(v) => v,
            // `lazy = true` means "install when a task or an explicit operand
            // needs it", so being absent is its normal state. Treating that
            // like a missing pin failed activation for the whole directory --
            // this repository's own `bun = { lazy = true }` did -- and the
            // prompt hook hid the error, so nothing else got activated either.
            None if is_lazy_declaration(ctx, backend.id()) => continue,
            None => {
                missing.push(MissingTool {
                    tool: backend.id().to_string(),
                    spec: spec.to_string(),
                });
                continue;
            }
        };
        // An exact dynamic version skipped the installed listing above; a lazy
        // one that was never installed is likewise absent, not invalid.
        if dynamic_request.is_some()
            && is_lazy_declaration(ctx, backend.id())
            && !backend
                .list_installed(ctx)
                .is_ok_and(|installed| installed.contains(&version))
        {
            continue;
        }
        let mut tv = ToolVersion::new(backend.id(), &version);
        let dynamic_install = if let Some(request) = dynamic_request.as_ref() {
            tv.options = request.options.clone();
            Some(crate::shim::validated_dynamic_install(
                ctx,
                &dynamic_report,
                request,
                &version,
            )?)
        } else {
            None
        };
        let routed_names = if let Some(install) = dynamic_install.as_ref() {
            install.bin_names()
        } else {
            crate::shim::routed_bin_names(ctx, backend.as_ref(), &tv).unwrap_or_default()
        };
        if routed_names
            .into_iter()
            .any(|name| shim_exists(&ctx.dirs.shims(), &name))
        {
            has_generated_shim = true;
        }
        let bins = if let Some(install) = dynamic_install.as_ref() {
            install.activation_bin_paths()
        } else {
            backend.bin_paths(ctx, &tv).unwrap_or_default()
        };
        for b in bins {
            if b.exists() {
                path_prepend.push(b);
            }
        }
        let env = if dynamic_request.is_some() {
            Some(backend.exec_env(ctx, &tv)?)
        } else {
            backend.exec_env(ctx, &tv).ok()
        };
        if let Some(env) = env {
            for (k, v) in env {
                set_vars.insert(k, v);
            }
        }
    }

    // A broad external directory can contain unrelated tools. Keep selected
    // managed directories ahead of it; generated shims remain first so they
    // dispatch each tool through its own selection/preflight.
    prioritize_managed_paths(&mut path_prepend, &ctx.dirs.shims(), has_generated_shim);
    path_prepend.extend(system_paths);
    if has_generated_shim && !path_prepend.contains(&ctx.dirs.shims()) {
        path_prepend.insert(0, ctx.dirs.shims());
    }
    if let Some(project_bin) = project_npm_bin {
        prioritize_project_bin(&mut path_prepend, project_bin);
    }

    let previous = std::env::var("OSDK_MANAGED_ENV").unwrap_or_default();
    let unset_vars = previous
        .split(',')
        .filter(|key| !key.is_empty() && valid_env_name(key) && !set_vars.contains_key(*key))
        .map(str::to_string)
        .collect();

    Ok((
        EnvDelta {
            path_prepend,
            set_vars,
            unset_vars,
        },
        missing,
    ))
}

fn trusted_project_npm_bin(ctx: &Ctx, cwd: &std::path::Path) -> crate::Result<Option<PathBuf>> {
    let config_path = ctx.config.tool_origins.iter().find_map(|(key, origin)| {
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
    });
    let Some(config_path) = config_path else {
        return Ok(None);
    };
    let Some(config_root) = config_path.parent() else {
        return Ok(None);
    };
    let Ok(config_root) = dunce::canonicalize(config_root) else {
        return Ok(None);
    };
    let Ok(cwd) = dunce::canonicalize(cwd) else {
        return Ok(None);
    };
    let Some(project_root) = nearest_package_root(&cwd) else {
        return Ok(None);
    };
    // The gate asks "was the config that declared these specs reviewed?", so a
    // config with nothing to review passes it. Demanding a positive record
    // regardless would fail closed in the wrong place: declaring an npm tool no
    // longer requires trust, so a hand-written config has no record, and
    // insisting on one would skip activation *silently* rather than refusing
    // loudly -- the curated launchers would just not appear on PATH, with no
    // error explaining why.
    let reviewed = crate::trust::requires_trust(config_path).is_ok_and(|required| !required)
        || crate::trust::is_trusted(
            &ctx.dirs.config,
            config_path,
            std::env::var_os("OSDK_TRUSTED_CONFIG_PATHS").as_ref(),
        )
        .unwrap_or(false);
    if !same_existing_path(&config_root, &project_root) || !reviewed {
        return Ok(None);
    }
    let configured_specs = project_npm_configured_specs(ctx, config_path)?;
    crate::backend::npm_package::validated_project_bin_dir(&project_root, &configured_specs)
        .map_err(|error| crate::error::Error::other(error.to_string()))
}

fn project_npm_configured_specs(
    ctx: &Ctx,
    config_path: &std::path::Path,
) -> crate::Result<BTreeMap<String, String>> {
    let config_root = config_path.parent().ok_or_else(|| {
        crate::error::Error::other(format!(
            "project config has no parent: {}",
            config_path.display()
        ))
    })?;
    let config = crate::config::Config::load(&ctx.dirs.user_config_file(), config_root)?;
    canonical_project_npm_specs(
        config
            .tools
            .iter()
            .filter(|(key, _)| {
                matches!(
                    config.tool_origins.get(*key),
                    Some(crate::config::ToolConfigOrigin::ProjectConfig(path))
                        if same_existing_path(path, config_path)
                )
            })
            .map(|(key, value)| (key.clone(), value.clone())),
    )
}

/// Reconstruct canonical npm backend specs from project tool entries without
/// letting raw-key ordering decide which of two aliases wins.
pub fn canonical_project_npm_specs(
    entries: impl IntoIterator<Item = (String, String)>,
) -> crate::Result<BTreeMap<String, String>> {
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut configured = BTreeMap::<String, (String, String)>::new();
    for (key, value) in entries {
        let entry = if key.starts_with("npm:") {
            Some((key.clone(), value))
        } else {
            crate::version::ToolRequest::parse(&value)
                .ok()
                .filter(|request| request.backend.starts_with("npm:"))
                .map(|request| (request.backend, request.spec.to_string()))
        };
        let Some((backend, spec)) = entry else {
            continue;
        };
        if let Some((existing_key, existing_spec)) = configured.get(&backend) {
            if existing_spec != &spec {
                return Err(crate::error::Error::other(format!(
                    "conflicting project npm aliases for `{backend}`: `{existing_key}` configures `{existing_spec}`, but `{key}` configures `{spec}`"
                )));
            }
            continue;
        }
        configured.insert(backend, (key, spec));
    }
    Ok(configured
        .into_iter()
        .map(|(backend, (_, spec))| (backend, spec))
        .collect())
}

fn nearest_package_root(cwd: &std::path::Path) -> Option<PathBuf> {
    let root = cwd.ancestors().find_map(|directory| {
        let package_json = directory.join("package.json");
        match std::fs::symlink_metadata(&package_json) {
            Ok(metadata) => Some(
                metadata
                    .file_type()
                    .is_file()
                    .then(|| directory.to_path_buf()),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            // Fail closed when a possible nearer boundary cannot be inspected.
            Err(_) => Some(None),
        }
    })??;
    let canonical_root = dunce::canonicalize(&root).ok()?;
    let canonical_package = dunce::canonicalize(root.join("package.json")).ok()?;
    (canonical_package.is_file()
        && canonical_package
            .parent()
            .is_some_and(|parent| same_existing_path(parent, &canonical_root)))
    .then_some(canonical_root)
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

/// The variable recording which missing-tool warning this shell has shown.
pub const MISSING_WARNED_VAR: &str = "OSDK_MISSING_WARNED";

/// Shell code that records (or, with `None`, clears) the missing-tool warning
/// already shown, so the prompt hook names a missing tool once per directory
/// instead of above every prompt.
pub fn render_missing_warned(shell: Shell, value: Option<&str>) -> String {
    match (shell, value) {
        (Shell::Fish, Some(value)) => {
            format!(
                "set -gx {MISSING_WARNED_VAR} {}
",
                shell_quote(shell, value)
            )
        }
        (Shell::Fish, None) => format!(
            "set -e {MISSING_WARNED_VAR}; or true
"
        ),
        (Shell::Powershell, Some(value)) => {
            format!(
                "$env:{MISSING_WARNED_VAR} = {}
",
                powershell_quote(value)
            )
        }
        (Shell::Powershell, None) => {
            format!(
                "Remove-Item Env:{MISSING_WARNED_VAR} -ErrorAction SilentlyContinue
"
            )
        }
        (_, Some(value)) => format!(
            "export {MISSING_WARNED_VAR}={}
",
            shell_quote(shell, value)
        ),
        (_, None) => format!(
            "unset {MISSING_WARNED_VAR}
"
        ),
    }
}

/// Render `hook-env` output: shell commands that prepend PATH and set vars.
pub fn render_hook_env(shell: Shell, delta: &EnvDelta) -> String {
    let mut out = String::new();
    render_path_update(shell, &delta.path_prepend, &mut out);

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
        Shell::Fish if managed.is_empty() => out.push_str("set -e OSDK_MANAGED_ENV; or true\n"),
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

/// Render `osdk env` output for a shell: the same environment as
/// [`render_hook_env`], as plain exports.
///
/// The hook's bookkeeping (`OSDK_ORIGINAL_PATH`, `OSDK_ORIG_*`,
/// `OSDK_MANAGED_ENV`) exists so the next prompt can replace what the last one
/// set and `deactivate` can restore it. A script or CI step evaluates this
/// once and never deactivates, and those markers would make a later
/// activation in the same process believe it had already captured the
/// originals. PATH is prepended to whatever the evaluating shell has.
pub fn render_env_exports(shell: Shell, delta: &EnvDelta) -> String {
    let mut out = String::new();
    if !delta.path_prepend.is_empty() {
        match shell {
            Shell::Fish => out.push_str(&format!(
                "set -gx PATH {} $PATH\n",
                delta
                    .path_prepend
                    .iter()
                    .map(|path| shell_quote(shell, &path.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
            Shell::Powershell => {
                let joined = std::env::join_paths(&delta.path_prepend)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                out.push_str(&format!(
                    "$env:PATH = {} + [IO.Path]::PathSeparator + $env:PATH\n",
                    powershell_quote(&joined)
                ));
            }
            _ => {
                let joined = delta
                    .path_prepend
                    .iter()
                    .map(|path| posix_path(&path.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(":");
                out.push_str(&format!(
                    "export PATH={}:\"$PATH\"\n",
                    shell_quote(shell, &joined)
                ));
            }
        }
    }
    for (key, value) in &delta.set_vars {
        if valid_env_name(key) {
            match shell {
                Shell::Fish => {
                    out.push_str(&format!("set -gx {key} {}\n", shell_quote(shell, value)))
                }
                Shell::Powershell => {
                    out.push_str(&format!("$env:{key} = {}\n", powershell_quote(value)))
                }
                _ => out.push_str(&format!("export {key}={}\n", shell_quote(shell, value))),
            }
        }
    }
    out
}

/// The complete variables `delta` amounts to on top of `base_path`.
///
/// For consumers that read a file rather than evaluate a shell (an IDE's env
/// file, `docker --env-file`, JSON), which cannot say "prepend to the existing
/// PATH" and so need it spelled out in full.
pub fn env_assignments(
    delta: &EnvDelta,
    base_path: &std::ffi::OsStr,
) -> crate::Result<BTreeMap<String, String>> {
    let mut vars: BTreeMap<String, String> = delta
        .set_vars
        .iter()
        .filter(|(key, _)| valid_env_name(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let path = std::env::join_paths(
        delta
            .path_prepend
            .iter()
            .cloned()
            .chain(std::env::split_paths(base_path)),
    )
    .map_err(|error| crate::error::Error::other(format!("cannot build PATH: {error}")))?;
    vars.insert("PATH".into(), path.to_string_lossy().into_owned());
    Ok(vars)
}

/// Replace the PATH entries the previous hook added, and nothing else.
///
/// The hook used to rebuild PATH from a copy captured at first activation
/// (`OSDK_ORIGINAL_PATH`). Every later prompt then threw away whatever had
/// changed since: a Python venv activated in the shell, a directory the user
/// prepended, an entry an IDE added for the terminal it spawned. Only the
/// entries osdk itself wrote are osdk's to replace, so they are recorded in
/// `OSDK_ADDED_PATH` and the new set goes where the old one was -- a venv
/// activated after osdk stays in front of osdk's Python. With nothing recorded
/// (first activation) the new set is prepended.
///
/// An empty `paths` removes osdk's entries, which is also what deactivation
/// does. A session activated by an older osdk is migrated by restoring its
/// captured PATH once.
fn render_path_update(shell: Shell, paths: &[PathBuf], out: &mut String) {
    match shell {
        Shell::Fish => {
            out.push_str(
                "if set -q OSDK_ORIGINAL_PATH_SET\n  set -gx PATH $OSDK_ORIGINAL_PATH\n  set -e OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET; or true\nend\n",
            );
            out.push_str("set -l __osdk_new");
            for path in paths {
                out.push(' ');
                out.push_str(&shell_quote(shell, &path.display().to_string()));
            }
            out.push_str(
                "\nset -l __osdk_out\nset -l __osdk_put 0\nfor __osdk_p in $PATH\n  if test -n \"$__osdk_p\"; and set -q OSDK_ADDED_PATH; and contains -- $__osdk_p $OSDK_ADDED_PATH\n    if test $__osdk_put = 0\n      set __osdk_put 1\n      set -a __osdk_out $__osdk_new\n    end\n    continue\n  end\n  set -a __osdk_out $__osdk_p\nend\nif test $__osdk_put = 0\n  set __osdk_out $__osdk_new $__osdk_out\nend\nset -gx PATH $__osdk_out\nif set -q __osdk_new[1]\n  set -gx OSDK_ADDED_PATH $__osdk_new\nelse\n  set -e OSDK_ADDED_PATH; or true\nend\nset -e __osdk_new __osdk_out __osdk_put __osdk_p; or true\n",
            );
        }
        Shell::Powershell => {
            out.push_str(
                "if (Test-Path Env:OSDK_ORIGINAL_PATH_SET) { $env:PATH = $env:OSDK_ORIGINAL_PATH; Remove-Item Env:OSDK_ORIGINAL_PATH,Env:OSDK_ORIGINAL_PATH_SET -ErrorAction SilentlyContinue }\n",
            );
            let quoted = paths
                .iter()
                .map(|path| powershell_quote(&path.display().to_string()))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("$osdkNewPath = [string[]]@({quoted})\n"));
            out.push_str(concat!(
                "$osdkSeparator = [IO.Path]::PathSeparator\n",
                "$osdkOldPath = [string[]]@()\n",
                "if (Test-Path Env:OSDK_ADDED_PATH) { $osdkOldPath = [string[]]@($env:OSDK_ADDED_PATH.Split($osdkSeparator) | Where-Object { $_ }) }\n",
                "$osdkPathOut = [System.Collections.Generic.List[string]]::new()\n",
                "$osdkPathPut = $false\n",
                // Every entry of the user's PATH is kept exactly, empty ones
                // included; only an empty PATH contributes none.
                "foreach ($osdkPathEntry in $(if ($env:PATH) { $env:PATH.Split($osdkSeparator) } else { @() })) {\n",
                "  if ($osdkPathEntry -and ($osdkOldPath -contains $osdkPathEntry)) {\n",
                "    if (-not $osdkPathPut) { $osdkPathPut = $true; $osdkPathOut.AddRange($osdkNewPath) }\n",
                "    continue\n",
                "  }\n",
                "  $osdkPathOut.Add($osdkPathEntry)\n",
                "}\n",
                "if (-not $osdkPathPut) { $osdkPathOut.InsertRange(0, $osdkNewPath) }\n",
                "$env:PATH = $osdkPathOut -join $osdkSeparator\n",
                "if ($osdkNewPath.Count) { $env:OSDK_ADDED_PATH = $osdkNewPath -join $osdkSeparator } else { Remove-Item Env:OSDK_ADDED_PATH -ErrorAction SilentlyContinue }\n",
                "Remove-Variable osdkNewPath,osdkSeparator,osdkOldPath,osdkPathOut,osdkPathPut,osdkPathEntry -ErrorAction SilentlyContinue\n",
            ));
        }
        _ => {
            out.push_str(
                "if [ -n \"${OSDK_ORIGINAL_PATH_SET+x}\" ]; then export PATH=\"$OSDK_ORIGINAL_PATH\"; unset OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET; fi\n",
            );
            let joined = paths
                .iter()
                .map(|path| posix_path(&path.display().to_string()))
                .collect::<Vec<_>>()
                .join(":");
            out.push_str(&format!("_osdk_new={}\n", shell_quote(shell, &joined)));
            // The only per-shell difference: splitting PATH on ':' without
            // pathname expansion.
            if shell == Shell::Zsh {
                out.push_str("_osdk_parts=(\"${(@s.:.)PATH}\")\n");
            } else {
                out.push_str("IFS=: read -r -a _osdk_parts <<< \"$PATH\"\n");
            }
            out.push_str(concat!(
                "_osdk_old=\":${OSDK_ADDED_PATH-}:\"\n",
                "_osdk_out=\n",
                "_osdk_put=\n",
                "for _osdk_p in \"${_osdk_parts[@]}\"; do\n",
                "  if [ -n \"$_osdk_p\" ] && [ -n \"${OSDK_ADDED_PATH-}\" ]; then\n",
                "    case \"$_osdk_old\" in\n",
                "      *\":$_osdk_p:\"*)\n",
                "        if [ -z \"$_osdk_put\" ]; then _osdk_put=1; [ -n \"$_osdk_new\" ] && _osdk_out=\"$_osdk_out:$_osdk_new\"; fi\n",
                "        continue ;;\n",
                "    esac\n",
                "  fi\n",
                "  _osdk_out=\"$_osdk_out:$_osdk_p\"\n",
                "done\n",
                "if [ -z \"$_osdk_put\" ] && [ -n \"$_osdk_new\" ]; then _osdk_out=\":$_osdk_new$_osdk_out\"; fi\n",
                "export PATH=\"${_osdk_out#:}\"\n",
                "if [ -n \"$_osdk_new\" ]; then export OSDK_ADDED_PATH=\"$_osdk_new\"; else unset OSDK_ADDED_PATH; fi\n",
                "unset _osdk_new _osdk_parts _osdk_old _osdk_out _osdk_put _osdk_p\n",
            ));
        }
    }
}

/// Render a filesystem path for a POSIX shell.
///
/// A no-op off Windows. On Windows the POSIX shells that exist are the msys /
/// Cygwin family (Git-Bash, MSYS2), whose `$PATH` is colon-separated and whose
/// path syntax is `/c/dir`. Emitting the native `C:\dir` form would make the
/// drive letter's own colon read as a `$PATH` separator, so `C:\a:C:\b` splits
/// into the four meaningless entries `C`, `\a`, `C`, `\b` — the shell reports no
/// error and every managed tool silently disappears from `$PATH`. See
/// docs/bugs/005.
///
/// This applies to `` only, and deliberately not to the managed variables
/// (`CARGO_HOME`, `GOCACHE`, `JAVA_HOME`, ...). `` is parsed by the shell
/// itself, so it must speak the shell's dialect; those variables are read by
/// native Windows executables (`cargo.exe`, `go.exe`, the JVM launcher), which
/// understand `C:\dir` and not `/c/dir`. Converting them would break the very
/// tools this function exists to make reachable.
fn posix_path(path: &str) -> String {
    #[cfg(not(windows))]
    {
        path.to_string()
    }
    #[cfg(windows)]
    {
        windows_path_to_posix(path)
    }
}

/// Convert `C:\dir\sub` into the msys form `/c/dir/sub`.
///
/// Leaves anything that is not a drive-letter path alone: a UNC share has no
/// portable msys spelling, and an already-POSIX path must not be rewritten.
#[cfg(windows)]
fn windows_path_to_posix(path: &str) -> String {
    let bytes = path.as_bytes();
    let is_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !is_drive_path {
        return path.replace('\\', "/");
    }
    let drive = path[..1].to_ascii_lowercase();
    let rest = path[2..].replace('\\', "/");
    format!("/{drive}{rest}")
}

// Fish reports a missing optional erase as failure, and later `set` assignments
// preserve that status. Treat absent capture/restore variables as successful
// no-ops so sourcing a valid hook (or repeating deactivation) returns success.
fn render_capture_var(shell: Shell, key: &str, out: &mut String) {
    let original = format!("OSDK_ORIG_{key}");
    let present = format!("{original}_PRESENT");
    let set = format!("{original}_SET");
    match shell {
        Shell::Fish => out.push_str(&format!(
            "if not set -q {set}\n  if set -q {key}\n    set -gx {original} \"${key}\"\n    set -gx {present} 1\n  else\n    set -e {original}; or true\n    set -gx {present} 0\n  end\n  set -gx {set} 1\nend\n"
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
            "if set -q {set}\n  if test \"${present}\" = 1\n    set -gx {key} \"${original}\"\n  else\n    set -e {key}; or true\n  end\n  set -e {original} {present} {set}; or true\nend\n"
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

/// A PowerShell expression for `value`, written in ASCII only.
///
/// PowerShell decodes a native command's stdout with `[Console]::OutputEncoding`
/// -- GBK by default on Chinese Windows -- while osdk writes UTF-8. So a
/// non-ASCII path in `osdk activate pwsh` or `hook-env` output (an install
/// under `C:\Users\张三`) reached `Invoke-Expression` as mojibake: the hook's
/// `& '<osdk.exe>'` was "not recognized", PATH entries pointed nowhere, and
/// activation silently did nothing. Non-ASCII characters are spelled as
/// `[char]0x....` instead, which survives any console encoding and works in
/// Windows PowerShell 5.1 and PowerShell 7 alike.
fn powershell_quote(value: &str) -> String {
    if value.is_ascii() {
        return format!("'{}'", value.replace('\'', "''"));
    }
    let mut parts = Vec::new();
    let mut literal = String::new();
    for character in value.chars() {
        if character.is_ascii() {
            if character == '\'' {
                literal.push('\'');
            }
            literal.push(character);
            continue;
        }
        if !literal.is_empty() {
            parts.push(format!("'{literal}'"));
            literal.clear();
        }
        let code = u32::from(character);
        parts.push(if code <= 0xFFFF {
            format!("[char]0x{code:04X}")
        } else {
            format!("[char]::ConvertFromUtf32(0x{code:X})")
        });
    }
    if !literal.is_empty() {
        parts.push(format!("'{literal}'"));
    }
    // A leading `[char]` would make `+` add code points as numbers; starting
    // from an empty string keeps every `+` a concatenation.
    format!("(''+{})", parts.join("+"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::backend::Backend;
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
                global_config_path: None,
                tool_aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[test]
    fn powershell_literals_are_ascii_whatever_the_value() {
        assert_eq!(powershell_quote(r"C:\it's"), r"'C:\it''s'");
        let quoted = powershell_quote(r"C:\Users\张三's 😀");
        assert!(quoted.is_ascii(), "{quoted}");
        assert_eq!(
            quoted,
            r"(''+'C:\Users\'+[char]0x5F20+[char]0x4E09+'''s '+[char]::ConvertFromUtf32(0x1F600))"
        );
    }

    /// The form above only helps if PowerShell reads it back as the value.
    /// Skipped without pwsh (Wine), like `powershell_snippets_survive_strict_mode`.
    #[cfg(windows)]
    #[test]
    fn powershell_literals_round_trip_through_pwsh() {
        let Some(pwsh) = locate_pwsh() else {
            eprintln!("skipping: no pwsh on PATH");
            return;
        };
        let value = r"C:\Users\张三's dir\😀";
        let script = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; [Console]::Write({})",
            powershell_quote(value)
        );
        let output = std::process::Command::new(pwsh)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("spawn pwsh");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
    }

    #[test]
    fn the_prompt_hook_does_not_discard_hook_env_errors() {
        // Activation fails closed when a pinned tool is missing; with stderr
        // thrown away that failure was invisible and the environment simply
        // stayed as it was.
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Powershell] {
            let script = activation_script(shell, "osdk");
            let line = script
                .lines()
                .find(|line| line.contains("hook-env --shell"))
                .unwrap_or_else(|| panic!("{shell:?} has no hook-env call"));
            assert!(!line.contains("2>"), "{shell:?}: {line}");
        }
    }

    #[test]
    fn an_uninstalled_lazy_tool_does_not_fail_activation() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut ctx = test_ctx(temporary.path(), &[("bun", "1.4.3")]);
        let lazy: crate::config::ToolConfigEntry =
            toml::from_str::<toml::Value>("bun = { version = \"1.4.3\", lazy = true }").unwrap()
                ["bun"]
                .clone()
                .try_into()
                .unwrap();
        ctx.config.tool_configs.insert("bun".into(), lazy);

        compute_env_delta(&ctx, &Registry::new(), &project)
            .expect("a lazy tool is absent until something needs it");

        // The same pin without `lazy` is still a hard error.
        let eager = test_ctx(temporary.path(), &[("bun", "1.4.3")]);
        assert!(compute_env_delta(&eager, &Registry::new(), &project).is_err());
    }

    #[test]
    fn configured_missing_runtime_fails_instead_of_falling_back_to_system_path() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = test_ctx(temporary.path(), &[("python", "3.14.7")]);

        let error = match compute_env_delta(&ctx, &Registry::new(), &project) {
            Ok(_) => panic!("missing configured Python must fail activation"),
            Err(error) => error,
        };
        let message = error.localized();
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("3.14.7"), "{message}");

        let empty = test_ctx(temporary.path(), &[]);
        let delta = compute_env_delta(&empty, &Registry::new(), &project).unwrap();
        assert!(delta.path_prepend.is_empty());
    }

    #[test]
    #[cfg(unix)]
    #[ignore = "requires a native fish shell; set OSDK_TEST_FISH and run explicitly"]
    fn fish_hook_and_deactivation_return_success() {
        let fish = std::env::var_os("OSDK_TEST_FISH").unwrap_or_else(|| "fish".into());
        let temporary = tempfile::tempdir().unwrap();
        for set_vars in [
            BTreeMap::new(),
            BTreeMap::from([("OSDK_TEST_VALUE".into(), "managed".into())]),
        ] {
            let delta = EnvDelta {
                path_prepend: vec![],
                set_vars,
                unset_vars: vec![],
            };
            let hook = render_hook_env(Shell::Fish, &delta);
            let deactivate = deactivation_script(Shell::Fish);
            let program = format!("function apply_hook\n{hook}\nend\napply_hook; or exit 31\napply_hook; or exit 32\nfunction undo_hook\n{deactivate}\nend\nundo_hook; or exit 33\nundo_hook; or exit 34\necho FISH_RUNTIME_OK\n");
            let output = std::process::Command::new(&fish)
                .args(["--no-config", "-c", &program])
                .env_clear()
                .env("HOME", temporary.path())
                .env("PATH", "/usr/bin:/bin")
                .output()
                .expect("failed to start the requested fish shell");
            assert!(
                output.status.success(),
                "fish failed: {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("FISH_RUNTIME_OK"));
        }
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
        // ASCII-only, so it survives a non-UTF-8 console decoding it.
        assert!(powershell.contains(
            r"& (''+'C:\Program Files\'+[char]0x4E2D+[char]0x6587+' osdk\osdk.exe') hook-env"
        ));
        assert!(powershell.is_ascii());
        assert!(powershell.contains("$script:OsdkHookRunning"));
        assert!(powershell.contains("finally"));
        assert!(
            powershell.find("Invoke-OsdkHook\n").unwrap()
                < powershell.find("function global:prompt").unwrap()
        );
    }

    /// Every shell's hook must stand down inside `osdk run`.
    ///
    /// This is the half of the `-NoProfile` decision that lives in the shell
    /// snippet. osdk deliberately does *not* pass `-NoProfile` -- doing so
    /// would drop the shell-activation half of its PATH injection, which is
    /// where `JAVA_HOME` and friends come from. The profile therefore still
    /// runs inside a task, and the hook inside it has to notice and do nothing;
    /// otherwise it re-derives the environment from the config and discards the
    /// per-task `env` the runner just set.
    ///
    /// Asserted for all four shells in one test because a new shell branch is
    /// exactly the place this gets forgotten.
    #[test]
    fn every_shell_hook_stands_down_inside_a_task() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Powershell] {
            let script = activation_script(shell, "/usr/bin/osdk");
            assert!(
                script.contains("OSDK_TASK"),
                "{shell:?} hook must check OSDK_TASK before re-deriving the \
                 environment:\n{script}"
            );
        }
    }

    /// The hook must fire per prompt, not per command lookup.
    ///
    /// `PostCommandLookupAction` runs on every command *resolution*, so one
    /// pipeline or loop body re-ran the entire activation: a ten-iteration loop
    /// spawned 22 `hook-env` subprocesses at ~1.1s each, while the prompt hook
    /// used by bash/zsh/fish spawned 1. Asserting the mechanism by name is the
    /// only cheap way to catch a revert -- both versions are functionally
    /// correct, so no behavioural test goes red when the frequency regresses.
    #[test]
    fn powershell_hook_runs_on_the_prompt_not_on_every_command_lookup() {
        let script = activation_script(Shell::Powershell, "osdk");
        // Match an assignment, not the bare name: the snippet explains in a
        // comment why the per-lookup hook was abandoned, and a substring check
        // would fail on that comment while a real revert slipped through a
        // reworded one.
        assert!(
            !script.contains("PostCommandLookupAction ="),
            "per-lookup hook reinstated; a single loop would re-run activation \
             once per iteration:\n{script}"
        );
        assert!(
            script.contains("function global:prompt"),
            "hook must be installed on the prompt:\n{script}"
        );
    }

    /// Installing the hook must not cost the user their own prompt.
    ///
    /// Oh My Posh, Starship and hand-written dotfile prompts are all just a
    /// `prompt` function, so defining ours unconditionally would silently
    /// discard theirs. The wrapper has to capture and call whatever was already
    /// there, and fall back to PowerShell's builtin default when nothing was.
    #[test]
    fn powershell_hook_preserves_an_existing_user_prompt() {
        let script = activation_script(Shell::Powershell, "osdk");
        assert!(
            script.contains("Get-Command prompt"),
            "must look for an existing prompt before overwriting it:\n{script}"
        );
        assert!(
            script.contains("$script:OsdkOriginalPrompt"),
            "the existing prompt must be captured for later restore:\n{script}"
        );
        assert!(
            script.contains("& $script:OsdkOriginalPrompt"),
            "the captured prompt must still be invoked, or its output is lost:\n{script}"
        );
        // Re-sourcing the snippet must not wrap our own wrapper: that nests one
        // hook per activation and multiplies the cost we just removed.
        //
        // The probe is `Test-Path Variable:` rather than a bare read of the
        // variable, because a bare read is a terminating error under
        // `Set-StrictMode -Version Latest`. Asserting the guard *exists* is not
        // enough on its own -- a guard spelled as a bare read still reads as a
        // guard here -- so `powershell_snippets_survive_strict_mode` executes
        // both snippets under StrictMode as the load-bearing check.
        assert!(
            script.contains("if (-not (Test-Path Variable:script:OsdkOriginalPrompt))"),
            "capture must be guarded so re-activation cannot nest wrappers:\n{script}"
        );
    }

    #[test]
    fn deactivation_snippets_remove_hooks_and_restore_path() {
        let bash = deactivation_script(Shell::Bash);
        assert!(bash.contains("unset -f _osdk_hook"));
        assert!(bash.contains(
            "unset OSDK_MANAGED_ENV OSDK_ORIGINAL_PATH OSDK_ORIGINAL_PATH_SET OSDK_ADDED_PATH"
        ));

        let zsh = deactivation_script(Shell::Zsh);
        assert!(zsh.contains("precmd_functions"));
        assert!(zsh.contains("unfunction _osdk_hook"));

        let fish = deactivation_script(Shell::Fish);
        assert!(fish.contains("functions -e _osdk_hook"));

        let powershell = deactivation_script(Shell::Powershell);
        assert!(powershell.contains("PostCommandLookupAction = $null"));
        assert!(powershell.contains(r"Remove-Item Function:\Invoke-OsdkHook"));
        assert!(powershell.contains("Remove-Variable OsdkHookRunning"));
    }

    /// Deactivation must hand the user's prompt back.
    ///
    /// Removing our wrapper without restoring what it wrapped would leave the
    /// session with no `prompt` function, so a user who ran `osdk deactivate`
    /// would lose the prompt they had before activating -- a worse outcome than
    /// the slow hook we set out to fix.
    #[test]
    fn powershell_deactivation_restores_the_wrapped_prompt() {
        let script = deactivation_script(Shell::Powershell);
        assert!(
            script.contains("Set-Item Function:global:prompt $script:OsdkOriginalPrompt"),
            "the captured prompt must be reinstalled:\n{script}"
        );
        assert!(
            script.contains("Remove-Variable OsdkOriginalPrompt"),
            "the capture slot must be cleared so a later activation re-captures \
             the real prompt rather than a stale one:\n{script}"
        );
    }

    /// Both PowerShell snippets must survive `Set-StrictMode -Version Latest`.
    ///
    /// The string assertions above can only see that a guard was written; they
    /// cannot see whether it is a *legal* guard. Under StrictMode, reading a
    /// variable that was never assigned is a terminating error, so the original
    /// `if (-not $script:OsdkOriginalPrompt)` aborted activation outright -- and
    /// `windows-runtime-smoke.ps1` runs under exactly that setting, which is
    /// where it surfaced. Deactivation had the same exposure on its restore path,
    /// reachable by deactivating a session that was never activated.
    ///
    /// This runs the rendered snippets through a real `pwsh -NoProfile` with
    /// StrictMode on, so a guard that merely looks like a guard fails here.
    /// Skipped, not silently passed, when no `pwsh` is on PATH: a check that
    /// cannot run must not report success.
    #[test]
    #[cfg(windows)]
    fn powershell_snippets_survive_strict_mode() {
        let Some(pwsh) = locate_pwsh() else {
            eprintln!("skipping: no pwsh on PATH to execute the rendered snippets");
            return;
        };

        // A prompt function is deliberately absent here: that is the fresh-shell
        // case, and it is the branch that reads the capture variable first.
        let activation = activation_script(Shell::Powershell, "osdk");
        let deactivation = deactivation_script(Shell::Powershell);
        for (label, snippet) in [
            ("activation", activation.clone()),
            ("deactivation", deactivation.clone()),
            // Deactivating a never-activated session, and activating twice, are
            // both ordinary user sequences that hit the guarded reads.
            ("activation twice", format!("{activation}\n{activation}")),
            (
                "activation then deactivation",
                format!("{activation}\n{deactivation}"),
            ),
        ] {
            let program = format!(
                "Set-StrictMode -Version Latest\n$ErrorActionPreference = 'Stop'\n\
                 function global:prompt {{ 'probe> ' }}\n{snippet}\n\
                 if (-not (prompt)) {{ throw 'prompt returned nothing' }}\n\
                 if ('{label}' -eq 'activation then deactivation' -and (Test-Path Function:global:Invoke-OsdkHook)) {{ throw 'global hook was not removed' }}\n\
                 Write-Output 'STRICT_MODE_OK'\n"
            );
            let output = std::process::Command::new(&pwsh)
                .args(["-NoProfile", "-NonInteractive", "-Command", &program])
                .output()
                .expect("failed to spawn pwsh");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("STRICT_MODE_OK"),
                "the {label} snippet failed under StrictMode \
                 (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
                output.status.code()
            );
        }
    }

    /// Locate PowerShell 7 for [`powershell_snippets_survive_strict_mode`].
    ///
    /// Only `pwsh` counts. Windows PowerShell 5 differs in StrictMode details and
    /// would make this check answer a question about a different interpreter than
    /// the one the project's own scripts run under.
    #[cfg(windows)]
    fn locate_pwsh() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("pwsh.exe"))
            .find(|candidate| candidate.is_file())
    }

    /// The PATH sequence a user actually produces, run in each real shell that
    /// is available: activate, prepend a venv by hand, change project, then
    /// deactivate. The hook used to rebuild PATH from a copy captured at the
    /// first prompt, so the venv vanished at the next one; and a session left
    /// by that older hook must still migrate cleanly.
    #[test]
    fn hook_replaces_only_its_own_path_entries_in_real_shells() {
        let (base, a1, a2, b1, user, legacy) = if cfg!(windows) {
            (
                [r"X:\base\one", r"X:\base\two"],
                r"X:\osdk\a1",
                r"X:\osdk\a2",
                r"X:\osdk\b1",
                r"X:\venv\Scripts",
                r"X:\old\entry",
            )
        } else {
            (
                ["/base/one", "/base/two"],
                "/osdk/a1",
                "/osdk/a2",
                "/osdk/b1",
                "/venv/bin",
                "/old/entry",
            )
        };
        let delta = |paths: &[&str]| EnvDelta {
            path_prepend: paths.iter().map(PathBuf::from).collect(),
            set_vars: BTreeMap::new(),
            unset_vars: Vec::new(),
        };
        let mut ran = Vec::new();
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Powershell] {
            let Some(mut command) = test_shell(shell) else {
                eprintln!("skipping {shell:?}: not available");
                continue;
            };
            // POSIX shells on Windows (Git Bash) speak msys paths.
            let spell = |value: &str| {
                if matches!(shell, Shell::Bash | Shell::Zsh) {
                    posix_path(value)
                } else {
                    value.to_string()
                }
            };
            let sep = if cfg!(windows) && shell == Shell::Powershell {
                ";"
            } else {
                ":"
            };
            let join = |items: &[&str]| {
                items
                    .iter()
                    .map(|item| spell(item))
                    .collect::<Vec<_>>()
                    .join(sep)
            };
            let first = render_hook_env(shell, &delta(&[a1, a2]));
            let second = render_hook_env(shell, &delta(&[b1]));
            let undo = deactivation_script(shell);
            let script = match shell {
                Shell::Fish => format!(
                    "function h1\n{first}\nend\nfunction h2\n{second}\nend\nfunction d\n{undo}\nend\n\
                     set -gx PATH {b0} {b1x}\nh1\nset -gx PATH {user} $PATH\nh2\n\
                     echo AFTER=(string join : $PATH)\nd\necho DEACT=(string join : $PATH)\n\
                     set -gx OSDK_ORIGINAL_PATH {b0} {b1x}\nset -gx OSDK_ORIGINAL_PATH_SET 1\n\
                     set -gx PATH {legacy} {b0} {b1x}\nh2\necho LEGACY=(string join : $PATH)\n",
                    b0 = shell_quote(shell, base[0]),
                    b1x = shell_quote(shell, base[1]),
                    user = shell_quote(shell, user),
                    legacy = shell_quote(shell, legacy),
                ),
                Shell::Powershell => format!(
                    "Set-StrictMode -Version Latest\n$ErrorActionPreference = 'Stop'\n\
                     function h1 {{\n{first}\n}}\nfunction h2 {{\n{second}\n}}\nfunction d {{\n{undo}\n}}\n\
                     $env:PATH = {base_q}\nh1\n$env:PATH = {user_q} + [IO.Path]::PathSeparator + $env:PATH\nh2\n\
                     Write-Output \"AFTER=$env:PATH\"\nd\nWrite-Output \"DEACT=$env:PATH\"\n\
                     $env:OSDK_ORIGINAL_PATH = {base_q}\n$env:OSDK_ORIGINAL_PATH_SET = '1'\n\
                     $env:PATH = {legacy_q} + [IO.Path]::PathSeparator + {base_q}\nh2\nWrite-Output \"LEGACY=$env:PATH\"\n",
                    base_q = powershell_quote(&join(&base)),
                    user_q = powershell_quote(user),
                    legacy_q = powershell_quote(legacy),
                ),
                _ => format!(
                    "h1() {{\n{first}\n}}\nh2() {{\n{second}\n}}\nd() {{\n{undo}\n}}\n\
                     export PATH={base_q}\nh1\nPATH={user_q}:\"$PATH\"\nh2\n\
                     printf 'AFTER=%s\\n' \"$PATH\"\nd\nprintf 'DEACT=%s\\n' \"$PATH\"\n\
                     export OSDK_ORIGINAL_PATH={base_q}\nexport OSDK_ORIGINAL_PATH_SET=1\n\
                     PATH={legacy_q}:{base_q}\nh2\nprintf 'LEGACY=%s\\n' \"$PATH\"\n",
                    base_q = shell_quote(shell, &join(&base)),
                    user_q = shell_quote(shell, &spell(user)),
                    legacy_q = shell_quote(shell, &spell(legacy)),
                ),
            };
            match shell {
                Shell::Powershell => {
                    command.args(["-NoProfile", "-NonInteractive", "-Command", &script])
                }
                Shell::Fish => command.args(["--no-config", "-c", &script]),
                Shell::Zsh => command.args(["-f", "-c", &script]),
                Shell::Bash => command.args(["--noprofile", "--norc", "-c", &script]),
            };
            let output = command.output().expect("spawn shell");
            let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
            let line = |key: &str| {
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .unwrap_or_else(|| {
                        panic!(
                            "{shell:?} printed no {key}\nstdout:\n{stdout}\nstderr:\n{}",
                            String::from_utf8_lossy(&output.stderr)
                        )
                    })
                    .to_string()
            };
            // fish joins its list with ':' above whatever the platform.
            let expect = |items: &[&str]| {
                if shell == Shell::Fish {
                    items.join(":")
                } else {
                    join(items)
                }
            };
            assert_eq!(
                line("AFTER="),
                expect(&[user, b1, base[0], base[1]]),
                "{shell:?}"
            );
            assert_eq!(
                line("DEACT="),
                expect(&[user, base[0], base[1]]),
                "{shell:?}"
            );
            assert_eq!(
                line("LEGACY="),
                expect(&[b1, base[0], base[1]]),
                "{shell:?}"
            );
            ran.push(shell);
        }
        eprintln!("ran in {ran:?}");
    }

    /// `OSDK_TASK` is inherited by everything a task starts, including an
    /// editor or an interactive shell that outlives it. The hook used to stand
    /// down on the bare marker, so such a shell never activated again in any
    /// directory. Run in each real shell available: inside the task's root the
    /// hook still stands down; outside it the hook runs and drops the markers.
    #[test]
    fn task_marker_only_holds_inside_the_task_root_in_real_shells() {
        let temporary = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temporary.path()).unwrap();
        let project = root.join("project");
        let inside = project.join("sub");
        let outside = root.join("elsewhere");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let fake_sh = root.join("fake-osdk.sh");
        std::fs::write(
            &fake_sh,
            "#!/bin/sh\ncase \"$3\" in fish) echo 'set -gx OSDK_HOOK_RAN 1' ;; *) echo 'export OSDK_HOOK_RAN=1' ;; esac\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fake_ps1 = root.join("fake-osdk.ps1");
        std::fs::write(&fake_ps1, "Write-Output '$env:OSDK_HOOK_RAN = ''1'''\n").unwrap();

        let mut ran = Vec::new();
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Powershell] {
            let Some(mut command) = test_shell(shell) else {
                eprintln!("skipping {shell:?}: not available");
                continue;
            };
            let spell = |path: &std::path::Path| {
                let text = path.display().to_string();
                if matches!(shell, Shell::Bash | Shell::Zsh) {
                    posix_path(&text)
                } else {
                    text
                }
            };
            let script = match shell {
                Shell::Powershell => format!(
                    "Set-StrictMode -Version Latest\n$ErrorActionPreference = 'Stop'\n\
                     Set-Location -LiteralPath {inside}\n{activation}\n\
                     Write-Output \"IN=$(if ($env:OSDK_HOOK_RAN) {{ 'ran' }} else {{ 'no' }}) TASK=$(if ($env:OSDK_TASK) {{ 'kept' }} else {{ 'gone' }})\"\n\
                     Set-Location -LiteralPath {outside}\nInvoke-OsdkHook\n\
                     Write-Output \"OUT=$(if ($env:OSDK_HOOK_RAN) {{ 'ran' }} else {{ 'no' }}) TASK=$(if ($env:OSDK_TASK) {{ 'kept' }} else {{ 'gone' }})\"\n",
                    inside = powershell_quote(&inside.display().to_string()),
                    outside = powershell_quote(&outside.display().to_string()),
                    activation = activation_script(shell, &fake_ps1.display().to_string()),
                ),
                Shell::Fish => format!(
                    "cd {inside}\n{activation}\n\
                     if set -q OSDK_HOOK_RAN; set r ran; else; set r no; end\n\
                     if set -q OSDK_TASK; set t kept; else; set t gone; end\necho IN=$r TASK=$t\n\
                     cd {outside}\n_osdk_hook\n\
                     if set -q OSDK_HOOK_RAN; set r ran; else; set r no; end\n\
                     if set -q OSDK_TASK; set t kept; else; set t gone; end\necho OUT=$r TASK=$t\n",
                    inside = shell_quote(shell, &inside.display().to_string()),
                    outside = shell_quote(shell, &outside.display().to_string()),
                    activation = activation_script(shell, &fake_sh.display().to_string()),
                ),
                _ => format!(
                    "cd {inside}\n{activation}\n\
                     echo \"IN=${{OSDK_HOOK_RAN:+ran}}${{OSDK_HOOK_RAN:-no}} TASK=${{OSDK_TASK:+kept}}${{OSDK_TASK:-gone}}\"\n\
                     cd {outside}\n_osdk_hook\n\
                     echo \"OUT=${{OSDK_HOOK_RAN:+ran}}${{OSDK_HOOK_RAN:-no}} TASK=${{OSDK_TASK:+kept}}${{OSDK_TASK:-gone}}\"\n",
                    inside = shell_quote(shell, &spell(&inside)),
                    outside = shell_quote(shell, &spell(&outside)),
                    activation = activation_script(shell, &spell(&fake_sh)),
                ),
            };
            match shell {
                Shell::Powershell => {
                    command.args(["-NoProfile", "-NonInteractive", "-Command", &script])
                }
                Shell::Fish => command.args(["--no-config", "-c", &script]),
                Shell::Zsh => command.args(["-f", "-c", &script]),
                Shell::Bash => command.args(["--noprofile", "--norc", "-c", &script]),
            };
            command
                .env("OSDK_TASK", "1")
                .env("OSDK_TASK_NAME", "dev")
                .env("OSDK_TASK_ROOT", &project)
                .env_remove("OSDK_HOOK_RAN");
            let output = command.output().expect("spawn shell");
            let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
            let context = format!(
                "{shell:?}\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            // `${X:+ran}${X:-no}` prints `ran1` when set; only the prefix matters.
            let field = |key: &str| {
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .unwrap_or_else(|| panic!("no {key}: {context}"))
                    .to_string()
            };
            let inside_line = field("IN=");
            let outside_line = field("OUT=");
            assert!(
                inside_line.starts_with("no") && inside_line.ends_with("TASK=kept1")
                    || inside_line == "no TASK=kept",
                "inside the task root the hook must stand down: {context}"
            );
            assert!(
                outside_line.starts_with("ran") && outside_line.ends_with("TASK=gone"),
                "outside the task root the hook must run and drop the marker: {context}"
            );
            ran.push(shell);
        }
        eprintln!("ran in {ran:?}");
    }

    /// A shell for [`hook_replaces_only_its_own_path_entries_in_real_shells`]:
    /// `OSDK_TEST_<SHELL>` when set, else the usual name on PATH.
    fn test_shell(shell: Shell) -> Option<std::process::Command> {
        let (variable, name) = match shell {
            Shell::Bash => ("OSDK_TEST_BASH", "bash"),
            Shell::Zsh => ("OSDK_TEST_ZSH", "zsh"),
            Shell::Fish => ("OSDK_TEST_FISH", "fish"),
            Shell::Powershell => ("OSDK_TEST_PWSH", "pwsh"),
        };
        let program = std::env::var_os(variable).map(PathBuf::from).or_else(|| {
            let path = std::env::var_os("PATH")?;
            let file = if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_string()
            };
            std::env::split_paths(&path)
                .map(|dir| dir.join(&file))
                .find(|candidate| candidate.is_file())
                // `bash.exe` in System32 is the WSL launcher, not a shell here.
                .filter(|candidate| {
                    !candidate
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("system32")
                })
        })?;
        Some(std::process::Command::new(program))
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
        assert!(out.contains("_osdk_new='/x/go/bin'"));
        assert!(out.contains("export GOROOT='/x/go'"));
        assert!(out.contains("unset JAVA_HOME"));
        assert!(out.contains("export OSDK_MANAGED_ENV='GOROOT'"));

        let fish = render_hook_env(Shell::Fish, &delta);
        assert!(fish.contains("set -gx PATH"));
        assert!(fish.contains("set -gx GOROOT"));
    }

    /// The POSIX shells available on Windows are msys / Cygwin (Git-Bash,
    /// MSYS2), where `$PATH` is colon-separated. A native `E:\dir` entry makes
    /// the drive letter's colon read as a separator, so the shell silently
    /// splits it into meaningless fragments and every managed tool vanishes
    /// from `$PATH`.
    ///
    /// The pre-existing `hook_env_renders_path_and_vars` cannot catch this: it
    /// feeds already-POSIX paths, which pass unchanged on every platform. Only
    /// a native Windows path exercises the conversion. See docs/bugs/005.
    #[cfg(windows)]
    #[test]
    fn windows_posix_shells_receive_msys_paths_not_drive_letters() {
        let delta = EnvDelta {
            path_prepend: vec![
                PathBuf::from(r"E:\osdk-data\data\shims"),
                PathBuf::from(r"C:\Program Files\osdk\bin"),
            ],
            set_vars: BTreeMap::new(),
            unset_vars: Vec::new(),
        };

        for shell in [Shell::Bash, Shell::Zsh] {
            let out = render_hook_env(shell, &delta);
            let path_line = out
                .lines()
                .find(|line| line.starts_with("_osdk_new="))
                .expect("a PATH line must be rendered");
            assert!(
                path_line.contains("/e/osdk-data/data/shims"),
                "drive-letter paths must become msys paths: {path_line}"
            );
            assert!(
                path_line.contains("/c/Program Files/osdk/bin"),
                "spaces are fine inside the quoted value: {path_line}"
            );
            assert!(
                !path_line.contains(r"E:\") && !path_line.contains("E:/"),
                "a bare drive letter leaves a stray colon that splits $PATH: {path_line}"
            );
        }
    }

    /// Drive-letter conversion must not mangle paths that have no msys drive
    /// spelling, and must not rewrite something already in POSIX form.
    #[cfg(windows)]
    #[test]
    fn posix_path_conversion_only_rewrites_drive_letter_paths() {
        assert_eq!(windows_path_to_posix(r"E:\a\b"), "/e/a/b");
        assert_eq!(windows_path_to_posix("E:/a/b"), "/e/a/b");
        assert_eq!(windows_path_to_posix("/already/posix"), "/already/posix");
        // A UNC share has no portable msys drive form; only separators change.
        assert_eq!(
            windows_path_to_posix(r"\\server\share\dir"),
            "//server/share/dir"
        );
        // Too short to be a drive path: must not lose characters.
        assert_eq!(windows_path_to_posix("E:"), "E:");
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
        let project_bin = temporary.path().join(".osdk/npm-bin/generations/test/bin");
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
        let project_bin = temporary.path().join(".osdk/npm-bin/generations/test/bin");
        let alias = temporary.path().join("project-bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        symlink(&project_bin, &alias).unwrap();
        let canonical = dunce::canonicalize(&project_bin).unwrap();
        let mut paths = vec![alias, PathBuf::from("/osdk/shims")];

        prioritize_project_bin(&mut paths, canonical.clone());

        assert_eq!(paths, vec![canonical, PathBuf::from("/osdk/shims")]);
    }

    #[test]
    fn raw_project_npm_bins_are_never_activated() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path(), &[]);
        let project = temporary.path().join("project");
        let bin = project.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(project.join("package.json"), b"{}").unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            "[tools.\"npm:prettier\"]\nversion = \"3\"\ninstaller = \"pnpm\"\n",
        )
        .unwrap();
        ctx.config.tools.insert("npm:prettier".into(), "3".into());
        ctx.config.tool_origins.insert(
            "npm:prettier".into(),
            crate::config::ToolConfigOrigin::ProjectConfig(project.join("osdk.toml")),
        );

        assert!(trusted_project_npm_bin(&ctx, &project).unwrap().is_none());
        assert!(!compute_env_delta(&ctx, &Registry::new(), &project)
            .unwrap()
            .path_prepend
            .contains(&dunce::canonicalize(bin).unwrap()));
    }

    #[test]
    fn project_npm_specs_reject_conflicting_aliases_and_accept_identical_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let mut ctx = test_ctx(temporary.path(), &[]);
        ctx.config =
            crate::config::Config::load(&temporary.path().join("config/config.toml"), &project)
                .unwrap();

        let error = project_npm_configured_specs(&ctx, &config_path).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("npm:fixture-cli"), "{message}");
        assert!(message.contains("tool.alpha"), "{message}");
        assert!(message.contains("1.2.3"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");
        assert!(message.contains("2.0.0"), "{message}");

        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(
            project_npm_configured_specs(&ctx, &config_path).unwrap(),
            BTreeMap::from([("npm:fixture-cli".into(), "1.2.3".into())])
        );
    }

    #[test]
    fn activation_fails_closed_on_corrupt_inventory_with_valid_configured_dynamic_tool() {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temporary.path(), &[("npm:fixture-cli", "1.2.3")]);
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let backend =
            crate::backend::npm_package::NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let identity = crate::tool::InstallIdentity::new(
            backend.id(),
            &version.version,
            ctx.platform.to_string(),
            crate::tool::InstallScope::Isolated,
            &version.options,
            vec![crate::tool::InstallDependency {
                kind: crate::tool::InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "1.0.0".into(),
                identity: None,
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let valid_root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let mut manifest = crate::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins.push(crate::inventory::DynamicToolBin {
            name: "fixture-cli".into(),
            path: "bin/fixture-cli".into(),
            ..Default::default()
        });
        manifest.write_atomic(&valid_root).unwrap();
        std::fs::create_dir_all(valid_root.join("bin")).unwrap();
        std::fs::write(valid_root.join("bin/fixture-cli"), b"fixture").unwrap();
        std::fs::write(valid_root.join(".osdk-complete"), b"").unwrap();

        let corrupt_root = ctx.dirs.installs.join("github/corrupt/tool/1.0.0");
        std::fs::create_dir_all(&corrupt_root).unwrap();
        std::fs::write(
            crate::inventory::DynamicToolManifest::manifest_path(&corrupt_root),
            b"{not valid json",
        )
        .unwrap();

        let error = match compute_env_delta(&ctx, &Registry::new(), &project) {
            Ok(_) => panic!("corrupt inventory unexpectedly produced an activation delta"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("refusing dynamic tool inventory scan"),
            "{message}"
        );
        assert!(message.contains("corrupt"), "{message}");
        assert!(
            message.contains(crate::inventory::INVENTORY_FILE),
            "{message}"
        );
    }

    fn configured_dynamic_fixture(
        root: &std::path::Path,
        configured_options: BTreeMap<String, crate::config::ToolConfigValue>,
        manifest_options: BTreeMap<String, String>,
    ) -> (Ctx, std::path::PathBuf, std::path::PathBuf) {
        let mut ctx = test_ctx(root, &[("npm:fixture-cli", "1.2.3")]);
        let mut configured_options = configured_options;
        configured_options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            crate::config::ToolConfigValue::String("1.0.0".into()),
        );
        ctx.config.tool_configs.insert(
            "npm:fixture-cli".into(),
            crate::config::ToolConfigEntry::structured("1.2.3", configured_options),
        );
        let backend =
            crate::backend::npm_package::NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options = manifest_options;
        version.options.insert(
            crate::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "1.0.0".into(),
        );
        let identity = crate::tool::InstallIdentity::new(
            backend.id(),
            &version.version,
            ctx.platform.to_string(),
            crate::tool::InstallScope::Isolated,
            &version.options,
            vec![crate::tool::InstallDependency {
                kind: crate::tool::InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "1.0.0".into(),
                identity: None,
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let install_root = crate::dirs::InstallLocator::new(&ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let bin = install_root.join("bin/fixture-cli");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();
        let mut manifest = crate::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins.push(crate::inventory::DynamicToolBin {
            name: "fixture-cli".into(),
            path: "bin/fixture-cli".into(),
            ..Default::default()
        });
        manifest.write_atomic(&install_root).unwrap();
        (ctx, install_root, bin)
    }

    fn activation_error(ctx: &Ctx, project: &std::path::Path) -> String {
        match compute_env_delta(ctx, &Registry::new(), project) {
            Ok(_) => panic!("invalid dynamic install unexpectedly produced an activation delta"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn activation_rejects_configured_dynamic_install_without_inventory() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (ctx, install_root, bin) =
            configured_dynamic_fixture(temporary.path(), BTreeMap::new(), BTreeMap::new());
        std::fs::remove_file(crate::inventory::DynamicToolManifest::manifest_path(
            &install_root,
        ))
        .unwrap();

        let message = activation_error(&ctx, &project);
        assert!(
            message.contains("missing or invalid install identity"),
            "{message}"
        );
        assert!(message.contains("reinstall"), "{message}");
        assert!(
            bin.exists(),
            "fixture must retain the otherwise runnable bin"
        );
    }

    #[test]
    fn activation_rejects_configured_dynamic_install_without_completion_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (ctx, install_root, bin) =
            configured_dynamic_fixture(temporary.path(), BTreeMap::new(), BTreeMap::new());
        std::fs::remove_file(install_root.join(".osdk-complete")).unwrap();

        let message = activation_error(&ctx, &project);
        assert!(
            message.contains("no complete selected install"),
            "{message}"
        );
        assert!(message.contains("reinstall"), "{message}");
        assert!(
            bin.exists(),
            "fixture must retain the otherwise runnable bin"
        );
    }

    #[test]
    fn activation_rejects_legacy_dynamic_inventory() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (ctx, install_root, _) =
            configured_dynamic_fixture(temporary.path(), BTreeMap::new(), BTreeMap::new());
        std::fs::remove_file(crate::inventory::DynamicToolManifest::manifest_path(
            &install_root,
        ))
        .unwrap();
        std::fs::write(
            install_root.join(crate::inventory::LEGACY_INVENTORY_FILE),
            r#"{"schema":2,"id":"npm:fixture-cli","version":"1.2.3","bins":[{"name":"fixture-cli","path":"bin/fixture-cli"}]}"#,
        )
        .unwrap();

        let message = activation_error(&ctx, &project);
        assert!(
            message.contains("missing or invalid install identity"),
            "{message}"
        );
        assert!(message.contains("reinstall"), "{message}");
    }

    #[test]
    fn activation_rejects_configured_dynamic_option_mismatch() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (ctx, _, _) = configured_dynamic_fixture(
            temporary.path(),
            BTreeMap::from([(
                "installer".into(),
                crate::config::ToolConfigValue::String("pnpm".into()),
            )]),
            BTreeMap::from([("installer".into(), "npm".into())]),
        );

        let message = activation_error(&ctx, &project);
        assert!(
            message.contains("no complete selected install"),
            "{message}"
        );
        assert!(message.contains("reinstall"), "{message}");
    }

    #[test]
    fn activation_rejects_configured_dynamic_version_mismatch() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (ctx, install_root, _) =
            configured_dynamic_fixture(temporary.path(), BTreeMap::new(), BTreeMap::new());
        let old_manifest = crate::inventory::DynamicToolManifest::load(&install_root).unwrap();
        let identity = crate::tool::InstallIdentity::new(
            &old_manifest.identity.tool,
            "9.9.9",
            &old_manifest.identity.platform,
            old_manifest.identity.scope,
            &old_manifest.identity.material_options,
            old_manifest.identity.dependencies,
            old_manifest.identity.materials,
        )
        .unwrap();
        let mut manifest = crate::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = old_manifest.bins;
        let path = crate::inventory::DynamicToolManifest::manifest_path(&install_root);
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        let message = activation_error(&ctx, &project);
        assert!(
            message.contains("refusing dynamic tool inventory scan"),
            "{message}"
        );
        assert!(message.contains("identity root"), "{message}");
    }

    #[test]
    fn http_activation_uses_inventory_offline_and_rejects_receipt_tampering() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let backend = "http:https://downloads.example.test/tool-{version}";
        let receipt_url = "https://downloads.example.test/tool-1.2.3";
        let digest = "a".repeat(64);
        let mut ctx = test_ctx(temporary.path(), &[(backend, "1.2.3")]);
        let options: BTreeMap<String, String> = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "file".into()),
            ("rename".into(), "fixture-http".into()),
        ]);
        ctx.config.tool_configs.insert(
            backend.into(),
            crate::config::ToolConfigEntry::structured(
                "1.2.3",
                options
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            crate::config::ToolConfigValue::String(value.clone()),
                        )
                    })
                    .collect(),
            ),
        );
        ctx.config.settings.offline = true;
        let mut version = ToolVersion::new(backend, "1.2.3");
        version.options = options;
        version.options.extend(BTreeMap::from([
            (
                crate::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                receipt_url.into(),
            ),
            (
                crate::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "tool-1.2.3".into(),
            ),
            (
                crate::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]));
        let locator = crate::backend::http::HttpBackend::install_locator_for(
            &ctx.dirs,
            ctx.platform,
            backend,
            &version,
        )
        .unwrap();
        let root = locator.install_root();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture-http"), b"fixture").unwrap();
        let mut manifest =
            crate::inventory::DynamicToolManifest::from_identity(locator.identity().clone())
                .unwrap();
        manifest.bins.push(crate::inventory::DynamicToolBin {
            name: "fixture-http".into(),
            path: "bin/fixture-http".into(),
            ..Default::default()
        });
        manifest.write_atomic(root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            format!(
                r#"{{"url":{receipt_url:?},"file_name":"tool-1.2.3","checksum":"sha256:{digest}"}}"#
            ),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        std::fs::create_dir_all(ctx.dirs.shims()).unwrap();
        std::fs::write(ctx.dirs.shims().join("fixture-http"), b"shim").unwrap();

        let delta = compute_env_delta(&ctx, &Registry::new(), &project).unwrap();
        let expected_bin = root.join("bin");
        assert!(delta
            .path_prepend
            .iter()
            .any(|path| same_existing_path(path, &expected_bin)));

        let receipt_path = root.join(".osdk-artifact.json");
        let tampered = std::fs::read_to_string(&receipt_path).unwrap().replace(
            receipt_url,
            "https://downloads.example.test/substitute-1.2.3",
        );
        std::fs::write(receipt_path, tampered).unwrap();
        let message = activation_error(&ctx, &project);
        assert!(message.contains("receipt"), "{message}");
    }

    /// Install a configured `http:` tool exporting `bins` as
    /// `(relative path, name, owned)` and return its install root.
    fn http_install_fixture(
        ctx: &mut Ctx,
        backend: &str,
        bins: &[(&str, &str, bool)],
    ) -> std::path::PathBuf {
        let receipt_url = backend
            .trim_start_matches("http:")
            .replace("{version}", "1.2.3");
        let file_name = receipt_url.rsplit('/').next().unwrap().to_string();
        let digest = "a".repeat(64);
        let options: BTreeMap<String, String> = BTreeMap::from([
            ("sha256".into(), digest.clone()),
            ("kind".into(), "file".into()),
            ("rename".into(), bins[0].1.into()),
        ]);
        ctx.config.tools.insert(backend.into(), "1.2.3".into());
        ctx.config.tool_configs.insert(
            backend.into(),
            crate::config::ToolConfigEntry::structured(
                "1.2.3",
                options
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            crate::config::ToolConfigValue::String(value.clone()),
                        )
                    })
                    .collect(),
            ),
        );
        let mut version = ToolVersion::new(backend, "1.2.3");
        version.options = options;
        version.options.extend(BTreeMap::from([
            (
                crate::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                receipt_url.clone(),
            ),
            (
                crate::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.clone(),
            ),
            (
                crate::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]));
        let locator = crate::backend::http::HttpBackend::install_locator_for(
            &ctx.dirs,
            ctx.platform,
            backend,
            &version,
        )
        .unwrap();
        let root = locator.install_root().to_path_buf();
        let mut manifest =
            crate::inventory::DynamicToolManifest::from_identity(locator.identity().clone())
                .unwrap();
        for (path, name, owned) in bins {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"fixture").unwrap();
            manifest.bins.push(crate::inventory::DynamicToolBin {
                name: (*name).into(),
                path: (*path).into(),
                owned: *owned,
            });
        }
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            format!(
                r#"{{"url":{receipt_url:?},"file_name":{file_name:?},"checksum":"sha256:{digest}"}}"#
            ),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        std::fs::create_dir_all(ctx.dirs.shims()).unwrap();
        for (_, name, owned) in bins {
            if *owned {
                std::fs::write(ctx.dirs.shims().join(name), b"shim").unwrap();
            }
        }
        root
    }

    /// Regression: `osdk use -g conda:ffmpeg` put the whole conda prefix on
    /// PATH, so an activated shell resolved `python` to the closure's Python
    /// and `openssl` to the closure's openssl, although the shim layer had
    /// withheld both. Two closure installs, because one alone cannot show
    /// that each keeps its dependencies to itself, and a plain install as the
    /// control that activation still exposes directories at all.
    #[test]
    fn activation_reaches_dependency_closures_only_through_shims() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut ctx = test_ctx(temporary.path(), &[]);
        ctx.config.settings.offline = true;
        let media = http_install_fixture(
            &mut ctx,
            "http:https://downloads.example.test/media-{version}",
            &[
                ("Library/bin/media", "media", true),
                ("Library/bin/openssl", "openssl", false),
                ("python", "python", false),
            ],
        );
        let build = http_install_fixture(
            &mut ctx,
            "http:https://downloads.example.test/build-{version}",
            &[
                ("Library/bin/build", "build", true),
                ("Scripts/pydoc", "pydoc", false),
            ],
        );
        let plain = http_install_fixture(
            &mut ctx,
            "http:https://downloads.example.test/plain-{version}",
            &[("bin/plain", "plain", true)],
        );

        let delta = compute_env_delta(&ctx, &Registry::new(), &project).unwrap();
        for leaked in [
            media.clone(),
            media.join("Library/bin"),
            build.join("Library/bin"),
            build.join("Scripts"),
        ] {
            assert!(
                !delta
                    .path_prepend
                    .iter()
                    .any(|path| same_existing_path(path, &leaked)),
                "{} exposes a dependency closure on PATH: {:?}",
                leaked.display(),
                delta.path_prepend
            );
        }
        assert!(
            delta
                .path_prepend
                .iter()
                .any(|path| same_existing_path(path, &plain.join("bin"))),
            "an install without a closure keeps its directory: {:?}",
            delta.path_prepend
        );
        assert_eq!(
            delta.path_prepend.first(),
            Some(&ctx.dirs.shims()),
            "closure commands are reached through the shims, which must lead"
        );
    }

    fn one_shot_delta() -> EnvDelta {
        EnvDelta {
            path_prepend: vec![
                PathBuf::from("/osdk/shims"),
                PathBuf::from("/osdk/node/bin"),
            ],
            set_vars: BTreeMap::from([
                ("JAVA_HOME".into(), "/jdk's home".into()),
                ("not a name".into(), "dropped".into()),
            ]),
            unset_vars: vec!["STALE".into()],
        }
    }

    /// `osdk env` is evaluated once and never deactivated, so it carries the
    /// hook's variables without the hook's bookkeeping, in every shell.
    #[test]
    fn env_exports_carry_the_hook_variables_without_its_bookkeeping() {
        let delta = one_shot_delta();
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Powershell] {
            let exports = render_env_exports(shell, &delta);
            let hook = render_hook_env(shell, &delta);
            assert!(hook.contains("OSDK_ADDED_PATH"), "control: {hook}");
            assert!(
                !exports.contains("OSDK_"),
                "{shell:?} exports activation bookkeeping: {exports}"
            );
            assert!(!exports.contains("dropped"), "{exports}");
            assert!(!exports.contains("STALE"), "{exports}");
        }
        assert!(render_env_exports(Shell::Bash, &delta)
            .contains(r#"export JAVA_HOME='/jdk'\''s home'"#));
        assert!(render_env_exports(Shell::Powershell, &delta)
            .contains("$env:JAVA_HOME = '/jdk''s home'"));
        assert!(render_env_exports(Shell::Fish, &delta)
            .contains("set -gx PATH '/osdk/shims' '/osdk/node/bin' $PATH"));
        assert!(render_env_exports(Shell::Bash, &delta)
            .ends_with("export JAVA_HOME='/jdk'\\''s home'\n"));
        let no_path = EnvDelta {
            path_prepend: Vec::new(),
            ..one_shot_delta()
        };
        assert!(!render_env_exports(Shell::Bash, &no_path).contains("PATH"));
    }

    #[test]
    fn env_assignments_spell_out_path_with_managed_entries_first() {
        let delta = one_shot_delta();
        let base = std::env::join_paths([PathBuf::from("/usr/bin")]).unwrap();
        let vars = env_assignments(&delta, &base).unwrap();
        assert_eq!(
            std::env::split_paths(&vars["PATH"]).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/osdk/shims"),
                PathBuf::from("/osdk/node/bin"),
                PathBuf::from("/usr/bin"),
            ]
        );
        assert_eq!(vars["JAVA_HOME"], "/jdk's home");
        assert!(!vars.contains_key("not a name"), "{vars:?}");
        assert!(!vars.contains_key("STALE"), "{vars:?}");
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
        assert!(out.contains("_osdk_new=''"));
        assert!(out.contains("unset GOROOT"));
        assert!(out.contains("unset OSDK_MANAGED_ENV"));
    }

    /// A hand-written npm config activates its curated bin without a trust
    /// record, because declaring an npm tool no longer requires one.
    ///
    /// The regression this guards against is silent: gating activation on a
    /// positive `is_trusted` record means a config that needs no review has no
    /// record, so the curated launchers vanish from PATH with no error. A probe
    /// that only checks "did any path appear" cannot see it -- an unpublished
    /// generation looks identical -- so this test publishes a real generation
    /// first, making the absence attributable to the gate alone.
    #[cfg(unix)]
    #[test]
    fn ungoverned_npm_config_activates_without_a_trust_record() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(project.join("package.json"), "{}").unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(&config_path, "[tools]\nformatter = \"npm:prettier@^3.6\"\n").unwrap();

        // Nothing in this config is governed, so there is no record to find.
        assert!(!crate::trust::requires_trust(&config_path).unwrap());

        // Mirror the npm module's own fixture: the `bin` field is what makes
        // the executable validatable, and without it publication fails.
        let target = project.join("node_modules/prettier/bin/tool.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            project.join("node_modules/prettier/package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "prettier",
                "version": "3.6.2",
                "bin": { "prettier": "bin/tool.js" }
            }))
            .unwrap(),
        )
        .unwrap();
        symlink(&target, project.join("node_modules/.bin/prettier")).unwrap();

        let selection = crate::backend::npm_package::ProjectNpmBinSelection {
            backend: "npm:prettier".into(),
            configured_spec: "^3.6".into(),
            version: "3.6.2".into(),
        };
        let configured =
            BTreeMap::from([(selection.backend.clone(), selection.configured_spec.clone())]);
        let published = crate::backend::npm_package::publish_project_bin_generation(
            &project,
            &selection,
            &configured,
        )
        .unwrap();

        let mut ctx = test_ctx(temporary.path(), &[("formatter", "npm:prettier@^3.6")]);
        ctx.config.tool_origins.insert(
            "formatter".into(),
            crate::config::ToolConfigOrigin::ProjectConfig(config_path.clone()),
        );

        let activated = trusted_project_npm_bin(&ctx, &project).unwrap();
        assert_eq!(
            activated,
            Some(published),
            "an ungoverned npm config must still activate its curated bin"
        );

        // The record-based path must still be what clears a governed config:
        // adding a governed key withholds activation until it is approved.
        std::fs::write(
            &config_path,
            "[tools]\nformatter = \"npm:prettier@^3.6\"\n\n[settings]\nverify_signatures = false\n",
        )
        .unwrap();
        assert!(crate::trust::requires_trust(&config_path).unwrap());
        assert_eq!(
            trusted_project_npm_bin(&ctx, &project).unwrap(),
            None,
            "a governed config must not activate until it is trusted"
        );

        crate::trust::trust(&ctx.dirs.config, &config_path).unwrap();
        assert!(
            trusted_project_npm_bin(&ctx, &project).unwrap().is_some(),
            "approving the governed config restores activation"
        );
    }
}
