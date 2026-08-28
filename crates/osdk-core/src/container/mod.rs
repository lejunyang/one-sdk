//! Native container-runtime contracts.
//!
//! This first foundation deliberately contains no OCI store and performs no
//! native configuration writes. It provides stable diagnostic, redaction, and
//! injectable process boundaries for later read-only runtime adapters.

pub mod buildkit;
pub mod containerd;
pub mod docker;
pub mod redact;
pub mod report;
pub mod runtime;

pub use buildkit::{
    BuildPlatform, BuilderDriver, BuilderNode, BuilderNodeStatus, BuildkitAdapter,
    BuildkitDiscovery, BuildxBuilderSelector, BuildxBuilderSelectorError, SelectedBuilder,
};
pub use containerd::{
    parse_effective_config, parse_hosts_toml, read_hosts_toml, ContainerdAdapter, ContainerdConfig,
    ContainerdDiscovery, ContainerdParseError, ContainerdVersions, LegacyRegistryWarning,
    RegistryHost, RegistryHostCapability, RegistryHostTls, RegistryHosts,
};
pub use docker::{
    DockerAdapter, DockerContext, DockerContextKind, DockerDiscovery, DockerInfo, DockerParseError,
    DockerVersion,
};

pub use redact::{
    CommandPurpose, HeaderName, NativeProgram, RedactedCommand, RedactedHeader, RedactedUrl,
    RedactedUrlError, RedactedValue, REDACTED,
};
pub use report::{
    Capability, CapabilityStatus, DiagnosticEvidence, DiagnosticReport, DiagnosticStatus, Endpoint,
    EndpointScope, EndpointTransport, Privilege, RuntimeKind, DIAGNOSTIC_SCHEMA_VERSION,
};
pub use runtime::{ForegroundCommand, ProbeCommand, RuntimeAdapter};

/// Convert one vendor-supplied version candidate into a semantic version.
///
/// Runtime-specific parsers remain responsible for locating a candidate in
/// their output. This shared boundary accepts an optional `v`/`V` prefix,
/// normalizes one- and two-component numeric versions, preserves valid SemVer
/// prerelease/build metadata, and tolerates the `~vendor` suffix emitted by
/// some distro packages. Extra numeric components are rejected.
pub(crate) fn parse_vendor_version(candidate: &str) -> Option<semver::Version> {
    let candidate = candidate.trim().trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
        )
    });
    let candidate = candidate.strip_prefix(['v', 'V']).unwrap_or(candidate);
    if let Ok(version) = semver::Version::parse(candidate) {
        return Some(version);
    }

    // Debian-style versions commonly append `~ds1` or a similar packaging
    // suffix which is not SemVer. Only discard that explicitly delimited
    // suffix; never truncate an arbitrary or fourth numeric component.
    if let Some((upstream, _vendor)) = candidate.split_once('~') {
        if let Ok(version) = semver::Version::parse(upstream) {
            return Some(version);
        }
    }

    let components = candidate.split('.').collect::<Vec<_>>();
    if !(1..=3).contains(&components.len())
        || components.iter().any(|component| {
            component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let major = components[0].parse().ok()?;
    let minor = components.get(1).unwrap_or(&"0").parse().ok()?;
    let patch = components.get(2).unwrap_or(&"0").parse().ok()?;
    Some(semver::Version::new(major, minor, patch))
}

/// Classify a native endpoint without retaining any unredacted URL fields.
/// Loopback TCP and HTTP(S) endpoints are local; SSH remains remote even when
/// it targets loopback because it still crosses an administrative boundary.
pub(crate) fn classify_endpoint(raw: &str) -> (EndpointTransport, EndpointScope) {
    let scheme = raw
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match scheme.as_str() {
        "unix" => (EndpointTransport::LocalSocket, EndpointScope::Local),
        "npipe" => (EndpointTransport::NamedPipe, EndpointScope::Local),
        "tcp" => (EndpointTransport::Tcp, network_endpoint_scope(raw)),
        "http" => (EndpointTransport::Http, network_endpoint_scope(raw)),
        "https" => (EndpointTransport::Https, network_endpoint_scope(raw)),
        "ssh" => (EndpointTransport::Ssh, EndpointScope::Remote),
        _ => (EndpointTransport::Unknown, EndpointScope::Unknown),
    }
}

fn network_endpoint_scope(raw: &str) -> EndpointScope {
    let local = reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_end_matches('.');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if local {
        EndpointScope::Local
    } else {
        EndpointScope::Remote
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;

    #[test]
    fn vendor_versions_have_one_consistent_strict_conversion() {
        for (raw, expected) in [
            ("1", Some(semver::Version::new(1, 0, 0))),
            ("V1.2", Some(semver::Version::new(1, 2, 0))),
            ("v1.2.3", Some(semver::Version::new(1, 2, 3))),
            (
                "(v1.2.3-rc.1+vendor)",
                semver::Version::parse("1.2.3-rc.1+vendor").ok(),
            ),
            ("1.7.22~ds1-1", Some(semver::Version::new(1, 7, 22))),
            ("1.2.3.4", None),
            ("release-1.2.3", None),
            ("", None),
        ] {
            assert_eq!(parse_vendor_version(raw), expected, "candidate: {raw}");
        }
    }

    #[test]
    fn loopback_network_endpoints_are_local_but_ssh_is_remote() {
        for raw in [
            "tcp://127.0.0.1:2375",
            "http://LOCALHOST:2375",
            "https://[::1]:2376",
        ] {
            assert_eq!(classify_endpoint(raw).1, EndpointScope::Local, "{raw}");
        }
        assert_eq!(
            classify_endpoint("tcp://192.0.2.10:2375").1,
            EndpointScope::Remote
        );
        assert_eq!(
            classify_endpoint("ssh://localhost").1,
            EndpointScope::Remote
        );
    }
}
