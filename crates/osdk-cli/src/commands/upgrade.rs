//! User-global upgrades follow only the user's configured constraints.

use super::*;

pub async fn upgrade_global(app: &mut App, tools: Vec<String>) -> Result<()> {
    let requests = global_upgrade_requests(app, &tools)?;
    if requests.is_empty() {
        println!("{}", t!("msg.nothing_to_install"));
        return Ok(());
    }
    let (npm, mut isolated): (Vec<_>, Vec<_>) = requests
        .into_iter()
        .partition(|request| request.backend.starts_with("npm:"));
    // Supply runtime declarations explicitly so dependency injection cannot pick
    // a project version file from the process working directory.
    for (runtime, needed) in [
        (
            "node",
            isolated
                .iter()
                .any(|r| matches!(r.backend.as_str(), "npm" | "pnpm" | "yarn")),
        ),
        (
            "rust",
            isolated.iter().any(|r| r.backend.starts_with("cargo:")),
        ),
        ("go", isolated.iter().any(|r| r.backend.starts_with("go:"))),
    ] {
        if needed && !isolated.iter().any(|r| r.backend == runtime) {
            let dependency = global_upgrade_requests(app, &[runtime.into()]);
            isolated.push(match dependency {
                Ok(mut requests) => requests.remove(0),
                Err(error) => {
                    return Err(error.context(format!(
                        "global tools require a global managed {runtime} selection"
                    )))
                }
            });
        }
    }
    if !isolated.is_empty() {
        let resolved = install_requests_fresh(app, isolated, Vec::new()).await?;
        crate::lockfile::upsert_resolved_many_with_scope(
            &app.ctx.dirs.user_lock_file(),
            app.ctx.platform,
            &app.ctx.dirs,
            &resolved,
            crate::lockfile::LockScope::Global,
        )?;
    }
    // The controlled npm/pnpm global prefix has its own transaction and native
    // lock. Never send these requests through the isolated project installer.
    for request in npm {
        crate::global_npm_use::upgrade(app, request).await?;
    }
    println!("updated {}", app.ctx.dirs.user_lock_file().display());
    Ok(())
}

fn global_upgrade_requests(app: &App, tools: &[String]) -> Result<Vec<ToolRequest>> {
    let mut configured = Vec::new();
    for (key, entry) in &app.ctx.config.global_tool_configs {
        if app.ctx.config.excluded_tools.contains_key(key) {
            continue;
        }
        let (mut request, indirect) = resolve_explicit_request_target(
            app,
            key,
            &app.ctx.config.global_tool_configs,
            &app.ctx.config.global_tools,
        )?;
        if indirect.is_none() {
            request.spec = VersionSpec::parse(entry.version());
        }
        request.backend = app.registry.get(&request.backend)?.id().to_string();
        if indirect.is_some() && request.backend.starts_with("npm:") {
            anyhow::bail!("global npm package aliases are not supported; use the canonical npm:<package> request");
        }
        request.options = entry.to_request_options();
        reject_public_internal_options(&request.options)?;
        configured.push((key, request));
    }
    if tools.is_empty() {
        let mut ids = std::collections::BTreeSet::new();
        for (_, request) in &configured {
            if !ids.insert(&request.backend) {
                anyhow::bail!(
                    "multiple global selections name {}; upgrade each configured alias separately",
                    request.backend
                );
            }
        }
        return Ok(configured.into_iter().map(|(_, request)| request).collect());
    }
    let mut requests = Vec::new();
    for tool in tools {
        let parsed = ToolRequest::parse(tool)?;
        if requested_spec_literal(tool).is_some() || !parsed.options.is_empty() {
            anyhow::bail!("global upgrade follows configured constraints; use `osdk use -g {tool}` to change the selection or options");
        }
        let canonical = app.registry.get(tool).ok().map(|b| b.id().to_string());
        let named = configured.iter().find(|(key, _)| key.as_str() == tool);
        let matching: Vec<_> = match named {
            Some((_, request)) => vec![request],
            None => configured
                .iter()
                .filter_map(|(_, request)| {
                    (Some(&request.backend) == canonical.as_ref()).then_some(request)
                })
                .collect(),
        };
        match matching.as_slice() {
            [request] => {
                if !requests.iter().any(|r: &ToolRequest| r.backend == request.backend) {
                    requests.push((*request).clone());
                }
            }
            [] => anyhow::bail!("{tool} is not selected globally on this platform; run `osdk use -g {tool}@latest` first"),
            _ => anyhow::bail!("multiple global selections name {tool}; pass the configured alias"),
        }
    }
    Ok(requests)
}
