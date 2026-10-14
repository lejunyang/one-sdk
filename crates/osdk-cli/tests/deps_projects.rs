use std::path::Path;
use std::process::{Command, Output};
fn run(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(args)
        .current_dir(cwd)
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_INSTALL_DIR", root.join("state/installs"))
        .env("OSDK_STORE_DIR", root.join("state/store"))
        .output()
        .unwrap()
}
fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
#[test]
fn filters_route_and_deduplicate_root_and_children_from_either_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "package.json", r#"{"packageManager":"npm@11"}"#);
    write(
        root,
        "apps/api/package.json",
        r#"{"packageManager":"pnpm@10"}"#,
    );
    write(
        root,
        "apps/web/package.json",
        r#"{"packageManager":"npm@11"}"#,
    );
    write(root,"osdk.toml","[deps]\nroots=['apps/*','apps/api']\n[deps.npm]\n[deps.pnpm]\n[deps.projects.'apps/web'.npm]\nauto=false\n");
    for cwd in [root.to_path_buf(), root.join("apps/api")] {
        let output = run(
            root,
            &cwd,
            &["deps", "--list", "--all", "--filter", "apps/*", "--explain"],
        );
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.matches("//apps/api:pnpm").count(), 1, "{text}");
        assert_eq!(text.matches("//apps/web:npm").count(), 1, "{text}");
        assert!(!text.contains("//:npm"), "root must be filtered: {text}");
    }
    let output = run(root, root, &["deps", "--list", "--all", "--filter", "."]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("//:npm"));
    write(root, "apps/api/package-lock.json", "{}");
    assert!(
        !run(root, root, &["deps", "--list", "--filter", "apps/web"])
            .status
            .success()
    );
}
#[test]
fn nested_project_commands_and_indexes_require_trust() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "apps/api/requirements.txt", "");
    write(
        root,
        "osdk.toml",
        "[deps.projects.'apps/api'.pip-requirements]\nrun=['custom','one argument']\n",
    );
    let output = run(root, root, &["deps", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("deps.projects.apps/api.pip-requirements.run"));
}

#[test]
fn workspace_member_selection_expands_to_one_shared_installation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "js/package.json",
        r#"{"name":"root","private":true,"packageManager":"pnpm@10"}"#,
    );
    write(
        root,
        "js/pnpm-workspace.yaml",
        "packages: ['packages/*', '!packages/excluded']",
    );
    write(root, "js/packages/a/package.json", r#"{"name":"a"}"#);
    write(root, "js/packages/b/package.json", r#"{"name":"b"}"#);
    write(root, "js/packages/excluded/package.json", "broken");
    write(
        root,
        "osdk.toml",
        "[deps.projects.'js'.pnpm]
",
    );
    let output = run(
        root,
        root,
        &["deps", "--dry-run", "--filter", "js/packages/a"],
    );
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("coverage expands"), "{text}");
    assert_eq!(text.matches("would run in").count(), 1, "{text}");
    assert!(text.contains("pnpm install"), "{text}");
    let output = run(root, &root.join("js/packages/a"), &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("would run in")
            .count(),
        1
    );
    write(
        root,
        "osdk.toml",
        "[deps.projects.'js'.pnpm]
[deps.projects.'js/packages/a'.pnpm]
auto=false",
    );
    let output = run(root, root, &["deps", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("identical provider options"));
}
#[test]
fn uv_workspace_plan_uses_all_packages_and_records_member_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "py/pyproject.toml",
        "[tool.uv.workspace]
members=['packages/*']",
    );
    write(
        root,
        "py/packages/a/pyproject.toml",
        "[project]
name='a'
version='0.1.0'",
    );
    write(
        root,
        "osdk.toml",
        "[deps.projects.'py'.uv]
",
    );
    let output = run(root, root, &["deps", "--list", "--all", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.matches("command:").count(), 1, "{text}");
    assert!(text.contains("--all-packages"), "{text}");
    assert!(text.contains("py/packages/a"), "{text}");
}
