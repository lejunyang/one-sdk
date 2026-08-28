//! Read-only BuildKit discovery through the Docker Buildx CLI.
//!
//! Discovery deliberately does not use `buildx inspect --bootstrap`: probing a
//! builder must not start it or otherwise mutate native state. Raw native
//! output is parsed into the typed values below and is never retained in a
//! serializable diagnostic report.

use std::collections::BTreeSet;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::redact::{CommandPurpose, NativeProgram};
use super::report::{
    Capability, CapabilityStatus, DiagnosticEvidence, DiagnosticReport, DiagnosticStatus, Endpoint,
    RuntimeKind,
};
use super::runtime::{ProbeCommand, RuntimeAdapter};
use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

const MINIMUM_BUILDX_VERSION: Version = Version::new(0, 10, 0);

/// Selection policy for Buildx discovery. `Auto` follows the one current
/// builder reported by Buildx; `Named` binds discovery to an explicit builder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BuildxBuilderSelector {
    #[default]
    Auto,
    Named(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid Buildx builder selector")]
pub struct BuildxBuilderSelectorError;

impl BuildxBuilderSelector {
    pub fn named(value: impl Into<String>) -> Result<Self, BuildxBuilderSelectorError> {
        let value = value.into();
        if is_safe_builder_name(&value) && value != "auto" {
            Ok(Self::Named(value))
        } else {
            Err(BuildxBuilderSelectorError)
        }
    }

    pub fn as_name(&self) -> Option<&str> {
        match self {
            Self::Auto => None,
            Self::Named(name) => Some(name),
        }
    }
}

impl std::str::FromStr for BuildxBuilderSelector {
    type Err = BuildxBuilderSelectorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "auto" {
            Ok(Self::Auto)
        } else {
            Self::named(value)
        }
    }
}

impl std::fmt::Display for BuildxBuilderSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Named(name) => formatter.write_str(name),
        }
    }
}

impl Serialize for BuildxBuilderSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for BuildxBuilderSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn is_safe_builder_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// The Buildx driver which owns a selected builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuilderDriver {
    Docker,
    DockerContainer,
    Kubernetes,
    Remote,
    Cloud,
    Unknown,
}

impl BuilderDriver {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "docker" => Self::Docker,
            "docker-container" => Self::DockerContainer,
            "kubernetes" => Self::Kubernetes,
            "remote" => Self::Remote,
            "cloud" => Self::Cloud,
            _ => Self::Unknown,
        }
    }
}

/// The observed state of a Buildx node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuilderNodeStatus {
    Running,
    Stopped,
    Inactive,
    Error,
    Unknown,
}

impl BuilderNodeStatus {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "inactive" => Self::Inactive,
            "error" => Self::Error,
            _ => Self::Unknown,
        }
    }
}

/// An OCI platform reported by Buildx.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BuildPlatform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl BuildPlatform {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().trim_end_matches('*');
        let mut components = raw.split('/');
        let os = components.next()?.trim();
        let architecture = components.next()?.trim();
        let variant = components.next().map(str::trim);
        if os.is_empty()
            || architecture.is_empty()
            || components.next().is_some()
            || variant.is_some_and(str::is_empty)
        {
            return None;
        }
        Some(Self {
            os: os.to_ascii_lowercase(),
            architecture: architecture.to_ascii_lowercase(),
            variant: variant.map(str::to_ascii_lowercase),
        })
    }
}

/// One node belonging to the selected Buildx builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuilderNode {
    pub name: String,
    pub endpoint: Option<Endpoint>,
    pub status: BuilderNodeStatus,
    pub buildkit_version: Option<Version>,
    pub platforms: BTreeSet<BuildPlatform>,
}

/// The selected builder and its read-only inspection facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedBuilder {
    pub name: String,
    pub driver: BuilderDriver,
    pub nodes: Vec<BuilderNode>,
    pub has_error: bool,
}

/// Typed Buildx discovery, alongside its stable secret-safe report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildkitDiscovery {
    pub report: DiagnosticReport,
    pub buildx_version: Option<Version>,
    pub selected_builder: Option<SelectedBuilder>,
}

/// Read-only BuildKit adapter backed by `docker buildx`.
#[derive(Clone, Debug, Default)]
pub struct BuildkitAdapter {
    selector: BuildxBuilderSelector,
}

impl BuildkitAdapter {
    pub fn new(selector: BuildxBuilderSelector) -> Self {
        Self { selector }
    }

    pub fn selector(&self) -> &BuildxBuilderSelector {
        &self.selector
    }

    /// Inspect the Buildx client, selected builder list entry, and selected
    /// builder details. The inspect command intentionally omits `--bootstrap`.
    pub fn inspect(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> BuildkitDiscovery {
        let mut report = DiagnosticReport::new(RuntimeKind::Buildkit, DiagnosticStatus::Degraded);
        let version_probe = probe(
            CommandPurpose::Version,
            CommandSpec::new("docker").args(["buildx", "version"]),
        );
        report.add_evidence(DiagnosticEvidence::Command(
            version_probe.evidence().clone(),
        ));
        let version_outcome = version_probe.execute(runner, limits);
        let version = match successful_stdout(&version_outcome) {
            Some(stdout) => parse_buildx_version(stdout),
            None => {
                report.status = classify_outcome(&version_outcome, DiagnosticStatus::NotInstalled);
                set_unavailable_capabilities(&mut report);
                return BuildkitDiscovery {
                    report,
                    buildx_version: None,
                    selected_builder: None,
                };
            }
        };
        report.set_capability(Capability::Client, CapabilityStatus::Supported);
        if version
            .as_ref()
            .is_none_or(|value| value < &MINIMUM_BUILDX_VERSION)
        {
            report.status = DiagnosticStatus::UnsupportedVersion;
            report.set_capability(Capability::BuilderInspection, CapabilityStatus::Unsupported);
            return BuildkitDiscovery {
                report,
                buildx_version: version,
                selected_builder: None,
            };
        }

        let list_probe = probe(
            CommandPurpose::BuilderInspect,
            CommandSpec::new("docker").args(["buildx", "ls", "--format", "{{json .}}"]),
        );
        report.add_evidence(DiagnosticEvidence::Command(list_probe.evidence().clone()));
        let list_outcome = list_probe.execute(runner, limits);
        let Some(list_stdout) = successful_stdout(&list_outcome) else {
            report.status = classify_outcome(&list_outcome, DiagnosticStatus::ClientOnly);
            report.set_capability(Capability::BuilderInspection, CapabilityStatus::Unavailable);
            return BuildkitDiscovery {
                report,
                buildx_version: version,
                selected_builder: None,
            };
        };
        let Some(mut selected) = parse_selected_builder(list_stdout, &self.selector) else {
            report.status =
                classify_text(&output_text(&list_outcome), DiagnosticStatus::ClientOnly);
            report.set_capability(Capability::BuilderInspection, CapabilityStatus::Unavailable);
            return BuildkitDiscovery {
                report,
                buildx_version: version,
                selected_builder: None,
            };
        };

        // Bind inspection to the exact validated list result so an ambient
        // current-builder change cannot mix facts. Never add `--bootstrap`.
        let inspect_probe = probe(
            CommandPurpose::BuilderInspect,
            CommandSpec::new("docker").args(["buildx", "inspect", selected.name.as_str()]),
        );
        report.add_evidence(DiagnosticEvidence::Command(
            inspect_probe.evidence().clone(),
        ));
        let inspect_outcome = inspect_probe.execute(runner, limits);
        let inspect_failure = if let Some(stdout) = successful_stdout(&inspect_outcome) {
            if merge_inspect_text(&mut selected, stdout) {
                None
            } else {
                selected.has_error = true;
                Some(DiagnosticStatus::Degraded)
            }
        } else {
            selected.has_error = true;
            Some(classify_outcome(
                &inspect_outcome,
                DiagnosticStatus::Degraded,
            ))
        };

        for node in &selected.nodes {
            if let Some(endpoint) = &node.endpoint {
                report.add_endpoint(endpoint.clone());
            }
        }
        let combined = format!(
            "{} {}",
            output_text(&list_outcome),
            output_text(&inspect_outcome)
        );
        report.status = if let Some(
            status @ (DiagnosticStatus::PermissionDenied | DiagnosticStatus::Unreachable),
        ) = inspect_failure
        {
            status
        } else if selected.has_error {
            classify_text(&combined, DiagnosticStatus::Degraded)
        } else if selected
            .nodes
            .iter()
            .any(|node| node.status == BuilderNodeStatus::Running)
        {
            DiagnosticStatus::Healthy
        } else {
            DiagnosticStatus::Degraded
        };
        report.set_capability(
            Capability::BuilderInspection,
            if report.status == DiagnosticStatus::Healthy {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unavailable
            },
        );
        report.set_capability(
            Capability::PlatformSelection,
            if selected.nodes.iter().any(|node| !node.platforms.is_empty()) {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unknown
            },
        );

        BuildkitDiscovery {
            report,
            buildx_version: version,
            selected_builder: Some(selected),
        }
    }
}

impl RuntimeAdapter for BuildkitAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Buildkit
    }

    fn diagnose(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport {
        self.inspect(runner, limits).report
    }
}

fn probe(purpose: CommandPurpose, command: CommandSpec) -> ProbeCommand {
    ProbeCommand::new(NativeProgram::Buildx, purpose, command)
}

fn successful_stdout(outcome: &CommandOutcome) -> Option<&[u8]> {
    match outcome {
        CommandOutcome::Exited { status, output }
            if status.success() && !output.stdout_truncated =>
        {
            Some(&output.stdout)
        }
        _ => None,
    }
}

fn output_text(outcome: &CommandOutcome) -> String {
    outcome
        .output()
        .map(|output| {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push(' ');
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text
        })
        .unwrap_or_default()
}

fn classify_outcome(outcome: &CommandOutcome, fallback: DiagnosticStatus) -> DiagnosticStatus {
    match outcome {
        CommandOutcome::NotInstalled => DiagnosticStatus::NotInstalled,
        CommandOutcome::PermissionDenied => DiagnosticStatus::PermissionDenied,
        CommandOutcome::TimedOut { .. } => DiagnosticStatus::Unreachable,
        _ => classify_text(&output_text(outcome), fallback),
    }
}

fn classify_text(text: &str, fallback: DiagnosticStatus) -> DiagnosticStatus {
    let text = text.to_ascii_lowercase();
    if text.contains("permission denied") || text.contains("access is denied") {
        DiagnosticStatus::PermissionDenied
    } else if text.contains("cannot connect")
        || text.contains("connection refused")
        || text.contains("connection error")
        || text.contains("deadline exceeded")
        || text.contains("timed out")
    {
        DiagnosticStatus::Unreachable
    } else if text.contains("is not a docker command")
        || text.contains("unknown command \"buildx\"")
        || text.contains("docker-buildx: executable file not found")
    {
        DiagnosticStatus::NotInstalled
    } else {
        fallback
    }
}

fn set_unavailable_capabilities(report: &mut DiagnosticReport) {
    report.set_capability(Capability::Client, CapabilityStatus::Unavailable);
    report.set_capability(Capability::BuilderInspection, CapabilityStatus::Unavailable);
    report.set_capability(Capability::PlatformSelection, CapabilityStatus::Unknown);
}

fn parse_buildx_version(bytes: &[u8]) -> Option<Version> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .filter_map(|token| super::parse_vendor_version(token))
        .next()
}

fn parse_selected_builder(
    bytes: &[u8],
    selector: &BuildxBuilderSelector,
) -> Option<SelectedBuilder> {
    let matches = String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| match selector {
            BuildxBuilderSelector::Auto => json_bool(value, "Current").unwrap_or(false),
            BuildxBuilderSelector::Named(name) => json_str(value, "Name") == Some(name),
        })
        .filter_map(parse_builder_json)
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches.into_iter().next().unwrap())
}

fn parse_builder_json(value: Value) -> Option<SelectedBuilder> {
    let name = json_str(&value, "Name")?.trim().to_owned();
    if !is_safe_builder_name(&name) {
        return None;
    }
    let driver = BuilderDriver::parse(json_str(&value, "Driver").unwrap_or_default());
    let has_error = json_str(&value, "Err").is_some_and(|error| !error.trim().is_empty());
    let nodes = value
        .get("Nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(parse_node_json)
        .collect();
    Some(SelectedBuilder {
        name,
        driver,
        nodes,
        has_error,
    })
}

fn parse_node_json(value: &Value) -> BuilderNode {
    let platforms = value
        .get("Platforms")
        .map(platform_values)
        .unwrap_or_default();
    BuilderNode {
        name: json_str(value, "Name")
            .unwrap_or_default()
            .trim()
            .to_owned(),
        endpoint: json_str(value, "Endpoint").and_then(parse_endpoint),
        status: BuilderNodeStatus::parse(json_str(value, "Status").unwrap_or_default()),
        buildkit_version: json_str(value, "Buildkit").and_then(super::parse_vendor_version),
        platforms,
    }
}

fn json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn platform_values(value: &Value) -> BTreeSet<BuildPlatform> {
    match value {
        Value::String(raw) => parse_platforms(raw),
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .filter_map(BuildPlatform::parse)
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn parse_platforms(raw: &str) -> BTreeSet<BuildPlatform> {
    raw.split(',').filter_map(BuildPlatform::parse).collect()
}

fn parse_endpoint(raw: &str) -> Option<Endpoint> {
    let address = super::redact::RedactedUrl::parse(raw.trim()).ok()?;
    let (transport, scope) = super::classify_endpoint(raw.trim());
    Some(Endpoint::new(transport, scope, address))
}

fn merge_inspect_text(builder: &mut SelectedBuilder, bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let top_level_name = text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == "Name").then(|| value.trim())
    });
    if top_level_name != Some(builder.name.as_str()) {
        return false;
    }

    let mut current_node: Option<usize> = None;
    let mut consumed_top_level_name = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Name" if !consumed_top_level_name => {
                consumed_top_level_name = true;
            }
            "Driver" if current_node.is_none() => builder.driver = BuilderDriver::parse(value),
            "Error" => builder.has_error |= !value.is_empty(),
            "Name" => {
                let index = builder
                    .nodes
                    .iter()
                    .position(|node| node.name == value)
                    .unwrap_or_else(|| {
                        builder.nodes.push(empty_node(value));
                        builder.nodes.len() - 1
                    });
                current_node = Some(index);
            }
            "Endpoint" => {
                if let Some(node) = current_node.and_then(|index| builder.nodes.get_mut(index)) {
                    node.endpoint = parse_endpoint(value);
                }
            }
            "Status" => {
                if let Some(node) = current_node.and_then(|index| builder.nodes.get_mut(index)) {
                    node.status = BuilderNodeStatus::parse(value);
                }
            }
            "BuildKit" => {
                if let Some(node) = current_node.and_then(|index| builder.nodes.get_mut(index)) {
                    node.buildkit_version = super::parse_vendor_version(value);
                }
            }
            "Platforms" => {
                if let Some(node) = current_node.and_then(|index| builder.nodes.get_mut(index)) {
                    node.platforms.extend(parse_platforms(value));
                }
            }
            _ => {}
        }
    }
    true
}

fn empty_node(name: &str) -> BuilderNode {
    BuilderNode {
        name: name.to_owned(),
        endpoint: None,
        status: BuilderNodeStatus::Unknown,
        buildkit_version: None,
        platforms: BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::container::EndpointTransport;
    use crate::process::CapturedOutput;

    struct FakeRunner {
        outcomes: Mutex<VecDeque<CommandOutcome>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(outcomes: impl IntoIterator<Item = CommandOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run_captured(&self, command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            self.calls.lock().unwrap().push(
                command
                    .arguments()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            );
            self.outcomes.lock().unwrap().pop_front().unwrap()
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("BuildkitAdapter discovery must not run foreground commands")
        }
    }

    fn exited(success: bool, stdout: &str, stderr: &str) -> CommandOutcome {
        let status = shell_status(success);
        CommandOutcome::Exited {
            status,
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(unix)]
    fn shell_status(success: bool) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
    }

    #[cfg(windows)]
    fn shell_status(success: bool) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 })
    }

    #[test]
    fn parses_selected_builder_json_lines_and_inspect_without_bootstrap() {
        let list = concat!(
            r#"{"Current":false,"Driver":"docker","Name":"other","Nodes":[]}"#,
            "\n",
            r#"{"Current":true,"Driver":"docker-container","Err":"","Name":"selected","Nodes":[{"Endpoint":"ssh://alice:secret@example.test/private?token=x","Name":"selected0","Status":"running","Buildkit":"v0.25.0","Platforms":"linux/amd64, linux/arm64/v8*"}],"FutureField":true}"#,
            "\n"
        );
        let inspect = concat!(
            "Name:          selected\n",
            "Driver:        docker-container\n",
            "Nodes:\n",
            "Name:          selected0\n",
            "Endpoint:      unix:///var/run/docker.sock\n",
            "Status:        running\n",
            "BuildKit:      v0.25.0\n",
            "Platforms:     linux/amd64, linux/arm64/v8*\n",
            "Labels:\n",
            " org.mobyproject.buildkit.worker.hostname: fixture:with:colons\n"
        );
        let runner = FakeRunner::new([
            exited(
                true,
                "github.com/docker/buildx v0.36.1 deadbeef\n",
                "warning\n",
            ),
            exited(true, list, "warning that must not invalidate stdout\n"),
            exited(true, inspect, ""),
        ]);

        let discovery = BuildkitAdapter::default().inspect(&runner, CaptureLimits::default());
        assert_eq!(discovery.report.status, DiagnosticStatus::Healthy);
        assert_eq!(discovery.buildx_version, Some(Version::new(0, 36, 1)));
        let builder = discovery.selected_builder.unwrap();
        assert_eq!(builder.name, "selected");
        assert_eq!(builder.driver, BuilderDriver::DockerContainer);
        assert_eq!(builder.nodes[0].status, BuilderNodeStatus::Running);
        assert_eq!(builder.nodes[0].platforms.len(), 2);
        let endpoint = builder.nodes[0].endpoint.as_ref().unwrap();
        assert_eq!(endpoint.transport, EndpointTransport::LocalSocket);
        assert!(!endpoint.address.as_str().contains("docker.sock"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[2], ["buildx", "inspect", "selected"]);
        assert!(!calls
            .iter()
            .flatten()
            .any(|argument| argument == "--bootstrap"));
    }

    #[test]
    fn explicit_builder_selects_non_current_and_is_one_safe_argument() {
        let list = concat!(
            r#"{"Current":true,"Driver":"docker","Name":"current","Nodes":[]}"#,
            "\n",
            r#"{"Current":false,"Driver":"docker-container","Name":"team.builder-2","Nodes":[{"Name":"team.builder-20","Status":"running"}]}"#,
            "\n"
        );
        let runner = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1 deadbeef\n", ""),
            exited(true, list, ""),
            exited(
                true,
                "Name: team.builder-2\nDriver: docker-container\nName: team.builder-20\nStatus: running\n",
                "",
            ),
        ]);
        let adapter = BuildkitAdapter::new(BuildxBuilderSelector::named("team.builder-2").unwrap());

        let discovery = adapter.inspect(&runner, CaptureLimits::default());

        assert_eq!(discovery.report.status, DiagnosticStatus::Healthy);
        assert_eq!(discovery.selected_builder.unwrap().name, "team.builder-2");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[2], ["buildx", "inspect", "team.builder-2"]);
        assert!(!calls.iter().flatten().any(|value| value == "--bootstrap"));
    }

    #[test]
    fn missing_named_or_ambiguous_current_builder_fails_without_inspection() {
        for (selector, list) in [
            (
                BuildxBuilderSelector::named("missing").unwrap(),
                r#"{"Current":true,"Name":"current","Nodes":[]}"#,
            ),
            (
                BuildxBuilderSelector::Auto,
                concat!(
                    r#"{"Current":true,"Name":"one","Nodes":[]}"#,
                    "\n",
                    r#"{"Current":true,"Name":"two","Nodes":[]}"#
                ),
            ),
        ] {
            let runner = FakeRunner::new([
                exited(true, "github.com/docker/buildx v0.36.1 deadbeef\n", ""),
                exited(true, list, ""),
            ]);
            let discovery =
                BuildkitAdapter::new(selector).inspect(&runner, CaptureLimits::default());
            assert_eq!(discovery.report.status, DiagnosticStatus::ClientOnly);
            assert!(discovery.selected_builder.is_none());
            assert_eq!(runner.calls.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn rejects_unsafe_builder_selectors_without_echoing_them() {
        let invalid = [
            "".to_owned(),
            "--help".to_owned(),
            "-builder".to_owned(),
            ".builder".to_owned(),
            "name with spaces".to_owned(),
            "b\u{fc}ilder".to_owned(),
            "a".repeat(129),
        ];
        for value in invalid {
            let error = value.parse::<BuildxBuilderSelector>().unwrap_err();
            if !value.is_empty() {
                assert!(!error.to_string().contains(&value));
                assert!(!format!("{error:?}").contains(&value));
            }
        }
        for value in ["default", "remote-builder_1", "team.builder-2"] {
            assert_eq!(
                value.parse::<BuildxBuilderSelector>().unwrap().as_name(),
                Some(value)
            );
        }
    }

    #[test]
    fn mismatched_inspect_name_is_not_merged() {
        let list = r#"{"Current":true,"Driver":"docker","Name":"selected","Nodes":[]}"#;
        let runner = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1 deadbeef\n", ""),
            exited(true, list, ""),
            exited(
                true,
                "Name: different\nDriver: docker-container\nName: node0\nStatus: running\n",
                "",
            ),
        ]);

        let discovery = BuildkitAdapter::default().inspect(&runner, CaptureLimits::default());

        assert_eq!(discovery.report.status, DiagnosticStatus::Degraded);
        let builder = discovery.selected_builder.unwrap();
        assert_eq!(builder.driver, BuilderDriver::Docker);
        assert!(builder.nodes.is_empty());
        assert!(builder.has_error);
    }

    #[test]
    fn buildkit_json_and_text_versions_share_vendor_conversion() {
        let list = r#"{"Current":true,"Driver":"docker-container","Name":"selected","Nodes":[{"Name":"selected0","Status":"running","Buildkit":"V0.25"}]}"#;
        let runner = FakeRunner::new([
            exited(true, "github.com/docker/buildx V0.36\n", ""),
            exited(true, list, ""),
            exited(
                true,
                "Name: selected\nDriver: docker-container\nName: selected0\nStatus: running\nBuildKit: V0.25\n",
                "",
            ),
        ]);

        let discovery = BuildkitAdapter::default().inspect(&runner, CaptureLimits::default());

        assert_eq!(discovery.buildx_version, Some(Version::new(0, 36, 0)));
        assert_eq!(
            discovery.selected_builder.unwrap().nodes[0].buildkit_version,
            Some(Version::new(0, 25, 0))
        );
    }

    #[test]
    fn loopback_buildkit_endpoints_are_local() {
        for raw in [
            "tcp://127.0.0.1:1234",
            "http://localhost:1234",
            "https://[::1]:1234",
        ] {
            assert_eq!(
                parse_endpoint(raw).unwrap().scope,
                crate::container::EndpointScope::Local
            );
        }
        assert_eq!(
            parse_endpoint("tcp://192.0.2.10:1234").unwrap().scope,
            crate::container::EndpointScope::Remote
        );
    }

    #[test]
    fn classifies_missing_old_and_client_only_buildx() {
        let missing = FakeRunner::new([CommandOutcome::NotInstalled]);
        assert_eq!(
            BuildkitAdapter::default()
                .inspect(&missing, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::NotInstalled
        );

        let old = FakeRunner::new([exited(true, "github.com/docker/buildx v0.9.1 abc\n", "")]);
        assert_eq!(
            BuildkitAdapter::default()
                .inspect(&old, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::UnsupportedVersion
        );

        let client_only = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1 abc\n", ""),
            exited(true, "", ""),
        ]);
        assert_eq!(
            BuildkitAdapter::default()
                .inspect(&client_only, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::ClientOnly
        );
    }

    #[test]
    fn classifies_builder_errors_even_when_buildx_exits_successfully() {
        for (message, expected) in [
            (
                "permission denied while trying to connect to the docker API",
                DiagnosticStatus::PermissionDenied,
            ),
            (
                "Cannot connect to the Docker daemon at tcp://host:2375",
                DiagnosticStatus::Unreachable,
            ),
        ] {
            let list = format!(
                r#"{{"Current":true,"Driver":"","Err":"{message}","Name":"default","Nodes":[{{"Endpoint":"","Name":""}}]}}"#
            );
            let inspect = format!("Name: default\nDriver:\nError: {message}\n");
            let runner = FakeRunner::new([
                exited(true, "github.com/docker/buildx v0.36.1 abc\n", ""),
                exited(true, &list, ""),
                exited(true, &inspect, ""),
            ]);
            assert_eq!(
                BuildkitAdapter::default()
                    .inspect(&runner, CaptureLimits::default())
                    .report
                    .status,
                expected
            );
        }
    }

    #[test]
    fn malformed_or_truncated_discovery_is_not_treated_as_healthy() {
        let malformed = FakeRunner::new([
            exited(true, "github.com/docker/buildx v0.36.1 abc\n", ""),
            exited(true, "not-json\n{}\n", ""),
        ]);
        assert_eq!(
            BuildkitAdapter::default()
                .inspect(&malformed, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::ClientOnly
        );

        let mut truncated = exited(true, "github.com/docker/buildx v0.36.1", "");
        if let CommandOutcome::Exited { output, .. } = &mut truncated {
            output.stdout_truncated = true;
        }
        let runner = FakeRunner::new([truncated]);
        assert_ne!(
            BuildkitAdapter::default()
                .inspect(&runner, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::Healthy
        );
    }

    #[test]
    fn classifies_inspect_timeout_and_spawn_permission_denial() {
        let list =
            r#"{"Current":true,"Driver":"docker-container","Err":"","Name":"selected","Nodes":[]}"#;
        for (outcome, expected) in [
            (
                CommandOutcome::TimedOut {
                    output: CapturedOutput::default(),
                    termination: crate::process::TerminationStatus::Requested,
                },
                DiagnosticStatus::Unreachable,
            ),
            (
                CommandOutcome::PermissionDenied,
                DiagnosticStatus::PermissionDenied,
            ),
        ] {
            let runner = FakeRunner::new([
                exited(true, "github.com/docker/buildx v0.36.1 abc\n", ""),
                exited(true, list, ""),
                outcome,
            ]);
            assert_eq!(
                BuildkitAdapter::default()
                    .inspect(&runner, CaptureLimits::default())
                    .report
                    .status,
                expected
            );
        }
    }
}
