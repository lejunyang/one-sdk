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
        PkgCommand::Status { missing, json } => {
            let report = build_status(app)?;
            if json {
                serde_json::to_writer(&mut stdout, &report)
                    .context("serializing package status")?;
                writeln!(stdout)?;
            } else {
                write_status(&mut stdout, &report)?;
            }
            // `--missing` is the CI contract: a requested package that is absent
            // must fail the check. A version difference does not, because the
            // config never promised to hold a version.
            if missing && report.has_missing() {
                anyhow::bail!("some requested packages are not installed");
            }
            Ok(())
        }
        PkgCommand::Plan {
            json,
            detailed_exitcode,
        } => {
            let (plan, _) = build_plan(app)?;
            if json {
                serde_json::to_writer(&mut stdout, &plan).context("serializing package plan")?;
                writeln!(stdout)?;
            } else {
                write_install_plan(&mut stdout, &plan)?;
            }
            if detailed_exitcode && !plan.is_empty() {
                // 2 means "changes pending", distinct from 1 for a real error,
                // so a pipeline can branch without parsing output.
                std::process::exit(2);
            }
            Ok(())
        }
        PkgCommand::Apply { dry_run, yes, json } => apply_packages(app, dry_run, yes, json).await,
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
            PkgMirrorsCommand::Apply {
                manager,
                dry_run,
                accept_plan,
                json,
            } => {
                let manager = match manager {
                    PkgManagerArg::Winget => ManagerKind::Winget,
                };
                apply_mirror(app, manager, dry_run, accept_plan.as_deref(), json).await
            }
        },
    }
}

/// Compare `[syspkg.packages]` against the host.
///
/// The installed set is read once and reused for every request, so a report is
/// one query rather than one per package.
fn build_status(app: &App) -> Result<syspkg::StatusReport> {
    let config = &app.ctx.config.sources.syspkg;
    let (parsed, key_errors) = config.parsed_packages();
    let runner = SystemCommandRunner;

    // Query only when something actually asks for that manager, so a project
    // with no winget packages never shells out to winget.
    let wants_winget = parsed
        .iter()
        .any(|(key, _)| key.manager == ManagerKind::Winget);
    let installed = if wants_winget && config.allows(ManagerKind::Winget) {
        syspkg::installed_winget_packages(&runner, &app.ctx.dirs.cache)
    } else {
        None
    };

    let platform_os = platform_os_name();
    let statuses = parsed
        .iter()
        .map(|(key, request)| {
            let available = match key.manager {
                ManagerKind::Winget if config.allows(ManagerKind::Winget) => installed.as_deref(),
                // Homebrew is not implemented, and a manager excluded by config
                // must not be reported on as though it had been queried.
                _ => None,
            };
            syspkg::evaluate(key, request, available, platform_os)
        })
        .collect();

    Ok(syspkg::StatusReport::new(
        statuses,
        key_errors.iter().map(ToString::to_string).collect(),
    ))
}

/// The platform name `[syspkg.packages]` `os` values are matched against.
fn platform_os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Build an install plan from the current status.
fn build_plan(app: &App) -> Result<(syspkg::InstallPlan, syspkg::StatusReport)> {
    let config = &app.ctx.config.sources.syspkg;
    let report = build_status(app)?;
    let (parsed, _) = config.parsed_packages();

    let triples: Vec<_> = parsed
        .into_iter()
        .filter_map(|(key, request)| {
            report
                .packages
                .iter()
                .find(|status| status.id == key.id && status.manager == key.manager)
                .cloned()
                .map(|status| (key, request, status))
        })
        .collect();

    let plan = syspkg::plan_installs(&triples, |manager| config.allows(manager));
    Ok((plan, report))
}

/// Install what is missing, after showing exactly what that is.
async fn apply_packages(app: &App, dry_run: bool, yes: bool, json: bool) -> Result<()> {
    let mut stdout = std::io::stdout();
    let (plan, _) = build_plan(app)?;

    if plan.is_empty() {
        if json {
            serde_json::to_writer(&mut stdout, &serde_json::json!({ "installed": [] }))
                .context("serializing package apply result")?;
            writeln!(stdout)?;
        } else {
            writeln!(
                stdout,
                "Nothing to install: every requested package is present."
            )?;
        }
        return Ok(());
    }

    if dry_run {
        if json {
            serde_json::to_writer(&mut stdout, &plan).context("serializing package plan")?;
            writeln!(stdout)?;
        } else {
            write_install_plan(&mut stdout, &plan)?;
        }
        return Ok(());
    }

    if !yes {
        // Installing software is not something to do on an implied yes.
        write_install_plan(&mut stdout, &plan)?;
        writeln!(
            stdout,
            "\nNothing has been installed. Re-run with --yes to proceed."
        )?;
        anyhow::bail!("confirmation required");
    }

    let runner = SystemCommandRunner;
    let results = syspkg::run_installs(&runner, &plan);
    let failed = results.iter().filter(|r| !r.succeeded).count();

    if json {
        serde_json::to_writer(&mut stdout, &serde_json::json!({ "installed": results }))
            .context("serializing package apply result")?;
        writeln!(stdout)?;
    } else {
        for result in &results {
            let mark = if result.succeeded { "ok" } else { "failed" };
            writeln!(stdout, "  {mark}: {} -- {}", result.id, result.explanation)?;
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} package(s) could not be installed");
    }
    Ok(())
}

/// Render a status report.
fn write_status(output: &mut dyn Write, report: &syspkg::StatusReport) -> Result<()> {
    if report.packages.is_empty() && report.invalid_keys.is_empty() {
        writeln!(
            output,
            "No system packages are configured. Add them under [syspkg.packages]."
        )?;
        return Ok(());
    }

    writeln!(output, "System packages")?;
    for package in &report.packages {
        let installed = package.installed.as_deref().unwrap_or("-");
        writeln!(
            output,
            "  {:<40} {:<16} requested {} (installed {})",
            package.id,
            state_label(package.state),
            package.requested,
            installed
        )?;
    }

    // Surfaced rather than logged: a key osdk cannot read is a package the user
    // believes is managed, so "nothing missing" would be untrue while it exists.
    for invalid in &report.invalid_keys {
        writeln!(output, "  invalid entry: {invalid}")?;
    }
    Ok(())
}

fn state_label(state: syspkg::PackageState) -> &'static str {
    match state {
        syspkg::PackageState::Satisfied => "ok",
        syspkg::PackageState::VersionDiffers => "other version",
        syspkg::PackageState::Missing => "missing",
        syspkg::PackageState::NotApplicable => "not for this os",
        syspkg::PackageState::ManagerUnavailable => "manager unavailable",
    }
}

/// Render an install plan, including what it deliberately leaves alone.
fn write_install_plan(output: &mut dyn Write, plan: &syspkg::InstallPlan) -> Result<()> {
    if plan.installs.is_empty() {
        writeln!(output, "Nothing to install.")?;
    } else {
        writeln!(output, "Would install:")?;
        for install in &plan.installs {
            writeln!(output, "  {} ({})", install.id, install.version)?;
            writeln!(output, "    {}", install.command.display())?;
        }
    }

    if !plan.skipped.is_empty() {
        writeln!(output, "\nLeft alone:")?;
        for skipped in &plan.skipped {
            writeln!(
                output,
                "  {:<40} {}",
                skipped.id,
                skip_label(skipped.reason)
            )?;
        }
    }
    Ok(())
}

fn skip_label(reason: syspkg::SkipReason) -> &'static str {
    match reason {
        syspkg::SkipReason::AlreadySatisfied => "already installed",
        syspkg::SkipReason::VersionDiffersButPresent => {
            "present at another version; the configured version is a wish, not a lock"
        }
        syspkg::SkipReason::NotApplicable => "not for this operating system",
        syspkg::SkipReason::ManagerUnavailable => "its manager could not be queried",
        syspkg::SkipReason::ManagerNotAllowed => "its manager is excluded by [syspkg] managers",
    }
}

/// Register the fastest usable mirror, or explain why that cannot be done.
///
/// The sequence is deliberate: measure, then rule out what cannot work, then
/// show the exact commands, and only then -- with an explicit confirmation --
/// change anything. Every stage before the last is read-only, so running this
/// without `--accept-plan` can never alter the host.
async fn apply_mirror(
    app: &App,
    manager: ManagerKind,
    dry_run: bool,
    accept_plan: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    let runner = SystemCommandRunner;

    let measurements = syspkg::probe_winget_sources(&app.ctx).await?;
    let registered = syspkg::registered_sources(&runner, manager);

    // The endpoint currently serving the default source is the baseline every
    // candidate must beat: replacing it with something older is what Windows
    // rejects outright.
    let installed_endpoint = registered
        .iter()
        .find(|source| source.name == syspkg::DEFAULT_WINGET_SOURCE_NAME)
        .and_then(|source| source.endpoint.clone());
    let installed_published = match installed_endpoint.as_deref() {
        Some(endpoint) => syspkg::source_published_at(endpoint).await,
        None => None,
    };

    // Fastest first, and only mirrors: the default source is already in effect,
    // so "applying" it would be a no-op that still costs a remove/add cycle.
    let mut candidates: Vec<&syspkg::MirrorMeasurement> = measurements
        .iter()
        .filter(|m| m.kind != osdk_core::source::SourceKind::Official)
        .collect();
    candidates.sort_by(|a, b| {
        b.throughput
            .unwrap_or(0.0)
            .total_cmp(&a.throughput.unwrap_or(0.0))
    });

    let mut rejected: Vec<(String, syspkg::Infeasible)> = Vec::new();
    let mut chosen = None;
    for candidate in candidates {
        let published = syspkg::source_published_at(&candidate.endpoint).await;
        match syspkg::assess_feasibility(
            published.as_deref(),
            installed_published.as_deref(),
            &candidate.endpoint,
            candidate.reachable == Some(true),
        ) {
            Ok(()) => {
                chosen = Some(candidate);
                break;
            }
            Err(reason) => rejected.push((candidate.source_id.clone(), reason)),
        }
    }

    let Some(candidate) = chosen else {
        if json {
            let payload = serde_json::json!({
                "applied": false,
                "plan": serde_json::Value::Null,
                "rejected": rejected
                    .iter()
                    .map(|(id, reason)| serde_json::json!({ "mirror": id, "infeasible": reason }))
                    .collect::<Vec<_>>(),
            });
            serde_json::to_writer(&mut stdout, &payload)
                .context("serializing mirror apply result")?;
            writeln!(stdout)?;
        } else {
            write_no_usable_mirror(&mut stdout, &rejected, installed_published.as_deref())?;
        }
        // Nothing was applied, so a script must not read this as success. The
        // explanation above has already been printed; the error only sets the
        // exit code.
        if dry_run {
            return Ok(());
        }
        anyhow::bail!("no mirror could be applied");
    };

    let plan =
        syspkg::MirrorPlan::replace_default(&candidate.source_id, &candidate.endpoint, &registered);

    if dry_run {
        if json {
            serde_json::to_writer(&mut stdout, &plan).context("serializing mirror plan")?;
            writeln!(stdout)?;
        } else {
            write_plan(&mut stdout, &plan)?;
        }
        return Ok(());
    }

    match syspkg::apply_plan(&runner, &plan, &registered, accept_plan) {
        Ok(outcome) => {
            if json {
                let payload = serde_json::json!({
                    "applied": outcome.failed.is_none(),
                    "outcome": outcome,
                });
                serde_json::to_writer(&mut stdout, &payload)
                    .context("serializing mirror apply result")?;
                writeln!(stdout)?;
            } else {
                write_outcome(&mut stdout, &outcome)?;
            }
            // A failed apply must not report success to a script.
            if outcome.failed.is_some() {
                anyhow::bail!("applying the mirror failed; see the output above");
            }
            Ok(())
        }
        Err(refusal) => {
            if json {
                let payload = serde_json::json!({
                    "applied": false,
                    "refused": refusal,
                    "plan": plan,
                });
                serde_json::to_writer(&mut stdout, &payload)
                    .context("serializing mirror apply refusal")?;
                writeln!(stdout)?;
            } else {
                write_plan(&mut stdout, &plan)?;
                write_refusal(&mut stdout, &refusal)?;
            }
            // Same reasoning as above: an unapplied plan is not a success.
            anyhow::bail!("the mirror was not applied");
        }
    }
}

/// Explain why no mirror can be applied, naming each candidate's obstacle.
fn write_no_usable_mirror(
    output: &mut dyn Write,
    rejected: &[(String, syspkg::Infeasible)],
    installed_published: Option<&str>,
) -> Result<()> {
    writeln!(output, "No mirror can be applied right now.")?;
    if let Some(installed) = installed_published {
        writeln!(
            output,
            "  currently registered source published: {installed}"
        )?;
    }
    for (mirror, reason) in rejected {
        let explanation = match reason {
            syspkg::Infeasible::MirrorIsStale {
                mirror_last_modified,
                ..
            } => format!(
                "published {mirror_last_modified}, older than what is installed -- \
                 winget would reject it with 0x80073D06"
            ),
            syspkg::Infeasible::PublishTimeUnknown { .. } => {
                "publish time could not be established, so staleness cannot be ruled out".to_owned()
            }
            syspkg::Infeasible::MirrorUnreachable { .. } => "unreachable".to_owned(),
        };
        writeln!(output, "  {mirror}: {explanation}")?;
    }
    writeln!(
        output,
        "\nA mirror lagging behind upstream is common and resolves itself once it\n\
         syncs. Nothing was changed."
    )?;
    Ok(())
}

/// Show a plan in full, with its costs and its rollback, before anything runs.
fn write_plan(output: &mut dyn Write, plan: &syspkg::MirrorPlan) -> Result<()> {
    writeln!(
        output,
        "Plan: point winget's `{}` source at the {} mirror",
        plan.source_name, plan.mirror_id
    )?;
    writeln!(output, "  endpoint: {}", plan.endpoint)?;

    writeln!(output, "\nCommands, in order:")?;
    for command in &plan.commands {
        writeln!(output, "  {}", command.display())?;
    }

    writeln!(output, "\nWhat this costs:")?;
    for consequence in &plan.consequences {
        writeln!(output, "  - {}", consequence_label(consequence))?;
    }

    if let Some(rollback) = &plan.rollback {
        writeln!(output, "\nTo undo:\n  {}", rollback.display())?;
    }

    writeln!(
        output,
        "\nNothing has been changed. To apply, re-run with:\n  \
         --accept-plan {}",
        plan.fingerprint
    )?;
    Ok(())
}

/// Plain-language wording for each disclosed cost.
fn consequence_label(consequence: &syspkg::Consequence) -> &'static str {
    match consequence {
        syspkg::Consequence::NeedsAdministrator => "needs administrator rights",
        syspkg::Consequence::MachineWide => {
            "affects every winget user on this machine, not just osdk"
        }
        syspkg::Consequence::OfficialSourceRemoved => {
            "Microsoft's own endpoint will no longer be registered"
        }
        syspkg::Consequence::BrieflyWithoutAnySource => {
            "between the two commands, winget has no package source at all"
        }
        syspkg::Consequence::LosesStoreOriginTrust => {
            "a mirror cannot carry the built-in source's StoreOrigin trust marker"
        }
        syspkg::Consequence::IndexOnlyAcceleration => {
            "only package search gets faster; installer downloads do not"
        }
    }
}

/// Report what a run actually did, including a rollback if one happened.
fn write_outcome(output: &mut dyn Write, outcome: &syspkg::ApplyOutcome) -> Result<()> {
    for command in &outcome.completed {
        writeln!(output, "  ok: {command}")?;
    }
    match (&outcome.failed, outcome.rolled_back) {
        (None, _) => writeln!(output, "\nMirror applied.")?,
        (Some(failed), rolled_back) => {
            writeln!(output, "\nfailed: {failed}")?;
            if let Some(code) = outcome.exit_code {
                writeln!(output, "  exit code: {code}")?;
            }
            match rolled_back {
                Some(true) => writeln!(
                    output,
                    "  rolled back: winget's built-in source was restored"
                )?,
                Some(false) => writeln!(
                    output,
                    "  ROLLBACK FAILED: winget may have no package source right now.\n  \
                     Run `winget source reset --name winget --force` as administrator."
                )?,
                None => writeln!(output, "  no rollback was available")?,
            }
        }
    }
    Ok(())
}

/// Explain a refusal, distinguishing a stale confirmation from a changed host.
fn write_refusal(output: &mut dyn Write, refusal: &syspkg::ApplyRefused) -> Result<()> {
    match refusal {
        syspkg::ApplyRefused::NotConfirmed { .. } => writeln!(
            output,
            "\nNothing was changed: this plan has not been confirmed."
        )?,
        syspkg::ApplyRefused::WrongFingerprint { expected, supplied } => writeln!(
            output,
            "\nNothing was changed: --accept-plan {supplied} does not match this plan.\n\
             The plan above is {expected}."
        )?,
        syspkg::ApplyRefused::StateChanged { .. } => writeln!(
            output,
            "\nNothing was changed: winget's sources changed after this plan was built,\n\
             so the confirmation no longer describes this host. Re-run to plan again."
        )?,
    }
    Ok(())
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
        Err(NoPreferredSource::AlreadyTheDefaultSource) => writeln!(
            output,
            "\nThe fastest source registered here is already the one winget uses by\n\
             default, so osdk will omit --source. Naming it would restrict the call\n\
             to that source alone and hide the others, including msstore."
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

    write_distro_managers(output, &report.distro_managers)?;

    if report.managers.iter().all(|m| !m.is_actionable()) {
        writeln!(output, "\nNothing to fix.")?;
    }

    Ok(())
}

/// Report the Linux package managers this host has, and osdk's stance on them.
///
/// Stating the stance is the point of the section. These are detected, never
/// driven: every change they make needs root and lands in `/usr`, `/etc` and
/// `/var`, Arch declares partial upgrades unsupported, and recovery after a
/// failure differs so much between them that no single promise would be true.
/// A user who sees them listed would reasonably assume `osdk pkg apply` covers
/// them, so the line saying otherwise is not boilerplate.
fn write_distro_managers(output: &mut dyn Write, reports: &[syspkg::DistroReport]) -> Result<()> {
    let present: Vec<&syspkg::DistroReport> = reports.iter().filter(|r| r.present).collect();
    if present.is_empty() {
        return Ok(());
    }

    writeln!(output, "\nLinux package managers (detected, not managed)")?;
    for report in present {
        writeln!(
            output,
            "  {}: present, rollback {}",
            report.manager.id(),
            rollback_label(report.rollback)
        )?;
        if report.full_system_upgrade_only {
            // Arch's own guidance, passed through rather than paraphrased away.
            writeln!(
                output,
                "    this distribution supports only full-system upgrades; run `pacman -Syu` yourself"
            )?;
        }
    }
    writeln!(
        output,
        "  osdk reports these and prints commands for you to run. It never installs,\n  \
         upgrades, or elevates through them."
    )?;
    Ok(())
}

/// How much a manager can undo, in plain words.
fn rollback_label(ability: syspkg::RollbackAbility) -> &'static str {
    match ability {
        // This is the one that makes dnf structurally different from the others.
        syspkg::RollbackAbility::Transactional => "transactional (`dnf history undo`)",
        syspkg::RollbackAbility::ManualFromCache => "manual downgrade from cache only",
        syspkg::RollbackAbility::None => "none (logs only)",
    }
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

    fn sample_plan() -> osdk_core::syspkg::MirrorPlan {
        osdk_core::syspkg::MirrorPlan::replace_default(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            &[],
        )
    }

    fn render_plan(plan: &osdk_core::syspkg::MirrorPlan) -> String {
        let mut buffer = Vec::new();
        write_plan(&mut buffer, plan).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn a_plan_states_that_nothing_has_changed_yet() {
        let text = render_plan(&sample_plan());

        // The single most important line: a user reading a wall of commands must
        // not be left wondering whether they already ran.
        assert!(text.contains("Nothing has been changed"), "got: {text}");
    }

    #[test]
    fn a_plan_shows_every_command_the_rollback_and_the_fingerprint() {
        let plan = sample_plan();
        let text = render_plan(&plan);

        for command in &plan.commands {
            assert!(
                text.contains(&command.display()),
                "a command that will run must be shown verbatim: {}",
                command.display()
            );
        }
        assert!(text.contains("To undo"));
        assert!(
            text.contains(&plan.fingerprint),
            "the confirmation token must be printed, or the plan cannot be accepted"
        );
    }

    #[test]
    fn a_plan_discloses_the_window_with_no_package_source() {
        let text = render_plan(&sample_plan());

        assert!(
            text.contains("no package source at all"),
            "the riskiest moment must be stated, got: {text}"
        );
        assert!(text.contains("only package search gets faster"));
    }

    #[test]
    fn a_stale_mirror_is_explained_with_the_error_winget_would_give() {
        let rejected = vec![(
            "ustc".to_owned(),
            osdk_core::syspkg::Infeasible::MirrorIsStale {
                mirror_last_modified: "Tue, 15 Sep 2026 10:21:23 GMT".to_owned(),
                installed_last_modified: "Tue, 15 Sep 2026 17:45:23 GMT".to_owned(),
            },
        )];

        let mut buffer = Vec::new();
        write_no_usable_mirror(
            &mut buffer,
            &rejected,
            Some("Tue, 15 Sep 2026 17:45:23 GMT"),
        )
        .unwrap();
        let text = String::from_utf8(buffer).unwrap();

        assert!(text.contains("0x80073D06"), "got: {text}");
        assert!(text.contains("Nothing was changed"));
    }

    #[test]
    fn an_unknown_publish_time_is_reported_as_unknown_not_as_stale() {
        let rejected = vec![(
            "huaweicloud".to_owned(),
            osdk_core::syspkg::Infeasible::PublishTimeUnknown {
                endpoint: "https://mirrors.huaweicloud.com/winget-source".to_owned(),
            },
        )];

        let mut buffer = Vec::new();
        write_no_usable_mirror(&mut buffer, &rejected, None).unwrap();
        let text = String::from_utf8(buffer).unwrap();

        assert!(text.contains("could not be established"));
        assert!(
            !text.contains("0x80073D06"),
            "an unknown time is not the same finding as a stale mirror"
        );
    }

    #[test]
    fn a_failed_rollback_is_shouted_about_rather_than_mentioned() {
        let outcome = osdk_core::syspkg::ApplyOutcome {
            completed: vec!["winget source remove --name winget".to_owned()],
            failed: Some("winget source add --name winget".to_owned()),
            exit_code: Some(-2147009274),
            rolled_back: Some(false),
        };

        let mut buffer = Vec::new();
        write_outcome(&mut buffer, &outcome).unwrap();
        let text = String::from_utf8(buffer).unwrap();

        // The host may be left without a source; the recovery command must be
        // right there rather than something the user has to look up.
        assert!(text.contains("ROLLBACK FAILED"), "got: {text}");
        assert!(text.contains("winget source reset --name winget --force"));
    }

    #[test]
    fn a_successful_rollback_says_the_source_was_restored() {
        let outcome = osdk_core::syspkg::ApplyOutcome {
            completed: vec!["winget source remove --name winget".to_owned()],
            failed: Some("winget source add --name winget".to_owned()),
            exit_code: Some(1),
            rolled_back: Some(true),
        };

        let mut buffer = Vec::new();
        write_outcome(&mut buffer, &outcome).unwrap();
        let text = String::from_utf8(buffer).unwrap();

        assert!(text.contains("rolled back"));
        assert!(!text.contains("ROLLBACK FAILED"));
    }

    #[test]
    fn a_changed_host_is_distinguished_from_a_mistyped_confirmation() {
        let mut changed = Vec::new();
        write_refusal(
            &mut changed,
            &osdk_core::syspkg::ApplyRefused::StateChanged {
                planned: "aaa".to_owned(),
                current: "bbb".to_owned(),
            },
        )
        .unwrap();
        let changed = String::from_utf8(changed).unwrap();

        let mut mistyped = Vec::new();
        write_refusal(
            &mut mistyped,
            &osdk_core::syspkg::ApplyRefused::WrongFingerprint {
                expected: "aaa".to_owned(),
                supplied: "typo".to_owned(),
            },
        )
        .unwrap();
        let mistyped = String::from_utf8(mistyped).unwrap();

        // Different causes need different remedies, so the wording must differ.
        assert!(changed.contains("sources changed"));
        assert!(mistyped.contains("does not match"));
        assert_ne!(changed, mistyped);
        for text in [&changed, &mistyped] {
            assert!(text.contains("Nothing was changed"));
        }
    }

    fn distro(manager: osdk_core::syspkg::DistroManager, present: bool) -> syspkg::DistroReport {
        syspkg::DistroReport {
            manager,
            present,
            rollback: manager.rollback(),
            full_system_upgrade_only: manager.requires_full_system_upgrade(),
        }
    }

    fn render_distro(reports: &[syspkg::DistroReport]) -> String {
        let mut buffer = Vec::new();
        write_distro_managers(&mut buffer, reports).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn a_detected_linux_manager_comes_with_the_stance_stated() {
        use osdk_core::syspkg::DistroManager;
        let text = render_distro(&[distro(DistroManager::Apt, true)]);

        assert!(text.contains("apt: present"), "got: {text}");
        // Without this line a user reasonably assumes `pkg apply` covers apt.
        assert!(
            text.contains("never installs"),
            "the boundary must be stated, got: {text}"
        );
    }

    #[test]
    fn arch_carries_its_own_projects_guidance() {
        use osdk_core::syspkg::DistroManager;
        let text = render_distro(&[distro(DistroManager::Pacman, true)]);

        assert!(
            text.contains("pacman -Syu"),
            "Arch supports only full-system upgrades; that must be passed through"
        );
    }

    #[test]
    fn only_dnf_is_described_as_transactional() {
        use osdk_core::syspkg::DistroManager;
        let dnf = render_distro(&[distro(DistroManager::Dnf, true)]);
        let apt = render_distro(&[distro(DistroManager::Apt, true)]);

        assert!(dnf.contains("transactional"), "got: {dnf}");
        assert!(
            apt.contains("none (logs only)"),
            "apt has no undo, and saying otherwise would misinform, got: {apt}"
        );
    }

    #[test]
    fn absent_managers_produce_no_section_at_all() {
        use osdk_core::syspkg::DistroManager;
        let text = render_distro(&[
            distro(DistroManager::Apt, false),
            distro(DistroManager::Pacman, false),
        ]);

        assert!(
            text.is_empty(),
            "a Windows host should not read about absent Linux managers, got: {text}"
        );
    }

    #[test]
    fn an_empty_list_is_silent() {
        assert!(render_distro(&[]).is_empty());
    }

    #[test]
    fn only_the_present_managers_are_listed() {
        use osdk_core::syspkg::DistroManager;
        let text = render_distro(&[
            distro(DistroManager::Apt, true),
            distro(DistroManager::Pacman, false),
        ]);

        assert!(text.contains("apt: present"));
        assert!(
            !text.contains("pacman:"),
            "a host with apt does not thereby have pacman, got: {text}"
        );
    }
}
