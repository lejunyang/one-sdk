use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use super::{ChannelRef, DEFAULT_CHANNEL, UPSTREAM_BASE};
use crate::error::{Error, Result};

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn confine_config(root: &Path) {
    TEST_CONFIG_ROOT.with(|value| *value.borrow_mut() = Some(root.to_path_buf()));
}

#[derive(Default, Deserialize)]
struct Condarc {
    channel_alias: Option<String>,
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
        #[cfg(test)]
        if let Some(root) = TEST_CONFIG_ROOT.with(|root| root.borrow().clone()) {
            return Self::read(
                &config_paths(Some(root.clone()), |_| None)
                    .into_iter()
                    .filter(|path| path.starts_with(&root))
                    .collect::<Vec<_>>(),
            );
        }
        Self::read_with_env(
            &config_paths(std::env::home_dir(), |name| std::env::var_os(name)),
            |name| std::env::var_os(name),
        )
    }

    #[cfg(test)]
    fn read(paths: &[PathBuf]) -> Result<Option<Self>> {
        Self::read_with_env(paths, |_| None)
    }

    fn read_with_env(
        paths: &[PathBuf],
        getenv: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Option<Self>> {
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
            warn_merge_directives(path, &body);
            config.channel_alias = read.channel_alias.or(config.channel_alias);
            // The file's environment channel list does not select osdk tools.
            merge_channel_list(&mut config.default_channels, read.default_channels);
            config.custom_channels.extend(read.custom_channels);
        }
        let value = |name| getenv(name).map(|v| v.to_string_lossy().into_owned());
        if let Some(alias) = value("CONDA_CHANNEL_ALIAS") {
            config.channel_alias = Some(alias);
        }
        if let Some(defaults) = value("CONDA_DEFAULT_CHANNELS") {
            merge_channel_list(
                &mut config.default_channels,
                Some(environment_list(&defaults)),
            );
        }
        if let Some(custom) = value("CONDA_CUSTOM_CHANNELS") {
            let custom =
                serde_yaml::from_str::<BTreeMap<String, String>>(&custom).map_err(|_| {
                    unsupported("CONDA_CUSTOM_CHANNELS must be a channel-to-base mapping")
                })?;
            config.custom_channels.extend(custom);
        }
        let channels = value("CONDA_CHANNELS").map(|v| environment_list(&v));
        if config.channel_alias.is_none()
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
            channels,
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
                } else {
                    for channel in ["main", "r"]
                        .into_iter()
                        .chain(cfg!(windows).then_some("msys2"))
                    {
                        urls.push(channel_url(&format!(
                            "https://repo.anaconda.com/pkgs/{channel}"
                        ))?);
                    }
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
        channel_url(&crate::http::join_url(UPSTREAM_BASE, name))
    }
}

fn environment_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn warn_merge_directives(path: &Path, body: &str) {
    let mut key = "";
    for line in body.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some((name, _)) = line.split_once(':') {
                key = name.trim();
            }
        }
        if !matches!(
            key,
            "channels" | "channel_alias" | "default_channels" | "custom_channels"
        ) {
            continue;
        }
        // A quoted # is data, not a YAML merge directive.
        let mut quote = None;
        let mut escaped = false;
        for (index, ch) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' && quote == Some('"') {
                escaped = true;
                continue;
            }
            if matches!(ch, '\'' | '"') {
                if quote == Some(ch) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(ch);
                }
            }
            if ch == '#' && quote.is_none() {
                if matches!(line[index + 1..].trim(), "!final" | "!top" | "!bottom") {
                    tracing::warn!(path = %path.display(), key, "Conda merge directive is not supported; using normal channel precedence");
                }
                break;
            }
        }
    }
}

// Conda puts later configuration files first, keeps each file's list order,
// and retains the first occurrence of each value. An empty list adds nothing.
fn merge_channel_list(current: &mut Option<Vec<String>>, incoming: Option<Vec<String>>) {
    if let Some(mut values) = incoming {
        values.extend(current.take().unwrap_or_default());
        let mut seen = std::collections::HashSet::new();
        values.retain(|value| seen.insert(value.clone()));
        *current = Some(values);
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
    fn channels_only_preserves_osdk_sources() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(&path, "channels: [defaults]\n").unwrap();
        assert!(NativeChannels::read(&[path]).unwrap().is_none());
    }

    #[test]
    fn custom_only_keeps_public_alias() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(
            &path,
            "custom_channels: {nvidia: https://cuda.example.test}\n",
        )
        .unwrap();
        let native = NativeChannels::read(&[path]).unwrap().unwrap();
        let channels = super::super::parse_channels("nvidia,conda-forge").unwrap();
        let urls = native.urls(Some(&channels)).unwrap();
        assert_eq!(urls[0].as_str(), "https://cuda.example.test/nvidia/");
        assert_eq!(urls[1].as_str(), "https://conda.anaconda.org/conda-forge/");
    }

    #[test]
    fn default_only_keeps_public_alias() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(
            &path,
            "default_channels: [https://mirror.example.test/main]\n",
        )
        .unwrap();
        let native = NativeChannels::read(&[path]).unwrap().unwrap();
        let channels = super::super::parse_channels("conda-forge,defaults").unwrap();
        let urls = native.urls(Some(&channels)).unwrap();
        assert_eq!(urls[0].as_str(), "https://conda.anaconda.org/conda-forge/");
        assert_eq!(urls[1].as_str(), "https://mirror.example.test/main/");
    }

    #[test]
    fn irrelevant_merge_directive_does_not_block_channels() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        for body in ["ssl_verify: true #!final\n", "ssl_verify: '#!final'\n"] {
            std::fs::write(&path, body).unwrap();
            assert!(NativeChannels::read(std::slice::from_ref(&path))
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn relevant_merge_directive_warns_and_keeps_channel() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(
            &path,
            "channel_alias: https://mirror.example.test #!final\n",
        )
        .unwrap();
        let native = NativeChannels::read(&[path]).unwrap().unwrap();
        assert_eq!(
            native.urls(None).unwrap()[0].as_str(),
            "https://mirror.example.test/conda-forge/"
        );
    }

    #[test]
    fn conda_environment_alias_overrides_file() {
        const CHILD: &str = "OSDK_CONDA_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let native = NativeChannels::load().unwrap().unwrap();
            assert_eq!(
                native.urls(None).unwrap()[0].as_str(),
                "https://env.example.test/tools/"
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(
            &path,
            "channel_alias: https://file.example.test\nchannels: [defaults]\n",
        )
        .unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args([
                "--exact",
                "backend::conda::channel_config::tests::conda_environment_alias_overrides_file",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("CONDARC", &path)
            .env("CONDA_CHANNEL_ALIAS", "https://env.example.test")
            .env("CONDA_CHANNELS", "tools");
        #[cfg(windows)]
        for key in [
            "SystemRoot",
            "WINDIR",
            "WINEPREFIX",
            "WINELOADER",
            "WINESERVER",
            "WINEDEBUG",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("running 1 test"), "{stdout}");
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn environment_channels_alone_keeps_osdk_and_maps_merge() {
        assert!(
            NativeChannels::read_with_env(&[], |name| (name == "CONDA_CHANNELS")
                .then(|| "defaults".into()))
            .unwrap()
            .is_none()
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("condarc");
        std::fs::write(&path, "default_channels: [https://file.example.test/main]\ncustom_channels: {tools: https://file.example.test}\n").unwrap();
        let native = NativeChannels::read_with_env(&[path], |name| match name {
            "CONDA_DEFAULT_CHANNELS" => Some("https://env.example.test/main".into()),
            "CONDA_CUSTOM_CHANNELS" => {
                Some("{tools: https://env.example.test, deps: https://deps.example.test}".into())
            }
            _ => None,
        })
        .unwrap()
        .unwrap();
        let names = super::super::parse_channels("tools,deps,defaults").unwrap();
        assert_eq!(
            native
                .urls(Some(&names))
                .unwrap()
                .iter()
                .map(Url::as_str)
                .collect::<Vec<_>>(),
            [
                "https://env.example.test/tools/",
                "https://deps.example.test/deps/",
                "https://env.example.test/main/",
                "https://file.example.test/main/"
            ]
        );
    }

    #[test]
    fn confined_config_ignores_ambient_channel_alias() {
        let temp = tempfile::tempdir().unwrap();
        confine_config(temp.path());
        assert!(NativeChannels::load().unwrap().is_none());
        std::fs::write(
            temp.path().join(".condarc"),
            "channel_alias: https://fixture.example.test\n",
        )
        .unwrap();
        assert_eq!(
            NativeChannels::load().unwrap().unwrap().urls(None).unwrap()[0].as_str(),
            "https://fixture.example.test/conda-forge/"
        );
    }

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
    fn condarc_lists_merge_in_priority_order_and_deduplicate() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let prefix = temp.path().join("environment");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();
        std::fs::write(
            home.join(".condarc"),
            r#"
channel_alias: https://old.example.test
channels: [tools, shared, tools]
default_channels: [https://old.example.test/main, https://shared.example.test/main]
custom_channels: {tools: https://tools.example.test, shared: https://old.example.test}
"#,
        )
        .unwrap();
        std::fs::write(
            prefix.join(".condarc"),
            r#"
channel_alias: https://new.example.test
channels: [deps, shared, deps]
default_channels: [https://new.example.test/main, https://shared.example.test/main]
custom_channels: {shared: https://new.example.test}
"#,
        )
        .unwrap();
        let paths = config_paths(Some(home), |name| {
            (name == "CONDA_PREFIX").then(|| prefix.clone().into_os_string())
        })
        .into_iter()
        .filter(|path| path.starts_with(temp.path()))
        .collect::<Vec<_>>();
        let config = NativeChannels::read(&paths).unwrap().unwrap();
        assert!(config.channels.is_none());
        assert_eq!(
            config.urls(None).unwrap()[0].as_str(),
            "https://new.example.test/main/"
        );
        let defaults = super::super::parse_channels("defaults").unwrap();
        assert_eq!(
            config
                .urls(Some(&defaults))
                .unwrap()
                .iter()
                .map(Url::as_str)
                .collect::<Vec<_>>(),
            [
                "https://new.example.test/main/",
                "https://shared.example.test/main/",
                "https://old.example.test/main/",
            ]
        );
        std::fs::write(
            prefix.join("condarc"),
            "channels: []\ndefault_channels: []\n",
        )
        .unwrap();
        let mut paths = paths;
        paths.push(prefix.join("condarc"));
        let config = NativeChannels::read(&paths).unwrap().unwrap();
        assert!(config.channels.is_none());
        assert_eq!(config.urls(Some(&defaults)).unwrap().len(), 3);
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
