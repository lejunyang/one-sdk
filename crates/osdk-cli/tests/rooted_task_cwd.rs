//! Rooted tasks must resolve their working directory inside their own project.
use std::process::Command;

#[test]
fn rooted_tasks_resolve_default_relative_and_absolute_working_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let child = root.join("child");
    let nested = child.join("nested");
    let absolute = root.join("absolute");
    for path in [&nested, &absolute, &root.join("home")] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(
        root.join("osdk.toml"),
        "[task]\nroots = [\"child\"]\n[tasks.root]\nlua = 'write(\"root-result\", \"root\"); return 0'\n",
    )
    .unwrap();
    let absolute_toml = toml::Value::String(absolute.to_string_lossy().into_owned());
    std::fs::write(
        child.join("osdk.toml"),
        format!(
            "[tasks.default]\nlua = 'write(\"default-result\", \"child\"); return 0'\n\
             [tasks.relative]\ndir = 'nested'\nlua = 'write(\"relative-result\", \"nested\"); return 0'\n\
             [tasks.absolute]\ndir = {absolute_toml}\nlua = 'write(\"absolute-result\", \"absolute\"); return 0'\n"
        ),
    )
    .unwrap();
    for task in [
        "root",
        "//child:default",
        "//child:relative",
        "//child:absolute",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
            .args(["run", task])
            .current_dir(root)
            .env_clear()
            .env("HOME", root.join("home"))
            .env("OSDK_CONFIG_DIR", root.join("config"))
            .env("OSDK_DATA_DIR", root.join("data"))
            .env("OSDK_CACHE_DIR", root.join("cache"))
            .env("OSDK_STORE_DIR", root.join("store"))
            .env("OSDK_INSTALL_DIR", root.join("installs"))
            .env("OSDK_TRUSTED_CONFIG_PATHS", root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{task}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for (path, expected) in [
        (root.join("root-result"), "root"),
        (child.join("default-result"), "child"),
        (nested.join("relative-result"), "nested"),
        (absolute.join("absolute-result"), "absolute"),
    ] {
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
    }
    assert!(!root.join("default-result").exists());
}
