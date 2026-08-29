use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
#[cfg(not(windows))]
use std::{io::Write, net::TcpListener, net::TcpStream};

const ISOLATED_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const FIXTURE_SERVER_TIMEOUT: Duration = Duration::from_secs(10);

fn osdk() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_osdk"))
}

fn osdk_aube() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_osdk-aube"))
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
        .env("OSDK_INSTALL_DIR", &installs)
        // Referencing Cargo's binary variable makes the sidecar an explicit
        // integration-test artifact. Individual tests can still override it
        // with a protocol fixture below.
        .env("OSDK_AUBE_BIN", osdk_aube());
    for (key, value) in env {
        command.env(key, value);
    }
    run_with_timeout(command, ISOLATED_COMMAND_TIMEOUT)
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = Output {
        status,
        stdout: stdout_reader.join().unwrap(),
        stderr: stderr_reader.join().unwrap(),
    };
    if timed_out {
        panic!(
            "isolated command timed out after {timeout:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(windows))]
fn accept_fixture_connection(listener: &TcpListener, context: &str) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + FIXTURE_SERVER_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(FIXTURE_SERVER_TIMEOUT))
                    .unwrap();
                stream
                    .set_write_timeout(Some(FIXTURE_SERVER_TIMEOUT))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!(
                        "{context}: no loopback request arrived within {:?}",
                        FIXTURE_SERVER_TIMEOUT
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{context}: accepting loopback request failed: {error}"),
        }
    }
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
    assert!(
        stdout.contains("registries.npm.urls = built-in (npmjs + npmmirror)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("registries.npm.probe_timeout_ms = 1500"),
        "{stdout}"
    );
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
fn global_model_env_enable_force_and_disable_round_trip() {
    let temporary = tempfile::tempdir().unwrap();
    let enabled = run_isolated(temporary.path(), &["model", "env", "enable", "huggingface"]);
    assert!(
        enabled.status.success(),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let config = std::fs::read_to_string(temporary.path().join("config/config.toml")).unwrap();
    assert!(config.contains("[sources.huggingface]"));
    assert!(config.contains("env = true"));

    let hook = run_isolated(temporary.path(), &["hook-env", "--shell", "bash"]);
    let hook = String::from_utf8(hook.stdout).unwrap();
    assert!(hook.contains("export HF_ENDPOINT='https://huggingface.co'"));
    assert!(hook.contains("export HF_HOME="));
    assert!(hook.contains("export HF_HUB_CACHE="));

    let repeated = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["hook-env", "--shell", "bash"],
        &[
            ("HF_ENDPOINT", "https://huggingface.co"),
            ("OSDK_MANAGED_ENV", "HF_ENDPOINT"),
            ("OSDK_ORIG_HF_ENDPOINT", "https://user.example"),
            ("OSDK_ORIG_HF_ENDPOINT_PRESENT", "1"),
            ("OSDK_ORIG_HF_ENDPOINT_SET", "1"),
        ],
    );
    let repeated = String::from_utf8(repeated.stdout).unwrap();
    assert!(repeated.contains("export HF_ENDPOINT='https://huggingface.co'"));
    assert!(!repeated.contains("export HF_ENDPOINT=\"$OSDK_ORIG_HF_ENDPOINT\""));

    let preserved = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["hook-env", "--shell", "bash"],
        &[("HF_ENDPOINT", "https://user.example")],
    );
    let preserved = String::from_utf8(preserved.stdout).unwrap();
    assert!(!preserved.contains("export HF_ENDPOINT='https://huggingface.co'"));

    let forced = run_isolated(
        temporary.path(),
        &["model", "env", "enable", "huggingface", "--force"],
    );
    assert!(forced.status.success());
    let overridden = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["hook-env", "--shell", "bash"],
        &[("HF_ENDPOINT", "https://user.example")],
    );
    assert!(String::from_utf8(overridden.stdout)
        .unwrap()
        .contains("export HF_ENDPOINT='https://huggingface.co'"));

    let disabled = run_isolated(
        temporary.path(),
        &["model", "env", "disable", "huggingface"],
    );
    assert!(disabled.status.success());
    let restore = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["hook-env", "--shell", "bash"],
        &[
            ("HF_ENDPOINT", "https://huggingface.co"),
            ("OSDK_MANAGED_ENV", "HF_ENDPOINT"),
            ("OSDK_ORIG_HF_ENDPOINT", "https://user.example"),
            ("OSDK_ORIG_HF_ENDPOINT_PRESENT", "1"),
            ("OSDK_ORIG_HF_ENDPOINT_SET", "1"),
        ],
    );
    let restore = String::from_utf8(restore.stdout).unwrap();
    assert!(restore.contains("export HF_ENDPOINT=\"$OSDK_ORIG_HF_ENDPOINT\""));
}

#[test]
fn global_custom_model_endpoint_suppresses_tokens_by_default() {
    let temporary = tempfile::tempdir().unwrap();
    let added = run_isolated(
        temporary.path(),
        &[
            "source",
            "add",
            "huggingface",
            "--id",
            "corp",
            "--download-url",
            "https://hub.example.test",
        ],
    );
    assert!(added.status.success());
    assert!(
        run_isolated(temporary.path(), &["source", "pin", "huggingface", "corp"])
            .status
            .success()
    );
    assert!(
        run_isolated(temporary.path(), &["model", "env", "enable", "huggingface"])
            .status
            .success()
    );
    let hook = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["hook-env", "--shell", "bash"],
        &[("HF_TOKEN", "secret")],
    );
    let hook = String::from_utf8(hook.stdout).unwrap();
    assert!(hook.contains("export HF_ENDPOINT='https://hub.example.test'"));
    assert!(hook.contains("export HF_TOKEN=''"));
    assert!(hook.contains("export HF_HUB_DISABLE_IMPLICIT_TOKEN='1'"));
    let config = std::fs::read_to_string(temporary.path().join("config/config.toml")).unwrap();
    assert!(!config.contains("secret"));
}

#[test]
fn activation_scripts_refresh_environment_immediately() {
    let temporary = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh", "fish"] {
        let output = run_isolated(temporary.path(), &["activate", shell]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.lines().any(|line| line == "_osdk_hook"));
    }
    let powershell = run_isolated(temporary.path(), &["activate", "powershell"]);
    assert!(String::from_utf8(powershell.stdout)
        .unwrap()
        .contains("Invoke-OsdkHook"));
}

#[test]
fn package_cache_hook_refreshes_managed_values_and_preserves_user_overrides() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"npm@11.5.2"}"#,
    )
    .unwrap();
    let install = temporary.path().join("installs/npm/11.5.2");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();
    // Build every component separately so the expected string uses the
    // target platform's separator. `Path::join("cache/pkg/npm")` keeps the
    // embedded forward slashes on Windows, while the CLI constructs this path
    // one component at a time and prints backslashes.
    let expected = temporary.path().join("cache").join("pkg").join("npm");

    let initial = run_isolated_in(temporary.path(), &project, &["hook-env", "--shell", "bash"]);
    let initial = String::from_utf8(initial.stdout).unwrap();
    assert!(initial.contains(&format!("export npm_config_cache='{}'", expected.display())));

    let preserved = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["hook-env", "--shell", "bash"],
        &[("npm_config_cache", "/custom/npm")],
    );
    assert!(!String::from_utf8(preserved.stdout)
        .unwrap()
        .contains("export npm_config_cache="));

    let managed = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["hook-env", "--shell", "bash"],
        &[
            ("npm_config_cache", "/old/osdk/pkg/npm"),
            ("OSDK_ORIG_npm_config_cache_SET", "1"),
        ],
    );
    assert!(String::from_utf8(managed.stdout)
        .unwrap()
        .contains(&format!("export npm_config_cache='{}'", expected.display())));
}

// Windows runners can block a child process from connecting back to a listener
// owned by the test process. The same provider/pull contract runs in-process in
// osdk-core on Windows; keep this cross-process CLI topology on Unix.
#[cfg(not(windows))]
#[test]
fn huggingface_model_pull_materializes_and_locks_snapshot() {
    let payload = br#"{"model":"fixture"}"#.to_vec();
    let digest =
        osdk_core::pipeline::verify::hash_bytes(&payload, osdk_core::pipeline::HashAlgo::Sha256);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server_payload = payload.clone();
    let server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let mut stream =
                accept_fixture_connection(&listener, "Hugging Face pull fixture server");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if request_number == 0 {
                let body = format!(
                    r#"{{"sha":"abc123","siblings":[{{"rfilename":"config.json","lfs":{{"sha256":"{digest}","size":{}}}}}]}}"#,
                    server_payload.len()
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
                    server_payload.len()
                )
                .unwrap();
                stream.write_all(&server_payload).unwrap();
            }
        }
    });

    let temporary = tempfile::tempdir().unwrap();
    let endpoint = format!("http://{address}");
    let output = run_isolated(
        temporary.path(),
        &[
            "model",
            "pull",
            "fixture",
            "hf:owner/repo@main",
            "--endpoint",
            &endpoint,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let path = run_isolated(temporary.path(), &["model", "path", "fixture"]);
    assert!(path.status.success());
    let snapshot = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
    assert_eq!(
        std::fs::read(snapshot.join("config.json")).unwrap(),
        payload
    );
    let lock = std::fs::read_to_string(temporary.path().join("osdk.lock")).unwrap();
    assert!(lock.contains("[models.fixture]"));
    assert!(lock.contains("revision = \"abc123\""));
    assert!(lock.contains("sha256 ="));
}

// See `huggingface_model_pull_materializes_and_locks_snapshot`.
#[cfg(not(windows))]
#[test]
fn modelscope_model_pull_materializes_and_locks_manifest_revision() {
    let payload = br#"{"model":"modelscope-fixture"}"#.to_vec();
    let digest =
        osdk_core::pipeline::verify::hash_bytes(&payload, osdk_core::pipeline::HashAlgo::Sha256);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server_payload = payload.clone();
    let server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let mut stream = accept_fixture_connection(&listener, "ModelScope pull fixture server");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if request_number == 0 {
                let body = format!(
                    r#"{{"Code":200,"Success":true,"Message":"success","Data":{{"Files":[{{"Path":"config.json","Size":{},"Sha256":"{digest}","Type":"blob"}}]}}}}"#,
                    server_payload.len()
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    server_payload.len()
                )
                .unwrap();
                stream.write_all(&server_payload).unwrap();
            }
        }
    });

    let temporary = tempfile::tempdir().unwrap();
    let endpoint = format!("http://{address}");
    let output = run_isolated(
        temporary.path(),
        &[
            "model",
            "pull",
            "fixture",
            "ms:owner/repo@master",
            "--endpoint",
            &endpoint,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let path = run_isolated(temporary.path(), &["model", "path", "fixture"]);
    let snapshot = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
    assert_eq!(
        std::fs::read(snapshot.join("config.json")).unwrap(),
        payload
    );
    let lock = std::fs::read_to_string(temporary.path().join("osdk.lock")).unwrap();
    assert!(lock.contains("provider = \"modelscope\""));
    assert!(lock.contains("revision = \"master+manifest-"));
}

// See `huggingface_model_pull_materializes_and_locks_snapshot`.
#[cfg(not(windows))]
#[test]
fn model_source_test_probes_target_file_and_prints_ranking() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let mut stream =
                accept_fixture_connection(&listener, "model source probe fixture server");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(!request
                .to_ascii_lowercase()
                .contains("authorization: bearer"));
            if request_number == 0 {
                assert!(request.contains("/api/models/owner/repo/revision/main"));
                let body = r#"{"sha":"abc123","siblings":[{"rfilename":"weights.bin","size":4}]}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                assert!(request
                    .to_ascii_lowercase()
                    .contains("range: bytes=0-1048575"));
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4\r\nConnection: close\r\n\r\ndata",
                    )
                    .unwrap();
            }
        }
    });
    let temporary = tempfile::tempdir().unwrap();
    let endpoint = format!("http://{address}");
    let added = run_isolated(
        temporary.path(),
        &[
            "source",
            "add",
            "huggingface",
            "--id",
            "fixture",
            "--download-url",
            &endpoint,
        ],
    );
    assert!(added.status.success());
    std::fs::write(
        temporary.path().join("config/config.toml"),
        format!(
            r#"
[sources.huggingface]
disable = ["official"]

[[sources.huggingface.custom]]
id = "fixture"
kind = "custom"
download_url = "{endpoint}"
"#
        ),
    )
    .unwrap();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "source",
            "test",
            "huggingface",
            "--model",
            "owner/repo@main",
        ],
        &[("HF_TOKEN", "must-not-leak")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("fixture"));
}

// See `huggingface_model_pull_materializes_and_locks_snapshot`.
#[cfg(not(windows))]
#[test]
fn model_pull_fails_over_within_provider() {
    let failing = TcpListener::bind("127.0.0.1:0").unwrap();
    let failing_address = failing.local_addr().unwrap();
    let failing_server = std::thread::spawn(move || {
        let mut stream =
            accept_fixture_connection(&failing, "failing model endpoint fixture server");
        let mut request = [0u8; 2048];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });

    let payload = br#"{"model":"fallback"}"#.to_vec();
    let digest =
        osdk_core::pipeline::verify::hash_bytes(&payload, osdk_core::pipeline::HashAlgo::Sha256);
    let healthy = TcpListener::bind("127.0.0.1:0").unwrap();
    let healthy_address = healthy.local_addr().unwrap();
    let server_payload = payload.clone();
    let healthy_server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let mut stream =
                accept_fixture_connection(&healthy, "healthy model endpoint fixture server");
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            if request_number == 0 {
                let body = format!(
                    r#"{{"sha":"abc123","siblings":[{{"rfilename":"config.json","lfs":{{"sha256":"{digest}","size":{}}}}}]}}"#,
                    server_payload.len()
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    server_payload.len()
                )
                .unwrap();
                stream.write_all(&server_payload).unwrap();
            }
        }
    });

    let temporary = tempfile::tempdir().unwrap();
    let config = format!(
        r#"
[sources]
selection = "ordered"

[sources.huggingface]
disable = ["official"]

[[sources.huggingface.custom]]
id = "failing"
kind = "custom"
download_url = "http://{failing_address}"
priority = 0

[[sources.huggingface.custom]]
id = "healthy"
kind = "custom"
download_url = "http://{healthy_address}"
priority = 1
"#
    );
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(temporary.path().join("config/config.toml"), config).unwrap();
    let output = run_isolated(
        temporary.path(),
        &["model", "pull", "fixture", "hf:owner/repo@main"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    failing_server.join().unwrap();
    healthy_server.join().unwrap();
    let path = run_isolated(temporary.path(), &["model", "path", "fixture"]);
    let snapshot = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
    assert_eq!(
        std::fs::read(snapshot.join("config.json")).unwrap(),
        payload
    );
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
fn cache_env_lists_both_pnpm_store_variable_generations() {
    let temporary = tempfile::tempdir().unwrap();
    let output = run_isolated(temporary.path(), &["cache", "env"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let package_cache = temporary.path().join("cache").join("pkg");
    let store = package_cache.join("pnpm-store");
    assert!(stdout.contains(&format!(
        "PNPM_HOME={}",
        package_cache.join("pnpm").display()
    )));
    assert!(stdout.contains(&format!("npm_config_store_dir={}", store.display())));
    assert!(stdout.contains(&format!("pnpm_config_store_dir={}", store.display())));
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
    use std::fs::File;
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("cache/downloads/archive.tar.gz");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(&archive, b"fixture").unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(
        result,
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    use std::os::fd::FromRawFd;
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
    assert_ne!(
        flags,
        -1,
        "F_GETFL failed: {}",
        std::io::Error::last_os_error()
    );
    assert_ne!(
        unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1,
        "F_SETFL failed: {}",
        std::io::Error::last_os_error()
    );
    let mut command = Command::new(osdk());
    command
        .args(["cache", "clean"])
        .current_dir(temp.path())
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "")
        .env("LANG", "C")
        .env("OSDK_DATA_DIR", temp.path().join("data"))
        .env("OSDK_CACHE_DIR", temp.path().join("cache"))
        .env("OSDK_CONFIG_DIR", temp.path().join("config"))
        .env("OSDK_STORE_DIR", temp.path().join("store"))
        .env("OSDK_INSTALL_DIR", temp.path().join("installs"))
        .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slave));
    let mut child = command.spawn().unwrap();
    use std::io::{Read, Write};
    let mut captured = Vec::new();
    let prompt_marker = b"[y/N]:";
    let mut answered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status;
    loop {
        loop {
            let mut bytes = [0; 256];
            match master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    captured.extend_from_slice(&bytes[..count]);
                    if !answered
                        && captured
                            .windows(prompt_marker.len())
                            .any(|window| window == prompt_marker)
                    {
                        master.write_all(b"y\n").unwrap();
                        master.flush().unwrap();
                        answered = true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("reading PTY failed: {error}"),
            }
        }
        if let Some(exit_status) = child.try_wait().unwrap() {
            status = exit_status;
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!(
                "terminal command timed out: {}",
                String::from_utf8_lossy(&captured)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        status.success(),
        "terminal output={}",
        String::from_utf8_lossy(&captured)
    );
    assert!(answered, "confirmation prompt was not observed");
    assert!(!archive.exists());
    assert!(String::from_utf8_lossy(&captured).contains("[y/N]:"));
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
    assert!(stdout.contains(
        "node, npm, go, python, java, maven, gradle, kotlin, rust, pnpm, yarn, deno, bun"
    ));
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
fn go_tool_use_publishes_locks_reuses_offline_and_uninstalls_exact_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for request_number in 0..3 {
            let mut stream = accept_fixture_connection(&listener, "Go proxy fixture server");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            let expected = match request_number {
                0 => "/example.com/acme/tool/cmd/tool/@v/v1.2.3.info",
                1 => "/example.com/acme/tool/cmd/@v/v1.2.3.info",
                _ => "/example.com/acme/tool/@v/v1.2.3.info",
            };
            assert!(
                request.starts_with(&format!("GET {expected} ")),
                "{request}"
            );
            if request_number < 2 {
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            } else {
                let body = r#"{"Version":"v1.2.3","Time":"2026-01-01T00:00:00Z"}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        }
    });

    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        format!(
            r#"[sources]
selection = "ordered"

[tools]
go = "1.24.0"

[[sources."go:example.com/acme/tool/cmd/tool".custom]]
id = "fixture"
kind = "custom"
download_url = {endpoint:?}
"#
        ),
    )
    .unwrap();

    let runtime = temporary.path().join("installs/go/1.24.0");
    std::fs::create_dir_all(runtime.join("pkg/tool")).unwrap();
    std::fs::create_dir_all(runtime.join("src/runtime")).unwrap();
    let calls = temporary.path().join("go-provider.log");
    write_executable(
        &runtime.join("bin/go"),
        &format!(
            "#!/bin/sh\nset -eu\nmkdir -p \"$GOBIN\"\nprintf '#!/bin/sh\nprintf go-tool-ok\\n\n' > \"$GOBIN/tool\"\nchmod +x \"$GOBIN/tool\"\nprintf '%s|%s|%s|%s|%s\\n' \"$*\" \"$GOROOT\" \"$GOPROXY\" \"$GOTOOLCHAIN\" \"$CGO_ENABLED\" >> '{}'\n",
            calls.display()
        ),
    );
    write_executable(&runtime.join("bin/gofmt"), "#!/bin/sh\nexit 0\n");
    write_executable(&runtime.join("pkg/tool/compile"), "#!/bin/sh\nexit 0\n");
    std::fs::write(runtime.join("src/runtime/runtime.go"), "package runtime\n").unwrap();
    std::fs::write(runtime.join("VERSION"), "go1.24.0\n").unwrap();
    std::fs::write(runtime.join("go.env"), "GOTOOLCHAIN=local\n").unwrap();
    std::fs::write(runtime.join(".osdk-complete"), b"").unwrap();

    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["use", "go:example.com/acme/tool/cmd/tool@1.2.3"],
        &[
            ("PATH", "/usr/bin:/bin"),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();

    let config = std::fs::read_to_string(project.join("osdk.toml")).unwrap();
    assert!(config.contains("\"go:example.com/acme/tool/cmd/tool\" = \"1.2.3\""));
    let lock = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    for expected in [
        "schema = 4",
        "runtime = \"go\"",
        "runtime_version = \"1.24.0\"",
        "source = \"http://127.0.0.1:",
        "module = \"example.com/acme/tool\"",
    ] {
        assert!(lock.contains(expected), "missing {expected}: {lock}");
    }
    let provider_calls = std::fs::read_to_string(&calls).unwrap();
    assert!(provider_calls.contains(&format!(
        "install example.com/acme/tool/cmd/tool@v1.2.3|{}|{}|local|0",
        runtime.display(),
        endpoint
    )));

    let offline = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["--offline", "install"],
        &[
            ("PATH", "/usr/bin:/bin"),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );
    assert!(
        offline.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&offline.stdout),
        String::from_utf8_lossy(&offline.stderr)
    );
    assert_eq!(std::fs::read_to_string(&calls).unwrap(), provider_calls);

    let location = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["where", "go:example.com/acme/tool/cmd/tool@1.2.3"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap())],
    );
    assert!(
        location.status.success(),
        "{}",
        String::from_utf8_lossy(&location.stderr)
    );
    let install_root = PathBuf::from(String::from_utf8(location.stdout).unwrap().trim());
    assert!(install_root.join("bin/tool").is_file());

    let uninstall = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &[
            "--yes",
            "uninstall",
            "go:example.com/acme/tool/cmd/tool@1.2.3",
        ],
        &[("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap())],
    );
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    assert!(!install_root.exists());
}

#[cfg(unix)]
#[test]
fn use_preserves_unique_indirect_project_key_for_preinstalled_backend() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.node\" = \"node@1.0.0\"\n",
    )
    .unwrap();

    let install = temporary.path().join("installs/node/1.0.0");
    let node_bin = install.join("bin/node");
    write_executable(&node_bin, "#!/bin/sh\nexit 0\n");
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    let shim_bin_dir = temporary.path().join("bin");
    write_executable(
        &shim_bin_dir.join("osdk-shim"),
        "#!/bin/sh\nprintf 'shim placeholder\\n'\n",
    );
    let data_bin = temporary.path().join("data/bin");
    std::fs::create_dir_all(&data_bin).unwrap();
    std::os::unix::fs::symlink(shim_bin_dir.join("osdk-shim"), data_bin.join("osdk-shim")).unwrap();

    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["--offline", "use", "tool.node"],
        &[
            ("PATH", shim_bin_dir.to_str().unwrap()),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = std::fs::read_to_string(project.join("osdk.toml")).unwrap();
    assert!(
        config.contains("\"tool.node\" = \"node@1.0.0\""),
        "{config}"
    );
    assert!(!config.contains("\nnode = "), "{config}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("pinned tool.node@1.0.0"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn global_use_rejects_indirect_npm_alias_without_reading_project_config() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[tools]\n\"tool.fixture\" = { version = \"npm:fixture-cli@1.2.3\", installer = \"aube\" }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.fixture\" = { version = \"npm:fixture-cli@9.9.9\", installer = \"npm\" }\n",
    )
    .unwrap();

    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["use", "--global", "tool.fixture"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap())],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("global npm package aliases are not supported"),
        "{stderr}"
    );
    let config = std::fs::read_to_string(temporary.path().join("config/config.toml")).unwrap();
    assert!(config.contains("installer = \"aube\""), "{config}");
    assert!(!config.contains("9.9.9"), "{config}");
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

#[cfg(unix)]
#[test]
fn http_artifact_lock_restarts_and_reinstalls_offline() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let backend = "http:https://downloads.example.test/tool-{version}";
    let bytes = b"#!/bin/sh\nprintf 'http-replay\n'\n";
    let digest =
        osdk_core::pipeline::verify::hash_bytes(bytes, osdk_core::pipeline::HashAlgo::Sha256);
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[tools.{backend:?}]\nversion = \"1.2.3\"\nsha256 = {digest:?}\nkind = \"file\"\nrename = \"fixture-http\"\n"
        ),
    )
    .unwrap();
    let mut version = osdk_core::version::ToolVersion::new(backend, "1.2.3");
    version.options = std::collections::BTreeMap::from([
        ("sha256".into(), digest.clone()),
        ("kind".into(), "file".into()),
        ("rename".into(), "fixture-http".into()),
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
            "https://downloads.example.test/tool-1.2.3".into(),
        ),
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
            "tool-1.2.3".into(),
        ),
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
            format!("sha256:{digest}"),
        ),
    ]);
    let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
        "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
        "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
        "OSDK_STORE_DIR" => Some(temp.path().join("store").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(temp.path().join("installs").display().to_string()),
        _ => None,
    })
    .unwrap();
    let locator = osdk_core::backend::http::HttpBackend::install_locator_for(
        &dirs,
        osdk_core::platform::Platform::current(),
        backend,
        &version,
    )
    .unwrap();
    let cached =
        osdk_core::pipeline::dynamic_artifact_cache_path(&dirs, &locator, "tool-1.2.3").unwrap();
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, bytes).unwrap();
    let trusted = project.to_string_lossy().into_owned();

    let install = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["--offline", "install"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let lock = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["--offline", "lock"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    assert!(std::fs::read_to_string(project.join("osdk.lock"))
        .unwrap()
        .contains("https://downloads.example.test/tool-1.2.3"));

    std::fs::remove_dir_all(locator.install_root()).unwrap();
    let reinstall = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["--offline", "install"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        reinstall.status.success(),
        "{}",
        String::from_utf8_lossy(&reinstall.stderr)
    );
    let executable = locator.install_root().join("bin/fixture-http");
    assert_eq!(
        Command::new(executable).output().unwrap().stdout,
        b"http-replay\n"
    );
}

#[test]
fn locked_evidence_is_not_trusted_without_cached_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let artifact_bytes = b"locked tool fixture";
    let checksum = osdk_core::pipeline::verify::hash_bytes(
        artifact_bytes,
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
        "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
        "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
        "OSDK_STORE_DIR" => Some(temp.path().join("store").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(temp.path().join("installs").display().to_string()),
        _ => None,
    })
    .unwrap();
    let mut version = osdk_core::version::ToolVersion::new("github:example/tool", "1.0.0");
    version.options.extend(std::collections::BTreeMap::from([
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
            "https://invalid.example/tool".into(),
        ),
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
            "tool".into(),
        ),
        (
            osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
            format!("sha256:{checksum}"),
        ),
    ]));
    let locator = osdk_core::backend::github::github_install_locator_for(
        &dirs,
        osdk_core::platform::Platform::current(),
        "github:example/tool",
        &version,
    )
    .unwrap();
    let artifact =
        osdk_core::pipeline::dynamic_artifact_cache_path(&dirs, &locator, "tool").unwrap();
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, artifact_bytes).unwrap();
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
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no cached bundle"),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
fn locked_npm_installs_independently_and_launcher_uses_managed_node() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"npm@11.5.2","engines":{"node":"20.0.0"}}"#,
    )
    .unwrap();

    let archive = temp.path().join("npm-11.5.2.tgz");
    {
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, contents) in [
            (
                "package/bin/npm-cli.js",
                b"process.stdout.write('npm-managed\\n');\n".as_slice(),
            ),
            (
                "package/bin/npx-cli.js",
                b"process.stdout.write('npx-managed\\n');\n".as_slice(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, contents).unwrap();
        }
        builder.finish().unwrap();
    }
    let checksum =
        osdk_core::pipeline::verify::hash_file(&archive, osdk_core::pipeline::HashAlgo::Sha256)
            .unwrap();
    let cached = temp
        .path()
        .join("cache/downloads/npm/11.5.2/npm-11.5.2.tgz");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::copy(&archive, &cached).unwrap();
    std::fs::write(
        project.join("osdk.lock"),
        format!(
            r#"schema = 1

[platforms.{platform}.tools.node]
request = "20.0.0"
version = "20.0.0"

[platforms.{platform}.tools.npm]
request = "11.5.2"
version = "11.5.2"

[platforms.{platform}.tools.npm.artifact]
url = "https://invalid.example/npm-11.5.2.tgz"
file_name = "npm-11.5.2.tgz"
checksum = "sha256:{checksum}"
"#,
            platform = platform_key(),
        ),
    )
    .unwrap();
    let node_bin = temp.path().join("installs/node/20.0.0/bin");
    std::fs::create_dir_all(&node_bin).unwrap();
    let node = node_bin.join("node");
    std::fs::write(
        &node,
        "#!/bin/sh\nprintf '%s\\n' \"$0\" > \"$OSDK_NODE_LOG\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(temp.path().join("installs/node/20.0.0/.osdk-complete"), b"").unwrap();

    let install = run_isolated_in(temp.path(), &project, &["--offline", "install"]);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let npm = temp.path().join("installs/npm/11.5.2/bin/npm");
    assert!(npm.is_file());
    assert!(std::fs::read_to_string(&npm).unwrap().contains("exec node"));

    let node_log = temp.path().join("node-used.log");
    let path_value = format!(
        "{}:{}",
        temp.path().join("installs/npm/11.5.2/bin").display(),
        node_bin.display()
    );
    let output = std::process::Command::new(&npm)
        .env_clear()
        .env("PATH", path_value)
        .env("OSDK_NODE_LOG", &node_log)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(node_log).unwrap().trim(),
        node.display().to_string()
    );

    let uninstall = run_isolated(temp.path(), &["--yes", "uninstall", "npm@11.5.2"]);
    assert!(uninstall.status.success());
    assert!(!temp.path().join("installs/npm/11.5.2").exists());
    assert!(temp.path().join("installs/node/20.0.0").exists());
}

#[test]
fn package_manager_field_auto_selects_exact_manager_and_node_in_lock() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"npm@11.5.2","engines":{"node":"20.0.0"}}"#,
    )
    .unwrap();
    let npm = temp.path().join("installs/npm/11.5.2");
    let node = temp.path().join("installs/node/20.0.0");
    for install in [&npm, &node] {
        std::fs::create_dir_all(install).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();
    }
    let output = run_isolated_in(temp.path(), &project, &["--offline", "lock"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lock.contains(".tools.npm]"));
    assert!(lock.contains("request = \"11.5.2\""));
    assert!(lock.contains(".tools.node]"));
    assert!(lock.contains("request = \"20.0.0\""));
}

#[test]
fn package_manager_current_and_invalid_field_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"pnpm@9.15.0"}"#,
    )
    .unwrap();
    let current = run_isolated_in(temp.path(), &project, &["current", "pnpm"]);
    assert!(current.status.success());
    let stdout = String::from_utf8(current.stdout).unwrap();
    assert!(stdout.contains("pnpm 9.15.0"));
    assert!(stdout.contains("package.json"));

    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"pnpm@https://example.test/pnpm.tgz"}"#,
    )
    .unwrap();
    let invalid = run_isolated_in(temp.path(), &project, &["lock"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("exact semver"));
}

#[test]
fn prerelease_channel_lock_preserves_request_and_exact_version_offline() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("osdk.lock"),
        format!(
            r#"schema = 1

[platforms.{platform}.tools.bun]
request = "canary"
version = "1.3.13-canary.20260425.1"
"#,
            platform = platform_key()
        ),
    )
    .unwrap();
    let install = temp.path().join("installs/bun/1.3.13-canary.20260425.1");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    let output = run_isolated_in(temp.path(), &project, &["--offline", "install"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lock.contains("request = \"canary\""));
    assert!(lock.contains("version = \"1.3.13-canary.20260425.1\""));
}

#[cfg(unix)]
#[test]
fn explicit_npm_exec_places_independent_manager_before_managed_node() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let npm_bin = temp.path().join("installs/npm/11.5.2/bin");
    let node_bin = temp.path().join("installs/node/20.0.0/bin");
    std::fs::create_dir_all(&npm_bin).unwrap();
    std::fs::create_dir_all(&node_bin).unwrap();
    let npm = npm_bin.join("npm");
    std::fs::write(
        &npm,
        "#!/bin/sh\nprintf '%s\n' \"$PATH\"\ncommand -v node\n",
    )
    .unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    let node = node_bin.join("node");
    std::fs::write(&node, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    for marker in [
        temp.path().join("installs/npm/11.5.2/.osdk-complete"),
        temp.path().join("installs/node/20.0.0/.osdk-complete"),
    ] {
        std::fs::write(marker, b"").unwrap();
    }

    let output = run_isolated(
        temp.path(),
        &[
            "--offline",
            "exec",
            "--tool",
            "npm@11.5.2",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let path = stdout
        .lines()
        .find(|line| line.starts_with(&npm_bin.display().to_string()))
        .unwrap();
    assert!(path.starts_with(&npm_bin.display().to_string()));
    assert!(stdout
        .lines()
        .any(|line| line == node.display().to_string()));
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

#[cfg(unix)]
#[test]
fn exec_uses_versioned_manager_native_caches_and_preserves_overrides() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let node_bin = temp.path().join("installs/node/20.0.0/bin");
    std::fs::create_dir_all(&node_bin).unwrap();
    std::fs::write(node_bin.join("node"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        node_bin.join("node"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(temp.path().join("installs/node/20.0.0/.osdk-complete"), b"").unwrap();

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
        let install = temp.path().join(format!("installs/{manager}/{version}"));
        let executable = install.join(relative_executable);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\n' \"{shell_value}\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();

        let request = format!("{manager}@{version}");
        let output = run_isolated(
            temp.path(),
            &[
                "--offline",
                "exec",
                "--tool",
                &request,
                "--tool",
                "node@20.0.0",
                "--",
                manager,
            ],
        );
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
                    temp.path()
                        .join("cache/pkg")
                        .join(part)
                        .display()
                        .to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == expected),
            "{manager}@{version} did not receive {expected}: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        let custom = format!("/custom/{manager}-{version}");
        let output = run_isolated_in_with_env(
            temp.path(),
            temp.path(),
            &[
                "--offline",
                "exec",
                "--tool",
                &request,
                "--tool",
                "node@20.0.0",
                "--",
                manager,
            ],
            &[(override_key, &custom)],
        );
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&custom),
            "{manager}@{version} did not preserve {override_key}: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        if manager == "pnpm" {
            let output = run_isolated_in_with_env(
                temp.path(),
                temp.path(),
                &[
                    "--offline",
                    "exec",
                    "--tool",
                    &request,
                    "--tool",
                    "node@20.0.0",
                    "--",
                    manager,
                ],
                &[("PNPM_HOME", "/custom/pnpm-home")],
            );
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("/custom/pnpm-home"));
        }
    }
}

#[cfg(unix)]
#[test]
fn node_only_exec_provides_cache_for_bundled_npm() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let node_bin = temp.path().join("installs/node/20.0.0/bin");
    std::fs::create_dir_all(&node_bin).unwrap();
    let npm = node_bin.join("npm");
    std::fs::write(
        &npm,
        "#!/bin/sh\nprintf '%s\n' \"${npm_config_cache-unset}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(temp.path().join("installs/node/20.0.0/.osdk-complete"), b"").unwrap();

    let output = run_isolated(
        temp.path(),
        &["--offline", "exec", "--tool", "node@20.0.0", "--", "npm"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected = temp.path().join("cache/pkg/npm").display().to_string();
    assert!(stdout.lines().any(|line| line == expected));
}

#[cfg(unix)]
fn write_fake_registry_manager(
    root: &Path,
    manager: &str,
    version: &str,
    alias: &str,
    script: &str,
) {
    use std::os::unix::fs::PermissionsExt;

    let relative = match manager {
        "npm" | "yarn" => format!("bin/{alias}"),
        "bun" => format!("bin/{alias}"),
        "pnpm" | "deno" => alias.to_string(),
        other => panic!("unsupported fixture manager {other}"),
    };
    let install = root.join(format!("installs/{manager}/{version}"));
    let executable = install.join(relative);
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, script).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(install.join(".osdk-complete"), b"").unwrap();

    if matches!(manager, "npm" | "pnpm" | "yarn") {
        let node_install = root.join("installs/node/20.0.0");
        let node = node_install.join("bin/node");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(node_install.join(".osdk-complete"), b"").unwrap();
    }
}

#[cfg(unix)]
fn write_fake_aube_helper(root: &Path) -> PathBuf {
    let helper = root.join("bin/osdk-aube-fixture");
    write_executable(
        &helper,
        r####"#!/bin/sh
set -eu
log="$NPM_CONFIG_CACHE_DIR/sidecar.log"
mkdir -p "$NPM_CONFIG_CACHE_DIR"
{
  printf 'args=%s\n' "$*"
  printf 'cwd=%s\n' "$PWD"
  printf 'HOME=%s\n' "$HOME"
  printf 'XDG_CONFIG_HOME=%s\n' "$XDG_CONFIG_HOME"
  printf 'XDG_DATA_HOME=%s\n' "$XDG_DATA_HOME"
  printf 'XDG_CACHE_HOME=%s\n' "$XDG_CACHE_HOME"
  printf 'NPM_CONFIG_USERCONFIG=%s\n' "$NPM_CONFIG_USERCONFIG"
  printf 'NPM_CONFIG_GLOBALCONFIG=%s\n' "$NPM_CONFIG_GLOBALCONFIG"
  printf 'NPM_CONFIG_GLOBAL_DIR=%s\n' "$NPM_CONFIG_GLOBAL_DIR"
  printf 'NPM_CONFIG_GLOBAL_BIN_DIR=%s\n' "$NPM_CONFIG_GLOBAL_BIN_DIR"
  printf 'NPM_CONFIG_STORE_DIR=%s\n' "$NPM_CONFIG_STORE_DIR"
  printf 'NPM_CONFIG_CACHE_DIR=%s\n' "$NPM_CONFIG_CACHE_DIR"
  printf 'NPM_CONFIG_NODE_VERSION=%s\n' "$NPM_CONFIG_NODE_VERSION"
  printf 'AUBE_RUNTIME_DIR=%s\n' "$AUBE_RUNTIME_DIR"
  printf 'AUBE_NO_UPDATE_CHECK=%s\n' "$AUBE_NO_UPDATE_CHECK"
  printf 'CI=%s\n' "${CI-unset}"
  printf 'PATH=%s\n' "$PATH"
} >> "$log"
pkg_root="$NPM_CONFIG_GLOBAL_DIR/global-aube"
install="$pkg_root/fixture-install"
mkdir -p "$install/node_modules/fixture-cli" "$NPM_CONFIG_GLOBAL_BIN_DIR"
printf '%s\n' '{"name":"aube-global","version":"0.0.0","private":true,"dependencies":{"fixture-cli":"1.2.3"}}' > "$install/package.json"
printf '%s\n' 'lockfileVersion: '\''9.0'\''' 'importers:' '  .:' '    dependencies:' '      fixture-cli:' '        specifier: 1.2.3' '        version: 1.2.3' 'packages:' '  fixture-cli@1.2.3:' '    resolution: {integrity: sha512-Zml4dHVyZQ==}' > "$install/aube-lock.yaml"
printf '%s\n' '{"name":"fixture-cli","version":"1.2.3","bin":{"fixture-cli":"cli.js"}}' > "$install/node_modules/fixture-cli/package.json"
printf '%s\n' '#!/bin/sh' 'echo "aube-fixture:$*"' > "$install/node_modules/fixture-cli/cli.js"
chmod +x "$install/node_modules/fixture-cli/cli.js"
ln -s "$install" "$pkg_root/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"####,
    );
    helper
}

#[cfg(not(windows))]
fn registry_fixture(requests: usize, healthy: bool) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..requests {
            let mut stream = accept_fixture_connection(&listener, "dependency registry fixture");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /-/ping "), "{request}");
            let lowercase = request.to_ascii_lowercase();
            assert!(!lowercase.contains("authorization:"), "{request}");
            assert!(!lowercase.contains("cookie:"), "{request}");
            if healthy {
                let body = r#"{}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
        }
    });
    (format!("http://{address}/"), server)
}

#[cfg(not(windows))]
fn unused_loopback_registry() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/")
}

#[cfg(not(windows))]
fn write_registry_config(root: &Path, urls: &[&str]) {
    let directory = root.join("config");
    std::fs::create_dir_all(&directory).unwrap();
    let urls = urls
        .iter()
        .map(|url| format!("\"{url}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        directory.join("config.toml"),
        format!("[registries.npm]\nurls = [{urls}]\nprobe_timeout_ms = 500\n"),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn global_aube_use_publishes_relocatable_install_and_reuses_it_offline() {
    let temporary = tempfile::tempdir().unwrap();
    let caller = temporary.path().join("caller");
    std::fs::create_dir_all(&caller).unwrap();
    let caller_manifest = caller.join("package.json");
    std::fs::write(&caller_manifest, r#"{"name":"untouched"}"#).unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    let (registry, registry_thread) = registry_fixture(1, true);
    std::fs::write(
        temporary.path().join("config/config.toml"),
        format!(
            "[sources]\nselection = \"ordered\"\n[registries.npm]\nurls = [{registry:?}]\nprobe_timeout_ms = 500\n[tools]\nnode = \"20.0.0\"\n"
        ),
    )
    .unwrap();
    write_executable(
        &temporary.path().join("installs/node/20.0.0/bin/node"),
        "#!/bin/sh\nexec /bin/sh \"$@\"\n",
    );
    std::fs::write(
        temporary.path().join("installs/node/20.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();
    let helper = write_fake_aube_helper(temporary.path());
    let helper_value = helper.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &caller,
        &[
            "use",
            "--global",
            "npm:fixture-cli@1.2.3",
            "-o",
            "installer=aube",
            "-o",
            "allow_builds=@Scope/Native,Plain-Native",
        ],
        &[
            ("OSDK_AUBE_BIN", helper_value.as_str()),
            ("PATH", "/usr/bin:/bin"),
        ],
    );
    assert!(
        output.status.success(),
        "root={}\nstdout={}\nstderr={}",
        temporary.path().display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    registry_thread.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(&caller_manifest).unwrap(),
        r#"{"name":"untouched"}"#
    );

    let log =
        std::fs::read_to_string(temporary.path().join("cache/aube/v1/cache/sidecar.log")).unwrap();
    assert_eq!(
        log.lines().filter(|line| line.starts_with("args=")).count(),
        1
    );
    let args = log.lines().find(|line| line.starts_with("args=")).unwrap();
    let expected_registry_arg = format!("--registry={registry}");
    for expected in [
        "add --global --save-exact --disable-gvs --config.nodeLinker=hoisted",
        "--allow-build=@scope/native",
        "--allow-build=plain-native",
        &expected_registry_arg,
        "fixture-cli@1.2.3",
    ] {
        assert!(args.contains(expected), "missing {expected} in {args}");
    }
    assert!(log.contains("CI=unset\n"), "{log}");
    assert!(log.contains("AUBE_NO_UPDATE_CHECK=1\n"), "{log}");
    assert!(log.lines().any(|line| {
        line.starts_with("HOME=")
            && line.contains(".osdk-stage-")
            && line.ends_with("/native-config/home")
    }));
    for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
        assert!(
            log.lines().any(|line| {
                line.starts_with(&format!("{key}=")) && line.contains(".osdk-stage-")
            }),
            "{log}"
        );
    }
    assert!(log.lines().any(|line| {
        line.starts_with("NPM_CONFIG_GLOBAL_DIR=")
            && line.contains(".osdk-stage-")
            && line.ends_with("/aube-global")
    }));
    assert!(log.lines().any(|line| {
        line.starts_with("NPM_CONFIG_GLOBAL_BIN_DIR=")
            && line.contains(".osdk-stage-")
            && line.ends_with("/bin")
    }));
    assert!(log.lines().any(|line| {
        line.starts_with("AUBE_RUNTIME_DIR=")
            && line.contains(".osdk-stage-")
            && line.ends_with("/aube-runtime-disabled")
    }));
    assert!(log.contains(&format!(
        "NPM_CONFIG_STORE_DIR={}\n",
        temporary.path().join("store/aube").display()
    )));
    assert!(log.contains(&format!(
        "NPM_CONFIG_CACHE_DIR={}\n",
        temporary.path().join("cache/aube/v1/cache").display()
    )));
    assert!(log.contains("NPM_CONFIG_NODE_VERSION=20.0.0\n"), "{log}");
    let path_line = log.lines().find(|line| line.starts_with("PATH=")).unwrap();
    assert!(path_line.contains(
        &temporary
            .path()
            .join("installs/node/20.0.0/bin")
            .display()
            .to_string()
    ));

    let inventory = osdk_core::inventory::scan_installs(
        &temporary.path().join("installs"),
        &osdk_core::inventory::ScanOptions::default(),
    )
    .unwrap();
    let matching = inventory
        .installs
        .into_iter()
        .filter(|install| {
            install.manifest.identity.tool == "npm:fixture-cli"
                && install.manifest.identity.version == "1.2.3"
                && install.manifest.identity.scope == osdk_core::tool::InstallScope::Global
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    let final_root = matching[0].install_root.clone();
    assert!(final_root.join("project/aube-lock.yaml").is_file());
    assert!(final_root
        .join("project/node_modules/fixture-cli/package.json")
        .is_file());
    assert!(final_root.join(".osdk-complete").is_file());
    assert!(!final_root.join("aube-global").exists());
    assert!(!final_root.join("native-config").exists());
    assert!(!final_root.join("aube-runtime-disabled").exists());
    let launcher = final_root.join("bin/fixture-cli");
    assert_eq!(
        std::fs::canonicalize(&launcher).unwrap(),
        std::fs::canonicalize(final_root.join("project/node_modules/fixture-cli/cli.js")).unwrap()
    );
    let executed = Command::new(&launcher).arg("works").output().unwrap();
    assert!(executed.status.success());
    assert_eq!(
        String::from_utf8(executed.stdout).unwrap(),
        "aube-fixture:works\n"
    );

    let location = run_isolated(
        temporary.path(),
        &["where", "--global", "npm:fixture-cli@1.2.3"],
    );
    assert!(
        location.status.success(),
        "{}",
        String::from_utf8_lossy(&location.stderr)
    );
    assert_eq!(
        String::from_utf8(location.stdout).unwrap().trim(),
        final_root.display().to_string()
    );
    let config = std::fs::read_to_string(temporary.path().join("config/config.toml")).unwrap();
    assert!(config.contains("installer = \"aube\""), "{config}");
    let lock = std::fs::read_to_string(temporary.path().join("config/osdk.lock")).unwrap();
    assert!(lock.contains("installer = \"aube\""), "{lock}");
    assert!(lock.contains("scope = \"global\""), "{lock}");

    let offline_helper_marker = temporary.path().join("offline-helper-ran");
    write_executable(
        &helper,
        &format!(
            "#!/bin/sh\nprintf ran > '{}'\nexit 79\n",
            offline_helper_marker.display()
        ),
    );
    let offline = run_isolated_in_with_env(
        temporary.path(),
        &caller,
        &[
            "--offline",
            "use",
            "--global",
            "npm:fixture-cli@1.2.3",
            "-o",
            "installer=aube",
        ],
        &[("OSDK_AUBE_BIN", helper_value.as_str())],
    );
    assert!(
        offline.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&offline.stdout),
        String::from_utf8_lossy(&offline.stderr)
    );
    assert!(
        !offline_helper_marker.exists(),
        "offline reuse unexpectedly launched Aube"
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("cache/aube/v1/cache/sidecar.log"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("args="))
            .count(),
        1
    );

    let uninstall = run_isolated(
        temporary.path(),
        &["--yes", "uninstall", "--global", "npm:fixture-cli@1.2.3"],
    );
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    assert!(!final_root.exists());
}

#[cfg(unix)]
#[test]
fn offline_global_aube_use_rejects_before_mutation_or_helper_spawn() {
    let temporary = tempfile::tempdir().unwrap();
    let marker = temporary.path().join("helper-ran");
    let helper = temporary.path().join("bin/osdk-aube-must-not-run");
    write_executable(
        &helper,
        &format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    );
    let helper_value = helper.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "--offline",
            "use",
            "--global",
            "npm:fixture-cli@1.2.3",
            "-o",
            "installer=aube",
        ],
        &[("OSDK_AUBE_BIN", helper_value.as_str())],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("global Aube installs are unavailable in offline mode"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists());
    assert!(!temporary.path().join("installs/npm-global").exists());
    assert!(!temporary.path().join("config/config.toml").exists());
    assert!(!temporary.path().join("config/osdk.lock").exists());
    assert!(!temporary.path().join("cache/aube").exists());
    assert!(!temporary.path().join("store/aube").exists());
}

#[cfg(unix)]
#[test]
fn authenticated_global_use_fails_before_public_metadata_probe_or_installer_spawn() {
    for installer in ["aube", "npm", "pnpm"] {
        for native_policy in ["auth-env", "private-npmrc"] {
            let temporary = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            std::fs::create_dir_all(temporary.path().join("config")).unwrap();
            std::fs::write(
                temporary.path().join("config/config.toml"),
                format!(
                    r#"[sources]
selection = "ordered"

[sources."npm:fixture-cli"]
disable = ["npmmirror", "npm"]

[[sources."npm:fixture-cli".custom]]
id = "public-fixture"
kind = "custom"
index_url = {endpoint:?}
download_url = {endpoint:?}

[registries.npm]
urls = [{endpoint:?}]
probe_timeout_ms = 500
"#
                ),
            )
            .unwrap();

            if native_policy == "private-npmrc" {
                std::fs::create_dir_all(temporary.path().join("home")).unwrap();
                std::fs::write(
                    temporary.path().join("home/.npmrc"),
                    "registry=https://packages.corp.invalid/\n",
                )
                .unwrap();
            }
            let marker = temporary
                .path()
                .join(format!("{installer}-{native_policy}-spawned"));
            let marker_value = marker.display().to_string();
            let mut environment = Vec::new();
            if native_policy == "auth-env" {
                environment.push((
                    "NPM_TOKEN".to_string(),
                    "must-not-appear-in-errors".to_string(),
                ));
            }
            if installer == "aube" {
                let helper = temporary.path().join("bin/osdk-aube-must-not-run");
                write_executable(
                    &helper,
                    &format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
                );
                let helper_value = helper.display().to_string();
                environment.push(("OSDK_AUBE_BIN".into(), helper_value));
            } else {
                write_fake_registry_manager(
                    temporary.path(),
                    installer,
                    "10.0.0",
                    installer,
                    "#!/bin/sh\nprintf ran > \"$OSDK_TEST_MARKER\"\n",
                );
                environment.push(("OSDK_TEST_MARKER".into(), marker_value));
            }
            let environment = environment
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            let installer_option = format!("installer={installer}");
            let output = run_isolated_in_with_env(
                temporary.path(),
                temporary.path(),
                &[
                    "use",
                    "--global",
                    "npm:fixture-cli",
                    "-o",
                    &installer_option,
                ],
                &environment,
            );

            assert!(
                !output.status.success(),
                "{installer} unexpectedly succeeded"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("cannot safely isolate global"),
                "{installer}: {stderr}"
            );
            let expected_reason = if native_policy == "auth-env" {
                "NPM_TOKEN"
            } else {
                "private or unknown native registry"
            };
            assert!(stderr.contains(expected_reason), "{installer}: {stderr}");
            assert!(
                !stderr.contains("must-not-appear-in-errors"),
                "{installer}: {stderr}"
            );
            assert!(
                listener.accept().is_err(),
                "{installer} contacted a public metadata or registry endpoint"
            );
            assert!(!marker.exists(), "{installer} spawned its installer/helper");
            assert!(!temporary.path().join("installs/npm-global").exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn project_npm_use_runs_managed_native_installer_once_and_publishes_metadata() {
    for (manager, lock_name, lock_contents, manifest, expected_args) in [
        (
            "npm",
            "package-lock.json",
            r#"{"lockfileVersion":3}"#,
            r#"{"packageManager":"npm@10.0.0","dependencies":{"fixture-cli":"^1"}}"#,
            "install --save-prod --ignore-scripts fixture-cli@1.2.3",
        ),
        (
            "pnpm",
            "pnpm-lock.yaml",
            "lockfileVersion: 9.0\n",
            r#"{"packageManager":"pnpm@10.0.0","devDependencies":{"fixture-cli":"^1"}}"#,
            "add -D --ignore-scripts fixture-cli@1.2.3",
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let nested = project.join("src");
        let project_bin = project.join("node_modules/.bin");
        std::fs::create_dir_all(&project_bin).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("package.json"), manifest).unwrap();
        std::fs::create_dir_all(temporary.path().join("config")).unwrap();
        std::fs::write(
            temporary.path().join("config/config.toml"),
            "[sources]\nselection = \"ordered\"\n[tools]\nnode = \"20.0.0\"\n",
        )
        .unwrap();

        let marker = temporary.path().join(format!("{manager}.calls"));
        let native_lock = project.join(lock_name);
        let installed_bin = project_bin.join("fixture-cli");
        let installed_package = project.join("node_modules/fixture-cli");
        std::fs::create_dir_all(&installed_package).unwrap();
        std::fs::write(
            installed_package.join("package.json"),
            r#"{"name":"fixture-cli","version":"1.2.3","bin":{"fixture-cli":"cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(installed_package.join("cli.js"), "fixture\n").unwrap();
        std::os::unix::fs::symlink(
            installed_package.join("cli.js"),
            project_bin.join("fixture-cli"),
        )
        .unwrap();
        write_fake_registry_manager(
            temporary.path(),
            manager,
            "10.0.0",
            manager,
            r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf 'args=%s\n' "$*" >> "$OSDK_TEST_MARKER"
printf 'path=%s\n' "$PATH" >> "$OSDK_TEST_MARKER"
printf '%s' "$OSDK_TEST_LOCK_CONTENTS" > "$OSDK_TEST_LOCK"
"#,
        );
        let marker_value = marker.display().to_string();
        let lock_value = native_lock.display().to_string();
        let bin_value = installed_bin.display().to_string();
        let installer = format!("installer={manager}");
        let registry_key = if manager == "pnpm" {
            "pnpm_config_registry"
        } else {
            "npm_config_registry"
        };
        let output = run_isolated_in_with_env(
            temporary.path(),
            &nested,
            &["use", "npm:fixture-cli@1.2.3", "-o", &installer],
            &[
                ("OSDK_TEST_MARKER", marker_value.as_str()),
                ("OSDK_TEST_LOCK", lock_value.as_str()),
                ("OSDK_TEST_LOCK_CONTENTS", lock_contents),
                ("OSDK_TEST_BIN", bin_value.as_str()),
                (registry_key, "https://registry.example.test/"),
            ],
        );
        assert!(
            output.status.success(),
            "{manager}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let calls = std::fs::read_to_string(&marker).unwrap();
        let lines = calls.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.iter().filter(|line| **line == "call").count(),
            1,
            "{manager} launched more than once: {calls}"
        );
        assert_eq!(lines[1], format!("args={expected_args}"));
        let manager_bin = if manager == "npm" {
            temporary.path().join("installs/npm/10.0.0/bin")
        } else {
            temporary.path().join("installs/pnpm/10.0.0")
        };
        let expected_path = std::env::join_paths([
            manager_bin,
            temporary.path().join("installs/node/20.0.0/bin"),
        ])
        .unwrap()
        .to_string_lossy()
        .into_owned();
        assert_eq!(lines[2], format!("path={expected_path}"));

        let config_path = project.join("osdk.toml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("node = \"20.0.0\""), "{config}");
        assert!(config.contains("\"npm:fixture-cli\" = {"), "{config}");
        assert!(config.contains("version = \"1.2.3\""), "{config}");
        assert!(
            config.contains(&format!("installer = \"{manager}\"")),
            "{config}"
        );
        assert!(
            osdk_core::trust::is_trusted(&temporary.path().join("config"), &config_path, None,)
                .unwrap()
        );

        let lock: toml::Value = std::fs::read_to_string(project.join("osdk.lock"))
            .unwrap()
            .parse()
            .unwrap();
        let tools = &lock["platforms"][platform_key()]["tools"];
        assert_eq!(tools["node"]["version"].as_str(), Some("20.0.0"));
        let npm = &tools["npm:fixture-cli"]["npm"];
        assert_eq!(npm["installer"].as_str(), Some(manager));
        assert_eq!(npm["scope"].as_str(), Some("project"));
        assert_eq!(npm["node_version"].as_str(), Some("20.0.0"));
        assert_eq!(npm["native_lock"]["kind"].as_str(), Some(manager));
        assert_eq!(npm["native_lock"]["sha256"].as_str().unwrap().len(), 64);
        let hook = run_isolated_in(temporary.path(), &nested, &["hook-env", "--shell", "bash"]);
        assert!(
            hook.status.success(),
            "{}",
            String::from_utf8_lossy(&hook.stderr)
        );
        let hook = String::from_utf8(hook.stdout).unwrap();
        assert!(hook.contains("/.osdk/npm-bin/generations/"), "{hook}");
        assert!(!hook.contains("/node_modules/.bin"), "{hook}");
        assert!(!temporary.path().join("installs/npm/fixture-cli").exists());
    }
}

#[cfg(unix)]
#[test]
fn project_npm_use_preserves_indirect_alias_key_while_curated_metadata_stays_canonical() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let nested = project.join("src");
    let project_bin = project.join("node_modules/.bin");
    std::fs::create_dir_all(&project_bin).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"npm@10.0.0","dependencies":{"fixture-cli":"^1"}}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.fixture\" = { version = \"npm:fixture-cli@1.0.0\", installer = \"npm\" }\n\"tool.fixture.same\" = \"npm:fixture-cli@1.2.3\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[sources]\nselection = \"ordered\"\n[tools]\nnode = \"20.0.0\"\n",
    )
    .unwrap();

    let marker = temporary.path().join("npm.calls");
    let native_lock = project.join("package-lock.json");
    let installed_package = project.join("node_modules/fixture-cli");
    std::fs::create_dir_all(&installed_package).unwrap();
    std::fs::write(
        installed_package.join("package.json"),
        r#"{"name":"fixture-cli","version":"1.2.3","bin":{"fixture-cli":"cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(installed_package.join("cli.js"), "fixture\n").unwrap();
    std::os::unix::fs::symlink(
        installed_package.join("cli.js"),
        project_bin.join("fixture-cli"),
    )
    .unwrap();
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf 'args=%s\n' "$*" >> "$OSDK_TEST_MARKER"
printf '%s' "$OSDK_TEST_LOCK_CONTENTS" > "$OSDK_TEST_LOCK"
"#,
    );
    let marker_value = marker.display().to_string();
    let lock_value = native_lock.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &nested,
        &["use", "tool.fixture@1.2.3"],
        &[
            ("OSDK_TEST_MARKER", marker_value.as_str()),
            ("OSDK_TEST_LOCK", lock_value.as_str()),
            ("OSDK_TEST_LOCK_CONTENTS", r#"{"lockfileVersion":3}"#),
            ("npm_config_registry", "https://registry.example.test/"),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = std::fs::read_to_string(project.join("osdk.toml")).unwrap();
    assert!(config.contains("\"tool.fixture\" = {"), "{config}");
    assert!(
        config.contains("version = \"npm:fixture-cli@1.2.3\""),
        "{config}"
    );
    assert!(
        config.contains("\"tool.fixture.same\" = \"npm:fixture-cli@1.2.3\""),
        "{config}"
    );
    assert!(!config.contains("\"npm:fixture-cli\" = {"), "{config}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("pinned tool.fixture@1.2.3"), "{stdout}");

    let hook = run_isolated_in(temporary.path(), &nested, &["hook-env", "--shell", "bash"]);
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hook = String::from_utf8(hook.stdout).unwrap();
    assert!(hook.contains("/.osdk/npm-bin/generations/"), "{hook}");

    let calls = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(
        calls.lines().filter(|line| *line == "call").count(),
        1,
        "{calls}"
    );
}

#[cfg(unix)]
#[test]
fn project_npm_use_rejects_conflicting_aliases_before_installer_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let nested = project.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"packageManager":"npm@10.0.0"}"#,
    )
    .unwrap();
    let config_path = project.join("osdk.toml");
    let original_config = "[tools]\n\"tool.alpha\" = { version = \"npm:fixture-cli@1.0.0\", installer = \"npm\" }\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n";
    std::fs::write(&config_path, original_config).unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[sources]\nselection = \"ordered\"\n[tools]\nnode = \"20.0.0\"\n",
    )
    .unwrap();

    let marker = temporary.path().join("npm.calls");
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        "#!/bin/sh\nprintf 'called\n' >> \"$OSDK_TEST_MARKER\"\n",
    );
    let marker_value = marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &nested,
        &["use", "tool.alpha@3.0.0"],
        &[
            ("OSDK_TEST_MARKER", marker_value.as_str()),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("npm:fixture-cli"), "{stderr}");
    assert!(stderr.contains("tool.alpha"), "{stderr}");
    assert!(stderr.contains("3.0.0"), "{stderr}");
    assert!(stderr.contains("tool.beta"), "{stderr}");
    assert!(stderr.contains("2.0.0"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        original_config
    );
    assert!(!marker.exists());
    assert!(!project.join("osdk.lock").exists());
    assert!(!project.join(".osdk/npm-bin/current.json").exists());
}

#[cfg(unix)]
#[test]
fn project_npm_use_restores_manifest_and_lock_after_bin_validation_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let nested = project.join("src");
    let project_bin = project.join("node_modules/.bin");
    let installed_package = project.join("node_modules/fixture-cli");
    std::fs::create_dir_all(&project_bin).unwrap();
    std::fs::create_dir_all(&installed_package).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let old_manifest = br#"{"packageManager":"npm@10.0.0","devDependencies":{"kept":"1"}}"#;
    let old_lock = br#"{"lockfileVersion":3,"old":true}"#;
    std::fs::write(project.join("package.json"), old_manifest).unwrap();
    std::fs::write(project.join("package-lock.json"), old_lock).unwrap();
    std::fs::write(
        installed_package.join("package.json"),
        r#"{"name":"fixture-cli","version":"1.2.3","bin":{"fixture-cli":"cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(installed_package.join("cli.js"), "fixture\n").unwrap();
    // A same-name opaque launcher is the post-install validation failure.
    std::fs::write(
        project_bin.join("fixture-cli"),
        "#!/bin/sh\necho hijacked\n",
    )
    .unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[tools]\nnode = \"20.0.0\"\n",
    )
    .unwrap();
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        r#"#!/bin/sh
printf '%s' '{"packageManager":"npm@10.0.0","devDependencies":{"fixture-cli":"1.2.3"}}' > "$OSDK_TEST_MANIFEST"
printf '%s' '{"lockfileVersion":3,"new":true}' > "$OSDK_TEST_LOCK"
"#,
    );
    let manifest_path = project.join("package.json").display().to_string();
    let lock_path = project.join("package-lock.json").display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &nested,
        &["use", "npm:fixture-cli@1.2.3", "-o", "installer=npm"],
        &[
            ("OSDK_TEST_MANIFEST", manifest_path.as_str()),
            ("OSDK_TEST_LOCK", lock_path.as_str()),
            ("npm_config_registry", "https://registry.example.test/"),
        ],
    );

    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(project.join("package.json")).unwrap(),
        old_manifest
    );
    assert_eq!(
        std::fs::read(project.join("package-lock.json")).unwrap(),
        old_lock
    );
    assert!(!project.join("osdk.lock").exists());
    assert!(!project.join("osdk.toml").exists());
    assert!(!osdk_core::backend::npm_package::project_bin_current_path(&project).exists());
}

#[cfg(unix)]
#[test]
fn project_npm_use_rejects_and_rolls_back_native_lock_owner_switch() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let installed_package = project.join("node_modules/fixture-cli");
    let project_bin = project.join("node_modules/.bin");
    std::fs::create_dir_all(&installed_package).unwrap();
    std::fs::create_dir_all(&project_bin).unwrap();
    let manifest = br#"{"packageManager":"npm@10.0.0","devDependencies":{"fixture-cli":"1.2.3"}}"#;
    let package_lock = br#"{"lockfileVersion":3,"old":true}"#;
    std::fs::write(project.join("package.json"), manifest).unwrap();
    std::fs::write(project.join("package-lock.json"), package_lock).unwrap();
    std::fs::write(
        installed_package.join("package.json"),
        r#"{"name":"fixture-cli","version":"1.2.3","bin":{"fixture-cli":"cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(installed_package.join("cli.js"), "fixture\n").unwrap();
    std::os::unix::fs::symlink(
        installed_package.join("cli.js"),
        project_bin.join("fixture-cli"),
    )
    .unwrap();
    std::fs::create_dir_all(temporary.path().join("config")).unwrap();
    std::fs::write(
        temporary.path().join("config/config.toml"),
        "[tools]\nnode = \"20.0.0\"\n",
    )
    .unwrap();
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        r#"#!/bin/sh
rm -f "$OSDK_TEST_OLD_LOCK"
printf 'lockfileVersion: 9.0\n' > "$OSDK_TEST_NEW_LOCK"
"#,
    );
    let old_lock = project.join("package-lock.json").display().to_string();
    let new_lock = project.join("pnpm-lock.yaml").display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["use", "npm:fixture-cli@1.2.3", "-o", "installer=npm"],
        &[
            ("OSDK_TEST_OLD_LOCK", old_lock.as_str()),
            ("OSDK_TEST_NEW_LOCK", new_lock.as_str()),
            ("npm_config_registry", "https://registry.example.test/"),
        ],
    );

    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(project.join("package.json")).unwrap(),
        manifest
    );
    assert_eq!(
        std::fs::read(project.join("package-lock.json")).unwrap(),
        package_lock
    );
    assert!(!project.join("pnpm-lock.yaml").exists());
    assert!(!project.join("osdk.lock").exists());
    assert!(!project.join("osdk.toml").exists());
}

#[cfg(unix)]
#[test]
fn exec_registry_fallback_injects_only_the_manager_variable_and_runs_once() {
    let temporary = tempfile::tempdir().unwrap();
    let cases = [
        ("npm", "10.0.0", "npm", "install", 0usize),
        ("pnpm", "10.0.0", "pnpm", "add", 1usize),
        ("yarn", "1.22.22", "yarn", "install", 2usize),
        ("yarn", "4.10.3", "yarnpkg", "up", 3usize),
        ("bun", "1.2.3", "bun", "install", 4usize),
        ("deno", "2.4.0", "deno", "add", 5usize),
    ];
    let (unavailable, failing_server) = registry_fixture(cases.len(), false);
    let (healthy, healthy_server) = registry_fixture(cases.len(), true);
    write_registry_config(temporary.path(), &[&unavailable, &healthy]);
    let script = r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf '%s|%s|%s|%s|%s|%s\n' "${npm_config_registry-unset}" "${pnpm_config_registry-unset}" "${YARN_REGISTRY-unset}" "${YARN_NPM_REGISTRY_SERVER-unset}" "${BUN_CONFIG_REGISTRY-unset}" "${NPM_CONFIG_REGISTRY-unset}"
"#;

    for (manager, version, alias, subcommand, selected_index) in cases {
        write_fake_registry_manager(temporary.path(), manager, version, alias, script);
        let marker = temporary.path().join(format!("{manager}-{version}.calls"));
        let marker_value = marker.display().to_string();
        let request = format!("{manager}@{version}");
        let mut arguments = vec!["exec", "--tool", request.as_str()];
        if matches!(manager, "npm" | "pnpm" | "yarn") {
            arguments.extend(["--tool", "node@20.0.0"]);
        }
        arguments.extend(["--", alias, subcommand]);
        if manager == "deno" {
            arguments.push("npm:fixture");
        }
        let output = run_isolated_in_with_env(
            temporary.path(),
            temporary.path(),
            &arguments,
            &[("OSDK_TEST_MARKER", &marker_value)],
        );
        assert!(
            output.status.success(),
            "{manager}@{version}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "call\n");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let registry_line = stdout
            .lines()
            .find(|line| line.matches('|').count() == 5)
            .unwrap_or_else(|| panic!("missing registry environment line: {stdout}"));
        let values = registry_line
            .split('|')
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 6);
        for (index, value) in values.iter().enumerate() {
            if index == selected_index {
                assert_eq!(value, &healthy, "{manager}@{version}");
            } else {
                assert_eq!(value, "unset", "{manager}@{version}");
            }
        }
    }
    failing_server.join().unwrap();
    healthy_server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn exec_launcher_aliases_use_managed_canonical_binaries_once() {
    use std::os::unix::fs::PermissionsExt;

    for (manager, version, alias, canonical, subcommand, registry_index) in [
        ("pnpm", "10.0.0", "pnpx", "pnpm", "dlx", 1usize),
        ("bun", "1.2.3", "bunx", "bun", "x", 4usize),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let (healthy, server) = registry_fixture(1, true);
        write_registry_config(temporary.path(), &[&healthy]);

        let managed_log = temporary.path().join("managed.calls");
        let managed_log_value = managed_log.display().to_string();
        write_fake_registry_manager(
            temporary.path(),
            manager,
            version,
            canonical,
            r#"#!/bin/sh
printf '%s|%s|%s|%s|%s|%s|%s\n' "${npm_config_registry-unset}" "${pnpm_config_registry-unset}" "${YARN_REGISTRY-unset}" "${YARN_NPM_REGISTRY_SERVER-unset}" "${BUN_CONFIG_REGISTRY-unset}" "${NPM_CONFIG_REGISTRY-unset}" "$*" >> "$OSDK_TEST_MARKER"
"#,
        );

        let global_log = temporary.path().join("global.calls");
        let global_bin = temporary.path().join("global-bin");
        std::fs::create_dir_all(&global_bin).unwrap();
        let global_alias = global_bin.join(alias);
        std::fs::write(
            &global_alias,
            format!(
                "#!/bin/sh\nprintf 'global\n' >> {}\nexit 88\n",
                global_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&global_alias, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = global_bin.display().to_string();
        let request = format!("{manager}@{version}");
        let mut arguments = vec!["exec", "--tool", request.as_str()];
        if manager == "pnpm" {
            arguments.extend(["--tool", "node@20.0.0"]);
        }
        arguments.extend(["--", alias, "fixture-package", "--registry", "child-value"]);

        let output = run_isolated_in_with_env(
            temporary.path(),
            temporary.path(),
            &arguments,
            &[("PATH", &path), ("OSDK_TEST_MARKER", &managed_log_value)],
        );

        assert!(
            output.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = std::fs::read_to_string(&managed_log).unwrap();
        let fields = line.trim_end().split('|').collect::<Vec<_>>();
        assert_eq!(fields.len(), 7, "{alias}: {line}");
        for (index, value) in fields[..6].iter().enumerate() {
            if index == registry_index {
                assert_eq!(*value, healthy, "{alias}");
            } else {
                assert_eq!(*value, "unset", "{alias}");
            }
        }
        assert_eq!(
            fields[6],
            format!("{subcommand} fixture-package --registry child-value"),
            "{alias}"
        );
        assert!(!global_log.exists(), "{alias} escaped to the user PATH");
        server.join().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn exec_registry_never_retries_a_failed_manager_command() {
    let temporary = tempfile::tempdir().unwrap();
    let (healthy, server) = registry_fixture(1, true);
    write_registry_config(temporary.path(), &[&healthy]);
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        "#!/bin/sh\nprintf 'call\n' >> \"$OSDK_TEST_MARKER\"\nexit 42\n",
    );
    let marker = temporary.path().join("calls");
    let marker_value = marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "exec",
            "--tool",
            "npm@10.0.0",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
            "install",
        ],
        &[("OSDK_TEST_MARKER", &marker_value)],
    );
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "call\n");
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn exec_registry_all_unavailable_starts_no_manager_process() {
    let temporary = tempfile::tempdir().unwrap();
    let (unavailable, server) = registry_fixture(1, false);
    write_registry_config(temporary.path(), &[&unavailable]);
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        "#!/bin/sh\nprintf 'call\n' >> \"$OSDK_TEST_MARKER\"\n",
    );
    let marker = temporary.path().join("calls");
    let marker_value = marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "exec",
            "--tool",
            "npm@10.0.0",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
            "install",
        ],
        &[("OSDK_TEST_MARKER", &marker_value)],
    );
    assert!(!output.status.success());
    assert!(!marker.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("command was not started"));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn exec_registry_respects_explicit_env_and_cli_registry_without_probing() {
    let temporary = tempfile::tempdir().unwrap();
    let dead = unused_loopback_registry();
    write_registry_config(temporary.path(), &[&dead]);
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf '%s\n' "${npm_config_registry-unset}"
"#,
    );

    let env_marker = temporary.path().join("env.calls");
    let env_marker_value = env_marker.display().to_string();
    let explicit = "https://private.example.test/";
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "exec",
            "--tool",
            "npm@10.0.0",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
            "install",
        ],
        &[
            ("OSDK_TEST_MARKER", &env_marker_value),
            ("npm_config_registry", explicit),
        ],
    );
    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(env_marker).unwrap(), "call\n");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == explicit),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let cli_marker = temporary.path().join("cli.calls");
    let cli_marker_value = cli_marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "exec",
            "--tool",
            "npm@10.0.0",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
            "install",
            "--registry",
            explicit,
        ],
        &[("OSDK_TEST_MARKER", &cli_marker_value)],
    );
    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(cli_marker).unwrap(), "call\n");
}

#[cfg(unix)]
#[test]
fn exec_registry_passes_through_non_fetch_commands_without_probing() {
    let temporary = tempfile::tempdir().unwrap();
    let dead = unused_loopback_registry();
    write_registry_config(temporary.path(), &[&dead]);
    write_fake_registry_manager(
        temporary.path(),
        "npm",
        "10.0.0",
        "npm",
        r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf '%s\n' "${npm_config_registry-unset}"
"#,
    );
    let marker = temporary.path().join("calls");
    let marker_value = marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "exec",
            "--tool",
            "npm@10.0.0",
            "--tool",
            "node@20.0.0",
            "--",
            "npm",
            "--version",
        ],
        &[("OSDK_TEST_MARKER", &marker_value)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "call\n");
    assert!(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == "unset"));
}

#[cfg(unix)]
#[test]
fn exec_registry_passes_through_when_yarn_major_is_unknown() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let dead = unused_loopback_registry();
    write_registry_config(temporary.path(), &[&dead]);
    let node_install = temporary.path().join("installs/node/20.0.0");
    let yarn = node_install.join("bin/yarn");
    std::fs::create_dir_all(yarn.parent().unwrap()).unwrap();
    std::fs::write(
        &yarn,
        r#"#!/bin/sh
printf 'call\n' >> "$OSDK_TEST_MARKER"
printf '%s|%s\n' "${YARN_REGISTRY-unset}" "${YARN_NPM_REGISTRY_SERVER-unset}"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&yarn, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(node_install.join(".osdk-complete"), b"").unwrap();
    let marker = temporary.path().join("calls");
    let marker_value = marker.display().to_string();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "--verbose",
            "exec",
            "--tool",
            "node@20.0.0",
            "--",
            "yarn",
            "install",
        ],
        &[("OSDK_TEST_MARKER", &marker_value)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "call\n");
    assert!(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == "unset|unset"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Yarn major is unknown"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn registry_test_reports_candidate_health_and_selection() {
    let temporary = tempfile::tempdir().unwrap();
    let (unavailable, failing_server) = registry_fixture(1, false);
    let (healthy, healthy_server) = registry_fixture(1, true);
    write_registry_config(temporary.path(), &[&unavailable, &healthy]);
    let output = run_isolated(temporary.path(), &["registry", "test", "npm"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    failing_server.join().unwrap();
    healthy_server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("npm:"), "{stdout}");
    assert!(stdout.contains("unavailable"), "{stdout}");
    assert!(stdout.contains("healthy"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("selected") && line.contains(&healthy)),
        "{stdout}"
    );
    assert!(stdout.contains("npm_config_registry"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn registry_test_without_manager_checks_every_supported_mode() {
    let temporary = tempfile::tempdir().unwrap();
    let (healthy, server) = registry_fixture(6, true);
    write_registry_config(temporary.path(), &[&healthy]);
    let output = run_isolated(temporary.path(), &["registry", "test"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    for manager in [
        "npm:",
        "pnpm:",
        "yarn-classic:",
        "yarn-berry:",
        "bun:",
        "deno:",
    ] {
        assert!(stdout.contains(manager), "missing {manager} in {stdout}");
    }
}

#[test]
fn registry_help_is_localized() {
    let temporary = tempfile::tempdir().unwrap();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["registry", "test", "--help"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("探测项目依赖 Registry"), "{stdout}");
    assert!(stdout.contains("要测试的包管理器"), "{stdout}");
    assert!(!stdout.contains("help.registry"), "{stdout}");
}

#[test]
fn native_container_help_is_localized() {
    let temporary = tempfile::tempdir().unwrap();
    let pull = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["container", "pull", "--help"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(pull.status.success());
    let pull = String::from_utf8(pull.stdout).unwrap();
    for expected in [
        "原生运行时拉取一次镜像",
        "containerd 守护进程地址",
        "namespace",
    ] {
        assert!(pull.contains(expected), "{pull}");
    }

    let prune = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["container", "prune", "--help"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(prune.status.success());
    let prune = String::from_utf8(prune.stdout).unwrap();
    for expected in ["窄范围原生清理", "精确 sha256 预览 ID", "Docker context"] {
        assert!(prune.contains(expected), "{prune}");
    }
}

#[cfg(unix)]
#[test]
fn reshim_keeps_same_dynamic_backend_across_multiple_installed_versions() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\n\"tool.ni\" = \"npm:@antfu/ni@1.1.0\"\nnode = \"1.0.0\"\n",
    )
    .unwrap();

    let shim_bin_dir = temporary.path().join("bin");
    write_executable(
        &shim_bin_dir.join("osdk-shim"),
        "#!/bin/sh\nprintf 'shim placeholder\\n'\n",
    );
    let data_bin = temporary.path().join("data/bin");
    std::fs::create_dir_all(&data_bin).unwrap();
    std::os::unix::fs::symlink(shim_bin_dir.join("osdk-shim"), data_bin.join("osdk-shim")).unwrap();

    for version in ["1.0.0", "1.1.0"] {
        let options = std::collections::BTreeMap::from([
            ("__osdk_node_version".into(), "1.0.0".into()),
            ("__osdk_npm_scope".into(), "project".into()),
        ]);
        let identity = osdk_core::tool::InstallIdentity::new(
            "npm:@antfu/ni",
            version,
            osdk_core::platform::Platform::current().to_string(),
            osdk_core::tool::InstallScope::Isolated,
            &options,
            vec![osdk_core::tool::InstallDependency {
                kind: osdk_core::tool::InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "1.0.0".into(),
                identity: None,
            }],
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(temporary.path().join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        let install_root = osdk_core::dirs::InstallLocator::new(&dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let project_root = install_root.join("project");
        let package_root = project_root.join("node_modules/@antfu/ni");
        let target = package_root.join("bin/ni.js");
        write_executable(&target, "#!/bin/sh\nexit 0\n");
        std::fs::write(
            package_root.join("package.json"),
            format!(r#"{{"name":"@antfu/ni","version":"{version}","bin":{{"ni":"bin/ni.js"}}}}"#),
        )
        .unwrap();
        std::fs::write(
            project_root.join("package.json"),
            format!(
                r#"{{"name":"osdk-dynamic-npm-tool","private":true,"dependencies":{{"@antfu/ni":"{version}"}}}}"#
            ),
        )
        .unwrap();
        let lockfile = format!(
            "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies:\n      '@antfu/ni':\n        specifier: {version}\n        version: {version}\n\npackages:\n  '@antfu/ni@{version}':\n    resolution: {{integrity: sha512-fixture-integrity}}\n"
        );
        std::fs::write(project_root.join("aube-lock.yaml"), &lockfile).unwrap();
        let launcher = project_root.join("node_modules/.bin/ni");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &launcher).unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "ni".into(),
            path: "project/node_modules/.bin/ni".into(),
        }];
        manifest.write_atomic(&install_root).unwrap();
        std::fs::write(
            osdk_core::backend::npm_package::npm_receipt_path(&install_root),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": 1,
                "provider": "npm-package",
                "package": "@antfu/ni",
                "installer": "aube",
                "node_version": "1.0.0",
                "build_policy": "deny",
                "graph_sha256": osdk_core::pipeline::verify::hash_bytes(
                    lockfile.as_bytes(),
                    osdk_core::pipeline::HashAlgo::Sha256,
                ),
                "root_integrity": "sha512-fixture-integrity",
                "root_source": format!("npm:@antfu/ni@{version}")
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(install_root.join(".osdk-complete"), b"").unwrap();
    }
    let node_install = temporary.path().join("installs/node/1.0.0/bin/node");
    write_executable(&node_install, "#!/bin/sh\nexit 0\n");
    std::fs::write(
        temporary.path().join("installs/node/1.0.0/.osdk-complete"),
        b"",
    )
    .unwrap();

    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["reshim"],
        &[
            ("PATH", shim_bin_dir.to_str().unwrap()),
            ("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shim_path = temporary.path().join("data/shims/ni");
    assert!(
        shim_path.exists(),
        "{}\nshims: {:?}\nstdout:\n{}\nstderr:\n{}",
        shim_path.display(),
        std::fs::read_dir(temporary.path().join("data/shims")).map(|entries| entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>()),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn native_container_pull_preserves_foreground_output_and_exit_code() {
    let temporary = tempfile::tempdir().unwrap();
    let bin = temporary.path().join("native-bin");
    let calls = temporary.path().join("pull.calls");
    write_executable(
        &bin.join("docker"),
        "#!/bin/sh\nprintf '%s\n' \"$@\" > \"$OSDK_NATIVE_CALLS\"\nprintf 'native pull stdout\n'\nprintf 'native pull stderr\n' >&2\nexit 23\n",
    );

    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "container",
            "pull",
            "ubuntu:24.04",
            "--runtime",
            "docker",
            "--platform",
            "Linux/X64",
        ],
        &[
            ("PATH", bin.to_str().unwrap()),
            ("OSDK_NATIVE_CALLS", calls.to_str().unwrap()),
        ],
    );

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "native pull stdout\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "native pull stderr\n"
    );
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "image\npull\n--platform\nlinux/amd64\ndocker.io/library/ubuntu:24.04\n"
    );
}

#[cfg(unix)]
#[test]
fn explicit_containerd_pull_requires_and_forwards_target_selectors() {
    let temporary = tempfile::tempdir().unwrap();
    let missing = run_isolated(
        temporary.path(),
        &["container", "pull", "alpine:3", "--runtime", "containerd"],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--address"));

    let bin = temporary.path().join("native-bin");
    let calls = temporary.path().join("ctr.calls");
    write_executable(
        &bin.join("ctr"),
        "#!/bin/sh\nprintf '%s\n' \"$@\" > \"$OSDK_NATIVE_CALLS\"\nexit 0\n",
    );
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "container",
            "pull",
            "alpine:3",
            "--runtime",
            "containerd",
            "--address",
            "unix:///run/private/containerd.sock",
            "--namespace",
            "k8s.io",
        ],
        &[
            ("PATH", bin.to_str().unwrap()),
            ("OSDK_NATIVE_CALLS", calls.to_str().unwrap()),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "--address\nunix:///run/private/containerd.sock\n--namespace\nk8s.io\nimages\npull\ndocker.io/library/alpine:3\n"
    );
}

#[cfg(unix)]
#[test]
fn native_container_prune_previews_then_requires_exact_id_and_preserves_exit_code() {
    let temporary = tempfile::tempdir().unwrap();
    let bin = temporary.path().join("native-bin");
    let calls = temporary.path().join("prune.calls");
    write_executable(
        &bin.join("docker"),
        "#!/bin/sh\nif [ \"$1\" = context ] && [ \"$2\" = inspect ]; then\n  printf '[{\"Name\":\"team-context\",\"Endpoints\":{\"docker\":{\"Host\":\"unix:///var/run/docker.sock\"}}}]\n'\n  exit 0\nfi\nprintf '%s\n' \"$@\" > \"$OSDK_NATIVE_CALLS\"\nprintf 'native prune stdout\n'\nprintf 'native prune stderr\n' >&2\nexit 37\n",
    );
    let common_env = [
        ("PATH", bin.to_str().unwrap()),
        ("OSDK_NATIVE_CALLS", calls.to_str().unwrap()),
    ];
    let preview = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "container",
            "prune",
            "--runtime",
            "docker",
            "--scope",
            "images",
            "--context",
            "team-context",
        ],
        &common_env,
    );
    assert!(preview.status.success());
    assert!(!calls.exists());
    let preview_stdout = String::from_utf8(preview.stdout).unwrap();
    let preview_id = preview_stdout
        .split_whitespace()
        .find(|value| value.starts_with("sha256:"))
        .expect("preview id");

    let mismatch = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "--yes",
            "container",
            "prune",
            "--runtime",
            "docker",
            "--scope",
            "images",
            "--context",
            "team-context",
            "--execute",
            "--accept-preview",
            "sha256:wrong",
        ],
        &common_env,
    );
    assert!(!mismatch.status.success());
    assert!(!calls.exists());

    let executed = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &[
            "--yes",
            "container",
            "prune",
            "--runtime",
            "docker",
            "--scope",
            "images",
            "--context",
            "team-context",
            "--execute",
            "--accept-preview",
            preview_id,
        ],
        &common_env,
    );
    assert_eq!(executed.status.code(), Some(37));
    let stdout = String::from_utf8(executed.stdout).unwrap();
    assert!(stdout.contains(preview_id), "{stdout}");
    assert!(stdout.contains("native prune stdout"), "{stdout}");
    assert_eq!(
        String::from_utf8(executed.stderr).unwrap(),
        "native prune stderr\n"
    );
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "--host\nunix:///var/run/docker.sock\nimage\nprune\n--force\n"
    );
}
