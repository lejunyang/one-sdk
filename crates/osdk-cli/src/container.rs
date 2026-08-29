//! User-facing native container diagnostics and explicitly gated operations.

use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use osdk_core::config::{ContainerPlatform, ContainerRegistryConfig, ContainerRuntime};
use osdk_core::container::operations::{
    BuildxPrune, ContainerdPrune, ContainerdPull, DockerImagePrune, DockerPull, NativePruneScope,
    NativePruneTarget, NativePruneWarning, PruneConfirmation, PrunePreview, UnsupportedPrune,
};
use osdk_core::container::{
    diagnose_registry, plan_buildkit_mirrors, plan_containerd_mirrors, plan_docker_mirrors,
    ActivationRequirement, ApiCheckStatus, BuilderDriver, BuilderNodeStatus, BuildkitAdapter,
    BuildkitMirrorPlanRequest, BuildxBuilderSelector, BuildxCacheQuery, CacheQueryStatus,
    Capability, CapabilityStatus, ContainerdAdapter, ContainerdCacheQuery,
    ContainerdMirrorPlanRequest, DiagnosticDetails, DiagnosticReport, DiagnosticStatus,
    DockerAdapter, DockerCacheQuery, DockerContextKind, DockerDaemonArchitecture, DockerDaemonOs,
    DockerMirrorPlanRequest, ImageReference, LegacyRegistryWarning, ManifestCheckStatus,
    MirrorCheckStatus, MirrorPlan, NativeCacheOwner, NativeCacheRecordKind, NativeCacheStatus,
    NativeConfigSnapshot, OciPlatform, PlanApplicability, PlanWarning, ProbeCommand,
    RegistryDiagnosticOptions, RegistryDiagnosticReport, RegistryDiagnosticStatus,
    RegistryEndpoint, RegistryLimits, RegistryName, RegistryTransport, ReqwestRegistryTransport,
    RuntimeAdapter, RuntimeKind, DEFAULT_MAX_REQUESTS, MAX_NATIVE_CONFIG_BYTES,
};
use osdk_core::i18n::{self, interpolate, trl, Lang};
use osdk_core::process::{
    CaptureLimits, CommandOutcome, CommandRunner, CommandSpec, SystemCommandRunner,
};
use serde::Serialize;

use crate::app::App;
use crate::cli::{
    ContainerCacheCommand, ContainerCacheRuntimeArg, ContainerCommand, ContainerMirrorRuntimeArg,
    ContainerMirrorsCommand, ContainerPruneRuntimeArg, ContainerPruneScopeArg,
    ContainerRegistryCommand, ContainerRuntimeArg,
};

const DOCTOR_SCHEMA_VERSION: u32 = 2;
const CAPTURE_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RuntimeSelection {
    Auto,
    Docker,
    Containerd,
}

#[derive(Debug, Serialize)]
struct DoctorOutput {
    schema_version: u32,
    requested_runtime: RuntimeSelection,
    selected_runtime: RuntimeKind,
    runtime: DiagnosticReport,
    attempted_runtimes: Vec<DiagnosticReport>,
    builder: Option<DiagnosticReport>,
}

pub async fn run(app: &App, command: ContainerCommand) -> Result<Option<ExitStatus>> {
    let runner = SystemCommandRunner;
    match command {
        ContainerCommand::Pull {
            image,
            runtime,
            platform,
            address,
            namespace,
        } => pull(
            &runner,
            app.ctx.config.containers(),
            app.ctx.config.settings.offline,
            runtime,
            image,
            platform,
            address,
            namespace,
        )
        .map(Some),
        ContainerCommand::Prune {
            runtime,
            scope,
            context,
            builder,
            execute,
            accept_preview,
        } => native_prune(
            &runner,
            app.prompt.as_ref(),
            app.ctx.config.containers(),
            runtime,
            scope,
            context,
            builder,
            execute,
            accept_preview.as_deref(),
            &mut std::io::stdout(),
        ),
        command => {
            run_with(
                &SystemCommandRunner,
                || {
                    ReqwestRegistryTransport::new().map_err(|error| {
                        anyhow!(osdk_core::t!(
                            "err.container.registry_transport",
                            error = error
                        ))
                    })
                },
                app.ctx.config.containers(),
                app.ctx.config.settings.offline,
                command,
                &mut std::io::stdout(),
            )
            .await?;
            Ok(None)
        }
    }
}

async fn run_with<T, F>(
    runner: &dyn CommandRunner,
    transport_factory: F,
    config: &osdk_core::config::ContainersConfig,
    offline: bool,
    command: ContainerCommand,
    output: &mut dyn Write,
) -> Result<()>
where
    T: RegistryTransport,
    F: FnOnce() -> Result<T>,
{
    let limits = capture_limits(config.probe_timeout_ms);
    match command {
        ContainerCommand::Pull { .. } | ContainerCommand::Prune { .. } => {
            unreachable!("native operations are dispatched before read-only commands")
        }
        ContainerCommand::Doctor {
            runtime,
            builder,
            json,
        } => {
            let runtime = runtime
                .map(RuntimeSelection::from)
                .unwrap_or_else(|| RuntimeSelection::from(config.runtime));
            let builder_explicit = builder.is_some();
            let builder = builder.unwrap_or_else(|| config.builder.clone());
            let report = doctor(runner, limits, runtime, builder, builder_explicit);
            if json {
                serde_json::to_writer(&mut *output, &report)
                    .context("serializing container diagnostic report")?;
                writeln!(output)?;
            } else {
                write_doctor_human(output, &report, i18n::current())?;
            }
        }
        ContainerCommand::Cache { command } => match command {
            ContainerCacheCommand::Status {
                runtime,
                builder,
                json,
            } => {
                let runtime = runtime.unwrap_or_else(|| config.runtime.into());
                let builder = builder.unwrap_or_else(|| config.builder.clone());
                let status = cache_status(runner, limits, runtime, builder);
                if json {
                    serde_json::to_writer(&mut *output, &status)
                        .context("serializing native cache status")?;
                    writeln!(output)?;
                } else {
                    write_cache_human(output, &status, i18n::current())?;
                }
            }
        },
        ContainerCommand::Registry { command } => match command {
            ContainerRegistryCommand::Test {
                registry,
                image,
                platform,
                json,
            } => {
                if offline {
                    return Err(anyhow!(osdk_core::t!("err.container.registry_offline")));
                }
                let transport = transport_factory()?;
                let report = registry_test(&transport, config, &registry, image, platform).await?;
                if json {
                    serde_json::to_writer(&mut *output, &report)
                        .context("serializing container registry diagnostic report")?;
                    writeln!(output)?;
                } else {
                    write_registry_human(output, &report, i18n::current())?;
                }
            }
        },
        ContainerCommand::Mirrors { command } => match command {
            ContainerMirrorsCommand::Plan {
                registry,
                runtime,
                builder,
                native_config,
                containerd_main_config,
                json,
            } => {
                let plan = mirror_plan(
                    runner,
                    config,
                    &registry,
                    runtime,
                    builder,
                    native_config.as_deref(),
                    containerd_main_config.as_deref(),
                )?;
                if json {
                    // Serialize only the semantic plan. `MirrorPlanBundle` and
                    // its candidate bytes deliberately never cross this boundary.
                    serde_json::to_writer(&mut *output, &plan)
                        .context("serializing native mirror plan")?;
                    writeln!(output)?;
                } else {
                    write_mirror_plan_human(output, &plan, i18n::current())?;
                }
            }
        },
    }
    Ok(())
}

fn pull(
    runner: &dyn CommandRunner,
    config: &osdk_core::config::ContainersConfig,
    offline: bool,
    runtime: Option<ContainerRuntimeArg>,
    image: ImageReference,
    platform: Option<OciPlatform>,
    address: Option<String>,
    namespace: Option<String>,
) -> Result<ExitStatus> {
    if offline {
        return Err(anyhow!(osdk_core::t!("err.container.pull_offline")));
    }
    let runtime = runtime
        .map(RuntimeSelection::from)
        .unwrap_or_else(|| RuntimeSelection::from(config.runtime));
    let platform = platform.or(configured_platform(config)?);
    let selected = match runtime {
        RuntimeSelection::Docker => RuntimeKind::Docker,
        RuntimeSelection::Containerd => RuntimeKind::Containerd,
        RuntimeSelection::Auto => resolve_pull_runtime(
            runner,
            capture_limits(config.probe_timeout_ms),
            address.as_deref().zip(namespace.as_deref()),
        )?,
    };
    if runtime == RuntimeSelection::Docker && (address.is_some() || namespace.is_some()) {
        return Err(anyhow!(osdk_core::t!(
            "err.container.containerd_selectors_runtime"
        )));
    }

    let operation = match selected {
        RuntimeKind::Docker => {
            let mut pull = DockerPull::new(image);
            if let Some(platform) = platform {
                pull = pull.with_platform(platform);
            }
            pull.into_command()
        }
        RuntimeKind::Containerd => {
            let (address, namespace) = address.zip(namespace).ok_or_else(|| {
                anyhow!(osdk_core::t!(
                    "err.container.containerd_pull_selectors_required"
                ))
            })?;
            let mut pull = ContainerdPull::new(address, namespace, image)
                .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_containerd_target")))?;
            if let Some(platform) = platform {
                pull = pull.with_platform(platform);
            }
            pull.into_command()
        }
        _ => unreachable!("pull selection only returns Docker or containerd"),
    };
    operation
        .execute(runner)
        .map_err(|error| anyhow!(osdk_core::t!("err.container.native_spawn", error = error)))
}

fn resolve_pull_runtime(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    containerd_target: Option<(&str, &str)>,
) -> Result<RuntimeKind> {
    // Resolve exactly once before starting a foreground operation. Both
    // candidates are inspected in fixed order and there is no post-launch
    // fallback.
    let docker = DockerAdapter.diagnose(runner, limits);
    let containerd = match containerd_target {
        Some((address, namespace)) => ContainerdAdapter::new(address, namespace)
            .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_containerd_target")))?
            .diagnose(runner, limits),
        None => ContainerdAdapter::default().diagnose(runner, limits),
    };
    let supports_pull = |report: &DiagnosticReport| {
        report.capabilities.get(&Capability::Pull) == Some(&CapabilityStatus::Supported)
    };
    match (supports_pull(&docker), supports_pull(&containerd)) {
        (true, true) => Ok(
            if status_rank(docker.status) >= status_rank(containerd.status) {
                RuntimeKind::Docker
            } else {
                RuntimeKind::Containerd
            },
        ),
        (true, false) => Ok(RuntimeKind::Docker),
        (false, true) => Ok(RuntimeKind::Containerd),
        (false, false) => Err(anyhow!(osdk_core::t!(
            "err.container.pull_unavailable",
            docker = diagnostic_status_label(i18n::current(), docker.status),
            containerd = diagnostic_status_label(i18n::current(), containerd.status)
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn native_prune(
    runner: &dyn CommandRunner,
    prompt: &dyn crate::prompt::Prompt,
    config: &osdk_core::config::ContainersConfig,
    runtime: ContainerPruneRuntimeArg,
    scope: ContainerPruneScopeArg,
    context: Option<String>,
    builder: Option<BuildxBuilderSelector>,
    execute: bool,
    accepted_preview: Option<&str>,
    output: &mut dyn Write,
) -> Result<Option<ExitStatus>> {
    let limits = capture_limits(config.probe_timeout_ms);
    match runtime {
        ContainerPruneRuntimeArg::Containerd => {
            if context.is_some() || builder.is_some() || execute || accepted_preview.is_some() {
                return Err(anyhow!(osdk_core::t!(
                    "err.container.prune_selector_runtime"
                )));
            }
            let unsupported = ContainerdPrune.preview();
            write_unsupported_prune(output, &unsupported, i18n::current())?;
            Err(anyhow!(osdk_core::t!("err.container.prune_unsupported")))
        }
        ContainerPruneRuntimeArg::Docker => {
            if scope != ContainerPruneScopeArg::Images {
                return Err(anyhow!(osdk_core::t!("err.container.prune_docker_scope")));
            }
            if builder.is_some() {
                return Err(anyhow!(osdk_core::t!(
                    "err.container.prune_builder_runtime"
                )));
            }
            if let Some(requested) = context.as_deref() {
                // Validate before forwarding the value to the read-only native
                // discovery command.
                validate_native_target_name(requested)
                    .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_docker_context")))?;
            }
            let prune = discover_docker_prune(runner, limits, context.as_deref())?;
            let preview = prune.preview()?;
            write_prune_preview(output, &preview, i18n::current())?;
            execute_docker_prune(
                runner,
                prompt,
                prune,
                &preview,
                execute,
                accepted_preview,
                output,
            )
        }
        ContainerPruneRuntimeArg::Buildkit => {
            if scope != ContainerPruneScopeArg::BuildCache {
                return Err(anyhow!(osdk_core::t!("err.container.prune_buildkit_scope")));
            }
            if context.is_some() {
                return Err(anyhow!(osdk_core::t!(
                    "err.container.prune_context_runtime"
                )));
            }
            let selector = builder.unwrap_or_else(|| config.builder.clone());
            let prune = discover_buildx_prune(runner, limits, selector)?;
            let preview = prune.preview()?;
            write_prune_preview(output, &preview, i18n::current())?;
            if execute {
                return Err(anyhow!(osdk_core::t!(
                    "err.container.prune_buildkit_execute_unsupported"
                )));
            }
            Ok(None)
        }
    }
}

fn discover_buildx_prune(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    selector: BuildxBuilderSelector,
) -> Result<BuildxPrune> {
    let discovery = BuildkitAdapter::new(selector).inspect(runner, limits);
    let selected = discovery.selected_builder.as_ref().ok_or_else(|| {
        anyhow!(osdk_core::t!(
            "err.container.prune_builder_unavailable",
            status = diagnostic_status_label(i18n::current(), discovery.report.status)
        ))
    })?;
    let topology_fingerprint = selected
        .topology_fingerprint()?
        .ok_or_else(|| anyhow!(osdk_core::t!("err.container.prune_target_identity")))?;
    let selector = BuildxBuilderSelector::named(selected.name.clone())
        .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_builder")))?;
    BuildxPrune::new(selector, topology_fingerprint).map_err(Into::into)
}

fn execute_docker_prune(
    runner: &dyn CommandRunner,
    prompt: &dyn crate::prompt::Prompt,
    prune: DockerImagePrune,
    preview: &PrunePreview,
    execute: bool,
    accepted_preview: Option<&str>,
    output: &mut dyn Write,
) -> Result<Option<ExitStatus>> {
    if !execute {
        return Ok(None);
    }
    if accepted_preview != Some(preview.preview_id.as_str()) {
        return Err(anyhow!(osdk_core::t!(
            "err.container.prune_preview_mismatch",
            preview_id = preview.preview_id.as_str()
        )));
    }
    output.flush()?;
    let question = osdk_core::t!(
        "prompt.container_prune",
        preview_id = preview.preview_id.as_str()
    );
    if !prompt.confirm(&question)? {
        writeln!(output, "{}", osdk_core::t!("msg.cancelled"))?;
        return Ok(None);
    }
    let confirmation = PruneConfirmation::accept(preview)?;
    let operation = prune.execute(confirmation)?;
    operation
        .execute(runner)
        .map(Some)
        .map_err(|error| anyhow!(osdk_core::t!("err.container.native_spawn", error = error)))
}

fn discover_docker_prune(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    requested: Option<&str>,
) -> Result<DockerImagePrune> {
    let mut command = CommandSpec::new("docker").args(["context", "inspect"]);
    if let Some(requested) = requested {
        command = command.arg(requested);
    }
    let outcome = ProbeCommand::new(
        osdk_core::container::NativeProgram::Docker,
        osdk_core::container::CommandPurpose::ContextInspect,
        command,
    )
    .execute(runner, limits);
    let output = match &outcome {
        CommandOutcome::Exited { status, output }
            if status.success() && !output.stdout_truncated =>
        {
            &output.stdout
        }
        _ => {
            return Err(anyhow!(osdk_core::t!(
                "err.container.docker_context_unavailable"
            )))
        }
    };
    let discovered = osdk_core::container::docker::parse_docker_context(output)
        .map_err(|_| anyhow!(osdk_core::t!("err.container.docker_context_unavailable")))?;
    if requested.is_some_and(|requested| discovered.name.as_deref() != Some(requested)) {
        return Err(anyhow!(osdk_core::t!(
            "err.container.docker_context_mismatch"
        )));
    }
    DockerImagePrune::from_context(discovered).map_err(|error| match error {
        osdk_core::container::operations::PrunePlanError::UnsupportedDockerEndpoint => {
            anyhow!(osdk_core::t!(
                "err.container.prune_docker_endpoint_unsupported"
            ))
        }
        _ => anyhow!(osdk_core::t!("err.container.prune_target_identity")),
    })
}

fn validate_native_target_name(value: &str) -> Result<()> {
    if value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        Ok(())
    } else {
        Err(anyhow!("invalid native target name"))
    }
}

fn write_prune_preview(
    output: &mut dyn Write,
    preview: &PrunePreview,
    lang: Lang,
) -> std::io::Result<()> {
    let owner = prune_owner_label(lang, preview.owner);
    let scope = prune_scope_label(lang, preview.scope);
    writeln!(output, "{}", trl(lang, "msg.container.prune_preview"))?;
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.prune_preview_id",
            &[("preview_id", preview.preview_id.as_str())]
        )
    )?;
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.prune_owner_scope",
            &[("owner", &owner), ("scope", &scope)]
        )
    )?;
    let (kind, name) = match &preview.target {
        NativePruneTarget::DockerContext { name, .. } => (
            trl(lang, "label.container.prune_target.docker_context"),
            name.as_str(),
        ),
        NativePruneTarget::BuildxBuilder { name, .. } => (
            trl(lang, "label.container.prune_target.buildx_builder"),
            name.as_str(),
        ),
    };
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.prune_target",
            &[("kind", &kind), ("name", name)]
        )
    )?;
    debug_assert_eq!(
        preview.warning,
        NativePruneWarning::MayRemoveStateCreatedOutsideOsdk
    );
    writeln!(output, "{}", trl(lang, "msg.container.prune_warning"))?;
    Ok(())
}

fn write_unsupported_prune(
    output: &mut dyn Write,
    unsupported: &UnsupportedPrune,
    lang: Lang,
) -> std::io::Result<()> {
    debug_assert_eq!(unsupported.owner, NativeCacheOwner::Containerd);
    writeln!(output, "{}", trl(lang, "msg.container.prune_unsupported"))
}

fn prune_owner_label(lang: Lang, owner: NativeCacheOwner) -> String {
    trl(
        lang,
        match owner {
            NativeCacheOwner::DockerEngine => "label.container.prune_owner.docker_engine",
            NativeCacheOwner::BuildkitBuilder => "label.container.prune_owner.buildkit_builder",
            NativeCacheOwner::Containerd => "label.container.prune_owner.containerd",
        },
    )
}

fn prune_scope_label(lang: Lang, scope: NativePruneScope) -> String {
    trl(
        lang,
        match scope {
            NativePruneScope::DanglingImages => "label.container.prune_scope.dangling_images",
            NativePruneScope::BuildCache => "label.container.prune_scope.build_cache",
        },
    )
}

async fn registry_test<T: RegistryTransport>(
    transport: &T,
    config: &osdk_core::config::ContainersConfig,
    raw_registry: &str,
    image: Option<ImageReference>,
    platform: Option<OciPlatform>,
) -> Result<RegistryDiagnosticReport> {
    let registry = RegistryName::parse(raw_registry)
        .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_registry")))?;
    if image
        .as_ref()
        .is_some_and(|image| image.registry() != &registry)
    {
        return Err(anyhow!(osdk_core::t!(
            "err.container.image_registry_mismatch",
            registry = registry
        )));
    }
    let upstream = RegistryEndpoint::for_registry(registry.clone())
        .context("constructing upstream registry endpoint")?;
    let mirrors = config
        .registries
        .get(registry.as_str())
        .into_iter()
        .flat_map(|policy| policy.mirrors.iter())
        .map(|mirror| RegistryEndpoint::parse_https(mirror))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("constructing configured registry mirror endpoints")?;
    let mut options = RegistryDiagnosticOptions::new(upstream)
        .with_mirrors(mirrors)
        .with_limits(registry_limits(config.probe_timeout_ms));
    if let Some(image) = image {
        options = options.with_image(image);
    }
    let platform = match platform {
        Some(platform) => Some(platform),
        None => configured_platform(config)?,
    };
    if let Some(platform) = platform {
        options = options.with_platform(platform);
    }
    diagnose_registry(transport, options)
        .await
        .context("running anonymous container registry diagnostic")
}

fn registry_limits(probe_timeout_ms: u64) -> RegistryLimits {
    let request_timeout = Duration::from_millis(probe_timeout_ms).min(Duration::from_secs(60));
    let total_timeout = request_timeout
        .saturating_mul(DEFAULT_MAX_REQUESTS as u32)
        .min(Duration::from_secs(5 * 60));
    RegistryLimits {
        request_timeout,
        total_timeout,
        ..RegistryLimits::default()
    }
}

fn configured_platform(
    config: &osdk_core::config::ContainersConfig,
) -> Result<Option<OciPlatform>> {
    match &config.platform {
        ContainerPlatform::Runtime => Ok(None),
        ContainerPlatform::Explicit { os, arch, variant } => {
            OciPlatform::new(os, arch, variant.as_deref())
                .map(Some)
                .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_platform")))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn mirror_plan(
    runner: &dyn CommandRunner,
    config: &osdk_core::config::ContainersConfig,
    raw_registry: &str,
    runtime: ContainerMirrorRuntimeArg,
    builder: Option<BuildxBuilderSelector>,
    native_config: Option<&Path>,
    containerd_main_config: Option<&Path>,
) -> Result<MirrorPlan> {
    let registry = RegistryName::parse(raw_registry)
        .map_err(|_| anyhow!(osdk_core::t!("err.container.invalid_registry")))?;
    let policy = registry_policy(config, &registry)?;
    let limits = capture_limits(config.probe_timeout_ms);

    if runtime != ContainerMirrorRuntimeArg::Buildkit && builder.is_some() {
        return Err(anyhow!(osdk_core::t!("err.container.builder_runtime")));
    }
    if runtime != ContainerMirrorRuntimeArg::Containerd && containerd_main_config.is_some() {
        return Err(anyhow!(osdk_core::t!(
            "err.container.containerd_main_config_runtime"
        )));
    }

    let bundle = match runtime {
        ContainerMirrorRuntimeArg::Docker => {
            let discovery = DockerAdapter.discover(runner, limits);
            let snapshot = native_config.map(capture_native_config).transpose()?;
            plan_docker_mirrors(DockerMirrorPlanRequest {
                registry: &registry,
                policy,
                discovery: &discovery,
                native_config: snapshot.as_ref(),
            })?
        }
        ContainerMirrorRuntimeArg::Containerd => {
            let discovery = ContainerdAdapter::default().inspect(runner, limits);
            if containerd_main_config.is_some()
                && discovery
                    .config
                    .as_ref()
                    .and_then(|config| config.registry_config_path.as_ref())
                    .is_some()
            {
                return Err(anyhow!(osdk_core::t!(
                    "err.container.containerd_main_config_already_configured"
                )));
            }
            let snapshot = native_config.map(capture_native_config).transpose()?;
            let main_snapshot = containerd_main_config
                .map(capture_native_config)
                .transpose()?;
            plan_containerd_mirrors(ContainerdMirrorPlanRequest {
                registry: &registry,
                policy,
                discovery: &discovery,
                hosts_config: snapshot.as_ref(),
                main_config: main_snapshot.as_ref(),
            })?
        }
        ContainerMirrorRuntimeArg::Buildkit => {
            let builder = builder.unwrap_or_else(|| config.builder.clone());
            let discovery = BuildkitAdapter::new(builder).inspect(runner, limits);
            let snapshot = native_config.map(capture_native_config).transpose()?;
            plan_buildkit_mirrors(BuildkitMirrorPlanRequest {
                registry: &registry,
                policy,
                discovery: &discovery,
                native_config: snapshot.as_ref(),
            })?
        }
    };
    Ok(bundle.plan)
}

fn capture_native_config(path: &Path) -> Result<NativeConfigSnapshot> {
    NativeConfigSnapshot::capture(path, MAX_NATIVE_CONFIG_BYTES).with_context(|| {
        osdk_core::t!(
            "err.container.native_config_snapshot",
            path = path.display()
        )
    })
}

fn registry_policy<'a>(
    config: &'a osdk_core::config::ContainersConfig,
    registry: &RegistryName,
) -> Result<&'a ContainerRegistryConfig> {
    config.registries.get(registry.as_str()).ok_or_else(|| {
        anyhow!(osdk_core::t!(
            "err.container.registry_not_configured",
            registry = registry
        ))
    })
}

fn capture_limits(timeout_ms: u64) -> CaptureLimits {
    CaptureLimits::new(
        Duration::from_millis(timeout_ms),
        CAPTURE_BYTES,
        CAPTURE_BYTES,
    )
}

fn doctor(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    requested: RuntimeSelection,
    builder: BuildxBuilderSelector,
    builder_explicit: bool,
) -> DoctorOutput {
    let (runtime, attempted_runtimes) = match requested {
        RuntimeSelection::Docker => {
            let report = docker_diagnostic(runner, limits);
            (report.clone(), vec![report])
        }
        RuntimeSelection::Containerd => {
            let report = containerd_diagnostic(runner, limits);
            (report.clone(), vec![report])
        }
        RuntimeSelection::Auto => {
            // Probe order and tie-breaking are stable. Selection depends on
            // diagnosed status, never binary presence.
            let docker = docker_diagnostic(runner, limits);
            let containerd = containerd_diagnostic(runner, limits);
            let selected = if status_rank(docker.status) >= status_rank(containerd.status) {
                docker.clone()
            } else {
                containerd.clone()
            };
            (selected, vec![docker, containerd])
        }
    };

    let inspect_builder = requested != RuntimeSelection::Containerd || builder_explicit;
    let builder = inspect_builder.then(|| BuildkitAdapter::new(builder).diagnose(runner, limits));

    DoctorOutput {
        schema_version: DOCTOR_SCHEMA_VERSION,
        requested_runtime: requested,
        selected_runtime: runtime.runtime,
        runtime,
        attempted_runtimes,
        builder,
    }
}

fn docker_diagnostic(runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport {
    DockerAdapter.diagnose(runner, limits)
}

fn containerd_diagnostic(runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport {
    ContainerdAdapter::default().diagnose(runner, limits)
}

fn cache_status(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    requested: ContainerCacheRuntimeArg,
    builder: BuildxBuilderSelector,
) -> NativeCacheStatus {
    match requested {
        ContainerCacheRuntimeArg::Docker => DockerCacheQuery.query(runner, limits),
        ContainerCacheRuntimeArg::Containerd => ContainerdCacheQuery.query(runner, limits),
        ContainerCacheRuntimeArg::Buildkit => BuildxCacheQuery::new(builder).query(runner, limits),
        ContainerCacheRuntimeArg::Auto => {
            let docker = DockerAdapter.diagnose(runner, limits);
            let containerd = ContainerdAdapter::default().diagnose(runner, limits);
            if status_rank(docker.status) >= status_rank(containerd.status) {
                DockerCacheQuery.query(runner, limits)
            } else {
                ContainerdCacheQuery.query(runner, limits)
            }
        }
    }
}

fn status_rank(status: DiagnosticStatus) -> u8 {
    match status {
        DiagnosticStatus::Healthy => 7,
        DiagnosticStatus::Degraded => 6,
        DiagnosticStatus::ClientOnly => 5,
        DiagnosticStatus::PermissionDenied => 4,
        DiagnosticStatus::Unreachable => 3,
        DiagnosticStatus::UnsupportedVersion => 2,
        DiagnosticStatus::NotInstalled => 1,
    }
}

fn write_doctor_human(
    output: &mut dyn Write,
    report: &DoctorOutput,
    lang: Lang,
) -> std::io::Result<()> {
    let runtime = runtime_label(report.selected_runtime);
    let status = diagnostic_status_label(lang, report.runtime.status);
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.doctor_conclusion",
            &[("runtime", runtime), ("status", &status)]
        )
    )?;
    write_diagnostic_details(output, &report.runtime, lang, "")?;
    if report.attempted_runtimes.len() > 1 {
        for attempted in &report.attempted_runtimes {
            writeln!(
                output,
                "  {}: {}",
                runtime_label(attempted.runtime),
                diagnostic_status_label(lang, attempted.status)
            )?;
            if attempted.runtime != report.selected_runtime {
                write_diagnostic_details(output, attempted, lang, "    ")?;
            }
        }
    }
    if let Some(builder) = &report.builder {
        writeln!(
            output,
            "{}",
            localized(
                lang,
                "msg.container.builder_status",
                &[("status", &diagnostic_status_label(lang, builder.status))]
            )
        )?;
        write_diagnostic_details(output, builder, lang, "  ")?;
    }
    Ok(())
}

fn write_diagnostic_details(
    output: &mut dyn Write,
    report: &DiagnosticReport,
    lang: Lang,
    indent: &str,
) -> std::io::Result<()> {
    let Some(details) = &report.details else {
        return Ok(());
    };
    let mut facts = 0usize;
    match details {
        DiagnosticDetails::Docker(details) => {
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.client_version",
                details.client_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.server_version",
                details.server_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.context_kind",
                details
                    .context_kind
                    .map(|kind| docker_context_kind_label(lang, kind))
                    .as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.daemon_os",
                details
                    .daemon_os
                    .map(|value| docker_daemon_os_label(lang, value))
                    .as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.daemon_architecture",
                details
                    .daemon_architecture
                    .map(|value| docker_daemon_architecture_label(lang, value))
                    .as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.rootless",
                details
                    .rootless
                    .map(|value| boolean_label(lang, value))
                    .as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.desktop",
                details
                    .desktop
                    .map(|value| boolean_label(lang, value))
                    .as_deref(),
            )?;
            for (index, mirror) in details.registry_mirror_origins.iter().enumerate() {
                writeln!(
                    output,
                    "{indent}  {}",
                    localized(
                        lang,
                        "msg.container.doctor.mirror",
                        &[
                            ("order", &(index + 1).to_string()),
                            ("origin", mirror.as_str()),
                        ],
                    )
                )?;
                facts += 1;
            }
        }
        DiagnosticDetails::Containerd(details) => {
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.containerd_version",
                details.containerd_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.ctr_client_version",
                details.ctr_client_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.ctr_server_version",
                details.ctr_server_version.as_deref(),
            )?;
            let config_version = details.config_version.map(|value| value.to_string());
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.config_version",
                config_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.config_path_configured",
                details
                    .config_path_configured
                    .map(|value| boolean_label(lang, value))
                    .as_deref(),
            )?;
            if !details.legacy_registry_settings.is_empty() {
                let values = details
                    .legacy_registry_settings
                    .iter()
                    .map(|warning| legacy_registry_label(lang, *warning))
                    .collect::<Vec<_>>()
                    .join(", ");
                write_fact(
                    output,
                    lang,
                    indent,
                    "label.container.doctor.legacy_registry",
                    &values,
                )?;
                facts += 1;
            }
        }
        DiagnosticDetails::Buildkit(details) => {
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.buildx_version",
                details.buildx_version.as_deref(),
            )?;
            facts += write_optional_fact(
                output,
                lang,
                indent,
                "label.container.doctor.driver",
                details
                    .driver
                    .as_ref()
                    .map(|driver| builder_driver_label(lang, driver))
                    .as_deref(),
            )?;
            for node in &details.nodes {
                writeln!(
                    output,
                    "{indent}  {}",
                    localized(
                        lang,
                        "msg.container.doctor.node",
                        &[
                            ("ordinal", &(node.ordinal + 1).to_string()),
                            ("status", &builder_node_status_label(lang, &node.status)),
                        ],
                    )
                )?;
                facts += 1;
                let node_indent = format!("{indent}    ");
                facts += write_optional_fact(
                    output,
                    lang,
                    &node_indent,
                    "label.container.doctor.node_version",
                    node.version.as_deref(),
                )?;
                facts += write_optional_fact(
                    output,
                    lang,
                    &node_indent,
                    "label.container.doctor.endpoint",
                    node.endpoint.as_ref().map(|endpoint| endpoint.as_str()),
                )?;
                if !node.platforms.is_empty() {
                    write_fact(
                        output,
                        lang,
                        &node_indent,
                        "label.container.doctor.platforms",
                        &node.platforms.join(", "),
                    )?;
                    facts += 1;
                }
            }
        }
    }
    if facts == 0 {
        writeln!(
            output,
            "{indent}  {}",
            trl(lang, "msg.container.doctor.details_unavailable")
        )?;
    }
    Ok(())
}

fn write_optional_fact(
    output: &mut dyn Write,
    lang: Lang,
    indent: &str,
    label_key: &str,
    value: Option<&str>,
) -> std::io::Result<usize> {
    let Some(value) = value else {
        return Ok(0);
    };
    write_fact(output, lang, indent, label_key, value)?;
    Ok(1)
}

fn write_fact(
    output: &mut dyn Write,
    lang: Lang,
    indent: &str,
    label_key: &str,
    value: &str,
) -> std::io::Result<()> {
    writeln!(output, "{indent}  {}: {value}", trl(lang, label_key))
}

fn write_cache_human(
    output: &mut dyn Write,
    status: &NativeCacheStatus,
    lang: Lang,
) -> std::io::Result<()> {
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.cache_conclusion",
            &[
                ("runtime", runtime_label(status.runtime)),
                ("status", &cache_status_label(lang, status.status)),
            ]
        )
    )?;
    if status.status == CacheQueryStatus::Available {
        writeln!(
            output,
            "{}",
            localized(
                lang,
                "msg.container.cache_totals",
                &[
                    ("total", &format_bytes(status.total)),
                    ("reclaimable", &format_bytes(status.reclaimable)),
                ]
            )
        )?;
        for record in &status.records {
            writeln!(
                output,
                "  {}: {}",
                cache_record_label(lang, record.kind),
                localized(
                    lang,
                    "msg.container.cache_record",
                    &[
                        ("count", &record.count.to_string()),
                        ("active", &record.active.to_string()),
                        ("total", &format_bytes(record.total)),
                        ("reclaimable", &format_bytes(record.reclaimable)),
                    ]
                )
            )?;
        }
    } else if status.status == CacheQueryStatus::Unsupported {
        writeln!(
            output,
            "{}",
            trl(lang, "msg.container.cache_unsupported_hint")
        )?;
    }
    Ok(())
}

fn write_registry_human(
    output: &mut dyn Write,
    report: &RegistryDiagnosticReport,
    lang: Lang,
) -> std::io::Result<()> {
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.registry_conclusion",
            &[
                ("registry", report.upstream.as_str()),
                ("status", &registry_status_label(lang, report.status)),
            ]
        )
    )?;
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.registry_api",
            &[("status", &api_status_label(lang, report.api.status))]
        )
    )?;
    if report.image.is_some() {
        writeln!(
            output,
            "{}",
            localized(
                lang,
                "msg.container.registry_manifest",
                &[(
                    "status",
                    &manifest_status_label(lang, report.manifest.status),
                )]
            )
        )?;
    }
    for mirror in &report.mirrors {
        writeln!(
            output,
            "  {}",
            localized(
                lang,
                "msg.container.registry_mirror",
                &[
                    ("order", &(mirror.order + 1).to_string()),
                    ("origin", &mirror.origin),
                    ("status", &mirror_status_label(lang, mirror.status)),
                ]
            )
        )?;
    }
    Ok(())
}

fn write_mirror_plan_human(
    output: &mut dyn Write,
    plan: &MirrorPlan,
    lang: Lang,
) -> std::io::Result<()> {
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.mirror_plan_id",
            &[("plan_id", plan.plan_id.as_str())]
        )
    )?;
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.mirror_plan_conclusion",
            &[
                ("runtime", mirror_plan_runtime(plan)),
                (
                    "applicability",
                    &plan_applicability_label(lang, plan.applicability),
                ),
            ]
        )
    )?;
    writeln!(
        output,
        "{}",
        localized(
            lang,
            "msg.container.mirror_plan_summary",
            &[
                ("changes", &plan.changes.len().to_string()),
                ("candidates", &plan.candidates.len().to_string()),
                ("activation", &activation_label(lang, plan.activation)),
            ]
        )
    )?;
    for warning in &plan.warnings {
        writeln!(
            output,
            "  {}: {}",
            trl(lang, "label.container.warning"),
            plan_warning_label(lang, *warning)
        )?;
    }
    Ok(())
}

fn api_status_label(lang: Lang, status: ApiCheckStatus) -> String {
    trl(
        lang,
        registry_check_label_key(match status {
            ApiCheckStatus::Available => "available",
            ApiCheckStatus::BearerChallenge => "bearer_challenge",
            ApiCheckStatus::AuthenticationRequired => "authentication_required",
            ApiCheckStatus::AccessDenied => "access_denied",
            ApiCheckStatus::RateLimited => "rate_limited",
            ApiCheckStatus::NotFound => "not_found",
            ApiCheckStatus::ServerError => "server_error",
            ApiCheckStatus::RedirectRejected => "redirect_rejected",
            ApiCheckStatus::InvalidChallenge => "invalid_challenge",
            ApiCheckStatus::UnexpectedResponse => "unexpected_response",
            ApiCheckStatus::Unreachable => "unreachable",
            ApiCheckStatus::TimedOut => "timed_out",
            ApiCheckStatus::BodyTooLarge => "body_too_large",
            ApiCheckStatus::RequestLimit => "request_limit",
            ApiCheckStatus::NotTested => "not_tested",
        }),
    )
}

fn manifest_status_label(lang: Lang, status: ManifestCheckStatus) -> String {
    trl(
        lang,
        registry_check_label_key(match status {
            ManifestCheckStatus::Verified => "verified",
            ManifestCheckStatus::AuthenticationRequired => "authentication_required",
            ManifestCheckStatus::AccessDenied => "access_denied",
            ManifestCheckStatus::RateLimited => "rate_limited",
            ManifestCheckStatus::NotFound => "not_found",
            ManifestCheckStatus::ServerError => "server_error",
            ManifestCheckStatus::RedirectRejected => "redirect_rejected",
            ManifestCheckStatus::InvalidMediaType => "invalid_media_type",
            ManifestCheckStatus::InvalidManifest => "invalid_manifest",
            ManifestCheckStatus::DigestMismatch => "digest_mismatch",
            ManifestCheckStatus::SizeMismatch => "size_mismatch",
            ManifestCheckStatus::BodyTooLarge => "body_too_large",
            ManifestCheckStatus::PlatformNotFound => "platform_not_found",
            ManifestCheckStatus::Unreachable => "unreachable",
            ManifestCheckStatus::TimedOut => "timed_out",
            ManifestCheckStatus::RequestLimit => "request_limit",
            ManifestCheckStatus::NotRequested => "not_requested",
            ManifestCheckStatus::NotTested => "not_tested",
        }),
    )
}

fn mirror_status_label(lang: Lang, status: MirrorCheckStatus) -> String {
    trl(
        lang,
        registry_check_label_key(match status {
            MirrorCheckStatus::Available => "available",
            MirrorCheckStatus::Equivalent => "equivalent",
            MirrorCheckStatus::Diverged => "diverged",
            MirrorCheckStatus::AuthenticationRequired => "authentication_required",
            MirrorCheckStatus::AccessDenied => "access_denied",
            MirrorCheckStatus::RateLimited => "rate_limited",
            MirrorCheckStatus::NotFound => "not_found",
            MirrorCheckStatus::ServerError => "server_error",
            MirrorCheckStatus::RedirectRejected => "redirect_rejected",
            MirrorCheckStatus::InvalidResponse => "invalid_response",
            MirrorCheckStatus::Unreachable => "unreachable",
            MirrorCheckStatus::TimedOut => "timed_out",
            MirrorCheckStatus::BodyTooLarge => "body_too_large",
            MirrorCheckStatus::RequestLimit => "request_limit",
            MirrorCheckStatus::NotTested => "not_tested",
        }),
    )
}

fn registry_check_label_key(name: &str) -> &'static str {
    match name {
        "available" => "label.container.registry_check.available",
        "bearer_challenge" => "label.container.registry_check.bearer_challenge",
        "authentication_required" => "label.container.registry_check.authentication_required",
        "access_denied" => "label.container.registry_check.access_denied",
        "rate_limited" => "label.container.registry_check.rate_limited",
        "not_found" => "label.container.registry_check.not_found",
        "server_error" => "label.container.registry_check.server_error",
        "redirect_rejected" => "label.container.registry_check.redirect_rejected",
        "invalid_challenge" => "label.container.registry_check.invalid_challenge",
        "unexpected_response" => "label.container.registry_check.unexpected_response",
        "unreachable" => "label.container.registry_check.unreachable",
        "timed_out" => "label.container.registry_check.timed_out",
        "body_too_large" => "label.container.registry_check.body_too_large",
        "request_limit" => "label.container.registry_check.request_limit",
        "not_tested" => "label.container.registry_check.not_tested",
        "verified" => "label.container.registry_check.verified",
        "invalid_media_type" => "label.container.registry_check.invalid_media_type",
        "invalid_manifest" => "label.container.registry_check.invalid_manifest",
        "digest_mismatch" => "label.container.registry_check.digest_mismatch",
        "size_mismatch" => "label.container.registry_check.size_mismatch",
        "platform_not_found" => "label.container.registry_check.platform_not_found",
        "not_requested" => "label.container.registry_check.not_requested",
        "equivalent" => "label.container.registry_check.equivalent",
        "diverged" => "label.container.registry_check.diverged",
        "invalid_response" => "label.container.registry_check.invalid_response",
        _ => unreachable!("all registry status labels are explicit"),
    }
}

fn activation_label(lang: Lang, activation: ActivationRequirement) -> String {
    trl(
        lang,
        match activation {
            ActivationRequirement::None => "label.container.activation.none",
            ActivationRequirement::RestartDaemon => "label.container.activation.restart_daemon",
            ActivationRequirement::RecreateBuilder => "label.container.activation.recreate_builder",
        },
    )
}

fn plan_warning_label(lang: Lang, warning: PlanWarning) -> String {
    trl(
        lang,
        match warning {
            PlanWarning::AnonymousOnlyNotEnforced => {
                "label.container.plan_warning.anonymous_only_not_enforced"
            }
            PlanWarning::DockerHubOnly => "label.container.plan_warning.docker_hub_only",
            PlanWarning::ResolutionSeparationUnavailable => {
                "label.container.plan_warning.resolution_separation_unavailable"
            }
            PlanWarning::RemoteTarget => "label.container.plan_warning.remote_target",
            PlanWarning::ManagedDesktop => "label.container.plan_warning.managed_desktop",
            PlanWarning::NativeConfigPathRequired => {
                "label.container.plan_warning.native_config_path_required"
            }
            PlanWarning::ContainerdConfigPathMissing => {
                "label.container.plan_warning.containerd_config_path_missing"
            }
            PlanWarning::ExistingNativeEntriesPreserved => {
                "label.container.plan_warning.existing_native_entries_preserved"
            }
            PlanWarning::DaemonRestartRequired => {
                "label.container.plan_warning.daemon_restart_required"
            }
            PlanWarning::BuilderRecreateRequired => {
                "label.container.plan_warning.builder_recreate_required"
            }
            PlanWarning::DockerDriverUsesEngineConfiguration => {
                "label.container.plan_warning.docker_driver_uses_engine_configuration"
            }
            PlanWarning::ExternalBuilderConfiguration => {
                "label.container.plan_warning.external_builder_configuration"
            }
        },
    )
}

fn registry_status_label(lang: Lang, status: RegistryDiagnosticStatus) -> String {
    trl(
        lang,
        match status {
            RegistryDiagnosticStatus::Healthy => "label.container.healthy",
            RegistryDiagnosticStatus::Degraded => "label.container.degraded",
            RegistryDiagnosticStatus::AuthenticationRequired => {
                "label.container.registry.authentication_required"
            }
            RegistryDiagnosticStatus::AccessDenied => "label.container.registry.access_denied",
            RegistryDiagnosticStatus::RateLimited => "label.container.registry.rate_limited",
            RegistryDiagnosticStatus::NotFound => "label.container.registry.not_found",
            RegistryDiagnosticStatus::Unreachable => "label.container.unreachable",
            RegistryDiagnosticStatus::TimedOut => "label.container.registry.timed_out",
            RegistryDiagnosticStatus::ProtocolError => "label.container.registry.protocol_error",
            RegistryDiagnosticStatus::Corrupt => "label.container.registry.corrupt",
            RegistryDiagnosticStatus::Unsupported => "label.container.cache.unsupported",
            RegistryDiagnosticStatus::LimitExceeded => "label.container.registry.limit_exceeded",
        },
    )
}

fn plan_applicability_label(lang: Lang, applicability: PlanApplicability) -> String {
    trl(
        lang,
        match applicability {
            PlanApplicability::Ready => "label.container.plan.ready",
            PlanApplicability::ManualOnly => "label.container.plan.manual_only",
            PlanApplicability::Unsupported => "label.container.cache.unsupported",
        },
    )
}

fn mirror_plan_runtime(plan: &MirrorPlan) -> &'static str {
    match &plan.target {
        osdk_core::container::MirrorPlanTarget::Docker { .. } => "docker",
        osdk_core::container::MirrorPlanTarget::Containerd { .. } => "containerd",
        osdk_core::container::MirrorPlanTarget::Buildkit { .. } => "buildkit",
    }
}

fn runtime_label(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Docker => "docker",
        RuntimeKind::Containerd => "containerd",
        RuntimeKind::Buildkit => "buildkit",
        RuntimeKind::Podman => "podman",
    }
}

fn localized(lang: Lang, key: &str, args: &[(&str, &str)]) -> String {
    interpolate(&trl(lang, key), args)
}

fn diagnostic_status_label(lang: Lang, status: DiagnosticStatus) -> String {
    let key = match status {
        DiagnosticStatus::Healthy => "label.container.healthy",
        DiagnosticStatus::Degraded => "label.container.degraded",
        DiagnosticStatus::NotInstalled => "label.container.not_installed",
        DiagnosticStatus::ClientOnly => "label.container.client_only",
        DiagnosticStatus::Unreachable => "label.container.unreachable",
        DiagnosticStatus::PermissionDenied => "label.container.permission_denied",
        DiagnosticStatus::UnsupportedVersion => "label.container.unsupported_version",
    };
    trl(lang, key)
}

fn boolean_label(lang: Lang, value: bool) -> String {
    trl(
        lang,
        if value {
            "label.container.yes"
        } else {
            "label.container.no"
        },
    )
}

fn docker_context_kind_label(lang: Lang, kind: DockerContextKind) -> String {
    trl(
        lang,
        match kind {
            DockerContextKind::Local => "label.container.context.local",
            DockerContextKind::Remote => "label.container.context.remote",
            DockerContextKind::Rootless => "label.container.context.rootless",
            DockerContextKind::Desktop => "label.container.context.desktop",
            DockerContextKind::Unknown => "label.container.unknown",
        },
    )
}

fn docker_daemon_os_label(lang: Lang, value: DockerDaemonOs) -> String {
    match value {
        DockerDaemonOs::Linux => "linux".to_owned(),
        DockerDaemonOs::Windows => "windows".to_owned(),
        DockerDaemonOs::Unknown => trl(lang, "label.container.unknown"),
    }
}

fn docker_daemon_architecture_label(lang: Lang, value: DockerDaemonArchitecture) -> String {
    match value {
        DockerDaemonArchitecture::Amd64 => "amd64".to_owned(),
        DockerDaemonArchitecture::Arm64 => "arm64".to_owned(),
        DockerDaemonArchitecture::Arm => "arm".to_owned(),
        DockerDaemonArchitecture::I386 => "386".to_owned(),
        DockerDaemonArchitecture::Ppc64le => "ppc64le".to_owned(),
        DockerDaemonArchitecture::S390x => "s390x".to_owned(),
        DockerDaemonArchitecture::Riscv64 => "riscv64".to_owned(),
        DockerDaemonArchitecture::Unknown => trl(lang, "label.container.unknown"),
    }
}

fn builder_driver_label(lang: Lang, driver: &BuilderDriver) -> String {
    trl(
        lang,
        match driver {
            BuilderDriver::Docker => "label.container.driver.docker",
            BuilderDriver::DockerContainer => "label.container.driver.docker_container",
            BuilderDriver::Kubernetes => "label.container.driver.kubernetes",
            BuilderDriver::Remote => "label.container.driver.remote",
            BuilderDriver::Cloud => "label.container.driver.cloud",
            BuilderDriver::Unknown => "label.container.unknown",
        },
    )
}

fn builder_node_status_label(lang: Lang, status: &BuilderNodeStatus) -> String {
    trl(
        lang,
        match status {
            BuilderNodeStatus::Running => "label.container.node.running",
            BuilderNodeStatus::Stopped => "label.container.node.stopped",
            BuilderNodeStatus::Inactive => "label.container.node.inactive",
            BuilderNodeStatus::Error => "label.container.node.error",
            BuilderNodeStatus::Unknown => "label.container.unknown",
        },
    )
}

fn legacy_registry_label(lang: Lang, warning: LegacyRegistryWarning) -> String {
    trl(
        lang,
        match warning {
            LegacyRegistryWarning::Auths => "label.container.legacy.auths",
            LegacyRegistryWarning::Configs => "label.container.legacy.configs",
            LegacyRegistryWarning::Mirrors => "label.container.legacy.mirrors",
        },
    )
}

fn cache_status_label(lang: Lang, status: CacheQueryStatus) -> String {
    trl(
        lang,
        match status {
            CacheQueryStatus::Available => "label.container.cache.available",
            CacheQueryStatus::NotInstalled => "label.container.not_installed",
            CacheQueryStatus::PermissionDenied => "label.container.permission_denied",
            CacheQueryStatus::Unreachable => "label.container.unreachable",
            CacheQueryStatus::TimedOut => "label.container.cache.timed_out",
            CacheQueryStatus::Unsupported => "label.container.cache.unsupported",
            CacheQueryStatus::UnsupportedVersion => "label.container.unsupported_version",
            CacheQueryStatus::OutputTruncated => "label.container.cache.output_truncated",
            CacheQueryStatus::InvalidOutput => "label.container.cache.invalid_output",
            CacheQueryStatus::CommandFailed => "label.container.cache.command_failed",
        },
    )
}

fn cache_record_label(lang: Lang, kind: NativeCacheRecordKind) -> String {
    trl(
        lang,
        match kind {
            NativeCacheRecordKind::Images => "label.container.cache.images",
            NativeCacheRecordKind::Containers => "label.container.cache.containers",
            NativeCacheRecordKind::LocalVolumes => "label.container.cache.local_volumes",
            NativeCacheRecordKind::BuildCache => "label.container.cache.build_cache",
        },
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl From<ContainerRuntimeArg> for RuntimeSelection {
    fn from(value: ContainerRuntimeArg) -> Self {
        match value {
            ContainerRuntimeArg::Auto => Self::Auto,
            ContainerRuntimeArg::Docker => Self::Docker,
            ContainerRuntimeArg::Containerd => Self::Containerd,
        }
    }
}

impl From<ContainerRuntime> for RuntimeSelection {
    fn from(value: ContainerRuntime) -> Self {
        match value {
            ContainerRuntime::Auto => Self::Auto,
            ContainerRuntime::Docker => Self::Docker,
            ContainerRuntime::Containerd => Self::Containerd,
        }
    }
}

impl From<ContainerRuntime> for ContainerCacheRuntimeArg {
    fn from(value: ContainerRuntime) -> Self {
        match value {
            ContainerRuntime::Auto => Self::Auto,
            ContainerRuntime::Docker => Self::Docker,
            ContainerRuntime::Containerd => Self::Containerd,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::process::ExitStatus;
    use std::sync::{Arc, Mutex};

    use osdk_core::container::RegistryRequest;
    use osdk_core::i18n::Lang;
    use osdk_core::process::{CapturedOutput, CommandOutcome, CommandSpec};

    use super::*;

    #[cfg(not(windows))]
    const DEFAULT_TEST_CONTAINERD_ADDRESS: &str = "unix:///run/containerd/containerd.sock";
    #[cfg(windows)]
    const DEFAULT_TEST_CONTAINERD_ADDRESS: &str = "npipe:////./pipe/containerd-containerd";
    const DEFAULT_CONTAINERD_NAMESPACE: &str = "default";

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        program: String,
        arguments: Vec<String>,
        limits: Option<CaptureLimits>,
    }

    struct FakeRunner {
        outcomes: Mutex<VecDeque<CommandOutcome>>,
        calls: Mutex<Vec<Call>>,
        foreground_status: ExitStatus,
    }

    impl FakeRunner {
        fn new(outcomes: impl IntoIterator<Item = CommandOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                foreground_status: exit_status_code(0),
            }
        }

        fn with_foreground_status(
            outcomes: impl IntoIterator<Item = CommandOutcome>,
            code: i32,
        ) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                foreground_status: exit_status_code(code),
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
                limits: Some(limits),
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected native command")
        }

        fn run_foreground(&self, command: &CommandSpec) -> io::Result<ExitStatus> {
            self.calls.lock().unwrap().push(Call {
                program: command.program().to_string_lossy().into_owned(),
                arguments: command
                    .arguments()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                limits: None,
            });
            Ok(self.foreground_status)
        }
    }

    struct FakePrompt {
        answer: bool,
        questions: Mutex<Vec<String>>,
    }

    impl FakePrompt {
        fn accepting() -> Self {
            Self {
                answer: true,
                questions: Mutex::new(Vec::new()),
            }
        }

        fn questions(&self) -> Vec<String> {
            self.questions.lock().unwrap().clone()
        }
    }

    impl crate::prompt::Prompt for FakePrompt {
        fn confirm(&self, question: &str) -> Result<bool> {
            self.questions.lock().unwrap().push(question.to_owned());
            Ok(self.answer)
        }
    }

    fn success(stdout: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: exit_status(true),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
        }
    }

    fn failure(stderr: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: exit_status(false),
            output: CapturedOutput {
                stderr: stderr.as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(unix)]
    fn exit_status(success: bool) -> ExitStatus {
        exit_status_code(i32::from(!success))
    }

    #[cfg(unix)]
    fn exit_status_code(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(success: bool) -> ExitStatus {
        exit_status_code(i32::from(!success))
    }

    #[cfg(windows)]
    fn exit_status_code(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    fn healthy_docker() -> [CommandOutcome; 3] {
        [
            success(r#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"29.0.1"}}"#),
            success(
                r#"[{"Name":"secret-context","Endpoints":{"docker":{"Host":"ssh://alice:password@example.test/private?token=secret"}}}]"#,
            ),
            success(
                r#"{"OperatingSystem":"Linux","OSType":"linux","Architecture":"amd64","SecurityOptions":[],"RegistryConfig":{"Mirrors":[]}}"#,
            ),
        ]
    }

    fn healthy_local_docker() -> [CommandOutcome; 3] {
        [
            success(r#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"29.0.1"}}"#),
            success(
                r#"[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            ),
            success(
                r#"{"OperatingSystem":"Linux","OSType":"linux","Architecture":"amd64","SecurityOptions":[],"RegistryConfig":{"Mirrors":[]}}"#,
            ),
        ]
    }

    fn healthy_containerd() -> [CommandOutcome; 3] {
        [
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
            success("version = 2"),
        ]
    }

    fn healthy_buildkit(name: &str) -> [CommandOutcome; 3] {
        healthy_buildkit_at(name, "docker-container", "unix:///var/run/docker.sock")
    }

    fn healthy_buildkit_at(name: &str, driver: &str, endpoint: &str) -> [CommandOutcome; 3] {
        [
            success("github.com/docker/buildx v0.36.1 deadbeef\n"),
            success(&format!(
                r#"{{"Current":true,"Driver":"{driver}","Name":"{name}","Nodes":[{{"Name":"{name}0","Endpoint":"{endpoint}","Status":"running"}}]}}"#
            )),
            success(&format!(
                "Name: {name}\nDriver: {driver}\nName: {name}0\nEndpoint: {endpoint}\nStatus: running\n"
            )),
        ]
    }

    #[test]
    fn explicit_pull_runs_one_foreground_operation_and_returns_native_status() {
        let runner = FakeRunner::with_foreground_status([], 23);
        let status = pull(
            &runner,
            &Default::default(),
            false,
            Some(ContainerRuntimeArg::Docker),
            ImageReference::parse("ubuntu:24.04").unwrap(),
            Some(OciPlatform::parse("Linux/X64").unwrap()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(status.code(), Some(23));
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
        assert!(calls[0].limits.is_none());
    }

    #[test]
    fn auto_pull_resolves_once_then_launches_only_the_selected_runtime() {
        let runner = FakeRunner::with_foreground_status(
            [
                vec![CommandOutcome::NotInstalled],
                healthy_containerd().into_iter().collect(),
            ]
            .concat(),
            0,
        );
        let status = pull(
            &runner,
            &Default::default(),
            false,
            Some(ContainerRuntimeArg::Auto),
            ImageReference::parse("alpine:3").unwrap(),
            None,
            Some(DEFAULT_TEST_CONTAINERD_ADDRESS.into()),
            Some(DEFAULT_CONTAINERD_NAMESPACE.into()),
        )
        .unwrap();

        assert!(status.success());
        let calls = runner.calls();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0].program, "docker");
        assert_eq!(calls[1].program, "containerd");
        assert_eq!(calls.last().unwrap().program, "ctr");
        assert_eq!(
            calls.last().unwrap().arguments,
            [
                "--address",
                DEFAULT_TEST_CONTAINERD_ADDRESS,
                "--namespace",
                DEFAULT_CONTAINERD_NAMESPACE,
                "images",
                "pull",
                "docker.io/library/alpine:3",
            ]
        );
        assert_eq!(calls.iter().filter(|call| call.limits.is_none()).count(), 1);
    }

    #[test]
    fn pull_rejects_offline_before_any_probe_or_launch() {
        let runner = FakeRunner::new([]);
        let error = pull(
            &runner,
            &Default::default(),
            true,
            Some(ContainerRuntimeArg::Auto),
            ImageReference::parse("alpine:3").unwrap(),
            None,
            None,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("offline"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn containerd_pull_requires_explicit_target_after_auto_resolution() {
        let runner = FakeRunner::new(
            [
                vec![CommandOutcome::NotInstalled],
                healthy_containerd().into_iter().collect(),
            ]
            .concat(),
        );
        let error = pull(
            &runner,
            &Default::default(),
            false,
            Some(ContainerRuntimeArg::Auto),
            ImageReference::parse("alpine:3").unwrap(),
            None,
            None,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("--address"));
        assert_eq!(runner.calls().len(), 4);
        assert!(runner.calls().iter().all(|call| call.limits.is_some()));
    }

    #[test]
    fn docker_pull_rejects_containerd_selectors_without_launching() {
        let runner = FakeRunner::new([]);
        let error = pull(
            &runner,
            &Default::default(),
            false,
            Some(ContainerRuntimeArg::Docker),
            ImageReference::parse("alpine:3").unwrap(),
            None,
            Some(DEFAULT_TEST_CONTAINERD_ADDRESS.into()),
            Some(DEFAULT_CONTAINERD_NAMESPACE.into()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("containerd"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn docker_prune_defaults_to_preview_and_binds_discovered_context() {
        let runner = FakeRunner::new([success(
            r#"[{"Name":"team-context","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
        )]);
        let prompt = FakePrompt::accepting();
        let mut output = Vec::new();
        let status = native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            false,
            None,
            &mut output,
        )
        .unwrap();

        assert!(status.is_none());
        assert!(prompt.questions().is_empty());
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, ["context", "inspect", "team-context"]);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("sha256:"), "{text}");
        assert!(text.contains("team-context"), "{text}");
        assert!(text.contains("dangling images"), "{text}");
    }

    #[test]
    fn docker_prune_requires_exact_acceptance_then_prompts_and_runs_once() {
        fn discovery() -> CommandOutcome {
            success(
                r#"[{"Name":"team-context","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]"#,
            )
        }

        let preview_runner = FakeRunner::new([discovery()]);
        let prompt = FakePrompt::accepting();
        let mut preview_output = Vec::new();
        native_prune(
            &preview_runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            false,
            None,
            &mut preview_output,
        )
        .unwrap();
        let preview_output = String::from_utf8(preview_output).unwrap();
        let preview_id = preview_output
            .split_whitespace()
            .find(|value| value.starts_with("sha256:"))
            .unwrap()
            .to_owned();

        let mismatch_runner = FakeRunner::new([discovery()]);
        let mut mismatch_output = Vec::new();
        let mismatch = native_prune(
            &mismatch_runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            true,
            Some("sha256:wrong"),
            &mut mismatch_output,
        )
        .unwrap_err();
        assert!(mismatch.to_string().contains(&preview_id));
        assert_eq!(mismatch_runner.calls().len(), 1);
        assert!(prompt.questions().is_empty());

        let runner = FakeRunner::with_foreground_status([discovery(), discovery()], 41);
        let mut output = Vec::new();
        let status = native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            true,
            Some(&preview_id),
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(status.code(), Some(41));
        assert_eq!(prompt.questions().len(), 1);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].arguments,
            [
                "--host",
                "unix:///var/run/docker.sock",
                "image",
                "prune",
                "--force",
            ]
        );
        assert!(calls[1].limits.is_none());
    }

    #[test]
    fn docker_prune_executes_the_endpoint_captured_before_confirmation() {
        fn discovery(endpoint: &str) -> CommandOutcome {
            success(&format!(
                r#"[{{"Name":"team-context","Endpoints":{{"docker":{{"Host":"{endpoint}"}}}}}}]"#
            ))
        }

        let prompt = FakePrompt::accepting();
        let first = FakeRunner::new([discovery("unix:///run/first/docker.sock")]);
        let mut output = Vec::new();
        native_prune(
            &first,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            false,
            None,
            &mut output,
        )
        .unwrap();
        let preview_id = String::from_utf8(output)
            .unwrap()
            .split_whitespace()
            .find(|value| value.starts_with("sha256:"))
            .unwrap()
            .to_owned();

        let retargeted =
            FakeRunner::with_foreground_status([discovery("unix:///run/first/docker.sock")], 0);
        let mut output = Vec::new();
        let status = native_prune(
            &retargeted,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Docker,
            ContainerPruneScopeArg::Images,
            Some("team-context".into()),
            None,
            true,
            Some(&preview_id),
            &mut output,
        )
        .unwrap()
        .unwrap();

        assert!(status.success());
        assert_eq!(retargeted.calls().len(), 2);
        assert_eq!(
            retargeted.calls()[1].arguments,
            [
                "--host",
                "unix:///run/first/docker.sock",
                "image",
                "prune",
                "--force",
            ]
        );
        assert!(retargeted.calls()[1].limits.is_none());
        assert_eq!(prompt.questions().len(), 1);
    }

    #[test]
    fn buildkit_prune_execution_is_unsupported_before_prompt_or_launch() {
        let prompt = FakePrompt::accepting();
        let first = FakeRunner::new(healthy_buildkit_at(
            "team-builder",
            "docker-container",
            "unix:///run/first/buildkit.sock",
        ));
        let mut output = Vec::new();
        native_prune(
            &first,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Buildkit,
            ContainerPruneScopeArg::BuildCache,
            None,
            Some(BuildxBuilderSelector::named("team-builder").unwrap()),
            false,
            None,
            &mut output,
        )
        .unwrap();
        let preview_id = String::from_utf8(output)
            .unwrap()
            .split_whitespace()
            .find(|value| value.starts_with("sha256:"))
            .unwrap()
            .to_owned();

        let retargeted = FakeRunner::with_foreground_status(
            healthy_buildkit_at(
                "team-builder",
                "docker-container",
                "unix:///run/first/buildkit.sock",
            ),
            0,
        );
        let mut output = Vec::new();
        let error = native_prune(
            &retargeted,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Buildkit,
            ContainerPruneScopeArg::BuildCache,
            None,
            Some(BuildxBuilderSelector::named("team-builder").unwrap()),
            true,
            Some(&preview_id),
            &mut output,
        )
        .unwrap_err();

        assert!(error.to_string().contains("preview-only"));
        assert_eq!(retargeted.calls().len(), 3);
        assert!(retargeted.calls().iter().all(|call| call.limits.is_some()));
        assert!(prompt.questions().is_empty());
    }

    #[test]
    fn buildkit_preview_resolves_one_exact_builder_and_containerd_is_unsupported() {
        let runner = FakeRunner::new(healthy_buildkit("team-builder"));
        let prompt = FakePrompt::accepting();
        let mut output = Vec::new();
        let status = native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Buildkit,
            ContainerPruneScopeArg::BuildCache,
            Some("unexpected-context".into()),
            None,
            false,
            None,
            &mut output,
        )
        .unwrap_err();
        assert!(status.to_string().contains("--context"));
        assert!(runner.calls().is_empty());

        let runner = FakeRunner::new(healthy_buildkit("team-builder"));
        let mut output = Vec::new();
        native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Buildkit,
            ContainerPruneScopeArg::BuildCache,
            None,
            None,
            false,
            None,
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("team-builder"), "{text}");
        assert!(text.contains("unused build cache"), "{text}");
        assert_eq!(runner.calls().len(), 3);

        let runner = FakeRunner::new([]);
        let mut output = Vec::new();
        let unsupported = native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Containerd,
            ContainerPruneScopeArg::BuildCache,
            None,
            None,
            false,
            None,
            &mut output,
        )
        .unwrap_err();
        assert!(String::from_utf8(output).unwrap().contains("unsupported"));
        assert!(unsupported.to_string().contains("no stable aggregate"));
        assert!(runner.calls().is_empty());

        let runner = FakeRunner::new([]);
        let mut output = Vec::new();
        let execute = native_prune(
            &runner,
            &prompt,
            &Default::default(),
            ContainerPruneRuntimeArg::Containerd,
            ContainerPruneScopeArg::Images,
            None,
            None,
            true,
            Some("sha256:unused"),
            &mut output,
        )
        .unwrap_err();
        assert!(execute.to_string().contains("unsupported"));
        assert!(output.is_empty());
        assert!(runner.calls().is_empty());
    }

    #[derive(Clone)]
    struct FakeTransport {
        responses: Arc<Mutex<VecDeque<osdk_core::container::RegistryResponse>>>,
        requests: Arc<Mutex<Vec<RegistryRequest>>>,
    }

    impl FakeTransport {
        fn available(count: usize) -> Self {
            Self {
                responses: Arc::new(Mutex::new(
                    std::iter::repeat_with(|| osdk_core::container::RegistryResponse::new(200))
                        .take(count)
                        .collect(),
                )),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<RegistryRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl RegistryTransport for FakeTransport {
        fn execute(
            &self,
            request: osdk_core::container::RegistryRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = std::result::Result<
                            osdk_core::container::RegistryResponse,
                            osdk_core::container::RegistryTransportError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { Ok(response) })
        }
    }

    fn registry_policy(mirrors: &[&str]) -> ContainerRegistryConfig {
        ContainerRegistryConfig {
            mirrors: mirrors.iter().map(|value| (*value).to_owned()).collect(),
            ..ContainerRegistryConfig::default()
        }
    }

    #[test]
    fn registry_limits_honor_probe_timeout_with_hard_bounds() {
        let short = registry_limits(250);
        assert_eq!(short.request_timeout, Duration::from_millis(250));
        assert_eq!(short.total_timeout, Duration::from_secs(3));

        let long = registry_limits(90_000);
        assert_eq!(long.request_timeout, Duration::from_secs(60));
        assert_eq!(long.total_timeout, Duration::from_secs(5 * 60));
    }

    #[tokio::test]
    async fn registry_test_allows_upstream_only_and_inherits_explicit_platform() {
        let transport = FakeTransport::available(1);
        let config = osdk_core::config::ContainersConfig {
            platform: ContainerPlatform::Explicit {
                os: "linux".into(),
                arch: "x86_64".into(),
                variant: None,
            },
            ..Default::default()
        };
        let report = registry_test(&transport, &config, "docker.io", None, None)
            .await
            .unwrap();

        assert_eq!(report.status, RegistryDiagnosticStatus::Healthy);
        assert_eq!(
            report.requested_platform.as_ref().map(ToString::to_string),
            Some("linux/amd64".into())
        );
        assert!(report.mirrors.is_empty());
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(format!("{:?}", requests[0]).contains("registry-1.docker.io"));
    }

    #[tokio::test]
    async fn registry_test_preserves_configured_mirror_order() {
        let transport = FakeTransport::available(3);
        let mut config = osdk_core::config::ContainersConfig::default();
        config.registries.insert(
            "docker.io".into(),
            registry_policy(&["https://first.example/", "https://second.example/"]),
        );
        let report = registry_test(&transport, &config, "docker.io", None, None)
            .await
            .unwrap();

        assert_eq!(
            report
                .mirrors
                .iter()
                .map(|mirror| (mirror.order, mirror.origin.as_str()))
                .collect::<Vec<_>>(),
            [(0, "https://first.example"), (1, "https://second.example"),]
        );
        assert_eq!(transport.requests().len(), 3);
    }

    #[tokio::test]
    async fn offline_registry_test_rejects_before_transport_creation() {
        let runner = FakeRunner::new([]);
        let factory_calls = Arc::new(Mutex::new(0));
        let observed_calls = factory_calls.clone();
        let mut output = Vec::new();
        let error = run_with(
            &runner,
            move || {
                *observed_calls.lock().unwrap() += 1;
                Ok(FakeTransport::available(1))
            },
            &Default::default(),
            true,
            ContainerCommand::Registry {
                command: ContainerRegistryCommand::Test {
                    registry: "docker.io".into(),
                    image: None,
                    platform: None,
                    json: true,
                },
            },
            &mut output,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("offline"));
        assert_eq!(*factory_calls.lock().unwrap(), 0);
        assert!(runner.calls().is_empty());
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn doctor_does_not_construct_registry_transport() {
        let runner = FakeRunner::new(
            [
                vec![CommandOutcome::NotInstalled],
                vec![CommandOutcome::NotInstalled],
                vec![CommandOutcome::NotInstalled],
            ]
            .concat(),
        );
        let factory_calls = Arc::new(Mutex::new(0));
        let observed_calls = factory_calls.clone();
        let mut output = Vec::new();
        run_with(
            &runner,
            move || {
                *observed_calls.lock().unwrap() += 1;
                Ok(FakeTransport::available(1))
            },
            &Default::default(),
            false,
            ContainerCommand::Doctor {
                runtime: Some(ContainerRuntimeArg::Docker),
                builder: None,
                json: true,
            },
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(*factory_calls.lock().unwrap(), 0);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("\"selected_runtime\":\"docker\""));
    }

    #[test]
    fn docker_mirror_plan_is_one_registry_manual_without_path_and_secret_safe() {
        let mut config = osdk_core::config::ContainersConfig::default();
        config.registries.insert(
            "docker.io".into(),
            registry_policy(&["https://first.example/", "https://second.example/"]),
        );
        config.registries.insert(
            "ghcr.io".into(),
            registry_policy(&["https://must-not-appear.example/"]),
        );
        let runner = FakeRunner::new(healthy_local_docker());
        let plan = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Docker,
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(plan.applicability, PlanApplicability::ManualOnly);
        assert!(plan.candidates.is_empty());
        let json = serde_json::to_string(&plan).unwrap();
        let first = json.find("https://first.example/").unwrap();
        let second = json.find("https://second.example/").unwrap();
        assert!(first < second, "configured mirror order changed: {json}");
        assert!(
            !json.contains("must-not-appear"),
            "planned multiple registries: {json}"
        );
    }

    #[test]
    fn mirror_plan_json_contains_fingerprints_not_candidate_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let native_config = temporary.path().join("daemon.json");
        let secret = "candidate-secret-7d9";
        std::fs::write(&native_config, format!(r#"{{"private-key":"{secret}"}}"#)).unwrap();
        let mut config = osdk_core::config::ContainersConfig::default();
        config.registries.insert(
            "docker.io".into(),
            registry_policy(&["https://mirror.example/"]),
        );
        let runner = FakeRunner::new(healthy_local_docker());
        let plan = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Docker,
            None,
            Some(&native_config),
            None,
        )
        .unwrap();

        assert_eq!(plan.candidates.len(), 1);
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains(secret), "candidate bytes leaked: {json}");
        assert!(json.contains("content_sha256"));
        assert!(json.contains("https://mirror.example/"));
    }

    #[test]
    fn mirror_plan_json_redacts_configured_path_prefixes() {
        let temporary = tempfile::tempdir().unwrap();
        let native_config = temporary.path().join("buildkitd.toml");
        let secret_path = "tenant-secret-7d9";
        let mut config = osdk_core::config::ContainersConfig::default();
        config.registries.insert(
            "docker.io".into(),
            registry_policy(&[&format!("https://mirror.example/{secret_path}/")]),
        );
        let runner = FakeRunner::new([
            success("github.com/docker/buildx v0.36.1 deadbeef\n"),
            success(
                r#"{"Current":true,"Driver":"docker-container","Name":"selected","Nodes":[{"Name":"selected0","Endpoint":"unix:///var/run/docker.sock","Status":"running"}]}"#,
            ),
            success("Name: selected\nDriver: docker-container\nName: selected0\nEndpoint: unix:///var/run/docker.sock\nStatus: running\n"),
        ]);
        let plan = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Buildkit,
            None,
            Some(&native_config),
            None,
        )
        .unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains(secret_path), "mirror path leaked: {json}");
        assert!(json.contains("[redacted]"), "{json}");
        assert!(json.contains("\"has_path_prefix\":true"), "{json}");
    }

    #[test]
    fn mirror_plan_requires_policy_and_rejects_cross_runtime_options_without_running() {
        let runner = FakeRunner::new([]);
        let config = osdk_core::config::ContainersConfig::default();
        let missing = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Docker,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(missing.to_string().contains("docker.io"));
        assert!(runner.calls().is_empty());

        let mut config = config;
        config
            .registries
            .insert("docker.io".into(), registry_policy(&[]));
        let invalid = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Docker,
            Some(BuildxBuilderSelector::named("team-builder").unwrap()),
            None,
            None,
        )
        .unwrap_err();
        assert!(invalid.to_string().contains("--runtime buildkit"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn containerd_main_config_is_rejected_when_config_path_is_already_active() {
        let runner = FakeRunner::new([
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
            success(
                "version = 2\n[plugins.\"io.containerd.grpc.v1.cri\".registry]\nconfig_path = \"/etc/containerd/certs.d\"\n",
            ),
        ]);
        let mut config = osdk_core::config::ContainersConfig::default();
        config
            .registries
            .insert("docker.io".into(), registry_policy(&[]));
        let error = mirror_plan(
            &runner,
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Containerd,
            None,
            None,
            Some(Path::new("unused-config.toml")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("config_path"));
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn registry_and_plan_human_output_use_explicit_bilingual_labels() {
        let report = RegistryDiagnosticReport {
            schema_version: 1,
            status: RegistryDiagnosticStatus::Degraded,
            upstream: RegistryName::parse("registry.example").unwrap(),
            upstream_origin: "https://registry.example".into(),
            image: Some(ImageReference::parse("registry.example/team/app:v1").unwrap()),
            requested_platform: None,
            api: osdk_core::container::ApiCheck {
                status: ApiCheckStatus::BearerChallenge,
                ..Default::default()
            },
            manifest: osdk_core::container::ManifestCheck {
                status: ManifestCheckStatus::DigestMismatch,
                ..Default::default()
            },
            blob_range: Default::default(),
            mirrors: vec![osdk_core::container::MirrorCheck {
                order: 0,
                origin: "https://mirror.example".into(),
                status: MirrorCheckStatus::AuthenticationRequired,
                digest: None,
            }],
            request_count: 3,
        };
        let mut english = Vec::new();
        write_registry_human(&mut english, &report, Lang::En).unwrap();
        let english = String::from_utf8(english).unwrap();
        for expected in [
            "bearer-challenge",
            "digest-mismatch",
            "authentication-required",
        ] {
            assert!(english.contains(expected), "{english}");
        }
        assert!(!english.contains("bearerchallenge"));

        let mut chinese = Vec::new();
        write_registry_human(&mut chinese, &report, Lang::Zh).unwrap();
        let chinese = String::from_utf8(chinese).unwrap();
        for expected in ["Bearer 质询", "摘要不匹配", "需要认证"] {
            assert!(chinese.contains(expected), "{chinese}");
        }

        let mut config = osdk_core::config::ContainersConfig::default();
        config.registries.insert(
            "docker.io".into(),
            registry_policy(&["https://mirror.example/"]),
        );
        let plan = mirror_plan(
            &FakeRunner::new(healthy_local_docker()),
            &config,
            "docker.io",
            ContainerMirrorRuntimeArg::Docker,
            None,
            None,
            None,
        )
        .unwrap();
        let mut english = Vec::new();
        write_mirror_plan_human(&mut english, &plan, Lang::En).unwrap();
        let english = String::from_utf8(english).unwrap();
        assert!(english.contains(plan.plan_id.as_str()), "{english}");
        assert!(english.contains("restart-daemon"), "{english}");
        assert!(english.contains("native-config-path-required"), "{english}");
        assert!(!english.contains("restartdaemon"));

        let mut chinese = Vec::new();
        write_mirror_plan_human(&mut chinese, &plan, Lang::Zh).unwrap();
        let chinese = String::from_utf8(chinese).unwrap();
        assert!(chinese.contains(plan.plan_id.as_str()), "{chinese}");
        assert!(chinese.contains("重启守护进程"), "{chinese}");
        assert!(chinese.contains("需要显式原生配置路径"), "{chinese}");
    }

    #[test]
    fn auto_doctor_selects_by_status_and_keeps_attempts_in_fixed_order() {
        let runner = FakeRunner::new(
            [
                vec![CommandOutcome::NotInstalled],
                healthy_containerd().into_iter().collect(),
                vec![CommandOutcome::NotInstalled],
            ]
            .concat(),
        );

        let report = doctor(
            &runner,
            capture_limits(321),
            RuntimeSelection::Auto,
            BuildxBuilderSelector::Auto,
            false,
        );

        assert_eq!(report.selected_runtime, RuntimeKind::Containerd);
        assert_eq!(
            report
                .attempted_runtimes
                .iter()
                .map(|attempt| attempt.runtime)
                .collect::<Vec<_>>(),
            [RuntimeKind::Docker, RuntimeKind::Containerd]
        );
        assert_eq!(runner.calls()[0].program, "docker");
        assert_eq!(runner.calls()[1].program, "containerd");
        assert!(runner
            .calls()
            .iter()
            .all(|call| call.limits == Some(capture_limits(321))));
    }

    #[test]
    fn explicit_containerd_skips_buildx_unless_builder_is_explicit() {
        let without_builder = FakeRunner::new(healthy_containerd());
        let report = doctor(
            &without_builder,
            CaptureLimits::default(),
            RuntimeSelection::Containerd,
            BuildxBuilderSelector::Auto,
            false,
        );
        assert!(report.builder.is_none());
        assert_eq!(without_builder.calls().len(), 3);

        let with_builder = FakeRunner::new(
            [
                healthy_containerd().into_iter().collect(),
                vec![CommandOutcome::NotInstalled],
            ]
            .concat(),
        );
        let report = doctor(
            &with_builder,
            CaptureLimits::default(),
            RuntimeSelection::Containerd,
            BuildxBuilderSelector::named("team-builder").unwrap(),
            true,
        );
        assert_eq!(
            report.builder.as_ref().map(|builder| builder.runtime),
            Some(RuntimeKind::Buildkit)
        );
        assert_eq!(with_builder.calls().len(), 4);
    }

    #[test]
    fn doctor_json_is_deterministic_language_neutral_and_secret_safe() {
        fn render(lang: Lang) -> String {
            let runner = FakeRunner::new(
                [
                    healthy_docker().into_iter().collect(),
                    vec![
                        CommandOutcome::NotInstalled,
                        CommandOutcome::NotInstalled,
                        CommandOutcome::NotInstalled,
                    ],
                    vec![failure("token=stderr-secret")],
                ]
                .concat(),
            );
            let report = doctor(
                &runner,
                CaptureLimits::default(),
                RuntimeSelection::Auto,
                BuildxBuilderSelector::named("private-builder").unwrap(),
                false,
            );
            // Human localization is deliberately outside the serialized
            // contract; accepting a language here guards that boundary.
            let _ = lang;
            serde_json::to_string(&report).unwrap()
        }

        let english = render(Lang::En);
        let chinese = render(Lang::Zh);
        assert_eq!(english, chinese);
        assert!(english.starts_with("{\"schema_version\":2,"));
        assert!(english
            .contains("\"attempted_runtimes\":[{\"schema_version\":2,\"runtime\":\"docker\""));
        assert!(english.contains("\"details\":{\"kind\":\"docker\",\"client_version\":\"29.0.1\",\"server_version\":\"29.0.1\",\"context_kind\":\"remote\",\"daemon_os\":\"linux\",\"daemon_architecture\":\"amd64\",\"rootless\":false,\"desktop\":false"));
        for secret in [
            "secret-context",
            "alice",
            "password",
            "private",
            "token",
            "stderr-secret",
            "private-builder",
        ] {
            assert!(!english.contains(secret), "leaked {secret}: {english}");
        }
    }

    #[test]
    fn human_output_is_conclusion_first_and_bilingual() {
        let report = DoctorOutput {
            schema_version: 2,
            requested_runtime: RuntimeSelection::Docker,
            selected_runtime: RuntimeKind::Docker,
            runtime: DiagnosticReport::new(RuntimeKind::Docker, DiagnosticStatus::Healthy),
            attempted_runtimes: vec![DiagnosticReport::new(
                RuntimeKind::Docker,
                DiagnosticStatus::Healthy,
            )],
            builder: None,
        };

        let mut english = Vec::new();
        write_doctor_human(&mut english, &report, Lang::En).unwrap();
        assert!(String::from_utf8(english)
            .unwrap()
            .starts_with("selected docker: healthy"));

        let mut chinese = Vec::new();
        write_doctor_human(&mut chinese, &report, Lang::Zh).unwrap();
        assert!(String::from_utf8(chinese)
            .unwrap()
            .starts_with("已选择 docker：健康"));
    }

    #[test]
    fn doctor_json_contains_closed_partial_details_without_extra_commands() {
        let docker_runner = FakeRunner::new([
            success(r#"{"Client":{"Version":"29.0.1"},"Server":null}"#),
            failure("context unavailable"),
            failure("daemon unavailable"),
            CommandOutcome::NotInstalled,
        ]);
        let docker = doctor(
            &docker_runner,
            CaptureLimits::default(),
            RuntimeSelection::Docker,
            BuildxBuilderSelector::Auto,
            false,
        );
        let json = serde_json::to_value(&docker).unwrap();
        let details = &json["runtime"]["details"];
        assert_eq!(details["kind"], "docker");
        assert_eq!(details["client_version"], "29.0.1");
        assert!(details["server_version"].is_null());
        assert!(details["rootless"].is_null());
        assert_eq!(docker_runner.calls().len(), 4);

        let containerd_runner = FakeRunner::new(healthy_containerd());
        let containerd = doctor(
            &containerd_runner,
            CaptureLimits::default(),
            RuntimeSelection::Containerd,
            BuildxBuilderSelector::Auto,
            false,
        );
        let json = serde_json::to_string(&containerd).unwrap();
        assert!(json.contains("\"kind\":\"containerd\""));
        assert!(json.contains("\"config_path_configured\":false"));
        assert!(!json.contains("\"namespace\""));
        assert_eq!(containerd_runner.calls().len(), 3);
    }

    #[test]
    fn buildkit_details_omit_names_redact_endpoints_and_sort_platforms() {
        let builder = "secret-builder";
        let node = "secret-node";
        let runner = FakeRunner::new([
            success("github.com/docker/buildx v0.36.1 deadbeef\n"),
            success(&format!(
                r#"{{"Current":true,"Driver":"docker-container","Name":"{builder}","Nodes":[{{"Name":"{node}","Endpoint":"ssh://alice:password@example.test/private?token=secret","Status":"running","Buildkit":"v0.25.0","Platforms":"linux/arm64, linux/amd64"}}]}}"#
            )),
            success(&format!(
                "Name: {builder}\nDriver: docker-container\nName: {node}\nEndpoint: ssh://alice:password@example.test/private?token=secret\nStatus: running\nBuildKit: v0.25.0\nPlatforms: linux/arm64, linux/amd64\n"
            )),
        ]);
        let report = BuildkitAdapter::new(BuildxBuilderSelector::named(builder).unwrap())
            .diagnose(&runner, CaptureLimits::default());
        let value = serde_json::to_value(&report).unwrap();
        let json = value.to_string();
        assert!(json.contains("\"driver\":\"docker-container\""));
        assert!(json.contains("\"ordinal\":0"));
        assert!(json.contains("\"platforms\":[\"linux/amd64\",\"linux/arm64\"]"));
        assert_eq!(
            value["details"]["nodes"][0]["endpoint"],
            "ssh://example.test"
        );
        for secret in [builder, node, "alice", "password", "private", "token"] {
            assert!(!json.contains(secret), "leaked {secret}: {json}");
        }
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn human_details_are_compact_conclusion_first_and_bilingual() {
        let runner = FakeRunner::new(
            [
                healthy_local_docker().into_iter().collect(),
                vec![CommandOutcome::NotInstalled],
            ]
            .concat(),
        );
        let report = doctor(
            &runner,
            CaptureLimits::default(),
            RuntimeSelection::Docker,
            BuildxBuilderSelector::Auto,
            false,
        );
        for (lang, conclusion, version, rootless) in [
            (
                Lang::En,
                "selected docker: healthy",
                "client version: 29.0.1",
                "rootless: no",
            ),
            (
                Lang::Zh,
                "已选择 docker：健康",
                "客户端版本: 29.0.1",
                "无 root 模式: 否",
            ),
        ] {
            let mut output = Vec::new();
            write_doctor_human(&mut output, &report, lang).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.starts_with(conclusion), "{output}");
            assert!(output.contains(version), "{output}");
            assert!(output.contains(rootless), "{output}");
            assert!(!output.contains("unknown"), "{output}");
            assert!(!output.contains("未知"), "{output}");
        }
    }

    #[test]
    fn human_partial_details_emit_one_localized_unavailable_line() {
        let report = DoctorOutput {
            schema_version: 2,
            requested_runtime: RuntimeSelection::Docker,
            selected_runtime: RuntimeKind::Docker,
            runtime: DiagnosticReport::new(RuntimeKind::Docker, DiagnosticStatus::NotInstalled),
            attempted_runtimes: vec![DiagnosticReport::new(
                RuntimeKind::Docker,
                DiagnosticStatus::NotInstalled,
            )],
            builder: None,
        };

        for (lang, message) in [
            (Lang::En, "runtime details unavailable"),
            (Lang::Zh, "运行时详情不可用"),
        ] {
            let mut output = Vec::new();
            write_doctor_human(&mut output, &report, lang).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output.matches(message).count(), 1, "{output}");
            assert!(!output.contains("client version"), "{output}");
            assert!(!output.contains("客户端版本"), "{output}");
        }
    }

    #[test]
    fn cache_auto_uses_diagnostic_selection_and_containerd_is_unsupported() {
        let docker_selected = FakeRunner::new(
            [
                healthy_docker().into_iter().collect(),
                    vec![
                        CommandOutcome::NotInstalled,
                        CommandOutcome::NotInstalled,
                        CommandOutcome::NotInstalled,
                    ],
                vec![success(
                    r#"{"Type":"Images","TotalCount":"2","Active":"1","Size":"10MB","Reclaimable":"4MB (40%)"}"#,
                )],
            ]
            .concat(),
        );
        let status = cache_status(
            &docker_selected,
            CaptureLimits::default(),
            ContainerCacheRuntimeArg::Auto,
            BuildxBuilderSelector::Auto,
        );
        assert_eq!(status.runtime, RuntimeKind::Docker);
        assert_eq!(status.status, CacheQueryStatus::Available);
        assert_eq!(
            docker_selected.calls().last().unwrap().arguments[0],
            "system"
        );

        let containerd_selected = FakeRunner::new(
            [
                vec![CommandOutcome::NotInstalled],
                healthy_containerd().into_iter().collect(),
            ]
            .concat(),
        );
        let status = cache_status(
            &containerd_selected,
            CaptureLimits::default(),
            ContainerCacheRuntimeArg::Auto,
            BuildxBuilderSelector::Auto,
        );
        assert_eq!(status.runtime, RuntimeKind::Containerd);
        assert_eq!(status.status, CacheQueryStatus::Unsupported);
        assert_eq!(containerd_selected.calls().len(), 4);
    }

    #[test]
    fn cache_status_named_builder_overrides_config_and_stays_out_of_json() {
        let runner = FakeRunner::new([
            success("github.com/docker/buildx v0.36.1 deadbeef\n"),
            success(r#"{"ID":"private-cache-id","Size":"2048","Reclaimable":true}"#),
        ]);
        let status = cache_status(
            &runner,
            capture_limits(450),
            ContainerCacheRuntimeArg::Buildkit,
            BuildxBuilderSelector::named("override-builder").unwrap(),
        );

        assert_eq!(status.status, CacheQueryStatus::Available);
        assert_eq!(
            runner.calls()[1].arguments,
            [
                "buildx",
                "du",
                "--format=json",
                "--builder",
                "override-builder"
            ]
        );
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("override-builder"));
        assert!(!json.contains("private-cache-id"));
    }
}
