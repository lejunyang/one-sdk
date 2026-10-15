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
fn removed_installer_is_rejected_in_root_project_and_inherited_configuration() {
    for config in [
        "[deps.npm]\ninstaller='npm'\n",
        "[deps.projects.'apps/web'.npm]\ninstaller='npm'\n",
        "[deps]\nroots=['apps/*']\n[deps.npm]\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "package.json", "{}");
        write(root, "apps/web/package.json", "{}");
        write(root, "osdk.toml", config);
        let inherited = !config.contains("installer");
        if inherited {
            write(
                root,
                "apps/web/osdk.toml",
                "extends='../../osdk.toml'\n[deps.npm]\ninstaller='npm'\n",
            );
        }
        for cwd in [root.to_path_buf(), root.join("apps/web")] {
            let output = run(root, &cwd, &["deps", "--list", "--all"]);
            assert!(!output.status.success(), "{output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("unknown field `installer`"), "{error}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains("would run in"));
        }
    }
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

#[test]
fn explicit_inheritance_has_same_deps_options_and_commands_from_root_or_child() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "apps/api/package.json",
        r#"{"packageManager":"npm@11"}"#,
    );
    write(root,"osdk.toml","[tools]\nnode='24.19.0'\n[deps]\nroots=['apps/*']\n[deps.npm]\noutputs=['node_modules']\nenv={A='1',B='1'}\n");
    write(
        root,
        "apps/api/osdk.toml",
        "extends='../../osdk.toml'\n[deps.npm]\nauto=false\ndir='.'\noutputs=[]\nenv={B='2'}\n",
    );
    let mut results = Vec::new();
    for cwd in [root.to_path_buf(), root.join("apps/api")] {
        let output = run(root, &cwd, &["deps", "--list", "--all", "--explain"]);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("auto=false"), "{text}");
        assert!(text.contains("outputs=[]"), "{text}");
        assert!(text.contains("A=1 B=2"), "{text}");
        results.push(text);
    }
    assert_eq!(results[0], results[1]);
}
#[test]
fn trusting_leaf_does_not_trust_inherited_executable_config() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "osdk.toml",
        "[deps.codegen]\nrun=['a-command-that-must-not-run']",
    );
    write(root, "child/osdk.toml", "extends='../osdk.toml'");
    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(["deps", "--dry-run"])
        .current_dir(root.join("child"))
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("child/osdk.toml"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("deps.codegen.run"), "{error}");
    assert!(
        error.contains(
            root.join("osdk.toml")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        ),
        "{error}"
    );
}

#[test]
fn inherited_tasks_keep_declaring_file_paths_in_root_and_child_calls() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root,"osdk.toml","[task]\nroots=['child']\n[tasks.shared]\nlua='write(\"marker\", \"done\"); return 0'\ndir='scripts'\n");
    write(
        root,
        "child/osdk.toml",
        "extends='../osdk.toml'\n[tasks.local]\nlua='write(\"marker\", \"done\"); return 0'\ndir='scripts'\n",
    );
    for (cwd, name, expected) in [
        (root.to_path_buf(), "//child:shared", root.join("scripts")),
        (root.join("child"), "shared", root.join("scripts")),
        (
            root.to_path_buf(),
            "//child:local",
            root.join("child/scripts"),
        ),
        (root.join("child"), "local", root.join("child/scripts")),
    ] {
        std::fs::create_dir_all(&expected).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
            .args(["run", "--no-deps", name])
            .current_dir(&cwd)
            .env("OSDK_CONFIG_DIR", root.join("state/config"))
            .env("OSDK_DATA_DIR", root.join("state/data"))
            .env("OSDK_CACHE_DIR", root.join("state/cache"))
            .env("OSDK_STORE_DIR", root.join("state/store"))
            .env("OSDK_INSTALL_DIR", root.join("state/installs"))
            .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("osdk.toml"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(expected.join("marker")).unwrap(),
            "done"
        );
    }
}

#[test]
fn inherited_offline_policy_and_workspace_tool_context_are_consistent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "osdk.toml", "[deps.projects.'api'.npm]\n");
    write(root, "api/package.json", r#"{"packageManager":"npm@11"}"#);
    write(
        root,
        "api/osdk.toml",
        "extends='../osdk.toml'\n[settings]\noffline=true\n",
    );
    for cwd in [root.to_path_buf(), root.join("api")] {
        let output = run(root, &cwd, &["deps", "--list", "--all", "--explain"]);
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("--offline"),
            "{output:?}"
        );
    }
    write(
        root,
        "osdk.toml",
        "[settings]\noffline=true\n[deps.projects.'api'.npm]\n",
    );
    write(
        root,
        "api/osdk.toml",
        "extends='../osdk.toml'\n[settings]\noffline=false\n",
    );
    let output = run(
        root,
        root,
        &["--offline", "deps", "--list", "--all", "--explain"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--offline"),
        "CLI offline must override even a matching parent flag: {output:?}"
    );
    write(root, "js/package.json", r#"{"packageManager":"pnpm@11"}"#);
    write(root, "js/pnpm-workspace.yaml", "packages: ['packages/*']");
    write(root, "js/packages/a/package.json", "{}");
    write(
        root,
        "js/packages/a/osdk.toml",
        "extends='../../../osdk.toml'\n[settings]\noffline=true\n",
    );
    write(root, "osdk.toml", "[deps.projects.'js'.pnpm]\n");
    let output = run(root, root, &["deps", "--list", "--all"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("conflicting effective tool settings or sources"),
        "{output:?}"
    );
}
