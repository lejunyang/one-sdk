//! Stable, deterministic, secret-safe diagnostic output contracts.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::redact::{RedactedCommand, RedactedHeader, RedactedUrl};

pub const DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

/// The native control plane being inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    Docker,
    Containerd,
    Buildkit,
    Podman,
}

/// Overall state of one native runtime or builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticStatus {
    Healthy,
    Degraded,
    NotInstalled,
    ClientOnly,
    Unreachable,
    PermissionDenied,
    UnsupportedVersion,
}

/// Native operations whose availability can be established by discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Client,
    Daemon,
    RuntimeInfo,
    Pull,
    CacheStatus,
    CachePrune,
    RegistryMirrors,
    RegistryHostMapping,
    BuilderInspection,
    PlatformSelection,
}

/// State of an individually discovered capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unavailable,
    Unknown,
}

/// Privilege boundary for an inspected or proposed native operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Privilege {
    None,
    CurrentUser,
    Root,
    Administrator,
    RemoteAdministrator,
    Unknown,
}

/// How the client reaches a daemon or builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointTransport {
    LocalSocket,
    NamedPipe,
    Tcp,
    Http,
    Https,
    Ssh,
    DockerDesktop,
    Cloud,
    Unknown,
}

/// Whether native configuration for an endpoint is local to this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointScope {
    Local,
    Remote,
    ManagedDesktop,
    ManagedCloud,
    Unknown,
}

/// A daemon or builder endpoint with its address sanitized at construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Endpoint {
    pub transport: EndpointTransport,
    pub scope: EndpointScope,
    pub address: RedactedUrl,
}

impl Endpoint {
    pub const fn new(
        transport: EndpointTransport,
        scope: EndpointScope,
        address: RedactedUrl,
    ) -> Self {
        Self {
            transport,
            scope,
            address,
        }
    }
}

/// Typed, redacted proof supporting a diagnostic conclusion.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum DiagnosticEvidence {
    Endpoint(Endpoint),
    Header(RedactedHeader),
    Command(RedactedCommand),
}

/// Stable diagnostic output. It contains no free-form serializable strings.
/// Callers must choose typed facts and redacted evidence, keeping report JSON
/// secret-safe by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub runtime: RuntimeKind,
    pub status: DiagnosticStatus,
    pub privilege: Privilege,
    pub capabilities: BTreeMap<Capability, CapabilityStatus>,
    pub endpoints: BTreeSet<Endpoint>,
    pub evidence: BTreeSet<DiagnosticEvidence>,
}

impl DiagnosticReport {
    pub fn new(runtime: RuntimeKind, status: DiagnosticStatus) -> Self {
        Self {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            runtime,
            status,
            privilege: Privilege::Unknown,
            capabilities: BTreeMap::new(),
            endpoints: BTreeSet::new(),
            evidence: BTreeSet::new(),
        }
    }

    pub fn with_privilege(mut self, privilege: Privilege) -> Self {
        self.privilege = privilege;
        self
    }

    pub fn set_capability(&mut self, capability: Capability, status: CapabilityStatus) {
        self.capabilities.insert(capability, status);
    }

    pub fn add_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints.insert(endpoint);
    }

    pub fn add_evidence(&mut self, evidence: DiagnosticEvidence) {
        self.evidence.insert(evidence);
    }
}

#[cfg(test)]
mod tests {
    use crate::process::CommandSpec;

    use super::*;
    use crate::container::redact::{CommandPurpose, NativeProgram};

    fn report(reverse: bool) -> DiagnosticReport {
        let endpoint_a = Endpoint::new(
            EndpointTransport::Https,
            EndpointScope::Remote,
            RedactedUrl::parse("https://alice:password@z.example/private?token=secret").unwrap(),
        );
        let endpoint_b = Endpoint::new(
            EndpointTransport::LocalSocket,
            EndpointScope::Local,
            RedactedUrl::parse("unix:///var/run/docker.sock").unwrap(),
        );
        let command = CommandSpec::new("docker").args(["info", "--format", "secret"]);
        let command = DiagnosticEvidence::Command(RedactedCommand::from_spec(
            NativeProgram::Docker,
            CommandPurpose::RuntimeInfo,
            &command,
        ));

        let mut report = DiagnosticReport::new(RuntimeKind::Docker, DiagnosticStatus::Healthy)
            .with_privilege(Privilege::CurrentUser);
        let capabilities = [
            (Capability::Pull, CapabilityStatus::Supported),
            (Capability::Daemon, CapabilityStatus::Supported),
        ];
        let endpoints = [endpoint_a, endpoint_b];
        if reverse {
            for (capability, status) in capabilities.into_iter().rev() {
                report.set_capability(capability, status);
            }
            for endpoint in endpoints.into_iter().rev() {
                report.add_endpoint(endpoint);
            }
        } else {
            for (capability, status) in capabilities {
                report.set_capability(capability, status);
            }
            for endpoint in endpoints {
                report.add_endpoint(endpoint);
            }
        }
        report.add_evidence(command);
        report
    }

    #[test]
    fn diagnostic_serialization_is_schema_versioned_and_deterministic() {
        let left = serde_json::to_string(&report(false)).unwrap();
        let right = serde_json::to_string(&report(true)).unwrap();

        assert_eq!(left, right);
        assert!(left.starts_with("{\"schema_version\":1,"));
        assert!(left.contains("\"daemon\":\"supported\""));
        assert!(left.contains("\"pull\":\"supported\""));
        for secret in ["alice", "password", "private", "token", "secret"] {
            assert!(!left.contains(secret), "leaked {secret}: {left}");
        }
    }
}
