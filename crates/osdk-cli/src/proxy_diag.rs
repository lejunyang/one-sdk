//! Proxy diagnostics.
//!
//! osdk's HTTP client is reqwest. On every platform reqwest derives its proxy
//! configuration from **environment variables** (`HTTPS_PROXY` / `HTTP_PROXY` /
//! `ALL_PROXY` and their lowercase forms). It deliberately does not consult a
//! desktop "system proxy":
//!
//! * on Windows the Settings / Internet Options toggle is a WinINET setting in
//!   the user registry, which browsers read but reqwest never does;
//! * on macOS the equivalent is the System Configuration framework;
//! * on Linux there is no single system proxy at all.
//!
//! So a Windows user who enabled "use a proxy server" in Settings has a working
//! browser and a tool that times out, with nothing pointing at the cause. This
//! module gathers the facts and the CLI reports them; it never silently adopts
//! the system proxy, because that would change where traffic goes and could
//! bypass an intentionally proxy-free configuration.

/// What the environment tells reqwest to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvProxy {
    /// At least one proxy variable is set to a non-empty value.
    Configured { vars: Vec<String> },
    /// Relevant variables exist but are all empty.
    ExplicitlyEmpty,
    /// No relevant variable is present.
    Absent,
}

/// The Windows WinINET settings, on Windows only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowsProxySettings {
    /// `ProxyEnable == 1`.
    pub enabled: bool,
    /// The redacted `ProxyServer` value, if any.
    pub server: Option<String>,
    /// A PAC URL (`AutoConfigURL`) is configured.
    pub pac: bool,
}

/// The advice the doctor command should act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAdvice {
    /// reqwest already has a proxy from the environment; nothing to warn about.
    EnvConfigured { vars: Vec<String> },
    /// Windows: the desktop proxy is on but no environment proxy is set, so
    /// osdk ignores the proxy the user can see working in their browser.
    WindowsSystemProxyIgnored { server: Option<String>, pac: bool },
    /// No proxy configured anywhere that we can see.
    NoneConfigured,
}

const PROXY_ENV_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Read the proxy state from an arbitrary variable source, so the logic is
/// testable without mutating process-global environment.
///
/// A value counts as configured only when it is non-empty after trimming; an
/// exported-but-empty variable explicitly means "no proxy" and must not be
/// mistaken for a configured proxy (the same empty-is-unset convention the rest
/// of the codebase uses).
pub fn env_proxy_from(mut get: impl FnMut(&str) -> Option<String>) -> EnvProxy {
    let mut configured = Vec::new();
    let mut present_but_empty = false;
    for name in PROXY_ENV_VARS {
        match get(name) {
            Some(value) if !value.trim().is_empty() => {
                // Display the canonical uppercase name once. On Windows the
                // process environment is case-insensitive, so setting
                // `HTTPS_PROXY` also answers a lookup for `https_proxy`; listing
                // both would report one proxy as if it were two.
                let canonical = name.to_ascii_uppercase();
                if !configured.contains(&canonical) {
                    configured.push(canonical);
                }
            }
            Some(_) => present_but_empty = true,
            None => {}
        }
    }
    if !configured.is_empty() {
        EnvProxy::Configured { vars: configured }
    } else if present_but_empty {
        EnvProxy::ExplicitlyEmpty
    } else {
        EnvProxy::Absent
    }
}

/// The process environment's proxy state.
pub fn env_proxy() -> EnvProxy {
    env_proxy_from(|name| std::env::var(name).ok())
}

/// Combine environment state with the platform's desktop/WinINET setting.
///
/// `windows` is `None` off Windows.
pub fn advise(env: &EnvProxy, windows: Option<&WindowsProxySettings>) -> ProxyAdvice {
    match env {
        EnvProxy::Configured { vars } => ProxyAdvice::EnvConfigured { vars: vars.clone() },
        // An explicit empty variable wins: it means the user deliberately
        // disabled proxies for child processes, so do not nag them about the
        // desktop setting they chose not to export.
        EnvProxy::ExplicitlyEmpty => ProxyAdvice::NoneConfigured,
        EnvProxy::Absent => match windows {
            Some(settings) if settings.enabled || settings.pac => {
                ProxyAdvice::WindowsSystemProxyIgnored {
                    server: settings.server.clone(),
                    pac: settings.pac,
                }
            }
            _ => ProxyAdvice::NoneConfigured,
        },
    }
}

/// Remove embedded credentials from a proxy value before displaying it.
///
/// `ProxyServer` is normally `host:port`, but it can carry
/// `scheme://user:pass@host:port`. Doctor output may be pasted into an issue, so
/// the secret must not survive. The host is what tells the user which proxy is
/// meant.
pub fn redact_proxy(value: &str) -> String {
    let after_scheme = match value.split_once("://") {
        Some((scheme, rest)) => {
            // Keep the scheme, drop everything up to and including the last `@`.
            match rest.rfind('@') {
                Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
                None => value.to_string(),
            }
        }
        None => match value.rfind('@') {
            Some(at) => value[at + 1..].to_string(),
            None => value.to_string(),
        },
    };
    after_scheme.trim().to_string()
}

/// Read the current user's WinINET proxy settings from the registry.
#[cfg(windows)]
pub fn windows_proxy_settings() -> Option<WindowsProxySettings> {
    #[path = "proxy_diag_windows.rs"]
    mod windows;
    windows::read_internet_settings()
}

#[cfg(not(windows))]
pub fn windows_proxy_settings() -> Option<WindowsProxySettings> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn a_non_empty_proxy_variable_is_configured() {
        assert_eq!(
            env_proxy_from(vars(&[("HTTPS_PROXY", "http://127.0.0.1:7897")])),
            EnvProxy::Configured {
                vars: vec!["HTTPS_PROXY".to_string()]
            }
        );
    }

    #[test]
    fn only_uppercase_and_lowercase_forms_are_recognised() {
        // An unrelated variable must not be mistaken for a proxy.
        let state = env_proxy_from(vars(&[("PROXY", "http://x"), ("NO_PROXY", "localhost")]));
        assert_eq!(state, EnvProxy::Absent);
    }

    #[test]
    fn an_empty_variable_means_explicitly_unset_not_configured() {
        assert_eq!(
            env_proxy_from(vars(&[("HTTPS_PROXY", "   ")])),
            EnvProxy::ExplicitlyEmpty
        );
    }

    #[test]
    fn configured_env_silences_the_windows_warning() {
        let env = EnvProxy::Configured {
            vars: vec!["HTTPS_PROXY".to_string()],
        };
        let win = WindowsProxySettings {
            enabled: true,
            server: Some("127.0.0.1:7897".into()),
            pac: false,
        };
        assert_eq!(
            advise(&env, Some(&win)),
            ProxyAdvice::EnvConfigured {
                vars: vec!["HTTPS_PROXY".to_string()]
            }
        );
    }

    #[test]
    fn an_enabled_windows_proxy_without_env_is_flagged() {
        assert_eq!(
            advise(
                &EnvProxy::Absent,
                Some(&WindowsProxySettings {
                    enabled: true,
                    server: Some("127.0.0.1:7897".into()),
                    pac: false,
                })
            ),
            ProxyAdvice::WindowsSystemProxyIgnored {
                server: Some("127.0.0.1:7897".into()),
                pac: false,
            }
        );
    }

    #[test]
    fn pac_alone_is_flagged_even_when_proxyenable_is_off() {
        assert_eq!(
            advise(
                &EnvProxy::Absent,
                Some(&WindowsProxySettings {
                    enabled: false,
                    server: None,
                    pac: true,
                })
            ),
            ProxyAdvice::WindowsSystemProxyIgnored {
                server: None,
                pac: true,
            }
        );
    }

    #[test]
    fn explicitly_empty_env_does_not_bring_back_the_warning() {
        assert_eq!(
            advise(
                &EnvProxy::ExplicitlyEmpty,
                Some(&WindowsProxySettings {
                    enabled: true,
                    server: Some("127.0.0.1:7897".into()),
                    pac: false,
                })
            ),
            ProxyAdvice::NoneConfigured
        );
    }

    #[test]
    fn off_windows_or_disabled_proxy_is_none_configured() {
        assert_eq!(advise(&EnvProxy::Absent, None), ProxyAdvice::NoneConfigured);
        assert_eq!(
            advise(&EnvProxy::Absent, Some(&WindowsProxySettings::default())),
            ProxyAdvice::NoneConfigured
        );
    }

    #[test]
    fn credentials_in_a_proxy_value_are_redacted() {
        assert_eq!(
            redact_proxy("http://user:secret@proxy.corp:8080"),
            "http://proxy.corp:8080"
        );
        assert_eq!(
            redact_proxy("user:secret@proxy.corp:8080"),
            "proxy.corp:8080"
        );
        assert_eq!(redact_proxy("127.0.0.1:7897"), "127.0.0.1:7897");
    }
}
