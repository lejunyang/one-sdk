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
