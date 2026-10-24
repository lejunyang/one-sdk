//! Read-only source inventory, kept separate from executable selection evidence.
use super::*;
use crate::config::ToolSource;
use crate::version::resolver;
use serde::Serialize;

#[derive(Serialize)]
pub struct SourceLocation {
    path: PathBuf,
    file_exists: bool,
}
#[derive(Serialize)]
pub struct SourceInventory {
    source: ToolSource,
    supported: bool,
    enabled: bool,
    rank: Option<usize>,
    /// present/missing describe declarations, not merely file existence.
    state: &'static str,
    reason: String,
    locations: Vec<SourceLocation>,
}
#[derive(Serialize)]
pub struct Selected {
    kind: &'static str,
    value: Option<String>,
    executable: Option<PathBuf>,
}
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool: String,
    pub project: PathBuf,
    pub configured_order: Vec<ToolSource>,
    pub effective_order: Vec<ToolSource>,
    pub tool_conflicts: ToolConflictPolicy,
    pub inventory_complete: bool,
    pub sources: Vec<SourceInventory>,
    pub evidence: Vec<SelectionEvidence>,
    pub selected: Option<Selected>,
    /// selected, none, error, or verification-required. Never claims strict
    /// system compatibility from filesystem discovery alone.
    pub resolution_status: &'static str,
    pub error: Option<String>,
    pub notes: Vec<String>,
}

fn files_for(backend: &dyn Backend, source: ToolSource) -> Vec<&str> {
    use ToolSource::*;
    match source {
        Osdk => vec!["osdk.toml", ".osdk.toml"],
        ToolVersions => vec![".tool-versions"],
        Nvmrc | NodeVersion | Idiomatic => backend
            .idiomatic_files()
            .iter()
            .copied()
            .filter(|name| match *name {
                ".nvmrc" => source == Nvmrc,
                ".node-version" => source == NodeVersion,
                _ => source == Idiomatic,
            })
            .collect(),
        PackageManager | DevPackageManager | Engines | DevRuntime => vec!["package.json"],
        _ => vec![],
    }
}

fn supported(backend: &dyn Backend, source: ToolSource) -> bool {
    use ToolSource::*;
    match source {
        System => adapter(backend.id()).is_some(),
        Osdk | Global | ToolVersions => true,
        Nvmrc | NodeVersion | Idiomatic => !files_for(backend, source).is_empty(),
        PackageManager | DevPackageManager => matches!(backend.id(), "npm" | "pnpm" | "yarn"),
        Engines | DevRuntime => backend.id() == "node",
    }
}

fn safe_value(value: &str) -> String {
    // Numeric selectors only. Opaque aliases/channels and URL-like values can
    // carry credentials; their locations and statuses suffice for diagnosis.
    if value.len() <= 256
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ".xX* +-<>=~^|".contains(c))
    {
        value.into()
    } else {
        "[opaque selector]".into()
    }
}

pub fn inspect(ctx: &Ctx, backend: &dyn Backend, cwd: &Path, probe: bool) -> Report {
    use ToolSource::*;
    let policy = &ctx.config.settings.precedence;
    let mut report = Report {
        schema_version: 1,
        tool: backend.id().into(),
        project: cwd.into(),
        configured_order: policy.tool_order.clone(),
        effective_order: policy.ordered_tool_sources(),
        tool_conflicts: policy.tool_conflicts,
        inventory_complete: true,
        sources: vec![], evidence: vec![], selected: None,
        resolution_status: "none", error: None,
        notes: vec!["Inventory describes this tool's declarations; file_exists alone does not mean a usable declaration. Explicit CLI versions and lock replay have separate authority.".into()],
    };
    let mut config = ctx.config.clone();
    config.settings.precedence.tool_conflicts = ToolConflictPolicy::Prefer;
    // Gather validated declarations without letting strict cross-kind conflicts
    // erase the inventory. Invalid declarations still make it incomplete.
    let declarations =
        resolver::resolve_selection(backend.id(), cwd, &config, backend.idiomatic_files());
    if let Ok(selection) = &declarations {
        report.evidence.clone_from(&selection.evidence);
    } else {
        report.inventory_complete = false;
    }
    let all = [
        System,
        Osdk,
        Global,
        ToolVersions,
        Nvmrc,
        NodeVersion,
        Idiomatic,
        PackageManager,
        DevPackageManager,
        Engines,
        DevRuntime,
    ];
    let mut system_present = false;
    let mut system_discovery_error = false;
    for source in all {
        let supported = supported(backend, source);
        let rank = report
            .effective_order
            .iter()
            .position(|s| *s == source)
            .map(|r| r + 1);
        let enabled = supported && rank.is_some();
        let mut locations = vec![];
        let (state, reason) = if !supported {
            (
                "inapplicable",
                "backend has no adapter for this source".into(),
            )
        } else if !enabled {
            (
                "disabled",
                "system is opt-in; omitted sources never enable it".into(),
            )
        } else if source == System {
            match discover(ctx, backend.id(), cwd) {
                Ok(Some(target)) => {
                    system_present = true;
                    locations.push(target.executable);
                    (
                        "present",
                        format!(
                            "{}; executable discovered, not version-validated",
                            target.origin
                        ),
                    )
                }
                Ok(None) => ("missing", "no eligible external executable".into()),
                Err(error) => {
                    system_discovery_error = true;
                    report.inventory_complete = false;
                    ("error", error.to_string())
                }
            }
        } else {
            if source == Global {
                locations.push(
                    ctx.config
                        .global_config_path
                        .clone()
                        .unwrap_or_else(|| ctx.dirs.config.join("config.toml")),
                );
            } else if source == Osdk {
                if let Some(path) = &ctx.config.project_config_path {
                    // Already validated during config loading; surface any unexpected error.
                    let rel_base = ctx
                        .config
                        .project
                        .as_ref()
                        .filter(|p| p.is_external())
                        .map(|p| p.root.as_path());
                    match crate::config::inheritance::sources(path, rel_base) {
                        Ok(paths) => locations.extend(paths),
                        Err(_) => report.inventory_complete = false,
                    }
                } else {
                    locations.extend(cwd.ancestors().flat_map(|dir| {
                        files_for(backend, source)
                            .into_iter()
                            .map(move |name| dir.join(name))
                    }));
                }
            } else {
                // Backend filename priority comes before ancestor distance.
                for name in files_for(backend, source) {
                    locations.extend(cwd.ancestors().map(|dir| dir.join(name)));
                }
            }
            if declarations.is_err() {
                (
                    "unknown",
                    "declaration validation failed; inventory is incomplete".into(),
                )
            } else if report.evidence.iter().any(|e| e.category == source) {
                (
                    "present",
                    "see resolution evidence for winner/shadowed/ignored declarations".into(),
                )
            } else {
                (
                    "missing",
                    "no declaration for this tool; an existing file can omit its field".into(),
                )
            }
        };
        report.sources.push(SourceInventory {
            source,
            supported,
            enabled,
            rank,
            state,
            reason,
            locations: locations
                .into_iter()
                .map(|path| SourceLocation {
                    file_exists: path.is_file(),
                    path,
                })
                .collect(),
        });
    }
    // Use exactly the execution resolver, including its strict managed conflict
    // checks. The no-probe variant does not validate system compatibility.
    let selection = if probe {
        resolve(ctx, backend, cwd)
    } else {
        resolve_without_probes(ctx, backend, cwd)
    };
    match selection {
        Err(error) => {
            report.resolution_status = "error";
            report.error = Some(error.to_string());
            // Prefer-order evidence is still useful but must not claim a winner.
            for evidence in &mut report.evidence {
                if evidence.status == "winner" {
                    evidence.status = "candidate".into();
                    evidence.reason = "highest priority declaration; resolution failed".into();
                }
            }
        }
        Ok(mut selection) => {
            if probe {
                if let Some(ExecutionTarget::System(target)) = &mut selection.target {
                    if target.version.is_none() {
                        let verification = (|| {
                            let mut bins = vec![target.bin_dir().to_path_buf()];
                            let mut env = BTreeMap::new();
                            if matches!(backend.id(), "npm" | "pnpm" | "yarn") {
                                let registry = crate::backend::registry::Registry::new();
                                let (runtime_bins, runtime_env) =
                                    runtime_environment(ctx, &registry, cwd, "node")?;
                                bins.splice(0..0, runtime_bins);
                                env.extend(runtime_env);
                            }
                            env.insert("PATH".into(), path_with(ctx, bins)?);
                            target.verify(ctx, &env)
                        })();
                        if let Err(error) = verification {
                            report.resolution_status = "error";
                            report.error = Some(error.to_string());
                            report.evidence = selection.evidence;
                            for e in &mut report.evidence {
                                if e.status == "winner" {
                                    e.status = "candidate".into();
                                    e.reason = "version verification failed".into();
                                }
                            }
                            return report;
                        }
                    }
                }
            }
            if let Some(ExecutionTarget::System(target)) = &selection.target {
                if target.version.is_some() {
                    for e in &mut selection.evidence {
                        if e.category == System && e.status == "winner" {
                            e.reason = "verified external executable".into();
                        }
                    }
                }
            }
            report.evidence = selection.evidence;
            report.selected = selection.target.map(|target| match target {
                ExecutionTarget::Managed(active) => Selected {
                    kind: "managed",
                    value: Some(safe_value(&active.spec)),
                    executable: None,
                },
                ExecutionTarget::System(target) => Selected {
                    kind: "system",
                    value: target.version,
                    executable: Some(target.executable),
                },
            });
            report.resolution_status = if !probe
                && policy.tool_conflicts == ToolConflictPolicy::Error
                && enabled(ctx)
                && (system_present || system_discovery_error)
            {
                // Even a malformed lower-ranked home would fail strict execution;
                // the non-probing selection deliberately does not inspect it.
                "verification-required"
            } else if report.selected.is_some() {
                "selected"
            } else {
                "none"
            };
        }
    }
    if !probe && (system_present || report.resolution_status == "verification-required") {
        report.notes.push("No executables were run. System identity/version and strict system compatibility remain unverified; use --probe to permit bounded native probes (including the selected Node for JS managers).".into());
    }
    if report.resolution_status == "verification-required" {
        for e in &mut report.evidence {
            if e.status == "winner" {
                e.status = "candidate".into();
                e.reason = "provisional order winner; strict system verification deferred".into();
            }
        }
    }
    report
}
