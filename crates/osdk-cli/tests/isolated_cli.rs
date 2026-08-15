use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn osdk() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_osdk"))
}

fn platform_key() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-x64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-arm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "macos-x64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "macos-arm64"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows-x64"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "windows-arm64"
    }
}

fn run_isolated(root: &Path, args: &[&str]) -> Output {
    run_isolated_in(root, root, args)
}

fn run_isolated_in(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    let data = root.join("data");
    let cache = root.join("cache");
    let config = root.join("config");
    let store = root.join("store");
    let installs = root.join("installs");
    std::fs::create_dir_all(&home).unwrap();

    Command::new(osdk())
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "")
        .env("LANG", "C")
        .env("OSDK_DATA_DIR", &data)
        .env("OSDK_CACHE_DIR", &cache)
        .env("OSDK_CONFIG_DIR", &config)
        .env("OSDK_STORE_DIR", &store)
        .env("OSDK_INSTALL_DIR", &installs)
        .output()
        .unwrap()
}

#[test]
fn config_list_uses_only_isolated_directories() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["config", "list"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    for directory in ["data", "cache", "store", "installs"] {
        assert!(
            stdout.contains(&temp.path().join(directory).display().to_string()),
            "missing {directory} in output: {stdout}"
        );
    }
    assert!(!stdout.contains("/.local/share/osdk"));
    assert!(!stdout.contains("/.cache/osdk"));
}

#[test]
fn attestation_policy_cli_override_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(
        temp.path(),
        &["--attestations", "required", "config", "list"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("attestations = required"));
}

#[test]
fn doctor_creates_state_only_under_isolated_root() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["doctor"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(temp.path().join("data/shims").is_dir());
    assert!(temp.path().join("cache/downloads").is_dir());
    assert!(temp.path().join("config").is_dir());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("node, go, python, java, rust, pnpm, yarn, deno, bun"));
}

#[test]
fn lock_resolves_static_python_versions_offline() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\npython = \"3.14\"\n").unwrap();

    let output = run_isolated_in(temp.path(), &project, &["--offline", "lock"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lockfile = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lockfile.contains("request = \"3.14\""));
    assert!(lockfile.contains("version = \"3.14.7\""));
    assert!(lockfile.contains(&format!("[platforms.{}.tools.python]", platform_key())));
}

#[test]
fn outdated_reports_missing_static_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["--offline", "outdated", "python@3.14"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "python - -> 3.14.7"
    );
}

#[test]
fn completions_emit_target_shell_script() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["completions", "bash"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("_osdk"));
    assert!(stdout.contains("complete"));
}

#[test]
fn deactivate_emits_shell_restoration_code() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["deactivate", "bash"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("unset -f _osdk_hook"));
    assert!(stdout.contains("OSDK_ORIGINAL_PATH"));
}

#[test]
fn version_aliases_chain_canonicalize_and_unset() {
    let temp = tempfile::tempdir().unwrap();
    let install = temp.path().join("installs/node/20.0.0");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    for args in [
        ["alias", "set", "nodejs", "default", "20.0.0"],
        ["alias", "set", "node", "maintenance", "default"],
    ] {
        let output = run_isolated(temp.path(), &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let config = std::fs::read_to_string(temp.path().join("config/config.toml")).unwrap();
    assert!(config.contains("[aliases.node]"));
    assert!(config.contains("default = \"20.0.0\""));
    assert!(config.contains("maintenance = \"default\""));

    let output = run_isolated(temp.path(), &["--offline", "install", "node@maintenance"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let list = run_isolated(temp.path(), &["alias", "list", "node"]);
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("node default = 20.0.0"));
    assert!(stdout.contains("node maintenance = default"));

    let unset = run_isolated(temp.path(), &["alias", "unset", "node", "maintenance"]);
    assert!(unset.status.success());
    let config = std::fs::read_to_string(temp.path().join("config/config.toml")).unwrap();
    assert!(!config.contains("maintenance"));
}

#[test]
fn version_alias_cycles_and_reserved_names_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let reserved = run_isolated(temp.path(), &["alias", "set", "node", "latest", "20"]);
    assert!(!reserved.status.success());
    assert!(String::from_utf8_lossy(&reserved.stderr).contains("reserved"));

    let first = run_isolated(temp.path(), &["alias", "set", "node", "a", "b"]);
    assert!(first.status.success());
    let cycle = run_isolated(temp.path(), &["alias", "set", "node", "b", "a"]);
    assert!(!cycle.status.success());
    assert!(String::from_utf8_lossy(&cycle.stderr).contains("cycle"));
}

#[test]
fn upgrade_updates_lock_for_an_already_installed_exact_version() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\nnode = \"1.0.0\"\n").unwrap();
    let install = temp.path().join("installs/node/1.0.0");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    let output = run_isolated_in(temp.path(), &project, &["--offline", "upgrade"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lockfile = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lockfile.contains("version = \"1.0.0\""));
}

#[test]
fn install_without_arguments_consumes_matching_platform_lock() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\nnode = \"2.0.0\"\n").unwrap();
    std::fs::write(
        project.join("osdk.lock"),
        format!(
            "schema = 1\n\n[platforms.{}.tools.node]\nrequest = \"1.0.0\"\nversion = \"1.0.0\"\n",
            platform_key()
        ),
    )
    .unwrap();
    let install = temp.path().join("installs/node/1.0.0");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    let output = run_isolated_in(temp.path(), &project, &["--offline", "install"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("installs/node/2.0.0").exists());
}

#[cfg(unix)]
#[test]
fn artifact_lock_reinstalls_offline_and_rejects_tampering() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\nnode = \"1.0.0\"\n").unwrap();

    let archive = temp
        .path()
        .join("cache/downloads/node/1.0.0/node-fixture.tar.gz");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    {
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let contents = b"#!/bin/sh\nprintf 'locked-node\\n'\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "node-fixture/bin/node", &contents[..])
            .unwrap();
        tar.finish().unwrap();
    }
    let checksum =
        osdk_core::pipeline::verify::hash_file(&archive, osdk_core::pipeline::HashAlgo::Sha256)
            .unwrap();
    let install = temp.path().join("installs/node/1.0.0");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();
    std::fs::write(
        install.join(".osdk-artifact.json"),
        format!(
            "{{\"url\":\"https://invalid.example/node-fixture.tar.gz\",\"file_name\":\"node-fixture.tar.gz\",\"checksum\":\"sha256:{checksum}\"}}"
        ),
    )
    .unwrap();

    let lock = run_isolated_in(temp.path(), &project, &["--offline", "lock"]);
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let lock_path = project.join("osdk.lock");
    let lockfile = std::fs::read_to_string(&lock_path).unwrap();
    assert!(lockfile.contains("file_name = \"node-fixture.tar.gz\""));
    assert!(lockfile.contains(&format!("checksum = \"sha256:{checksum}\"")));

    std::fs::remove_dir_all(&install).unwrap();
    let reinstall = run_isolated_in(temp.path(), &project, &["--offline", "install"]);
    assert!(
        reinstall.status.success(),
        "{}",
        String::from_utf8_lossy(&reinstall.stderr)
    );
    let node = install.join("bin/node");
    assert!(node.is_file());
    assert_ne!(
        std::fs::metadata(&node).unwrap().permissions().mode() & 0o111,
        0
    );

    std::fs::remove_dir_all(&install).unwrap();
    let tampered = lockfile.replace(
        &format!("sha256:{checksum}"),
        &format!("sha256:{}", "0".repeat(64)),
    );
    std::fs::write(&lock_path, tampered).unwrap();
    let rejected = run_isolated_in(temp.path(), &project, &["--offline", "install"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("checksum mismatch"));
}

#[test]
fn locked_evidence_is_not_trusted_without_cached_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let artifact = temp
        .path()
        .join("cache/downloads/github/example/tool/1.0.0/tool");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"locked tool fixture").unwrap();
    let checksum =
        osdk_core::pipeline::verify::hash_file(&artifact, osdk_core::pipeline::HashAlgo::Sha256)
            .unwrap();
    std::fs::write(
        project.join("osdk.lock"),
        format!(
            r#"schema = 1

[platforms.{platform}.tools."github:example/tool"]
request = "1.0.0"
version = "1.0.0"

[platforms.{platform}.tools."github:example/tool".artifact]
url = "https://invalid.example/tool"
file_name = "tool"
checksum = "sha256:{checksum}"

[[platforms.{platform}.tools."github:example/tool".artifact.evidence]]
kind = "sigstore-bundle"
repository = "example/tool"
issuer = "https://token.actions.githubusercontent.com"
digest = "sha256:{checksum}"
"#,
            platform = platform_key(),
        ),
    )
    .unwrap();

    let output = run_isolated_in(
        temp.path(),
        &project,
        &["--offline", "--attestations", "required", "install"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no cached bundle"));
    assert!(!temp
        .path()
        .join("installs/github/example/tool/1.0.0/.osdk-complete")
        .exists());
}

#[cfg(unix)]
#[test]
fn exec_runs_with_exact_managed_tool_environment() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let install = temp.path().join("installs/fake/1.0.0/bin");
    std::fs::create_dir_all(&install).unwrap();
    let executable = install.join("fake");
    std::fs::write(&executable, "#!/bin/sh\nprintf 'fake:%s\\n' \"$*\"\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(temp.path().join("installs/fake/1.0.0/.osdk-complete"), b"").unwrap();

    // The generic GitHub backend can represent arbitrary binaries but would
    // require metadata. Use node's fixed install layout for an exact, already
    // installed version and expose a temporary `node` executable instead.
    let node_bin = temp.path().join("installs/node/1.0.0/bin");
    std::fs::create_dir_all(&node_bin).unwrap();
    let node = node_bin.join("node");
    std::fs::write(&node, "#!/bin/sh\nprintf 'node:%s\\n' \"$*\"\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(temp.path().join("installs/node/1.0.0/.osdk-complete"), b"").unwrap();

    let output = run_isolated(
        temp.path(),
        &[
            "--offline",
            "exec",
            "--tool",
            "node@1.0.0",
            "--",
            "node",
            "hello",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("node:hello"));
}
