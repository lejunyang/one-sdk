use super::*;
use serde_json::{json, Value};

fn diagnose(root: &Path, tool: &str) -> (Output, Value) {
    let output =
        run_isolated_in_with_env(root, root, &["current", tool, "--sources", "--json"], &[]);
    let report = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (output, report)
}
fn source<'a>(report: &'a Value, kind: &str) -> &'a Value {
    report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == kind)
        .unwrap()
}

#[test]
fn source_diagnostics_separates_capabilities_files_and_evidence() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    std::fs::write(
        root.join("osdk.toml"),
        "[tools]\nnode='20'\n[settings.precedence]\ntool_order=['.node-version','.nvmrc']\n",
    )
    .unwrap();
    std::fs::write(root.join(".node-version"), "22\n").unwrap();
    std::fs::write(root.join("package.json"), "{}").unwrap();
    let (out, report) = diagnose(root, "node");
    assert!(out.status.success());
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["inventory_complete"], true);
    assert_eq!(
        report["configured_order"],
        json!([".node-version", ".nvmrc"])
    );
    assert_eq!(report["effective_order"].as_array().unwrap().len(), 10);
    assert_eq!(report["effective_order"][2], "osdk");
    assert_eq!(source(&report, "system")["state"], "disabled");
    assert_eq!(source(&report, "idiomatic")["state"], "inapplicable");
    assert_eq!(source(&report, ".nvmrc")["state"], "missing");
    assert_eq!(
        source(&report, "package.json#engines.node")["state"],
        "missing"
    );
    assert_eq!(
        source(&report, "package.json#engines.node")["locations"][0]["file_exists"],
        true
    );
    assert_eq!(report["selected"]["value"], "22");
    assert!(report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["status"] == "winner" && e["category"] == ".node-version"));
    assert!(report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["status"] == "shadowed" && e["category"] == "osdk"));
    // No matching manager declaration is not the same as a missing manifest.
    std::fs::write(
        root.join("package.json"),
        r#"{"packageManager":"yarn@https://private:secret@example.test/manager"}"#,
    )
    .unwrap();
    let (out, manager) = diagnose(root, "pnpm");
    assert!(out.status.success());
    assert_eq!(
        source(&manager, "package.json#packageManager")["state"],
        "present"
    );
    assert_eq!(manager["evidence"][0]["status"], "ignored");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret"));
    assert!(!root.join("osdk.lock").exists());
    assert!(!root.join("installs/node").exists());
}

#[test]
fn source_diagnostics_project_inheritance_clear_and_backend_files() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let project = root.join("nested project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        root.join("osdk.toml"),
        "[settings.precedence]\ntool_order=['system','idiomatic']\n",
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "extends='../osdk.toml'\n[tools]\npython='3.12'\n",
    )
    .unwrap();
    let out = run_isolated_in_with_env(
        root,
        root,
        &[
            "current",
            "python",
            "--sources",
            "--json",
            "--project",
            project.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["effective_order"][0], "system");
    assert_eq!(
        source(&report, "osdk")["locations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(source(&report, ".nvmrc")["state"], "inapplicable");
    assert!(source(&report, "idiomatic")["locations"][0]["path"]
        .as_str()
        .unwrap()
        .ends_with(".python-version"));
    std::fs::write(
        project.join("osdk.toml"),
        "extends='../osdk.toml'\n[settings.precedence]\ntool_order=[]\n",
    )
    .unwrap();
    let cleared = run_isolated_in_with_env(
        root,
        root,
        &[
            "current",
            "rust",
            "--sources",
            "--json",
            "--project",
            project.to_str().unwrap(),
        ],
        &[],
    );
    assert!(cleared.status.success());
    let report: Value = serde_json::from_slice(&cleared.stdout).unwrap();
    assert_eq!(report["configured_order"], json!([]));
    assert_eq!(source(&report, "system")["state"], "inapplicable");
    assert_eq!(report["effective_order"][0], "osdk");
    let locations = source(&report, "idiomatic")["locations"]
        .as_array()
        .unwrap();
    assert!(locations[0]["path"]
        .as_str()
        .unwrap()
        .ends_with("rust-toolchain.toml"));
    let first_plain = locations
        .iter()
        .position(|p| p["path"].as_str().unwrap().ends_with("rust-toolchain"))
        .unwrap();
    assert!(first_plain > 0);
    // Python ignores malformed Node metadata.
    std::fs::write(root.join("package.json"), "malformed").unwrap();
    let (out, _) = diagnose(root, "python");
    assert!(out.status.success());
}

#[test]
fn source_diagnostics_strict_error_and_invalid_declaration_never_claim_complete_selection() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    std::fs::write(
        root.join("osdk.toml"),
        "[tools]\nnode='20'\n[settings.precedence]\ntool_conflicts='error'\n",
    )
    .unwrap();
    std::fs::write(root.join(".nvmrc"), "22").unwrap();
    let (out, report) = diagnose(root, "node");
    assert!(!out.status.success());
    assert_eq!(report["inventory_complete"], true);
    assert_eq!(report["resolution_status"], "error");
    assert!(report["selected"].is_null());
    assert!(!report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["status"] == "winner"));
    std::fs::write(
        root.join(".nvmrc"),
        "https://secret:password@example.test/node",
    )
    .unwrap();
    let (out, report) = diagnose(root, "node");
    assert!(!out.status.success());
    assert_eq!(report["inventory_complete"], false);
    assert_eq!(source(&report, ".nvmrc")["state"], "unknown");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("password"));
    std::fs::write(
        root.join("osdk.toml"),
        "[settings.precedence]\ntool_order=['system','system']\n",
    )
    .unwrap();
    let out =
        run_isolated_in_with_env(root, root, &["current", "node", "--sources", "--json"], &[]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate"));
}

#[cfg(unix)]
#[test]
fn source_diagnostics_never_probes_without_explicit_flag_even_under_strict_policy() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = root.join("host/bin");
    let marker = root.join("executed");
    // Use a shell builtin so PATH can remain isolated from all host commands.
    write_executable(
        &bin.join("node"),
        &format!(
            "#!/bin/sh\n: > '{}'\n[ \"$1\" = '--version' ] || exit 70\nprintf 'v22.3.0\\n'\n",
            marker.display()
        ),
    );
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='22'\n[settings.precedence]\ntool_order=['system']\ntool_conflicts='error'\n").unwrap();
    let env = [
        ("PATH", bin.to_str().unwrap()),
        ("OSDK_TRUSTED_CONFIG_PATHS", root.to_str().unwrap()),
    ];
    let out = run_isolated_in_with_env(
        root,
        root,
        &["current", "node", "--sources", "--json"],
        &env,
    );
    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["resolution_status"], "verification-required");
    assert_eq!(source(&report, "system")["state"], "present");
    assert_eq!(report["selected"]["value"], Value::Null);
    assert!(!marker.exists());
    let out = run_isolated_in_with_env(
        root,
        root,
        &["current", "node", "--sources", "--json", "--probe"],
        &env,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["resolution_status"], "selected");
    assert_eq!(report["selected"]["value"], "22.3.0");
    assert!(marker.exists());
    assert!(!root.join("installs/node").exists());
}

#[test]
fn source_diagnostics_flags_require_tool_and_sources() {
    let t = tempfile::tempdir().unwrap();
    for args in [
        vec!["current", "--sources"],
        vec!["current", "node", "--json"],
        vec!["current", "node", "--project", "."],
        vec!["current", "node", "--probe"],
    ] {
        let out = run_isolated_in_with_env(t.path(), t.path(), &args, &[]);
        assert!(!out.status.success(), "{args:?}");
    }
}

#[test]
fn source_diagnostics_unavailable_and_unsupported_system_are_distinct() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    std::fs::write(
        root.join("osdk.toml"),
        "[settings.precedence]\ntool_order=['system']\ntool_conflicts='error'\n",
    )
    .unwrap();
    let (out, report) = diagnose(root, "node");
    assert!(out.status.success());
    assert_eq!(source(&report, "system")["state"], "missing");
    assert_eq!(report["resolution_status"], "none");
    let (out, report) = diagnose(root, "rust");
    assert!(out.status.success());
    assert_eq!(source(&report, "system")["state"], "inapplicable");
    assert_eq!(source(&report, "system")["supported"], false);
    assert_eq!(source(&report, "system")["rank"], 1);
}

#[cfg(windows)]
#[test]
fn source_diagnostics_windows_exe_discovery_does_not_execute_candidate() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = root.join("host with spaces");
    std::fs::create_dir_all(&bin).unwrap();
    // An invalid PE proves discovery never starts it; an explicit probe fails.
    std::fs::write(bin.join("node.EXE"), b"not a PE executable").unwrap();
    std::fs::write(
        root.join("osdk.toml"),
        "[settings.precedence]\ntool_order=['system']\n",
    )
    .unwrap();
    let env = [
        ("PATH", bin.to_str().unwrap()),
        ("OSDK_TRUSTED_CONFIG_PATHS", root.to_str().unwrap()),
    ];
    let out = run_isolated_in_with_env(
        root,
        root,
        &["current", "node", "--sources", "--json"],
        &env,
    );
    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(source(&report, "system")["state"], "present");
    assert!(report["selected"]["value"].is_null());
    let out = run_isolated_in_with_env(
        root,
        root,
        &["current", "node", "--sources", "--json", "--probe"],
        &env,
    );
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["resolution_status"], "error");
    assert!(report["selected"].is_null());
}
