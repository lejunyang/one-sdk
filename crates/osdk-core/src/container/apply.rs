//! Stale-checked, atomic application of one ready native mirror candidate.

use std::fs::{OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::plan::{
    Fingerprint, MirrorPlanBundle, NativeConfigCandidate, NativeConfigFormat,
    NativeInputFingerprint, NativeInputState, PlanApplicability, PlanError,
    MAX_NATIVE_CONFIG_BYTES,
};

pub const MIRROR_APPLY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MirrorApplyReport {
    pub schema_version: u32,
    pub plan_id: Fingerprint,
    pub path: String,
    pub content_sha256: Fingerprint,
    pub backup_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MirrorApplyError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("only a ready mirror plan can be applied")]
    PlanNotReady,
    #[error("a ready mirror plan must contain exactly one native candidate")]
    CandidateCount,
    #[error("native candidate identity does not match the mirror plan")]
    CandidateMismatch,
    #[error("native configuration changed after the mirror plan was created")]
    StaleInput,
    #[error("native candidate is not valid for its declared format")]
    InvalidCandidate,
    #[error("could not write native configuration ({0:?})")]
    Write(io::ErrorKind),
}

/// Apply exactly one candidate while holding an osdk-owned cross-process lock.
///
/// The plan and candidate hashes are verified first. Once locked, the native
/// input is captured again with the same no-follow and size constraints used
/// by planning. Candidate bytes are parsed before a sibling file is flushed
/// and atomically installed. Native services are deliberately not restarted.
pub fn apply_mirror_plan(
    bundle: &MirrorPlanBundle,
    lock_path: &Path,
) -> Result<MirrorApplyReport, MirrorApplyError> {
    bundle.plan.validate()?;
    if bundle.plan.applicability != PlanApplicability::Ready {
        return Err(MirrorApplyError::PlanNotReady);
    }
    let [candidate] = bundle.candidates.as_slice() else {
        return Err(MirrorApplyError::CandidateCount);
    };
    if bundle.plan.inputs.len() != 1
        || bundle.plan.inputs.first() != Some(candidate.input())
        || bundle.plan.candidates.first() != Some(candidate.fingerprint())
        || candidate.fingerprint().size != candidate.bytes().len() as u64
        || candidate.fingerprint().content_sha256 != Fingerprint::for_bytes(candidate.bytes())
    {
        return Err(MirrorApplyError::CandidateMismatch);
    }
    validate_candidate(candidate)?;

    let _lock = crate::lock::FileLock::acquire(lock_path)
        .map_err(|_| MirrorApplyError::Write(io::ErrorKind::Other))?;
    let target = Path::new(&candidate.fingerprint().path);
    let current = match super::plan::NativeConfigSnapshot::capture(target, MAX_NATIVE_CONFIG_BYTES)
    {
        Ok(current) => current,
        Err(PlanError::InputChanged) => return Err(MirrorApplyError::StaleInput),
        Err(error) => return Err(MirrorApplyError::Plan(error)),
    };
    if current.fingerprint() != candidate.input() {
        return Err(MirrorApplyError::StaleInput);
    }

    let permissions = target
        .metadata()
        .ok()
        .map(|metadata| metadata.permissions());
    let backup_path = current
        .bytes()
        .map(|bytes| write_backup(target, bytes, permissions.as_ref()))
        .transpose()?;
    atomic_write(
        target,
        candidate.bytes(),
        permissions.as_ref(),
        candidate.input(),
    )?;
    Ok(MirrorApplyReport {
        schema_version: MIRROR_APPLY_SCHEMA_VERSION,
        plan_id: bundle.plan.plan_id.clone(),
        path: candidate.fingerprint().path.clone(),
        content_sha256: candidate.fingerprint().content_sha256.clone(),
        backup_path: backup_path
            .map(|path| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or(MirrorApplyError::Plan(PlanError::NonUtf8Path))
            })
            .transpose()?,
    })
}

fn validate_candidate(candidate: &NativeConfigCandidate) -> Result<(), MirrorApplyError> {
    match candidate.fingerprint().format {
        NativeConfigFormat::Json => serde_json::from_slice::<serde_json::Value>(candidate.bytes())
            .map(|_| ())
            .map_err(|_| MirrorApplyError::InvalidCandidate),
        NativeConfigFormat::Toml => std::str::from_utf8(candidate.bytes())
            .ok()
            .and_then(|value| value.parse::<toml::Value>().ok())
            .map(|_| ())
            .ok_or(MirrorApplyError::InvalidCandidate),
    }
}

fn atomic_write(
    target: &Path,
    bytes: &[u8],
    permissions: Option<&Permissions>,
    expected_input: &NativeInputFingerprint,
) -> Result<(), MirrorApplyError> {
    let parent = target
        .parent()
        .ok_or(MirrorApplyError::Write(io::ErrorKind::InvalidInput))?;
    let mut temporary = temporary_path(parent, target);
    let mut file = None;
    for attempt in 0..1024u32 {
        temporary.set_extension(format!("osdk-tmp-{}-{attempt}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(opened) => {
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(MirrorApplyError::Write(error.kind())),
        }
    }
    let mut file = file.ok_or(MirrorApplyError::Write(io::ErrorKind::AlreadyExists))?;
    if let Err(error) = file.write_all(bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(MirrorApplyError::Write(error.kind()));
    }
    if let Some(permissions) = permissions {
        if let Err(error) = file.set_permissions(permissions.clone()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(MirrorApplyError::Write(error.kind()));
        }
    }
    if let Err(error) = file.sync_all() {
        let _ = std::fs::remove_file(&temporary);
        return Err(MirrorApplyError::Write(error.kind()));
    }
    drop(file);

    let unchanged = match expected_input.state {
        NativeInputState::RegularFile => {
            super::plan::NativeConfigSnapshot::capture(target, MAX_NATIVE_CONFIG_BYTES)
                .is_ok_and(|snapshot| snapshot.fingerprint() == expected_input)
        }
        NativeInputState::Missing => std::fs::symlink_metadata(target)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
    };
    if !unchanged {
        let _ = std::fs::remove_file(&temporary);
        return Err(MirrorApplyError::StaleInput);
    }
    if let Err(error) = atomic_replace(&temporary, target).and_then(|_| sync_parent(parent)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(MirrorApplyError::Write(error.kind()));
    }
    Ok(())
}

fn write_backup(
    target: &Path,
    bytes: &[u8],
    source_permissions: Option<&Permissions>,
) -> Result<PathBuf, MirrorApplyError> {
    let parent = target
        .parent()
        .ok_or(MirrorApplyError::Write(io::ErrorKind::InvalidInput))?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(MirrorApplyError::Plan(PlanError::NonUtf8Path))?;
    for attempt in 0..1024u32 {
        let backup = parent.join(format!(
            ".{name}.osdk-backup-{}-{attempt}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&backup) {
            Ok(mut file) => {
                let result = file
                    .write_all(bytes)
                    .and_then(|_| {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mut permissions = source_permissions
                                .cloned()
                                .unwrap_or_else(|| Permissions::from_mode(0o600));
                            permissions.set_mode(permissions.mode() & 0o700);
                            file.set_permissions(permissions)?;
                        }
                        #[cfg(not(unix))]
                        if let Some(permissions) = source_permissions {
                            file.set_permissions(permissions.clone())?;
                        }
                        Ok(())
                    })
                    .and_then(|_| file.sync_all());
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&backup);
                    return Err(MirrorApplyError::Write(error.kind()));
                }
                return Ok(backup);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(MirrorApplyError::Write(error.kind())),
        }
    }
    Err(MirrorApplyError::Write(io::ErrorKind::AlreadyExists))
}

fn temporary_path(parent: &Path, target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("native-config");
    parent.join(format!(".{name}"))
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::container::plan::{
        ActivationRequirement, DockerTargetKind, MirrorPlanDraft, MirrorPlanTarget,
        NativeConfigSnapshot, RequiredPrivilege, ValidationStep,
    };

    fn ready_bundle(path: &Path, bytes: &[u8]) -> MirrorPlanBundle {
        let snapshot = NativeConfigSnapshot::capture(path, MAX_NATIVE_CONFIG_BYTES).unwrap();
        let candidate =
            NativeConfigCandidate::new(&snapshot, NativeConfigFormat::Json, bytes.to_vec())
                .unwrap();
        let plan = MirrorPlanDraft {
            target: MirrorPlanTarget::Docker {
                context: Fingerprint::for_bytes(b"default"),
                kind: DockerTargetKind::Local,
                endpoint: None,
                version: Some("29.0.0".into()),
            },
            applicability: PlanApplicability::Ready,
            policy_fingerprint: Fingerprint::for_bytes(b"policy"),
            inputs: vec![snapshot.fingerprint().clone()],
            candidates: vec![candidate.fingerprint().clone()],
            changes: Vec::new(),
            privilege: RequiredPrivilege::CurrentUser,
            activation: ActivationRequirement::RestartDaemon,
            validation: BTreeSet::from([ValidationStep::CompareInputFingerprints]),
            warnings: BTreeSet::new(),
        }
        .finalize()
        .unwrap();
        MirrorPlanBundle {
            plan,
            candidates: vec![candidate],
        }
    }

    #[test]
    fn applies_one_ready_candidate_atomically() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("daemon.json");
        std::fs::write(&target, b"{\"debug\":true}").unwrap();
        let bundle = ready_bundle(
            &target,
            b"{\"registry-mirrors\":[\"https://mirror.example/\"]}",
        );
        let report = apply_mirror_plan(&bundle, &temporary.path().join("apply.lock")).unwrap();
        assert_eq!(report.plan_id, bundle.plan.plan_id);
        assert!(report
            .backup_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_file()));
        assert_eq!(
            std::fs::read(report.backup_path.as_deref().unwrap()).unwrap(),
            b"{\"debug\":true}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let backup = report.backup_path.as_deref().unwrap();
            assert_eq!(
                std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read(&target).unwrap(),
            bundle.candidates[0].bytes()
        );
    }

    #[test]
    fn creates_a_missing_target_without_inventing_a_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let lock_dir = temporary.path().join("locks");
        std::fs::create_dir(&lock_dir).unwrap();
        let target = temporary.path().join("daemon.json");
        let bundle = ready_bundle(&target, b"{\"registry-mirrors\":[]}");
        let report = apply_mirror_plan(&bundle, &lock_dir.join("apply.lock")).unwrap();
        assert_eq!(report.backup_path, None);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            bundle.candidates[0].bytes()
        );
    }

    #[test]
    fn rejects_stale_inputs_without_overwriting_them() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("daemon.json");
        std::fs::write(&target, b"{}").unwrap();
        let bundle = ready_bundle(&target, b"{\"debug\":true}");
        std::fs::write(&target, b"{\"changed\":true}").unwrap();
        assert_eq!(
            apply_mirror_plan(&bundle, &temporary.path().join("apply.lock")).unwrap_err(),
            MirrorApplyError::StaleInput
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"changed\":true}");
        assert_eq!(
            std::fs::read_dir(temporary.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("osdk-backup"))
                .count(),
            0
        );
    }

    #[test]
    fn rejects_tampered_non_ready_and_mismatched_candidates() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("daemon.json");
        std::fs::write(&target, b"{}").unwrap();
        let mut bundle = ready_bundle(&target, b"{}");
        bundle.plan.applicability = PlanApplicability::ManualOnly;
        assert_eq!(
            apply_mirror_plan(&bundle, &temporary.path().join("apply.lock")).unwrap_err(),
            MirrorApplyError::Plan(PlanError::InvalidPlan)
        );

        let snapshot = NativeConfigSnapshot::capture(&target, MAX_NATIVE_CONFIG_BYTES).unwrap();
        let candidate = NativeConfigCandidate::new(
            &snapshot,
            NativeConfigFormat::Json,
            b"{\"other\":true}".to_vec(),
        )
        .unwrap();
        let mut manual = ready_bundle(&target, b"{}");
        manual.plan = MirrorPlanDraft {
            target: manual.plan.target.clone(),
            applicability: PlanApplicability::ManualOnly,
            policy_fingerprint: manual.plan.policy_fingerprint.clone(),
            inputs: manual.plan.inputs.clone(),
            candidates: manual.plan.candidates.clone(),
            changes: manual.plan.changes.clone(),
            privilege: manual.plan.privilege,
            activation: manual.plan.activation,
            validation: manual.plan.validation.clone(),
            warnings: manual.plan.warnings.clone(),
        }
        .finalize()
        .unwrap();
        assert_eq!(
            apply_mirror_plan(&manual, &temporary.path().join("apply.lock")).unwrap_err(),
            MirrorApplyError::PlanNotReady
        );

        let mut mismatch = ready_bundle(&target, b"{}");
        mismatch.candidates = vec![candidate];
        assert_eq!(
            apply_mirror_plan(&mismatch, &temporary.path().join("apply.lock")).unwrap_err(),
            MirrorApplyError::CandidateMismatch
        );
    }
}
