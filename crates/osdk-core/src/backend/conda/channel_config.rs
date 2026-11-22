use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use super::{ChannelRef, DEFAULT_CHANNEL, UPSTREAM_BASE};
use crate::error::{Error, Result};

#[derive(Default, Deserialize)]
struct Condarc {
    channel_alias: Option<String>,
    channels: Option<Vec<String>>,
    default_channels: Option<Vec<String>>,
    #[serde(default)]
    custom_channels: BTreeMap<String, String>,
    #[serde(flatten)]
    other: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Default)]
pub(super) struct NativeChannels {
    alias: Option<Url>,
    channels: Option<Vec<String>>,
    defaults: Option<Vec<String>>,
    custom: BTreeMap<String, Url>,
}

impl NativeChannels {
    pub(super) fn load() -> Result<Option<Self>> {
        Self::read(&config_paths(std::env::home_dir(), |name| {
            std::env::var_os(name)
        }))
    }

    fn read(paths: &[PathBuf]) -> Result<Option<Self>> {
        let mut config = Condarc::default();
        for path in paths {
            let body = match std::fs::read_to_string(path) {
                Ok(body) => body,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(ignored(path, "could not read the file")),
            };
            let Some(read) = serde_yaml::from_str::<Option<Condarc>>(&body)
                .map_err(|_| ignored(path, "unsupported YAML or channel configuration"))?
            else {
                continue;
            };
            for key in [
                "custom_multichannels",
                "migrated_channel_aliases",
                "migrated_custom_channels",
                "allowlist_channels",
                "denylist_channels",
            ] {
                if read.other.get(key).is_some_and(|value| {
                    !value.is_null()
                        && !value.as_sequence().is_some_and(Vec::is_empty)
                        && !value
                            .as_mapping()
                            .is_some_and(serde_yaml::Mapping::is_empty)
                }) {
                    return Err(ignored(path, &format!("unsupported channel setting {key}")));
                }
            }
            if body.lines().any(|line| {
                line.split_once('#').is_some_and(|(_, comment)| {
                    matches!(comment.trim(), "!final" | "!top" | "!bottom")
                })
            }) {
                return Err(ignored(path, "unsupported Conda merge directive"));
            }
            config.channel_alias = read.channel_alias.or(config.channel_alias);
            config.channels = read.channels.or(config.channels);
            config.default_channels = read.default_channels.or(config.default_channels);
            config.custom_channels.extend(read.custom_channels);
        }
        if config.channel_alias.is_none()
            && config.channels.is_none()
            && config.default_channels.is_none()
            && config.custom_channels.is_empty()
        {
            return Ok(None);
        }
        Ok(Some(Self {
            alias: config
                .channel_alias
                .as_deref()
                .map(channel_url)
                .transpose()?,
            channels: config.channels,
            defaults: config.default_channels,
            custom: config
                .custom_channels
                .into_iter()
                .map(|(name, base)| Ok((name, channel_url(&base)?)))
                .collect::<Result<_>>()?,
        }))
    }

    pub(super) fn urls(&self, explicit_channels: Option<&[ChannelRef]>) -> Result<Vec<Url>> {
        let implicit = if self.defaults.is_some() {
            "defaults"
        } else {
            DEFAULT_CHANNEL
        };
        let names = match explicit_channels {
            Some(channels) => channels.iter().map(ChannelRef::name).collect::<Vec<_>>(),
            None => self.channels.as_ref().map_or_else(
                || vec![implicit],
                |channels| channels.iter().map(String::as_str).collect(),
            ),
        };
        let mut urls = Vec::new();
        for name in names {
            if name == "defaults" {
                if let Some(defaults) = &self.defaults {
                    for channel in defaults {
                        urls.push(self.resolve(channel)?);
                    }
                } else if self.alias.is_none() && self.custom.is_empty() {
                    for channel in ["main", "r"]
                        .into_iter()
                        .chain(cfg!(windows).then_some("msys2"))
                    {
                        urls.push(channel_url(&format!(
                            "https://repo.anaconda.com/pkgs/{channel}"
                        ))?);
                    }
                } else {
                    return Err(unsupported(
                        "defaults needs default_channels when using a private Conda configuration",
                    ));
                }
            } else {
                urls.push(self.resolve(name)?);
            }
        }
        let mut seen = std::collections::HashSet::new();
        urls.retain(|url| seen.insert(url.clone()));
        if urls.is_empty() {
            return Err(unsupported("the configured Conda channel list is empty"));
        }
        Ok(urls)
    }

    fn resolve(&self, name: &str) -> Result<Url> {
        if name.contains("://") {
            return channel_url(name);
        }
        let name = ChannelRef::parse(name).map_err(|_| {
            unsupported("expected a plain Conda channel name or an anonymous HTTP(S) URL")
        })?;
        let name = name.name();
        if let Some(base) = self.custom.get(name).or(self.alias.as_ref()) {
            return channel_url(&crate::http::join_url(base.as_str(), name));
        }
        if self.custom.is_empty() && self.defaults.is_none() {
            return channel_url(&crate::http::join_url(UPSTREAM_BASE, name));
        }
        Err(unsupported(&format!(
            "no Conda base URL is configured for channel {name}"
        )))
    }
}

fn unsupported(reason: &str) -> Error {
    tracing::warn!(
        reason,
        "unsupported Conda channel configuration was ignored"
    );
    Error::config(format!("{reason}. Set an anonymous Conda channel or select an osdk custom `[sources]` entry with `--source`"))
}

fn ignored(path: &Path, reason: &str) -> Error {
    unsupported(&format!("{}: {reason}", path.display()))
}

fn channel_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim_end_matches('/')).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path_segments().is_some_and(|mut parts| {
                !parts.any(|part| part == "t") || parts.next().is_none_or(str::is_empty)
            })
    });
    let Some(url) = url else {
        return Err(unsupported(
            "expected an anonymous HTTP(S) Conda channel URL without login credentials",
        ));
    };
    // Channel roots are directories, including when URL::join adds a channel
    // or platform component to a base with an existing path.
    Ok(
        Url::parse(&format!("{}/", url.as_str().trim_end_matches('/')))
            .expect("validated channel URL"),
    )
}

fn config_paths(home: Option<PathBuf>, getenv: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let env_dir = |name| {
        getenv(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let mut directories = Vec::new();
    if cfg!(windows) {
        if let Some(program_data) = env_dir("ProgramData") {
            directories.push(program_data.join("conda"));
        }
    } else {
        directories.extend(["/etc/conda", "/var/lib/conda"].map(PathBuf::from));
    }
    directories.extend(env_dir("CONDA_ROOT"));
    directories.extend(env_dir("XDG_CONFIG_HOME").map(|path| path.join("conda")));
    if let Some(home) = &home {
        directories.extend([home.join(".config/conda"), home.join(".conda")]);
    }
    let mut paths = Vec::new();
    for directory in directories {
        add_config_paths(&mut paths, &directory);
    }
    paths.extend(home.map(|path| path.join(".condarc")));
    if let Some(prefix) = env_dir("CONDA_PREFIX") {
        add_config_paths(&mut paths, &prefix);
    }
    paths.extend(env_dir("CONDARC"));
    paths.dedup();
    paths
}

fn add_config_paths(paths: &mut Vec<PathBuf>, directory: &Path) {
    paths.extend([directory.join(".condarc"), directory.join("condarc")]);
    if let Ok(entries) = std::fs::read_dir(directory.join("condarc.d")) {
        let mut snippets = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
            })
            .collect::<Vec<_>>();
        snippets.sort();
        paths.extend(snippets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condarc_locations_and_channel_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = home.join(".config/conda/condarc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "channel_alias: https://old.example.test/").unwrap();
        std::fs::write(
            home.join(".condarc"),
            "channel_alias: https://alias.example.test/",
        )
        .unwrap();
        let explicit = temp.path().join("active.rc");
        std::fs::write(&explicit,
            "default_channels: [https://defaults.example.test/main]\ncustom_channels:\n  nvidia: https://cuda.example.test/channel\n",
        ).unwrap();
        let paths = config_paths(Some(home), |name| {
            (name == "CONDARC").then(|| explicit.clone().into_os_string())
        });
        let paths = paths
            .into_iter()
            .filter(|path| path.starts_with(temp.path()))
            .collect::<Vec<_>>();
        let config = NativeChannels::read(&paths).unwrap().unwrap();
        let explicit_channels =
            super::super::parse_channels("nvidia,conda-forge,defaults").unwrap();
        assert_eq!(
            config
                .urls(Some(&explicit_channels))
                .unwrap()
                .iter()
                .map(Url::as_str)
                .collect::<Vec<_>>(),
            [
                "https://cuda.example.test/channel/nvidia/",
                "https://alias.example.test/conda-forge/",
                "https://defaults.example.test/main/"
            ]
        );
        assert_eq!(
            config.urls(None).unwrap()[0].as_str(),
            "https://defaults.example.test/main/"
        );
    }

    #[test]
    fn unsupported_condarc_credentials_and_channel_types_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        for config in [
            "channel_alias: https://user:secret@example.test/",
            "channel_alias: https://example.test/t/secret/",
            "custom_multichannels: {corp: [corp-a, corp-b]}",
            "channel_alias: [https://example.test/]",
        ] {
            std::fs::write(&path, config).unwrap();
            let error = NativeChannels::read(std::slice::from_ref(&path))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("[sources]"), "{error}");
            assert!(!error.contains("secret"), "{error}");
        }
    }
}
