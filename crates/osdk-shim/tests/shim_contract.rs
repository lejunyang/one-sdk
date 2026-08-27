use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

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
        .env("OSDK_INSTALL_DIR", root.join("installs"))
        .env("CARGO_HOME", root.join("cargo"))
        .env("RUSTUP_HOME", root.join("rustup"));
    command
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn install_npm_fixture(root: &Path, project: &Path, npm_script: &str, node_script: &str) {
    std::fs::create_dir_all(project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"engines":{"node":"1.0.0"},"packageManager":"npm@2.0.0"}"#,
    )
    .unwrap();
    write_executable(&root.join("installs/npm/2.0.0/bin/npm"), npm_script);
    write_executable(&root.join("installs/node/1.0.0/bin/node"), node_script);
    for marker in [
        root.join("installs/npm/2.0.0/.osdk-complete"),
        root.join("installs/node/1.0.0/.osdk-complete"),
    ] {
        std::fs::write(marker, b"").unwrap();
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn install_dynamic_npm_fixture(
    root: &Path,
    project: &Path,
    tool_key: &str,
    backend_id: &str,
    version: &str,
    executable_name: &str,
    executable_script: &str,
    node_script: &str,
) {
    std::fs::create_dir_all(project).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        format!("[tools]\n{tool_key:?} = \"{backend_id}@{version}\"\nnode = \"1.0.0\"\n"),
    )
    .unwrap();
    let install_root = root
        .join("installs")
        .join(osdk_core::dirs::sanitize_tool_id(backend_id).join(version));
    write_executable(
        &install_root.join(format!("node_modules/.bin/{executable_name}")),
        executable_script,
    );
    let mut manifest = osdk_core::inventory::DynamicToolManifest::new(backend_id).unwrap();
    manifest.version = Some(version.into());
    manifest.config_keys = vec![tool_key.into()];
    manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
        name: executable_name.into(),
        path: format!("node_modules/.bin/{executable_name}"),
    }];
    manifest.metadata = [("node_version".into(), "1.0.0".into())]
        .into_iter()
        .collect();
    manifest.write_atomic(&install_root).unwrap();
    std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();

    write_executable(&root.join("installs/node/1.0.0/bin/node"), node_script);
    std::fs::write(root.join("installs/node/1.0.0/.osdk-complete"), b"").unwrap();
}

#[cfg(unix)]
fn configure_registry(root: &Path, url: &str) {
    let config = root.join("config/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        config,
        format!("[registries.npm]\nurls = [{url:?}]\nprobe_timeout_ms = 250\n"),
    )
    .unwrap();
}

#[cfg(unix)]
struct ProbeServer {
    url: String,
    requests: Arc<AtomicUsize>,
    request_headers: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl ProbeServer {
    fn start(status: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let request_headers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = requests.clone();
        let thread_request_headers = request_headers.clone();
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0_u8; 4096];
                        let length = stream.read(&mut request).unwrap_or(0);
                        thread_request_headers
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&request[..length]).into_owned());
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        let body = if status.starts_with("200") {
                            r#"{"name":"npm","version":"11.0.0"}"#
                        } else {
                            ""
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accepting registry probe failed: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            request_headers,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn assert_anonymous(&self) {
        for request in self.request_headers.lock().unwrap().iter() {
            let lower = request.to_ascii_lowercase();
            assert!(!lower.contains("\nauthorization:"), "{request}");
            assert!(!lower.contains("\ncookie:"), "{request}");
        }
    }

    fn wait_for_requests(&self, expected: usize) -> usize {
        for _ in 0..100 {
            let requests = self.request_count();
            if requests >= expected {
                return requests;
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.request_count()
    }
}

#[cfg(unix)]
impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
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
fn dependency_fetch_selects_registry_and_executes_manager_once() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf '%s|%s|%s\n' \"$npm_config_registry\" \"$*\" \"$(command -v npm)\" >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());
    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "fixture-package"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.wait_for_requests(1), 1);
    server.assert_anonymous();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "{}|install fixture-package|{}\n",
            server.url(),
            temporary
                .path()
                .join("installs/npm/2.0.0/bin/npm")
                .display()
        )
    );
}

#[cfg(unix)]
#[test]
fn npx_alias_preflights_and_uses_the_npm_backend_version() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        "#!/bin/sh\nexit 99\n",
        "#!/bin/sh\nexit 0\n",
    );
    write_executable(
        &temporary.path().join("installs/npm/2.0.0/bin/npx"),
        &format!(
            "#!/bin/sh\nprintf '%s|%s\n' \"$npm_config_registry\" \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());
    let output = isolated_command(temporary.path(), &project)
        .args(["npx", "fixture-package"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.wait_for_requests(1), 1);
    server.assert_anonymous();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("{}|fixture-package\n", server.url())
    );
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_injects_managed_node_and_uses_inventory_owned_bin() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!(
            "#!/bin/sh\nprintf '%s|%s|%s|%s\n' \"$npm_config_registry\" \"$*\" \"$(command -v node)\" \"$(command -v ni)\" > {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.ni\" = \"npm:@antfu/ni@1.0.0\"\nnode = \"2.0.0\"\n",
    )
    .unwrap();
    write_executable(
        &temporary.path().join("installs/node/2.0.0/bin/node"),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::write(
        temporary.path().join("installs/node/2.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();
    write_executable(
        &temporary
            .path()
            .join("installs/npm/@antfu/ni/1.0.0/node_modules/.bin/node"),
        "#!/bin/sh\nexit 99\n",
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());
    let output = isolated_command(temporary.path(), &project)
        .args(["ni", "install", "fixture-package"])
        .env("OSDK_TRUSTED_CONFIG_PATHS", &project)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 0);
    let line = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        line,
        format!(
            "|install fixture-package|{}|{}\n",
            temporary
                .path()
                .join("installs/node/1.0.0/bin/node")
                .display(),
            temporary
                .path()
                .join("installs/npm/@antfu/ni/1.0.0/node_modules/.bin/ni")
                .display()
        )
    );
}

#[cfg(unix)]
#[test]
fn dynamic_shim_fails_closed_on_corrupt_inventory_with_valid_configured_tool() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    let corrupt_root = temporary.path().join("installs/github/corrupt/tool/1.0.0");
    std::fs::create_dir_all(&corrupt_root).unwrap();
    std::fs::write(
        osdk_core::inventory::DynamicToolManifest::manifest_path(&corrupt_root),
        b"{not valid json",
    )
    .unwrap();

    let output = isolated_command(temporary.path(), &project)
        .arg("ni")
        .env("OSDK_TRUSTED_CONFIG_PATHS", &project)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing dynamic tool inventory scan"),
        "{stderr}"
    );
    assert!(stderr.contains("corrupt"), "{stderr}");
    assert!(
        stderr.contains(osdk_core::inventory::INVENTORY_FILE),
        "{stderr}"
    );
    assert!(!log.exists(), "configured dynamic tool unexpectedly ran");
}

#[cfg(unix)]
fn assert_dynamic_identity_rejected(root: &Path, project: &Path, log: &Path, expected: &str) {
    let output = isolated_command(root, project)
        .arg("ni")
        .env("OSDK_TRUSTED_CONFIG_PATHS", project)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr}");
    assert!(stderr.contains("reinstall"), "{stderr}");
    assert!(!log.exists(), "configured dynamic tool unexpectedly ran");
}

#[cfg(unix)]
#[test]
fn configured_dynamic_shim_rejects_missing_inventory() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    let install_root = temporary.path().join("installs/npm/@antfu/ni/1.0.0");
    std::fs::remove_file(osdk_core::inventory::DynamicToolManifest::manifest_path(
        &install_root,
    ))
    .unwrap();

    assert_dynamic_identity_rejected(
        temporary.path(),
        &project,
        &log,
        "missing or invalid install identity",
    );
}

#[cfg(unix)]
#[test]
fn configured_dynamic_shim_rejects_missing_completion_marker() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    let install_root = temporary.path().join("installs/npm/@antfu/ni/1.0.0");
    std::fs::remove_file(install_root.join(".osdk-complete")).unwrap();

    assert_dynamic_identity_rejected(
        temporary.path(),
        &project,
        &log,
        "no complete selected install",
    );
}

#[cfg(unix)]
#[test]
fn configured_dynamic_shim_rejects_schema_one_inventory() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    let install_root = temporary.path().join("installs/npm/@antfu/ni/1.0.0");
    let path = osdk_core::inventory::DynamicToolManifest::manifest_path(&install_root);
    std::fs::write(
        &path,
        r#"{"schema":1,"id":"npm:@antfu/ni","version":"1.0.0","config_keys":["tool.ni"],"bins":[{"name":"ni","path":"node_modules/.bin/ni"}],"metadata":{"node_version":"1.0.0"}}"#,
    )
    .unwrap();

    assert_dynamic_identity_rejected(temporary.path(), &project, &log, "different identity");
}

#[cfg(unix)]
#[test]
fn configured_dynamic_shim_rejects_option_identity_mismatch() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "npm:fixture-cli",
        "npm:fixture-cli",
        "1.2.3",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"npm:fixture-cli\" = { version = \"1.2.3\", installer = \"aube\" }\nnode = \"1.0.0\"\n",
    )
    .unwrap();
    let install_root = temporary.path().join("installs/npm/fixture-cli/1.2.3");
    let old_manifest = osdk_core::inventory::DynamicToolManifest::load(&install_root).unwrap();
    let mut manifest = osdk_core::inventory::DynamicToolManifest::new(&old_manifest.id)
        .unwrap()
        .with_identity_options(&std::collections::BTreeMap::from([(
            "installer".into(),
            "npm".into(),
        )]))
        .unwrap();
    manifest.version = old_manifest.version;
    manifest.config_keys = old_manifest.config_keys;
    manifest.bins = old_manifest.bins;
    manifest.metadata = old_manifest.metadata;
    manifest.write_atomic(&install_root).unwrap();

    assert_dynamic_identity_rejected(temporary.path(), &project, &log, "different identity");
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_restarts_from_global_only_canonical_root() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("global.log");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[tools]\n\"npm:fixture-cli\" = { version = \"1.2.3\", installer = \"aube\" }\nnode = \"1.0.0\"\n",
    )
    .unwrap();
    let global_root = temporary
        .path()
        .join("installs/npm-global/fixture-cli/1.2.3");
    write_executable(
        &global_root.join("bin/fixture-cli"),
        &format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$(command -v node)\" \"$(command -v fixture-cli)\" > {}\n",
            log.display()
        ),
    );
    let mut manifest = osdk_core::inventory::DynamicToolManifest::new("npm:fixture-cli")
        .unwrap()
        .with_identity_options(&std::collections::BTreeMap::from([(
            "installer".into(),
            "aube".into(),
        )]))
        .unwrap();
    manifest.version = Some("1.2.3".into());
    manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
        name: "fixture-cli".into(),
        path: "bin/fixture-cli".into(),
    }];
    manifest.metadata.insert("scope".into(), "global".into());
    manifest
        .metadata
        .insert("node_version".into(), "1.0.0".into());
    manifest.write_atomic(&global_root).unwrap();
    std::fs::write(global_root.join(".osdk-complete"), b"").unwrap();
    write_executable(
        &temporary.path().join("installs/node/1.0.0/bin/node"),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();

    let output = isolated_command(temporary.path(), &project)
        .arg("fixture-cli")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        format!(
            "{}|{}\n",
            temporary
                .path()
                .join("installs/node/1.0.0/bin/node")
                .display(),
            global_root.join("bin/fixture-cli").display()
        )
    );
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_does_not_fall_back_when_recorded_node_is_missing() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::remove_dir_all(temporary.path().join("installs/node/1.0.0")).unwrap();
    write_executable(
        &temporary.path().join("installs/node/2.0.0/bin/node"),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::write(
        temporary.path().join("installs/node/2.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.ni\" = \"npm:@antfu/ni@1.0.0\"\nnode = \"2.0.0\"\n",
    )
    .unwrap();

    let output = isolated_command(temporary.path(), &project)
        .arg("ni")
        .env("OSDK_TRUSTED_CONFIG_PATHS", &project)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("`npm:@antfu/ni` requires a managed Node installation"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!log.exists(), "dynamic npm CLI ran with a fallback Node");
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_rejects_legacy_manifest_without_node_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    let install_root = temporary.path().join("installs/npm/@antfu/ni/1.0.0");
    osdk_core::inventory::remove_manifest_metadata(&install_root, "node_version").unwrap();

    let output = isolated_command(temporary.path(), &project)
        .arg("ni")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        !log.exists(),
        "legacy manifest unexpectedly launched the CLI"
    );
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_rejects_non_exact_manifest_node_identity() {
    for recorded in ["", "latest", "1", "definitely-not-semver", " 1.0.0 "] {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let log = temporary.path().join("dynamic.log");
        install_dynamic_npm_fixture(
            temporary.path(),
            &project,
            "tool.ni",
            "npm:@antfu/ni",
            "1.0.0",
            "ni",
            &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
            "#!/bin/sh\nexit 0\n",
        );
        let install_root = temporary.path().join("installs/npm/@antfu/ni/1.0.0");
        osdk_core::inventory::update_manifest_metadata(&install_root, |metadata| {
            metadata.insert("node_version".into(), recorded.into());
        })
        .unwrap();

        let output = isolated_command(temporary.path(), &project)
            .arg("ni")
            .output()
            .unwrap();

        assert!(
            !output.status.success(),
            "accepted node_version={recorded:?}"
        );
        assert!(
            !log.exists(),
            "CLI ran with invalid node_version={recorded:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_rejects_recorded_node_without_executable() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("dynamic.log");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        &format!("#!/bin/sh\nprintf ran > {}\n", log.display()),
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::remove_file(temporary.path().join("installs/node/1.0.0/bin/node")).unwrap();

    let output = isolated_command(temporary.path(), &project)
        .arg("ni")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        !log.exists(),
        "dynamic npm CLI ran without its recorded Node executable"
    );
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_localizes_missing_managed_node_from_environment() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        "#!/bin/sh\nexit 0\n",
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::remove_dir_all(temporary.path().join("installs/node")).unwrap();

    let output = isolated_command(temporary.path(), &project)
        .args(["ni"])
        .env("OSDK_TRUSTED_CONFIG_PATHS", &project)
        .env("OSDK_LANG", "zh")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`npm:@antfu/ni` 需要受管的 Node 安装"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn dynamic_npm_shim_localizes_missing_managed_node_from_config() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    install_dynamic_npm_fixture(
        temporary.path(),
        &project,
        "tool.ni",
        "npm:@antfu/ni",
        "1.0.0",
        "ni",
        "#!/bin/sh\nexit 0\n",
        "#!/bin/sh\nexit 0\n",
    );
    std::fs::remove_dir_all(temporary.path().join("installs/node")).unwrap();
    let config = temporary.path().join("config/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[settings]\nlang = \"zh\"\n").unwrap();

    let output = isolated_command(temporary.path(), &project)
        .args(["ni"])
        .env("OSDK_TRUSTED_CONFIG_PATHS", &project)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`npm:@antfu/ni` 需要受管的 Node 安装"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn launcher_aliases_use_managed_canonical_binaries_once() {
    for (backend, version, alias, canonical, subcommand, registry_key) in [
        (
            "pnpm",
            "10.0.0",
            "pnpx",
            "pnpm",
            "dlx",
            "pnpm_config_registry",
        ),
        ("bun", "1.2.3", "bunx", "bun", "x", "BUN_CONFIG_REGISTRY"),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let tools = if backend == "pnpm" {
            format!("[tools]\n{backend} = \"{version}\"\nnode = \"1.0.0\"\n")
        } else {
            format!("[tools]\n{backend} = \"{version}\"\n")
        };
        std::fs::write(project.join("osdk.toml"), tools).unwrap();

        let managed_log = temporary.path().join("managed.log");
        let managed = if backend == "pnpm" {
            temporary
                .path()
                .join(format!("installs/{backend}/{version}/{canonical}"))
        } else {
            temporary
                .path()
                .join(format!("installs/{backend}/{version}/bin/{canonical}"))
        };
        write_executable(
            &managed,
            &format!(
                "#!/bin/sh\nprintf '%s|%s\n' \"${{{registry_key}-unset}}\" \"$*\" >> {}\n",
                managed_log.display()
            ),
        );
        std::fs::write(
            temporary
                .path()
                .join(format!("installs/{backend}/{version}/.osdk-complete")),
            b"",
        )
        .unwrap();
        if backend == "pnpm" {
            write_executable(
                &temporary.path().join("installs/node/1.0.0/bin/node"),
                "#!/bin/sh\nexit 0\n",
            );
            std::fs::write(
                temporary.path().join("installs/node/1.0.0/.osdk-complete"),
                b"",
            )
            .unwrap();
        }

        let global_log = temporary.path().join("global.log");
        let global_bin = temporary.path().join("global-bin");
        write_executable(
            &global_bin.join(alias),
            &format!(
                "#!/bin/sh\nprintf 'global\n' >> {}\nexit 88\n",
                global_log.display()
            ),
        );
        let server = ProbeServer::start("200 OK");
        configure_registry(temporary.path(), server.url());

        let output = isolated_command(temporary.path(), &project)
            .args([alias, "fixture-package", "--registry", "child-value"])
            .env("PATH", &global_bin)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(server.wait_for_requests(1), 1, "{alias}");
        server.assert_anonymous();
        assert_eq!(
            std::fs::read_to_string(&managed_log).unwrap(),
            format!(
                "{}|{subcommand} fixture-package --registry child-value\n",
                server.url()
            ),
            "{alias}"
        );
        assert!(!global_log.exists(), "{alias} escaped to the user PATH");
    }
}

#[cfg(unix)]
#[test]
fn node_only_activation_routes_bundled_npm_and_npx_through_preflight() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"engines":{"node":"1.0.0"}}"#,
    )
    .unwrap();

    let node_bin = temporary.path().join("installs/node/1.0.0/bin");
    write_executable(&node_bin.join("node"), "#!/bin/sh\nexit 0\n");
    for alias in ["npm", "npx"] {
        let log = temporary.path().join(format!("{alias}.log"));
        write_executable(
            &node_bin.join(alias),
            &format!(
                "#!/bin/sh\nprintf '%s|%s\n' \"$npm_config_registry\" \"$*\" > {}\nexit 0\n",
                log.display()
            ),
        );
    }
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();

    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
        "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
        "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
        "OSDK_STORE_DIR" => Some(temporary.path().join("store").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(temporary.path().join("installs").display().to_string()),
        _ => None,
    })
    .unwrap();
    dirs.ensure().unwrap();
    let config = osdk_core::config::Config::load(&dirs.user_config_file(), &project).unwrap();
    let context = osdk_core::backend::Ctx {
        cas: Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
        dirs: dirs.clone(),
        platform: osdk_core::platform::Platform::current(),
        config,
        client: osdk_core::http::client().unwrap(),
        show_progress: false,
    };
    let registry = osdk_core::backend::registry::Registry::new();
    let node_backend = registry.get("node").unwrap();
    let version = osdk_core::version::ToolVersion::new("node", "1.0.0");
    let routed =
        osdk_core::shim::routed_bin_names(&context, node_backend.as_ref(), &version).unwrap();
    assert!(routed.contains(&"npm".to_string()));
    assert!(routed.contains(&"npx".to_string()));
    for alias in routed {
        osdk_core::shim::generate_shim(&dirs, &alias, &shim()).unwrap();
    }

    let activation = osdk_core::activate::compute_env_delta(&context, &registry, &project).unwrap();
    assert_eq!(activation.path_prepend, vec![dirs.shims(), node_bin]);
    let activation_path = std::env::join_paths(activation.path_prepend).unwrap();

    for (alias, args) in [
        ("npm", vec!["install", "fixture-package"]),
        ("npx", vec!["fixture-package"]),
    ] {
        let output = Command::new(alias)
            .args(&args)
            .current_dir(&project)
            .env_clear()
            .env("HOME", temporary.path().join("home"))
            .env("PATH", &activation_path)
            .env("OSDK_DATA_DIR", temporary.path().join("data"))
            .env("OSDK_CACHE_DIR", temporary.path().join("cache"))
            .env("OSDK_CONFIG_DIR", temporary.path().join("config"))
            .env("OSDK_STORE_DIR", temporary.path().join("store"))
            .env("OSDK_INSTALL_DIR", temporary.path().join("installs"))
            .env("CARGO_HOME", temporary.path().join("cargo"))
            .env("RUSTUP_HOME", temporary.path().join("rustup"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(temporary.path().join(format!("{alias}.log"))).unwrap(),
            format!("{}|{}\n", server.url(), args.join(" "))
        );
    }

    assert_eq!(server.wait_for_requests(2), 2);
    server.assert_anonymous();
}

#[cfg(unix)]
#[test]
fn yarn_registry_env_follows_the_owning_backend_major() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let node = temporary.path().join("installs/node/1.0.0/bin/node");
    write_executable(&node, "#!/bin/sh\nexit 0\n");
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    for (version, expected_key, unexpected_key) in [
        ("1.22.22", "YARN_REGISTRY", "YARN_NPM_REGISTRY_SERVER"),
        ("4.10.3", "YARN_NPM_REGISTRY_SERVER", "YARN_REGISTRY"),
    ] {
        std::fs::write(
            project.join("package.json"),
            format!(r#"{{"engines":{{"node":"1.0.0"}},"packageManager":"yarn@{version}"}}"#),
        )
        .unwrap();
        let install = temporary.path().join(format!("installs/yarn/{version}"));
        let log = temporary.path().join(format!("yarn-{version}.log"));
        write_executable(
            &install.join("bin/yarn"),
            &format!(
                "#!/bin/sh\nprintf '%s|%s|%s\n' \"${{{expected_key}-unset}}\" \"${{{unexpected_key}-unset}}\" \"$*\" > {}\nexit 0\n",
                log.display()
            ),
        );
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();

        let output = isolated_command(temporary.path(), &project)
            .args(["yarn", "add", "fixture-package"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{version}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            format!("{}|unset|add fixture-package\n", server.url())
        );
    }
    assert_eq!(server.wait_for_requests(2), 2);
    server.assert_anonymous();
}

#[cfg(unix)]
#[test]
fn explicit_registry_is_passed_through_without_probe() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf '%s|%s\n' \"${{npm_config_registry-unset}}\" \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    let explicit = "https://explicit.example.test/";
    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "--registry", explicit, "fixture-package"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 0);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("unset|install --registry {explicit} fixture-package\n")
    );

    let output = isolated_command(temporary.path(), &project)
        .args([
            "npm",
            "install",
            "--registry=https://equals.example.test/",
            "fixture-package",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 0);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "unset|install --registry {explicit} fixture-package\nunset|install --registry=https://equals.example.test/ fixture-package\n"
        )
    );
}

#[cfg(unix)]
#[test]
fn explicit_registry_environment_is_preserved_without_probe() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf '%s|%s\n' \"$npm_config_registry\" \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    let explicit = "https://environment.example.test/";
    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "fixture-package"])
        .env("npm_config_registry", explicit)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 0);
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        format!("{explicit}|install fixture-package\n")
    );
}

#[cfg(unix)]
#[test]
fn unavailable_registry_does_not_execute_manager() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf 'executed\n' >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let server = ProbeServer::start("503 Service Unavailable");
    configure_registry(temporary.path(), server.url());

    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "fixture-package"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(server.wait_for_requests(1), 1);
    server.assert_anonymous();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no reachable package registry"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!log.exists(), "unavailable registry still ran npm");
}

#[cfg(unix)]
#[test]
fn non_fetching_manager_command_does_not_probe() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf '%s\n' \"$*\" >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "--version"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 0);
    assert_eq!(std::fs::read_to_string(log).unwrap(), "--version\n");
}

#[cfg(unix)]
#[test]
fn non_manager_shim_does_not_probe() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\nnode = \"1.0.0\"\n").unwrap();
    let node = temporary.path().join("installs/node/1.0.0/bin/node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::write(&node, "#!/bin/sh\nprintf 'node\n'\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();
    let server = ProbeServer::start("200 OK");
    configure_registry(temporary.path(), server.url());

    let output = isolated_command(temporary.path(), &project)
        .arg("node")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(server.request_count(), 0);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "node\n");
}

#[cfg(unix)]
#[test]
fn lifecycle_path_uses_real_manager_instead_of_reentering_shims() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf '%s\n' \"$(command -v npm)\" > {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let shims = temporary.path().join("data/shims");
    std::fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(shim(), shims.join("npm")).unwrap();

    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "--version"])
        .env("PATH", &shims)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(log).unwrap().trim(),
        temporary
            .path()
            .join("installs/npm/2.0.0/bin/npm")
            .display()
            .to_string()
    );
}

#[cfg(unix)]
#[test]
fn untrusted_project_registry_is_rejected_but_safe_pins_still_run() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf 'executed\n' >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let safe = isolated_command(temporary.path(), &project)
        .args(["npm", "--version"])
        .output()
        .unwrap();
    assert!(safe.status.success());

    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\nnpm = \"2.0.0\"\nnode = \"1.0.0\"\n[registries.npm]\nurls = [\"https://registry.example.test/\"]\n",
    )
    .unwrap();
    let rejected = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "fixture-package"])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("is not trusted"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(std::fs::read_to_string(log).unwrap(), "executed\n");
}

#[cfg(unix)]
#[test]
fn malformed_registry_configuration_fails_closed() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let log = temporary.path().join("manager.log");
    install_npm_fixture(
        temporary.path(),
        &project,
        &format!(
            "#!/bin/sh\nprintf 'executed\n' >> {}\nexit 0\n",
            log.display()
        ),
        "#!/bin/sh\nexit 0\n",
    );
    let config = temporary.path().join("config/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        config,
        "[registries.npm]\nurls = [\"not a registry URL\"]\n",
    )
    .unwrap();

    let output = isolated_command(temporary.path(), &project)
        .args(["npm", "install", "fixture-package"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid registry URL"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!log.exists(), "manager must not run with discarded policy");
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
            .arg("--version")
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
            .arg("--version")
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
                .arg("--version")
                .env("PNPM_HOME", "/custom/pnpm-home")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("/custom/pnpm-home"));
        }
    }
}
