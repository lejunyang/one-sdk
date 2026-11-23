//! Native index settings that a standalone pypi tool can use for both
//! anonymous version discovery and installer execution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::pypi::EnvCreator;
use crate::error::{Error, Result};

const UV_DEFAULT: [&str; 2] = ["UV_DEFAULT_INDEX", "UV_INDEX_URL"];
const UV_EXTRA: [&str; 2] = ["UV_INDEX", "UV_EXTRA_INDEX_URL"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NativeIndex {
    pub default: String,
    pub extra: Vec<String>,
}
impl NativeIndex {
    fn single(url: String) -> Self {
        Self {
            default: url,
            extra: Vec::new(),
        }
    }
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.extra
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.default.as_str()))
    }
}

pub(super) fn index(ctx: &crate::backend::Ctx, creator: EnvCreator) -> Result<Option<NativeIndex>> {
    if ctx.config.settings.precedence.package_policy("python")
        == crate::config::SourcePrecedence::Osdk
    {
        return Ok(None);
    }
    let cwd = std::env::current_dir()?;
    let home = crate::python_index::home_directory();
    let getenv = |key: &str| std::env::var(key).ok();
    let native = native_index(creator, &cwd, home.as_deref(), &getenv)?;
    native
        .map(|index| {
            Ok(NativeIndex {
                default: anonymous_url(&index.default)?,
                extra: index
                    .extra
                    .iter()
                    .map(|url| anonymous_url(url))
                    .collect::<Result<_>>()?,
            })
        })
        .transpose()
}

fn native_index(
    creator: EnvCreator,
    cwd: &Path,
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<Option<NativeIndex>> {
    let uv = || uv_index(cwd, home, getenv);
    let pip = || pip_index(home, getenv).map(|url| url.map(NativeIndex::single));
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
        "{reason}; set [registries.python].urls and [settings.precedence] python = \"osdk\" to use an explicit osdk index"
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
) -> Result<Option<NativeIndex>> {
    let options = uv_config(cwd, home, getenv)?;
    let options = options.pip.with_fallback(options.top_level);
    let mut default = UV_DEFAULT.iter().find_map(|key| value(getenv, key));
    let mut named = BTreeSet::new();
    let mut urls = Vec::new();
    for key in UV_EXTRA {
        if let Some(indexes) = value(getenv, key) {
            urls.extend(indexes.split_whitespace().map(|index| {
                index
                    .split_once('=')
                    .map_or(index, |(_, url)| url)
                    .to_owned()
            }));
        }
    }
    for index in options.index {
        if index.name.is_some_and(|name| !named.insert(name)) {
            continue;
        }
        if index.explicit {
            continue;
        }
        if index.format.as_deref() == Some("flat") {
            return Err(index_error("uv configures a non-index source"));
        }
        if index.default {
            default.get_or_insert(index.url);
        } else {
            urls.push(index.url);
        }
    }
    urls.extend(options.extra_index_url);
    let default = default.or(options.index_url);
    if default.is_none() && urls.is_empty() {
        return Ok(None);
    }
    let default = default.unwrap_or_else(|| crate::python_index::PYPI.into());
    urls.retain(|url| url != &default);
    let mut seen = BTreeSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    Ok(Some(NativeIndex {
        default,
        extra: urls.into_iter().collect(),
    }))
}

#[derive(Default, serde::Deserialize)]
struct UvConfig {
    #[serde(flatten)]
    top_level: UvIndexOptions,
    #[serde(default)]
    pip: UvIndexOptions,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct UvIndexOptions {
    index_url: Option<String>,
    no_index: Option<bool>,
    find_links: Vec<String>,
    extra_index_url: Vec<String>,
    index: Vec<UvIndex>,
}

#[derive(serde::Deserialize)]
struct UvIndex {
    url: String,
    name: Option<String>,
    format: Option<String>,
    #[serde(default)]
    explicit: bool,
    #[serde(default)]
    default: bool,
}

impl UvIndexOptions {
    fn with_fallback(mut self, lower: Self) -> Self {
        self.index_url = self.index_url.or(lower.index_url);
        self.no_index = self.no_index.or(lower.no_index);
        self.find_links.extend(lower.find_links);
        self.extra_index_url.extend(lower.extra_index_url);
        self.index.extend(lower.index);
        self
    }
}

fn uv_config(
    _cwd: &Path,
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<UvConfig> {
    if let Some(explicit) = value(getenv, "UV_CONFIG_FILE") {
        return read_uv_config(Path::new(&explicit), false)?
            .ok_or_else(|| index_error("UV_CONFIG_FILE could not be read"));
    }
    if value(getenv, "UV_NO_CONFIG").is_some_and(|v| enabled(&v)) {
        return Ok(UvConfig::default());
    }

    let mut merged = UvConfig::default();
    // uv combines each section across files, before combining [pip] with the
    // top-level settings. Scalars take the higher value and arrays concatenate.
    for path in uv_user_and_system_files(home, getenv) {
        if let Some(lower) = read_uv_config(&path, false)? {
            merged.top_level = merged.top_level.with_fallback(lower.top_level);
            merged.pip = merged.pip.with_fallback(lower.pip);
        }
    }
    Ok(merged)
}

fn read_uv_config(path: &Path, pyproject: bool) -> Result<Option<UvConfig>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    let invalid = || {
        index_error(if pyproject {
            "invalid pyproject.toml"
        } else {
            "invalid uv.toml"
        })
    };
    if !pyproject {
        return toml::from_str(&text).map(Some).map_err(|_| invalid());
    }
    let mut parsed: toml::Table = toml::from_str(&text).map_err(|_| invalid())?;
    let uv = parsed
        .get_mut("tool")
        .and_then(toml::Value::as_table_mut)
        .and_then(|tool| tool.remove("uv"));
    uv.map(|config| config.try_into().map_err(|_| invalid()))
        .transpose()
}

fn uv_user_and_system_files(
    home: Option<&Path>,
    getenv: &impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let user = if cfg!(windows) {
        value(getenv, "APPDATA").map(PathBuf::from)
    } else {
        value(getenv, "XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|dir| dir.join(".config")))
    };
    files.extend(user.map(|dir| dir.join("uv/uv.toml")));
    if value(getenv, "UV_NO_SYSTEM_CONFIG").is_some_and(|v| enabled(&v)) {
        return files;
    }
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
            .find(|path| path.is_file()),
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
    fn global_tool_ignores_project_explicit_uv_index() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[[tool.uv.index]]\nurl = \"https://pytorch.example/simple\"\nexplicit = true\n",
        )
        .unwrap();
        assert_eq!(
            uv_index(root.path(), None, &|key| (key == "UV_NO_SYSTEM_CONFIG")
                .then(|| "1".into()))
            .unwrap(),
            None
        );
    }

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
    fn unrelated_uv_variables_do_not_select_or_reject_indexes() {
        let root = tempfile::tempdir().unwrap();
        for name in ["UV_OFFLINE", "UV_ENV_FILE", "UV_FIND_LINKS", "UV_NO_INDEX"] {
            assert_eq!(
                uv_index(root.path(), None, &|key| {
                    (key == name).then(|| "1".into())
                })
                .unwrap(),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn named_uv_indexes_keep_their_url_and_default() {
        let root = tempfile::tempdir().unwrap();
        let index = uv_index(root.path(), None, &|key| {
            (key == "UV_INDEX").then(|| "corp=https://private.example/simple/".into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(index.extra, vec!["https://private.example/simple/"]);
        assert_eq!(index.default, crate::python_index::PYPI);
    }

    #[test]
    fn uv_additional_indexes_keep_default_pypi() {
        let root = tempfile::tempdir().unwrap();
        let url = "https://private.example/simple/";
        for name in UV_EXTRA {
            let index = uv_index(root.path(), None, &|key| (key == name).then(|| url.into()))
                .unwrap()
                .unwrap();
            assert_eq!(index.default, crate::python_index::PYPI);
            assert_eq!(index.extra, vec![url]);
        }
        let index = uv_index(root.path(), None, &|key| {
            (key == "UV_DEFAULT_INDEX").then(|| url.into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(index.default, url);
        assert!(index.extra.is_empty());
    }

    #[test]
    fn private_python_sources_fail_closed_for_ambiguous_or_credentialed_urls() {
        let error = anonymous_url("https://user:fixture-secret@private.example/simple")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-secret"), "{error}");
    }
}
