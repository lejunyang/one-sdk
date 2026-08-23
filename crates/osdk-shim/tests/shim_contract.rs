use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::Stdio;

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_osdk-shim"))
}

fn isolated_command(root: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(shim());
    command
        .current_dir(cwd)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("PATH", "")
        .env("OSDK_DATA_DIR", root.join("data"))
        .env("OSDK_CACHE_DIR", root.join("cache"))
        .env("OSDK_CONFIG_DIR", root.join("config"))
        .env("OSDK_STORE_DIR", root.join("store"))
        .env("OSDK_INSTALL_DIR", root.join("installs"));
    command
}

#[cfg(unix)]
#[test]
fn forwards_stdio_arguments_environment_and_exit_code() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\nnode = \"1.0.0\"\n").unwrap();
    let bin = temporary.path().join("installs/node/1.0.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let node = bin.join("node");
    std::fs::write(
        &node,
        "#!/bin/sh\nread line\nprintf 'out:%s:%s\\n' \"$1\" \"$line\"\nprintf 'err:%s\\n' \"$2\" >&2\nexit 23\n",
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();

    let mut child = isolated_command(temporary.path(), &project)
        .args(["node", "first", "second"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"input\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "out:first:input\n"
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "err:second\n");
}

#[test]
fn recursive_invocation_fails_before_resolution() {
    let temporary = tempfile::tempdir().unwrap();
    let output = isolated_command(temporary.path(), temporary.path())
        .args(["node"])
        .env("OSDK_SHIM_ACTIVE", "node")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(String::from_utf8_lossy(&output.stderr).contains("recursive shim invocation"));
}

#[cfg(unix)]
#[test]
fn independent_npm_backend_wins_over_bundled_node_npm() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"engines":{"node":"1.0.0"},"packageManager":"npm@2.0.0"}"#,
    )
    .unwrap();
    for (path, output) in [
        ("installs/node/1.0.0/bin/node", "node"),
        ("installs/node/1.0.0/bin/npm", "bundled"),
        ("installs/npm/2.0.0/bin/npm", "independent"),
    ] {
        let executable = temporary.path().join(path);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '{output}\\n'\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for marker in [
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        temporary.path().join("installs/npm/2.0.0/.osdk-complete"),
    ] {
        std::fs::write(marker, b"").unwrap();
    }

    let output = isolated_command(temporary.path(), &project)
        .args(["npm"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "independent\n");
}

#[cfg(unix)]
#[test]
fn direct_shims_use_versioned_manager_native_caches_and_preserve_overrides() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let node = temporary.path().join("installs/node/1.0.0/bin/node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::write(&node, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();

    let cases = [
        (
            "npm",
            "11.5.2",
            "bin/npm",
            "${npm_config_cache-unset}",
            "npm",
            "npm_config_cache",
        ),
        (
            "pnpm",
            "10.15.0",
            "pnpm",
            "${PNPM_HOME-unset}|${npm_config_store_dir-unset}|${pnpm_config_store_dir-unset}",
            "pnpm|pnpm-store|unset",
            "npm_config_store_dir",
        ),
        (
            "pnpm",
            "11.0.0",
            "pnpm",
            "${PNPM_HOME-unset}|${npm_config_store_dir-unset}|${pnpm_config_store_dir-unset}",
            "pnpm|unset|pnpm-store",
            "pnpm_config_store_dir",
        ),
        (
            "yarn",
            "1.22.22",
            "bin/yarn",
            "${YARN_CACHE_FOLDER-unset}|${YARN_GLOBAL_FOLDER-unset}|${YARN_ENABLE_GLOBAL_CACHE-unset}",
            "yarn-classic|unset|unset",
            "YARN_CACHE_FOLDER",
        ),
        (
            "yarn",
            "4.10.3",
            "bin/yarn",
            "${YARN_CACHE_FOLDER-unset}|${YARN_GLOBAL_FOLDER-unset}|${YARN_ENABLE_GLOBAL_CACHE-unset}",
            "unset|yarn|unset",
            "YARN_GLOBAL_FOLDER",
        ),
    ];

    for (manager, version, relative_executable, shell_value, expected_suffix, override_key) in cases
    {
        std::fs::write(
            project.join("package.json"),
            format!(r#"{{"engines":{{"node":"1.0.0"}},"packageManager":"{manager}@{version}"}}"#),
        )
        .unwrap();
        let install = temporary
            .path()
            .join(format!("installs/{manager}/{version}"));
        let executable = install.join(relative_executable);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\n' \"{shell_value}\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();

        let output = isolated_command(temporary.path(), &project)
            .arg(manager)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{manager}@{version}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = expected_suffix
            .split('|')
            .map(|part| {
                if part == "unset" {
                    part.to_string()
                } else {
                    temporary
                        .path()
                        .join("cache/pkg")
                        .join(part)
                        .display()
                        .to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);

        let custom = format!("/custom/{manager}-{version}");
        let output = isolated_command(temporary.path(), &project)
            .arg(manager)
            .env(override_key, &custom)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&custom),
            "{manager}@{version} did not preserve {override_key}: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        if manager == "pnpm" {
            let output = isolated_command(temporary.path(), &project)
                .arg(manager)
                .env("PNPM_HOME", "/custom/pnpm-home")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("/custom/pnpm-home"));
        }
    }
}
