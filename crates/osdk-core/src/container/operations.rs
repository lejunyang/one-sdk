//! Safe construction of exactly-once native pull and prune operations.
//!
//! This module only delegates to documented native CLI surfaces. It never
//! reads credentials, inspects a native private store, invokes a shell, or
//! retries a command. Pull and executable prune plans produce a
//! [`ForegroundCommand`], which preserves the native process streams and exit
//! status. Prune preview is data-only and cannot start a process.

use serde::Serialize;

use super::buildkit::BuildxBuilderSelector;
use super::cache::NativeCacheOwner;
use super::containerd::{ContainerdAdapter, ContainerdParseError};
use super::docker::DockerContext;
use super::plan::Fingerprint;
use super::redact::{CommandPurpose, NativeProgram};
use super::reference::{ImageReference, OciPlatform};
use super::report::{EndpointScope, EndpointTransport};
use super::runtime::ForegroundCommand;
use crate::process::CommandSpec;

/// A Docker Engine image pull built from canonical OCI input.
#[derive(Clone)]
pub struct DockerPull {
    image: ImageReference,
    platform: Option<OciPlatform>,
}

impl std::fmt::Debug for DockerPull {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerPull")
            .field("image", &"[redacted]")
            .field("has_platform", &self.platform.is_some())
            .finish()
    }
}

impl DockerPull {
    pub fn new(image: ImageReference) -> Self {
        Self {
            image,
            platform: None,
        }
    }

    pub fn with_platform(mut self, platform: OciPlatform) -> Self {
        self.platform = Some(platform);
        self
    }

    /// Consume this request and build one direct foreground operation.
    pub fn into_command(self) -> ForegroundCommand {
        let mut command = CommandSpec::new("docker").args(["image", "pull"]);
        if let Some(platform) = self.platform {
            command = command.args(["--platform".to_owned(), platform.to_string()]);
        }
        command = command.arg(self.image.to_string());
        ForegroundCommand::new(NativeProgram::Docker, CommandPurpose::Pull, command)
    }
}

/// A validated explicit containerd endpoint and namespace.
///
/// The raw values are retained only after [`ContainerdAdapter`] has validated
/// them. They are needed for the actual `ctr` invocation because the adapter's
/// public endpoint getter intentionally returns a path-redacted diagnostic
/// value. This type has no raw `Debug` or serialization surface.
#[derive(Clone, PartialEq, Eq)]
struct ContainerdSelectors {
    address: String,
    namespace: String,
}

impl ContainerdSelectors {
    fn new(
        address: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Result<Self, ContainerdParseError> {
        let address = address.into();
        let namespace = namespace.into();
        // Reject option-like values before they can become process arguments.
        // ContainerdAdapter remains the single source of truth for the full
        // endpoint and namespace grammar.
        if address.starts_with('-') {
            return Err(ContainerdParseError::InvalidEndpoint);
        }
        let endpoint =
            reqwest::Url::parse(&address).map_err(|_| ContainerdParseError::InvalidEndpoint)?;
        let supported_scheme = match endpoint.scheme() {
            "tcp" => true,
            #[cfg(not(windows))]
            "unix" => true,
            #[cfg(windows)]
            "npipe" => true,
            _ => false,
        };
        if !supported_scheme {
            return Err(ContainerdParseError::InvalidEndpoint);
        }
        ContainerdAdapter::new(address.clone(), namespace.clone())?;
        Ok(Self { address, namespace })
    }
}

impl std::fmt::Debug for ContainerdSelectors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerdSelectors")
            .field("address", &"[redacted]")
            .field("namespace", &"[redacted]")
            .finish()
    }
}

/// A containerd image pull bound to an explicit validated daemon and
/// namespace.
#[derive(Clone)]
pub struct ContainerdPull {
    selectors: ContainerdSelectors,
    image: ImageReference,
    platform: Option<OciPlatform>,
}

impl std::fmt::Debug for ContainerdPull {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerdPull")
            .field("selectors", &self.selectors)
            .field("image", &"[redacted]")
            .field("has_platform", &self.platform.is_some())
            .finish()
    }
}

impl ContainerdPull {
    pub fn new(
        address: impl Into<String>,
        namespace: impl Into<String>,
        image: ImageReference,
    ) -> Result<Self, ContainerdParseError> {
        Ok(Self {
            selectors: ContainerdSelectors::new(address, namespace)?,
            image,
            platform: None,
        })
    }

    pub fn with_platform(mut self, platform: OciPlatform) -> Self {
        self.platform = Some(platform);
        self
    }

    /// Consume this request and build one direct foreground operation.
    pub fn into_command(self) -> ForegroundCommand {
        let mut command = CommandSpec::new("ctr").args([
            "--address",
            self.selectors.address.as_str(),
            "--namespace",
            self.selectors.namespace.as_str(),
            "images",
            "pull",
        ]);
        if let Some(platform) = self.platform {
            command = command.args(["--platform".to_owned(), platform.to_string()]);
        }
        command = command.arg(self.image.to_string());
        ForegroundCommand::new(NativeProgram::Ctr, CommandPurpose::Pull, command)
    }
}

/// Version of the stable native-prune preview contract.
pub const NATIVE_PRUNE_PREVIEW_SCHEMA_VERSION: u32 = 2;

/// The exact native state category a prune plan is allowed to affect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativePruneScope {
    /// Only dangling Docker Engine images. This never expands to all images or
    /// to containers, volumes, networks, or system-wide state.
    DanglingImages,
    /// Unused cache owned by one selected BuildKit builder.
    BuildCache,
}

/// Typed warning attached to every supported native prune preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativePruneWarning {
    MayRemoveStateCreatedOutsideOsdk,
}

/// The exact native target bound into a prune preview and confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NativePruneTarget {
    /// The Docker daemon selected by an explicit CLI context name. The
    /// fingerprint binds the complete discovered endpoint without exposing it.
    DockerContext {
        name: String,
        endpoint_fingerprint: Fingerprint,
    },
    /// One explicitly named Buildx builder. The fingerprint binds its driver
    /// and complete node endpoint topology without exposing raw endpoints.
    BuildxBuilder {
        name: String,
        topology_fingerprint: Fingerprint,
    },
}

/// A non-executable description of a narrowly scoped native prune.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrunePreview {
    pub schema_version: u32,
    pub preview_id: Fingerprint,
    pub owner: NativeCacheOwner,
    pub scope: NativePruneScope,
    pub target: NativePruneTarget,
    pub warning: NativePruneWarning,
}

/// An explicit caller acknowledgement bound to one exact preview identity.
///
/// The CLI should create this only after displaying and confirming the preview.
/// Core still independently rebuilds the preview and rejects a stale or
/// mismatched identity before it constructs a mutating command.
#[derive(Debug, PartialEq, Eq)]
pub struct PruneConfirmation {
    accepted_preview_id: Fingerprint,
}

impl PruneConfirmation {
    /// Accept the exact immutable identity of a preview already available to
    /// the caller. Core cannot prove it was displayed, but it does require a
    /// real preview value rather than a free-standing boolean.
    pub fn accept(preview: &PrunePreview) -> Result<Self, PrunePlanError> {
        if preview.semantic_id()? != preview.preview_id {
            return Err(PrunePlanError::PreviewNotAccepted);
        }
        Ok(Self {
            accepted_preview_id: preview.preview_id.clone(),
        })
    }

    pub fn accepted_preview_id(&self) -> &Fingerprint {
        &self.accepted_preview_id
    }
}

impl PrunePreview {
    fn semantic_id(&self) -> Result<Fingerprint, PrunePlanError> {
        Fingerprint::for_canonical(&UnsignedPrunePreview {
            schema_version: self.schema_version,
            owner: self.owner,
            scope: self.scope,
            target: &self.target,
            warning: self.warning,
        })
        .map_err(|_| PrunePlanError::PreviewIdentity)
    }
}

/// Why a native owner has no executable prune plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneUnsupportedReason {
    NoStableAggregateContainerdPrune,
    NoImmutableBuildxExecutionTarget,
}

/// Typed, non-executable unsupported result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UnsupportedPrune {
    pub owner: NativeCacheOwner,
    pub reason: PruneUnsupportedReason,
}

/// A structural prune-plan error that never retains native selectors or
/// command output.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrunePlanError {
    #[error("invalid Docker context selector")]
    InvalidDockerContext,
    #[error("Buildx prune requires an explicit validated builder")]
    ExplicitBuildxBuilderRequired,
    #[error("invalid Buildx builder selector")]
    InvalidBuildxBuilder,
    #[error("native prune target identity is unavailable")]
    TargetIdentityUnavailable,
    #[error("Docker prune requires a direct local unix endpoint without context TLS material")]
    UnsupportedDockerEndpoint,
    #[error("Buildx prune execution is unsupported because builder names are mutable")]
    BuildxExecutionUnsupported,
    #[error("prune preview identity could not be generated")]
    PreviewIdentity,
    #[error("accepted prune preview does not match the current request")]
    PreviewNotAccepted,
}

#[derive(Serialize)]
struct UnsignedPrunePreview<'a> {
    schema_version: u32,
    owner: NativeCacheOwner,
    scope: NativePruneScope,
    target: &'a NativePruneTarget,
    warning: NativePruneWarning,
}

fn prune_preview(
    owner: NativeCacheOwner,
    scope: NativePruneScope,
    target: NativePruneTarget,
) -> Result<PrunePreview, PrunePlanError> {
    let unsigned = UnsignedPrunePreview {
        schema_version: NATIVE_PRUNE_PREVIEW_SCHEMA_VERSION,
        owner,
        scope,
        target: &target,
        warning: NativePruneWarning::MayRemoveStateCreatedOutsideOsdk,
    };
    let preview_id =
        Fingerprint::for_canonical(&unsigned).map_err(|_| PrunePlanError::PreviewIdentity)?;
    Ok(PrunePreview {
        schema_version: NATIVE_PRUNE_PREVIEW_SCHEMA_VERSION,
        preview_id,
        owner,
        scope,
        target,
        warning: NativePruneWarning::MayRemoveStateCreatedOutsideOsdk,
    })
}

fn validate_confirmation(
    preview: &PrunePreview,
    confirmation: &PruneConfirmation,
) -> Result<(), PrunePlanError> {
    if preview.semantic_id()? != preview.preview_id
        || confirmation.accepted_preview_id() != &preview.preview_id
    {
        return Err(PrunePlanError::PreviewNotAccepted);
    }
    Ok(())
}

fn is_safe_target_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Narrow Docker Engine image pruning.
///
/// Execution delegates only to `docker image prune --force`, whose default
/// scope is dangling images. There is intentionally no `--all`, `system
/// prune`, or access to containers, volumes, or networks.
#[derive(Clone)]
pub struct DockerImagePrune {
    context: String,
    endpoint: String,
    endpoint_fingerprint: Fingerprint,
}

impl std::fmt::Debug for DockerImagePrune {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerImagePrune")
            .field("context", &"explicit")
            .field("endpoint", &"[redacted]")
            .finish()
    }
}

impl DockerImagePrune {
    /// Construct an executable prune from one discovered Docker context. The
    /// raw endpoint stays private and only local socket transports without
    /// context-held TLS behavior are accepted.
    pub fn from_context(context: DockerContext) -> Result<Self, PrunePlanError> {
        let parts = context
            .into_prune_parts()
            .ok_or(PrunePlanError::TargetIdentityUnavailable)?;
        Self::new(
            parts.name,
            parts.raw_endpoint,
            parts.transport,
            parts.scope,
            parts.skip_tls_verify,
            parts.has_tls_material,
        )
    }

    fn new(
        context: impl Into<String>,
        endpoint: impl Into<String>,
        transport: EndpointTransport,
        scope: EndpointScope,
        skip_tls_verify: bool,
        has_tls_material: bool,
    ) -> Result<Self, PrunePlanError> {
        let context = context.into();
        if !is_safe_target_name(&context) {
            return Err(PrunePlanError::InvalidDockerContext);
        }
        if scope != EndpointScope::Local
            || skip_tls_verify
            || has_tls_material
            || !matches!(
                transport,
                EndpointTransport::LocalSocket | EndpointTransport::NamedPipe
            )
        {
            return Err(PrunePlanError::UnsupportedDockerEndpoint);
        }
        let endpoint = endpoint.into();
        let parsed = reqwest::Url::parse(&endpoint)
            .map_err(|_| PrunePlanError::UnsupportedDockerEndpoint)?;
        let valid_scheme = match transport {
            EndpointTransport::LocalSocket => {
                parsed.scheme() == "unix"
                    && parsed.host_str().is_none()
                    && parsed.path().starts_with('/')
                    && parsed.path() != "/"
            }
            EndpointTransport::NamedPipe => is_canonical_local_named_pipe(&endpoint),
            _ => false,
        };
        if !valid_scheme
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(PrunePlanError::UnsupportedDockerEndpoint);
        }
        let mut identity = b"osdk-docker-prune-endpoint-v1\0".to_vec();
        identity.extend_from_slice(endpoint.as_bytes());
        Ok(Self {
            context,
            endpoint,
            endpoint_fingerprint: Fingerprint::for_bytes(&identity),
        })
    }

    pub fn preview(&self) -> Result<PrunePreview, PrunePlanError> {
        prune_preview(
            NativeCacheOwner::DockerEngine,
            NativePruneScope::DanglingImages,
            NativePruneTarget::DockerContext {
                name: self.context.clone(),
                endpoint_fingerprint: self.endpoint_fingerprint.clone(),
            },
        )
    }

    pub fn execute(
        &self,
        confirmation: PruneConfirmation,
    ) -> Result<ForegroundCommand, PrunePlanError> {
        let preview = self.preview()?;
        validate_confirmation(&preview, &confirmation)?;
        Ok(ForegroundCommand::new(
            NativeProgram::Docker,
            CommandPurpose::Prune,
            CommandSpec::new("docker").args([
                "--host",
                self.endpoint.as_str(),
                "image",
                "prune",
                "--force",
            ]),
        ))
    }
}

fn is_canonical_local_named_pipe(endpoint: &str) -> bool {
    let Some(name) = endpoint.strip_prefix("npipe:////./pipe/") else {
        return false;
    };
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Narrow BuildKit cache pruning through a validated Buildx selector.
#[derive(Clone)]
pub struct BuildxPrune {
    selector: BuildxBuilderSelector,
    topology_fingerprint: Fingerprint,
}

impl std::fmt::Debug for BuildxPrune {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuildxPrune")
            .field(
                "selector",
                &self.selector.as_name().map(|_| "named").unwrap_or("auto"),
            )
            .finish()
    }
}

impl BuildxPrune {
    pub fn new(
        selector: BuildxBuilderSelector,
        topology_fingerprint: Fingerprint,
    ) -> Result<Self, PrunePlanError> {
        let name = selector
            .as_name()
            .ok_or(PrunePlanError::ExplicitBuildxBuilderRequired)?;
        // `Named(String)` is a public compatibility variant, so reconstruct it
        // through the validating constructor rather than trusting provenance.
        let selector = BuildxBuilderSelector::named(name.to_owned())
            .map_err(|_| PrunePlanError::InvalidBuildxBuilder)?;
        Ok(Self {
            selector,
            topology_fingerprint,
        })
    }

    pub fn selector(&self) -> &BuildxBuilderSelector {
        &self.selector
    }

    pub fn preview(&self) -> Result<PrunePreview, PrunePlanError> {
        prune_preview(
            NativeCacheOwner::BuildkitBuilder,
            NativePruneScope::BuildCache,
            NativePruneTarget::BuildxBuilder {
                name: self
                    .selector
                    .as_name()
                    .expect("BuildxPrune always has an explicit builder")
                    .to_owned(),
                topology_fingerprint: self.topology_fingerprint.clone(),
            },
        )
    }

    pub fn execute(
        &self,
        _confirmation: PruneConfirmation,
    ) -> Result<ForegroundCommand, PrunePlanError> {
        Err(PrunePlanError::BuildxExecutionUnsupported)
    }

    pub fn unsupported(&self) -> UnsupportedPrune {
        UnsupportedPrune {
            owner: NativeCacheOwner::BuildkitBuilder,
            reason: PruneUnsupportedReason::NoImmutableBuildxExecutionTarget,
        }
    }
}

/// Explicitly unsupported containerd aggregate pruning.
///
/// containerd cache ownership is split across namespaces, content, snapshots,
/// images, and leases. This builder never guesses an aggregate command and
/// never scans containerd's private storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContainerdPrune;

impl ContainerdPrune {
    pub fn preview(&self) -> UnsupportedPrune {
        UnsupportedPrune {
            owner: NativeCacheOwner::Containerd,
            reason: PruneUnsupportedReason::NoStableAggregateContainerdPrune,
        }
    }

    pub fn execute(&self, _confirmation: PruneConfirmation) -> UnsupportedPrune {
        self.preview()
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    use super::*;
    use crate::process::{CaptureLimits, CommandOutcome, CommandRunner};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        program: String,
        arguments: Vec<String>,
        environment_count: usize,
        has_working_directory: bool,
    }

    struct FakeRunner {
        status: ExitStatus,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        fn with_status(status: ExitStatus) -> Self {
            Self {
                status,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run_captured(&self, _command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            panic!("native operations must not capture or probe")
        }

        fn run_foreground(&self, command: &CommandSpec) -> io::Result<ExitStatus> {
            self.calls.lock().unwrap().push(Call {
                program: command.program().to_string_lossy().into_owned(),
                arguments: command
                    .arguments()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                environment_count: command.environment().len(),
                has_working_directory: command.working_directory().is_some(),
            });
            Ok(self.status)
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

    fn image(value: &str) -> ImageReference {
        ImageReference::parse(value).unwrap()
    }

    fn platform(value: &str) -> OciPlatform {
        OciPlatform::parse(value).unwrap()
    }

    fn identity(value: &str) -> Fingerprint {
        Fingerprint::for_bytes(value.as_bytes())
    }

    fn local_docker_prune(context: &str, endpoint: &str) -> DockerImagePrune {
        DockerImagePrune::new(
            context,
            endpoint,
            if endpoint.starts_with("npipe:") {
                EndpointTransport::NamedPipe
            } else {
                EndpointTransport::LocalSocket
            },
            EndpointScope::Local,
            false,
            false,
        )
        .unwrap()
    }

    #[cfg(not(windows))]
    fn explicit_containerd_address() -> &'static str {
        "unix:///run/private/containerd.sock"
    }

    #[cfg(windows)]
    fn explicit_containerd_address() -> &'static str {
        "npipe:////./pipe/private-containerd"
    }

    fn assert_direct(call: &Call) {
        assert!(call.environment_count == 0);
        assert!(!call.has_working_directory);
        assert!(!matches!(call.program.as_str(), "sh" | "bash" | "cmd"));
    }

    #[test]
    fn docker_pull_uses_canonical_image_and_platform_in_one_foreground_call() {
        let operation = DockerPull::new(image("ubuntu:24.04"))
            .with_platform(platform("Linux/X64"))
            .into_command();
        let evidence = serde_json::to_string(operation.evidence()).unwrap();
        for secret in ["ubuntu", "24.04", "linux/amd64"] {
            assert!(!evidence.contains(secret), "leaked {secret}: {evidence}");
        }
        assert!(evidence.contains("\"program\":\"docker\""));
        assert!(evidence.contains("\"purpose\":\"pull\""));
        assert_eq!(operation.evidence().argument_count(), 5);

        let runner = FakeRunner::with_status(exit_status(0));
        assert!(operation.execute(&runner).unwrap().success());

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "docker");
        assert_eq!(
            calls[0].arguments,
            [
                "image",
                "pull",
                "--platform",
                "linux/amd64",
                "docker.io/library/ubuntu:24.04",
            ]
        );
        assert_direct(&calls[0]);
    }

    #[test]
    fn docker_pull_without_platform_has_no_platform_flag() {
        let runner = FakeRunner::with_status(exit_status(0));
        DockerPull::new(image("GHCR.IO/example/tool:v1"))
            .into_command()
            .execute(&runner)
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments,
            ["image", "pull", "ghcr.io/example/tool:v1"]
        );
        assert!(!calls[0]
            .arguments
            .iter()
            .any(|argument| argument == "--platform"));
    }

    #[test]
    fn docker_pull_does_not_inherit_prune_context_selection() {
        let runner = FakeRunner::with_status(exit_status(0));
        DockerPull::new(image("alpine:3"))
            .into_command()
            .execute(&runner)
            .unwrap();
        assert!(!runner.calls()[0]
            .arguments
            .iter()
            .any(|argument| argument == "--context"));
    }

    #[test]
    fn containerd_pull_keeps_validated_raw_selectors_only_in_execution() {
        let operation = ContainerdPull::new(
            explicit_containerd_address(),
            "k8s.io",
            image("registry.example/team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .unwrap()
        .with_platform(platform("linux/arm64/v8"))
        .into_command();

        let evidence = serde_json::to_string(operation.evidence()).unwrap();
        for secret in ["private", "k8s.io", "registry.example", "linux/arm64/v8"] {
            assert!(!evidence.contains(secret), "leaked {secret}: {evidence}");
        }
        assert!(evidence.contains("\"program\":\"ctr\""));
        assert_eq!(operation.evidence().argument_count(), 9);

        let runner = FakeRunner::with_status(exit_status(0));
        operation.execute(&runner).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "ctr");
        assert_eq!(calls[0].arguments[0], "--address");
        assert_eq!(calls[0].arguments[1], explicit_containerd_address());
        assert_eq!(
            &calls[0].arguments[2..],
            [
                "--namespace",
                "k8s.io",
                "images",
                "pull",
                "--platform",
                "linux/arm64/v8",
                "registry.example/team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ]
        );
        assert_direct(&calls[0]);
    }

    #[test]
    fn containerd_rejects_option_like_or_credential_bearing_selectors() {
        let rejected_address =
            ContainerdPull::new("--address=unix:///evil", "default", image("alpine:3"))
                .unwrap_err();
        assert!(matches!(
            rejected_address,
            ContainerdParseError::InvalidEndpoint
        ));
        let rejected_namespace = ContainerdPull::new(
            explicit_containerd_address(),
            "--namespace",
            image("alpine:3"),
        )
        .unwrap_err();
        assert!(matches!(
            rejected_namespace,
            ContainerdParseError::InvalidNamespace
        ));
        let credential_error = ContainerdPull::new(
            "tcp://user:top-secret@example.test:1234",
            "default",
            image("alpine:3"),
        )
        .unwrap_err();
        assert!(matches!(
            credential_error,
            ContainerdParseError::UnsafeEndpoint
        ));
        assert!(!credential_error.to_string().contains("top-secret"));
        for unsupported in [
            "http://127.0.0.1:1234",
            "https://containerd.example.test",
            "ssh://containerd.example.test",
        ] {
            assert!(matches!(
                ContainerdPull::new(unsupported, "default", image("alpine:3")),
                Err(ContainerdParseError::InvalidEndpoint)
            ));
        }

        #[cfg(not(windows))]
        assert!(matches!(
            ContainerdPull::new(
                "npipe:////./pipe/containerd-containerd",
                "default",
                image("alpine:3")
            ),
            Err(ContainerdParseError::InvalidEndpoint)
        ));
        #[cfg(windows)]
        assert!(matches!(
            ContainerdPull::new(
                "unix:///run/containerd/containerd.sock",
                "default",
                image("alpine:3")
            ),
            Err(ContainerdParseError::InvalidEndpoint)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn containerd_windows_named_pipe_is_retained_in_execution_only() {
        let operation = ContainerdPull::new(
            "npipe:////./pipe/private-containerd",
            "default",
            image("alpine:3"),
        )
        .unwrap()
        .into_command();
        let evidence = serde_json::to_string(operation.evidence()).unwrap();
        assert!(!evidence.contains("private-containerd"));

        let runner = FakeRunner::with_status(exit_status(0));
        operation.execute(&runner).unwrap();
        assert_eq!(
            runner.calls()[0].arguments,
            [
                "--address",
                "npipe:////./pipe/private-containerd",
                "--namespace",
                "default",
                "images",
                "pull",
                "docker.io/library/alpine:3",
            ]
        );
    }

    #[test]
    fn a_nonzero_pull_exit_is_returned_without_retry() {
        let runner = FakeRunner::with_status(exit_status(23));
        let status = DockerPull::new(image("alpine:3"))
            .into_command()
            .execute(&runner)
            .unwrap();

        assert_eq!(status.code(), Some(23));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn docker_prune_preview_is_target_bound_and_contains_no_cache_totals() {
        let prune = local_docker_prune("desktop-linux", "unix:///run/docker.sock");
        let preview = prune.preview().unwrap();
        let endpoint_fingerprint = match &preview.target {
            NativePruneTarget::DockerContext {
                endpoint_fingerprint,
                ..
            } => endpoint_fingerprint.clone(),
            _ => unreachable!(),
        };

        assert_eq!(preview.schema_version, NATIVE_PRUNE_PREVIEW_SCHEMA_VERSION);
        assert_eq!(preview.owner, NativeCacheOwner::DockerEngine);
        assert_eq!(preview.scope, NativePruneScope::DanglingImages);
        assert_eq!(
            preview.target,
            NativePruneTarget::DockerContext {
                name: "desktop-linux".to_owned(),
                endpoint_fingerprint,
            }
        );
        assert_eq!(
            preview.warning,
            NativePruneWarning::MayRemoveStateCreatedOutsideOsdk
        );
        let serialized = serde_json::to_string(&preview).unwrap();
        assert!(serialized.contains("desktop-linux"));
        for excluded in ["cache", "total", "reclaimable", "containers", "volumes"] {
            assert!(
                !serialized.contains(excluded),
                "leaked {excluded}: {serialized}"
            );
        }
    }

    #[test]
    fn docker_image_prune_requires_matching_preview_and_executes_exactly_once() {
        let prune = local_docker_prune("desktop-linux", "unix:///run/docker.sock");
        let preview = prune.preview().unwrap();
        let confirmation = PruneConfirmation::accept(&preview).unwrap();
        let operation = prune.execute(confirmation).unwrap();
        let evidence = serde_json::to_string(operation.evidence()).unwrap();
        assert!(evidence.contains("\"purpose\":\"prune\""));
        assert!(!evidence.contains("--force"));
        assert!(!evidence.contains("desktop-linux"));

        let runner = FakeRunner::with_status(exit_status(19));
        let status = operation.execute(&runner).unwrap();
        assert_eq!(status.code(), Some(19));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "docker");
        assert_eq!(
            calls[0].arguments,
            [
                "--host",
                "unix:///run/docker.sock",
                "image",
                "prune",
                "--force",
            ]
        );
        assert_direct(&calls[0]);
        for forbidden in [
            "--context",
            "system",
            "--all",
            "-a",
            "volumes",
            "containers",
            "networks",
        ] {
            assert!(!calls[0]
                .arguments
                .iter()
                .any(|argument| argument == forbidden));
        }
    }

    #[test]
    fn docker_prune_rejects_unsafe_contexts_and_mutated_previews() {
        for invalid in ["", "--context", "team/context", "context with space"] {
            assert_eq!(
                DockerImagePrune::new(
                    invalid,
                    "unix:///run/docker.sock",
                    EndpointTransport::LocalSocket,
                    EndpointScope::Local,
                    false,
                    false,
                )
                .unwrap_err(),
                PrunePlanError::InvalidDockerContext
            );
        }

        let prune = local_docker_prune("desktop-linux", "unix:///run/docker.sock");
        let mut preview = prune.preview().unwrap();
        preview.scope = NativePruneScope::BuildCache;
        assert_eq!(
            PruneConfirmation::accept(&preview).unwrap_err(),
            PrunePlanError::PreviewNotAccepted
        );
    }

    #[test]
    fn docker_prune_rejects_endpoints_that_cannot_be_pinned_without_context_state() {
        for (endpoint, transport, scope, skip_tls, has_tls) in [
            (
                "ssh://builder.example",
                EndpointTransport::Ssh,
                EndpointScope::Remote,
                false,
                false,
            ),
            (
                "tcp://127.0.0.1:2375",
                EndpointTransport::Tcp,
                EndpointScope::Local,
                false,
                false,
            ),
            (
                "npipe:////server/pipe/docker_engine",
                EndpointTransport::NamedPipe,
                EndpointScope::Local,
                false,
                false,
            ),
            (
                "npipe:////./pipe/docker/engine",
                EndpointTransport::NamedPipe,
                EndpointScope::Local,
                false,
                false,
            ),
            (
                "npipe:////./pipe/",
                EndpointTransport::NamedPipe,
                EndpointScope::Local,
                false,
                false,
            ),
            (
                "unix:///run/docker.sock",
                EndpointTransport::LocalSocket,
                EndpointScope::Local,
                true,
                false,
            ),
            (
                "unix:///run/docker.sock",
                EndpointTransport::LocalSocket,
                EndpointScope::Local,
                false,
                true,
            ),
        ] {
            assert_eq!(
                DockerImagePrune::new("context", endpoint, transport, scope, skip_tls, has_tls,)
                    .unwrap_err(),
                PrunePlanError::UnsupportedDockerEndpoint
            );
        }

        let local_pipe = DockerImagePrune::new(
            "default",
            "npipe:////./pipe/docker_engine",
            EndpointTransport::NamedPipe,
            EndpointScope::Local,
            false,
            false,
        )
        .unwrap();
        let operation = local_pipe
            .execute(PruneConfirmation::accept(&local_pipe.preview().unwrap()).unwrap())
            .unwrap();
        let runner = FakeRunner::with_status(exit_status(0));
        operation.execute(&runner).unwrap();
        assert_eq!(
            runner.calls()[0].arguments,
            [
                "--host",
                "npipe:////./pipe/docker_engine",
                "image",
                "prune",
                "--force",
            ]
        );
    }

    #[test]
    fn preview_id_is_deterministic_and_binds_target() {
        let prune = local_docker_prune("desktop-linux", "unix:///run/docker.sock");
        let first = prune.preview().unwrap();
        let second = prune.preview().unwrap();
        assert_eq!(first.preview_id, second.preview_id);
        assert_ne!(
            first.preview_id,
            local_docker_prune("another-context", "unix:///run/docker.sock")
                .preview()
                .unwrap()
                .preview_id
        );
        assert_ne!(
            first.preview_id,
            local_docker_prune("desktop-linux", "unix:///run/other.sock")
                .preview()
                .unwrap()
                .preview_id
        );
    }

    #[test]
    fn buildx_prune_uses_only_the_validated_selected_builder() {
        let selector = BuildxBuilderSelector::named("team.private-builder").unwrap();
        let topology_fingerprint = identity("builder-topology-a");
        let prune = BuildxPrune::new(selector.clone(), topology_fingerprint.clone()).unwrap();
        assert_eq!(prune.selector(), &selector);
        let preview = prune.preview().unwrap();
        assert_eq!(
            preview.target,
            NativePruneTarget::BuildxBuilder {
                name: "team.private-builder".to_owned(),
                topology_fingerprint,
            }
        );
        assert_eq!(
            prune
                .execute(PruneConfirmation::accept(&preview).unwrap())
                .unwrap_err(),
            PrunePlanError::BuildxExecutionUnsupported
        );
    }

    #[test]
    fn buildx_prune_revalidates_and_requires_an_explicit_builder() {
        assert_eq!(
            BuildxPrune::new(BuildxBuilderSelector::Auto, identity("topology")).unwrap_err(),
            PrunePlanError::ExplicitBuildxBuilderRequired
        );
        assert_eq!(
            BuildxPrune::new(
                BuildxBuilderSelector::Named("--all".to_owned()),
                identity("topology")
            )
            .unwrap_err(),
            PrunePlanError::InvalidBuildxBuilder
        );
        assert_eq!(
            BuildxPrune::new(
                BuildxBuilderSelector::Named(String::new()),
                identity("topology")
            )
            .unwrap_err(),
            PrunePlanError::InvalidBuildxBuilder
        );
    }

    #[test]
    fn docker_prune_confirmation_rejects_changed_target() {
        let first = local_docker_prune("first", "unix:///run/docker.sock");
        let preview = first.preview().unwrap();
        let confirmation = PruneConfirmation::accept(&preview).unwrap();
        let second = local_docker_prune("second", "unix:///run/docker.sock");
        assert!(matches!(
            second.execute(confirmation),
            Err(PrunePlanError::PreviewNotAccepted)
        ));

        let first = local_docker_prune("same", "unix:///run/first.sock");
        let confirmation = PruneConfirmation::accept(&first.preview().unwrap()).unwrap();
        let retargeted = local_docker_prune("same", "unix:///run/second.sock");
        assert!(matches!(
            retargeted.execute(confirmation),
            Err(PrunePlanError::PreviewNotAccepted)
        ));
    }

    #[test]
    fn containerd_prune_is_always_explicitly_unsupported() {
        let unsupported = ContainerdPrune.preview();
        assert_eq!(unsupported.owner, NativeCacheOwner::Containerd);
        assert_eq!(
            unsupported.reason,
            PruneUnsupportedReason::NoStableAggregateContainerdPrune
        );
    }
}
