//! `registry` command handlers (split from commands.rs).

use super::*;

pub async fn registry(app: &mut App, command: RegistryCommand) -> Result<()> {
    match command {
        RegistryCommand::Test { manager } => {
            let cwd = std::env::current_dir().context("getting current dir for registry test")?;
            // `python` is not an npm-family manager, so it has to be recognized
            // before the argument reaches that parser -- otherwise asking for it
            // fails with `unknown package manager` and the Python probe, which is
            // the whole point of the request, never runs.
            let python_requested = manager.is_none() || manager.as_deref() == Some("python");
            let managers = if manager.as_deref() == Some("python") {
                Vec::new()
            } else {
                registry_test_managers(manager.as_deref(), &cwd)?
            };
            let mut unavailable = Vec::new();
            for manager in managers {
                let (executable, args) = registry_test_invocation(manager);
                let plan =
                    package_registry::plan(&app.ctx, &cwd, manager, executable, &args, |key| {
                        std::env::var(key).ok()
                    })
                    .await?;
                print_registry_plan(manager, &plan);
                if matches!(plan, RegistryPlan::Unavailable { .. }) {
                    unavailable.push(manager.to_string());
                }
            }
            // Python indexes are probed alongside the npm-family registries.
            // The probing logic already existed but was never reachable from
            // any command, so a configured mirror could not be verified -- the
            // user found out whether it worked by attempting an install.
            if python_requested {
                let config = app.ctx.config.registries().python.clone();
                // The same candidate list the install path resolves against,
                // including the built-in mirrors when nothing is configured.
                // Probing `config.urls` directly would report on an empty set and
                // print "no Python index mirrors configured" while installs were in
                // fact ranking mirrors -- a diagnostic disagreeing with the thing it
                // diagnoses.
                let candidates = osdk_core::python_index::effective_candidates(&config.urls);
                let plan =
                    osdk_core::python_index::plan(&candidates, config.probe_timeout_ms).await;
                print_python_index_plan(&plan);
                if matches!(plan, osdk_core::python_index::IndexPlan::Unavailable { .. }) {
                    unavailable.push("python".to_string());
                }
            }
            if unavailable.is_empty() {
                Ok(())
            } else {
                Err(anyhow!(t!(
                    "err.registry_test_unavailable",
                    managers = unavailable.join(", ")
                )))
            }
        }
    }
}

pub(crate) fn registry_test_managers(
    requested: Option<&str>,
    cwd: &std::path::Path,
) -> Result<Vec<PackageManager>> {
    let project_yarn = project_yarn_version(cwd);
    let yarn_manager = project_yarn
        .as_deref()
        .and_then(|version| package_registry::manager_for_command("yarn", Some(version)));
    match requested.map(|value| value.to_ascii_lowercase()) {
        None => {
            let mut managers = vec![PackageManager::Npm, PackageManager::Pnpm];
            if let Some(manager) = yarn_manager {
                managers.push(manager);
            } else {
                managers.extend([PackageManager::YarnClassic, PackageManager::YarnBerry]);
            }
            managers.extend([PackageManager::Bun, PackageManager::Deno]);
            Ok(managers)
        }
        Some(value) if matches!(value.as_str(), "yarn" | "yarnpkg") => Ok(yarn_manager
            .map_or_else(
                || vec![PackageManager::YarnClassic, PackageManager::YarnBerry],
                |manager| vec![manager],
            )),
        Some(value) => value
            .parse::<PackageManager>()
            .map(|manager| vec![manager])
            .map_err(|error| anyhow!(error)),
    }
}

pub(crate) fn registry_test_invocation(manager: PackageManager) -> (&'static str, Vec<String>) {
    match manager {
        PackageManager::Npm => ("npm", vec!["install".into()]),
        PackageManager::Pnpm => ("pnpm", vec!["install".into()]),
        PackageManager::YarnClassic | PackageManager::YarnBerry => ("yarn", vec!["install".into()]),
        PackageManager::Bun => ("bun", vec!["install".into()]),
        PackageManager::Deno => ("deno", vec!["add".into(), "npm:probe".into()]),
    }
}

/// Report the Python index plan in the same shape as the npm-family output.
///
/// A pass-through here means "no mirror configured", which is materially
/// different from a mirror that failed: the first is the default state and the
/// second is a problem the user needs to see.
pub(crate) fn print_python_index_plan(plan: &osdk_core::python_index::IndexPlan) {
    use osdk_core::python_index::IndexPlan;

    println!("{}", t!("msg.registry_manager_header", manager = "python"));
    let probes = match plan {
        IndexPlan::PassThrough { reason } => {
            println!("  {}: {reason}", t!("label.registry_pass_through"));
            return;
        }
        IndexPlan::Selected { probes, .. } | IndexPlan::Unavailable { probes } => probes,
    };
    for probe in probes {
        if probe.ok {
            let latency = probe
                .latency_ms
                .map(|latency| format!("{latency} ms"))
                .unwrap_or_else(|| t!("label.registry_ok"));
            println!(
                "  {:<12} {latency:>8}  {}",
                t!("label.registry_healthy"),
                probe.url
            );
        } else {
            println!(
                "  {:<12}          {}{}",
                t!("label.registry_unavailable"),
                probe.url,
                probe
                    .error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            );
        }
    }
    if let Some(url) = plan.selected_url() {
        println!("  {}: {url}", t!("label.registry_selected"));
    }
}

pub(crate) fn print_registry_plan(manager: PackageManager, plan: &RegistryPlan) {
    println!("{}", t!("msg.registry_manager_header", manager = manager));
    let probes = match plan {
        RegistryPlan::PassThrough { reason } => {
            println!("  {}: {reason}", t!("label.registry_pass_through"));
            return;
        }
        RegistryPlan::Selected { probes, .. } | RegistryPlan::Unavailable { probes } => probes,
    };
    for probe in probes {
        if probe.ok {
            let latency = probe
                .latency_ms
                .map(|latency| format!("{latency} ms"))
                .unwrap_or_else(|| t!("label.registry_ok"));
            println!(
                "  {:<12} {latency:>8}  {}",
                t!("label.registry_healthy"),
                probe.url
            );
        } else {
            println!(
                "  {:<12}          {}{}",
                t!("label.registry_unavailable"),
                probe.url,
                probe
                    .error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            );
        }
    }
    match plan {
        RegistryPlan::Selected { url, .. } => {
            println!(
                "  {:<12} {url} ({})",
                t!("label.registry_selected"),
                package_registry::registry_env(manager)
            );
        }
        RegistryPlan::Unavailable { .. } => println!(
            "  {:<12} {}",
            t!("label.registry_unavailable"),
            t!("msg.registry_no_healthy_candidate")
        ),
        RegistryPlan::PassThrough { .. } => unreachable!(),
    }
}
