//! Shell activation.
//!
//! Two mechanisms (both offered, like mise):
//! - Shims (default): the shims dir on PATH; robust in IDEs/CI. Set up by
//!   `osdk` itself when tools are installed.
//! - Shell activation (`osdk activate <shell>`): injects a hook that runs
//!   `osdk hook-env` on each prompt / dir change, rewriting PATH to the active
//!   versions' bin dirs and exporting their env (GOROOT/JAVA_HOME/...).
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
"#,
            bin = osdk_bin
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
"#,
            bin = osdk_bin
        ),
        Shell::Fish => format!(
            r#"# osdk shell integration (fish)
function _osdk_hook --on-variable PWD --on-event fish_prompt
  {bin} hook-env --shell fish 2>/dev/null | source
end
"#,
            bin = osdk_bin
        ),
        Shell::Powershell => format!(
            r#"# osdk shell integration (powershell)
function Invoke-OsdkHook {{
  $out = & {bin} hook-env --shell powershell 2>$null
  if ($out) {{ Invoke-Expression ($out -join "`n") }}
}}
$ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = {{ Invoke-OsdkHook }}
Invoke-OsdkHook
"#,
            bin = osdk_bin
        ),
    }
}

/// The env changes to apply for the active toolset in `cwd`.
pub struct EnvDelta {
    /// Directories to prepend to PATH (active tools' bin dirs).
    pub path_prepend: Vec<PathBuf>,
    /// Variables to set (GOROOT, JAVA_HOME, ...).
    pub set_vars: BTreeMap<String, String>,
}

/// Compute the env delta for the directory `cwd`: for each backend with an
/// active + installed version, collect its bin dirs and exec env.
pub fn compute_env_delta(ctx: &Ctx, registry: &Registry, cwd: &std::path::Path) -> EnvDelta {
    let mut path_prepend = Vec::new();
    let mut set_vars = BTreeMap::new();

    for backend in registry.all() {
        let active = match resolve_active(
            backend.id(),
            cwd,
            &ctx.config.tools,
            backend.idiomatic_files(),
        ) {
            Some(a) => a,
            None => continue,
        };
        // Resolve to an installed version.
        let installed = backend.list_installed(ctx).unwrap_or_default();
        if installed.is_empty() {
            continue;
        }
        let spec = strip_distribution_prefix(&active.spec);
        let parsed = VersionSpec::parse(spec);
        let version = match &parsed {
            VersionSpec::Exact(v) if installed.iter().any(|i| i == v) => Some(v.clone()),
            _ => {
                let infos: Vec<VersionInfo> = installed.iter().map(VersionInfo::stable).collect();
                select_version(&parsed, &infos).map(|vi| vi.version.clone())
            }
        };
        let version = match version {
            Some(v) => v,
            None => continue,
        };
        let tv = ToolVersion::new(backend.id(), &version);
        if let Ok(bins) = backend.bin_paths(ctx, &tv) {
            for b in bins {
                if b.exists() {
                    path_prepend.push(b);
                }
            }
        }
        if let Ok(env) = backend.exec_env(ctx, &tv) {
            for (k, v) in env {
                set_vars.insert(k, v);
            }
        }
    }

    EnvDelta {
        path_prepend,
        set_vars,
    }
}

/// Render `hook-env` output: shell commands that prepend PATH and set vars.
pub fn render_hook_env(shell: Shell, delta: &EnvDelta) -> String {
    let mut out = String::new();
    // PATH prepend.
    if !delta.path_prepend.is_empty() {
        let joined = delta
            .path_prepend
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(path_sep(shell));
        match shell {
            Shell::Fish => {
                out.push_str(&format!(
                    "set -gx PATH {} $PATH\n",
                    delta
                        .path_prepend
                        .iter()
                        .map(|p| shell_quote(shell, &p.display().to_string()))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            Shell::Powershell => {
                out.push_str(&format!(
                    "$env:PATH = \"{}\" + [IO.Path]::PathSeparator + $env:PATH\n",
                    joined
                ));
            }
            _ => {
                out.push_str(&format!(
                    "export PATH={}:$PATH\n",
                    shell_quote(shell, &joined)
                ));
            }
        }
    }
    // set vars
    for (k, v) in &delta.set_vars {
        match shell {
            Shell::Fish => out.push_str(&format!("set -gx {} {}\n", k, shell_quote(shell, v))),
            Shell::Powershell => out.push_str(&format!("$env:{} = \"{}\"\n", k, v)),
            _ => out.push_str(&format!("export {}={}\n", k, shell_quote(shell, v))),
        }
    }
    out
}

fn path_sep(shell: Shell) -> &'static str {
    match shell {
        Shell::Powershell if cfg!(windows) => ";",
        _ => ":",
    }
}

fn shell_quote(shell: Shell, s: &str) -> String {
    match shell {
        Shell::Powershell => s.to_string(),
        _ => {
            // single-quote for POSIX/fish, escaping embedded quotes
            let escaped = s.replace('\'', r"'\''");
            format!("'{escaped}'")
        }
    }
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
    use super::*;

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
    }

    #[test]
    fn hook_env_renders_path_and_vars() {
        let mut set_vars = BTreeMap::new();
        set_vars.insert("GOROOT".to_string(), "/x/go".to_string());
        let delta = EnvDelta {
            path_prepend: vec![PathBuf::from("/x/go/bin")],
            set_vars,
        };
        let out = render_hook_env(Shell::Bash, &delta);
        assert!(out.contains("export PATH='/x/go/bin':$PATH"));
        assert!(out.contains("export GOROOT='/x/go'"));

        let fish = render_hook_env(Shell::Fish, &delta);
        assert!(fish.contains("set -gx PATH"));
        assert!(fish.contains("set -gx GOROOT"));
    }
}
