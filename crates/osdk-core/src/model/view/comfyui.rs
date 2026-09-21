//! The ComfyUI view planner.
//!
//! Maps an immutable snapshot onto ComfyUI's `<root>/<category>/<file>` layout.
//! Every file is assigned exactly one category, or left out fail-closed and
//! reported. Directory conventions encode the multi-component diffusion repo
//! shape (`unet/` holds diffusion_models, not checkpoints), which the
//! source-edition verification (research §5.12) proved needs per-category
//! mapping.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::model::view::{snapshot_file_path, Placement, Plan};
use crate::model::InstalledModel;

/// All 25 ComfyUI model folder categories (research §5.11). Empty ones are
/// pre-created by the ViewStore so a consumer never mkdirs into a read-only view.
pub fn categories() -> &'static [&'static str] {
    &[
        "checkpoints",
        "configs",
        "loras",
        "vae",
        "text_encoders",
        "diffusion_models",
        "clip",
        "clip_vision",
        "style_models",
        "embeddings",
        "vae_approx",
        "controlnet",
        "t2i_adapter",
        "gligen",
        "upscale_models",
        "latent_upscale_models",
        "hypernetworks",
        "classifiers",
        "detection",
        "segmentation",
        "ultralytics",
        "sams",
        "grounding",
        "onnx",
        "photomaker",
    ]
}

/// Classify one snapshot-relative file (`/` separators). None when no
/// convention applies. `explicit` (repo prefix -> category) wins over
/// conventions, longest prefix first.
pub fn classify(relative: &str, explicit: &BTreeMap<String, String>) -> Option<Placement> {
    let file_name = relative.rsplit('/').next().unwrap_or(relative).to_string();

    let mut best: Option<(&str, &str)> = None;
    for (prefix, category) in explicit {
        let normalized = prefix.trim_matches('/');
        if normalized.is_empty() {
            continue;
        }
        if (relative == normalized || relative.starts_with(&format!("{normalized}/")))
            && best.is_none_or(|(p, _)| normalized.len() > p.len())
        {
            best = Some((normalized, category));
        }
    }
    if let Some((_, category)) = best {
        return Some(Placement {
            category: category.to_string(),
            file_name,
        });
    }

    let top = relative.split('/').next().unwrap_or("");
    let category = match top {
        "unet" | "diffusion_models" => "diffusion_models",
        "vae" => "vae",
        "text_encoder" | "text_encoders" | "clip" if relative.contains('/') => "text_encoders",
        "lora" | "loras" => "loras",
        "controlnet" => "controlnet",
        "t2i_adapter" => "t2i_adapter",
        "clip_vision" => "clip_vision",
        "embeddings" => "embeddings",
        "upscale_models" | "upscaler" | "esrgan" => "upscale_models",
        "style_models" => "style_models",
        _ => {
            return classify_loose(&file_name).map(|category| Placement {
                category: category.to_string(),
                file_name,
            })
        }
    };
    Some(Placement {
        category: category.to_string(),
        file_name,
    })
}

/// Infer a category for a repo-root file from its suffix. Conservative: an
/// unidentified weight is left unclassified rather than offered as a
/// checkpoint it may not be; the explicit map is the supported override.
fn classify_loose(_file_name: &str) -> Option<&'static str> {
    None
}

/// Build the plan: assign every snapshot file to a category-relative path.
pub fn plan(installed: &InstalledModel, explicit: &BTreeMap<String, String>) -> Result<Plan> {
    let mut plan = Plan::default();
    for file in &installed.manifest.files {
        let normalized = file.path.replace('\\', "/");
        let Some(placement) = classify(&normalized, explicit) else {
            plan.unclassified.push(file.path.clone());
            continue;
        };
        let source = snapshot_file_path(installed, &file.path)?;
        plan.link(
            format!("{}/{}", placement.category, placement.file_name),
            source,
        );
    }
    plan.files.sort_by(|a, b| a.relative.cmp(&b.relative));
    plan.unclassified.sort();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn directory_conventions_split_a_multi_component_repo() {
        let empty = BTreeMap::new();
        assert_eq!(
            classify("unet/flux.safetensors", &empty).unwrap().category,
            "diffusion_models"
        );
        assert_eq!(
            classify("vae/ae.safetensors", &empty).unwrap().category,
            "vae"
        );
        assert_eq!(
            classify("text_encoder/clip_l.safetensors", &empty)
                .unwrap()
                .category,
            "text_encoders"
        );
        assert_eq!(
            classify("loras/style.safetensors", &empty)
                .unwrap()
                .category,
            "loras"
        );
    }

    #[test]
    fn explicit_mapping_overrides_convention_and_picks_longest_prefix() {
        let m = map(&[("unet", "checkpoints"), ("unet/special", "loras")]);
        assert_eq!(
            classify("unet/special/x.safetensors", &m).unwrap().category,
            "loras"
        );
        assert_eq!(
            classify("unet/y.safetensors", &m).unwrap().category,
            "checkpoints"
        );
    }

    #[test]
    fn an_unidentified_loose_weight_is_fail_closed_not_dumped_in_checkpoints() {
        let empty = BTreeMap::new();
        assert!(classify("weights-only.safetensors", &empty).is_none());
        let m = map(&[("", "checkpoints")]); // empty prefix must be ignored
        assert!(classify("weights-only.safetensors", &m).is_none());
        // an explicit exact-file mapping rescues it
        let m = map(&[("weights-only.safetensors", "checkpoints")]);
        assert_eq!(
            classify("weights-only.safetensors", &m).unwrap().category,
            "checkpoints"
        );
    }

    #[test]
    fn all_twenty_five_categories_are_distinct_and_present() {
        let cats = categories();
        assert_eq!(cats.len(), 25, "expected the full ComfyUI folder set");
        let unique: std::collections::HashSet<_> = cats.iter().collect();
        assert_eq!(unique.len(), cats.len());
    }
}
