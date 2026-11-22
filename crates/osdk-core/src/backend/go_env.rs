//! The part of Go's native environment that selects module sources.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const SOURCE_KEYS: [&str; 4] = ["GOPROXY", "GOPRIVATE", "GONOPROXY", "GOINSECURE"];

pub(super) fn has_module_policy(settings: &BTreeMap<String, String>) -> bool {
    ["GOPROXY", "GOPRIVATE", "GONOPROXY"]
        .iter()
        .any(|key| settings.contains_key(*key))
}

pub(crate) fn config_file(getenv: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let value = |key| getenv(key).filter(|value| !value.is_empty());
    match value("GOENV").as_deref() {
        Some("off") => return None,
        Some(path) => return Some(PathBuf::from(path)),
        None => {}
    }
    let base = if cfg!(windows) {
        value("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        value("HOME").map(|home| Path::new(&home).join("Library/Application Support"))
    } else {
        value("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| value("HOME").map(|home| Path::new(&home).join(".config")))
    };
    base.map(|path| path.join("go/env"))
}

pub(crate) fn source_settings(
    getenv: &impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>> {
    let contents = match config_file(getenv) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(Error::io(path, error)),
        },
        None => String::new(),
    };
    let mut settings = BTreeMap::new();
    for (key, value) in contents.lines().filter_map(|line| line.split_once('=')) {
        if SOURCE_KEYS.contains(&key) {
            settings.insert(key.to_owned(), value.to_owned());
        }
    }
    for key in SOURCE_KEYS {
        if let Some(value) = getenv(key).filter(|value| !value.is_empty()) {
            settings.insert(key.to_owned(), value);
        }
    }
    settings.retain(|_, value| !value.is_empty());
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_env_file_sources_obey_native_environment_precedence_and_off() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("go.env");
        std::fs::write(
            &file,
            "GOPROXY=https://private.example/|direct\r\nGOPRIVATE=*.private.example\r\nGOFLAGS=-mod=vendor\r\n",
        )
        .unwrap();
        let settings = source_settings(&|key| match key {
            "GOENV" => Some(file.display().to_string()),
            "GOPROXY" => Some("https://preferred.example,direct".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(settings.len(), 2);
        assert_eq!(settings["GOPROXY"], "https://preferred.example,direct");
        assert_eq!(settings["GOPRIVATE"], "*.private.example");
        assert!(
            source_settings(&|key| (key == "GOENV").then(|| "off".into()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn go_env_default_file_matches_the_platform_configuration_directory() {
        let root = tempfile::tempdir().unwrap();
        let env_name = if cfg!(windows) {
            "APPDATA"
        } else if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
            "HOME"
        } else {
            "XDG_CONFIG_HOME"
        };
        let expected = if env_name == "HOME" {
            root.path().join("Library/Application Support/go/env")
        } else {
            root.path().join("go/env")
        };
        assert_eq!(
            config_file(&|key| (key == env_name).then(|| root.path().display().to_string())),
            Some(expected)
        );
    }
}
