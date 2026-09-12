//! `osdk pkg`: read-only diagnostics for the host's own package managers.
//!
//! This command reports; it does not install, configure, or elevate.

use std::io::Write;

use anyhow::{Context, Result};
use osdk_core::process::SystemCommandRunner;
use osdk_core::syspkg::{
    self, Capability, CapabilityStatus, ManagerDetails, ManagerKind, ManagerReport, ManagerStatus,
    SourceRecord, SourceTrust, SystemPackageReport,
};

use crate::app::App;
use crate::cli::PkgCommand;

pub fn run(app: &App, command: PkgCommand) -> Result<()> {
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
            let _ = app;
            Ok(())
        }
    }
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
    write!(output, "      {} ({})", source.name, trust_label(source.trust))?;
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
}
