//! Read-only semantic mirror planners for native container control planes.
//!
//! The planners consume already validated osdk policy plus typed discovery.
//! They return versioned fingerprints and in-memory candidate bytes, but never
//! write, restart, recreate, or elevate a native runtime.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{Array, DocumentMut, Item, Table};

use super::buildkit::{BuilderDriver, BuildkitDiscovery};
use super::containerd::ContainerdDiscovery;
use super::docker::{DockerContextKind, DockerDiscovery};
use super::plan::{
    policy_fingerprint, ActivationRequirement, BuildkitTargetDriver, DockerTargetKind,
    EffectiveResolution, Fingerprint, MirrorChange, MirrorPlanBundle, MirrorPlanDraft,
    MirrorPlanTarget, NativeConfigCandidate, NativeConfigFormat, NativeConfigSnapshot,
    PlanApplicability, PlanError, PlanWarning, PlannedCapability, PlannedMirrorEndpoint,
    RequiredPrivilege, ValidationStep,
};
use super::reference::RegistryName;
use crate::config::{ContainerRegistryConfig, ContainerResolve};

pub struct DockerMirrorPlanRequest<'a> {
    pub registry: &'a RegistryName,
    pub policy: &'a ContainerRegistryConfig,
    pub discovery: &'a DockerDiscovery,
    /// Exact explicitly selected daemon JSON, when local and unambiguous.
    pub native_config: Option<&'a NativeConfigSnapshot>,
}

pub struct ContainerdMirrorPlanRequest<'a> {
    pub registry: &'a RegistryName,
    pub policy: &'a ContainerRegistryConfig,
    pub discovery: &'a ContainerdDiscovery,
    /// Exact `<config_path>/<registry>/hosts.toml` target.
    pub hosts_config: Option<&'a NativeConfigSnapshot>,
    /// Explicit main containerd config, required only when `config_path` is absent.
    pub main_config: Option<&'a NativeConfigSnapshot>,
}

pub struct BuildkitMirrorPlanRequest<'a> {
    pub registry: &'a RegistryName,
    pub policy: &'a ContainerRegistryConfig,
    pub discovery: &'a BuildkitDiscovery,
    /// Explicit local `buildkitd.toml` associated with the selected builder.
    pub native_config: Option<&'a NativeConfigSnapshot>,
}

pub fn plan_docker_mirrors(
    request: DockerMirrorPlanRequest<'_>,
) -> Result<MirrorPlanBundle, MirrorPlanError> {
    let policy_fingerprint = policy_fingerprint(request.registry, request.policy)?;
    let target = docker_target(request.discovery)?;
    let mut warnings = BTreeSet::new();
    let mut applicability = docker_applicability(request.discovery, &mut warnings);
    let mut changes = Vec::new();
    let mut candidates = Vec::new();
    let mut inputs = Vec::new();

    if request.policy.anonymous_only {
        warnings.insert(PlanWarning::AnonymousOnlyNotEnforced);
    }
    if !request.registry.is_docker_hub() {
        applicability = PlanApplicability::Unsupported;
        warnings.insert(PlanWarning::DockerHubOnly);
    } else {
        let mirrors = validated_mirrors(request.policy)?;
        let effective_resolution = match request.policy.resolve {
            ContainerResolve::Mirror => EffectiveResolution::Mirror,
            ContainerResolve::Upstream => {
                warnings.insert(PlanWarning::ResolutionSeparationUnavailable);
                if applicability == PlanApplicability::Ready {
                    applicability = PlanApplicability::ManualOnly;
                }
                EffectiveResolution::RuntimeDefined
            }
        };
        // Moby accepts only origin URLs for `registry-mirrors`. Preserve a
        // configured path by refusing to render it instead of silently
        // truncating the destination. Other native runtimes can express it.
        if mirrors.iter().any(|mirror| mirror_has_path(mirror)) {
            return Err(MirrorPlanError::UnsupportedDockerMirrorPath);
        }
        changes.push(MirrorChange::DockerHubMirrors {
            mirrors: planned_mirrors(&mirrors)?,
            effective_resolution,
        });

        if applicability != PlanApplicability::Unsupported {
            if let Some(snapshot) = request.native_config {
                inputs.push(snapshot.fingerprint().clone());
                if !matches!(
                    request
                        .discovery
                        .context
                        .as_ref()
                        .map(|context| context.kind),
                    Some(DockerContextKind::Remote | DockerContextKind::Desktop)
                ) {
                    candidates.push(docker_candidate(snapshot, request.policy)?);
                }
            } else {
                warnings.insert(PlanWarning::NativeConfigPathRequired);
                if applicability == PlanApplicability::Ready {
                    applicability = PlanApplicability::ManualOnly;
                }
            }
        }
    }

    warnings.insert(PlanWarning::DaemonRestartRequired);
    let candidate_fingerprints = candidates
        .iter()
        .map(|candidate| candidate.fingerprint().clone())
        .collect();
    let plan = MirrorPlanDraft {
        target,
        applicability,
        policy_fingerprint,
        inputs,
        candidates: candidate_fingerprints,
        changes,
        privilege: docker_privilege(request.discovery),
        activation: ActivationRequirement::RestartDaemon,
        validation: BTreeSet::from([
            ValidationStep::ParseCompleteJson,
            ValidationStep::ValidateDockerDaemonConfig,
            ValidationStep::RediscoverRuntimeIdentity,
            ValidationStep::CompareInputFingerprints,
        ]),
        warnings,
    }
    .finalize()?;
    Ok(MirrorPlanBundle { plan, candidates })
}

pub fn plan_containerd_mirrors(
    request: ContainerdMirrorPlanRequest<'_>,
) -> Result<MirrorPlanBundle, MirrorPlanError> {
    let policy_fingerprint = policy_fingerprint(request.registry, request.policy)?;
    let target = containerd_target(request.discovery);
    let mirrors = validated_mirrors(request.policy)?;
    let capabilities = match request.policy.resolve {
        ContainerResolve::Upstream => BTreeSet::from([PlannedCapability::Pull]),
        ContainerResolve::Mirror => {
            BTreeSet::from([PlannedCapability::Pull, PlannedCapability::Resolve])
        }
    };
    let mut changes = vec![MirrorChange::ContainerdRegistryHosts {
        registry: request.registry.clone(),
        mirrors: planned_mirrors(&mirrors)?,
        capabilities: capabilities.clone(),
    }];
    let mut warnings = BTreeSet::from([PlanWarning::ExistingNativeEntriesPreserved]);
    if request.policy.anonymous_only {
        warnings.insert(PlanWarning::AnonymousOnlyNotEnforced);
    }
    let mut applicability =
        if request.discovery.report.status == super::report::DiagnosticStatus::Healthy {
            PlanApplicability::Ready
        } else {
            PlanApplicability::ManualOnly
        };
    let (_, endpoint_scope) = super::classify_endpoint(request.discovery.address.as_str());
    if endpoint_scope == super::report::EndpointScope::Remote {
        warnings.insert(PlanWarning::RemoteTarget);
        applicability = PlanApplicability::ManualOnly;
    } else if endpoint_scope != super::report::EndpointScope::Local {
        applicability = PlanApplicability::ManualOnly;
    }
    let mut inputs = Vec::new();
    let mut candidates = Vec::new();
    let desired_config_path = request
        .hosts_config
        .map(hosts_root_from_snapshot)
        .transpose()?;

    if let (Some(discovered), Some(desired)) = (
        request
            .discovery
            .config
            .as_ref()
            .and_then(|config| config.registry_config_path.as_ref()),
        desired_config_path.as_ref(),
    ) {
        if !paths_equivalent(discovered, desired) {
            return Err(MirrorPlanError::ConfigPathMismatch);
        }
    }

    if let Some(snapshot) = request.hosts_config {
        inputs.push(snapshot.fingerprint().clone());
        candidates.push(containerd_hosts_candidate(
            snapshot,
            request.registry,
            &mirrors,
            &capabilities,
        )?);
    } else {
        warnings.insert(PlanWarning::NativeConfigPathRequired);
        applicability = PlanApplicability::ManualOnly;
    }

    let missing_config_path = request
        .discovery
        .config
        .as_ref()
        .and_then(|config| config.registry_config_path.as_ref())
        .is_none();
    let activation = if missing_config_path {
        warnings.insert(PlanWarning::ContainerdConfigPathMissing);
        warnings.insert(PlanWarning::DaemonRestartRequired);
        applicability = PlanApplicability::ManualOnly;
        if let Some(path) = desired_config_path.as_ref() {
            changes.push(MirrorChange::ContainerdConfigPath {
                path: path_string(path)?,
            });
            if let Some(main) = request.main_config {
                inputs.push(main.fingerprint().clone());
                if main.is_missing() {
                    warnings.insert(PlanWarning::NativeConfigPathRequired);
                } else {
                    candidates.push(containerd_main_candidate(main, request.discovery, path)?);
                }
            } else {
                warnings.insert(PlanWarning::NativeConfigPathRequired);
            }
        } else {
            warnings.insert(PlanWarning::NativeConfigPathRequired);
        }
        ActivationRequirement::RestartDaemon
    } else {
        ActivationRequirement::None
    };

    let candidate_fingerprints = candidates
        .iter()
        .map(|candidate| candidate.fingerprint().clone())
        .collect();
    let plan = MirrorPlanDraft {
        target,
        applicability,
        policy_fingerprint,
        inputs,
        candidates: candidate_fingerprints,
        changes,
        privilege: containerd_privilege(request.discovery),
        activation,
        validation: BTreeSet::from([
            ValidationStep::ParseCompleteToml,
            ValidationStep::RediscoverRuntimeIdentity,
            ValidationStep::CompareInputFingerprints,
        ]),
        warnings,
    }
    .finalize()?;
    Ok(MirrorPlanBundle { plan, candidates })
}

pub fn plan_buildkit_mirrors(
    request: BuildkitMirrorPlanRequest<'_>,
) -> Result<MirrorPlanBundle, MirrorPlanError> {
    let policy_fingerprint = policy_fingerprint(request.registry, request.policy)?;
    let target = buildkit_target(request.discovery)?;
    let mirrors = validated_mirrors(request.policy)?;
    let selected = request
        .discovery
        .selected_builder
        .as_ref()
        .ok_or(MirrorPlanError::MissingBuilder)?;
    let mut warnings = BTreeSet::new();
    if request.policy.anonymous_only {
        warnings.insert(PlanWarning::AnonymousOnlyNotEnforced);
    }
    let effective_resolution = match request.policy.resolve {
        ContainerResolve::Mirror => EffectiveResolution::Mirror,
        ContainerResolve::Upstream => {
            warnings.insert(PlanWarning::ResolutionSeparationUnavailable);
            EffectiveResolution::RuntimeDefined
        }
    };
    let mut changes = Vec::new();
    let mut inputs = Vec::new();
    let mut candidates = Vec::new();
    let mut activation = ActivationRequirement::None;
    let mut applicability = match selected.driver {
        BuilderDriver::Docker => {
            warnings.insert(PlanWarning::DockerDriverUsesEngineConfiguration);
            PlanApplicability::Unsupported
        }
        BuilderDriver::DockerContainer => {
            activation = ActivationRequirement::RecreateBuilder;
            warnings.insert(PlanWarning::BuilderRecreateRequired);
            if builder_is_local(selected) {
                PlanApplicability::Ready
            } else {
                warnings.insert(PlanWarning::ExternalBuilderConfiguration);
                PlanApplicability::ManualOnly
            }
        }
        BuilderDriver::Kubernetes
        | BuilderDriver::Remote
        | BuilderDriver::Cloud
        | BuilderDriver::Unknown => {
            warnings.insert(PlanWarning::ExternalBuilderConfiguration);
            PlanApplicability::ManualOnly
        }
    };
    if request.policy.resolve == ContainerResolve::Upstream
        && applicability == PlanApplicability::Ready
    {
        applicability = PlanApplicability::ManualOnly;
    }

    if !matches!(selected.driver, BuilderDriver::Docker) {
        changes.push(MirrorChange::BuildkitRegistryMirrors {
            registry: request.registry.clone(),
            mirrors: planned_mirrors(&mirrors)?,
            effective_resolution,
        });
    }
    if matches!(selected.driver, BuilderDriver::DockerContainer) {
        if let Some(snapshot) = request.native_config {
            inputs.push(snapshot.fingerprint().clone());
            if builder_is_local(selected) {
                candidates.push(buildkit_candidate(snapshot, request.registry, &mirrors)?);
            }
        } else {
            warnings.insert(PlanWarning::NativeConfigPathRequired);
            applicability = PlanApplicability::ManualOnly;
        }
    }

    let candidate_fingerprints = candidates
        .iter()
        .map(|candidate| candidate.fingerprint().clone())
        .collect();
    let plan = MirrorPlanDraft {
        target,
        applicability,
        policy_fingerprint,
        inputs,
        candidates: candidate_fingerprints,
        changes,
        privilege: buildkit_privilege(selected.driver.clone()),
        activation,
        validation: BTreeSet::from([
            ValidationStep::ParseCompleteToml,
            ValidationStep::RediscoverBuilderIdentity,
            ValidationStep::CompareInputFingerprints,
        ]),
        warnings,
    }
    .finalize()?;
    Ok(MirrorPlanBundle { plan, candidates })
}

fn docker_target(discovery: &DockerDiscovery) -> Result<MirrorPlanTarget, MirrorPlanError> {
    let context = discovery
        .context
        .as_ref()
        .and_then(|context| context.name.as_deref())
        .unwrap_or("unknown");
    let kind = match discovery.context.as_ref().map(|context| context.kind) {
        Some(DockerContextKind::Local) => DockerTargetKind::Local,
        Some(DockerContextKind::Rootless) => DockerTargetKind::Rootless,
        Some(DockerContextKind::Desktop) => DockerTargetKind::Desktop,
        Some(DockerContextKind::Remote) => DockerTargetKind::Remote,
        Some(DockerContextKind::Unknown) | None => DockerTargetKind::Unknown,
    };
    Ok(MirrorPlanTarget::Docker {
        context: Fingerprint::for_bytes(context.as_bytes()),
        kind,
        endpoint: discovery
            .context
            .as_ref()
            .and_then(|context| context.endpoint.as_ref())
            .map(|endpoint| endpoint.address.clone()),
        version: discovery.version.as_ref().and_then(|version| {
            version
                .server
                .as_ref()
                .or(version.client.as_ref())
                .map(ToString::to_string)
        }),
    })
}

fn docker_applicability(
    discovery: &DockerDiscovery,
    warnings: &mut BTreeSet<PlanWarning>,
) -> PlanApplicability {
    match discovery.context.as_ref().map(|context| context.kind) {
        Some(DockerContextKind::Local | DockerContextKind::Rootless) => PlanApplicability::Ready,
        Some(DockerContextKind::Remote) => {
            warnings.insert(PlanWarning::RemoteTarget);
            PlanApplicability::ManualOnly
        }
        Some(DockerContextKind::Desktop) => {
            warnings.insert(PlanWarning::ManagedDesktop);
            PlanApplicability::ManualOnly
        }
        Some(DockerContextKind::Unknown) | None => PlanApplicability::ManualOnly,
    }
}

fn docker_privilege(discovery: &DockerDiscovery) -> RequiredPrivilege {
    match discovery.context.as_ref().map(|context| context.kind) {
        Some(DockerContextKind::Local) => RequiredPrivilege::Root,
        Some(DockerContextKind::Rootless | DockerContextKind::Desktop) => {
            RequiredPrivilege::CurrentUser
        }
        Some(DockerContextKind::Remote) => RequiredPrivilege::RemoteAdministrator,
        Some(DockerContextKind::Unknown) | None => RequiredPrivilege::Unknown,
    }
}

fn docker_candidate(
    snapshot: &NativeConfigSnapshot,
    policy: &ContainerRegistryConfig,
) -> Result<NativeConfigCandidate, MirrorPlanError> {
    let mut value = match snapshot.bytes() {
        Some(bytes) => serde_json::from_slice::<serde_json::Value>(bytes)
            .map_err(|_| MirrorPlanError::InvalidNativeJson)?,
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    let object = value
        .as_object_mut()
        .ok_or(MirrorPlanError::InvalidNativeJson)?;
    object.insert(
        "registry-mirrors".to_owned(),
        serde_json::Value::Array(
            validated_mirrors(policy)?
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|_| MirrorPlanError::InvalidNativeJson)?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| MirrorPlanError::InvalidNativeJson)?;
    Ok(NativeConfigCandidate::new(
        snapshot,
        NativeConfigFormat::Json,
        bytes,
    )?)
}

fn containerd_target(discovery: &ContainerdDiscovery) -> MirrorPlanTarget {
    let version = discovery
        .versions
        .ctr_server
        .as_ref()
        .or(discovery.versions.containerd.as_ref())
        .map(ToString::to_string);
    MirrorPlanTarget::Containerd {
        endpoint: discovery.address.clone(),
        namespace: discovery.namespace.clone(),
        version,
        config_path: discovery
            .config
            .as_ref()
            .and_then(|config| config.registry_config_path.as_ref())
            .and_then(|path| path.to_str())
            .map(str::to_owned),
    }
}

fn containerd_privilege(discovery: &ContainerdDiscovery) -> RequiredPrivilege {
    let (_, scope) = super::classify_endpoint(discovery.address.as_str());
    match scope {
        super::report::EndpointScope::Local => RequiredPrivilege::Root,
        super::report::EndpointScope::Remote => RequiredPrivilege::RemoteAdministrator,
        _ => RequiredPrivilege::Unknown,
    }
}

fn hosts_root_from_snapshot(snapshot: &NativeConfigSnapshot) -> Result<PathBuf, MirrorPlanError> {
    let path = Path::new(&snapshot.fingerprint().path);
    if path.file_name().and_then(|name| name.to_str()) != Some("hosts.toml") {
        return Err(MirrorPlanError::InvalidHostsPath);
    }
    path.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or(MirrorPlanError::InvalidHostsPath)
}

fn containerd_hosts_candidate(
    snapshot: &NativeConfigSnapshot,
    registry: &RegistryName,
    mirrors: &[String],
    capabilities: &BTreeSet<PlannedCapability>,
) -> Result<NativeConfigCandidate, MirrorPlanError> {
    let mut document = parse_toml_snapshot(snapshot)?;
    if snapshot.is_missing() {
        document["server"] = toml_edit::value(origin_endpoint(registry));
    }
    let host = ensure_table(&mut document, "host")?;
    for mirror in mirrors {
        // Configured mirror paths are base prefixes. With containerd's normal
        // host semantics it appends `/v2/...` after this prefix, matching the
        // registry diagnostic transport. Do not infer `override_path`; that
        // flag means the configured path is already the complete API root.
        let rendered = trim_root_url(mirror);
        let key = existing_url_key(host, &rendered).unwrap_or(rendered);
        let entry = host
            .entry(&key)
            .or_insert_with(|| Item::Table(Table::new()));
        let table = entry
            .as_table_like_mut()
            .ok_or(MirrorPlanError::InvalidNativeToml)?;
        let mut values = Array::new();
        values.push("pull");
        if capabilities.contains(&PlannedCapability::Resolve) {
            values.push("resolve");
        }
        table.insert("capabilities", toml_edit::value(values));
    }
    candidate_from_toml(snapshot, document)
}

fn containerd_main_candidate(
    snapshot: &NativeConfigSnapshot,
    discovery: &ContainerdDiscovery,
    config_path: &Path,
) -> Result<NativeConfigCandidate, MirrorPlanError> {
    let mut document = parse_toml_snapshot(snapshot)?;
    let major = discovery
        .versions
        .ctr_server
        .as_ref()
        .or(discovery.versions.containerd.as_ref())
        .map(|version| version.major)
        .unwrap_or(1);
    let plugin = if major >= 2 {
        "io.containerd.cri.v1.images"
    } else {
        "io.containerd.grpc.v1.cri"
    };
    document["plugins"][plugin]["registry"]["config_path"] =
        toml_edit::value(path_string(config_path)?);
    candidate_from_toml(snapshot, document)
}

fn buildkit_target(discovery: &BuildkitDiscovery) -> Result<MirrorPlanTarget, MirrorPlanError> {
    let selected = discovery
        .selected_builder
        .as_ref()
        .ok_or(MirrorPlanError::MissingBuilder)?;
    let mut nodes = selected
        .nodes
        .iter()
        .map(|node| {
            #[derive(Serialize)]
            struct NodeIdentity<'a> {
                name: &'a str,
                endpoint: Option<&'a super::report::Endpoint>,
                version: Option<String>,
                platforms: Vec<String>,
            }
            let mut platforms = node
                .platforms
                .iter()
                .map(|platform| {
                    let mut value = format!("{}/{}", platform.os, platform.architecture);
                    if let Some(variant) = &platform.variant {
                        value.push('/');
                        value.push_str(variant);
                    }
                    value
                })
                .collect::<Vec<_>>();
            platforms.sort();
            Fingerprint::for_canonical(&NodeIdentity {
                name: &node.name,
                endpoint: node.endpoint.as_ref(),
                version: node.buildkit_version.as_ref().map(ToString::to_string),
                platforms,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    nodes.sort();
    Ok(MirrorPlanTarget::Buildkit {
        builder: selected.name.clone(),
        driver: map_builder_driver(&selected.driver),
        nodes,
        version: discovery.buildx_version.as_ref().map(ToString::to_string),
    })
}

fn map_builder_driver(driver: &BuilderDriver) -> BuildkitTargetDriver {
    match driver {
        BuilderDriver::Docker => BuildkitTargetDriver::Docker,
        BuilderDriver::DockerContainer => BuildkitTargetDriver::DockerContainer,
        BuilderDriver::Kubernetes => BuildkitTargetDriver::Kubernetes,
        BuilderDriver::Remote => BuildkitTargetDriver::Remote,
        BuilderDriver::Cloud => BuildkitTargetDriver::Cloud,
        BuilderDriver::Unknown => BuildkitTargetDriver::Unknown,
    }
}

fn builder_is_local(builder: &super::buildkit::SelectedBuilder) -> bool {
    !builder.nodes.is_empty()
        && builder.nodes.iter().all(|node| {
            node.endpoint
                .as_ref()
                .is_some_and(|endpoint| endpoint.scope == super::report::EndpointScope::Local)
        })
}

fn buildkit_privilege(driver: BuilderDriver) -> RequiredPrivilege {
    match driver {
        BuilderDriver::Docker | BuilderDriver::DockerContainer => RequiredPrivilege::CurrentUser,
        BuilderDriver::Kubernetes | BuilderDriver::Remote | BuilderDriver::Cloud => {
            RequiredPrivilege::RemoteAdministrator
        }
        BuilderDriver::Unknown => RequiredPrivilege::Unknown,
    }
}

fn buildkit_candidate(
    snapshot: &NativeConfigSnapshot,
    registry: &RegistryName,
    mirrors: &[String],
) -> Result<NativeConfigCandidate, MirrorPlanError> {
    let mut document = parse_toml_snapshot(snapshot)?;
    let registries = ensure_table(&mut document, "registry")?;
    let entry = registries
        .entry(registry.as_str())
        .or_insert_with(|| Item::Table(Table::new()));
    let table = entry
        .as_table_like_mut()
        .ok_or(MirrorPlanError::InvalidNativeToml)?;
    let mut values = Array::new();
    for mirror in mirrors {
        values.push(mirror_authority(mirror)?);
    }
    table.insert("mirrors", toml_edit::value(values));
    candidate_from_toml(snapshot, document)
}

fn parse_toml_snapshot(snapshot: &NativeConfigSnapshot) -> Result<DocumentMut, MirrorPlanError> {
    match snapshot.bytes() {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| MirrorPlanError::InvalidNativeToml)?
            .parse()
            .map_err(|_| MirrorPlanError::InvalidNativeToml),
        None => Ok(DocumentMut::new()),
    }
}

fn candidate_from_toml(
    snapshot: &NativeConfigSnapshot,
    document: DocumentMut,
) -> Result<NativeConfigCandidate, MirrorPlanError> {
    let bytes = document.to_string().into_bytes();
    std::str::from_utf8(&bytes)
        .map_err(|_| MirrorPlanError::InvalidNativeToml)?
        .parse::<toml::Value>()
        .map_err(|_| MirrorPlanError::InvalidNativeToml)?;
    Ok(NativeConfigCandidate::new(
        snapshot,
        NativeConfigFormat::Toml,
        bytes,
    )?)
}

fn ensure_table<'a>(
    document: &'a mut DocumentMut,
    key: &str,
) -> Result<&'a mut Table, MirrorPlanError> {
    if document.get(key).is_none() {
        let mut table = Table::new();
        table.set_implicit(true);
        document.insert(key, Item::Table(table));
    }
    document
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or(MirrorPlanError::InvalidNativeToml)
}

fn existing_url_key(table: &Table, requested: &str) -> Option<String> {
    table
        .iter()
        .find(|(key, _)| urls_equivalent(key, requested))
        .map(|(key, _)| key.to_owned())
}

fn urls_equivalent(left: &str, right: &str) -> bool {
    match (mirror_url(left), mirror_url(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn validated_mirrors(policy: &ContainerRegistryConfig) -> Result<Vec<String>, MirrorPlanError> {
    policy
        .mirrors
        .iter()
        .map(|mirror| mirror_url(mirror).map(Into::into))
        .collect()
}

fn mirror_url(value: &str) -> Result<reqwest::Url, MirrorPlanError> {
    let mut url = reqwest::Url::parse(value).map_err(|_| MirrorPlanError::UnsafeMirrorUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MirrorPlanError::UnsafeMirrorUrl);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{path}/"));
    Ok(url)
}

fn mirror_has_path(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| !matches!(url.path(), "" | "/"))
}

fn planned_mirrors(values: &[String]) -> Result<Vec<PlannedMirrorEndpoint>, MirrorPlanError> {
    values
        .iter()
        .map(|value| {
            let url = mirror_url(value)?;
            Ok(PlannedMirrorEndpoint {
                origin: super::RedactedUrl::parse(value)
                    .map_err(|_| MirrorPlanError::UnsafeMirrorUrl)?,
                has_path_prefix: !matches!(url.path(), "" | "/"),
            })
        })
        .collect()
}

fn trim_root_url(value: &str) -> String {
    value.trim_end_matches('/').to_owned()
}

fn mirror_authority(value: &str) -> Result<String, MirrorPlanError> {
    let value = mirror_url(value)?.to_string();
    trim_root_url(&value)
        .strip_prefix("https://")
        .map(str::to_owned)
        .ok_or(MirrorPlanError::UnsafeMirrorUrl)
}

fn origin_endpoint(registry: &RegistryName) -> String {
    if registry.is_docker_hub() {
        "https://registry-1.docker.io".to_owned()
    } else {
        format!("https://{}", registry.as_str())
    }
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    let left = dunce::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = dunce::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn path_string(path: &Path) -> Result<String, MirrorPlanError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(MirrorPlanError::InvalidNativePath)
}

#[derive(Debug, thiserror::Error)]
pub enum MirrorPlanError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("mirror endpoints must be HTTPS URLs without credentials, query, or fragment")]
    UnsafeMirrorUrl,
    #[error("Docker Engine registry mirrors do not support path-prefixed endpoints")]
    UnsupportedDockerMirrorPath,
    #[error("native Docker configuration is not a JSON object")]
    InvalidNativeJson,
    #[error("native container configuration is not valid TOML for this semantic change")]
    InvalidNativeToml,
    #[error("containerd hosts target must explicitly name <config_path>/<namespace>/hosts.toml")]
    InvalidHostsPath,
    #[error("containerd hosts target does not belong to the discovered config_path")]
    ConfigPathMismatch,
    #[error("selected Buildx builder was not discovered")]
    MissingBuilder,
    #[error("native configuration path is not valid UTF-8")]
    InvalidNativePath,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use semver::Version;

    use super::*;
    use crate::container::buildkit::{
        BuildPlatform, BuilderNode, BuilderNodeStatus, SelectedBuilder,
    };
    use crate::container::containerd::{ContainerdConfig, ContainerdVersions};
    use crate::container::docker::{DockerContext, DockerVersion};
    use crate::container::report::{
        DiagnosticReport, DiagnosticStatus, Endpoint, EndpointScope, EndpointTransport, RuntimeKind,
    };
    use crate::container::RedactedUrl;

    fn policy(resolve: ContainerResolve) -> ContainerRegistryConfig {
        ContainerRegistryConfig {
            mirrors: vec![
                "https://first.example/".into(),
                "https://second.example/".into(),
            ],
            anonymous_only: true,
            resolve,
        }
    }

    fn docker(kind: DockerContextKind) -> DockerDiscovery {
        DockerDiscovery {
            status: DiagnosticStatus::Healthy,
            context: Some(DockerContext {
                name: Some("default".into()),
                endpoint: Some(Endpoint::new(
                    EndpointTransport::LocalSocket,
                    EndpointScope::Local,
                    RedactedUrl::parse("unix:///var/run/docker.sock").unwrap(),
                )),
                kind,
                skip_tls_verify: false,
            }),
            version: Some(DockerVersion {
                client: Some(Version::new(28, 0, 0)),
                server: Some(Version::new(28, 0, 0)),
            }),
            info: None,
        }
    }

    #[test]
    fn docker_preserves_unknown_json_keys_and_only_supports_hub() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("daemon.json");
        std::fs::write(
            &path,
            br#"{"debug":true,"features":{"containerd-snapshotter":true},"registry-mirrors":["https://old.example"]}"#,
        )
        .unwrap();
        let snapshot = NativeConfigSnapshot::capture(&path, 4096).unwrap();
        let hub = RegistryName::parse("docker.io").unwrap();
        let bundle = plan_docker_mirrors(DockerMirrorPlanRequest {
            registry: &hub,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &docker(DockerContextKind::Local),
            native_config: Some(&snapshot),
        })
        .unwrap();
        let candidate: serde_json::Value =
            serde_json::from_slice(bundle.candidates[0].bytes()).unwrap();
        assert_eq!(candidate["debug"], true);
        assert_eq!(candidate["features"]["containerd-snapshotter"], true);
        assert_eq!(candidate["registry-mirrors"].as_array().unwrap().len(), 2);
        assert_eq!(bundle.plan.applicability, PlanApplicability::Ready);

        let ghcr = RegistryName::parse("ghcr.io").unwrap();
        let unsupported = plan_docker_mirrors(DockerMirrorPlanRequest {
            registry: &ghcr,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &docker(DockerContextKind::Local),
            native_config: Some(&snapshot),
        })
        .unwrap();
        assert_eq!(
            unsupported.plan.applicability,
            PlanApplicability::Unsupported
        );
        assert!(unsupported.candidates.is_empty());
        assert!(unsupported
            .plan
            .warnings
            .contains(&PlanWarning::DockerHubOnly));
    }

    #[test]
    fn docker_upstream_resolution_and_remote_desktop_are_manual_only() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("daemon.json");
        std::fs::write(&path, b"{}").unwrap();
        let snapshot = NativeConfigSnapshot::capture(&path, 64).unwrap();
        let registry = RegistryName::parse("docker.io").unwrap();
        for kind in [
            DockerContextKind::Local,
            DockerContextKind::Remote,
            DockerContextKind::Desktop,
        ] {
            let bundle = plan_docker_mirrors(DockerMirrorPlanRequest {
                registry: &registry,
                policy: &policy(ContainerResolve::Upstream),
                discovery: &docker(kind),
                native_config: Some(&snapshot),
            })
            .unwrap();
            assert_eq!(bundle.plan.applicability, PlanApplicability::ManualOnly);
            assert!(bundle
                .plan
                .warnings
                .contains(&PlanWarning::ResolutionSeparationUnavailable));
            if matches!(kind, DockerContextKind::Remote | DockerContextKind::Desktop) {
                assert!(bundle.candidates.is_empty());
            }
        }
    }

    fn containerd(config_path: Option<PathBuf>, major: u64) -> ContainerdDiscovery {
        ContainerdDiscovery {
            report: DiagnosticReport::new(RuntimeKind::Containerd, DiagnosticStatus::Healthy),
            versions: ContainerdVersions {
                containerd: Some(Version::new(major, 0, 0)),
                ctr_client: Some(Version::new(major, 0, 0)),
                ctr_server: Some(Version::new(major, 0, 0)),
            },
            address: RedactedUrl::parse("unix:///run/containerd/containerd.sock").unwrap(),
            namespace: "k8s.io".into(),
            config: Some(ContainerdConfig {
                version: Some(if major >= 2 { 3 } else { 2 }),
                registry_config_path: config_path,
                legacy_registry: BTreeSet::new(),
            }),
        }
    }

    #[test]
    fn containerd_preserves_host_order_tls_and_unrelated_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("certs.d");
        let namespace = root.join("ghcr.io");
        std::fs::create_dir_all(&namespace).unwrap();
        let path = namespace.join("hosts.toml");
        std::fs::write(
            &path,
            concat!(
                "server = \"https://ghcr.io\"\n",
                "custom = \"keep\"\n",
                "[host.\"https://unrelated.example\"]\n",
                "  capabilities = [\"pull\", \"resolve\", \"push\"]\n",
                "  ca = \"/secret/ca.pem\"\n",
                "[host.\"https://first.example\"]\n",
                "  capabilities = [\"pull\", \"resolve\", \"push\"]\n",
                "  skip_verify = true\n",
                "  override_path = true\n",
            ),
        )
        .unwrap();
        let snapshot = NativeConfigSnapshot::capture(&path, 4096).unwrap();
        let registry = RegistryName::parse("ghcr.io").unwrap();
        let bundle = plan_containerd_mirrors(ContainerdMirrorPlanRequest {
            registry: &registry,
            policy: &policy(ContainerResolve::Upstream),
            discovery: &containerd(Some(root), 1),
            hosts_config: Some(&snapshot),
            main_config: None,
        })
        .unwrap();
        let text = std::str::from_utf8(bundle.candidates[0].bytes()).unwrap();
        let unrelated = text.find("https://unrelated.example").unwrap();
        let first = text.find("https://first.example").unwrap();
        let second = text.find("https://second.example").unwrap();
        assert!(unrelated < first && first < second, "{text}");
        assert!(text.contains("ca = \"/secret/ca.pem\""));
        assert!(text.contains("skip_verify = true"));
        assert!(text.contains("override_path = true"));
        assert!(text.contains("custom = \"keep\""));

        let parsed: toml::Value = toml::from_str(text).unwrap();
        let first_caps = parsed["host"]["https://first.example"]["capabilities"]
            .as_array()
            .unwrap();
        assert_eq!(first_caps.len(), 1);
        assert_eq!(first_caps[0].as_str(), Some("pull"));
        let unrelated_caps = parsed["host"]["https://unrelated.example"]["capabilities"]
            .as_array()
            .unwrap();
        assert_eq!(unrelated_caps.len(), 3);
    }

    #[test]
    fn containerd_mirror_resolution_adds_resolve_without_push() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("certs.d");
        let namespace = root.join("ghcr.io");
        std::fs::create_dir_all(&namespace).unwrap();
        let snapshot = NativeConfigSnapshot::capture(&namespace.join("hosts.toml"), 4096).unwrap();
        let registry = RegistryName::parse("ghcr.io").unwrap();
        let bundle = plan_containerd_mirrors(ContainerdMirrorPlanRequest {
            registry: &registry,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &containerd(Some(root), 1),
            hosts_config: Some(&snapshot),
            main_config: None,
        })
        .unwrap();
        let text = std::str::from_utf8(bundle.candidates[0].bytes()).unwrap();
        assert!(text.contains("capabilities = [\"pull\", \"resolve\"]"));
        assert!(!text.contains("push"));
        assert_eq!(bundle.plan.activation, ActivationRequirement::None);
    }

    #[test]
    fn missing_containerd_config_path_plans_versioned_key_and_restart() {
        for (major, plugin) in [
            (1, "io.containerd.grpc.v1.cri"),
            (2, "io.containerd.cri.v1.images"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("certs.d");
            let namespace = root.join("docker.io");
            std::fs::create_dir_all(&namespace).unwrap();
            let hosts = NativeConfigSnapshot::capture(&namespace.join("hosts.toml"), 4096).unwrap();
            let main_path = temporary.path().join("config.toml");
            std::fs::write(&main_path, "version = 2\n[debug]\nlevel = \"info\"\n").unwrap();
            let main = NativeConfigSnapshot::capture(&main_path, 4096).unwrap();
            let registry = RegistryName::parse("docker.io").unwrap();
            let bundle = plan_containerd_mirrors(ContainerdMirrorPlanRequest {
                registry: &registry,
                policy: &policy(ContainerResolve::Mirror),
                discovery: &containerd(None, major),
                hosts_config: Some(&hosts),
                main_config: Some(&main),
            })
            .unwrap();
            let main_candidate = bundle
                .candidates
                .iter()
                .find(|candidate| candidate.fingerprint().path.ends_with("config.toml"))
                .unwrap();
            let text = std::str::from_utf8(main_candidate.bytes()).unwrap();
            assert!(text.contains(plugin), "{text}");
            assert!(text.contains("level = \"info\""));
            assert_eq!(bundle.plan.activation, ActivationRequirement::RestartDaemon);
            assert_eq!(bundle.plan.applicability, PlanApplicability::ManualOnly);
        }
    }

    fn buildkit(driver: BuilderDriver, scope: EndpointScope) -> BuildkitDiscovery {
        BuildkitDiscovery {
            report: DiagnosticReport::new(RuntimeKind::Buildkit, DiagnosticStatus::Healthy),
            buildx_version: Some(Version::new(0, 36, 1)),
            selected_builder: Some(SelectedBuilder {
                name: "selected".into(),
                driver,
                nodes: vec![BuilderNode {
                    name: "selected0".into(),
                    endpoint: Some(Endpoint::new(
                        EndpointTransport::LocalSocket,
                        scope,
                        RedactedUrl::parse("unix:///var/run/docker.sock").unwrap(),
                    )),
                    status: BuilderNodeStatus::Running,
                    buildkit_version: Some(Version::new(0, 25, 0)),
                    platforms: BTreeSet::from([BuildPlatform {
                        os: "linux".into(),
                        architecture: "amd64".into(),
                        variant: None,
                    }]),
                }],
                has_error: false,
            }),
        }
    }

    #[test]
    fn buildkit_enforces_driver_boundaries_and_preserves_gc_tls() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("buildkitd.toml");
        std::fs::write(
            &path,
            concat!(
                "[worker.oci]\n  gc = true\n",
                "[registry.\"ghcr.io\"]\n  ca = [\"/secret/ca.pem\"]\n",
            ),
        )
        .unwrap();
        let snapshot = NativeConfigSnapshot::capture(&path, 4096).unwrap();
        let registry = RegistryName::parse("ghcr.io").unwrap();
        let docker_driver = plan_buildkit_mirrors(BuildkitMirrorPlanRequest {
            registry: &registry,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &buildkit(BuilderDriver::Docker, EndpointScope::Local),
            native_config: Some(&snapshot),
        })
        .unwrap();
        assert_eq!(
            docker_driver.plan.applicability,
            PlanApplicability::Unsupported
        );
        assert!(docker_driver.candidates.is_empty());

        let container_driver = plan_buildkit_mirrors(BuildkitMirrorPlanRequest {
            registry: &registry,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &buildkit(BuilderDriver::DockerContainer, EndpointScope::Local),
            native_config: Some(&snapshot),
        })
        .unwrap();
        assert_eq!(
            container_driver.plan.applicability,
            PlanApplicability::Ready
        );
        assert_eq!(
            container_driver.plan.activation,
            ActivationRequirement::RecreateBuilder
        );
        let text = std::str::from_utf8(container_driver.candidates[0].bytes()).unwrap();
        assert!(text.contains("gc = true"));
        assert!(text.contains("ca = [\"/secret/ca.pem\"]"));
        assert!(text.contains("mirrors = [\"first.example\", \"second.example\"]"));

        let remote = plan_buildkit_mirrors(BuildkitMirrorPlanRequest {
            registry: &registry,
            policy: &policy(ContainerResolve::Mirror),
            discovery: &buildkit(BuilderDriver::Remote, EndpointScope::Remote),
            native_config: Some(&snapshot),
        })
        .unwrap();
        assert_eq!(remote.plan.applicability, PlanApplicability::ManualOnly);
        assert!(remote.candidates.is_empty());
    }

    #[test]
    fn path_prefixed_mirrors_are_rendered_per_runtime_without_truncation() {
        let registry = RegistryName::parse("docker.io").unwrap();
        let prefixed_policy = ContainerRegistryConfig {
            mirrors: vec!["https://mirror.example:5443/cache/".into()],
            anonymous_only: true,
            resolve: ContainerResolve::Mirror,
        };
        let docker = plan_docker_mirrors(DockerMirrorPlanRequest {
            registry: &registry,
            policy: &prefixed_policy,
            discovery: &docker(DockerContextKind::Local),
            native_config: None,
        })
        .unwrap_err();
        assert!(matches!(
            docker,
            MirrorPlanError::UnsupportedDockerMirrorPath
        ));

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("certs.d");
        let namespace = root.join("docker.io");
        std::fs::create_dir_all(&namespace).unwrap();
        let hosts = NativeConfigSnapshot::capture(&namespace.join("hosts.toml"), 4096).unwrap();
        let containerd = plan_containerd_mirrors(ContainerdMirrorPlanRequest {
            registry: &registry,
            policy: &prefixed_policy,
            discovery: &containerd(Some(root), 2),
            hosts_config: Some(&hosts),
            main_config: None,
        })
        .unwrap();
        let text = std::str::from_utf8(containerd.candidates[0].bytes()).unwrap();
        assert!(text.contains("https://mirror.example:5443/cache"), "{text}");
        assert!(!text.contains("override_path"), "{text}");
        assert!(
            text.contains("capabilities = [\"pull\", \"resolve\"]"),
            "{text}"
        );
        assert!(!text.contains("push"), "{text}");
        let plan_json = serde_json::to_string(&containerd.plan).unwrap();
        assert!(!plan_json.contains("cache"), "prefix leaked: {plan_json}");
        assert!(plan_json.contains("[redacted]"), "{plan_json}");
        assert!(
            plan_json.contains("\"has_path_prefix\":true"),
            "{plan_json}"
        );

        let buildkit_path = temporary.path().join("buildkitd.toml");
        let buildkit_snapshot = NativeConfigSnapshot::capture(&buildkit_path, 4096).unwrap();
        let buildkit = plan_buildkit_mirrors(BuildkitMirrorPlanRequest {
            registry: &registry,
            policy: &prefixed_policy,
            discovery: &buildkit(BuilderDriver::DockerContainer, EndpointScope::Local),
            native_config: Some(&buildkit_snapshot),
        })
        .unwrap();
        let text = std::str::from_utf8(buildkit.candidates[0].bytes()).unwrap();
        assert!(
            text.contains("mirrors = [\"mirror.example:5443/cache\"]"),
            "{text}"
        );
        let plan_json = serde_json::to_string(&buildkit.plan).unwrap();
        assert!(!plan_json.contains("cache"), "prefix leaked: {plan_json}");
        assert!(plan_json.contains("[redacted]"), "{plan_json}");
    }
}
