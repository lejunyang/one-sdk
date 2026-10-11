//! Required HTTP options supplied as CLI flags must reach offline lock creation.
use std::process::Command;

#[test]
fn http_lock_binds_flags_and_preserves_checksum_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("home")).unwrap();
    std::fs::write(root.join("osdk.toml"), "[tools]\n").unwrap();
    let checksum = "a".repeat(64);
    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args([
            "--offline",
            "lock",
            "http:https://example.test/tool-{version}.zip@1.2.3",
            "-o",
            &format!("sha256={checksum}"),
            "-o",
            "bin=tool",
        ])
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
    let lock: toml::Value = std::fs::read_to_string(root.join("osdk.lock"))
        .unwrap()
        .parse()
        .unwrap();
    let platform = lock["platforms"]
        .as_table()
        .unwrap()
        .values()
        .next()
        .unwrap();
    let tool = &platform["tools"]["http:https://example.test/tool-{version}.zip"];
    assert_eq!(tool["version"].as_str(), Some("1.2.3"));
    assert_eq!(tool["options"]["sha256"].as_str(), Some(checksum.as_str()));
    assert!(!root.join("installs").join("http").exists());
}
