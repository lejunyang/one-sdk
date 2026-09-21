//! The ComfyUI view renderer.
//!
//! Renders a snapshot into ComfyUI's `<root>/<category>/<file>` layout. Every
//! file is classified into exactly one category, or left out fail-closed and
//! reported. Category conventions encode the real multi-component diffusion
//! repo shape (a `unet/` directory holds diffusion_models, not checkpoints),
//! which is the case the source-edition verification proved needs per-category
//! mapping (research §5.12).

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::Result;
use crate::model::view::{place_file, snapshot_file_path, Placement, RenderReport};
use crate::model::InstalledModel;
use crate::store::link::LinkMode;

/// All 25 ComfyUI model folder categories (research §5.11). Empty ones are
/// created so a consumer does not try to mkdir into a read-only view.
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

/// Classify one snapshot-relative file (path separators already normalized to
/// `/` by the caller). Returns None when no convention applies.
///
/// `explicit` holds user-supplied repo-prefix -> category mappings and wins
/// over every convention.
pub fn classify(relative: &str, explicit: &BTreeMap<String, String>) -> Option<Placement> {
    let file_name = relative.rsplit('/').next().unwrap_or(relative).to_string();

    // 1. Explicit prefix mapping (longest prefix wins, so a more specific
    //    directory beats a broader one).
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

    // 2. Directory conventions for multi-component repos.
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
            // 3. Loose file at the repo root: infer a few well-known suffixes,
            //    but never dump an unknown weight into checkpoints.
            return classify_loose(&file_name).map(|category| Placement {
                category: category.to_string(),
                file_name,
            });
        }
    };
    Some(Placement {
        category: category.to_string(),
        file_name,
    })
}

/// Infer category for a file at the repo root from its suffix. Conservative by
/// design: a weight whose type cannot be named is left unclassified rather than
/// being offered as a checkpoint it may not be.
fn classify_loose(file_name: &str) -> Option<&'static str> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".safetensors")
        || lower.ends_with(".ckpt")
        || lower.ends_with(".pt")
        || lower.ends_with(".pth")
        || lower.ends_with(".bin")
    {
        // Even a recognized weight suffix at the repo root is ambiguous for a
        // diffusion repo; fail-closed per the research design. The explicit map
        // is the supported way to place these.
        return None;
    }
    None
}

pub(crate) fn render(
    installed: &InstalledModel,
    explicit: &BTreeMap<String, String>,
    model_dir: &Path,
    mode: LinkMode,
) -> Result<RenderReport> {
    let mut report = RenderReport::default();

    // Pre-create every category directory. Empty categories are valid and this
    // keeps a consumer from trying to create folders inside the read-only view.
    for category in categories() {
        crate::dirs::create_dir_all(&model_dir.join(category))?;
    }

    for file in &installed.manifest.files {
        let normalized = file.path.replace('\\', "/");
        let Some(placement) = classify(&normalized, explicit) else {
            report.unclassified.push(file.path.clone());
            continue;
        };
        let source = snapshot_file_path(installed, file)?;
        let destination = model_dir
            .join(&placement.category)
            .join(&placement.file_name);
        let used = place_file(&source, &destination, mode)?;
        report.note_mode(used);
        report
            .placed
            .push(format!("{}/{}", placement.category, placement.file_name));
    }

    report.placed.sort();
    report.unclassified.sort();
    Ok(report)
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
        // The more specific prefix wins.
        let p = classify("unet/special/x.safetensors", &m).unwrap();
        assert_eq!(p.category, "loras");
        // The broader one still applies elsewhere under unet/.
        let p = classify("unet/y.safetensors", &m).unwrap();
        assert_eq!(p.category, "checkpoints");
    }

    #[test]
    fn an_unidentified_loose_weight_is_fail_closed_not_dumped_in_checkpoints() {
        let empty = BTreeMap::new();
        // weights-only.safetensors at the repo root has no classifiable folder
        assert!(classify("weights-only.safetensors", &empty).is_none());
        // but an explicit mapping rescues it
        let m = map(&[("", "checkpoints")]); // empty prefix must be ignored
        assert!(classify("weights-only.safetensors", &m).is_none());
        // but an explicit exact-file mapping rescues it
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
