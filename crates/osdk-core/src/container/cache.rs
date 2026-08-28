//! Read-only, secret-safe native cache status queries.
//!
//! Native cache ownership remains with Docker Engine, the selected BuildKit
//! builder, or containerd. Queries use only supported native CLI surfaces and
//! never inspect `/var/lib/docker`, containerd roots, BuildKit state, or any
//! other implementation-private storage. Raw command output is discarded after
//! conversion into the closed, serializable contract below.

use std::collections::BTreeSet;

use semver::Version;
use serde::Serialize;
use serde_json::Value;

use super::buildkit::BuildxBuilderSelector;
use super::redact::{CommandPurpose, NativeProgram};
use super::report::{DiagnosticEvidence, RuntimeKind};
use super::runtime::ProbeCommand;
use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

/// Version of the stable native-cache status JSON contract.
pub const NATIVE_CACHE_STATUS_SCHEMA_VERSION: u32 = 1;

// `docker buildx du --format=json` became a documented NDJSON interface in
// Buildx v0.28.0. Older releases only expose human-oriented output, which this
// module deliberately refuses to scrape.
const MINIMUM_BUILDX_JSON_DU_VERSION: Version = Version::new(0, 28, 0);

/// The native component that owns the reported cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeCacheOwner {
    DockerEngine,
    BuildkitBuilder,
    Containerd,
}

/// Outcome of a bounded native cache query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheQueryStatus {
    Available,
    NotInstalled,
    PermissionDenied,
    Unreachable,
    TimedOut,
    Unsupported,
    UnsupportedVersion,
    OutputTruncated,
    InvalidOutput,
    CommandFailed,
}

/// A stable cache category. Unknown native labels are rejected instead of
/// entering the output contract as free-form strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeCacheRecordKind {
    Images,
    Containers,
    LocalVolumes,
    BuildCache,
}

/// Aggregate facts for one stable cache category.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NativeCacheRecord {
    pub kind: NativeCacheRecordKind,
    pub count: u64,
    pub active: u64,
    /// Total bytes reported by the owning native component.
    pub total: u64,
    /// Bytes the owning native component identifies as reclaimable.
    pub reclaimable: u64,
}

/// Stable, deterministic, secret-safe native cache status.
///
/// This type intentionally has no free-form serializable strings. In
/// particular, builder names, cache record IDs and descriptions, native paths,
/// stderr, and unparsed stdout cannot enter the contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NativeCacheStatus {
    pub schema_version: u32,
    pub runtime: RuntimeKind,
    pub owner: NativeCacheOwner,
    pub status: CacheQueryStatus,
    pub records: Vec<NativeCacheRecord>,
    /// Sum of `records[].total`, in bytes.
    pub total: u64,
    /// Sum of `records[].reclaimable`, in bytes.
    pub reclaimable: u64,
    pub evidence: BTreeSet<DiagnosticEvidence>,
}

impl NativeCacheStatus {
    fn terminal(
        runtime: RuntimeKind,
        owner: NativeCacheOwner,
        status: CacheQueryStatus,
        evidence: BTreeSet<DiagnosticEvidence>,
    ) -> Self {
        Self {
            schema_version: NATIVE_CACHE_STATUS_SCHEMA_VERSION,
            runtime,
            owner,
            status,
            records: Vec::new(),
            total: 0,
            reclaimable: 0,
            evidence,
        }
    }

    fn available(
        runtime: RuntimeKind,
        owner: NativeCacheOwner,
        mut records: Vec<NativeCacheRecord>,
        evidence: BTreeSet<DiagnosticEvidence>,
    ) -> Result<Self, CacheParseError> {
        records.sort_by_key(|record| record.kind);
        let mut total = 0_u64;
        let mut reclaimable = 0_u64;
        for record in &records {
            if record.active > record.count || record.reclaimable > record.total {
                return Err(CacheParseError::Invalid);
            }
            total = total
                .checked_add(record.total)
                .ok_or(CacheParseError::Overflow)?;
            reclaimable = reclaimable
                .checked_add(record.reclaimable)
                .ok_or(CacheParseError::Overflow)?;
        }
        Ok(Self {
            schema_version: NATIVE_CACHE_STATUS_SCHEMA_VERSION,
            runtime,
            owner,
            status: CacheQueryStatus::Available,
            records,
            total,
            reclaimable,
            evidence,
        })
    }
}

/// Read-only Docker Engine cache query.
#[derive(Clone, Copy, Debug, Default)]
pub struct DockerCacheQuery;

impl DockerCacheQuery {
    /// Query aggregate Docker Engine usage through the documented formatter.
    pub fn query(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> NativeCacheStatus {
        let probe = ProbeCommand::new(
            NativeProgram::Docker,
            CommandPurpose::CacheStatus,
            CommandSpec::new("docker").args(["system", "df", "--format", "{{json .}}"]),
        );
        let mut evidence = BTreeSet::new();
        evidence.insert(DiagnosticEvidence::Command(probe.evidence().clone()));
        let outcome = probe.execute(runner, limits);
        if let Some(status) = failed_outcome_status(&outcome, ProbeFlavor::Docker) {
            return NativeCacheStatus::terminal(
                RuntimeKind::Docker,
                NativeCacheOwner::DockerEngine,
                status,
                evidence,
            );
        }

        let output = outcome.output().expect("successful outcome has output");
        match parse_docker_cache_records(&output.stdout).and_then(|records| {
            NativeCacheStatus::available(
                RuntimeKind::Docker,
                NativeCacheOwner::DockerEngine,
                records,
                evidence.clone(),
            )
        }) {
            Ok(status) => status,
            Err(_) => NativeCacheStatus::terminal(
                RuntimeKind::Docker,
                NativeCacheOwner::DockerEngine,
                CacheQueryStatus::InvalidOutput,
                evidence,
            ),
        }
    }
}

/// Read-only BuildKit cache query backed by Docker Buildx.
#[derive(Clone, Default)]
pub struct BuildxCacheQuery {
    selector: BuildxBuilderSelector,
}

impl std::fmt::Debug for BuildxCacheQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuildxCacheQuery")
            .field(
                "selector",
                &self.selector.as_name().map(|_| "named").unwrap_or("auto"),
            )
            .finish()
    }
}

impl BuildxCacheQuery {
    pub fn new(selector: BuildxBuilderSelector) -> Self {
        Self { selector }
    }

    pub fn selector(&self) -> &BuildxBuilderSelector {
        &self.selector
    }

    /// Query the selected builder through documented Buildx NDJSON output.
    ///
    /// A local version probe is mandatory: invoking `du --format=json` without
    /// first proving Buildx v0.28.0 or newer would risk treating a human format
    /// as a machine contract. No `--bootstrap` flag is used.
    pub fn query(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> NativeCacheStatus {
        let version_probe = ProbeCommand::new(
            NativeProgram::Buildx,
            CommandPurpose::Version,
            CommandSpec::new("docker").args(["buildx", "version"]),
        );
        let mut evidence = BTreeSet::new();
        evidence.insert(DiagnosticEvidence::Command(
            version_probe.evidence().clone(),
        ));
        let version_outcome = version_probe.execute(runner, limits);
        if let Some(status) = failed_outcome_status(&version_outcome, ProbeFlavor::BuildxVersion) {
            return NativeCacheStatus::terminal(
                RuntimeKind::Buildkit,
                NativeCacheOwner::BuildkitBuilder,
                status,
                evidence,
            );
        }

        let version_output = version_outcome
            .output()
            .expect("successful outcome has output");
        let version = parse_buildx_version(&version_output.stdout);
        if version
            .as_ref()
            .is_none_or(|version| version < &MINIMUM_BUILDX_JSON_DU_VERSION)
        {
            return NativeCacheStatus::terminal(
                RuntimeKind::Buildkit,
                NativeCacheOwner::BuildkitBuilder,
                CacheQueryStatus::UnsupportedVersion,
                evidence,
            );
        }

        let mut command = CommandSpec::new("docker").args(["buildx", "du", "--format=json"]);
        if let Some(name) = self.selector.as_name() {
            // `BuildxBuilderSelector::named` has already restricted this to one
            // bounded ASCII argument; it is never copied into report output.
            command = command.args(["--builder", name]);
        }
        let du_probe =
            ProbeCommand::new(NativeProgram::Buildx, CommandPurpose::CacheStatus, command);
        evidence.insert(DiagnosticEvidence::Command(du_probe.evidence().clone()));
        let du_outcome = du_probe.execute(runner, limits);
        if let Some(status) = failed_outcome_status(&du_outcome, ProbeFlavor::BuildxDu) {
            return NativeCacheStatus::terminal(
                RuntimeKind::Buildkit,
                NativeCacheOwner::BuildkitBuilder,
                status,
                evidence,
            );
        }

        let output = du_outcome.output().expect("successful outcome has output");
        match parse_buildx_cache_records(&output.stdout).and_then(|records| {
            NativeCacheStatus::available(
                RuntimeKind::Buildkit,
                NativeCacheOwner::BuildkitBuilder,
                records,
                evidence.clone(),
            )
        }) {
            Ok(status) => status,
            Err(_) => NativeCacheStatus::terminal(
                RuntimeKind::Buildkit,
                NativeCacheOwner::BuildkitBuilder,
                CacheQueryStatus::InvalidOutput,
                evidence,
            ),
        }
    }
}

/// Explicitly unsupported containerd cache query.
///
/// containerd has namespaces, snapshots, content, and CRI views but no single
/// selected, stable aggregate contract equivalent to the two CLI interfaces
/// above. Returning `unsupported` is safer than walking its private roots.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContainerdCacheQuery;

impl ContainerdCacheQuery {
    pub fn query(&self, _runner: &dyn CommandRunner, _limits: CaptureLimits) -> NativeCacheStatus {
        NativeCacheStatus::terminal(
            RuntimeKind::Containerd,
            NativeCacheOwner::Containerd,
            CacheQueryStatus::Unsupported,
            BTreeSet::new(),
        )
    }
}

#[derive(Clone, Copy)]
enum ProbeFlavor {
    Docker,
    BuildxVersion,
    BuildxDu,
}

fn failed_outcome_status(
    outcome: &CommandOutcome,
    flavor: ProbeFlavor,
) -> Option<CacheQueryStatus> {
    match outcome {
        CommandOutcome::NotInstalled => Some(CacheQueryStatus::NotInstalled),
        CommandOutcome::PermissionDenied => Some(CacheQueryStatus::PermissionDenied),
        CommandOutcome::TimedOut { .. } => Some(CacheQueryStatus::TimedOut),
        CommandOutcome::SpawnFailed { .. } | CommandOutcome::ExecutionFailed { .. } => {
            Some(classify_failed_output(outcome, flavor))
        }
        CommandOutcome::Exited { status, output } => {
            if output.stdout_truncated || output.stderr_truncated {
                Some(CacheQueryStatus::OutputTruncated)
            } else if status.success() {
                None
            } else {
                Some(classify_failed_output(outcome, flavor))
            }
        }
    }
}

fn classify_failed_output(outcome: &CommandOutcome, flavor: ProbeFlavor) -> CacheQueryStatus {
    let Some(output) = outcome.output() else {
        return CacheQueryStatus::CommandFailed;
    };
    let mut text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.push(' ');
    text.push_str(&String::from_utf8_lossy(&output.stderr).to_ascii_lowercase());

    if text.contains("permission denied") || text.contains("access is denied") {
        CacheQueryStatus::PermissionDenied
    } else if text.contains("cannot connect")
        || text.contains("connection refused")
        || text.contains("connection error")
        || text.contains("deadline exceeded")
        || text.contains("context deadline")
        || text.contains("timed out")
    {
        CacheQueryStatus::Unreachable
    } else if matches!(flavor, ProbeFlavor::BuildxVersion | ProbeFlavor::BuildxDu)
        && (text.contains("is not a docker command")
            || text.contains("unknown command \"buildx\"")
            || text.contains("docker-buildx: executable file not found")
            || text.contains("docker: 'buildx' is not a docker command"))
    {
        CacheQueryStatus::NotInstalled
    } else if matches!(flavor, ProbeFlavor::BuildxDu)
        && (text.contains("unknown flag: --format")
            || text.contains("unknown flag: --format=json")
            || text.contains("unknown shorthand flag"))
    {
        CacheQueryStatus::UnsupportedVersion
    } else {
        CacheQueryStatus::CommandFailed
    }
}

fn parse_buildx_version(bytes: &[u8]) -> Option<Version> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .find_map(super::parse_vendor_version)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheParseError {
    Invalid,
    Overflow,
}

fn parse_docker_cache_records(bytes: &[u8]) -> Result<Vec<NativeCacheRecord>, CacheParseError> {
    let text = std::str::from_utf8(bytes).map_err(|_| CacheParseError::Invalid)?;
    let mut records = Vec::new();
    let mut kinds = BTreeSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let value: Value = serde_json::from_str(line).map_err(|_| CacheParseError::Invalid)?;
        let kind = match required_string(&value, "Type")? {
            "Images" => NativeCacheRecordKind::Images,
            "Containers" => NativeCacheRecordKind::Containers,
            "Local Volumes" => NativeCacheRecordKind::LocalVolumes,
            "Build Cache" => NativeCacheRecordKind::BuildCache,
            _ => return Err(CacheParseError::Invalid),
        };
        if !kinds.insert(kind) {
            return Err(CacheParseError::Invalid);
        }
        let count = required_u64(&value, "TotalCount")?;
        let active = required_u64(&value, "Active")?;
        let total = required_human_bytes(&value, "Size")?;
        let reclaimable = required_reclaimable_bytes(&value, "Reclaimable")?;
        if active > count || reclaimable > total {
            return Err(CacheParseError::Invalid);
        }
        records.push(NativeCacheRecord {
            kind,
            count,
            active,
            total,
            reclaimable,
        });
    }
    if records.is_empty() {
        return Err(CacheParseError::Invalid);
    }
    Ok(records)
}

fn parse_buildx_cache_records(bytes: &[u8]) -> Result<Vec<NativeCacheRecord>, CacheParseError> {
    let text = std::str::from_utf8(bytes).map_err(|_| CacheParseError::Invalid)?;
    let mut count = 0_u64;
    let mut active = 0_u64;
    let mut total = 0_u64;
    let mut reclaimable = 0_u64;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let value: Value = serde_json::from_str(line).map_err(|_| CacheParseError::Invalid)?;
        let size = required_u64(&value, "Size")?;
        let is_reclaimable = value
            .get("Reclaimable")
            .and_then(Value::as_bool)
            .ok_or(CacheParseError::Invalid)?;
        count = count.checked_add(1).ok_or(CacheParseError::Overflow)?;
        total = total.checked_add(size).ok_or(CacheParseError::Overflow)?;
        if is_reclaimable {
            reclaimable = reclaimable
                .checked_add(size)
                .ok_or(CacheParseError::Overflow)?;
        } else {
            active = active.checked_add(1).ok_or(CacheParseError::Overflow)?;
        }
    }
    Ok(vec![NativeCacheRecord {
        kind: NativeCacheRecordKind::BuildCache,
        count,
        active,
        total,
        reclaimable,
    }])
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, CacheParseError> {
    value
        .as_object()
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .ok_or(CacheParseError::Invalid)
}

fn required_u64(value: &Value, key: &str) -> Result<u64, CacheParseError> {
    let value = value
        .as_object()
        .and_then(|object| object.get(key))
        .ok_or(CacheParseError::Invalid)?;
    match value {
        Value::Number(number) => number.as_u64().ok_or(CacheParseError::Invalid),
        Value::String(number)
            if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            number.parse().map_err(|_| CacheParseError::Overflow)
        }
        _ => Err(CacheParseError::Invalid),
    }
}

fn required_human_bytes(value: &Value, key: &str) -> Result<u64, CacheParseError> {
    let value = value
        .as_object()
        .and_then(|object| object.get(key))
        .ok_or(CacheParseError::Invalid)?;
    match value {
        Value::Number(number) => number.as_u64().ok_or(CacheParseError::Invalid),
        Value::String(size) => parse_human_bytes(size),
        _ => Err(CacheParseError::Invalid),
    }
}

fn required_reclaimable_bytes(value: &Value, key: &str) -> Result<u64, CacheParseError> {
    let value = value
        .as_object()
        .and_then(|object| object.get(key))
        .ok_or(CacheParseError::Invalid)?;
    match value {
        Value::Number(number) => number.as_u64().ok_or(CacheParseError::Invalid),
        Value::String(size) => {
            let size = size.split_once('(').map_or(size.as_str(), |(size, _)| size);
            parse_human_bytes(size.trim())
        }
        _ => Err(CacheParseError::Invalid),
    }
}

/// Parse Docker CLI decimal SI sizes without floating-point conversion.
fn parse_human_bytes(value: &str) -> Result<u64, CacheParseError> {
    let value = value.trim();
    let unit_start = value
        .bytes()
        .position(|byte| byte.is_ascii_alphabetic())
        .ok_or(CacheParseError::Invalid)?;
    let (number, unit) = value.split_at(unit_start);
    if number.is_empty() || unit.is_empty() {
        return Err(CacheParseError::Invalid);
    }

    let mut coefficient = 0_u128;
    let mut scale = 1_u128;
    let mut saw_digit = false;
    let mut saw_decimal = false;
    for byte in number.bytes() {
        match byte {
            b'0'..=b'9' => {
                saw_digit = true;
                coefficient = coefficient
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u128::from(byte - b'0')))
                    .ok_or(CacheParseError::Overflow)?;
                if saw_decimal {
                    scale = scale.checked_mul(10).ok_or(CacheParseError::Overflow)?;
                }
            }
            b'.' if !saw_decimal => saw_decimal = true,
            _ => return Err(CacheParseError::Invalid),
        }
    }
    if !saw_digit || number.ends_with('.') {
        return Err(CacheParseError::Invalid);
    }

    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "b" => 1_u128,
        "kb" => 1_000_u128,
        "mb" => 1_000_000_u128,
        "gb" => 1_000_000_000_u128,
        "tb" => 1_000_000_000_000_u128,
        "pb" => 1_000_000_000_000_000_u128,
        "eb" => 1_000_000_000_000_000_000_u128,
        _ => return Err(CacheParseError::Invalid),
    };
    let numerator = coefficient
        .checked_mul(multiplier)
        .ok_or(CacheParseError::Overflow)?;
    // A fractional byte cannot represent a native byte count. Docker's own
    // formatter never emits one, so rejecting it catches incompatible output.
    if numerator % scale != 0 {
        return Err(CacheParseError::Invalid);
    }
    u64::try_from(numerator / scale).map_err(|_| CacheParseError::Overflow)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::process::{CapturedOutput, TerminationStatus};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        program: String,
        arguments: Vec<String>,
        limits: CaptureLimits,
        environment_count: usize,
        has_working_directory: bool,
    }

    struct FakeRunner {
        outcomes: Mutex<VecDeque<CommandOutcome>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        fn new(outcomes: impl IntoIterator<Item = CommandOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run_captured(&self, command: &CommandSpec, limits: CaptureLimits) -> CommandOutcome {
            self.calls.lock().unwrap().push(Call {
                program: command.program().to_string_lossy().into_owned(),
                arguments: command
                    .arguments()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                limits,
                environment_count: command.environment().len(),
                has_working_directory: command.working_directory().is_some(),
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected native command")
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("cache status must never execute a foreground command")
        }
    }

    fn exited(success: bool, stdout: &str, stderr: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: exit_status(success),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                ..CapturedOutput::default()
            },
        }
    }

    fn truncated(stdout: &str, stderr: bool) -> CommandOutcome {
        CommandOutcome::Exited {
            status: exit_status(true),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                stdout_truncated: !stderr,
                stderr_truncated: stderr,
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(unix)]
    fn exit_status(success: bool) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
    }

    #[cfg(windows)]
    fn exit_status(success: bool) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 })
    }

    fn docker_fixture() -> &'static str {
        concat!(
            r#"{"Type":"Build Cache","TotalCount":"5","Active":"1","Size":"1.5GB","Reclaimable":"500MB (33%)","Secret":"top-secret"}"#,
            "\n",
            r#"{"Type":"Images","TotalCount":"2","Active":"1","Size":"2GB","Reclaimable":"1GB (50%)"}"#,
            "\n",
            r#"{"Type":"Containers","TotalCount":"3","Active":"2","Size":"12.5kB","Reclaimable":"2.5kB (20%)"}"#,
            "\n",
            r#"{"Type":"Local Volumes","TotalCount":"1","Active":"0","Size":"1MB","Reclaimable":"1MB (100%)"}"#,
            "\n"
        )
    }

    #[test]
    fn docker_parser_converts_documented_json_lines_to_checked_bytes() {
        let records = parse_docker_cache_records(docker_fixture().as_bytes()).unwrap();
        assert_eq!(records.len(), 4);
        let images = records
            .iter()
            .find(|record| record.kind == NativeCacheRecordKind::Images)
            .unwrap();
        assert_eq!((images.count, images.active), (2, 1));
        assert_eq!(
            (images.total, images.reclaimable),
            (2_000_000_000, 1_000_000_000)
        );
        let containers = records
            .iter()
            .find(|record| record.kind == NativeCacheRecordKind::Containers)
            .unwrap();
        assert_eq!((containers.total, containers.reclaimable), (12_500, 2_500));
    }

    #[test]
    fn docker_size_parser_is_decimal_exact_and_bounded() {
        for (raw, expected) in [
            ("0B", 0),
            ("999B", 999),
            ("1.234kB", 1_234),
            ("12.5MB", 12_500_000),
            ("2GB", 2_000_000_000),
            ("0.001TB", 1_000_000_000),
        ] {
            assert_eq!(parse_human_bytes(raw), Ok(expected), "{raw}");
        }
        for raw in ["", "12", "-1GB", "NaNGB", "1GiB", "1.2.3GB", "0.1B"] {
            assert_eq!(
                parse_human_bytes(raw),
                Err(CacheParseError::Invalid),
                "{raw}"
            );
        }
        assert_eq!(parse_human_bytes("19EB"), Err(CacheParseError::Overflow));
    }

    #[test]
    fn docker_parser_rejects_partial_unknown_duplicate_and_inconsistent_rows() {
        for raw in [
            "not-json\n",
            r#"{"Type":"Future Cache","TotalCount":"1","Active":"0","Size":"1B","Reclaimable":"1B"}"#,
            concat!(
                r#"{"Type":"Images","TotalCount":"1","Active":"0","Size":"1B","Reclaimable":"1B"}"#,
                "\n",
                r#"{"Type":"Images","TotalCount":"1","Active":"0","Size":"1B","Reclaimable":"1B"}"#
            ),
            r#"{"Type":"Images","TotalCount":"1","Active":"2","Size":"1B","Reclaimable":"0B"}"#,
            r#"{"Type":"Images","TotalCount":"1","Active":"0","Size":"1B","Reclaimable":"2B"}"#,
            "",
        ] {
            assert_eq!(
                parse_docker_cache_records(raw.as_bytes()),
                Err(CacheParseError::Invalid),
                "{raw}"
            );
        }
    }

    #[test]
    fn docker_query_is_bounded_read_only_deterministic_and_secret_safe() {
        let limits = CaptureLimits::new(Duration::from_millis(321), 4_096, 2_048);
        let runner = FakeRunner::new([exited(
            true,
            docker_fixture(),
            "warning includes bearer super-secret",
        )]);
        let status = DockerCacheQuery.query(&runner, limits);

        assert_eq!(status.status, CacheQueryStatus::Available);
        assert_eq!(status.schema_version, 1);
        assert_eq!(status.total, 3_501_012_500);
        assert_eq!(status.reclaimable, 1_501_002_500);
        assert_eq!(
            status
                .records
                .iter()
                .map(|record| record.kind)
                .collect::<Vec<_>>(),
            [
                NativeCacheRecordKind::Images,
                NativeCacheRecordKind::Containers,
                NativeCacheRecordKind::LocalVolumes,
                NativeCacheRecordKind::BuildCache,
            ]
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "docker");
        assert_eq!(
            calls[0].arguments,
            ["system", "df", "--format", "{{json .}}"]
        );
        assert_eq!(calls[0].limits, limits);
        assert_eq!(calls[0].environment_count, 0);
        assert!(!calls[0].has_working_directory);
        assert!(!calls[0]
            .arguments
            .iter()
            .any(|argument| argument == "prune" || argument == "-c"));

        let serialized = serde_json::to_string(&status).unwrap();
        assert!(serialized.starts_with("{\"schema_version\":1,\"runtime\":\"docker\""));
        for secret in ["top-secret", "super-secret", "bearer"] {
            assert!(!serialized.to_ascii_lowercase().contains(secret));
        }
        assert_eq!(serialized, serde_json::to_string(&status).unwrap());
    }

    #[test]
    fn docker_overflow_becomes_invalid_without_partial_records() {
        let output = concat!(
            r#"{"Type":"Images","TotalCount":"1","Active":"0","Size":"18446744073709551615B","Reclaimable":"0B"}"#,
            "\n",
            r#"{"Type":"Containers","TotalCount":"1","Active":"0","Size":"1B","Reclaimable":"0B"}"#
        );
        let runner = FakeRunner::new([exited(true, output, "")]);
        let status = DockerCacheQuery.query(&runner, CaptureLimits::default());
        assert_eq!(status.status, CacheQueryStatus::InvalidOutput);
        assert!(status.records.is_empty());
        assert_eq!((status.total, status.reclaimable), (0, 0));
    }

    #[test]
    fn nonzero_docker_exits_are_typed_without_retaining_output() {
        for (stderr, expected) in [
            (
                "permission denied token=secret",
                CacheQueryStatus::PermissionDenied,
            ),
            (
                "Cannot connect to the Docker daemon",
                CacheQueryStatus::Unreachable,
            ),
            (
                "daemon returned proprietary failure",
                CacheQueryStatus::CommandFailed,
            ),
        ] {
            let runner = FakeRunner::new([exited(false, "stale parseable secret", stderr)]);
            let status = DockerCacheQuery.query(&runner, CaptureLimits::default());
            assert_eq!(status.status, expected, "{stderr}");
            assert!(status.records.is_empty());
            let serialized = serde_json::to_string(&status).unwrap();
            assert!(!serialized.contains("secret"));
            assert!(!serialized.contains("proprietary"));
        }
    }

    #[test]
    fn docker_truncation_and_timeout_are_never_parsed() {
        for outcome in [
            truncated(docker_fixture(), false),
            truncated(docker_fixture(), true),
        ] {
            let runner = FakeRunner::new([outcome]);
            let status = DockerCacheQuery.query(&runner, CaptureLimits::default());
            assert_eq!(status.status, CacheQueryStatus::OutputTruncated);
            assert!(status.records.is_empty());
        }
        let runner = FakeRunner::new([CommandOutcome::TimedOut {
            output: CapturedOutput {
                stdout: docker_fixture().as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
            termination: TerminationStatus::Requested,
        }]);
        let status = DockerCacheQuery.query(&runner, CaptureLimits::default());
        assert_eq!(status.status, CacheQueryStatus::TimedOut);
        assert!(status.records.is_empty());
    }

    #[test]
    fn buildx_parser_uses_only_documented_numeric_and_boolean_fields() {
        let output = concat!(
            r#"{"ID":"private-id","Description":"token=secret","Size":"829889526","Reclaimable":true,"Shared":false}"#,
            "\n",
            r#"{"ID":"other-id","Description":"/private/path","Size":170474,"Reclaimable":false}"#,
            "\n"
        );
        let records = parse_buildx_cache_records(output.as_bytes()).unwrap();
        assert_eq!(
            records,
            [NativeCacheRecord {
                kind: NativeCacheRecordKind::BuildCache,
                count: 2,
                active: 1,
                total: 830_060_000,
                reclaimable: 829_889_526,
            }]
        );
        assert_eq!(
            parse_buildx_cache_records(b""),
            Ok(vec![NativeCacheRecord {
                kind: NativeCacheRecordKind::BuildCache,
                count: 0,
                active: 0,
                total: 0,
                reclaimable: 0,
            }])
        );
    }

    #[test]
    fn buildx_parser_rejects_malformed_human_or_overflowing_output() {
        for raw in [
            "not-json",
            r#"{"Size":"1.2GB","Reclaimable":true}"#,
            r#"{"Size":"1","Reclaimable":"true"}"#,
            r#"{"Size":-1,"Reclaimable":true}"#,
            concat!(
                r#"{"Size":"18446744073709551615","Reclaimable":true}"#,
                "\n",
                r#"{"Size":"1","Reclaimable":true}"#
            ),
        ] {
            assert!(parse_buildx_cache_records(raw.as_bytes()).is_err(), "{raw}");
        }
    }

    #[test]
    fn buildx_query_is_version_gated_bounded_and_builder_bound() {
        let limits = CaptureLimits::new(Duration::from_millis(777), 8_192, 1_024);
        let du = concat!(
            r#"{"ID":"secret-id","Description":"credential=secret","Size":"1000","Reclaimable":true}"#,
            "\n",
            r#"{"ID":"active-id","Size":"2000","Reclaimable":false}"#,
            "\n"
        );
        let runner = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1 deadbeef\n", ""),
            exited(true, du, "warning secret-stderr"),
        ]);
        let query =
            BuildxCacheQuery::new(BuildxBuilderSelector::named("team.private-builder").unwrap());
        let status = query.query(&runner, limits);

        assert_eq!(status.status, CacheQueryStatus::Available);
        assert_eq!(status.runtime, RuntimeKind::Buildkit);
        assert_eq!(status.owner, NativeCacheOwner::BuildkitBuilder);
        assert_eq!((status.total, status.reclaimable), (3_000, 1_000));
        assert_eq!((status.records[0].count, status.records[0].active), (2, 1));
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, ["buildx", "version"]);
        assert_eq!(
            calls[1].arguments,
            [
                "buildx",
                "du",
                "--format=json",
                "--builder",
                "team.private-builder"
            ]
        );
        assert!(calls.iter().all(|call| call.limits == limits));
        assert!(!calls
            .iter()
            .flat_map(|call| &call.arguments)
            .any(|argument| argument == "--bootstrap" || argument == "prune"));

        let serialized = serde_json::to_string(&status).unwrap();
        for secret in [
            "team.private-builder",
            "secret-id",
            "credential",
            "secret-stderr",
        ] {
            assert!(
                !serialized.contains(secret),
                "leaked {secret}: {serialized}"
            );
        }
        assert_eq!(status.evidence.len(), 2);
    }

    #[test]
    fn old_or_unparseable_buildx_never_runs_disk_usage() {
        for version in [
            "github.com/docker/buildx v0.27.9 deadbeef\n",
            "vendor buildx unknown-version\n",
        ] {
            let runner = FakeRunner::new([exited(true, version, "")]);
            let status = BuildxCacheQuery::default().query(&runner, CaptureLimits::default());
            assert_eq!(status.status, CacheQueryStatus::UnsupportedVersion);
            assert_eq!(runner.calls().len(), 1);
            assert_eq!(status.evidence.len(), 1);
        }
    }

    #[test]
    fn buildx_nonzero_exit_and_format_rejection_are_typed() {
        let missing = FakeRunner::new([exited(
            false,
            "",
            "docker: 'buildx' is not a docker command",
        )]);
        assert_eq!(
            BuildxCacheQuery::default()
                .query(&missing, CaptureLimits::default())
                .status,
            CacheQueryStatus::NotInstalled
        );

        let unsupported = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.28.0\n", ""),
            exited(false, "", "unknown flag: --format"),
        ]);
        assert_eq!(
            BuildxCacheQuery::default()
                .query(&unsupported, CaptureLimits::default())
                .status,
            CacheQueryStatus::UnsupportedVersion
        );

        let unreachable = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1\n", ""),
            exited(false, "", "connection refused"),
        ]);
        assert_eq!(
            BuildxCacheQuery::default()
                .query(&unreachable, CaptureLimits::default())
                .status,
            CacheQueryStatus::Unreachable
        );
    }

    #[test]
    fn buildx_invalid_overflow_truncation_and_timeout_have_no_partial_records() {
        let cases = [
            (
                exited(true, r#"{"Size":"human","Reclaimable":true}"#, ""),
                CacheQueryStatus::InvalidOutput,
            ),
            (
                exited(
                    true,
                    concat!(
                        r#"{"Size":"18446744073709551615","Reclaimable":true}"#,
                        "\n",
                        r#"{"Size":"1","Reclaimable":true}"#
                    ),
                    "",
                ),
                CacheQueryStatus::InvalidOutput,
            ),
            (truncated("{}", false), CacheQueryStatus::OutputTruncated),
            (
                CommandOutcome::TimedOut {
                    output: CapturedOutput::default(),
                    termination: TerminationStatus::Requested,
                },
                CacheQueryStatus::TimedOut,
            ),
        ];
        for (du_outcome, expected) in cases {
            let runner = FakeRunner::new([
                exited(true, "github.com/docker/buildx v0.36.1\n", ""),
                du_outcome,
            ]);
            let status = BuildxCacheQuery::default().query(&runner, CaptureLimits::default());
            assert_eq!(status.status, expected);
            assert!(status.records.is_empty());
            assert_eq!((status.total, status.reclaimable), (0, 0));
        }

        let version_timeout = FakeRunner::new([CommandOutcome::TimedOut {
            output: CapturedOutput::default(),
            termination: TerminationStatus::Requested,
        }]);
        let status = BuildxCacheQuery::default().query(&version_timeout, CaptureLimits::default());
        assert_eq!(status.status, CacheQueryStatus::TimedOut);
        assert_eq!(version_timeout.calls().len(), 1);
    }

    #[test]
    fn containerd_is_explicitly_unsupported_without_any_probe_or_private_scan() {
        let runner = FakeRunner::new([]);
        let status = ContainerdCacheQuery.query(&runner, CaptureLimits::default());
        assert_eq!(status.schema_version, 1);
        assert_eq!(status.runtime, RuntimeKind::Containerd);
        assert_eq!(status.owner, NativeCacheOwner::Containerd);
        assert_eq!(status.status, CacheQueryStatus::Unsupported);
        assert!(status.records.is_empty());
        assert!(status.evidence.is_empty());
        assert!(runner.calls().is_empty());
    }
}
