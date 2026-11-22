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

pub(super) fn process_value(key: &str) -> Option<String> {
    if key == "GOPROXY" {
        crate::execution::original_env(key).and_then(|value| value.into_string().ok())
    } else {
        std::env::var(key).ok()
    }
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

/// The broadest matching Go path-prefix pattern still covers the command and
/// any parent module root at or below this prefix.
pub(super) fn private_query_prefix<'a>(path: &'a str, patterns: &str) -> Result<Option<&'a str>> {
    let mut matched: Option<&str> = None;
    for pattern in patterns.split(',') {
        let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
        let depth = pattern.bytes().filter(|&byte| byte == b'/').count();
        let Some(end) = path
            .match_indices('/')
            .map(|(offset, _)| offset)
            .chain([path.len()])
            .nth(depth)
        else {
            continue;
        };
        if !matched.is_none_or(|prefix| end < prefix.len()) {
            continue;
        }
        let Some(expression) = go_path_glob(pattern) else {
            continue;
        };
        let matcher = regex::Regex::new(&expression)
            .map_err(|_| Error::config("could not evaluate Go module privacy pattern"))?;
        if matcher.is_match(&path[..end]) {
            matched = Some(&path[..end]);
        }
    }
    Ok(matched)
}

/// Translate Go's path.Match syntax. In particular, braces are literal,
/// '*' and '?' cannot cross '/', and '^' is the only class negation.
fn go_path_glob(pattern: &str) -> Option<String> {
    use std::fmt::Write as _;

    let mut expression = String::from(r"\A");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => {
                while chars.next_if_eq(&'*').is_some() {}
                expression.push_str("[^/]*");
            }
            '?' => expression.push_str("[^/]"),
            '[' => {
                let negated = chars.next_if_eq(&'^').is_some();
                let mut ranges = String::new();
                loop {
                    let start = class_char(&mut chars)?;
                    let end = if chars.next_if_eq(&'-').is_some() {
                        class_char(&mut chars)?
                    } else {
                        start
                    };
                    if start <= end {
                        write!(ranges, r"\x{{{:x}}}-\x{{{:x}}}", start as u32, end as u32).ok()?;
                    }
                    if chars.next_if_eq(&']').is_some() {
                        break;
                    }
                }
                // Seed with the empty set so even a class whose ranges all
                // run backwards matches exactly what Go matches.
                expression.push_str(if negated { "[^" } else { "[" });
                expression.push_str(r"[\x00&&[^\x00]]");
                expression.push_str(&ranges);
                expression.push(']');
            }
            '\\' => write!(expression, r"\x{{{:x}}}", chars.next()? as u32).ok()?,
            literal => write!(expression, r"\x{{{:x}}}", literal as u32).ok()?,
        }
    }
    expression.push_str(r"\z");
    Some(expression)
}

fn class_char(chars: &mut impl Iterator<Item = char>) -> Option<char> {
    match chars.next()? {
        '-' | ']' => None,
        '\\' => chars.next(),
        ch => Some(ch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_query_prefix_uses_go_globs_and_the_broadest_match() {
        let path = "corp.example/secret/cli/nested";
        let find = |patterns| private_query_prefix(path, patterns).unwrap();
        assert_eq!(
            find("corp.example/secret/*/"),
            Some("corp.example/secret/cli")
        );
        assert_eq!(
            find("corp.example/secret/cli,*.example/sec[!r]et"),
            Some("corp.example/secret")
        );
        assert_eq!(find("corp.example/sec[^r]et"), None);
        assert_eq!(find("corp.example/{secret,other}"), None);
        assert_eq!(find("corp.example/sec\\ret"), Some("corp.example/secret"));
        assert_eq!(
            find("corp.example/*nested,corp.example/secret/[malformed"),
            None
        );
    }

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
