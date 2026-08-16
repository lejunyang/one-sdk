//! Model references, immutable snapshot manifests, and local materialization.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dirs::{create_dir_all, sanitize_tool_id, Dirs};
use crate::error::{Error, Result};
use crate::lock::FileLock;
use crate::pipeline::verify::{hash_file, HashAlgo};
use crate::store::link::LinkMode;
use crate::store::manifest::{FileEntry, Manifest};
use crate::store::Cas;

pub mod provider;
pub mod pull;
pub mod source;

const MODEL_MANIFEST_FILE: &str = ".osdk-model.json";
const CURRENT_FILE: &str = "current.json";
const COMPLETE_MARKER: &str = ".osdk-complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderId {
    #[serde(rename = "huggingface", alias = "hugging-face", alias = "hf")]
    HuggingFace,
    #[serde(rename = "modelscope", alias = "model-scope", alias = "ms")]
    ModelScope,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HuggingFace => "huggingface",
            Self::ModelScope => "modelscope",
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProviderId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "hf" | "huggingface" | "hugging-face" => Ok(Self::HuggingFace),
            "ms" | "modelscope" | "model-scope" => Ok(Self::ModelScope),
            other => Err(Error::config(format!(
                "unknown model provider `{other}` (expected huggingface|modelscope)"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub repository: String,
    pub revision: String,
}

impl ModelRef {
    pub fn parse(value: &str) -> Result<Self> {
        let (provider, rest) = value.split_once(':').ok_or_else(|| {
            Error::config(format!(
                "invalid model reference `{value}` (expected hf:owner/repo@revision)"
            ))
        })?;
        let provider = provider.parse()?;
        let (repository, revision) = rest
            .rsplit_once('@')
            .unwrap_or((rest, default_revision(provider)));
        validate_repository(repository)?;
        if revision.trim().is_empty() {
            return Err(Error::config("model revision cannot be empty"));
        }
        Ok(Self {
            provider,
            repository: repository.to_string(),
            revision: revision.to_string(),
        })
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}@{}",
            self.provider, self.repository, self.revision
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelFile {
    pub path: String,
    pub size: u64,
    pub cas_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotManifest {
    pub schema: u32,
    pub name: String,
    pub provider: ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub files: Vec<ModelFile>,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub struct SnapshotIdentity {
    pub name: String,
    pub provider: ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    pub variant: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DownloadedModelFile {
    pub path: String,
    pub source: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentSnapshot {
    snapshot: String,
}

#[derive(Debug, Clone)]
pub struct InstalledModel {
    pub manifest: SnapshotManifest,
    pub path: PathBuf,
}

pub struct ModelStore {
    dirs: Dirs,
    cas: Arc<Cas>,
    link_mode: LinkMode,
}

impl ModelStore {
    pub fn new(dirs: Dirs, cas: Arc<Cas>, link_mode: LinkMode) -> Self {
        Self {
            dirs,
            cas,
            link_mode,
        }
    }

    pub fn publish(
        &self,
        identity: SnapshotIdentity,
        mut files: Vec<DownloadedModelFile>,
    ) -> Result<InstalledModel> {
        validate_model_name(&identity.name)?;
        validate_repository(&identity.repository)?;
        if files.is_empty() {
            return Err(Error::other("model snapshot contains no files"));
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if files.windows(2).any(|pair| pair[0].path == pair[1].path) {
            return Err(Error::other("model snapshot contains duplicate file paths"));
        }

        let model_root = self.model_root(&identity.name);
        let snapshots = model_root.join("snapshots");
        create_dir_all(&snapshots)?;
        let snapshot = snapshot_key(&identity, &files);
        let destination = snapshots.join(&snapshot);
        let lock_path = model_root.join(".locks").join(format!("{snapshot}.lock"));
        let _lock = FileLock::acquire(&lock_path)?;

        if destination.join(COMPLETE_MARKER).is_file() {
            self.write_current(&model_root, &snapshot)?;
            return self.load_path(&destination);
        }

        let temporary = snapshots.join(format!(".{snapshot}.tmp-{}", std::process::id()));
        if temporary.exists() {
            std::fs::remove_dir_all(&temporary).map_err(|error| Error::io(&temporary, error))?;
        }
        create_dir_all(&temporary)?;

        let mut model_files = Vec::with_capacity(files.len());
        let mut cas_manifest = Manifest::new(
            format!("model:{}", identity.name),
            snapshot.clone(),
            self.link_mode.to_string(),
        );
        let result = (|| {
            for file in files {
                let relative = safe_relative_path(&file.path)?;
                let actual_size = std::fs::metadata(&file.source)
                    .map_err(|error| Error::io(&file.source, error))?
                    .len();
                if actual_size != file.size {
                    return Err(Error::other(format!(
                        "model file size mismatch for {}: expected {}, got {}",
                        file.path, file.size, actual_size
                    )));
                }
                if let Some(expected) = file.sha256.as_deref() {
                    crate::pipeline::verify::verify_file(
                        &file.source,
                        expected,
                        HashAlgo::Sha256,
                        &file.path,
                    )?;
                }
                let (cas_hash, _, _) = self.cas.ingest_preserve(&file.source)?;
                let destination_file = temporary.join(&relative);
                self.cas
                    .materialize_object(&cas_hash, &destination_file, self.link_mode)?;
                model_files.push(ModelFile {
                    path: file.path.clone(),
                    size: file.size,
                    cas_hash: cas_hash.clone(),
                    sha256: file.sha256,
                    etag: file.etag,
                });
                cas_manifest.files.push(FileEntry {
                    path: file.path,
                    hash: Some(cas_hash),
                    mode: 0o644,
                    symlink: None,
                });
            }

            model_files.sort_by(|left, right| left.path.cmp(&right.path));
            cas_manifest
                .files
                .sort_by(|left, right| left.path.cmp(&right.path));
            cas_manifest.save(&temporary)?;
            let manifest = SnapshotManifest {
                schema: 1,
                name: identity.name,
                provider: identity.provider,
                repository: identity.repository,
                requested_revision: identity.requested_revision,
                revision: identity.revision,
                endpoint: identity.endpoint,
                variant: identity.variant,
                files: model_files,
                created_at: crate::source::now_secs(),
            };
            write_json_atomic(&temporary.join(MODEL_MANIFEST_FILE), &manifest)?;
            std::fs::write(temporary.join(COMPLETE_MARKER), b"")
                .map_err(|error| Error::io(temporary.join(COMPLETE_MARKER), error))?;
            if destination.exists() {
                std::fs::remove_dir_all(&destination)
                    .map_err(|error| Error::io(&destination, error))?;
            }
            std::fs::rename(&temporary, &destination)
                .map_err(|error| Error::io(&destination, error))?;
            self.write_current(&model_root, &snapshot)?;
            Ok(InstalledModel {
                manifest,
                path: destination.clone(),
            })
        })();

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temporary);
        }
        result
    }

    pub fn current(&self, name: &str) -> Result<InstalledModel> {
        validate_model_name(name)?;
        let model_root = self.model_root(name);
        let marker_path = model_root.join(CURRENT_FILE);
        let bytes = std::fs::read(&marker_path).map_err(|error| Error::io(&marker_path, error))?;
        let current: CurrentSnapshot = serde_json::from_slice(&bytes)?;
        self.load_path(&model_root.join("snapshots").join(current.snapshot))
    }

    pub fn list(&self) -> Result<Vec<InstalledModel>> {
        let root = self.dirs.models();
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut installed = Vec::new();
        for entry in std::fs::read_dir(&root).map_err(|error| Error::io(&root, error))? {
            let entry = entry.map_err(|error| Error::io(&root, error))?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(model) = self.current(&name) {
                installed.push(model);
            }
        }
        installed.sort_by(|left, right| left.manifest.name.cmp(&right.manifest.name));
        Ok(installed)
    }

    pub fn verify(&self, name: &str) -> Result<SnapshotManifest> {
        let installed = self.current(name)?;
        for file in &installed.manifest.files {
            let path = installed.path.join(safe_relative_path(&file.path)?);
            let actual = crate::store::hash_file(&path)?;
            if actual != file.cas_hash {
                return Err(Error::ChecksumMismatch {
                    name: file.path.clone(),
                    expected: file.cas_hash.clone(),
                    actual,
                });
            }
            if let Some(expected) = file.sha256.as_deref() {
                let actual = hash_file(&path, HashAlgo::Sha256)?;
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err(Error::ChecksumMismatch {
                        name: file.path.clone(),
                        expected: expected.to_string(),
                        actual,
                    });
                }
            }
        }
        Ok(installed.manifest)
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        validate_model_name(name)?;
        let root = self.model_root(name);
        if !root.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&root).map_err(|error| Error::io(&root, error))?;
        Ok(true)
    }

    fn model_root(&self, name: &str) -> PathBuf {
        self.dirs.models().join(sanitize_tool_id(name))
    }

    fn load_path(&self, path: &Path) -> Result<InstalledModel> {
        if !path.join(COMPLETE_MARKER).is_file() {
            return Err(Error::other(format!(
                "model snapshot is incomplete: {}",
                path.display()
            )));
        }
        let manifest_path = path.join(MODEL_MANIFEST_FILE);
        let bytes =
            std::fs::read(&manifest_path).map_err(|error| Error::io(&manifest_path, error))?;
        Ok(InstalledModel {
            manifest: serde_json::from_slice(&bytes)?,
            path: path.to_path_buf(),
        })
    }

    fn write_current(&self, model_root: &Path, snapshot: &str) -> Result<()> {
        write_json_atomic(
            &model_root.join(CURRENT_FILE),
            &CurrentSnapshot {
                snapshot: snapshot.to_string(),
            },
        )
    }
}

fn default_revision(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::HuggingFace => "main",
        ProviderId::ModelScope => "master",
    }
}

fn validate_repository(repository: &str) -> Result<()> {
    let mut parts = repository.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty()
        || repo.is_empty()
        || parts.next().is_some()
        || [owner, repo].iter().any(|part| {
            matches!(*part, "." | "..")
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        })
    {
        return Err(Error::config(format!(
            "invalid model repository `{repository}` (expected owner/name)"
        )));
    }
    Ok(())
}

pub fn validate_model_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(Error::config(format!(
            "invalid model name `{name}` (use letters, digits, dot, dash, or underscore)"
        )));
    }
    Ok(())
}

pub fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::config(format!("unsafe model file path `{value}`")));
    }
    Ok(path.to_path_buf())
}

fn snapshot_key(identity: &SnapshotIdentity, files: &[DownloadedModelFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(
        format!(
            "{}\0{}\0{}\0{}",
            identity.provider,
            identity.repository,
            identity.revision,
            identity.variant.as_deref().unwrap_or_default()
        )
        .as_bytes(),
    );
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.size.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(file.sha256.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
        hasher.update(file.etag.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex()[..24].to_string()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(path, error))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parses_provider_references_and_defaults() {
        let hf = ModelRef::parse("hf:Qwen/Qwen2.5-7B-Instruct@abc123").unwrap();
        assert_eq!(hf.provider, ProviderId::HuggingFace);
        assert_eq!(hf.revision, "abc123");
        let modelscope = ModelRef::parse("modelscope:Qwen/Qwen2.5-7B-Instruct").unwrap();
        assert_eq!(modelscope.revision, "master");
        assert!(ModelRef::parse("hf:../secret").is_err());
    }

    #[test]
    fn publishes_lists_and_verifies_immutable_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("config.json");
        std::fs::write(&source, br#"{"model":"fixture"}"#).unwrap();
        let size = std::fs::metadata(&source).unwrap().len();
        let sha256 = hash_file(&source, HashAlgo::Sha256).unwrap();
        let store = store(temporary.path());
        let installed = store
            .publish(
                SnapshotIdentity {
                    name: "fixture".into(),
                    provider: ProviderId::HuggingFace,
                    repository: "owner/repo".into(),
                    requested_revision: "main".into(),
                    revision: "abc123".into(),
                    endpoint: "https://example.test".into(),
                    variant: None,
                },
                vec![DownloadedModelFile {
                    path: "config.json".into(),
                    source,
                    size,
                    sha256: Some(sha256),
                    etag: Some("etag".into()),
                }],
            )
            .unwrap();

        assert_eq!(
            std::fs::read(installed.path.join("config.json")).unwrap(),
            br#"{"model":"fixture"}"#
        );
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.verify("fixture").unwrap().revision, "abc123");
        std::fs::write(installed.path.join("config.json"), b"tampered").unwrap();
        assert!(store.verify("fixture").is_err());
    }

    #[test]
    fn model_manifests_keep_cas_objects_live() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("weights.bin");
        std::fs::write(&source, b"same weights").unwrap();
        let size = std::fs::metadata(&source).unwrap().len();
        let store = store(temporary.path());
        let installed = store
            .publish(
                SnapshotIdentity {
                    name: "fixture".into(),
                    provider: ProviderId::ModelScope,
                    repository: "owner/repo".into(),
                    requested_revision: "master".into(),
                    revision: "v1".into(),
                    endpoint: "https://example.test".into(),
                    variant: None,
                },
                vec![DownloadedModelFile {
                    path: "weights.bin".into(),
                    source,
                    size,
                    sha256: None,
                    etag: None,
                }],
            )
            .unwrap();
        let hash = installed.manifest.files[0].cas_hash.clone();
        let (removed, _) = store
            .cas
            .gc_roots(&[&store.dirs.installs, &store.dirs.models()])
            .unwrap();
        assert_eq!(removed, 0);
        assert!(store.cas.object_path(&hash).is_file());
        store.remove("fixture").unwrap();
        let (removed, _) = store
            .cas
            .gc_roots(&[&store.dirs.installs, &store.dirs.models()])
            .unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn file_selection_is_part_of_snapshot_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first.bin");
        let second = temporary.path().join("second.bin");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let store = store(temporary.path());
        let identity = SnapshotIdentity {
            name: "fixture".into(),
            provider: ProviderId::HuggingFace,
            repository: "owner/repo".into(),
            requested_revision: "main".into(),
            revision: "abc123".into(),
            endpoint: "https://example.test".into(),
            variant: None,
        };
        let first_snapshot = store
            .publish(
                identity.clone(),
                vec![DownloadedModelFile {
                    path: "first.bin".into(),
                    source: first,
                    size: 5,
                    sha256: None,
                    etag: None,
                }],
            )
            .unwrap();
        let second_snapshot = store
            .publish(
                identity,
                vec![DownloadedModelFile {
                    path: "second.bin".into(),
                    source: second,
                    size: 6,
                    sha256: None,
                    etag: None,
                }],
            )
            .unwrap();
        assert_ne!(first_snapshot.path, second_snapshot.path);
        assert!(second_snapshot.path.join("second.bin").is_file());
        assert!(!second_snapshot.path.join("first.bin").exists());
    }
}
