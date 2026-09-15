//! `osdk pkg`: read-only diagnostics for the host's own package managers.
//!
//! This command reports; it does not install, configure, or elevate.

use std::io::Write;

use anyhow::{Context, Result};
use osdk_core::process::SystemCommandRunner;
use osdk_core::syspkg::{
    self, Acceleration, Capability, CapabilityStatus, ManagerDetails, ManagerKind, ManagerReport,
    ManagerStatus, MirrorMeasurement, SourceRecord, SourceTrust, SystemPackageReport,
};

use crate::app::App;
use crate::cli::{PkgCommand, PkgManagerArg, PkgMirrorsCommand};

pub async fn run(app: &App, command: PkgCommand) -> Result<()> {
    let mut stdout = std::io::stdout();
    match command {
        PkgCommand::Doctor { json } => {
            let runner = SystemCommandRunner;
            let report = syspkg::diagnose_all(&runner);
            if json {
                serde_json::to_writer(&mut stdout, &report)
                    .context("serializing system package manager report")?;
                writeln!(stdout)?;
            } else {
                write_human(&mut stdout, &report)?;
            }
            Ok(())
        }
        PkgCommand::Mirrors { command } => match command {
            PkgMirrorsCommand::Test { manager, json } => {
                let manager = match manager {
                    PkgManagerArg::Winget => ManagerKind::Winget,
                };
                let measurements = syspkg::probe_winget_sources(&app.ctx).await?;

                // Which source osdk would actually pass to its own calls. Shown
                // next to the ranking because the two can legitimately differ:
                // the fastest endpoint is useless unless this host has it
                // registered, and that gap is exactly what confuses people.
                let runner = SystemCommandRunner;
                let registered = syspkg::registered_sources(&runner, manager);
                let selection = syspkg::preferred_winget_source(&registered, &measurements, None);

                if json {
                    let payload = serde_json::json!({
                        "measurements": measurements,
                        "selected": selection.as_ref().ok(),
                        "not_selected_because": selection.as_ref().err(),
                    });
                    serde_json::to_writer(&mut stdout, &payload)
                        .context("serializing mirror measurements")?;
                    writeln!(stdout)?;
                } else {
                    write_mirrors_human(&mut stdout, manager, &measurements)?;
                    write_selection(&mut stdout, &selection)?;
                }
                Ok(())
            }
        },
    }
}

/// Report which source osdk will hand its own calls, and why.
///
/// Every "no source" case gets a distinct explanation. A generic "unavailable"
/// would leave the user unable to tell "register the mirror" from "your pin is
/// fine and being honoured".
fn write_selection(
    output: &mut dyn Write,
    selection: &std::result::Result<String, syspkg::NoPreferredSource>,
) -> Result<()> {
    use syspkg::NoPreferredSource;

    match selection {
        Ok(name) => writeln!(
            output,
            "\nosdk will pass --source {name} on winget calls it issues itself.\n\
             Your own winget commands are unaffected."
        )?,
        Err(NoPreferredSource::UserPinned(pin)) => writeln!(
            output,
            "\nSource '{pin}' is pinned but not registered on this host, so osdk\n\
             will omit --source rather than fail the call. Register it, or clear the pin."
        )?,
        Err(NoPreferredSource::NoMirrorRegistered) => writeln!(
            output,
            "\nNone of the measured mirrors is registered with winget, so osdk will\n\
             omit --source and let winget choose. Run `osdk pkg mirrors apply` to\n\
             register the fastest one (needs administrator)."
        )?,
        Err(NoPreferredSource::OfficialIsFastest) => writeln!(
            output,
            "\nThe official source is the fastest one registered here, so osdk will\n\
             omit --source. Naming it would restrict the call to that source alone\n\
             and hide the others, including msstore."
        )?,
        Err(NoPreferredSource::NoMeasurement) => writeln!(
            output,
            "\nNo mirror was reachable, so osdk will omit --source and let winget choose."
        )?,
    }
    Ok(())
}

fn human_throughput(bytes_per_second: f64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes_per_second;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Render measured mirrors, fastest first.
///
/// The acceleration note is not decoration. A winget source mirror carries only
/// the manifest index; the installers those manifests point at still come from
/// the vendor. Someone who switches sources to fix a slow download and is not
/// told this will conclude the feature is broken.
fn write_mirrors_human(
    output: &mut dyn Write,
    manager: ManagerKind,
    measurements: &[MirrorMeasurement],
) -> Result<()> {
    writeln!(output, "Mirrors for {}", manager_label(manager))?;

    if measurements.is_empty() {
        writeln!(output, "  no candidates configured")?;
        return Ok(());
    }

    for (position, measurement) in measurements.iter().enumerate() {
        match (measurement.reachable, measurement.throughput) {
            (Some(true), Some(throughput)) => writeln!(
                output,
                "  {}. {:<14} {:>12}/s  ttfb {}ms",
                position + 1,
                measurement.source_id,
                human_throughput(throughput),
                measurement.ttfb_ms.unwrap_or_default()
            )?,
            (Some(false), _) => writeln!(output, "  -. {:<14} unreachable", measurement.source_id)?,
            _ => writeln!(output, "  -. {:<14} not probed", measurement.source_id)?,
        }
    }

    if measurements
        .iter()
        .all(|m| m.acceleration == Acceleration::IndexOnly)
    {
        writeln!(
            output,
            "\nThese mirrors carry the package index only. Installers are downloaded\n\
             from each vendor's own servers, so switching source speeds up finding\n\
             a package, not downloading it."
        )?;
    }

    Ok(())
}

fn status_label(status: ManagerStatus) -> &'static str {
    match status {
        ManagerStatus::Healthy => "ok",
        ManagerStatus::Degraded => "degraded",
        ManagerStatus::NotInstalled => "not installed",
        ManagerStatus::NotApplicable => "not applicable on this platform",
        ManagerStatus::PermissionDenied => "permission denied",
        ManagerStatus::Unresponsive => "unresponsive",
        ManagerStatus::UnsupportedVersion => "unsupported version",
    }
}

fn manager_label(manager: ManagerKind) -> &'static str {
    match manager {
        ManagerKind::Winget => "winget",
        ManagerKind::Homebrew => "homebrew",
    }
}

fn trust_label(trust: SourceTrust) -> &'static str {
    match trust {
        SourceTrust::Trusted => "trusted",
        SourceTrust::Untrusted => "untrusted",
        SourceTrust::Unknown => "unknown",
    }
}

/// Advice for a manager that is not usable.
///
/// Returns nothing when the situation is not the user's to fix, which is what
/// separates "winget is missing on Windows" from "winget is missing on macOS".
fn remedy(report: &ManagerReport) -> Option<&'static str> {
    match (report.manager, report.status) {
        (_, ManagerStatus::Healthy | ManagerStatus::NotApplicable) => None,
        (ManagerKind::Winget, ManagerStatus::NotInstalled) => Some(
            "install App Installer from the Microsoft Store, or see \
             https://learn.microsoft.com/windows/package-manager/winget/",
        ),
        (ManagerKind::Homebrew, ManagerStatus::NotInstalled) => {
            Some("install Homebrew from https://brew.sh")
        }
        (_, ManagerStatus::PermissionDenied) => {
            Some("the executable exists but this account may not run it")
        }
        (_, ManagerStatus::Unresponsive) => {
            Some("the manager did not answer in time; try running it directly to see why")
        }
        (_, ManagerStatus::UnsupportedVersion) => Some("update the manager to a supported version"),
        (_, ManagerStatus::Degraded) => {
            Some("the client works but part of its state could not be read")
        }
    }
}

fn write_source(output: &mut dyn Write, source: &SourceRecord) -> Result<()> {
    write!(
        output,
        "      {} ({})",
        source.name,
        trust_label(source.trust)
    )?;
    if let Some(endpoint) = &source.endpoint {
        write!(output, " {endpoint}")?;
    }
    writeln!(output)?;
    Ok(())
}

fn write_human(output: &mut dyn Write, report: &SystemPackageReport) -> Result<()> {
    writeln!(output, "System package managers")?;

    for manager in &report.managers {
        writeln!(
            output,
            "  {}: {}",
            manager_label(manager.manager),
            status_label(manager.status)
        )?;

        if let Some(ManagerDetails::Winget(details)) = &manager.details {
            if let Some(version) = &details.version {
                writeln!(output, "    version: {version}")?;
            }
            if !details.sources.is_empty() {
                writeln!(output, "    sources:")?;
                for source in &details.sources {
                    write_source(output, source)?;
                }
            }
            if details.has_non_default_source {
                writeln!(
                    output,
                    "    note: a non-default source is configured, so manifests may come \
                     from a mirror"
                )?;
            }
        }

        if manager.capabilities.get(&Capability::SourceEnumeration)
            == Some(&CapabilityStatus::Unavailable)
        {
            writeln!(output, "    sources could not be read in this pass")?;
        }

        if let Some(remedy) = remedy(manager) {
            writeln!(output, "    → {remedy}")?;
        }
    }

    if report.managers.iter().all(|m| !m.is_actionable()) {
        writeln!(output, "\nNothing to fix.")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use osdk_core::syspkg::{ProbeOutcome, ProbePurpose, ProbeRecord, WingetDetails};

    fn healthy_winget() -> ManagerReport {
        let mut report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::Healthy);
        report.set_capability(Capability::Client, CapabilityStatus::Supported);
        report.set_capability(Capability::SourceEnumeration, CapabilityStatus::Supported);
        report.record_probe(ProbeRecord {
            purpose: ProbePurpose::VersionQuery,
            outcome: ProbeOutcome::Succeeded,
            exit_code: Some(0),
        });
        report.set_details(ManagerDetails::Winget(WingetDetails {
            version: Some("v1.29.290".to_owned()),
            sources: vec![SourceRecord {
                identifier: "Microsoft.Winget.Source_8wekyb3d8bbwe".to_owned(),
                name: "winget".to_owned(),
                endpoint: Some("https://cdn.winget.microsoft.com/cache".to_owned()),
                kind: Some("Microsoft.PreIndexed.Package".to_owned()),
                trust: SourceTrust::Trusted,
            }],
            has_non_default_source: false,
        }));
        report
    }

    fn render(report: &SystemPackageReport) -> String {
        let mut buffer = Vec::new();
        write_human(&mut buffer, report).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn a_healthy_host_is_reported_without_advice() {
        let text = render(&SystemPackageReport::new(vec![healthy_winget()]));

        assert!(text.contains("winget: ok"));
        assert!(text.contains("v1.29.290"));
        assert!(text.contains("Nothing to fix."));
        assert!(
            !text.contains('→'),
            "a working manager needs no remediation line"
        );
    }

    #[test]
    fn a_missing_manager_on_its_platform_gets_advice() {
        let report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::NotInstalled);
        let text = render(&SystemPackageReport::new(vec![report]));

        assert!(text.contains("not installed"));
        assert!(text.contains("App Installer"));
        assert!(!text.contains("Nothing to fix."));
    }

    #[test]
    fn a_manager_absent_from_this_platform_gets_no_advice() {
        let report = ManagerReport::not_applicable(ManagerKind::Winget);
        let text = render(&SystemPackageReport::new(vec![report]));

        assert!(text.contains("not applicable"));
        assert!(
            text.contains("Nothing to fix."),
            "winget missing on macOS is not a problem the user should act on"
        );
    }

    #[test]
    fn a_configured_mirror_is_called_out() {
        let mut report = healthy_winget();
        if let Some(ManagerDetails::Winget(details)) = &mut report.details {
            details.has_non_default_source = true;
            details.sources.push(SourceRecord {
                identifier: "USTC.Mirror".to_owned(),
                name: "ustc".to_owned(),
                endpoint: Some("https://mirrors.ustc.edu.cn/winget-source".to_owned()),
                kind: Some("Microsoft.PreIndexed.Package".to_owned()),
                trust: SourceTrust::Trusted,
            });
        }

        let text = render(&SystemPackageReport::new(vec![report]));

        assert!(text.contains("mirrors.ustc.edu.cn"));
        assert!(text.contains("non-default source"));
    }

    #[test]
    fn json_output_carries_the_schema_version() {
        let report = SystemPackageReport::new(vec![healthy_winget()]);

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(osdk_core::syspkg::SYSPKG_DIAGNOSTIC_SCHEMA_VERSION)
        );
    }

    fn measurement(id: &str, throughput: Option<f64>) -> MirrorMeasurement {
        MirrorMeasurement {
            source_id: id.to_owned(),
            endpoint: format!("https://example.invalid/{id}"),
            kind: osdk_core::source::SourceKind::Mirror,
            acceleration: Acceleration::IndexOnly,
            reachable: Some(throughput.is_some()),
            ttfb_ms: throughput.map(|_| 42),
            throughput,
        }
    }

    fn render_mirrors(measurements: &[MirrorMeasurement]) -> String {
        let mut buffer = Vec::new();
        write_mirrors_human(&mut buffer, ManagerKind::Winget, measurements).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn index_only_mirrors_say_so_plainly() {
        let text = render_mirrors(&[measurement("ustc", Some(5_000_000.0))]);

        // Without this the user switches source, sees downloads unchanged, and
        // concludes osdk's mirror support does not work.
        assert!(
            text.contains("not downloading it"),
            "the index-only limit must be stated, not implied"
        );
    }

    #[test]
    fn unreachable_mirrors_are_listed_rather_than_hidden() {
        let text = render_mirrors(&[
            measurement("ustc", Some(5_000_000.0)),
            measurement("nju", None),
        ]);

        assert!(text.contains("unreachable"));
        assert!(
            text.contains("nju"),
            "a mirror that failed is information, not noise"
        );
    }

    #[test]
    fn throughput_is_rendered_in_readable_units() {
        let text = render_mirrors(&[measurement("ustc", Some(5_242_880.0))]);

        assert!(text.contains("5.0 MiB/s"), "got: {text}");
        assert!(text.contains("ttfb 42ms"));
    }

    #[test]
    fn an_empty_candidate_set_does_not_claim_anything() {
        let text = render_mirrors(&[]);

        assert!(text.contains("no candidates"));
        assert!(
            !text.contains("not downloading it"),
            "with nothing measured there is no acceleration claim to qualify"
        );
    }
}
