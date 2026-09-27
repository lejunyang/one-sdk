//! Stable machine-readable output for `osdk model` commands.
//!
//! These documents are a CLI protocol, distinct from the on-disk snapshot and
//! lock schemas. Absolute paths describe this machine and therefore retain native
//! separators; repository-relative file paths come from manifests and stay `/`
//! normalized.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

pub(crate) const MODEL_OUTPUT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub(crate) struct ModelListOutput {
    pub schema_version: u32,
    pub models: Vec<ModelOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelShowOutput {
    pub schema_version: u32,
    pub model: ModelOutput,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelVerifyOutput {
    pub schema_version: u32,
    pub status: &'static str,
    pub model: ModelOutput,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelPathOutput {
    pub schema_version: u32,
    pub name: String,
    pub stable: bool,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelOutput {
    pub name: String,
    pub provider: osdk_core::model::ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub files: Vec<ModelFileOutput>,
    pub created_at: u64,
    pub snapshot_path: String,
    pub stable_path: String,
    pub stable_path_available: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelFileOutput {
    pub path: String,
    pub size: u64,
    pub cas_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl ModelOutput {
    pub(crate) fn from_installed(
        installed: osdk_core::model::InstalledModel,
        stable_path: &Path,
    ) -> Self {
        let manifest = installed.manifest;
        Self {
            name: manifest.name,
            provider: manifest.provider,
            repository: manifest.repository,
            requested_revision: manifest.requested_revision,
            revision: manifest.revision,
            endpoint: manifest.endpoint,
            variant: manifest.variant,
            files: manifest
                .files
                .into_iter()
                .map(|file| ModelFileOutput {
                    path: file.path,
                    size: file.size,
                    cas_hash: file.cas_hash,
                    sha256: file.sha256,
                    etag: file.etag,
                })
                .collect(),
            created_at: manifest.created_at,
            snapshot_path: installed.path.display().to_string(),
            stable_path: stable_path.display().to_string(),
            stable_path_available: osdk_core::store::dirlink::exists(stable_path),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewListOutput {
    pub schema_version: u32,
    pub views: Vec<ViewOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewOutput {
    pub consumer: String,
    pub profile: String,
    pub root: Option<String>,
    pub models: Vec<ViewModelOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewModelOutput {
    pub name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewPathOutput {
    pub schema_version: u32,
    pub consumer: osdk_core::model::view::ViewKind,
    pub profile: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewDoctorOutput {
    pub schema_version: u32,
    pub consumer: osdk_core::model::view::ViewKind,
    pub profile: String,
    pub status: &'static str,
    pub root: String,
    pub models: Vec<ViewDoctorModelOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ViewDoctorModelOutput {
    pub name: String,
    pub placed: Vec<String>,
    pub unclassified: Vec<String>,
    pub copies: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelSyncEvent {
    pub schema_version: u32,
    pub event: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<usize>,
}

impl ModelSyncEvent {
    pub(crate) fn new(event: &str, status: &str, dry_run: bool) -> Self {
        Self {
            schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
            event: event.to_string(),
            status: status.to_string(),
            model: None,
            action: None,
            revision: None,
            path: None,
            reason: None,
            dry_run,
            changed: None,
        }
    }

    pub(crate) fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    pub(crate) fn action(mut self, action: &str) -> Self {
        self.action = Some(action.to_string());
        self
    }

    pub(crate) fn revision(mut self, revision: &str) -> Self {
        self.revision = Some(revision.to_string());
        self
    }

    pub(crate) fn path(mut self, path: &Path) -> Self {
        self.path = Some(path.display().to_string());
        self
    }

    pub(crate) fn reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.to_string());
        self
    }

    pub(crate) fn changed(mut self, changed: usize) -> Self {
        self.changed = Some(changed);
        self
    }
}

pub(crate) struct ModelSyncEmitter {
    jsonl: bool,
}

impl ModelSyncEmitter {
    pub(crate) fn new(jsonl: bool) -> Self {
        Self { jsonl }
    }

    pub(crate) fn jsonl(&self) -> bool {
        self.jsonl
    }

    pub(crate) fn emit(&self, event: &ModelSyncEvent, human: impl FnOnce()) -> Result<()> {
        if self.jsonl {
            write_json(event)
        } else {
            human();
            Ok(())
        }
    }
}

pub(crate) fn write_json<T: Serialize>(value: &T) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value).context("serializing model machine output")?;
    writeln!(output).context("writing model machine output")?;
    Ok(())
}
