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
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '{output}\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            &executable,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
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
