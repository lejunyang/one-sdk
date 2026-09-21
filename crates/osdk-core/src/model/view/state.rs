//! Persisted membership of models in consumer views.
//!
//! Before declarative `[models]` config exists (P1-4), the `osdk model view`
//! CLI needs somewhere durable to remember "model X belongs to consumer Y's
//! profile P with these explicit mappings". This is a tiny machine-managed
//! JSON file under the views root, never hand-edited (same preference the
//! product applies to all managed state). P1-4 can later seed the same
//! [`ViewEntry`] set from config/lock; the on-disk shape here intentionally
//! matches that so no migration is needed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dirs::Dirs;
use crate::error::{Error, Result};
use crate::model::view::ViewKind;

const VIEW_STATE_FILE: &str = ".osdk-views.json";

/// All memberships, grouped `consumer -> profile -> [entries]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewState {
    #[serde(flatten)]
    pub consumers: BTreeMap<String, ViewConsumer>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewConsumer {
    #[serde(flatten)]
    pub profiles: BTreeMap<String, Vec<ViewEntrySpec>>,
}

/// One model's membership as persisted. Mirrors
/// [`crate::model::view::ViewEntry`] but is plain-data friendly for serde.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewEntrySpec {
    pub model: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
}

impl ViewState {
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let path = state_path(dirs);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::other(format!("invalid {VIEW_STATE_FILE}: {e}")))
    }

    pub fn save(&self, dirs: &Dirs) -> Result<()> {
        let path = state_path(dirs);
        if let Some(parent) = path.parent() {
            crate::dirs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))?;
        Ok(())
    }

    fn consumer_mut(&mut self, kind: ViewKind) -> &mut ViewConsumer {
        self.consumers.entry(kind.as_str().to_string()).or_default()
    }

    pub fn profiles(&self, kind: ViewKind) -> &BTreeMap<String, Vec<ViewEntrySpec>> {
        static EMPTY: BTreeMap<String, Vec<ViewEntrySpec>> = BTreeMap::new();
        self.consumers
            .get(kind.as_str())
            .map(|c| &c.profiles)
            .unwrap_or(&EMPTY)
    }

    /// Add or replace one model's membership.
    pub fn add(&mut self, kind: ViewKind, profile: &str, spec: ViewEntrySpec) {
        let list = self
            .consumer_mut(kind)
            .profiles
            .entry(profile.to_string())
            .or_default();
        if let Some(position) = list.iter().position(|e| e.model == spec.model) {
            list[position] = spec;
        } else {
            list.push(spec);
        }
    }

    /// Remove one model, or the whole profile when `model` is None. Returns
    /// whether anything was present.
    pub fn remove(&mut self, kind: ViewKind, profile: &str, model: Option<&str>) -> bool {
        let Some(consumer) = self.consumers.get_mut(kind.as_str()) else {
            return false;
        };
        match model {
            Some(model) => {
                let removed;
                let drop_profile;
                {
                    let Some(list) = consumer.profiles.get_mut(profile) else {
                        return false;
                    };
                    let before = list.len();
                    list.retain(|e| e.model != model);
                    removed = before != list.len();
                    drop_profile = list.is_empty();
                }
                if drop_profile {
                    consumer.profiles.remove(profile);
                }
                removed
            }
            None => consumer.profiles.remove(profile).is_some(),
        }
    }
}

fn state_path(dirs: &Dirs) -> PathBuf {
    dirs.model_views().join(VIEW_STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;

    fn dirs(root: &std::path::Path) -> Dirs {
        let d = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        d.ensure().unwrap();
        d
    }

    #[test]
    fn add_is_idempotent_and_removes_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let mut state = ViewState::load(&dirs).unwrap();
        assert_eq!(state.consumers.len(), 0);

        state.add(
            ViewKind::Comfyui,
            "default",
            ViewEntrySpec {
                model: "mix".into(),
                map: BTreeMap::new(),
            },
        );
        state.save(&dirs).unwrap();

        // Reload from disk.
        let mut state = ViewState::load(&dirs).unwrap();
        assert_eq!(state.profiles(ViewKind::Comfyui)["default"].len(), 1);

        // Adding the same model again replaces, never duplicates.
        state.add(
            ViewKind::Comfyui,
            "default",
            ViewEntrySpec {
                model: "mix".into(),
                map: BTreeMap::from([("unet/".into(), "diffusion_models".into())]),
            },
        );
        assert_eq!(state.profiles(ViewKind::Comfyui)["default"].len(), 1);
        assert_eq!(
            state.profiles(ViewKind::Comfyui)["default"][0]
                .map
                .get("unet/")
                .map(String::as_str),
            Some("diffusion_models")
        );

        assert!(state.remove(ViewKind::Comfyui, "default", Some("mix")));
        // Last member removed drops the profile too.
        assert!(state.profiles(ViewKind::Comfyui).get("default").is_none());
        assert!(!state.remove(ViewKind::Comfyui, "default", Some("mix")));
    }
}
