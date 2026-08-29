//! User-facing, read-only native container diagnostics.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use osdk_core::config::ContainerRuntime;
use osdk_core::container::{
    BuildkitAdapter, BuildxBuilderSelector, BuildxCacheQuery, CacheQueryStatus, ContainerdAdapter,
    ContainerdCacheQuery, DiagnosticReport, DiagnosticStatus, DockerAdapter, DockerCacheQuery,
    NativeCacheRecordKind, NativeCacheStatus, RuntimeAdapter, RuntimeKind,
};
use osdk_core::i18n::{self, interpolate, trl, Lang};
use osdk_core::process::{CaptureLimits, CommandRunner, SystemCommandRunner};
use serde::Serialize;

use crate::app::App;
use crate::cli::{
    ContainerCacheCommand, ContainerCacheRuntimeArg, ContainerCommand, ContainerRuntimeArg,
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

pub fn run(app: &App, command: ContainerCommand) -> Result<()> {
    run_with(
        &SystemCommandRunner,
        app.ctx.config.containers(),
        command,
        &mut std::io::stdout(),
    )
}

fn run_with(
    runner: &dyn CommandRunner,
    config: &osdk_core::config::ContainersConfig,
    command: ContainerCommand,
    output: &mut dyn Write,
) -> Result<()> {
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
    }
    Ok(())
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
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;

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

    fn healthy_containerd() -> [CommandOutcome; 3] {
        [
            success("containerd github.com/containerd/containerd v1.7.22 abc"),
            success("Client:\n  Version: v1.7.22\nServer:\n  Version: v1.7.22\n"),
            success("version = 2"),
        ]
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
