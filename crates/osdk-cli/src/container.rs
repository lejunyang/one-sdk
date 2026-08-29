//! User-facing, read-only native container diagnostics.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use osdk_core::config::{ContainerPlatform, ContainerRegistryConfig, ContainerRuntime};
use osdk_core::container::{
    diagnose_registry, plan_buildkit_mirrors, plan_containerd_mirrors, plan_docker_mirrors,
    ActivationRequirement, ApiCheckStatus, BuildkitAdapter, BuildkitMirrorPlanRequest,
    BuildxBuilderSelector, BuildxCacheQuery, CacheQueryStatus, ContainerdAdapter,
    ContainerdCacheQuery, ContainerdMirrorPlanRequest, DiagnosticReport, DiagnosticStatus,
    DockerAdapter, DockerCacheQuery, DockerMirrorPlanRequest, ImageReference, ManifestCheckStatus,
    MirrorCheckStatus, MirrorPlan, NativeCacheRecordKind, NativeCacheStatus, NativeConfigSnapshot,
    OciPlatform, PlanApplicability, PlanWarning, RegistryDiagnosticOptions,
    RegistryDiagnosticReport, RegistryDiagnosticStatus, RegistryEndpoint, RegistryLimits,
    RegistryName, RegistryTransport, ReqwestRegistryTransport, RuntimeAdapter, RuntimeKind,
    DEFAULT_MAX_REQUESTS, MAX_NATIVE_CONFIG_BYTES,
};
use osdk_core::i18n::{self, interpolate, trl, Lang};
use osdk_core::process::{CaptureLimits, CommandRunner, SystemCommandRunner};
use serde::Serialize;

use crate::app::App;
use crate::cli::{
    ContainerCacheCommand, ContainerCacheRuntimeArg, ContainerCommand, ContainerMirrorRuntimeArg,
    ContainerMirrorsCommand, ContainerRegistryCommand, ContainerRuntimeArg,
};

const DOCTOR_SCHEMA_VERSION: u32 = 1;
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

pub async fn run(app: &App, command: ContainerCommand) -> Result<()> {
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
    .await
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
            let report = DockerAdapter.diagnose(runner, limits);
            (report.clone(), vec![report])
        }
        RuntimeSelection::Containerd => {
            let report = ContainerdAdapter::default().diagnose(runner, limits);
            (report.clone(), vec![report])
        }
        RuntimeSelection::Auto => {
            // Probe order and tie-breaking are stable. Selection depends on
            // diagnosed status, never binary presence.
            let docker = DockerAdapter.diagnose(runner, limits);
            let containerd = ContainerdAdapter::default().diagnose(runner, limits);
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
    if report.attempted_runtimes.len() > 1 {
        for attempted in &report.attempted_runtimes {
            writeln!(
                output,
                "  {}: {}",
                runtime_label(attempted.runtime),
                diagnostic_status_label(lang, attempted.status)
            )?;
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
    }
    Ok(())
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

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        program: String,
        arguments: Vec<String>,
        limits: CaptureLimits,
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
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected native command")
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("container inspection must not execute foreground commands")
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
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
    }

    #[cfg(windows)]
    fn exit_status(success: bool) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(if success { 0 } else { 1 })
    }

    fn healthy_docker() -> [CommandOutcome; 3] {
        [
            success(r#"{"Client":{"Version":"29.0.1"},"Server":{"Version":"29.0.1"}}"#),
            success(
                r#"[{"Name":"secret-context","Endpoints":{"docker":{"Host":"ssh://alice:password@example.test/private?token=secret"}}}]"#,
            ),
            success(
                r#"{"OperatingSystem":"Linux","OSType":"linux","Architecture":"amd64","RegistryConfig":{"Mirrors":[]}}"#,
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
                r#"{"OperatingSystem":"Linux","OSType":"linux","Architecture":"amd64","RegistryConfig":{"Mirrors":[]}}"#,
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
            .all(|call| call.limits == capture_limits(321)));
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
        assert!(english.starts_with("{\"schema_version\":1,"));
        assert!(english
            .contains("\"attempted_runtimes\":[{\"schema_version\":1,\"runtime\":\"docker\""));
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
            schema_version: 1,
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
