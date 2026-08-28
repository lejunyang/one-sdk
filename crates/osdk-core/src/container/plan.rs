//! Versioned, deterministic contracts for read-only native mirror plans.
//!
//! A plan records semantic changes and fingerprints only. Native configuration
//! bytes stay in the non-serializable [`NativeConfigSnapshot`] and
//! [`NativeConfigCandidate`] values so diagnostic JSON cannot become a copy of
//! daemon configuration. This module performs no writes.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::redact::RedactedUrl;
use super::reference::RegistryName;
use crate::config::{ContainerRegistryConfig, ContainerResolve};

pub const MIRROR_PLAN_SCHEMA_VERSION: u32 = 1;
pub const MAX_NATIVE_CONFIG_BYTES: usize = 4 * 1024 * 1024;

/// A stable SHA-256 identity over canonical JSON or exact file bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }

    pub fn for_canonical<T: Serialize>(value: &T) -> Result<Self, PlanError> {
        let bytes = serde_json_canonicalizer::to_vec(value)
            .map_err(|_| PlanError::CanonicalSerialization)?;
        Ok(Self::for_bytes(&bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeInputState {
    Missing,
    RegularFile,
}

/// Serializable stale-input identity. It contains no native file contents.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct NativeInputFingerprint {
    pub path: String,
    pub state: NativeInputState,
    pub size: u64,
    pub content_sha256: Option<Fingerprint>,
    pub metadata_sha256: Fingerprint,
}

/// A bounded, no-follow snapshot used only while constructing a plan.
///
/// The custom `Debug` implementation deliberately omits the captured bytes.
pub struct NativeConfigSnapshot {
    fingerprint: NativeInputFingerprint,
    bytes: Option<Vec<u8>>,
}

impl std::fmt::Debug for NativeConfigSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeConfigSnapshot")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl NativeConfigSnapshot {
    /// Capture an existing regular file, or record an explicit missing-file
    /// state. The final path component is never followed through a symlink or
    /// Windows reparse point.
    pub fn capture(path: &Path, max_bytes: usize) -> Result<Self, PlanError> {
        if max_bytes == 0 || max_bytes > MAX_NATIVE_CONFIG_BYTES {
            return Err(PlanError::InvalidSizeLimit);
        }
        let (path, ancestor_identity) = canonical_target_path(path)?;
        let printable_path = path.to_str().ok_or(PlanError::NonUtf8Path)?.to_owned();

        let before = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let metadata_sha256 = Fingerprint::for_canonical(&(
                    printable_path.as_str(),
                    NativeInputState::Missing,
                    ancestor_identity,
                ))?;
                return Ok(Self {
                    fingerprint: NativeInputFingerprint {
                        path: printable_path,
                        state: NativeInputState::Missing,
                        size: 0,
                        content_sha256: None,
                        metadata_sha256,
                    },
                    bytes: None,
                });
            }
            Err(error) => return Err(PlanError::Read(error.kind())),
        };
        ensure_regular_no_link(&before)?;
        if before.len() > max_bytes as u64 {
            return Err(PlanError::ConfigTooLarge);
        }

        let mut file = open_no_follow(&path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => PlanError::InputChanged,
            kind => PlanError::Read(kind),
        })?;
        let opened = file
            .metadata()
            .map_err(|error| PlanError::Read(error.kind()))?;
        ensure_regular_no_link(&opened)?;
        if metadata_identity(&before)? != metadata_identity(&opened)? {
            return Err(PlanError::InputChanged);
        }

        let mut bytes = Vec::with_capacity((opened.len() as usize).min(max_bytes));
        file.by_ref()
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| PlanError::Read(error.kind()))?;
        if bytes.len() > max_bytes {
            return Err(PlanError::ConfigTooLarge);
        }

        let after_handle = file
            .metadata()
            .map_err(|error| PlanError::Read(error.kind()))?;
        let after_path = std::fs::symlink_metadata(&path).map_err(|_| PlanError::InputChanged)?;
        ensure_regular_no_link(&after_path)?;
        let identity = metadata_identity(&opened)?;
        if identity != metadata_identity(&after_handle)?
            || identity != metadata_identity(&after_path)?
            || after_handle.len() != bytes.len() as u64
        {
            return Err(PlanError::InputChanged);
        }

        Ok(Self {
            fingerprint: NativeInputFingerprint {
                path: printable_path,
                state: NativeInputState::RegularFile,
                size: bytes.len() as u64,
                content_sha256: Some(Fingerprint::for_bytes(&bytes)),
                metadata_sha256: Fingerprint::for_canonical(&identity)?,
            },
            bytes: Some(bytes),
        })
    }

    pub fn fingerprint(&self) -> &NativeInputFingerprint {
        &self.fingerprint
    }

    pub fn is_missing(&self) -> bool {
        self.bytes.is_none()
    }

    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeConfigFormat {
    Json,
    Toml,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct NativeCandidateFingerprint {
    pub path: String,
    pub format: NativeConfigFormat,
    pub size: u64,
    pub content_sha256: Fingerprint,
}

/// Candidate bytes accompany a plan in memory but are never serializable.
pub struct NativeConfigCandidate {
    input: NativeInputFingerprint,
    fingerprint: NativeCandidateFingerprint,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for NativeConfigCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeConfigCandidate")
            .field("input", &self.input)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl NativeConfigCandidate {
    pub(crate) fn new(
        snapshot: &NativeConfigSnapshot,
        format: NativeConfigFormat,
        bytes: Vec<u8>,
    ) -> Result<Self, PlanError> {
        if bytes.len() > MAX_NATIVE_CONFIG_BYTES {
            return Err(PlanError::ConfigTooLarge);
        }
        let fingerprint = NativeCandidateFingerprint {
            path: snapshot.fingerprint.path.clone(),
            format,
            size: bytes.len() as u64,
            content_sha256: Fingerprint::for_bytes(&bytes),
        };
        Ok(Self {
            input: snapshot.fingerprint.clone(),
            fingerprint,
            bytes,
        })
    }

    pub fn input(&self) -> &NativeInputFingerprint {
        &self.input
    }

    pub fn fingerprint(&self) -> &NativeCandidateFingerprint {
        &self.fingerprint
    }

    /// Complete validated candidate bytes for an explicit renderer or future
    /// stale-checked apply consumer. Merely building a plan never writes them.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanApplicability {
    Ready,
    ManualOnly,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequiredPrivilege {
    None,
    CurrentUser,
    Root,
    Administrator,
    RemoteAdministrator,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationRequirement {
    None,
    RestartDaemon,
    RecreateBuilder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectiveResolution {
    Upstream,
    Mirror,
    RuntimeDefined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannedCapability {
    Pull,
    Resolve,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockerTargetKind {
    Local,
    Rootless,
    Desktop,
    Remote,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildkitTargetDriver {
    Docker,
    DockerContainer,
    Kubernetes,
    Remote,
    Cloud,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "runtime", rename_all = "kebab-case")]
pub enum MirrorPlanTarget {
    Docker {
        context: Fingerprint,
        kind: DockerTargetKind,
        endpoint: Option<RedactedUrl>,
        version: Option<String>,
    },
    Containerd {
        endpoint: RedactedUrl,
        namespace: String,
        version: Option<String>,
        config_path: Option<String>,
    },
    Buildkit {
        builder: String,
        driver: BuildkitTargetDriver,
        nodes: Vec<Fingerprint>,
        version: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MirrorChange {
    DockerHubMirrors {
        mirrors: Vec<String>,
        effective_resolution: EffectiveResolution,
    },
    ContainerdRegistryHosts {
        registry: RegistryName,
        mirrors: Vec<String>,
        capabilities: BTreeSet<PlannedCapability>,
    },
    ContainerdConfigPath {
        path: String,
    },
    BuildkitRegistryMirrors {
        registry: RegistryName,
        mirrors: Vec<String>,
        effective_resolution: EffectiveResolution,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanWarning {
    AnonymousOnlyNotEnforced,
    DockerHubOnly,
    ResolutionSeparationUnavailable,
    RemoteTarget,
    ManagedDesktop,
    NativeConfigPathRequired,
    ContainerdConfigPathMissing,
    ExistingNativeEntriesPreserved,
    DaemonRestartRequired,
    BuilderRecreateRequired,
    DockerDriverUsesEngineConfiguration,
    ExternalBuilderConfiguration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationStep {
    ParseCompleteJson,
    ParseCompleteToml,
    ValidateDockerDaemonConfig,
    RediscoverRuntimeIdentity,
    RediscoverBuilderIdentity,
    CompareInputFingerprints,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MirrorPlan {
    pub schema_version: u32,
    pub plan_id: Fingerprint,
    pub target: MirrorPlanTarget,
    pub applicability: PlanApplicability,
    pub policy_fingerprint: Fingerprint,
    pub target_fingerprint: Fingerprint,
    pub inputs: Vec<NativeInputFingerprint>,
    pub candidates: Vec<NativeCandidateFingerprint>,
    pub changes: Vec<MirrorChange>,
    pub privilege: RequiredPrivilege,
    pub activation: ActivationRequirement,
    pub validation: BTreeSet<ValidationStep>,
    pub warnings: BTreeSet<PlanWarning>,
}

/// Builder-friendly unsigned plan. Finalization sorts file identities and binds
/// `plan_id` to every semantic field except the ID itself.
pub struct MirrorPlanDraft {
    pub target: MirrorPlanTarget,
    pub applicability: PlanApplicability,
    pub policy_fingerprint: Fingerprint,
    pub inputs: Vec<NativeInputFingerprint>,
    pub candidates: Vec<NativeCandidateFingerprint>,
    pub changes: Vec<MirrorChange>,
    pub privilege: RequiredPrivilege,
    pub activation: ActivationRequirement,
    pub validation: BTreeSet<ValidationStep>,
    pub warnings: BTreeSet<PlanWarning>,
}

impl MirrorPlanDraft {
    pub fn finalize(mut self) -> Result<MirrorPlan, PlanError> {
        self.inputs
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.candidates
            .sort_by(|left, right| left.path.cmp(&right.path));
        let target_fingerprint = Fingerprint::for_canonical(&self.target)?;
        let unsigned = UnsignedPlan {
            schema_version: MIRROR_PLAN_SCHEMA_VERSION,
            target: &self.target,
            applicability: self.applicability,
            policy_fingerprint: &self.policy_fingerprint,
            target_fingerprint: &target_fingerprint,
            inputs: &self.inputs,
            candidates: &self.candidates,
            changes: &self.changes,
            privilege: self.privilege,
            activation: self.activation,
            validation: &self.validation,
            warnings: &self.warnings,
        };
        let plan_id = Fingerprint::for_canonical(&unsigned)?;
        Ok(MirrorPlan {
            schema_version: MIRROR_PLAN_SCHEMA_VERSION,
            plan_id,
            target: self.target,
            applicability: self.applicability,
            policy_fingerprint: self.policy_fingerprint,
            target_fingerprint,
            inputs: self.inputs,
            candidates: self.candidates,
            changes: self.changes,
            privilege: self.privilege,
            activation: self.activation,
            validation: self.validation,
            warnings: self.warnings,
        })
    }
}

#[derive(Serialize)]
struct UnsignedPlan<'a> {
    schema_version: u32,
    target: &'a MirrorPlanTarget,
    applicability: PlanApplicability,
    policy_fingerprint: &'a Fingerprint,
    target_fingerprint: &'a Fingerprint,
    inputs: &'a [NativeInputFingerprint],
    candidates: &'a [NativeCandidateFingerprint],
    changes: &'a [MirrorChange],
    privilege: RequiredPrivilege,
    activation: ActivationRequirement,
    validation: &'a BTreeSet<ValidationStep>,
    warnings: &'a BTreeSet<PlanWarning>,
}

/// In-memory result: serializable semantic plan plus non-serializable candidate
/// bytes. No file is written by constructing this value.
#[derive(Debug)]
pub struct MirrorPlanBundle {
    pub plan: MirrorPlan,
    pub candidates: Vec<NativeConfigCandidate>,
}

pub fn policy_fingerprint(
    registry: &RegistryName,
    policy: &ContainerRegistryConfig,
) -> Result<Fingerprint, PlanError> {
    #[derive(Serialize)]
    struct Policy<'a> {
        registry: &'a RegistryName,
        mirrors: &'a [String],
        anonymous_only: bool,
        resolve: ContainerResolve,
    }
    Fingerprint::for_canonical(&Policy {
        registry,
        mirrors: &policy.mirrors,
        anonymous_only: policy.anonymous_only,
        resolve: policy.resolve,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("native configuration path has no final file name")]
    InvalidPath,
    #[error("native configuration path is not valid UTF-8")]
    NonUtf8Path,
    #[error("native configuration size limit must be positive")]
    InvalidSizeLimit,
    #[error("native configuration is not a regular no-follow file")]
    UnsafeFileType,
    #[error("native configuration exceeds the bounded snapshot limit")]
    ConfigTooLarge,
    #[error("native configuration changed while it was being inspected")]
    InputChanged,
    #[error("could not read native configuration ({0:?})")]
    Read(io::ErrorKind),
    #[error("could not canonicalize plan data")]
    CanonicalSerialization,
}

fn canonical_target_path(path: &Path) -> Result<(PathBuf, Vec<(String, String)>), PlanError> {
    use std::path::Component;

    if path.file_name().is_none()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(PlanError::InvalidPath);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PlanError::Read(error.kind()))?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .ok_or(PlanError::InvalidPath)?
        .to_os_string();
    // Resolve only ancestors. Resolving `absolute` itself would follow the
    // final symlink before the no-follow metadata/open checks below.
    let mut existing = absolute.parent().ok_or(PlanError::InvalidPath)?;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or(PlanError::InvalidPath)?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or(PlanError::InvalidPath)?;
    }
    let existing = dunce::canonicalize(existing).map_err(|error| PlanError::Read(error.kind()))?;
    let existing_metadata =
        std::fs::metadata(&existing).map_err(|error| PlanError::Read(error.kind()))?;
    let ancestor_identity = metadata_identity(&existing_metadata)?;
    let mut canonical = existing;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    canonical.push(file_name);
    Ok((canonical, ancestor_identity))
}

fn ensure_regular_no_link(metadata: &std::fs::Metadata) -> Result<(), PlanError> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || is_reparse(metadata)
    {
        return Err(PlanError::UnsafeFileType);
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn metadata_identity(metadata: &std::fs::Metadata) -> Result<Vec<(String, String)>, PlanError> {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos().to_string())
        .unwrap_or_default();
    let mut values = vec![
        ("len".to_owned(), metadata.len().to_string()),
        ("modified-nanos".to_owned(), modified),
        (
            "readonly".to_owned(),
            metadata.permissions().readonly().to_string(),
        ),
    ];
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        values.extend([
            ("dev".to_owned(), metadata.dev().to_string()),
            ("ino".to_owned(), metadata.ino().to_string()),
            ("mode".to_owned(), metadata.mode().to_string()),
        ]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        values.extend([
            (
                "attributes".to_owned(),
                metadata.file_attributes().to_string(),
            ),
            (
                "creation-time".to_owned(),
                metadata.creation_time().to_string(),
            ),
            (
                "last-write-time".to_owned(),
                metadata.last_write_time().to_string(),
            ),
        ]);
    }
    values.sort();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(input: NativeInputFingerprint) -> MirrorPlanDraft {
        let target = MirrorPlanTarget::Docker {
            context: Fingerprint::for_bytes(b"default"),
            kind: DockerTargetKind::Local,
            endpoint: None,
            version: Some("28.0.0".into()),
        };
        MirrorPlanDraft {
            target,
            applicability: PlanApplicability::Ready,
            policy_fingerprint: Fingerprint::for_bytes(b"policy"),
            inputs: vec![input],
            candidates: Vec::new(),
            changes: vec![MirrorChange::DockerHubMirrors {
                mirrors: vec!["https://mirror.example/".into()],
                effective_resolution: EffectiveResolution::RuntimeDefined,
            }],
            privilege: RequiredPrivilege::Root,
            activation: ActivationRequirement::RestartDaemon,
            validation: BTreeSet::from([ValidationStep::ParseCompleteJson]),
            warnings: BTreeSet::new(),
        }
    }

    #[test]
    fn snapshot_is_bounded_nofollow_and_records_missing_state() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("daemon.json");
        std::fs::write(&file, b"{\"debug\":true}").unwrap();
        let snapshot = NativeConfigSnapshot::capture(&file, 64).unwrap();
        assert_eq!(snapshot.fingerprint.state, NativeInputState::RegularFile);
        assert_eq!(snapshot.bytes(), Some(&b"{\"debug\":true}"[..]));
        assert_eq!(
            NativeConfigSnapshot::capture(&file, 4).unwrap_err(),
            PlanError::ConfigTooLarge
        );
        assert_eq!(
            NativeConfigSnapshot::capture(&file, MAX_NATIVE_CONFIG_BYTES + 1).unwrap_err(),
            PlanError::InvalidSizeLimit
        );

        let missing =
            NativeConfigSnapshot::capture(&temporary.path().join("missing.json"), 64).unwrap();
        assert_eq!(missing.fingerprint.state, NativeInputState::Missing);
        assert!(missing.bytes().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlink_without_following_it() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("secret");
        let link = temporary.path().join("daemon.json");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(
            NativeConfigSnapshot::capture(&link, 64).unwrap_err(),
            PlanError::UnsafeFileType
        );
    }

    #[test]
    fn candidate_debug_and_plan_json_do_not_contain_native_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("daemon.json");
        std::fs::write(&file, b"{\"token\":\"top-secret\"}").unwrap();
        let snapshot = NativeConfigSnapshot::capture(&file, 128).unwrap();
        let candidate = NativeConfigCandidate::new(
            &snapshot,
            NativeConfigFormat::Json,
            b"{\"token\":\"changed-secret\"}".to_vec(),
        )
        .unwrap();
        let mut draft = draft(snapshot.fingerprint().clone());
        draft.candidates.push(candidate.fingerprint().clone());
        let plan = draft.finalize().unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        let debug = format!("{candidate:?}");
        for secret in ["top-secret", "changed-secret"] {
            assert!(!json.contains(secret));
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn deterministic_id_binds_semantics_but_not_input_order() {
        let temporary = tempfile::tempdir().unwrap();
        let first = NativeConfigSnapshot::capture(&temporary.path().join("a"), 64)
            .unwrap()
            .fingerprint()
            .clone();
        let second = NativeConfigSnapshot::capture(&temporary.path().join("b"), 64)
            .unwrap()
            .fingerprint()
            .clone();
        let mut left = draft(first.clone());
        left.inputs.push(second.clone());
        let mut right = draft(second);
        right.inputs.push(first);
        let left = left.finalize().unwrap();
        let right = right.finalize().unwrap();
        assert_eq!(left.plan_id, right.plan_id);

        let mut changed = draft(left.inputs[0].clone());
        changed.applicability = PlanApplicability::ManualOnly;
        assert_ne!(left.plan_id, changed.finalize().unwrap().plan_id);
    }

    #[test]
    fn policy_fingerprint_binds_order_and_resolution() {
        let registry = RegistryName::parse("docker.io").unwrap();
        let mut policy = ContainerRegistryConfig {
            mirrors: vec!["https://a.example/".into(), "https://b.example/".into()],
            anonymous_only: true,
            resolve: ContainerResolve::Upstream,
        };
        let original = policy_fingerprint(&registry, &policy).unwrap();
        policy.mirrors.reverse();
        assert_ne!(original, policy_fingerprint(&registry, &policy).unwrap());
        policy.mirrors.reverse();
        policy.resolve = ContainerResolve::Mirror;
        assert_ne!(original, policy_fingerprint(&registry, &policy).unwrap());
    }
}
