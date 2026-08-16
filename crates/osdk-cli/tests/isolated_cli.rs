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
    run_isolated_in_with_env(root, cwd, args, &[])
}

fn run_isolated_in_with_env(
    root: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let home = root.join("home");
    let data = root.join("data");
    let cache = root.join("cache");
    let config = root.join("config");
    let store = root.join("store");
    let installs = root.join("installs");
    std::fs::create_dir_all(&home).unwrap();

    let mut command = Command::new(osdk());
    command
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
        .env("OSDK_INSTALL_DIR", &installs);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
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
fn destructive_commands_require_explicit_non_interactive_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("cache/downloads/archive.tar.gz");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(&archive, b"fixture").unwrap();

    let output = run_isolated(temp.path(), &["--quiet", "cache", "clean"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("rerun with --yes"));
    assert!(archive.is_file());
}

#[test]
fn yes_flag_confirms_cache_clean() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("cache/downloads/archive.tar.gz");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(&archive, b"fixture").unwrap();

    let output = run_isolated(temp.path(), &["--yes", "cache", "clean"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!archive.exists());
    assert!(temp.path().join("cache/downloads").is_dir());
}

#[test]
fn yes_environment_confirms_prune() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated_in_with_env(
        temp.path(),
        temp.path(),
        &["prune"],
        &[("OSDK_YES", "true")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("pruned 0 object"));
}

#[test]
fn non_interactive_confirmation_error_is_localized() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated_in_with_env(
        temp.path(),
        temp.path(),
        &["cache", "clean"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("非交互模式需要确认"));
}

#[test]
fn safe_project_pins_do_not_require_trust() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\nnode = \"20\"\n[aliases.node]\ndefault = \"20\"\n",
    )
    .unwrap();

    let output = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("node = 20"));
}

#[test]
fn trust_is_content_bound_and_untrust_blocks_dangerous_project_config() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let config = project.join("osdk.toml");
    std::fs::write(
        &config,
        "[tools]\nnode = \"20\"\n[sources]\nselection = \"ordered\"\n",
    )
    .unwrap();

    let rejected = run_isolated_in(temp.path(), &project, &["--yes", "config", "list"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("is not trusted"));

    let trusted = run_isolated_in(temp.path(), &project, &["--yes", "trust"]);
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    let accepted = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(accepted.status.success());

    std::fs::write(
        &config,
        "[tools]\nnode = \"22\"\n[sources]\nselection = \"ordered\"\n",
    )
    .unwrap();
    let changed = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("is not trusted"));

    let config_value = config.to_string_lossy().into_owned();
    let retrusted = run_isolated_in(temp.path(), &project, &["--yes", "trust", &config_value]);
    assert!(retrusted.status.success());
    let listed = run_isolated_in(temp.path(), &project, &["trust", "list"]);
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("trusted"));

    let removed = run_isolated_in(temp.path(), &project, &["untrust"]);
    assert!(removed.status.success());
    let rejected_again = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(!rejected_again.status.success());
}

#[test]
fn trusted_config_path_whitelist_allows_ci_without_persisted_trust() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[settings]\nyes = true\n[tools]\nnode = \"20\"\n",
    )
    .unwrap();

    let project_value = project.to_string_lossy().into_owned();
    let output = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["config", "list"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &project_value)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn terminal_prompt_accepts_interactive_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("cache/downloads/archive.tar.gz");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(&archive, b"fixture").unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let shell_command = format!(
        "env -i HOME={} PATH=/usr/bin:/bin LANG=C OSDK_DATA_DIR={} OSDK_CACHE_DIR={} OSDK_CONFIG_DIR={} OSDK_STORE_DIR={} OSDK_INSTALL_DIR={} {} cache clean",
        home.display(),
        temp.path().join("data").display(),
        temp.path().join("cache").display(),
        temp.path().join("config").display(),
        temp.path().join("store").display(),
        temp.path().join("installs").display(),
        osdk().display()
    );
    let mut child = Command::new("script")
        .args(["-qec", &shell_command, "/dev/null"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!archive.exists());
    assert!(String::from_utf8_lossy(&output.stdout).contains("[y/N]:"));
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
    assert!(stdout
        .contains("node, go, python, java, maven, gradle, kotlin, rust, pnpm, yarn, deno, bun"));
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
fn package_json_node_range_is_discovered_with_documented_priority() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"engines":{"node":">=20 <23"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.toml"),
        "[tools]\nnode = \"18\"\n",
    )
    .unwrap();
    std::fs::write(project.join(".node-version"), "21.7.3\n").unwrap();
    let install = temp.path().join("installs/node/21.7.3");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    let current = run_isolated_in(temp.path(), &project, &["current", "node"]);
    assert!(current.status.success());
    assert!(String::from_utf8_lossy(&current.stdout).contains("21.7.3"));

    std::fs::remove_file(project.join(".node-version")).unwrap();
    let current = run_isolated_in(temp.path(), &project, &["current", "node"]);
    assert!(current.status.success());
    assert!(String::from_utf8_lossy(&current.stdout).contains(">=20 <23"));

    std::fs::write(
        project.join("package.json"),
        r#"{"engines":{"node":"not-a-range"}}"#,
    )
    .unwrap();
    let invalid = run_isolated_in(temp.path(), &project, &["lock"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid semver range"));
}

#[cfg(unix)]
#[test]
fn python_find_reports_managed_path_and_system_layers() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let managed = temp.path().join("installs/python/pypy-3.11.15/bin/pypy3");
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        temp.path()
            .join("installs/python/pypy-3.11.15/.osdk-complete"),
        b"",
    )
    .unwrap();

    let path_bin = temp.path().join("path-bin");
    std::fs::create_dir_all(&path_bin).unwrap();
    let path_python = path_bin.join("python3");
    std::fs::write(&path_python, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path_python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path_value = path_bin.to_string_lossy().into_owned();
    let output = run_isolated_in_with_env(
        temp.path(),
        temp.path(),
        &["python", "find", "pypy-3.11"],
        &[("PATH", &path_value)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("managed\tpypy-3.11.15"));
    assert!(stdout.contains(&managed.display().to_string()));
    assert!(stdout.contains("path\t-\t"));
    assert!(stdout.contains(&path_python.display().to_string()));
}

#[test]
fn node_cross_arch_lock_uses_target_platform_and_install_rejects_execution() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let target_arch = if cfg!(target_arch = "aarch64") {
        "x64"
    } else {
        "arm64"
    };
    let target_key = platform_key().replacen(
        if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x64"
        } else if cfg!(target_arch = "x86") {
            "x86"
        } else {
            "arm"
        },
        target_arch,
        1,
    );
    let lock = run_isolated_in(
        temp.path(),
        &project,
        &[
            "--offline",
            "lock",
            "node@20.11.1",
            "-o",
            &format!("arch={target_arch}"),
        ],
    );
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let lockfile = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lockfile.contains(&format!("[platforms.{target_key}.tools.node]")));
    assert!(lockfile.contains(&format!("arch = \"{target_arch}\"")));

    let install = run_isolated(
        temp.path(),
        &[
            "--offline",
            "install",
            "node@20.11.1",
            "-o",
            &format!("arch={target_arch}"),
        ],
    );
    assert!(!install.status.success());
    assert!(String::from_utf8_lossy(&install.stderr).contains("cross-architecture"));
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
fn write_fake_managed_npm(root: &Path, version: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let bin = root.join(format!("installs/node/{version}/bin"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.parent().unwrap().join(".osdk-complete"), b"").unwrap();
    let node = bin.join("node");
    std::fs::write(&node, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    let npm = bin.join("npm");
    std::fs::write(&npm, script).unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn node_package_migration_dry_run_uses_managed_npm_and_filters_packages() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("npm.log");
    let source_script = format!(
        r#"#!/bin/sh
printf '%s|%s\n' "$PATH" "$*" >> '{}'
printf '%s\n' '{{"dependencies":{{"npm":{{"version":"10.0.0"}},"eslint":{{"version":"9.1.0"}},"native-addon":{{"version":"1.0.0","gypfile":true}}}}}}'
"#,
        log.display()
    );
    let target_script = format!(
        r#"#!/bin/sh
printf '%s|%s\n' "$PATH" "$*" >> '{}'
printf '%s\n' '{{"dependencies":{{}}}}'
"#,
        log.display()
    );
    write_fake_managed_npm(temp.path(), "20.0.0", &source_script);
    write_fake_managed_npm(temp.path(), "22.0.0", &target_script);

    let output = run_isolated(
        temp.path(),
        &[
            "node",
            "migrate-packages",
            "--from",
            "20.0.0",
            "--to",
            "22.0.0",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("would install eslint@9.1.0"));
    assert!(stdout.contains("skip npm itself"));
    assert!(stdout.contains("skip native"));
    assert!(stdout.contains("dry-run only"));
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(calls.contains("installs/node/20.0.0/bin"));
    assert!(calls.contains("installs/node/22.0.0/bin"));
    assert!(!calls.contains("install -g"));
}

#[cfg(unix)]
#[test]
fn node_package_migration_apply_installs_the_plan() {
    let temp = tempfile::tempdir().unwrap();
    let installed = temp.path().join("installed-specs");
    let source_script = r#"#!/bin/sh
printf '{"dependencies":{"eslint":{"version":"9.1.0"}}}\n'
"#;
    let target_script = format!(
        r#"#!/bin/sh
if [ "$1" = "ls" ]; then
  printf '{{"dependencies":{{}}}}\n'
elif [ "$1" = "install" ]; then
  printf '%s\n' "$3" > '{}'
fi
"#,
        installed.display()
    );
    write_fake_managed_npm(temp.path(), "20.0.0", source_script);
    write_fake_managed_npm(temp.path(), "22.0.0", &target_script);

    let output = run_isolated(
        temp.path(),
        &[
            "node",
            "migrate-packages",
            "--from",
            "20.0.0",
            "--to",
            "22.0.0",
            "--apply",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(installed).unwrap().trim(),
        "eslint@9.1.0"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("migrated 1 global package"));
}

#[cfg(unix)]
#[test]
fn failed_node_package_migration_restores_target_packages() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("target-state");
    let calls = temp.path().join("target-calls");
    std::fs::write(&state, "before").unwrap();
    let source_script = r#"#!/bin/sh
printf '{"dependencies":{"eslint":{"version":"9.1.0"}}}\n'
"#;
    let target_script = format!(
        r#"#!/bin/sh
state='{}'
calls='{}'
printf '%s\n' "$*" >> "$calls"
if [ "$1" = "ls" ]; then
  IFS= read -r current < "$state"
  if [ "$current" = "before" ]; then
    printf '{{"dependencies":{{"typescript":{{"version":"5.5.0"}}}}}}\n'
  else
    printf '{{"dependencies":{{"broken":{{"version":"1.0.0"}}}}}}\n'
  fi
elif [ "$1" = "install" ] && [ "$3" = "eslint@9.1.0" ]; then
  printf changed > "$state"
  exit 9
elif [ "$1" = "uninstall" ]; then
  printf empty > "$state"
elif [ "$1" = "install" ] && [ "$3" = "typescript@5.5.0" ]; then
  printf before > "$state"
fi
"#,
        state.display(),
        calls.display()
    );
    write_fake_managed_npm(temp.path(), "20.0.0", source_script);
    write_fake_managed_npm(temp.path(), "22.0.0", &target_script);

    let output = run_isolated(
        temp.path(),
        &[
            "node",
            "migrate-packages",
            "--from",
            "20.0.0",
            "--to",
            "22.0.0",
            "--apply",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("restored"));
    assert_eq!(std::fs::read_to_string(state).unwrap(), "before");
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(calls.contains("uninstall -g broken"));
    assert!(calls.contains("install -g typescript@5.5.0"));
}

#[cfg(unix)]
fn write_fake_rustup(root: &Path, project: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let rustup = root.join("data/cargo/bin/rustup");
    std::fs::create_dir_all(rustup.parent().unwrap()).unwrap();
    let log = root.join("rustup-calls.log");
    std::fs::write(
        &rustup,
        format!(
            r#"#!/bin/sh
printf '%s|%s|%s\n' "$RUSTUP_HOME" "$CARGO_HOME" "$*" >> '{}'
case "$1 $2" in
  "component list") printf 'rustfmt-x86_64-unknown-linux-gnu (installed)\n' ;;
  "target list") printf 'x86_64-unknown-linux-gnu (installed)\n' ;;
  "check ") printf 'stable - Up to date\n' ;;
  "override list") printf '{} stable-x86_64-unknown-linux-gnu\n' ;;
esac
"#,
            log.display(),
            project.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&rustup, std::fs::Permissions::from_mode(0o755)).unwrap();
    rustup
}

#[cfg(unix)]
#[test]
fn rust_lifecycle_commands_use_isolated_rustup_and_repair_markers() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_fake_rustup(temp.path(), &project);
    std::fs::create_dir_all(temp.path().join("data/rustup/toolchains/stable/bin")).unwrap();
    let stale = temp.path().join("installs/rust/stale");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join(".osdk-complete"), b"").unwrap();

    for args in [
        vec![
            "rust",
            "component",
            "add",
            "rustfmt",
            "--toolchain",
            "stable",
        ],
        vec![
            "rust",
            "component",
            "remove",
            "rustfmt",
            "--toolchain",
            "stable",
        ],
        vec!["rust", "component", "list", "--toolchain", "stable"],
        vec![
            "rust",
            "target",
            "add",
            "x86_64-pc-windows-gnu",
            "--toolchain",
            "stable",
        ],
        vec![
            "rust",
            "target",
            "remove",
            "x86_64-pc-windows-gnu",
            "--toolchain",
            "stable",
        ],
        vec!["rust", "target", "list", "--toolchain", "stable"],
        vec!["rust", "check", "--repair"],
    ] {
        let output = run_isolated_in(temp.path(), &project, &args);
        assert!(
            output.status.success(),
            "args={args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let calls = std::fs::read_to_string(temp.path().join("rustup-calls.log")).unwrap();
    let rustup_home = temp.path().join("data/rustup").display().to_string();
    let cargo_home = temp.path().join("data/cargo").display().to_string();
    for line in calls.lines() {
        assert!(line.starts_with(&format!("{rustup_home}|{cargo_home}|")));
    }
    assert!(calls.contains("component add rustfmt --toolchain stable"));
    assert!(calls.contains("target add x86_64-pc-windows-gnu --toolchain stable"));
    assert!(calls.contains("|check"));
    assert!(temp
        .path()
        .join("installs/rust/stable/.osdk-complete")
        .is_file());
    assert!(!stale.exists());
}

#[cfg(unix)]
#[test]
fn rust_override_import_export_and_toolchain_link_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_fake_rustup(temp.path(), &project);

    let import = run_isolated_in(temp.path(), &project, &["rust", "override", "import"]);
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );
    let config = std::fs::read_to_string(project.join("osdk.toml")).unwrap();
    assert!(config.contains("rust = \"stable-x86_64-unknown-linux-gnu\""));

    let export = run_isolated_in(temp.path(), &project, &["rust", "override", "export"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );

    let linked = temp.path().join("custom-rust");
    std::fs::create_dir_all(linked.join("bin")).unwrap();
    let link = run_isolated_in(
        temp.path(),
        &project,
        &[
            "rust",
            "toolchain",
            "link",
            "local-dev",
            &linked.to_string_lossy(),
        ],
    );
    assert!(
        link.status.success(),
        "{}",
        String::from_utf8_lossy(&link.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("installs/rust/local-dev/.osdk-linked")).unwrap(),
        std::fs::canonicalize(&linked)
            .unwrap()
            .display()
            .to_string()
    );
    let calls = std::fs::read_to_string(temp.path().join("rustup-calls.log")).unwrap();
    assert!(calls.contains("override list"));
    assert!(calls.contains("override set stable-x86_64-unknown-linux-gnu --path"));
    assert!(calls.contains("toolchain link local-dev"));
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
