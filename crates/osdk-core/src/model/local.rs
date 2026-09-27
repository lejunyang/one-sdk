//! Import local model bytes into immutable snapshots.
//!
//! Local imports deliberately do not create project declarations or lock entries:
//! an arbitrary machine-local path is not a reproducible source on another host.

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{
    DownloadedModelFile, InstalledModel, ModelKind, ModelStore, ProviderId, SnapshotIdentity,
};
use crate::pipeline::verify::{hash_file, HashAlgo};

#[derive(Debug, Clone, Default)]
pub struct LocalImportOptions {
    pub target_path: Option<String>,
    pub variant: Option<String>,
    pub kind: Option<ModelKind>,
    pub family: Option<String>,
    pub derived_from: Option<String>,
}

pub fn import_local_model(
    store: &ModelStore,
    name: &str,
    input: &Path,
    options: LocalImportOptions,
) -> Result<InstalledModel> {
    crate::model::validate_model_name(name)?;
    crate::model::validate_model_metadata(
        options.family.as_deref(),
        options.derived_from.as_deref(),
    )?;

    let metadata = link_aware_metadata(input)?;
    if crate::store::dirlink::is_link(&metadata) {
        return Err(Error::config(format!(
            "refusing to import link or reparse point: {}",
            input.display()
        )));
    }

    let mut files = if metadata.is_file() {
        vec![local_file(
            input,
            single_file_target(input, options.kind, options.target_path.as_deref())?,
        )?]
    } else if metadata.is_dir() {
        if options.target_path.is_some() {
            return Err(Error::config(
                "--target-path is supported only when importing one file",
            ));
        }
        collect_directory(input)?
    } else {
        return Err(Error::config(format!(
            "local model input is neither a regular file nor a directory: {}",
            input.display()
        )));
    };

    if files.is_empty() {
        return Err(Error::config(format!(
            "local model directory contains no regular files: {}",
            input.display()
        )));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let revision = local_revision(&files);
    store.publish(
        SnapshotIdentity {
            name: name.to_string(),
            provider: ProviderId::Local,
            repository: name.to_string(),
            requested_revision: revision.clone(),
            revision,
            endpoint: "local".to_string(),
            variant: options.variant,
            kind: options.kind,
            family: options.family,
            derived_from: options.derived_from,
        },
        files,
    )
}

fn collect_directory(root: &Path) -> Result<Vec<DownloadedModelFile>> {
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries =
            std::fs::read_dir(&directory).map_err(|error| Error::io(&directory, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&directory, error))?;
            let path = entry.path();
            let metadata = link_aware_metadata(&path)?;
            if crate::store::dirlink::is_link(&metadata) {
                return Err(Error::config(format!(
                    "refusing to import link or reparse point: {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).map_err(|_| {
                    Error::config(format!(
                        "model file escaped import root: {}",
                        path.display()
                    ))
                })?;
                let relative = portable_relative_path(relative)?;
                files.push(local_file(&path, relative)?);
            } else {
                return Err(Error::config(format!(
                    "refusing to import special file: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(files)
}

fn local_file(path: &Path, target: String) -> Result<DownloadedModelFile> {
    let metadata = link_aware_metadata(path)?;
    if !metadata.is_file() || crate::store::dirlink::is_link(&metadata) {
        return Err(Error::config(format!(
            "refusing to import non-regular file: {}",
            path.display()
        )));
    }
    let sha256 = hash_file(path, HashAlgo::Sha256)?;
    Ok(DownloadedModelFile {
        path: target,
        source: path.to_path_buf(),
        size: metadata.len(),
        sha256: Some(sha256),
        etag: None,
    })
}

fn single_file_target(
    input: &Path,
    kind: Option<ModelKind>,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return portable_relative_path(Path::new(explicit));
    }
    let file_name = input
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::config(format!(
                "local model filename is not valid UTF-8: {}",
                input.display()
            ))
        })?;
    let prefix = match kind {
        Some(ModelKind::Checkpoint) => Some("checkpoints"),
        Some(ModelKind::Lora) => Some("loras"),
        Some(ModelKind::Vae) => Some("vae"),
        Some(ModelKind::TextEncoder) => Some("text_encoders"),
        Some(ModelKind::DiffusionModel) => Some("diffusion_models"),
        Some(ModelKind::Controlnet) => Some("controlnet"),
        Some(ModelKind::Upscaler) => Some("upscale_models"),
        Some(ModelKind::Embedding) => Some("embeddings"),
        Some(ModelKind::Other) | None => None,
    };
    Ok(prefix
        .map(|prefix| format!("{prefix}/{file_name}"))
        .unwrap_or_else(|| file_name.to_string()))
}

fn portable_relative_path(path: &Path) -> Result<String> {
    let value = path.to_str().ok_or_else(|| {
        Error::config(format!(
            "local model path is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    let normalized = value.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(':')
        || normalized.chars().any(char::is_control)
        || normalized
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(Error::config(format!(
            "unsafe local model target path `{value}`"
        )));
    }
    Ok(normalized)
}

fn local_revision(files: &[DownloadedModelFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"osdk-local-model-v1\0");
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.size.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(file.sha256.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
    }
    format!("local-{}", &hasher.finalize().to_hex()[..24])
}

fn link_aware_metadata(path: &Path) -> Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;
    use crate::store::link::LinkMode;
    use crate::store::Cas;
    use std::sync::Arc;

    fn store(root: &Path) -> ModelStore {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        ModelStore::new(
            dirs.clone(),
            Arc::new(Cas::new(dirs.store.clone())),
            LinkMode::Copy,
        )
    }

    #[test]
    fn imports_one_lora_into_a_content_addressed_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("style.safetensors");
        std::fs::write(&input, b"local lora").unwrap();
        let store = store(temporary.path());
        let installed = import_local_model(
            &store,
            "style",
            &input,
            LocalImportOptions {
                kind: Some(ModelKind::Lora),
                family: Some("sdxl".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(installed.manifest.provider, ProviderId::Local);
        assert!(installed.manifest.revision.starts_with("local-"));
        assert_eq!(installed.manifest.files[0].path, "loras/style.safetensors");
        assert!(installed.path.join("loras/style.safetensors").is_file());
        std::fs::write(&input, b"changed source bytes").unwrap();
        assert_eq!(
            std::fs::read(installed.path.join("loras/style.safetensors")).unwrap(),
            b"local lora"
        );
        assert_eq!(store.verify("style").unwrap(), installed.manifest);
    }

    #[test]
    fn target_paths_are_normalized_and_validated_independently_of_the_host() {
        assert_eq!(
            portable_relative_path(Path::new("loras\\style.safetensors")).unwrap(),
            "loras/style.safetensors"
        );
        assert!(portable_relative_path(Path::new("..\\outside.safetensors")).is_err());
        assert!(portable_relative_path(Path::new("loras//style.safetensors")).is_err());
        assert!(portable_relative_path(Path::new("C:\\models\\style.safetensors")).is_err());
    }

    #[test]
    fn directory_import_preserves_layout_and_changes_revision_with_content() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        std::fs::create_dir_all(input.join("unet")).unwrap();
        std::fs::write(input.join("config.json"), b"{}").unwrap();
        std::fs::write(input.join("unet/model.safetensors"), b"first").unwrap();
        let store = store(temporary.path());
        let first =
            import_local_model(&store, "bundle", &input, LocalImportOptions::default()).unwrap();
        assert_eq!(first.manifest.files[0].path, "config.json");
        assert_eq!(first.manifest.files[1].path, "unet/model.safetensors");
        std::fs::write(input.join("unet/model.safetensors"), b"second").unwrap();
        let second =
            import_local_model(&store, "bundle", &input, LocalImportOptions::default()).unwrap();
        assert_ne!(first.manifest.revision, second.manifest.revision);
        assert_ne!(first.path, second.path);
    }

    #[cfg(unix)]
    #[test]
    fn directory_import_rejects_symlinks_before_following_them() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        symlink(&outside, input.join("linked")).unwrap();
        let error = import_local_model(
            &store(temporary.path()),
            "bundle",
            &input,
            LocalImportOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("link or reparse point"));
    }

    #[cfg(windows)]
    #[test]
    fn directory_import_rejects_junctions_before_following_them() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        crate::store::dirlink::create(&outside, &input.join("linked")).unwrap();
        let error = import_local_model(
            &store(temporary.path()),
            "bundle",
            &input,
            LocalImportOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("link or reparse point"));
    }
}
