//! Real task groups must make progress together and preserve child lifecycles.
use std::process::Command;

#[test]
fn grouped_tasks_share_preparation_and_execute_concurrently() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("home")).unwrap();
    std::fs::write(
        root.join("osdk.toml"),
        r#"
[tasks.prepare]
lua = 'assert(not exists("prepared")); write("prepared", "once")'
[tasks.left]
depends = ["prepare"]
timeout = "5s"
lua = 'assert(exists("prepared")); write("left", "ready"); while not exists("right") do end'
run_post = [{ tasks = ["cleanup"] }]
[tasks.right]
depends = ["prepare"]
timeout = "5s"
lua = 'assert(exists("prepared")); write("right", "ready"); while not exists("left") do end'
[tasks.cleanup]
lua = 'write("cleaned", "done")'
[tasks.all]
run = [{ tasks = ["left", "right"] }]
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(["run", "all"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("OSDK_CONFIG_DIR", root.join("config"))
        .env("OSDK_DATA_DIR", root.join("data"))
        .env("OSDK_CACHE_DIR", root.join("cache"))
        .env("OSDK_STORE_DIR", root.join("store"))
        .env("OSDK_INSTALL_DIR", root.join("installs"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for file in ["prepared", "left", "right", "cleaned"] {
        assert!(root.join(file).is_file(), "{file}");
    }
}
