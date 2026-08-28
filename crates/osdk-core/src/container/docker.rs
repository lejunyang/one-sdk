//! Read-only Docker Engine discovery.
//!
//! The adapter deliberately asks the Docker CLI for public context and daemon
//! metadata. It never reads Docker's private state or configuration files.

use semver::Version;
use serde_json::Value;

use super::redact::{CommandPurpose, NativeProgram, RedactedUrl};
use super::report::{
    Capability, CapabilityStatus, DiagnosticEvidence, DiagnosticReport, DiagnosticStatus, Endpoint,
    EndpointScope, EndpointTransport, Privilege, RuntimeKind,
};
use super::runtime::{ProbeCommand, RuntimeAdapter};
use crate::process::{CaptureLimits, CapturedOutput, CommandOutcome, CommandRunner, CommandSpec};

const MINIMUM_DOCKER_VERSION: Version = Version::new(19, 3, 0);

/// The ownership and locality inferred for the selected Docker context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockerContextKind {
    Local,
    Remote,
    Rootless,
    Desktop,
    Unknown,
}

/// Public fields returned by `docker context inspect` for the selected context.
///
/// This type intentionally does not implement `Serialize`: the context name is
/// useful to an interactive caller but must not accidentally enter the stable
/// secret-safe diagnostic JSON contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerContext {
    pub name: Option<String>,
    pub endpoint: Option<Endpoint>,
    pub kind: DockerContextKind,
    pub skip_tls_verify: bool,
}

/// Parsed client and daemon versions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DockerVersion {
    pub client: Option<Version>,
    pub server: Option<Version>,
}

/// Bounded daemon facts returned by `docker info`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DockerInfo {
    pub operating_system: Option<String>,
    pub os_type: Option<String>,
    pub architecture: Option<String>,
    pub rootless: bool,
    pub desktop: bool,
    pub registry_mirrors: Vec<RedactedUrl>,
}

/// Complete typed result of a Docker discovery pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerDiscovery {
    pub status: DiagnosticStatus,
    pub context: Option<DockerContext>,
    pub version: Option<DockerVersion>,
    pub info: Option<DockerInfo>,
}

/// A structural parse error which never retains or echoes command output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DockerParseError {
    #[error("invalid Docker JSON output")]
    InvalidJson,
    #[error("Docker output did not contain the expected object")]
    MissingObject,
}

/// Read-only Docker Engine runtime adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct DockerAdapter;

impl DockerAdapter {
    pub fn discover(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> DockerDiscovery {
        let version_probe = ProbeCommand::new(
            NativeProgram::Docker,
            CommandPurpose::Version,
            CommandSpec::new("docker").args(["version", "--format", "{{json .}}"]),
        );
        let version_outcome = version_probe.execute(runner, limits);

        if let Some(status) = spawn_status(&version_outcome) {
            return DockerDiscovery {
                status,
                context: None,
                version: None,
                info: None,
            };
        }

        let version = successful_output(&version_outcome)
            .and_then(|output| parse_docker_version(&output.stdout).ok());
        let version_failure = classify_outcome(&version_outcome);
        if version.as_ref().is_some_and(|version| {
            [&version.client, &version.server]
                .into_iter()
                .flatten()
                .any(|version| version < &MINIMUM_DOCKER_VERSION)
        }) || output_contains(&version_outcome, &unsupported_version_markers())
        {
            return DockerDiscovery {
                status: DiagnosticStatus::UnsupportedVersion,
                context: None,
                version,
                info: None,
            };
        }

        let context_outcome = ProbeCommand::new(
            NativeProgram::Docker,
            CommandPurpose::ContextInspect,
            CommandSpec::new("docker").args(["context", "inspect"]),
        )
        .execute(runner, limits);
        let mut context = successful_output(&context_outcome)
            .and_then(|output| parse_docker_context(&output.stdout).ok());

        let info_outcome = ProbeCommand::new(
            NativeProgram::Docker,
            CommandPurpose::RuntimeInfo,
            CommandSpec::new("docker").args(["info", "--format", "{{json .}}"]),
        )
        .execute(runner, limits);
        let info = successful_output(&info_outcome)
            .and_then(|output| parse_docker_info(&output.stdout).ok());

        if let Some(context) = context.as_mut() {
            refine_context_kind(context, info.as_ref());
        }

        let has_client = version
            .as_ref()
            .is_some_and(|version| version.client.is_some());
        let has_daemon = version
            .as_ref()
            .is_some_and(|version| version.server.is_some())
            || info.is_some();
        let failures = [
            version_failure,
            classify_outcome(&context_outcome),
            classify_outcome(&info_outcome),
        ];
        let status = if failures.contains(&Some(DiagnosticStatus::PermissionDenied)) {
            DiagnosticStatus::PermissionDenied
        } else if !has_daemon && failures.contains(&Some(DiagnosticStatus::UnsupportedVersion)) {
            DiagnosticStatus::UnsupportedVersion
        } else if !has_daemon && failures.contains(&Some(DiagnosticStatus::Unreachable)) {
            DiagnosticStatus::Unreachable
        } else if has_daemon && info.is_some() {
            if has_client && context.is_some() {
                DiagnosticStatus::Healthy
            } else {
                DiagnosticStatus::Degraded
            }
        } else if has_daemon {
            DiagnosticStatus::Degraded
        } else if has_client {
            DiagnosticStatus::ClientOnly
        } else if failures.contains(&Some(DiagnosticStatus::Unreachable)) {
            DiagnosticStatus::Unreachable
        } else {
            DiagnosticStatus::Degraded
        };

        DockerDiscovery {
            status,
            context,
            version,
            info,
        }
    }

    fn report(&self, discovery: &DockerDiscovery) -> DiagnosticReport {
        let mut report = DiagnosticReport::new(RuntimeKind::Docker, discovery.status);
        let client = discovery
            .version
            .as_ref()
            .is_some_and(|version| version.client.is_some());
        let daemon = discovery
            .version
            .as_ref()
            .is_some_and(|version| version.server.is_some())
            || discovery.info.is_some();

        report.set_capability(
            Capability::Client,
            if client {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unavailable
            },
        );
        for capability in [
            Capability::Daemon,
            Capability::RuntimeInfo,
            Capability::Pull,
            Capability::CacheStatus,
            Capability::CachePrune,
            Capability::RegistryMirrors,
        ] {
            report.set_capability(
                capability,
                if daemon {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unavailable
                },
            );
        }
        // Docker Engine's mirror setting is Docker Hub-specific, not a
        // per-registry host mapping mechanism.
        report.set_capability(
            Capability::RegistryHostMapping,
            CapabilityStatus::Unsupported,
        );

        if let Some(context) = &discovery.context {
            report.privilege = match context.kind {
                DockerContextKind::Local
                | DockerContextKind::Rootless
                | DockerContextKind::Desktop => Privilege::CurrentUser,
                DockerContextKind::Remote => Privilege::RemoteAdministrator,
                DockerContextKind::Unknown => Privilege::Unknown,
            };
            if let Some(endpoint) = &context.endpoint {
                report.add_endpoint(endpoint.clone());
            }
        }
        report
    }
}

impl RuntimeAdapter for DockerAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Docker
    }

    fn diagnose(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport {
        let discovery = self.discover(runner, limits);
        let mut report = self.report(&discovery);
        for (purpose, arguments) in [
            (
                CommandPurpose::Version,
                vec!["version", "--format", "{{json .}}"],
            ),
            (CommandPurpose::ContextInspect, vec!["context", "inspect"]),
            (
                CommandPurpose::RuntimeInfo,
                vec!["info", "--format", "{{json .}}"],
            ),
        ] {
            if discovery.context.is_none()
                && discovery.info.is_none()
                && matches!(
                    discovery.status,
                    DiagnosticStatus::NotInstalled
                        | DiagnosticStatus::PermissionDenied
                        | DiagnosticStatus::UnsupportedVersion
                )
                && purpose != CommandPurpose::Version
            {
                continue;
            }
            let command = CommandSpec::new("docker").args(arguments);
            let evidence =
                super::redact::RedactedCommand::from_spec(NativeProgram::Docker, purpose, &command);
            report.add_evidence(DiagnosticEvidence::Command(evidence));
        }
        report
    }
}

pub fn parse_docker_version(output: &[u8]) -> Result<DockerVersion, DockerParseError> {
    let value: Value = serde_json::from_slice(output).map_err(|_| DockerParseError::InvalidJson)?;
    let object = value.as_object().ok_or(DockerParseError::MissingObject)?;
    let client = object
        .get("Client")
        .and_then(|value| value.get("Version"))
        .and_then(Value::as_str)
        .and_then(super::parse_vendor_version);
    let server = object
        .get("Server")
        .and_then(|value| value.get("Version"))
        .and_then(Value::as_str)
        .and_then(super::parse_vendor_version);
    if client.is_none() && server.is_none() {
        return Err(DockerParseError::MissingObject);
    }
    Ok(DockerVersion { client, server })
}

pub fn parse_docker_context(output: &[u8]) -> Result<DockerContext, DockerParseError> {
    let value: Value = serde_json::from_slice(output).map_err(|_| DockerParseError::InvalidJson)?;
    let object = match &value {
        Value::Array(contexts) => contexts.first().and_then(Value::as_object),
        Value::Object(object) => Some(object),
        _ => None,
    }
    .ok_or(DockerParseError::MissingObject)?;

    let name = object
        .get("Name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let metadata_desktop = object
        .get("Metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            metadata.values().any(|value| {
                value
                    .as_str()
                    .is_some_and(|value| value.to_ascii_lowercase().contains("docker desktop"))
            })
        });
    let docker_endpoint = object
        .get("Endpoints")
        .and_then(|value| value.get("docker"))
        .and_then(Value::as_object);
    let raw_endpoint = docker_endpoint
        .and_then(|endpoint| endpoint.get("Host"))
        .and_then(Value::as_str);
    let skip_tls_verify = docker_endpoint
        .and_then(|endpoint| endpoint.get("SkipTLSVerify"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let name_lower = name.as_deref().unwrap_or_default().to_ascii_lowercase();
    let endpoint_lower = raw_endpoint.unwrap_or_default().to_ascii_lowercase();
    let desktop = metadata_desktop
        || name_lower == "desktop-linux"
        || name_lower == "docker-desktop"
        || endpoint_lower.contains("dockerdesktop");
    let rootless = endpoint_lower.starts_with("unix:///run/user/")
        || endpoint_lower.contains("/.docker/run/docker.sock");
    let remote = raw_endpoint.is_some_and(is_remote_endpoint);
    let kind = if desktop {
        DockerContextKind::Desktop
    } else if remote {
        DockerContextKind::Remote
    } else if rootless {
        DockerContextKind::Rootless
    } else if raw_endpoint.is_some() {
        DockerContextKind::Local
    } else {
        DockerContextKind::Unknown
    };
    let endpoint = raw_endpoint.and_then(|raw| docker_endpoint_from_raw(raw, kind).ok());

    Ok(DockerContext {
        name,
        endpoint,
        kind,
        skip_tls_verify,
    })
}

pub fn parse_docker_info(output: &[u8]) -> Result<DockerInfo, DockerParseError> {
    let value: Value = serde_json::from_slice(output).map_err(|_| DockerParseError::InvalidJson)?;
    let object = value.as_object().ok_or(DockerParseError::MissingObject)?;
    if ![
        "OperatingSystem",
        "OSType",
        "Architecture",
        "Name",
        "ServerVersion",
        "SecurityOptions",
        "RegistryConfig",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return Err(DockerParseError::MissingObject);
    }
    let operating_system = string_field(object, "OperatingSystem");
    let os_type = string_field(object, "OSType");
    let architecture = string_field(object, "Architecture");
    let name = string_field(object, "Name");
    let rootless = object
        .get("SecurityOptions")
        .and_then(Value::as_array)
        .is_some_and(|options| options.iter().any(value_mentions_rootless));
    let desktop = operating_system
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("docker desktop"))
        || name
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("docker-desktop"));
    let registry_mirrors = object
        .get("RegistryConfig")
        .and_then(|value| value.get("Mirrors"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| RedactedUrl::parse(value).ok())
        .collect();

    Ok(DockerInfo {
        operating_system,
        os_type,
        architecture,
        rootless,
        desktop,
        registry_mirrors,
    })
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn value_mentions_rootless(value: &Value) -> bool {
    match value {
        Value::String(value) => value.to_ascii_lowercase().contains("rootless"),
        Value::Object(object) => object.iter().any(|(key, value)| {
            key.to_ascii_lowercase().contains("rootless") || value_mentions_rootless(value)
        }),
        Value::Array(values) => values.iter().any(value_mentions_rootless),
        _ => false,
    }
}

fn refine_context_kind(context: &mut DockerContext, info: Option<&DockerInfo>) {
    let Some(info) = info else {
        return;
    };
    if info.desktop {
        context.kind = DockerContextKind::Desktop;
    } else if info.rootless && context.kind != DockerContextKind::Remote {
        context.kind = DockerContextKind::Rootless;
    }
    if context.kind == DockerContextKind::Desktop {
        if let Some(endpoint) = context.endpoint.as_mut() {
            endpoint.transport = EndpointTransport::DockerDesktop;
            endpoint.scope = EndpointScope::ManagedDesktop;
        }
    }
}

fn docker_endpoint_from_raw(
    raw: &str,
    kind: DockerContextKind,
) -> Result<Endpoint, super::redact::RedactedUrlError> {
    let (transport, mut scope) = super::classify_endpoint(raw);
    let transport = if kind == DockerContextKind::Desktop {
        scope = EndpointScope::ManagedDesktop;
        EndpointTransport::DockerDesktop
    } else {
        transport
    };
    Ok(Endpoint::new(transport, scope, RedactedUrl::parse(raw)?))
}

fn is_remote_endpoint(raw: &str) -> bool {
    super::classify_endpoint(raw).1 == EndpointScope::Remote
}

fn successful_output(outcome: &CommandOutcome) -> Option<&CapturedOutput> {
    match outcome {
        CommandOutcome::Exited { status, output }
            if status.success() && !output.stdout_truncated =>
        {
            Some(output)
        }
        _ => None,
    }
}

fn spawn_status(outcome: &CommandOutcome) -> Option<DiagnosticStatus> {
    match outcome {
        CommandOutcome::NotInstalled => Some(DiagnosticStatus::NotInstalled),
        CommandOutcome::PermissionDenied => Some(DiagnosticStatus::PermissionDenied),
        _ => None,
    }
}

fn classify_outcome(outcome: &CommandOutcome) -> Option<DiagnosticStatus> {
    if let Some(status) = spawn_status(outcome) {
        return Some(status);
    }
    if matches!(outcome, CommandOutcome::TimedOut { .. }) {
        return Some(DiagnosticStatus::Unreachable);
    }
    if output_contains(outcome, &permission_markers()) {
        return Some(DiagnosticStatus::PermissionDenied);
    }
    if output_contains(outcome, &unsupported_version_markers()) {
        return Some(DiagnosticStatus::UnsupportedVersion);
    }
    if output_contains(outcome, &unreachable_markers()) {
        return Some(DiagnosticStatus::Unreachable);
    }
    match outcome {
        CommandOutcome::Exited { status, .. } if !status.success() => {
            Some(DiagnosticStatus::Degraded)
        }
        CommandOutcome::ExecutionFailed { .. } | CommandOutcome::SpawnFailed { .. } => {
            Some(DiagnosticStatus::Degraded)
        }
        _ => None,
    }
}

fn output_contains(outcome: &CommandOutcome, markers: &[&str]) -> bool {
    let Some(output) = outcome.output() else {
        return false;
    };
    [&output.stdout[..], &output.stderr[..]]
        .into_iter()
        .any(|bytes| {
            let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
            markers.iter().any(|marker| text.contains(marker))
        })
}

fn permission_markers() -> [&'static str; 4] {
    [
        "permission denied",
        "access is denied",
        "operation not permitted",
        "authorization denied",
    ]
}

fn unreachable_markers() -> [&'static str; 8] {
    [
        "cannot connect",
        "connection refused",
        "is the docker daemon running",
        "error during connect",
        "context deadline exceeded",
        "i/o timeout",
        "no such host",
        "daemon is not running",
    ]
}

fn unsupported_version_markers() -> [&'static str; 4] {
    [
        "client version is too old",
        "server version is too old",
        "unsupported api version",
        "requires docker engine",
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    use super::*;
    use crate::process::TerminationStatus;

    #[derive(Default)]
    struct FakeRunner {
        outcomes: Mutex<VecDeque<CommandOutcome>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn with_outcomes(outcomes: impl IntoIterator<Item = CommandOutcome>) -> Self {
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
            panic!("Docker discovery must not execute foreground commands")
        }
    }

    fn success(stdout: &str) -> CommandOutcome {
        exited(0, stdout, "")
    }

    fn failure(stdout: &str, stderr: &str) -> CommandOutcome {
        exited(1, stdout, stderr)
    }

    fn exited(code: i32, stdout: &str, stderr: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: exit_status(code),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    #[test]
    fn parses_local_remote_rootless_and_desktop_contexts() {
        let local = parse_docker_context(
            br#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock","SkipTLSVerify":false}}}]"#,
        )
        .unwrap();
        assert_eq!(local.kind, DockerContextKind::Local);
        assert_eq!(
            local.endpoint.unwrap().transport,
            EndpointTransport::LocalSocket
        );

        let remote = parse_docker_context(
            br#"[{"Name":"prod","Endpoints":{"docker":{"Host":"ssh://alice:secret@host.example/private?token=x"}}}]"#,
        )
        .unwrap();
        assert_eq!(remote.kind, DockerContextKind::Remote);
        let endpoint = remote.endpoint.unwrap();
        assert_eq!(endpoint.transport, EndpointTransport::Ssh);
        assert_eq!(endpoint.scope, EndpointScope::Remote);
        let json = serde_json::to_string(&endpoint).unwrap();
        for secret in ["alice", "secret", "private", "token"] {
            assert!(!json.contains(secret), "leaked {secret}: {json}");
        }

        let rootless = parse_docker_context(
            br#"[{"Name":"rootless","Endpoints":{"docker":{"Host":"unix:///run/user/1000/docker.sock"}}}]"#,
        )
        .unwrap();
        assert_eq!(rootless.kind, DockerContextKind::Rootless);

        let desktop = parse_docker_context(
            br#"[{"Name":"desktop-linux","Metadata":{"Description":"Docker Desktop"},"Endpoints":{"docker":{"Host":"npipe:////./pipe/dockerDesktopLinuxEngine"}}}]"#,
        )
        .unwrap();
        assert_eq!(desktop.kind, DockerContextKind::Desktop);
        assert_eq!(
            desktop.endpoint.unwrap().transport,
            EndpointTransport::DockerDesktop
        );
    }

    #[test]
    fn loopback_network_contexts_are_local() {
        for host in [
            "tcp://127.0.0.1:2375",
            "http://localhost:2375",
            "https://[::1]:2376",
        ] {
            let output =
                format!(r#"[{{"Name":"loopback","Endpoints":{{"docker":{{"Host":{host:?}}}}}}}]"#);
            let context = parse_docker_context(output.as_bytes()).unwrap();
            assert_eq!(context.kind, DockerContextKind::Local, "{host}");
            assert_eq!(context.endpoint.unwrap().scope, EndpointScope::Local);
        }
    }

    #[test]
    fn parses_version_info_rootless_desktop_and_sanitized_mirrors() {
        let version = parse_docker_version(
            br#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"28.5.2"}}"#,
        )
        .unwrap();
        assert_eq!(version.client.unwrap(), Version::new(29, 0, 1));
        assert_eq!(version.server.unwrap(), Version::new(28, 5, 2));

        let info = parse_docker_info(
            br#"{"Name":"docker-desktop","OperatingSystem":"Docker Desktop","OSType":"linux","Architecture":"aarch64","SecurityOptions":["name=rootless"],"RegistryConfig":{"Mirrors":["https://alice:secret@mirror.example/private?token=x"]}}"#,
        )
        .unwrap();
        assert!(info.rootless);
        assert!(info.desktop);
        assert_eq!(info.architecture.as_deref(), Some("aarch64"));
        let mirror = serde_json::to_string(&info.registry_mirrors).unwrap();
        assert_eq!(mirror, r#"["https://mirror.example/[redacted]?redacted"]"#);
    }

    #[test]
    fn discovery_uses_only_injected_read_only_commands() {
        let runner = FakeRunner::with_outcomes([
            success(r#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"29.0.1"}}"#),
            success(
                r#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            ),
            success(
                r#"{"OperatingSystem":"Linux","OSType":"linux","Architecture":"x86_64","RegistryConfig":{"Mirrors":[]}}"#,
            ),
        ]);

        let discovery = DockerAdapter.discover(&runner, CaptureLimits::default());
        assert_eq!(discovery.status, DiagnosticStatus::Healthy);
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec![
                vec!["version", "--format", "{{json .}}"],
                vec!["context", "inspect"],
                vec!["info", "--format", "{{json .}}"],
            ]
        );
    }

    #[test]
    fn classifies_not_installed_without_extra_probes() {
        let runner = FakeRunner::with_outcomes([CommandOutcome::NotInstalled]);
        let discovery = DockerAdapter.discover(&runner, CaptureLimits::default());
        assert_eq!(discovery.status, DiagnosticStatus::NotInstalled);
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn classifies_client_only_unreachable_permission_and_unsupported() {
        let client = r#"{"Client":{"Version":"29.0.1"},"Server":null}"#;
        let client_only = FakeRunner::with_outcomes([
            success(client),
            success(
                r#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            ),
            success("{}"),
        ]);
        assert_eq!(
            DockerAdapter
                .discover(&client_only, CaptureLimits::default())
                .status,
            DiagnosticStatus::ClientOnly
        );

        let unreachable = FakeRunner::with_outcomes([
            failure(client, "Cannot connect to the Docker daemon"),
            success(
                r#"[{"Name":"remote","Endpoints":{"docker":{"Host":"tcp://host.example:2376"}}}]"#,
            ),
            failure("{}", "connection refused"),
        ]);
        assert_eq!(
            DockerAdapter
                .discover(&unreachable, CaptureLimits::default())
                .status,
            DiagnosticStatus::Unreachable
        );

        let denied = FakeRunner::with_outcomes([
            failure(client, "permission denied while trying to connect"),
            success(
                r#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            ),
            failure("{}", "permission denied"),
        ]);
        assert_eq!(
            DockerAdapter
                .discover(&denied, CaptureLimits::default())
                .status,
            DiagnosticStatus::PermissionDenied
        );

        let unsupported = FakeRunner::with_outcomes([success(
            r#"{"Client":{"Version":"18.09.9"},"Server":null}"#,
        )]);
        assert_eq!(
            DockerAdapter
                .discover(&unsupported, CaptureLimits::default())
                .status,
            DiagnosticStatus::UnsupportedVersion
        );
    }

    #[test]
    fn truncated_output_is_not_parsed_as_authoritative() {
        let runner = FakeRunner::with_outcomes([
            CommandOutcome::Exited {
                status: exit_status(0),
                output: CapturedOutput {
                    stdout: br#"{"Client":{"Version":"29.0.1"}}"#.to_vec(),
                    stdout_truncated: true,
                    ..CapturedOutput::default()
                },
            },
            success("[]"),
            success("{}"),
        ]);
        assert_eq!(
            DockerAdapter
                .discover(&runner, CaptureLimits::default())
                .status,
            DiagnosticStatus::Degraded
        );
    }

    #[test]
    fn failed_parseable_stdout_never_establishes_discovery_facts() {
        let version = r#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"29.0.1"}}"#;
        let context =
            r#"[{"Name":"stale","Endpoints":{"docker":{"Host":"tcp://host.example:2375"}}}]"#;
        let info = r#"{"OperatingSystem":"stale","ServerVersion":"29.0.1"}"#;
        let runner = FakeRunner::with_outcomes([
            failure(version, "cannot connect to the Docker daemon"),
            failure(context, "cannot connect"),
            failure(info, "connection refused"),
        ]);

        let discovery = DockerAdapter.discover(&runner, CaptureLimits::default());

        assert_eq!(discovery.status, DiagnosticStatus::Unreachable);
        assert!(discovery.version.is_none());
        assert!(discovery.context.is_none());
        assert!(discovery.info.is_none());
    }

    #[test]
    fn docker_versions_use_shared_vendor_parser_without_truncation() {
        let parsed = parse_docker_version(
            br#"{"Client":{"Version":"V29.1"},"Server":{"Version":"1.2.3.4"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.client, Some(Version::new(29, 1, 0)));
        assert_eq!(parsed.server, None);
    }

    #[test]
    fn timeout_is_unreachable_and_does_not_require_a_daemon() {
        let runner = FakeRunner::with_outcomes([
            success(r#"{"Client":{"Version":"29.0.1"},"Server":null}"#),
            success(
                r#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            ),
            CommandOutcome::TimedOut {
                output: CapturedOutput::default(),
                termination: TerminationStatus::Requested,
            },
        ]);
        assert_eq!(
            DockerAdapter
                .discover(&runner, CaptureLimits::default())
                .status,
            DiagnosticStatus::Unreachable
        );
    }

    #[test]
    fn diagnostic_evidence_matches_only_commands_that_ran() {
        let runner = FakeRunner::with_outcomes([CommandOutcome::PermissionDenied]);
        let report = DockerAdapter.diagnose(&runner, CaptureLimits::default());

        assert_eq!(report.status, DiagnosticStatus::PermissionDenied);
        assert_eq!(report.evidence.len(), 1);
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }
}
