//! Read-only containerd discovery and registry-host inspection.
//!
//! The adapter uses only native read-only commands. Configuration parsers are
//! also public so callers can inspect already captured text or an explicitly
//! selected file without granting this module directory-discovery authority.

use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

use semver::Version;

use super::redact::{CommandPurpose, NativeProgram, RedactedUrl};
use super::report::{
    Capability, CapabilityStatus, DiagnosticEvidence, DiagnosticReport, DiagnosticStatus, Endpoint,
    EndpointScope, Privilege, RuntimeKind,
};
use super::runtime::{ProbeCommand, RuntimeAdapter};
use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

const MINIMUM_CONTAINERD_VERSION: Version = Version::new(1, 6, 0);
#[cfg(not(windows))]
const DEFAULT_ADDRESS: &str = "unix:///run/containerd/containerd.sock";
#[cfg(windows)]
const DEFAULT_ADDRESS: &str = "npipe:////./pipe/containerd-containerd";
const DEFAULT_NAMESPACE: &str = "default";

/// Client and server versions reported by the native programs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerdVersions {
    pub containerd: Option<Version>,
    pub ctr_client: Option<Version>,
    pub ctr_server: Option<Version>,
}

/// A typed warning about deprecated inline CRI registry configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LegacyRegistryWarning {
    Mirrors,
    Configs,
    Auths,
}

/// Relevant fields from `containerd config dump`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerdConfig {
    pub version: Option<u32>,
    pub registry_config_path: Option<PathBuf>,
    pub legacy_registry: BTreeSet<LegacyRegistryWarning>,
}

/// Registry-host capabilities understood by containerd.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistryHostCapability {
    Pull,
    Resolve,
    Push,
    Unknown,
}

/// TLS-related host settings. Paths are intentionally represented only by
/// presence/count so reports cannot accidentally disclose local layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryHostTls {
    pub skip_verify: bool,
    pub ca_count: usize,
    pub client_certificate_configured: bool,
}

/// One `[host."..."]` entry in a containerd `hosts.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryHost {
    pub endpoint: RedactedUrl,
    pub capabilities: BTreeSet<RegistryHostCapability>,
    pub override_path: bool,
    pub tls: RegistryHostTls,
}

/// Parsed, secret-safe registry namespace host configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryHosts {
    pub server: Option<RedactedUrl>,
    pub hosts: Vec<RegistryHost>,
}

/// Structural errors that never retain native output or file contents.
#[derive(Debug, thiserror::Error)]
pub enum ContainerdParseError {
    #[error("invalid containerd TOML")]
    InvalidToml,
    #[error("invalid containerd endpoint URL")]
    InvalidEndpoint,
    #[error("containerd endpoint selectors cannot contain credentials, query, or fragments")]
    UnsafeEndpoint,
    #[error("invalid containerd namespace")]
    InvalidNamespace,
    #[error("hosts path must explicitly name hosts.toml without parent traversal")]
    UnsafeHostsPath,
    #[error("could not read the explicitly selected hosts.toml")]
    Read(#[source] io::Error),
}

/// Typed result of one containerd discovery pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerdDiscovery {
    pub report: DiagnosticReport,
    pub versions: ContainerdVersions,
    pub address: RedactedUrl,
    pub namespace: String,
    pub config: Option<ContainerdConfig>,
}

/// Read-only containerd adapter. Selectors are passed directly to `ctr` rather
/// than inferred from mutable process-global environment variables.
#[derive(Clone, PartialEq, Eq)]
pub struct ContainerdAdapter {
    address: String,
    namespace: String,
}

impl std::fmt::Debug for ContainerdAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerdAdapter")
            .field("address", &self.address().ok())
            .field("namespace", &"[redacted]")
            .finish()
    }
}

impl Default for ContainerdAdapter {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS.to_owned(),
            namespace: DEFAULT_NAMESPACE.to_owned(),
        }
    }
}

impl ContainerdAdapter {
    /// Create an adapter with explicit daemon address and namespace selectors.
    /// The address must use a supported endpoint URL scheme.
    pub fn new(
        address: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Result<Self, ContainerdParseError> {
        let address = address.into();
        let parsed =
            reqwest::Url::parse(&address).map_err(|_| ContainerdParseError::InvalidEndpoint)?;
        RedactedUrl::parse(&address).map_err(|_| ContainerdParseError::InvalidEndpoint)?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ContainerdParseError::UnsafeEndpoint);
        }
        let namespace = namespace.into();
        if namespace.is_empty()
            || namespace.starts_with('-')
            || namespace.len() > 128
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(ContainerdParseError::InvalidNamespace);
        }
        Ok(Self { address, namespace })
    }

    pub fn address(&self) -> Result<RedactedUrl, ContainerdParseError> {
        RedactedUrl::parse(&self.address).map_err(|_| ContainerdParseError::InvalidEndpoint)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Query the containerd binary, the `ctr` client/server, and the effective
    /// config. No command writes configuration or daemon state.
    pub fn inspect(
        &self,
        runner: &dyn CommandRunner,
        limits: CaptureLimits,
    ) -> ContainerdDiscovery {
        let address = self.address().expect("validated containerd address");
        let mut report = DiagnosticReport::new(RuntimeKind::Containerd, DiagnosticStatus::Degraded)
            .with_privilege(Privilege::CurrentUser);
        let endpoint = endpoint_from_url(address.clone());
        report.add_endpoint(endpoint.clone());
        report.add_evidence(DiagnosticEvidence::Endpoint(endpoint.clone()));

        let daemon_probe = probe(
            NativeProgram::Containerd,
            CommandPurpose::Version,
            CommandSpec::new("containerd").arg("--version"),
        );
        report.add_evidence(DiagnosticEvidence::Command(daemon_probe.evidence().clone()));
        let daemon_outcome = daemon_probe.execute(runner, limits);

        let ctr_probe = probe(
            NativeProgram::Ctr,
            CommandPurpose::Version,
            self.ctr_command().arg("version"),
        );
        report.add_evidence(DiagnosticEvidence::Command(ctr_probe.evidence().clone()));
        let ctr_outcome = ctr_probe.execute(runner, limits);

        let mut versions = ContainerdVersions {
            containerd: successful_stdout(&daemon_outcome).and_then(parse_first_version),
            ctr_client: None,
            ctr_server: None,
        };
        if let Some(stdout) = successful_stdout(&ctr_outcome) {
            let parsed = parse_ctr_versions(stdout);
            versions.ctr_client = parsed.0;
            versions.ctr_server = parsed.1;
        }

        let (config_outcome, config) = if endpoint.scope == EndpointScope::Local {
            let config_probe = probe(
                NativeProgram::Containerd,
                CommandPurpose::RuntimeInfo,
                CommandSpec::new("containerd").args(["config", "dump"]),
            );
            report.add_evidence(DiagnosticEvidence::Command(config_probe.evidence().clone()));
            let outcome = config_probe.execute(runner, limits);
            let config =
                successful_stdout(&outcome).and_then(|bytes| parse_effective_config(bytes).ok());
            (Some(outcome), config)
        } else {
            // A local `containerd config dump` says nothing authoritative
            // about a daemon reached through a remote endpoint.
            (None, None)
        };

        let ctr_installed = !matches!(ctr_outcome, CommandOutcome::NotInstalled);
        let daemon_installed = !matches!(daemon_outcome, CommandOutcome::NotInstalled);
        let server_reached = versions.ctr_server.is_some();
        let unsupported = [
            &versions.containerd,
            &versions.ctr_client,
            &versions.ctr_server,
        ]
        .into_iter()
        .flatten()
        .any(|version| version < &MINIMUM_CONTAINERD_VERSION);
        report.status = classify_status(
            daemon_installed,
            ctr_installed,
            server_reached,
            unsupported,
            [&daemon_outcome, &ctr_outcome]
                .into_iter()
                .chain(config_outcome.as_ref())
                .collect::<Vec<_>>(),
        );
        if report.status == DiagnosticStatus::Healthy
            && matches!(
                config_outcome.as_ref(),
                Some(CommandOutcome::Exited { status, .. }) if status.success()
            )
            && config.is_none()
        {
            report.status = DiagnosticStatus::Degraded;
        }

        report.set_capability(
            Capability::Client,
            if versions.ctr_client.is_some() {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unavailable
            },
        );
        for capability in [
            Capability::Daemon,
            Capability::RuntimeInfo,
            Capability::Pull,
        ] {
            report.set_capability(
                capability,
                if server_reached {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unavailable
                },
            );
        }
        // containerd exposes several namespace-dependent content, snapshot,
        // and CRI views, but no single supported aggregate cache contract.
        // Keep diagnostics aligned with ContainerdCacheQuery instead of
        // inferring cache support from daemon reachability.
        for capability in [Capability::CacheStatus, Capability::CachePrune] {
            report.set_capability(capability, CapabilityStatus::Unsupported);
        }
        report.set_capability(
            Capability::RegistryHostMapping,
            if config
                .as_ref()
                .and_then(|value| value.registry_config_path.as_ref())
                .is_some()
            {
                CapabilityStatus::Supported
            } else if server_reached {
                CapabilityStatus::Unknown
            } else {
                CapabilityStatus::Unavailable
            },
        );
        report.set_capability(
            Capability::RegistryMirrors,
            *report
                .capabilities
                .get(&Capability::RegistryHostMapping)
                .unwrap_or(&CapabilityStatus::Unknown),
        );

        ContainerdDiscovery {
            report,
            versions,
            address,
            namespace: self.namespace.clone(),
            config,
        }
    }

    fn ctr_command(&self) -> CommandSpec {
        CommandSpec::new("ctr").args([
            "--address",
            self.address.as_str(),
            "--namespace",
            self.namespace.as_str(),
        ])
    }
}

impl RuntimeAdapter for ContainerdAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Containerd
    }

    fn diagnose(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport {
        self.inspect(runner, limits).report
    }
}

/// Parse the relevant portions of `containerd config dump` for either the 1.x
/// or 2.x CRI plugin table. Unknown keys are ignored, never rewritten.
pub fn parse_effective_config(input: &[u8]) -> Result<ContainerdConfig, ContainerdParseError> {
    let text = std::str::from_utf8(input).map_err(|_| ContainerdParseError::InvalidToml)?;
    let root: toml::Value = toml::from_str(text).map_err(|_| ContainerdParseError::InvalidToml)?;
    let version = root
        .get("version")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok());
    let plugins = root.get("plugins").and_then(toml::Value::as_table);
    let legacy_registry = plugins
        .and_then(|plugins| plugins.get("io.containerd.grpc.v1.cri"))
        .and_then(|value| value.get("registry"));
    let current_registry = plugins
        .and_then(|plugins| plugins.get("io.containerd.cri.v1.images"))
        .and_then(|value| value.get("registry"))
        .or_else(|| {
            plugins
                .and_then(|plugins| plugins.get("io.containerd.cri.v1.images"))
                .and_then(|value| value.get("containerd"))
                .and_then(|value| value.get("registry"))
        });
    let registry_config_path = current_registry
        .and_then(|value| value.get("config_path"))
        .and_then(toml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            legacy_registry
                .and_then(|value| value.get("config_path"))
                .and_then(toml::Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .map(PathBuf::from);
    let mut legacy_warnings = BTreeSet::new();
    for (name, warning) in [
        ("mirrors", LegacyRegistryWarning::Mirrors),
        ("configs", LegacyRegistryWarning::Configs),
        ("auths", LegacyRegistryWarning::Auths),
    ] {
        if legacy_registry.and_then(|value| value.get(name)).is_some()
            || current_registry.and_then(|value| value.get(name)).is_some()
        {
            legacy_warnings.insert(warning);
        }
    }
    Ok(ContainerdConfig {
        version,
        registry_config_path,
        legacy_registry: legacy_warnings,
    })
}

/// Parse one `hosts.toml`, sanitizing every endpoint and reducing TLS paths to
/// typed presence metadata.
pub fn parse_hosts_toml(input: &str) -> Result<RegistryHosts, ContainerdParseError> {
    let root: toml::Value = toml::from_str(input).map_err(|_| ContainerdParseError::InvalidToml)?;
    let ordered: toml_edit::DocumentMut = input
        .parse()
        .map_err(|_| ContainerdParseError::InvalidToml)?;
    let server = root
        .get("server")
        .and_then(toml::Value::as_str)
        .map(parse_url)
        .transpose()?;
    let mut hosts = Vec::new();
    if let Some(entries) = ordered.get("host").and_then(toml_edit::Item::as_table) {
        for (raw_endpoint, value) in entries.iter() {
            let Some(table) = value.as_table_like() else {
                continue;
            };
            let endpoint = parse_url(raw_endpoint)?;
            let mut capabilities = BTreeSet::new();
            if let Some(values) = table
                .get("capabilities")
                .and_then(toml_edit::Item::as_array)
            {
                for value in values.iter().filter_map(toml_edit::Value::as_str) {
                    capabilities.insert(match value.to_ascii_lowercase().as_str() {
                        "pull" => RegistryHostCapability::Pull,
                        "resolve" => RegistryHostCapability::Resolve,
                        "push" => RegistryHostCapability::Push,
                        _ => RegistryHostCapability::Unknown,
                    });
                }
            }
            let tls = RegistryHostTls {
                skip_verify: table
                    .get("skip_verify")
                    .and_then(toml_edit::Item::as_bool)
                    .unwrap_or(false),
                ca_count: path_value_count(table.get("ca")),
                client_certificate_configured: path_value_count(table.get("client")) > 0,
            };
            hosts.push(RegistryHost {
                endpoint,
                capabilities,
                override_path: table
                    .get("override_path")
                    .and_then(toml_edit::Item::as_bool)
                    .unwrap_or(false),
                tls,
            });
        }
    }
    Ok(RegistryHosts { server, hosts })
}

/// Read exactly the caller-selected `hosts.toml`. Parent components are
/// rejected to prevent this convenience helper from becoming a traversal API.
pub fn read_hosts_toml(path: &Path) -> Result<RegistryHosts, ContainerdParseError> {
    if path.file_name().and_then(|name| name.to_str()) != Some("hosts.toml")
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(ContainerdParseError::UnsafeHostsPath);
    }
    let text = std::fs::read_to_string(path).map_err(ContainerdParseError::Read)?;
    parse_hosts_toml(&text)
}

fn probe(program: NativeProgram, purpose: CommandPurpose, command: CommandSpec) -> ProbeCommand {
    ProbeCommand::new(program, purpose, command)
}

fn endpoint_from_url(address: RedactedUrl) -> Endpoint {
    let (transport, scope) = super::classify_endpoint(address.as_str());
    Endpoint::new(transport, scope, address)
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

fn parse_first_version(input: &[u8]) -> Option<Version> {
    let text = std::str::from_utf8(input).ok()?;
    text.split(|character: char| character.is_whitespace() || character == ',' || character == ':')
        .find_map(super::parse_vendor_version)
}

fn parse_ctr_versions(input: &[u8]) -> (Option<Version>, Option<Version>) {
    let Ok(text) = std::str::from_utf8(input) else {
        return (None, None);
    };
    let mut section = None;
    let mut client = None;
    let mut server = None;
    for line in text.lines() {
        match line
            .trim()
            .trim_end_matches(':')
            .to_ascii_lowercase()
            .as_str()
        {
            "client" => section = Some(false),
            "server" => section = Some(true),
            _ if line
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("version:") =>
            {
                let version = line
                    .split_once(':')
                    .and_then(|pair| super::parse_vendor_version(pair.1.trim()));
                match section {
                    Some(false) => client = version,
                    Some(true) => server = version,
                    None => {}
                }
            }
            _ => {}
        }
    }
    (client, server)
}

fn parse_url(raw: &str) -> Result<RedactedUrl, ContainerdParseError> {
    RedactedUrl::parse(raw).map_err(|_| ContainerdParseError::InvalidEndpoint)
}

fn path_value_count(value: Option<&toml_edit::Item>) -> usize {
    match value {
        Some(value) if value.as_str().is_some() => usize::from(!value.as_str().unwrap().is_empty()),
        Some(value) if value.as_array().is_some() => value.as_array().unwrap().len(),
        _ => 0,
    }
}

fn classify_status(
    daemon_installed: bool,
    ctr_installed: bool,
    server_reached: bool,
    unsupported: bool,
    outcomes: Vec<&CommandOutcome>,
) -> DiagnosticStatus {
    if outcomes
        .iter()
        .any(|outcome| matches!(outcome, CommandOutcome::PermissionDenied))
        || outcomes
            .iter()
            .any(|outcome| output_contains(outcome, &["permission denied", "access is denied"]))
    {
        DiagnosticStatus::PermissionDenied
    } else if !daemon_installed && !ctr_installed {
        DiagnosticStatus::NotInstalled
    } else if unsupported {
        DiagnosticStatus::UnsupportedVersion
    } else if outcomes
        .iter()
        .any(|outcome| matches!(outcome, CommandOutcome::TimedOut { .. }))
    {
        DiagnosticStatus::Unreachable
    } else if outcomes.iter().any(|outcome| {
        output_contains(
            outcome,
            &[
                "connection refused",
                "deadline exceeded",
                "transport is closing",
                "failed to dial",
                "context deadline",
            ],
        )
    }) {
        DiagnosticStatus::Unreachable
    } else if server_reached {
        DiagnosticStatus::Healthy
    } else if ctr_installed {
        DiagnosticStatus::ClientOnly
    } else {
        DiagnosticStatus::Degraded
    }
}

fn output_contains(outcome: &CommandOutcome, needles: &[&str]) -> bool {
    let Some(output) = outcome.output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| stdout.contains(needle) || stderr.contains(needle))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    use crate::process::CapturedOutput;

    use super::*;

    struct FakeRunner {
        outcomes: Mutex<VecDeque<CommandOutcome>>,
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run_captured(&self, command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            self.commands.lock().unwrap().push(
                std::iter::once(command.program().to_string_lossy().into_owned())
                    .chain(
                        command
                            .arguments()
                            .iter()
                            .map(|value| value.to_string_lossy().into_owned()),
                    )
                    .collect(),
            );
            self.outcomes.lock().unwrap().pop_front().unwrap()
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("discovery must not run foreground commands")
        }
    }

    fn success(stdout: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: success_status(),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(unix)]
    fn success_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn success_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[test]
    fn parses_v1_and_v2_cri_config_paths_and_legacy_keys() {
        let v1 = parse_effective_config(
            br#"
version = 2
[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = "/etc/containerd/certs.d"
  [plugins."io.containerd.grpc.v1.cri".registry.mirrors]
"#,
        )
        .unwrap();
        assert_eq!(v1.version, Some(2));
        assert_eq!(
            v1.registry_config_path,
            Some(PathBuf::from("/etc/containerd/certs.d"))
        );
        assert!(v1.legacy_registry.contains(&LegacyRegistryWarning::Mirrors));

        let v2 = parse_effective_config(
            br#"
version = 3
[plugins."io.containerd.cri.v1.images".registry]
config_path = "/etc/containerd/certs-v2.d"
"#,
        )
        .unwrap();
        assert_eq!(
            v2.registry_config_path,
            Some(PathBuf::from("/etc/containerd/certs-v2.d"))
        );
    }

    #[test]
    fn current_config_path_wins_while_legacy_warnings_are_unioned() {
        let config = parse_effective_config(
            br#"
version = 3
[plugins."io.containerd.grpc.v1.cri".registry]
config_path = "/legacy"
[plugins."io.containerd.grpc.v1.cri".registry.mirrors]
[plugins."io.containerd.cri.v1.images".registry]
config_path = "/current"
[plugins."io.containerd.cri.v1.images".registry.configs]
"#,
        )
        .unwrap();

        assert_eq!(config.registry_config_path, Some(PathBuf::from("/current")));
        assert_eq!(
            config.legacy_registry,
            BTreeSet::from([
                LegacyRegistryWarning::Mirrors,
                LegacyRegistryWarning::Configs,
            ])
        );
    }

    #[test]
    fn parses_hosts_without_retaining_tls_paths_or_endpoint_secrets() {
        let parsed = parse_hosts_toml(
            r#"
server = "https://user:password@registry.example/private?token=secret"
[host."https://mirror.example/prefix?key=secret"]
capabilities = ["pull", "resolve"]
override_path = true
skip_verify = true
ca = ["/secret/ca.pem", "/other/ca.pem"]
client = [["/secret/cert.pem", "/secret/key.pem"]]
"#,
        )
        .unwrap();
        assert_eq!(
            parsed.server.as_ref().unwrap().as_str(),
            "https://registry.example/[redacted]?redacted"
        );
        assert_eq!(parsed.hosts.len(), 1);
        let host = &parsed.hosts[0];
        assert!(host.capabilities.contains(&RegistryHostCapability::Pull));
        assert!(host.capabilities.contains(&RegistryHostCapability::Resolve));
        assert!(host.override_path);
        assert!(host.tls.skip_verify);
        assert_eq!(host.tls.ca_count, 2);
        assert!(host.tls.client_certificate_configured);
        assert!(!format!("{parsed:?}").contains("secret"));
    }

    #[test]
    fn hosts_toml_preserves_declared_host_priority() {
        let parsed = parse_hosts_toml(
            r#"
[host."https://z-first.example"]
capabilities = ["pull"]
[host."https://a-second.example"]
capabilities = ["pull"]
"#,
        )
        .unwrap();

        assert_eq!(
            parsed
                .hosts
                .iter()
                .map(|host| host.endpoint.as_str())
                .collect::<Vec<_>>(),
            ["https://z-first.example/", "https://a-second.example/"]
        );
    }

    #[test]
    fn discovery_uses_explicit_selectors_and_reports_healthy_server() {
        let runner = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
            success("version = 2\n[plugins.\"io.containerd.grpc.v1.cri\".registry]\nconfig_path = \"/etc/containerd/certs.d\"\n"),
        ]);
        let adapter = ContainerdAdapter::new("unix:///custom/containerd.sock", "k8s.io").unwrap();
        let discovery = adapter.inspect(&runner, CaptureLimits::default());
        assert_eq!(discovery.report.status, DiagnosticStatus::Healthy);
        assert_eq!(discovery.namespace, "k8s.io");
        assert_eq!(discovery.versions.ctr_server, Some(Version::new(1, 7, 22)));
        assert_eq!(
            discovery.report.capabilities.get(&Capability::CacheStatus),
            Some(&CapabilityStatus::Unsupported)
        );
        assert_eq!(
            discovery.report.capabilities.get(&Capability::CachePrune),
            Some(&CapabilityStatus::Unsupported)
        );
        let commands = runner.commands.lock().unwrap();
        assert_eq!(
            commands[1],
            [
                "ctr",
                "--address",
                "unix:///custom/containerd.sock",
                "--namespace",
                "k8s.io",
                "version"
            ]
        );
    }

    #[test]
    fn remote_address_does_not_probe_or_associate_local_config() {
        let runner = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
        ]);
        let adapter = ContainerdAdapter::new("tcp://192.0.2.10:1234", "default").unwrap();

        let discovery = adapter.inspect(&runner, CaptureLimits::default());

        assert_eq!(discovery.report.status, DiagnosticStatus::Healthy);
        assert!(discovery.config.is_none());
        assert_eq!(
            discovery
                .report
                .capabilities
                .get(&Capability::RegistryHostMapping),
            Some(&CapabilityStatus::Unknown)
        );
        assert_eq!(runner.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn loopback_tcp_address_is_local_and_may_probe_local_config() {
        let runner = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
            success("version = 2"),
        ]);
        let adapter = ContainerdAdapter::new("tcp://127.0.0.1:1234", "default").unwrap();

        let discovery = adapter.inspect(&runner, CaptureLimits::default());

        assert_eq!(
            discovery.report.endpoints.iter().next().unwrap().scope,
            EndpointScope::Local
        );
        assert!(discovery.config.is_some());
        assert_eq!(runner.commands.lock().unwrap().len(), 3);
    }

    #[test]
    fn failed_ctr_stdout_does_not_establish_server_reachability() {
        let runner = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            CommandOutcome::Exited {
                status: failure_status(),
                output: CapturedOutput {
                    stdout: b"Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n".to_vec(),
                    stderr: b"failed to dial: connection refused".to_vec(),
                    ..CapturedOutput::default()
                },
            },
            success("version = 2"),
        ]);

        let discovery = ContainerdAdapter::default().inspect(&runner, CaptureLimits::default());

        assert_eq!(discovery.report.status, DiagnosticStatus::Unreachable);
        assert!(discovery.versions.ctr_client.is_none());
        assert!(discovery.versions.ctr_server.is_none());
        assert_eq!(
            discovery.report.capabilities.get(&Capability::Daemon),
            Some(&CapabilityStatus::Unavailable)
        );
    }

    #[test]
    fn containerd_versions_use_shared_vendor_parser() {
        assert_eq!(
            parse_first_version(b"containerd V1.7.22~ds1-1 abc"),
            Some(Version::new(1, 7, 22))
        );
        assert_eq!(parse_first_version(b"containerd 1.2.3.4 abc"), None);
        assert_eq!(
            parse_ctr_versions(b"Client:\n Version: V1.7\nServer:\n Version: v1.7.22~ds1-1\n"),
            (Some(Version::new(1, 7, 0)), Some(Version::new(1, 7, 22)))
        );
    }

    #[test]
    fn platform_default_address_uses_native_transport() {
        let adapter = ContainerdAdapter::default();
        #[cfg(windows)]
        assert_eq!(adapter.address, "npipe:////./pipe/containerd-containerd");
        #[cfg(not(windows))]
        assert_eq!(adapter.address, "unix:///run/containerd/containerd.sock");
    }

    #[test]
    fn windows_named_pipe_is_a_valid_direct_ctr_selector() {
        let adapter =
            ContainerdAdapter::new("npipe:////./pipe/containerd-containerd", "default").unwrap();
        let command = adapter.ctr_command().arg("version");
        let arguments = command
            .arguments()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "--address",
                "npipe:////./pipe/containerd-containerd",
                "--namespace",
                "default",
                "version",
            ]
        );
        assert_eq!(adapter.address().unwrap().as_str(), "npipe:///[redacted]");
    }

    #[test]
    fn rejects_namespace_values_that_could_be_cli_options() {
        assert!(matches!(
            ContainerdAdapter::new("unix:///run/containerd/containerd.sock", "--help"),
            Err(ContainerdParseError::InvalidNamespace)
        ));
        assert!(ContainerdAdapter::new("unix:///run/containerd/containerd.sock", "k8s.io").is_ok());
        assert!(matches!(
            ContainerdAdapter::new(
                "tcp://user:secret@host.example:1234/socket?token=x",
                "default"
            ),
            Err(ContainerdParseError::UnsafeEndpoint)
        ));
    }

    #[test]
    fn missing_tools_and_unreachable_daemon_are_distinct() {
        let missing = FakeRunner::new(vec![
            CommandOutcome::NotInstalled,
            CommandOutcome::NotInstalled,
            CommandOutcome::NotInstalled,
        ]);
        assert_eq!(
            ContainerdAdapter::default()
                .inspect(&missing, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::NotInstalled
        );

        let unreachable = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            CommandOutcome::Exited {
                status: failure_status(),
                output: CapturedOutput {
                    stderr: b"failed to dial: connection refused".to_vec(),
                    ..CapturedOutput::default()
                },
            },
            success("version = 2"),
        ]);
        assert_eq!(
            ContainerdAdapter::default()
                .inspect(&unreachable, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::Unreachable
        );

        let timed_out = FakeRunner::new(vec![
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            CommandOutcome::TimedOut {
                output: CapturedOutput::default(),
                termination: crate::process::TerminationStatus::Requested,
            },
            success("version = 2"),
        ]);
        assert_eq!(
            ContainerdAdapter::default()
                .inspect(&timed_out, CaptureLimits::default())
                .report
                .status,
            DiagnosticStatus::Unreachable
        );
    }

    #[cfg(unix)]
    fn failure_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(256)
    }

    #[cfg(windows)]
    fn failure_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(1)
    }
}
