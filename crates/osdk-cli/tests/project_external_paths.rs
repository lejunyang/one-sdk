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
