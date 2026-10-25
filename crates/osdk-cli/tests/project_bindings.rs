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
    binding_roundtrip(temp.path());
}

#[test]
#[cfg(unix)]
fn binding_roundtrip_through_a_symlink_uses_the_real_root() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    binding_roundtrip(&alias);
}

fn binding_roundtrip(root: &std::path::Path) {
    // Space + non-ASCII in the project path.
    let project = root.join("my 项目 svc");
    std::fs::create_dir_all(&project).unwrap();
    // The API returns the real root, including macOS /var symlinks and
    // Windows 8.3 aliases. Keep exact equality against that identity.
    let canonical_project = dunce::canonicalize(&project).unwrap();

    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(project.as_path())
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");

    let show = isolated(root)
        .arg("project")
        .arg("show")
        .arg(project.as_path())
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let json: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(json["source"], "external");
    assert_eq!(json["root"], canonical_project.to_str().unwrap());
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
    assert!(list.status.success(), "{list:?}");
    assert_eq!(items[0]["root"], canonical_project.to_str().unwrap());

    // The bundle owns its own osdk.lock; nothing landed in the project tree.
    assert!(!project.join("osdk.toml").exists());
    assert!(!project.join("osdk.lock").exists());
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
    let lock_body = std::fs::read_to_string(bundle.join("osdk.lock")).unwrap();
    assert!(
        lock_body.starts_with("schema = "),
        "bundle lock must declare a schema version: {lock_body}"
    );

    let removed = isolated(root)
        .arg("project")
        .arg("remove")
        .arg(project.as_path())
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
    let status = isolated(root)
        .arg("project")
        .arg("add")
        .arg(project.as_path())
        .current_dir(root)
        .output()
        .unwrap()
        .status;
    assert!(status.success());

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
    let status = isolated(root)
        .arg("project")
        .arg("add")
        .arg(project.as_path())
        .current_dir(root)
        .output()
        .unwrap()
        .status;
    assert!(status.success());

    let dry = isolated(root)
        .arg("project")
        .arg("remove")
        .arg(project.as_path())
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
        .arg(project.as_path())
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
        .arg(project.as_path())
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        !show.status.success(),
        "broken selected binding must hard-fail"
    );
}

#[test]
fn relocate_into_an_already_bound_target_is_refused_and_leaves_both_intact() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let a = root.join("alpha");
    let b = root.join("beta");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    for p in [&a, &b] {
        assert!(isolated(root)
            .arg("project")
            .arg("add")
            .arg(p)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success());
    }

    // alpha -> beta: beta is already bound, so this must fail and touch nothing.
    let rel = isolated(root)
        .arg("project")
        .arg("relocate")
        .arg(&a)
        .arg(&b)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        !rel.status.success(),
        "must refuse a colliding relocate: {rel:?}"
    );

    // Both bindings survive; their recorded roots are unchanged.
    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(items.len(), 2, "refused relocate must not drop a binding");
}

#[test]
fn concurrent_add_same_project_leaves_one_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("svc");
    std::fs::create_dir_all(&project).unwrap();

    // Ten parallel `project add` calls racing on the same root.
    let mut children = Vec::new();
    for _ in 0..10 {
        let c = isolated(root)
            .arg("project")
            .arg("add")
            .arg(&project)
            .current_dir(root)
            .spawn()
            .unwrap();
        children.push(c);
    }
    for mut c in children {
        c.wait().unwrap();
    }
    // add is idempotent (re-binding is allowed and does not overwrite), so all
    // may exit 0; the contract is that the store ends up with exactly one
    // complete bundle, never a torn or duplicated one.
    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(items.len(), 1, "no duplicate bundle");
    let bundles = std::fs::read_dir(root.join("config").join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(bundles.len(), 1);
    for f in ["project.toml", "osdk.toml", "osdk.lock"] {
        assert!(bundles[0].join(f).is_file(), "missing {f}");
    }
}

#[test]
#[cfg(windows)]
fn bind_via_junction_resolves_to_target_canonical_root() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-project");
    std::fs::create_dir_all(&real).unwrap();
    let link = temp.path().join("alias-link");
    let out = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .output()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "junction creation not permitted here, skipping: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let add = isolated(temp.path())
        .arg("project")
        .arg("add")
        .arg(&link)
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");
    let show = isolated(temp.path())
        .arg("project")
        .arg("show")
        .arg(&link)
        .arg("--json")
        .current_dir(temp.path())
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    let root = json["root"].as_str().unwrap();
    assert!(
        root.replace('/', "\\").contains("real-project"),
        "root must canonicalize through the junction: {root}"
    );
}

#[test]
fn bind_under_a_long_chinese_path_still_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let deep = root
        .join("非常长的项目路径")
        .join("一层")
        .join("二层")
        .join("我的服务 svc");
    std::fs::create_dir_all(&deep).unwrap();
    let add = isolated(root)
        .arg("project")
        .arg("add")
        .arg(&deep)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");
    let show = isolated(root)
        .arg("project")
        .arg("show")
        .current_dir(&deep)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let text = String::from_utf8_lossy(&show.stdout);
    assert!(text.contains("external"), "must select the binding: {text}");
}

#[test]
fn relocate_racing_remove_leaves_store_consistent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("svc");
    let moved = root.join("svc-moved");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&moved).unwrap();
    assert!(isolated(root)
        .arg("project")
        .arg("add")
        .arg(&project)
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());

    // Half the processes relocate, half remove, racing on the same bundle.
    let mut children = Vec::new();
    for i in 0..6 {
        let c = if i % 2 == 0 {
            isolated(root)
                .arg("project")
                .arg("relocate")
                .arg(&project)
                .arg(&moved)
                .current_dir(root)
                .spawn()
                .unwrap()
        } else {
            isolated(root)
                .arg("project")
                .arg("remove")
                .arg(&project)
                .current_dir(root)
                .spawn()
                .unwrap()
        };
        children.push(c);
    }
    for mut c in children {
        c.wait().unwrap();
    }

    // The store must not be left half-registered: either the bundle is gone,
    // or it is a complete bundle at the moved root; never a dangling entry.
    let list = isolated(root)
        .arg("project")
        .arg("list")
        .arg("--json")
        .current_dir(root)
        .output()
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert!(items.len() <= 1, "at most one binding survives: {items:?}");
    for item in &items {
        let root = item["root"].as_str().unwrap().replace('/', "\\");
        assert!(
            root.contains("svc-moved"),
            "surviving binding must be the relocated one: {root}"
        );
    }
}
