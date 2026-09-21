use std::collections::BTreeMap;

use crate::backend::Ctx;
use crate::model::ProviderId;

pub fn configured_env(
    ctx: &Ctx,
    getenv: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for provider in [ProviderId::HuggingFace, ProviderId::ModelScope] {
        let Some(config) = ctx.config.tool_sources(provider.as_str()) else {
            continue;
        };
        if !config.env {
            continue;
        }
        let sources = crate::model::source::effective_sources(ctx, provider);
        let selected = config
            .pin
            .as_deref()
            .and_then(|pin| sources.iter().find(|source| source.id == pin))
            .or_else(|| sources.first());
        let Some(source) = selected else {
            continue;
        };
        let force = config.env_force;
        match provider {
            ProviderId::HuggingFace => {
                let endpoint_managed = insert_if_allowed(
                    &mut environment,
                    "HF_ENDPOINT",
                    &source.download_url,
                    force,
                    &getenv,
                );
                let root = ctx
                    .dirs
                    .cache
                    .join("pkg")
                    .join("models")
                    .join("huggingface");
                insert_path_if_allowed(&mut environment, "HF_HOME", &root, force, &getenv);
                insert_path_if_allowed(
                    &mut environment,
                    "HF_HUB_CACHE",
                    &root.join("hub"),
                    force,
                    &getenv,
                );
                insert_path_if_allowed(
                    &mut environment,
                    "HF_XET_CACHE",
                    &root.join("xet"),
                    force,
                    &getenv,
                );
                insert_path_if_allowed(
                    &mut environment,
                    "HF_ASSETS_CACHE",
                    &root.join("assets"),
                    force,
                    &getenv,
                );
                // llama.cpp speaks its own dialect. Its `-hf` downloader reads the
                // HF-compatible endpoint from MODEL_ENDPOINT, *not* HF_ENDPOINT
                // (upstream docs/models.md), so mirroring only HF_ENDPOINT never
                // redirected llama.cpp. Export the same chosen source under the
                // name it actually reads.
                if endpoint_managed {
                    insert_if_allowed(
                        &mut environment,
                        "MODEL_ENDPOINT",
                        &source.download_url,
                        force,
                        &getenv,
                    );
                }
                // LLAMA_CACHE overrides llama.cpp's own download directory. Modern
                // builds store `-hf` files in the standard HF cache where
                // HF_HOME/HF_HUB_CACHE already point, so this equals the hub dir on
                // purpose: old and new llama.cpp then share one managed location
                // instead of writing two copies of the same GGUF.
                insert_path_if_allowed(
                    &mut environment,
                    "LLAMA_CACHE",
                    &root.join("hub"),
                    force,
                    &getenv,
                );
                if ctx.config.settings.offline {
                    insert_if_allowed(&mut environment, "HF_HUB_OFFLINE", "1", force, &getenv);
                }
                if endpoint_managed && !source.forward_credentials {
                    environment.insert("HF_HUB_DISABLE_IMPLICIT_TOKEN".into(), "1".into());
                    environment.insert("HF_TOKEN".into(), String::new());
                    environment.insert("HUGGING_FACE_HUB_TOKEN".into(), String::new());
                    environment.insert(
                        "HF_HOME".into(),
                        root.join("anonymous-home").display().to_string(),
                    );
                }
            }
            ProviderId::ModelScope => {
                let endpoint_managed = insert_if_allowed(
                    &mut environment,
                    "MODELSCOPE_ENDPOINT",
                    &source.download_url,
                    force,
                    &getenv,
                );
                let cache = ctx.dirs.cache.join("pkg").join("models").join("modelscope");
                insert_path_if_allowed(
                    &mut environment,
                    "MODELSCOPE_CACHE",
                    &cache,
                    force,
                    &getenv,
                );
                if endpoint_managed && !source.forward_credentials {
                    environment.insert("MODELSCOPE_API_TOKEN".into(), String::new());
                    environment.insert(
                        "MODELSCOPE_HOME".into(),
                        cache.join("anonymous-home").display().to_string(),
                    );
                }
            }
        }
    }
    environment
}

fn insert_path_if_allowed(
    environment: &mut BTreeMap<String, String>,
    key: &str,
    value: &std::path::Path,
    force: bool,
    getenv: &impl Fn(&str) -> Option<String>,
) -> bool {
    insert_if_allowed(
        environment,
        key,
        &value.display().to_string(),
        force,
        getenv,
    )
}

fn insert_if_allowed(
    environment: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
    force: bool,
    getenv: &impl Fn(&str) -> Option<String>,
) -> bool {
    if force || variable_is_available(key, getenv) {
        environment.insert(key.to_string(), value.to_string());
        true
    } else {
        false
    }
}

fn variable_is_available(key: &str, getenv: &impl Fn(&str) -> Option<String>) -> bool {
    let original_set = format!("OSDK_ORIG_{key}_SET");
    if getenv(&original_set).is_some() {
        return true;
    }
    getenv(key).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::{Config, Settings, ToolSources};
    use crate::dirs::Dirs;
    use crate::platform::Platform;
    use crate::source::Source;
    use crate::store::Cas;

    #[test]
    fn global_huggingface_env_preserves_user_values_unless_forced() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        ctx.config.sources.per_tool.insert(
            "huggingface".into(),
            ToolSources {
                env: true,
                ..Default::default()
            },
        );
        let environment = configured_env(&ctx, |key| {
            (key == "HF_ENDPOINT").then(|| "https://user.example".into())
        });
        assert!(!environment.contains_key("HF_ENDPOINT"));
        assert!(environment.contains_key("HF_HUB_CACHE"));

        ctx.config
            .sources
            .per_tool
            .get_mut("huggingface")
            .unwrap()
            .env_force = true;
        let environment = configured_env(&ctx, |key| {
            (key == "HF_ENDPOINT").then(|| "https://user.example".into())
        });
        assert_eq!(
            environment["HF_ENDPOINT"],
            "https://huggingface.co".to_string()
        );
    }

    #[test]
    fn custom_endpoints_disable_implicit_credentials_and_restore_managed_values() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        ctx.config.sources.per_tool.insert(
            "huggingface".into(),
            ToolSources {
                pin: Some("custom".into()),
                custom: vec![Source::mirror("custom", "https://mirror.example.test", 0)],
                env: true,
                ..Default::default()
            },
        );
        let environment = configured_env(&ctx, |_| None);
        assert_eq!(environment["HF_TOKEN"], "");
        assert_eq!(environment["HF_HUB_DISABLE_IMPLICIT_TOKEN"], "1");
        assert!(environment["HF_HOME"].contains("anonymous-home"));

        let environment = configured_env(&ctx, |key| match key {
            "HF_ENDPOINT" => Some("https://mirror.example.test".into()),
            "OSDK_ORIG_HF_ENDPOINT_SET" => Some("1".into()),
            "OSDK_ORIG_HF_ENDPOINT_PRESENT" => Some("1".into()),
            "OSDK_ORIG_HF_ENDPOINT" => Some("https://user.example".into()),
            _ => None,
        });
        assert_eq!(environment["HF_ENDPOINT"], "https://mirror.example.test");
    }

    #[test]
    fn llama_cpp_gets_its_own_endpoint_and_cache_names() {
        // llama.cpp reads MODEL_ENDPOINT, not HF_ENDPOINT, and LLAMA_CACHE for its
        // download directory. Exporting only the HF names left `-hf` downloads
        // ignoring the chosen mirror.
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        ctx.config.sources.per_tool.insert(
            "huggingface".into(),
            ToolSources {
                env: true,
                ..Default::default()
            },
        );
        let environment = configured_env(&ctx, |_| None);
        assert_eq!(
            environment["MODEL_ENDPOINT"],
            "https://huggingface.co".to_string()
        );
        assert!(
            environment["LLAMA_CACHE"]
                .replace('\\', "/")
                .ends_with("pkg/models/huggingface/hub"),
            "LLAMA_CACHE must point at the same managed hub dir: {}",
            environment["LLAMA_CACHE"]
        );

        // A user who already exports MODEL_ENDPOINT keeps it unless forced, the
        // same contract as every other variable here.
        let environment = configured_env(&ctx, |key| {
            (key == "MODEL_ENDPOINT").then(|| "https://mine.example".into())
        });
        assert!(!environment.contains_key("MODEL_ENDPOINT"));
        // LLAMA_CACHE is an unset-able location like HF_HUB_CACHE, so an existing
        // user value is preserved too.
        let environment = configured_env(&ctx, |key| {
            (key == "LLAMA_CACHE").then(|| "/srv/models".into())
        });
        assert!(!environment.contains_key("LLAMA_CACHE"));

        // Force overrides both.
        ctx.config
            .sources
            .per_tool
            .get_mut("huggingface")
            .unwrap()
            .env_force = true;
        let environment = configured_env(&ctx, |key| {
            (key == "MODEL_ENDPOINT").then(|| "https://mine.example".into())
        });
        assert_eq!(environment["MODEL_ENDPOINT"], "https://huggingface.co");
    }

    #[test]
    fn llama_vars_are_not_exported_for_modelscope_or_when_env_disabled() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        // ModelScope enabled, huggingface env disabled: the llama.cpp variables are
        // HF-specific and must not leak from the ModelScope block.
        ctx.config.sources.per_tool.insert(
            "modelscope".into(),
            ToolSources {
                env: true,
                ..Default::default()
            },
        );
        let environment = configured_env(&ctx, |_| None);
        assert!(!environment.contains_key("MODEL_ENDPOINT"));
        assert!(!environment.contains_key("LLAMA_CACHE"));

        // And with huggingface env off entirely, nothing is exported.
        ctx.config.sources.per_tool.clear();
        let environment = configured_env(&ctx, |_| None);
        assert!(!environment.contains_key("MODEL_ENDPOINT"));
    }

    #[test]
    fn offline_is_only_exported_for_clients_that_support_it() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        ctx.config.settings.offline = true;
        for provider in ["huggingface", "modelscope"] {
            ctx.config.sources.per_tool.insert(
                provider.into(),
                ToolSources {
                    env: true,
                    ..Default::default()
                },
            );
        }
        let environment = configured_env(&ctx, |_| None);
        assert_eq!(environment["HF_HUB_OFFLINE"], "1");
        assert!(!environment.contains_key("MODELSCOPE_OFFLINE"));
    }

    #[test]
    fn custom_modelscope_endpoint_isolates_persisted_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temporary.path());
        ctx.config.sources.per_tool.insert(
            "modelscope".into(),
            ToolSources {
                pin: Some("custom".into()),
                custom: vec![Source::mirror(
                    "custom",
                    "https://modelscope.example.test",
                    0,
                )],
                env: true,
                ..Default::default()
            },
        );
        let environment = configured_env(&ctx, |key| {
            (key == "MODELSCOPE_HOME").then(|| "/home/user/.modelscope".into())
        });
        assert_eq!(environment["MODELSCOPE_API_TOKEN"], "");
        assert!(environment["MODELSCOPE_HOME"].contains("anonymous-home"));
        assert_ne!(environment["MODELSCOPE_HOME"], "/home/user/.modelscope");
    }

    fn test_ctx(root: &std::path::Path) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: Platform::current(),
            config: Config {
                settings: Settings::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            cas: Arc::new(Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }
}
