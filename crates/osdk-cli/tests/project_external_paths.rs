use std::process::Command;

fn isolated(temp: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_osdk"));
    cmd.env("OSDK_CONFIG_DIR", temp.join("config"))
        .env("OSDK_DATA_DIR", temp.join("data"))
        .env("OSDK_CACHE_DIR", temp.join("cache"))
        .env("OSDK_STORE_DIR", temp.join("store"))
        .env("OSDK_INSTALL_DIR", temp.join("installs"));
    cmd
}

#[test]
fn external_bundle_relative_deps_resolve_at_real_project_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // Real project tree.
    let app = root.join("svc");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"packageManager":"npm@11.0.0"}"#,
    )
    .unwrap();

    // Bind it, then write deps config into the bundle -- relative paths as if
    // the file lived in the project root.
    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    std::fs::write(bundle.join("osdk.toml"), "[deps.projects.'svc'.npm]\n").unwrap();

    let out = isolated(root)
        .args(["deps", "--list", "--all"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The relative project 'svc' resolved into the real tree (apps/svc), not
    // into the bundle directory.
    assert!(stdout.contains("svc"), "{stdout}");
    assert!(
        !stdout.contains("projects"),
        "bundle dir leaked into deps output"
    );
}

#[test]
fn external_bundle_skills_install_lands_in_the_real_project_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // A local skill package source outside the project.
    let src = temp.path().join("pkg");
    std::fs::create_dir_all(src.join("demo")).unwrap();
    std::fs::write(
        src.join("demo/SKILL.md"),
        "---\nname: demo\ndescription: t\n---\n# demo\n",
    )
    .unwrap();
    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");

    let out = isolated(root)
        .args(["skills", "add"])
        .arg(&src)
        .args(["-a", "claude-code", "--yes"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{out:?} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // Output landed in the real project tree...
    assert!(
        text.contains(".claude/skills") || text.contains(".claude\\skills"),
        "{text}"
    );
    assert!(root.join(".claude/skills/demo/SKILL.md").is_file());
    // ...and the bundle must not have become an agent skills root.
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    assert!(!bundle.join(".claude").exists());
    assert!(!bundle.join(".agents").exists());
}

#[test]
fn selected_local_lock_is_not_borrowed_from_another_project() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // A parent dir holds an osdk.lock that belongs to a different project.
    std::fs::write(root.join("osdk.lock"), "schema = 3\n").unwrap();
    // A nested local project with its own osdk.toml.
    let proj = root.join("work");
    std::fs::create_dir(&proj).unwrap();
    std::fs::write(proj.join("osdk.toml"), "[tools]\npython = \"3.12\"\n").unwrap();

    let out = isolated(root)
        .arg("project")
        .arg("show")
        .current_dir(proj)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // The selected project's lock must be work/osdk.lock, not the parent's.
    assert!(text.contains("osdk.lock"), "{text}");
    assert!(
        text.contains("work\\osdk.lock") || text.contains("work/osdk.lock"),
        "lock must belong to the selected project: {text}"
    );
}

#[test]
fn external_bundle_trust_gate_track_and_relocate_renews_review() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    let bundle_toml = bundle.join("osdk.toml");
    // A governed key: signature verification disabled.
    std::fs::write(&bundle_toml, "[settings]\nverify_signatures = false\n").unwrap();

    // Helper: a gated command's stderr, and whether the trust gate refused.
    let gated = |cwd: &std::path::Path| -> String {
        let out = isolated(temp.path())
            .arg("install")
            .arg("osdk-nonexistent-tool@0.0.1")
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    // (1) Untrusted bundle: the gate refuses (before any resolution attempt).
    let err = gated(root);
    assert!(
        err.to_lowercase().contains("trust"),
        "gate must refuse untrusted bundle: {err}"
    );

    // (2) Trust the physical bundle file explicitly: the gate stops refusing.
    let trust = isolated(root)
        .arg("trust")
        .arg(&bundle_toml)
        .arg("--yes")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(trust.status.success(), "{trust:?}");
    let err = gated(root);
    assert!(
        !err.to_lowercase().contains("trust"),
        "trust must clear the gate: {err}"
    );

    // (3) Tamper with governed content: record identity no longer matches.
    std::fs::write(&bundle_toml, "[settings]\nverify_signatures = true\n").unwrap();
    let err = gated(root);
    assert!(
        err.to_lowercase().contains("trust"),
        "tampered governed key must re-trigger the gate: {err}"
    );

    // (4) Relocate: the bundle file moves to a new recorded root; the old
    // record is gone and the new path must not be trusted.
    std::fs::write(&bundle_toml, "[settings]\nverify_signatures = false\n").unwrap();
    let ret = isolated(root)
        .arg("trust")
        .arg(&bundle_toml)
        .arg("--yes")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(ret.status.success(), "{ret:?}");
    let new_root = temp.path().join("moved-project");
    std::fs::create_dir(&new_root).unwrap();
    let rel = isolated(root)
        .arg("project")
        .arg("relocate")
        .arg(root)
        .arg(&new_root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(rel.status.success(), "{rel:?}");
    let err = gated(&new_root);
    assert!(
        err.to_lowercase().contains("trust"),
        "after relocate the new bundle path must be untrusted: {err}"
    );
}

#[test]
fn project_scoped_config_set_writes_bundle_not_project_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("real-proj");
    std::fs::create_dir_all(&project).unwrap();
    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");

    // Locate the bundle config.
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    let bundle_cfg = bundle.join("osdk.toml");

    // Project-scoped write.
    let set = isolated(root)
        .arg("config")
        .arg("set")
        .arg("offline")
        .arg("true")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(set.status.success(), "{set:?}");

    // Bundle config now carries the setting; the real project tree stays clean.
    let text = std::fs::read_to_string(&bundle_cfg).unwrap();
    assert!(
        text.contains("offline"),
        "bundle config must record the setting: {text}"
    );
    assert!(
        !project.join("osdk.toml").exists(),
        "must not create a local osdk.toml that shadows the bundle"
    );
}

#[test]
fn shim_dispatch_under_external_binding_has_clean_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("real-proj");
    std::fs::create_dir_all(&project).unwrap();
    assert!(isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());

    // The shim binary itself, invoked from within the external-binding project,
    // must exit 0 and keep stdout clean (diagnostics go to stderr only).
    let shim_exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_osdk"))
        .parent()
        .unwrap()
        .join(if cfg!(windows) {
            "osdk-shim.exe"
        } else {
            "osdk-shim"
        });
    let shim = std::process::Command::new(&shim_exe)
        .env("OSDK_CONFIG_DIR", root.join("config"))
        .env("OSDK_DATA_DIR", root.join("data"))
        .env("OSDK_CACHE_DIR", root.join("cache"))
        .env("OSDK_STORE_DIR", root.join("store"))
        .env("OSDK_INSTALL_DIR", root.join("installs"))
        .arg("--version")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(shim.status.success(), "shim must exit 0: {:?}", shim);
    assert!(
        !shim.stdout.is_empty(),
        "shim --version must print its version"
    );
    // No warnings leaked onto stdout.
    let out = String::from_utf8_lossy(&shim.stdout);
    assert!(
        !out.to_lowercase().contains("warning"),
        "stdout must stay clean: {out}"
    );
}
