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
fn add_show_list_remove_roundtrip_with_spaces_and_unicode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // Space + non-ASCII in the project path.
    let project = root.join("my 项目 svc");
    std::fs::create_dir_all(&project).unwrap();

    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");

    let show = isolated(root)
        .arg("project")
        .arg("show")
        .arg(&project)
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let json: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(json["source"], "external");
    assert_eq!(json["root"], project.to_str().unwrap());
    assert_eq!(json["missing"], false);
    assert_eq!(json["shadowed_local_config"], serde_json::Value::Null);

    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["root"], project.to_str().unwrap());

    // The bundle owns its own osdk.lock; nothing landed in the project tree.
    assert!(project.join("osdk.toml").exists() == false);
    assert!(project.join("osdk.lock").exists() == false);
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("exactly one bundle directory");
    assert!(
        bundle.join("osdk.lock").exists(),
        "bundle should carry its own lock"
    );

    let removed = isolated(root)
        .arg("project")
        .arg("remove")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(removed.status.success(), "{removed:?}");
    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert!(items.is_empty(), "binding removed: {items:?}");
}

#[test]
fn removed_project_root_is_reported_missing_but_does_not_break_other_cwds() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("bound-project");
    std::fs::create_dir_all(&project).unwrap();
    isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap()
        .status;

    // The project tree disappears; the binding becomes "missing".
    std::fs::remove_dir_all(&project).unwrap();

    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(list.status.success(), "{list:?}");
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["root_exists"], false);

    // A cwd with no binding and no local config simply has no project context.
    let plain = root.join("plain-dir");
    std::fs::create_dir_all(&plain).unwrap();
    let show = isolated(root)
        .arg("project")
        .arg("show")
        .current_dir(&plain)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
}

#[test]
fn remove_is_dry_run_first_class() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("svc");
    std::fs::create_dir_all(&project).unwrap();
    isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap()
        .status;

    let dry = isolated(root)
        .arg("project")
        .arg("remove")
        .arg(&project)
        .arg("--dry-run")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(dry.status.success(), "{dry:?}");
    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(items.len(), 1, "dry run must not delete");
}

#[test]
fn broken_binding_is_reported_not_silently_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("svc");
    std::fs::create_dir_all(&project).unwrap();
    let added = isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");

    // Corrupt the manifest of the bundle.
    let bundle = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("exactly one bundle directory");
    std::fs::write(bundle.join("project.toml"), "= not toml =").unwrap();

    let show = isolated(root)
        .arg("project")
        .arg("show")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        !show.status.success(),
        "broken selected binding must hard-fail"
    );
}
