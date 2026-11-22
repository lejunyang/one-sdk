//! Native index settings that a standalone pypi tool can use for both
//! anonymous version discovery and installer execution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::pypi::EnvCreator;
use crate::error::{Error, Result};

const UV_DEFAULT: [&str; 2] = ["UV_DEFAULT_INDEX", "UV_INDEX_URL"];
const UV_EXTRA: [&str; 2] = ["UV_INDEX", "UV_EXTRA_INDEX_URL"];

pub(super) fn index(creator: EnvCreator) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let home = crate::python_index::home_directory();
    let getenv = |key: &str| std::env::var(key).ok();
    let native = native_index(creator, &cwd, home.as_deref(), &getenv)?;
    if native.is_none() {
        let names = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .collect::<Vec<_>>();
        if crate::python_index::credential_configuration(getenv, &names, None).is_some()
            || value(&getenv, "NETRC").is_some()
            || home.is_some_and(|home| {
                [".netrc", "_netrc"]
                    .iter()
                    .any(|name| home.join(name).is_file())
            })
        {
            return Err(index_error(
                "native credentials are configured without a usable index",
            ));
        }
    }
    native.map(|url| anonymous_url(&url)).transpose()
}

fn native_index(
    creator: EnvCreator,
    cwd: &Path,
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let uv = || uv_index(cwd, home, getenv);
    let pip = || pip_index(home, getenv);
    match creator {
        EnvCreator::Uv => match uv()? {
            Some(url) => Ok(Some(url)),
            None => pip(),
        },
        EnvCreator::Stdlib => match pip()? {
            Some(url) => Ok(Some(url)),
            None => uv(),
        },
    }
}

fn value(getenv: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    getenv(key).filter(|value| !value.trim().is_empty())
}

fn enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn index_error(reason: &str) -> Error {
    Error::config(format!(
        "{reason}; set a single anonymous Python index in pip/uv, or clear the native source settings and set [registries.python].urls"
    ))
}

fn pip_index(
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let explicit = value(getenv, "PIP_CONFIG_FILE").map(PathBuf::from);
    let null = if cfg!(windows) { "nul" } else { "/dev/null" };
    let mut settings = BTreeMap::new();
    if !explicit.as_deref().is_some_and(|p| {
        p.to_str()
            .is_some_and(|path| path.eq_ignore_ascii_case(null))
    }) {
        let mut files = Vec::new();
        if cfg!(windows) {
            files.extend(
                value(getenv, "PROGRAMDATA").map(|base| Path::new(&base).join("pip/pip.ini")),
            );
        } else {
            if cfg!(target_os = "macos") {
                files.push(PathBuf::from("/Library/Application Support/pip/pip.conf"));
            }
            let xdg_dirs = value(getenv, "XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
            files.extend(std::env::split_paths(&xdg_dirs).map(|dir| dir.join("pip/pip.conf")));
            files.push(PathBuf::from("/etc/pip.conf"));
        }
        if !explicit.as_ref().is_some_and(|path| path.is_file()) {
            if cfg!(windows) {
                files.extend(home.map(|dir| dir.join("pip/pip.ini")));
                files.extend(
                    value(getenv, "APPDATA").map(|base| Path::new(&base).join("pip/pip.ini")),
                );
            } else {
                files.extend(home.map(|dir| dir.join(".pip/pip.conf")));
                let xdg = value(getenv, "XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| home.map(|dir| dir.join(".config")));
                let mac = home
                    .map(|dir| dir.join("Library/Application Support/pip"))
                    .filter(|dir| cfg!(target_os = "macos") && dir.is_dir());
                files.extend(
                    mac.map(|dir| dir.join("pip.conf"))
                        .or_else(|| xdg.map(|dir| dir.join("pip/pip.conf"))),
                );
            }
        }
        files.extend(explicit);
        for path in files {
            let Some(text) = read_config(&path)? else {
                continue;
            };
            let mut section = "";
            let mut last_option: Option<(bool, &str)> = None;
            for line in text.lines() {
                let entry = line.trim();
                if entry.is_empty() || entry.starts_with(['#', ';']) {
                    continue;
                }
                if let Some(name) = entry.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    section = name;
                    last_option = None;
                } else if let Some(previous) = last_option
                    .filter(|_| line.starts_with(char::is_whitespace))
                    .and_then(|key| settings.get_mut(&key))
                {
                    *previous = format!("{previous} {entry}");
                } else if matches!(section, "global" | "install") {
                    last_option = entry.split_once(['=', ':']).and_then(|(name, value)| {
                        let name = match name.trim().to_ascii_lowercase().replace('_', "-").as_str()
                        {
                            "index-url" => "index-url",
                            "extra-index-url" => "extra-index-url",
                            "find-links" => "find-links",
                            "no-index" => "no-index",
                            _ => return None,
                        };
                        let key = (section == "install", name);
                        settings.insert(key, value.trim().to_string());
                        Some(key)
                    });
                }
            }
        }
    }
    let setting = |env, key| {
        value(getenv, env)
            .or_else(|| {
                settings
                    .get(&(true, key))
                    .or_else(|| settings.get(&(false, key)))
                    .cloned()
            })
            .filter(|v| !v.is_empty())
    };
    if setting("PIP_NO_INDEX", "no-index").is_some_and(|v| enabled(&v))
        || setting("PIP_FIND_LINKS", "find-links").is_some()
        || setting("PIP_EXTRA_INDEX_URL", "extra-index-url").is_some()
    {
        return Err(index_error(
            "pip configures multiple indexes or a non-index source",
        ));
    }
    Ok(setting("PIP_INDEX_URL", "index-url"))
}

fn uv_index(
    cwd: &Path,
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    if value(getenv, "UV_ENV_FILE").is_some()
        || value(getenv, "UV_OFFLINE").is_some_and(|v| enabled(&v))
        || value(getenv, "UV_NO_INDEX").is_some_and(|v| enabled(&v))
        || value(getenv, "UV_FIND_LINKS").is_some()
    {
        return Err(index_error(
            "uv configures an environment file or a non-index source",
        ));
    }
    let mut defaults = UV_DEFAULT.iter().find_map(|key| value(getenv, key));
    let mut extras = UV_EXTRA.iter().find_map(|key| value(getenv, key));
    if !value(getenv, "UV_NO_CONFIG").is_some_and(|v| enabled(&v)) {
        let files = if let Some(explicit) = value(getenv, "UV_CONFIG_FILE") {
            vec![(PathBuf::from(explicit), false)]
        } else {
            uv_config_files(cwd, home, getenv)
        };
        for (path, pyproject) in files {
            let Some(text) = read_config(&path)? else {
                if value(getenv, "UV_CONFIG_FILE").is_some() {
                    return Err(index_error("UV_CONFIG_FILE could not be read"));
                }
                continue;
            };
            let parsed: toml::Value = match toml::from_str(&text) {
                Ok(parsed) => parsed,
                Err(_) if pyproject => return Err(index_error("invalid pyproject.toml")),
                Err(_) => return Err(index_error("invalid uv.toml")),
            };
            let config = if pyproject {
                let Some(config) = parsed.get("tool").and_then(|tool| tool.get("uv")) else {
                    continue;
                };
                config
            } else {
                &parsed
            };
            let pip = config.get("pip");
            let setting = |key| {
                pip.and_then(|value| value.get(key))
                    .or_else(|| config.get(key))
            };
            if setting("no-index").and_then(toml::Value::as_bool) == Some(true)
                || setting("find-links").is_some()
            {
                return Err(index_error("uv configures a non-index source"));
            }
            defaults = defaults.or_else(|| {
                setting("index-url")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            });
            extras = extras.or_else(|| {
                setting("extra-index-url").and_then(|value| {
                    value.as_str().map(str::to_owned).or_else(|| {
                        value.as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(toml::Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
                })
            });
            if let Some(indexes) = config.get("index").and_then(toml::Value::as_array) {
                for index in indexes {
                    let Some(url) = index.get("url").and_then(toml::Value::as_str) else {
                        return Err(index_error("uv configures an index without a URL"));
                    };
                    if index.get("explicit").and_then(toml::Value::as_bool) == Some(true) {
                        return Err(index_error("uv configures package-specific indexes"));
                    }
                    extras = Some(match extras {
                        Some(extra) if extra != url => format!("{extra} {url}"),
                        _ => url.to_string(),
                    });
                }
            }
        }
    }
    match (defaults, extras) {
        (Some(default), Some(extra)) if default != extra => {
            Err(index_error("uv configures multiple indexes"))
        }
        (default, extra) => Ok(default.or(extra)),
    }
}

fn uv_config_files(
    cwd: &Path,
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Vec<(PathBuf, bool)> {
    let mut files = Vec::new();
    for dir in cwd.ancestors() {
        let uv = dir.join("uv.toml");
        if uv.is_file() {
            files.push((uv, false));
            break;
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.is_file() {
            files.push((pyproject, true));
        }
    }
    let user = if cfg!(windows) {
        value(getenv, "APPDATA").map(PathBuf::from)
    } else {
        value(getenv, "XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|dir| dir.join(".config")))
    };
    files.extend(user.map(|dir| (dir.join("uv/uv.toml"), false)));
    let system = if cfg!(windows) {
        value(getenv, "PROGRAMDATA")
            .into_iter()
            .map(PathBuf::from)
            .collect()
    } else {
        let dirs = value(getenv, "XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
        std::env::split_paths(&dirs)
            .chain([PathBuf::from("/etc")])
            .collect::<Vec<_>>()
    };
    files.extend(
        system
            .into_iter()
            .map(|dir| dir.join("uv/uv.toml"))
            .find(|path| path.is_file())
            .map(|path| (path, false)),
    );
    files
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(path, error)),
    }
}

fn anonymous_url(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value).map_err(|_| index_error("invalid native Python index URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if value.contains(char::is_whitespace)
        || (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(index_error(
            "native Python index requires HTTPS and a URL without credentials, query, or fragment",
        ));
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_reads_native_install_index_with_config_file_and_environment_precedence() {
        let root = tempfile::tempdir().unwrap();
        let explicit = root.path().join("explicit.ini");
        std::fs::write(
            &explicit,
            "[global]\nindex-url = https://global.example/simple\n[install]\nindex-url = https://install.example/simple\n",
        ).unwrap();
        let config = |key: &str| (key == "PIP_CONFIG_FILE").then(|| explicit.display().to_string());
        assert_eq!(
            pip_index(Some(root.path()), &config).unwrap().as_deref(),
            Some("https://install.example/simple")
        );
        assert_eq!(
            pip_index(Some(root.path()), &|key| {
                if key == "PIP_INDEX_URL" {
                    Some("https://env.example/simple".into())
                } else {
                    config(key)
                }
            })
            .unwrap()
            .as_deref(),
            Some("https://env.example/simple")
        );
    }

    #[test]
    fn uv_index_environment_addresses_and_native_config_are_discovered() {
        let root = tempfile::tempdir().unwrap();
        let url = "https://private.example/simple/";
        for name in UV_DEFAULT.into_iter().chain(UV_EXTRA) {
            assert_eq!(
                uv_index(root.path(), None, &|key| (key == name)
                    .then(|| url.to_owned()))
                .unwrap()
                .as_deref(),
                Some(url)
            );
        }
        let uv_dir = root.path().join("uv");
        std::fs::create_dir(&uv_dir).unwrap();
        std::fs::write(
            uv_dir.join("uv.toml"),
            format!("[[index]]\nurl = {url:?}\ndefault = true\n"),
        )
        .unwrap();
        let config_home = if cfg!(windows) {
            "APPDATA"
        } else {
            "XDG_CONFIG_HOME"
        };
        assert_eq!(
            uv_index(root.path(), None, &|key| (key == config_home)
                .then(|| root.path().display().to_string()))
            .unwrap()
            .as_deref(),
            Some(url)
        );
    }

    #[test]
    fn private_python_sources_fail_closed_for_ambiguous_or_credentialed_urls() {
        let root = tempfile::tempdir().unwrap();
        assert!(uv_index(root.path(), None, &|key| match key {
            "UV_DEFAULT_INDEX" => Some("https://default.example/simple".into()),
            "UV_INDEX" => Some("https://higher.example/simple".into()),
            _ => None,
        })
        .is_err());
        let error = anonymous_url("https://user:fixture-secret@private.example/simple")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-secret"), "{error}");
    }
}
