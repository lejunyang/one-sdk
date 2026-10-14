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
