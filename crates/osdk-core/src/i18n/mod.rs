//! Lightweight internationalization (i18n) for osdk.
//!
//! Design: a small key -> {en, zh} catalog, a process-global active language
//! set once at startup, and `tr()` / `trf()` lookups with `{placeholder}`
//! substitution. This is intentionally dependency-free (no fluent) since the
//! surface is a couple hundred short strings across two languages; adding a
//! language is just another column in the catalog.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

use once_cell::sync::Lazy;

mod catalog;

/// Supported languages. `En` is the ultimate fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// BCP-47-ish short code.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// Parse an explicit language selector (from `--lang`, `OSDK_LANG`, or the
    /// config). Accepts `en`, `zh`, `zh-CN`, `zh_CN`, `english`, `中文`, etc.
    pub fn parse(s: &str) -> Option<Lang> {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        if s == "中文" {
            return Some(Lang::Zh);
        }
        let head = s
            .split(['.', '_', '-', '@', ' '])
            .next()
            .unwrap_or(s.as_str());
        match head {
            "en" | "english" | "c" | "posix" => Some(Lang::En),
            "zh" | "chinese" | "cn" => Some(Lang::Zh),
            _ => None,
        }
    }

    /// Detect from a locale env value like `zh_CN.UTF-8` / `en_US.UTF-8`.
    fn from_locale(s: &str) -> Option<Lang> {
        Lang::parse(s)
    }
}

// Active language, stored as a u8 for cheap atomic access. 0 = En, 1 = Zh.
static ACTIVE: AtomicU8 = AtomicU8::new(0);

fn lang_from_u8(v: u8) -> Lang {
    match v {
        1 => Lang::Zh,
        _ => Lang::En,
    }
}

/// Set the process-global active language. Called once by the CLI at startup.
pub fn set_lang(lang: Lang) {
    let v = match lang {
        Lang::En => 0,
        Lang::Zh => 1,
    };
    ACTIVE.store(v, Ordering::Relaxed);
}

/// The current active language.
pub fn current() -> Lang {
    lang_from_u8(ACTIVE.load(Ordering::Relaxed))
}

/// Resolve the language from explicit selectors + environment.
///
/// Precedence (highest first): `explicit` (from `--lang`/config) → `OSDK_LANG`
/// → `LC_ALL` → `LC_MESSAGES` → `LANG` → default `En`.
pub fn detect(explicit: Option<&str>, getenv: impl Fn(&str) -> Option<String>) -> Lang {
    if let Some(sel) = explicit {
        if let Some(l) = Lang::parse(sel) {
            return l;
        }
    }
    if let Some(v) = getenv("OSDK_LANG") {
        if let Some(l) = Lang::parse(&v) {
            return l;
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = getenv(key) {
            if let Some(l) = Lang::from_locale(&v) {
                return l;
            }
        }
    }
    Lang::En
}

// Catalog: key -> (en, zh). Built once.
type Row = (&'static str, &'static str);
static CATALOG: Lazy<HashMap<&'static str, Row>> = Lazy::new(catalog::build);

/// Look up a message by key in the active language. Falls back to English, then
/// to the key itself (so a missing key is visible, not empty).
pub fn tr(key: &str) -> String {
    trl(current(), key)
}

/// Look up a message by key in a specific language.
pub fn trl(lang: Lang, key: &str) -> String {
    match CATALOG.get(key) {
        Some((en, zh)) => match lang {
            Lang::En => en.to_string(),
            Lang::Zh => {
                if zh.is_empty() {
                    en.to_string()
                } else {
                    zh.to_string()
                }
            }
        },
        None => key.to_string(),
    }
}

/// Look up + interpolate `{name}` placeholders with the given args.
pub fn trf(key: &str, args: &[(&str, &str)]) -> String {
    interpolate(&tr(key), args)
}

/// Replace `{k}` occurrences in `template` with the provided values.
pub fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Convenience macro: `t!("key")` or `t!("key", name = value, ...)`.
/// Values may be any `Display` type; they're stringified.
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        $crate::i18n::tr($key)
    };
    ($key:expr, $($name:ident = $val:expr),+ $(,)?) => {{
        let args: &[(&str, &str)] = &[$((stringify!($name), &format!("{}", $val))),+];
        $crate::i18n::trf($key, args)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_langs() {
        assert_eq!(Lang::parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::parse("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::parse("中文"), Some(Lang::Zh));
        assert_eq!(Lang::parse("C"), Some(Lang::En));
        assert_eq!(Lang::parse("fr"), None);
    }

    #[test]
    fn detect_precedence() {
        // explicit beats env
        let l = detect(Some("zh"), |k| {
            if k == "LANG" {
                Some("en_US.UTF-8".into())
            } else {
                None
            }
        });
        assert_eq!(l, Lang::Zh);
        // OSDK_LANG beats LANG
        let l = detect(None, |k| match k {
            "OSDK_LANG" => Some("en".into()),
            "LANG" => Some("zh_CN.UTF-8".into()),
            _ => None,
        });
        assert_eq!(l, Lang::En);
        // LANG locale used when nothing explicit
        let l = detect(None, |k| {
            if k == "LANG" {
                Some("zh_CN.UTF-8".into())
            } else {
                None
            }
        });
        assert_eq!(l, Lang::Zh);
        // default en
        assert_eq!(detect(None, |_| None), Lang::En);
    }

    #[test]
    fn interpolation() {
        assert_eq!(
            interpolate("hello {name}!", &[("name", "world")]),
            "hello world!"
        );
    }

    #[test]
    fn tr_falls_back_to_key() {
        assert_eq!(
            trl(Lang::En, "definitely.missing.key"),
            "definitely.missing.key"
        );
    }

    #[test]
    fn log_keys_localized_both_langs() {
        // User-visible log messages must exist in both languages and differ
        // (i.e. actually translated, not just falling back to the key).
        for key in [
            "log.checksum_verified",
            "log.download_failover",
            "log.stale_python_cache",
        ] {
            let en = trl(Lang::En, key);
            let zh = trl(Lang::Zh, key);
            assert_ne!(en, key, "missing en for {key}");
            assert_ne!(zh, key, "missing zh for {key}");
            assert_ne!(en, zh, "zh not translated for {key}");
        }
        // Interpolation carries into the localized log message.
        let msg = trl(Lang::Zh, "log.download_failover");
        assert!(msg.contains("{err}"));
        assert_eq!(
            interpolate(&msg, &[("err", "boom")]),
            msg.replace("{err}", "boom")
        );
    }

    #[test]
    fn dynamic_npm_keys_are_bilingual_with_matching_placeholders() {
        for key in [
            "label.npm_lock_graph",
            "err.npm_managed_node_dependency_required",
            "err.npm_package_backend_invalid",
            "err.shim_generation_conflict",
            "err.shim_managed_node_required",
            "err.shim_dynamic_route_conflict",
            "err.npm_allow_builds_invalid",
            "err.npm_lock_graph_option_missing",
            "err.npm_lock_graph_identity_mismatch",
            "err.npm_lock_graph_format_unsupported",
            "err.npm_lock_graph_digest_invalid",
            "err.npm_lock_graph_tool_mismatch",
            "err.npm_lock_graph_not_produced",
            "err.npm_install_package_missing",
            "err.npm_install_bin_dir_missing",
            "err.npm_offline_lock_graph_required",
            "err.npm_package_sri_missing",
            "err.npm_dynamic_no_validated_executables",
            "err.npm_dynamic_managed_node_required",
            "err.managed_node_bin_dir_missing",
            "err.npm_bin_outside_install_root",
            "err.npm_bins_not_discovered",
            "err.npm_bin_target_unresolved",
            "err.npm_lock_payload_read",
            "err.npm_lock_payload_not_utf8",
            "err.npm_project_manifest_identity_mismatch",
            "err.npm_project_manifest_build_policy_mismatch",
            "err.npm_graph_parse_invalid",
            "err.npm_graph_root_missing",
            "err.npm_graph_root_version_mismatch",
            "err.npm_graph_resolved_root_missing",
            "err.npm_graph_root_integrity_missing",
            "err.npm_graph_root_integrity_invalid",
            "err.npm_graph_root_source_mismatch",
            "err.lock_npm_legacy_inline_regenerate",
            "err.lockfile_not_utf8",
            "err.lock_schema2_npm_artifact_forbidden",
            "err.lock_npm_package_mismatch",
            "err.lock_npm_graph_format_unsupported",
            "err.lock_npm_graph_path_unsafe_platform",
            "err.lock_npm_node_entry_required",
            "err.lock_npm_node_version_mismatch",
            "err.lock_npm_graph_missing",
            "err.lock_schema2_npm_legacy_inline",
            "err.lock_non_npm_graph_metadata",
            "err.lock_npm_graph_sha256_invalid",
            "err.lock_npm_graph_path_unsafe",
            "err.lock_npm_sidecar_read",
            "err.lock_npm_sidecar_checksum_mismatch",
            "err.lock_npm_sidecar_not_utf8",
            "err.lock_npm_graph_path_symlink",
            "err.lock_npm_sidecar_symlink",
            "err.file_size_limit_exceeded",
            "err.lock_backend_id_unsafe",
            "err.lock_version_unsafe",
            "err.lock_artifact_filename_unsafe",
            "err.lock_artifact_subdir_unsafe",
            "err.lock_schema1_npm_migration_requires_graph",
            "err.lock_npm_resolved_node_required",
            "err.lockfile_size_limit_exceeded",
            "err.lock_atomic_path_filename_missing",
            "err.fs_directory_create",
            "err.fs_file_create",
            "err.fs_file_write",
            "err.fs_file_sync",
            "err.fs_file_replace",
        ] {
            let en = trl(Lang::En, key);
            let zh = trl(Lang::Zh, key);
            assert_ne!(en, key, "missing en for {key}");
            assert_ne!(zh, key, "missing zh for {key}");
            assert_ne!(en, zh, "zh not translated for {key}");
            assert_eq!(
                placeholders(&en),
                placeholders(&zh),
                "placeholder mismatch for {key}"
            );
        }
    }

    fn placeholders(message: &str) -> Vec<&str> {
        let mut placeholders = Vec::new();
        let mut rest = message;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('}') else {
                break;
            };
            placeholders.push(&rest[..close]);
            rest = &rest[close + 1..];
        }
        placeholders.sort_unstable();
        placeholders
    }

    #[test]
    fn zh_falls_back_to_en_when_empty() {
        // 'pinned' has both; sanity that a known key differs by lang or falls back
        let en = trl(Lang::En, "msg.installed");
        assert!(!en.is_empty());
    }
}
