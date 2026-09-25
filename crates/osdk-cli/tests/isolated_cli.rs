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
                // Accepted sockets may inherit the listener's nonblocking mode
                // on macOS and Windows. Switch back to blocking I/O before the
                // fixture reads a complete HTTP request.
                stream.set_nonblocking(false).unwrap();
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

/// `sources.probe_timeout_ms` is reachable through `osdk config` end to end.
///
/// It lives in the `[sources]` table, not `[settings]`, so three read paths had
/// to learn it; a whitelist entry alone would have written it and then answered
/// `unknown setting` on read. The global scope is used deliberately: a project
/// `[sources]` table is gated by trust, while the user-global file never is, so
/// this exercises the round trip without a trust decision entering the test.
#[test]
fn sources_probe_timeout_round_trips_through_config_commands() {
    let temp = tempfile::tempdir().unwrap();

    let default = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.probe_timeout_ms"],
    );
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    assert_eq!(String::from_utf8(default.stdout).unwrap().trim(), "1500");
    let model_default = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.model_probe_timeout_ms"],
    );
    assert!(model_default.status.success(), "{model_default:?}");
    assert_eq!(
        String::from_utf8(model_default.stdout).unwrap().trim(),
        "8000"
    );

    let attempts_default = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.model_download_attempts"],
    );
    assert_eq!(
        String::from_utf8(attempts_default.stdout).unwrap().trim(),
        "6"
    );
    let retry_base_default = run_isolated(
        temp.path(),
        &[
            "config",
            "get",
            "-g",
            "sources.model_download_retry_base_ms",
        ],
    );
    assert_eq!(
        String::from_utf8(retry_base_default.stdout).unwrap().trim(),
        "1000"
    );
    let attempts_set = run_isolated(
        temp.path(),
        &[
            "config",
            "set",
            "-g",
            "sources.model_download_attempts",
            "8",
        ],
    );
    assert!(attempts_set.status.success(), "{attempts_set:?}");
    let attempts_read = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.model_download_attempts"],
    );
    assert_eq!(String::from_utf8(attempts_read.stdout).unwrap().trim(), "8");

    // model_jobs bounds how many models `model sync` downloads at once; default 2,
    // settable, and rejected at zero like the other concurrency knob.
    let model_jobs_default =
        run_isolated(temp.path(), &["config", "get", "-g", "sources.model_jobs"]);
    assert_eq!(
        String::from_utf8(model_jobs_default.stdout).unwrap().trim(),
        "2"
    );
    let model_jobs_set = run_isolated(
        temp.path(),
        &["config", "set", "-g", "sources.model_jobs", "3"],
    );
    assert!(model_jobs_set.status.success(), "{model_jobs_set:?}");
    let model_jobs_read = run_isolated(temp.path(), &["config", "get", "-g", "sources.model_jobs"]);
    assert_eq!(
        String::from_utf8(model_jobs_read.stdout).unwrap().trim(),
        "3"
    );
    let model_jobs_zero = run_isolated(
        temp.path(),
        &["config", "set", "-g", "sources.model_jobs", "0"],
    );
    assert!(
        !model_jobs_zero.status.success(),
        "zero model_jobs stalls sync"
    );

    let model_set = run_isolated(
        temp.path(),
        &[
            "config",
            "set",
            "-g",
            "sources.model_probe_timeout_ms",
            "12000",
        ],
    );
    assert!(model_set.status.success(), "{model_set:?}");
    let model_read = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.model_probe_timeout_ms"],
    );
    assert_eq!(
        String::from_utf8(model_read.stdout).unwrap().trim(),
        "12000"
    );

    let set = run_isolated(
        temp.path(),
        &["config", "set", "-g", "sources.probe_timeout_ms", "4000"],
    );
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );

    // Read the artifact, not the echoed value: a write that lands somewhere the
    // loader ignores would still print success. The user-global file is
    // `<config dir>/config.toml` (Dirs::user_config_file).
    let user_config = temp.path().join("config").join("config.toml");
    let body = std::fs::read_to_string(&user_config).unwrap_or_else(|error| {
        panic!("missing user config at {}: {error}", user_config.display())
    });
    assert!(body.contains("[sources]"), "config body was: {body}");
    assert!(
        body.contains("probe_timeout_ms = 4000"),
        "config body was: {body}"
    );

    let read = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.probe_timeout_ms"],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(String::from_utf8(read.stdout).unwrap().trim(), "4000");

    // Validation: PositiveInt must reject zero.
    let zero = run_isolated(
        temp.path(),
        &["config", "set", "-g", "sources.probe_timeout_ms", "0"],
    );
    assert!(!zero.status.success());
    assert!(
        String::from_utf8_lossy(&zero.stderr).contains("at least 1"),
        "{}",
        String::from_utf8_lossy(&zero.stderr)
    );

    // And unset restores the default.
    let unset = run_isolated(
        temp.path(),
        &["config", "unset", "-g", "sources.probe_timeout_ms"],
    );
    assert!(
        unset.status.success(),
        "{}",
        String::from_utf8_lossy(&unset.stderr)
    );
    let back = run_isolated(
        temp.path(),
        &["config", "get", "-g", "sources.probe_timeout_ms"],
    );
    assert_eq!(String::from_utf8(back.stdout).unwrap().trim(), "1500");
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

#[test]
fn model_pull_without_reference_requires_an_applicable_declaration() {
    let temporary = tempfile::tempdir().unwrap();
    let output = run_isolated(temporary.path(), &["model", "pull", "fixture"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs a reference or an applicable [models.fixture] declaration"),
        "{stderr}"
    );
}

#[test]
fn model_sync_dry_run_bootstraps_an_empty_lock_from_declarations() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("osdk.toml"),
        "[models.fixture]\nsource = \"hf:owner/repo@main\"\ninclude = [\"*.safetensors\"]\n",
    )
    .unwrap();

    let output = run_isolated(temporary.path(), &["model", "sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would pull fixture (hf:owner/repo@main) from project declaration"),
        "{stdout}"
    );
    assert!(!temporary.path().join("osdk.lock").exists());
}

// The regression this fixes: bootstrapping used to trigger only when the whole
// model lock was empty, so a `[models]` entry added by hand to a project that
// already had a locked model was silently ignored. With a non-empty lock, a
// newly declared model must still be recognized and (dry-run) reported as a
// pull. Dry run keeps this cross-platform -- no fixture server needed.
#[test]
fn model_sync_dry_run_pulls_a_declaration_added_to_a_non_empty_lock() {
    let temporary = tempfile::tempdir().unwrap();
    // A lock that already describes one model (no local snapshot behind it).
    std::fs::write(
        temporary.path().join("osdk.lock"),
        "schema = 2\n\n[models.existing]\nprovider = \"huggingface\"\n\
         repository = \"owner/existing\"\nrequested_revision = \"main\"\n\
         revision = \"abc123\"\nendpoint = \"https://huggingface.co\"\nfiles = []\n",
    )
    .unwrap();
    // Config declares the existing model plus a brand-new one.
    std::fs::write(
        temporary.path().join("osdk.toml"),
        "[models.existing]\nsource = \"hf:owner/existing@main\"\n\n\
         [models.added]\nsource = \"hf:owner/added@main\"\n",
    )
    .unwrap();

    let output = run_isolated(temporary.path(), &["model", "sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The newly declared model is picked up despite the lock being non-empty.
    assert!(
        stdout.contains("would pull added (hf:owner/added@main) from project declaration"),
        "{stdout}"
    );
    // The already-locked, unchanged declaration is not reported as a config
    // bootstrap; it belongs to the replay pass instead.
    assert!(
        !stdout.contains("existing (hf:owner/existing@main) from project declaration"),
        "{stdout}"
    );
}

// A declaration whose identity changed against the lock (edited `source`) must
// be reported as a re-lock, not treated as up to date.
#[test]
fn model_sync_dry_run_relocks_a_changed_declaration() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("osdk.lock"),
        "schema = 2\n\n[models.fixture]\nprovider = \"huggingface\"\n\
         repository = \"owner/repo\"\nrequested_revision = \"main\"\n\
         revision = \"abc123\"\nendpoint = \"https://huggingface.co\"\nfiles = []\n",
    )
    .unwrap();
    // Same name, different requested revision than the lock records.
    std::fs::write(
        temporary.path().join("osdk.toml"),
        "[models.fixture]\nsource = \"hf:owner/repo@dev\"\n",
    )
    .unwrap();

    let output = run_isolated(temporary.path(), &["model", "sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would re-lock fixture (hf:owner/repo@dev) from declaration changed"),
        "{stdout}"
    );
}

// Several declared models are all collected for the (concurrent) pull pass, not
// just the first. Dry run keeps this cross-platform; concurrency itself is
// bounded by `sources.model_jobs` and exercised by the pull path's own tests.
#[test]
fn model_sync_dry_run_reports_every_declared_model() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("osdk.toml"),
        "[models.first]\nsource = \"hf:owner/first@main\"\n\n\
         [models.second]\nsource = \"hf:owner/second@main\"\n\n\
         [models.third]\nsource = \"ms:owner/third@master\"\n",
    )
    .unwrap();

    let output = run_isolated(temporary.path(), &["model", "sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would pull first (hf:owner/first@main) from project declaration"),
        "{stdout}"
    );
    assert!(
        stdout.contains("would pull second (hf:owner/second@main) from project declaration"),
        "{stdout}"
    );
    assert!(
        stdout.contains("would pull third (ms:owner/third@master) from project declaration"),
        "{stdout}"
    );
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
    std::fs::write(
        temporary.path().join("osdk.toml"),
        format!(
            "[models.fixture]\nsource = \"hf:owner/repo@main\"\nendpoint = \"{endpoint}\"\ninclude = [\"config.json\"]\nvariant = \"declared\"\n"
        ),
    )
    .unwrap();
    let trusted = temporary.path().to_string_lossy().into_owned();
    let output = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["model", "pull", "fixture"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let path = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["model", "path", "fixture"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        path.status.success(),
        "{}",
        String::from_utf8_lossy(&path.stderr)
    );
    let snapshot = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
    assert_eq!(
        std::fs::read(snapshot.join("config.json")).unwrap(),
        payload
    );
    let lock = std::fs::read_to_string(temporary.path().join("osdk.lock")).unwrap();
    assert!(lock.contains("[models.fixture]"));
    assert!(lock.contains("revision = \"abc123\""));
    assert!(lock.contains("variant = \"declared\""));
    assert!(lock.contains("sha256 ="));
}

// End-to-end declarative flow (Unix, needs the fixture server): a project with
// a `[models.<name>.views]` declaration pulls, and the pull both records the
// views in the lock and renders the consumer view without a separate
// `model view add`. This is the config -> lock -> view chain, asserted by
// reading the produced files (not exit codes).
#[cfg(not(windows))]
#[test]
fn declared_model_pull_records_views_in_lock_and_renders_view() {
    let payload = br#"{"model":"fixture"}"#.to_vec();
    let digest =
        osdk_core::pipeline::verify::hash_bytes(&payload, osdk_core::pipeline::HashAlgo::Sha256);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server_payload = payload.clone();
    let server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let mut stream = accept_fixture_connection(&listener, "HF declared-views fixture");
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
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let endpoint = format!("http://{address}");
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[models.fixture]\nsource = \"hf:owner/repo@main\"\nendpoint = \"{endpoint}\"\n\
             [models.fixture.views.comfyui]\n\
             map = {{ \"config.json\" = \"configs\" }}\n"
        ),
    )
    .unwrap();
    let trusted = project.to_string_lossy().into_owned();
    let output = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["model", "sync"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();

    // 1) The lock carries the declared view.
    let lock = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(lock.contains("[models.fixture.views.comfyui]"), "{lock}");
    assert!(lock.contains("profile = \"default\""), "{lock}");
    assert!(lock.contains("\"config.json\" = \"configs\""), "{lock}");

    // 2) The view was rendered: read the actual placed file through the view.
    let view_list = run_isolated_in_with_env(
        temporary.path(),
        &project,
        &["model", "view", "path", "comfyui"],
        &[("OSDK_TRUSTED_CONFIG_PATHS", &trusted)],
    );
    assert!(
        view_list.status.success(),
        "{}",
        String::from_utf8_lossy(&view_list.stderr)
    );
    let view_root = PathBuf::from(String::from_utf8(view_list.stdout).unwrap().trim());
    let placed = view_root.join("configs").join("config.json");
    assert!(
        placed.is_file(),
        "view file missing at {}",
        placed.display()
    );
    assert_eq!(std::fs::read(&placed).unwrap(), payload);

    // 3) The state file records the membership (so `view list` shows it).
    let state =
        std::fs::read_to_string(temporary.path().join("data/views/.osdk-views.json")).unwrap();
    assert!(state.contains("fixture"), "{state}");
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
                    .contains("range: bytes=0-65535"));
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

/// Read-only commands skip the trust *gate*, not the project *config*.
///
/// `bypasses_trust_check` routed `task list` through a loader that reads only
/// user-global configuration, so the command whose entire job is to print what
/// the project declares reported "no tasks defined" for every project -- this
/// repository included, whose `[tasks]` table is the documented answer to "what
/// does CI run". Nothing failed and nothing warned: exit code 0, and output
/// that reads as a legitimate "there are none".
///
/// `osdk run <task>` stays gated and therefore used the full loader, which is
/// how a task could be runnable while being unlistable -- two code paths
/// disagreeing about whether the same table exists.
#[test]
fn read_only_commands_see_the_project_config_they_report_on() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // Deliberately free of trust-requiring keys: this must work with no trust
    // record at all, which is the point of the read-only exemption.
    std::fs::write(
        project.join("osdk.toml"),
        "[tasks]\nlisted-task = \"echo hi\"\n",
    )
    .unwrap();

    let listed = run_isolated_in(temp.path(), &project, &["task", "list"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("listed-task"),
        "`task list` must print the project's task; got: {stdout}"
    );

    // `task info` reads the same set and was empty for the same reason.
    let info = run_isolated_in(temp.path(), &project, &["task", "info", "listed-task"]);
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    assert!(
        String::from_utf8_lossy(&info.stdout).contains("echo hi"),
        "`task info` must resolve the project's task"
    );
}

/// The exemption must not become a silent widening of trust.
///
/// Loading the project config for read-only commands is only safe while the
/// commands that *act* on it stay refused. Without this, the fix above would be
/// indistinguishable from "untrusted configs are now honoured everywhere".
#[test]
fn loading_project_config_for_read_only_commands_does_not_unlock_the_gate() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // `[sources]` requires trust (WeakensVerification); `[tasks]` gives the run
    // path something to attempt.
    std::fs::write(
        project.join("osdk.toml"),
        "[sources]\nselection = \"ordered\"\n[tasks]\nlisted-task = \"echo hi\"\n",
    )
    .unwrap();

    // Read-only still works despite the untrusted, trust-requiring table.
    let listed = run_isolated_in(temp.path(), &project, &["task", "list"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(String::from_utf8_lossy(&listed.stdout).contains("listed-task"));

    // ...and the command that would execute it is still refused.
    let refused = run_isolated_in(temp.path(), &project, &["run", "listed-task"]);
    assert!(
        !refused.status.success(),
        "`run` must stay gated on an untrusted config; stdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("is not trusted"), "{message}");

    // A command that installs is likewise still refused.
    let install = run_isolated_in(temp.path(), &project, &["install"]);
    assert!(
        !install.status.success(),
        "`install` must stay gated on an untrusted config"
    );
    assert!(
        String::from_utf8_lossy(&install.stderr).contains("is not trusted"),
        "`install` must be refused for the trust reason, not some other error"
    );
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

    // Bumping a tool version is not a governed change: `[tools]` grants no
    // execution on its own, so an existing approval must survive it. This is
    // the papercut the key-level gate exists to remove -- under the old
    // whole-file hash this demanded re-approval.
    std::fs::write(
        &config,
        "[tools]\nnode = \"22\"\n[sources]\nselection = \"ordered\"\n",
    )
    .unwrap();
    let bumped = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(
        bumped.status.success(),
        "bumping a tool version must not invalidate trust: {}",
        String::from_utf8_lossy(&bumped.stderr)
    );

    // Editing the governed `[sources]` table does invalidate it, and the
    // refusal must name the key so the user knows what to review.
    std::fs::write(
        &config,
        "[tools]\nnode = \"22\"\n[sources]\nselection = \"auto\"\n",
    )
    .unwrap();
    let changed = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(!changed.status.success());
    let message = String::from_utf8_lossy(&changed.stderr);
    assert!(message.contains("is not trusted"), "{message}");
    assert!(message.contains("sources"), "{message}");

    let config_value = config.to_string_lossy().into_owned();
    let retrusted = run_isolated_in(temp.path(), &project, &["--yes", "trust", &config_value]);
    assert!(retrusted.status.success());
    let listed = run_isolated_in(temp.path(), &project, &["trust", "list"]);
    assert!(listed.status.success());
    // The label is `active`, not `trusted`: `list` now distinguishes a record
    // that still applies from one whose file changed or went missing.
    assert!(String::from_utf8_lossy(&listed.stdout).contains("active"));

    let removed = run_isolated_in(temp.path(), &project, &["untrust"]);
    assert!(removed.status.success());
    let rejected_again = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(!rejected_again.status.success());
}

/// Declaring `[models]` (what to fetch + which views to render) runs nothing,
/// so a source-only declaration must not demand trust -- the same way
/// declaring an npm dependency does not. An `endpoint` override does, because it
/// chooses where the bytes come from. This goes through the real CLI config
/// gate, and `affects_tool_dispatch("models..") == false` is what keeps the
/// shim from blocking an unrelated `cargo --version` in such a project (the
/// shim-side predicate is unit-tested in trust.rs).
#[test]
fn source_only_models_declaration_needs_no_trust_but_endpoint_does() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let config = project.join("osdk.toml");

    // Source + include + a view mapping: no byte-source override, no trust.
    std::fs::write(
        &config,
        "[models.sd]\nsource = \"hf:runwayml/stable-diffusion-v1-5@main\"\n         include = [\"*.safetensors\"]\n         [models.sd.views.comfyui.map]\nunet = \"diffusion_models\"\n",
    )
    .unwrap();
    let accepted = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(
        accepted.status.success(),
        "a source-only [models] declaration must need no trust: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    // Adding an endpoint to the same entry makes it trust-requiring.
    std::fs::write(
        &config,
        "[models.sd]\nsource = \"hf:runwayml/stable-diffusion-v1-5@main\"\n         endpoint = \"https://mirror.example.com\"\n",
    )
    .unwrap();
    let rejected = run_isolated_in(temp.path(), &project, &["config", "list"]);
    assert!(
        !rejected.status.success(),
        "an endpoint override must require trust"
    );
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(message.contains("is not trusted"), "{message}");
    assert!(message.contains("models.sd.endpoint"), "{message}");
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
fn lock_records_project_tools_and_leaves_global_pins_out() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    // The global layer pins a different tool, and a *different version* of the
    // one the project pins. Both must stay out of the project's lock: it is
    // committed alongside `osdk.toml`, so a global pin leaking in would make one
    // machine's configuration everybody else's locked truth.
    std::fs::write(
        temp.path().join("config/config.toml"),
        "[tools]\ngo = \"1.26.5\"\n",
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\npython = \"3.14\"\n").unwrap();

    let output = run_isolated_in(temp.path(), &project, &["--offline", "lock"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lockfile = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(
        lockfile.contains(&format!("[platforms.{}.tools.python]", platform_key())),
        "project pin is missing from the lock: {lockfile}"
    );
    assert!(
        lockfile.contains("request = \"3.14\""),
        "lock did not record the project's requested spec: {lockfile}"
    );
    // A tool only the global config names must not appear at all. This is the
    // assertion the defect trips: before the fix the lock named every global
    // pin, so a project pinning one tool produced a lock naming all of them.
    assert!(
        !lockfile.contains("tools.go"),
        "global-only pin leaked into the project lock: {lockfile}"
    );
}

#[test]
fn lock_still_records_a_global_tool_when_it_is_named_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.toml"),
        "[tools]\npython = \"3.14\"\n",
    )
    .unwrap();
    // No project config at all: the only pin is global. Naming it on the command
    // line is an instruction, so filtering must not swallow it -- otherwise
    // `osdk lock python` would silently write an empty lock.
    let output = run_isolated_in(temp.path(), &project, &["--offline", "lock", "python"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lockfile = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    assert!(
        lockfile.contains(&format!("[platforms.{}.tools.python]", platform_key())),
        "explicit operand was filtered out of the lock: {lockfile}"
    );
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
fn self_upgrade_offline_reports_the_blocker_without_touching_the_binaries() {
    // `self upgrade` replaces the running programs, so the failure mode that
    // matters most is a half-applied upgrade. Offline is the cheapest way to
    // make the download fail deterministically: the command must refuse before
    // it writes anything next to the binaries.
    let temp = tempfile::tempdir().unwrap();
    let install_dir = osdk().parent().unwrap().to_path_buf();
    let before = installed_program_snapshot(&install_dir);

    let output = run_isolated(temp.path(), &["--offline", "self", "upgrade"]);
    assert!(
        !output.status.success(),
        "offline upgrade unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("offline"),
        "offline upgrade must explain the blocker, got: {stderr}"
    );
    assert_eq!(
        installed_program_snapshot(&install_dir),
        before,
        "a failed upgrade must leave the installed programs untouched"
    );
}

#[test]
fn self_upgrade_help_documents_the_mirror_choice_in_both_languages() {
    let temp = tempfile::tempdir().unwrap();

    let english = run_isolated(temp.path(), &["self", "upgrade", "--help"]);
    assert!(
        english.status.success(),
        "{}",
        String::from_utf8_lossy(&english.stderr)
    );
    let english = String::from_utf8(english.stdout).unwrap();
    for expected in ["--version", "--dry-run", "--force", "osdk-shim", "mirror"] {
        assert!(english.contains(expected), "missing {expected}: {english}");
    }

    let chinese = run_isolated(temp.path(), &["--lang", "zh", "self", "upgrade", "--help"]);
    assert!(
        chinese.status.success(),
        "{}",
        String::from_utf8_lossy(&chinese.stderr)
    );
    let chinese = String::from_utf8(chinese.stdout).unwrap();
    assert!(chinese.contains("镜像"), "{chinese}");
    assert!(!chinese.contains("help.self."), "{chinese}");
}

#[test]
fn self_is_a_configurable_download_source_of_its_own() {
    // The upgrade path must be steerable the same way a tool download is, and
    // must not share state with a `github:` install of the same repository.
    let temp = tempfile::tempdir().unwrap();

    let listed = run_isolated(temp.path(), &["source", "list", "self"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains("github"), "{listed}");
    assert!(listed.contains("ghproxy"), "{listed}");

    let pinned = run_isolated(temp.path(), &["source", "pin", "self", "ghproxy"]);
    assert!(
        pinned.status.success(),
        "{}",
        String::from_utf8_lossy(&pinned.stderr)
    );
    let config = std::fs::read_to_string(temp.path().join("config/config.toml")).unwrap();
    assert!(config.contains("[sources.self]"), "{config}");
    assert!(config.contains("pin = \"ghproxy\""), "{config}");

    let unknown = run_isolated(temp.path(), &["source", "pin", "self", "nope"]);
    assert!(!unknown.status.success());
}

/// File name plus length for every program beside the test binary, which is
/// enough to notice a replacement without depending on content.
fn installed_program_snapshot(dir: &Path) -> Vec<(String, u64)> {
    let mut snapshot: Vec<(String, u64)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("osdk") {
                return None;
            }
            Some((name, entry.metadata().ok()?.len()))
        })
        .collect();
    snapshot.sort();
    snapshot
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
        "[tools]\n\"tool.fixture\" = { version = \"npm:fixture-cli@1.2.3\", installer = \"pnpm\" }\n",
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
    assert!(config.contains("installer = \"pnpm\""), "{config}");
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

/// Compile a single-file Windows fixture executable with the ambient rustc.
///
/// Returns `false` when this environment cannot produce a runnable `.exe`, so the
/// caller can skip rather than report a product failure.
///
/// # Why a skip and not an assert
///
/// `scripts/windows-wine-tests.sh` runs the `x86_64-pc-windows-gnu` test
/// binaries under Wine, with `RUSTC` pointing at the host's *Linux* rustc. Wine
/// cannot spawn an ELF image, so the compile fails with `Invalid handle.` --
/// which says nothing about the behaviour under test, and indeed all three
/// affected tests pass on a real Windows runner. Treating the missing fixture as
/// "cannot be observed here" keeps the Wine job meaningful for the several dozen
/// tests it *can* execute, instead of leaving three permanent red results that
/// train everyone to ignore the job.
///
/// A failure is still distinguished from an impossibility: a rustc that starts
/// and then rejects the source is a real problem and panics.
#[cfg(windows)]
fn compile_windows_fixture(source: &Path, output: &Path, crate_name: &str) -> bool {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let compile = Command::new(&rustc)
        .args(["--crate-name", crate_name, "--edition", "2021", "-O"])
        .arg(source)
        .arg("-o")
        .arg(output)
        .output();
    let compile = match compile {
        Ok(compile) => compile,
        Err(error) => {
            // Could not even start the compiler: the only known cause is the
            // cross-execution mismatch described above.
            eprintln!(
                "skipping: cannot spawn `{rustc}` to build the {crate_name} fixture \
                 ({error}); a Windows test binary running under Wine cannot execute \
                 the host's Linux rustc"
            );
            return false;
        }
    };
    // rustc ran. A rejected source is a genuine fault in the fixture, not an
    // environment limitation, so do not let it pass as a skip.
    assert!(
        compile.status.success(),
        "building the {crate_name} fixture failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    // A cross-compiled ELF named `.exe` would load nowhere; make sure something
    // was actually produced before promising the caller a usable fixture.
    assert!(
        output.is_file(),
        "the {crate_name} fixture reported success but produced no file at {}",
        output.display()
    );
    true
}

/// A stand-in rustup that appends the delegate environment it received, so a
/// test can assert on the mirror osdk actually passed down.
///
/// Windows needs a real executable here (osdk resolves `rustup.exe`, and the
/// loader rejects a batch file under that name), so the recorder is compiled
/// with the rustc that is already running the test.
///
/// `None` means this environment cannot build that executable at all (see
/// [`compile_windows_fixture`]); the caller skips instead of failing.
#[cfg(windows)]
fn write_fake_rustup(root: &Path, project: &Path) -> Option<PathBuf> {
    let rustup = root.join("data/cargo/bin/rustup.exe");
    std::fs::create_dir_all(rustup.parent().unwrap()).unwrap();
    let log = root.join("rustup-calls.log");
    let source = root.join("fake-rustup.rs");
    // The log path and canned `override list` output are baked in as literals so
    // the recorder needs no environment of its own.
    std::fs::write(
        &source,
        format!(
            r##"
fn main() {{
    use std::io::Write;
    let log = std::path::PathBuf::from(r#"{log}"#);
    let var = |key: &str| std::env::var(key).unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap();
    writeln!(
        file,
        "{{}}|{{}}|{{}}|{{}}|{{}}",
        var("RUSTUP_HOME"),
        var("CARGO_HOME"),
        var("RUSTUP_DIST_SERVER"),
        var("RUSTUP_UPDATE_ROOT"),
        args.join(" ")
    )
    .unwrap();
    let joined = args.join(" ");
    if joined.starts_with("component list") {{
        print!("rustfmt-x86_64-pc-windows-msvc (installed)\n");
    }} else if joined.starts_with("target list") {{
        print!("x86_64-pc-windows-msvc (installed)\n");
    }} else if joined.starts_with("check") {{
        print!("stable - Up to date\n");
    }} else if joined.starts_with("override list") {{
        print!(r#"{project}"#);
        print!(" stable-x86_64-pc-windows-msvc\n");
    }}
}}
"##,
            log = log.display(),
            project = project.display()
        ),
    )
    .unwrap();
    if !compile_windows_fixture(&source, &rustup, "fake_rustup") {
        return None;
    }
    Some(rustup)
}

/// Always `Some` on Unix: a shell script needs no compiler. The `Option` exists
/// only so both platforms present one signature to the call sites.
#[cfg(unix)]
fn write_fake_rustup(root: &Path, project: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let rustup = root.join("data/cargo/bin/rustup");
    std::fs::create_dir_all(rustup.parent().unwrap()).unwrap();
    let log = root.join("rustup-calls.log");
    std::fs::write(
        &rustup,
        format!(
            r#"#!/bin/sh
printf '%s|%s|%s|%s|%s\n' "$RUSTUP_HOME" "$CARGO_HOME" "$RUSTUP_DIST_SERVER" "$RUSTUP_UPDATE_ROOT" "$*" >> '{}'
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
    Some(rustup)
}

#[cfg(unix)]
#[test]
fn rust_lifecycle_commands_use_isolated_rustup_and_repair_markers() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // Unix-only test: the fixture is a shell script and cannot fail to build.
    write_fake_rustup(temp.path(), &project).expect("the unix fixture is always available");
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
    // Unix-only test: the fixture is a shell script and cannot fail to build.
    write_fake_rustup(temp.path(), &project).expect("the unix fixture is always available");

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
fn rust_subcommand_without_managed_rustup_points_at_install() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(
        temp.path(),
        &["rust", "target", "list", "--toolchain", "stable"],
    );
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("osdk install rust"), "{err}");
    assert!(
        err.contains("never drives a rustup already on your PATH"),
        "{err}"
    );
}

/// `rust target add` downloads from the dist server, so the selected source has
/// to reach rustup as `RUSTUP_DIST_SERVER`. It previously did not: the pin was
/// only consulted by the install path, leaving `add` on whatever the ambient
/// environment held.
#[test]
fn rust_target_add_drives_the_selected_source_over_an_ambient_mirror() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The recorder must be a runnable executable; where it cannot be built this
    // contract is unobservable rather than broken.
    if write_fake_rustup(temp.path(), &project).is_none() {
        return;
    }
    std::fs::create_dir_all(temp.path().join("data/rustup/toolchains/stable/bin")).unwrap();

    // An ambient mirror that must not win, mimicking a shell profile or CI.
    let ambient = [
        ("RUSTUP_DIST_SERVER", "https://ambient.example/rustup"),
        (
            "RUSTUP_UPDATE_ROOT",
            "https://ambient.example/rustup/rustup",
        ),
    ];

    let pinned = run_isolated_in_with_env(
        temp.path(),
        &project,
        &[
            "--source",
            "rsproxy",
            "rust",
            "target",
            "add",
            "x86_64-linux-android",
            "--toolchain",
            "stable",
        ],
        &ambient,
    );
    assert!(
        pinned.status.success(),
        "{}",
        String::from_utf8_lossy(&pinned.stderr)
    );

    let calls = std::fs::read_to_string(temp.path().join("rustup-calls.log")).unwrap();
    let add = calls
        .lines()
        .find(|line| line.contains("target add"))
        .expect("target add was never delegated to rustup");
    let fields: Vec<&str> = add.split('|').collect();
    assert_eq!(
        fields[2], "https://rsproxy.cn",
        "--source rsproxy must reach rustup as RUSTUP_DIST_SERVER: {add}"
    );
    assert_eq!(
        fields[3], "https://rsproxy.cn/rustup",
        "the source index must reach rustup as RUSTUP_UPDATE_ROOT: {add}"
    );
}

/// Local rust operations never download, so they must not inherit an ambient
/// mirror either: leaking one made the managed toolchain answer to a host osdk
/// had not selected.
#[test]
fn local_rust_operations_do_not_inherit_an_ambient_mirror() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The recorder must be a runnable executable; where it cannot be built this
    // contract is unobservable rather than broken.
    if write_fake_rustup(temp.path(), &project).is_none() {
        return;
    }
    std::fs::create_dir_all(temp.path().join("data/rustup/toolchains/stable/bin")).unwrap();

    let output = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["rust", "target", "list", "--toolchain", "stable"],
        &[
            ("RUSTUP_DIST_SERVER", "https://ambient.example/rustup"),
            (
                "RUSTUP_UPDATE_ROOT",
                "https://ambient.example/rustup/rustup",
            ),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = std::fs::read_to_string(temp.path().join("rustup-calls.log")).unwrap();
    for line in calls.lines() {
        let fields: Vec<&str> = line.split('|').collect();
        assert!(
            !fields[2].contains("ambient.example"),
            "ambient RUSTUP_DIST_SERVER leaked into managed rustup: {line}"
        );
        assert!(
            !fields[3].contains("ambient.example"),
            "ambient RUSTUP_UPDATE_ROOT leaked into managed rustup: {line}"
        );
    }
}

#[test]
fn source_pin_rust_notes_managed_scope_in_both_languages() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_isolated(temp.path(), &["source", "pin", "rust", "rsproxy"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pinned rust to source rsproxy"), "{stdout}");
    assert!(
        stdout.contains("applies only to Rust installed by osdk"),
        "{stdout}"
    );
    let config = std::fs::read_to_string(temp.path().join("config/config.toml")).unwrap();
    assert!(config.contains("pin = \"rsproxy\""), "{config}");

    let zh = run_isolated_in_with_env(
        temp.path(),
        temp.path(),
        &["source", "pin", "rust", "tuna"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(
        zh.status.success(),
        "{}",
        String::from_utf8_lossy(&zh.stderr)
    );
    let zh_out = String::from_utf8_lossy(&zh.stdout);
    assert!(zh_out.contains("只对 osdk 安装的 Rust 生效"), "{zh_out}");
}

#[test]
fn where_explicit_selector_matches_installed_version_instead_of_active() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The project pins java to the 26 install; this must not leak into the
    // answer for an explicit `where java@...` selector.
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\njava = \"26.0.2.1+1\"\n",
    )
    .unwrap();
    let trusted = [("OSDK_TRUSTED_CONFIG_PATHS", project.to_str().unwrap())];
    let v21 = temp.path().join("installs/java/21.0.12.1+1");
    let v26 = temp.path().join("installs/java/26.0.2.1+1");
    for install in [&v21, &v26] {
        std::fs::create_dir_all(install).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();
    }

    // A four-part Java PSU version is not valid semver and parses as a
    // prefix, but `where` must still locate the matching install.
    let four_part = run_isolated_in_with_env(
        temp.path(),
        &project,
        &["where", "java@21.0.12.1+1"],
        &trusted,
    );
    assert!(
        four_part.status.success(),
        "{}",
        String::from_utf8_lossy(&four_part.stderr)
    );
    assert_eq!(
        PathBuf::from(String::from_utf8(four_part.stdout).unwrap().trim()),
        v21
    );

    // A three-part selector reaches the four-part install via the same
    // semver-aware fallback used at install time.
    let same_core =
        run_isolated_in_with_env(temp.path(), &project, &["where", "java@21.0.12"], &trusted);
    assert!(
        same_core.status.success(),
        "{}",
        String::from_utf8_lossy(&same_core.stderr)
    );
    assert_eq!(
        PathBuf::from(String::from_utf8(same_core.stdout).unwrap().trim()),
        v21
    );

    // A plain numeric prefix selects among installed versions too.
    let prefix = run_isolated_in_with_env(temp.path(), &project, &["where", "java@21"], &trusted);
    assert!(
        prefix.status.success(),
        "{}",
        String::from_utf8_lossy(&prefix.stderr)
    );
    assert_eq!(
        PathBuf::from(String::from_utf8(prefix.stdout).unwrap().trim()),
        v21
    );

    // An explicit selector without an installed match fails clearly.
    let missing = run_isolated_in_with_env(temp.path(), &project, &["where", "java@17"], &trusted);
    assert!(!missing.status.success());
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(err.contains("java@17 is not installed"), "{err}");

    // A bare `where java` keeps active-version semantics and answers 26.
    let bare = run_isolated_in_with_env(temp.path(), &project, &["where", "java"], &trusted);
    assert!(
        bare.status.success(),
        "{}",
        String::from_utf8_lossy(&bare.stderr)
    );
    assert_eq!(
        PathBuf::from(String::from_utf8(bare.stdout).unwrap().trim()),
        v26
    );
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
            "bin/pnpm",
            "${PNPM_HOME-unset}|${npm_config_store_dir-unset}|${pnpm_config_store_dir-unset}",
            "pnpm|pnpm-store|unset",
            "npm_config_store_dir",
        ),
        (
            "pnpm",
            "11.0.0",
            "bin/pnpm",
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

/// A JVM tool receives the JDK its directory selects, and osdk's own stale
/// export never stands in for the user's choice.
///
/// `kotlin` ships a compiler that runs on a JVM but bundles no `java`, so `exec`
/// fills in a JDK. It used to stand aside whenever `JAVA_HOME` was set at all,
/// which conflated two opposite cases: a value the user chose, and a value osdk
/// itself exported for whichever directory the last shell prompt saw. The second
/// let a JDK from one project silently drive a build in another.
#[test]
fn exec_recomputes_a_stale_managed_java_home_but_keeps_the_users_own() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The project pins the older JDK; the newer one is merely installed, and is
    // what a previous activation elsewhere would have exported.
    std::fs::write(
        project.join("osdk.toml"),
        "[tools]\njava = \"21.0.2+13\"\nkotlin = \"2.4.10\"\n",
    )
    .unwrap();
    for version in ["21.0.2+13", "26.0.1+9"] {
        let home = temp.path().join(format!("installs/java/{version}/bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            temp.path()
                .join(format!("installs/java/{version}/.osdk-complete")),
            b"",
        )
        .unwrap();
    }
    let java_install = |version: &str| {
        temp.path()
            .join("installs")
            .join("java")
            .join(version)
            .display()
            .to_string()
    };
    let selected = java_install("21.0.2+13");
    let user_choice = java_install("26.0.1+9");

    let tools = temp.path().join("installs/kotlin/2.4.10/bin");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(
        temp.path().join("installs/kotlin/2.4.10/.osdk-complete"),
        b"",
    )
    .unwrap();
    // The reporter must be a runnable executable; where it cannot be built this
    // contract is unobservable rather than broken.
    let Some(reporter) = write_java_home_reporter(&tools) else {
        return;
    };

    let run = |env: &[(&str, &str)]| {
        let output = run_isolated_in_with_env(
            temp.path(),
            &project,
            &[
                "--offline",
                "exec",
                "--tool",
                "kotlin@2.4.10",
                "--",
                &reporter,
            ],
            env,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("JAVA_HOME="),
            "the stand-in JVM tool did not run, so this proves nothing: {stdout}"
        );
        stdout
    };

    // Nothing set: the directory's own pin is used.
    let unset = run(&[]);
    assert!(
        unset.contains(&selected),
        "an unset JAVA_HOME did not get the pinned JDK; got: {unset}"
    );

    // Set, and claimed by OSDK_MANAGED_ENV: osdk's own leftover output. It
    // describes some other directory, so it must be recomputed. This is the
    // assertion the defect trips.
    let stale = run(&[
        ("JAVA_HOME", user_choice.as_str()),
        ("OSDK_MANAGED_ENV", "GOROOT,JAVA_HOME,CARGO_HOME"),
    ]);
    assert!(
        stale.contains(&selected),
        "a stale osdk-exported JAVA_HOME overrode the project pin: {stale}"
    );

    // Set with no managed marker: the user's own choice, which survives.
    let owned = run(&[("JAVA_HOME", user_choice.as_str())]);
    assert!(
        owned.contains(&user_choice),
        "a user-set JAVA_HOME was overwritten: {owned}"
    );

    // Managed, but managing other variables: still the user's JAVA_HOME.
    let other = run(&[
        ("JAVA_HOME", user_choice.as_str()),
        ("OSDK_MANAGED_ENV", "GOROOT,CARGO_HOME"),
    ]);
    assert!(
        other.contains(&user_choice),
        "a user-set JAVA_HOME was overwritten inside an activated shell: {other}"
    );
}

/// A stand-in JVM tool that prints the `JAVA_HOME` it was launched with.
///
/// Returns the command name `exec` should be given.
///
/// Windows needs a real executable: the isolated harness clears `PATH` and sets
/// no `ComSpec`, so a `.cmd` cannot be started at all -- and it fails *quietly*,
/// with `exec` reporting success and producing no output, which would read as
/// "the JDK was wrong" rather than "the fixture never ran". Compiled with the
/// rustc already running the tests, as the fake rustup fixture does.
///
/// `None` means this environment cannot build that executable at all (see
/// [`compile_windows_fixture`]); the caller skips instead of failing.
#[cfg(windows)]
fn write_java_home_reporter(dir: &Path) -> Option<String> {
    let source = dir.join("java-home-reporter.rs");
    std::fs::write(
        &source,
        r#"
fn main() {
    println!(
        "JAVA_HOME={}",
        std::env::var("JAVA_HOME").unwrap_or_else(|_| "unset".to_string())
    );
}
"#,
    )
    .unwrap();
    if !compile_windows_fixture(&source, &dir.join("kotlinc.exe"), "java_home_reporter") {
        return None;
    }
    std::fs::remove_file(&source).unwrap();
    Some("kotlinc".to_string())
}

/// Always `Some` off Windows: a shell script needs no compiler. The `Option`
/// exists only so both platforms present one signature to the call sites.
#[cfg(not(windows))]
fn write_java_home_reporter(dir: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("kotlinc");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'JAVA_HOME=%s\\n' \"${JAVA_HOME-unset}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    Some("kotlinc".to_string())
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
        "pnpm" => format!("bin/{alias}"),
        "deno" => alias.to_string(),
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

/// Write an npm registry config, optionally pinning `[sources] mode`.
///
/// `mode = "env"` is the setting under which an explicit registry environment
/// variable is obeyed verbatim and no probing happens at all.
#[cfg(not(windows))]
fn write_registry_config_with_sources(root: &Path, urls: &[&str], mode: Option<&str>) {
    let directory = root.join("config");
    std::fs::create_dir_all(&directory).unwrap();
    let urls = urls
        .iter()
        .map(|url| format!("\"{url}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let sources = match mode {
        Some(mode) => format!("[sources]\nmode = \"{mode}\"\n\n"),
        None => String::new(),
    };
    std::fs::write(
        directory.join("config.toml"),
        format!("{sources}[registries.npm]\nurls = [{urls}]\nprobe_timeout_ms = 500\n"),
    )
    .unwrap();
}

/// The common case: registry urls with whatever source mode is the default.
#[cfg(not(windows))]
fn write_registry_config(root: &Path, urls: &[&str]) {
    write_registry_config_with_sources(root, urls, None);
}

#[cfg(unix)]
#[test]
fn authenticated_global_use_fails_before_public_metadata_probe_or_installer_spawn() {
    for installer in ["npm", "pnpm"] {
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
            write_fake_registry_manager(
                temporary.path(),
                installer,
                "10.0.0",
                installer,
                "#!/bin/sh\nprintf ran > \"$OSDK_TEST_MARKER\"\n",
            );
            environment.push(("OSDK_TEST_MARKER".into(), marker_value));
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
            temporary.path().join("installs/pnpm/10.0.0/bin")
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
    // `mode = "env"` is what now means "obey the explicit registry and do not
    // probe". A bare environment variable no longer ends planning: it became one
    // more candidate to rank, so that an unreachable mirror in the environment
    // loses to a working one instead of winning by default. Without this the
    // dead candidate below is probed, every probe fails, and the command is
    // refused -- which is correct behaviour, just not what this test is about.
    write_registry_config_with_sources(temporary.path(), &[&dead], Some("env"));
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

    let apply = run_isolated_in_with_env(
        temporary.path(),
        temporary.path(),
        &["container", "mirrors", "apply", "--help"],
        &[("OSDK_LANG", "zh")],
    );
    assert!(apply.status.success());
    let apply = String::from_utf8(apply.stdout).unwrap();
    for expected in ["测速、确认并原子应用", "精确原生配置文件", "本次计划 ID"]
    {
        assert!(apply.contains(expected), "{apply}");
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
        // A real npm lockfile: JSON, `lockfileVersion` 2 or 3, with the tool
        // keyed by its install path. Feeding pnpm's YAML to a file named
        // package-lock.json makes the graph unparseable, and the install is then
        // dropped for "invalid provider evidence" without anything saying so.
        let lockfile = serde_json::to_string_pretty(&serde_json::json!({
            "name": "osdk-dynamic-npm-tool",
            "lockfileVersion": 3,
            "requires": true,
            "packages": {
                "": {
                    "name": "osdk-dynamic-npm-tool",
                    "dependencies": { "@antfu/ni": version },
                },
                "node_modules/@antfu/ni": {
                    "version": version,
                    "resolved": format!(
                        "https://registry.example.test/@antfu/ni/-/ni-{version}.tgz"
                    ),
                    "integrity": "sha512-fixture-integrity",
                },
            },
        }))
        .unwrap();
        std::fs::write(project_root.join("package-lock.json"), &lockfile).unwrap();
        let launcher = project_root.join("node_modules/.bin/ni");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &launcher).unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "ni".into(),
            path: "project/node_modules/.bin/ni".into(),
            ..Default::default()
        }];
        manifest.write_atomic(&install_root).unwrap();
        std::fs::write(
            osdk_core::backend::npm_package::npm_receipt_path(&install_root),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": 1,
                "provider": "npm-package",
                "package": "@antfu/ni",
                "installer": "npm",
                "node_version": "1.0.0",
                "build_policy": "deny",
                "graph_sha256": osdk_core::pipeline::verify::hash_bytes(
                    lockfile.as_bytes(),
                    osdk_core::pipeline::HashAlgo::Sha256,
                ),
                "root_integrity": "sha512-fixture-integrity",
                // The graph's root_source is the lockfile's `resolved` URL, not
                // the request string -- it must match what npm_graph_identity
                // reads back out of the lockfile written above.
                "root_source": format!(
                    "https://registry.example.test/@antfu/ni/-/ni-{version}.tgz"
                )
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

fn view_list_empty(output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no model views configured"),
        "empty list output: {stdout}"
    );
}

#[test]
fn model_view_offline_contract() {
    let temp = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| run_isolated(temp.path(), args);

    // 1. List with nothing configured.
    view_list_empty(&run(&["model", "view", "list"]));

    // 2. add refuses an unpulled model with an actionable message (not a raw
    //    io error).
    let output = run(&["model", "view", "add", "comfyui", "ghost"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not pulled") && stderr.contains("osdk model pull ghost"),
        "expected actionable unpulled error, got: {stderr}"
    );
    // Nothing was persisted after the failed add.
    view_list_empty(&run(&["model", "view", "list"]));

    // 3. path prints the stable, profile-scoped view root.
    let output = run(&["model", "view", "path", "comfyui"]);
    assert!(output.status.success());
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        path.replace('\\', "/").ends_with("views/comfyui/default"),
        "unexpected stable view path: {path}"
    );

    // 4. remove of a nonexistent view is a benign "nothing to remove".
    let output = run(&["model", "view", "remove", "comfyui"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("nothing to remove"),
        "got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // 5. export prints a fragment with a unique key and, critically, no
    //    is_default (research 搂5.11: that flag silently reorders name
    //    collisions against the user's own roots).
    let output = run(&["model", "view", "export", "comfyui"]);
    assert!(output.status.success());
    let fragment = String::from_utf8_lossy(&output.stdout);
    assert!(fragment.contains("osdk-comfyui-default:"), "{fragment}");
    assert!(fragment.contains("base_path:"), "{fragment}");
    assert!(
        !fragment.to_ascii_lowercase().contains("is_default"),
        "export must never emit is_default: {fragment}"
    );
}

#[test]
fn model_view_export_to_merge_is_idempotent_and_preserves_user_yaml() {
    let temp = tempfile::tempdir().unwrap();
    let yaml = temp.path().join("extra_model_paths.yaml");
    // Pre-existing user content that must survive repeated osdk merges.
    std::fs::write(
        &yaml,
        "# my user header\nmy_own:\n  base_path: /elsewhere\n  checkpoints: ckpt\n",
    )
    .unwrap();
    let run = |args: &[&str]| run_isolated(temp.path(), args);
    let y = yaml.to_str().unwrap();

    run(&["model", "view", "export", "comfyui", "--to", y]);
    run(&["model", "view", "export", "comfyui", "--to", y]);

    let body = std::fs::read_to_string(&yaml).unwrap();
    // User block untouched and still present once.
    assert!(body.contains("my_own:"));
    assert_eq!(body.matches("my_own:").count(), 1);
    // Managed block present exactly once despite two exports.
    assert_eq!(body.matches("osdk-comfyui-default:").count(), 1);
    assert!(!body.to_ascii_lowercase().contains("is_default"));

    let _ = Path::new("");
}

/// `osdk deps` with no `[deps]` section reports what it found and stops.
///
/// Running `npm ci` because a package.json exists would be precisely the kind of
/// implicit large side effect osdk avoids: a bare `install` does not fetch models
/// either. The grouping matters too -- four Node providers read the same
/// `package.json`, so one line per provider would claim four findings where there
/// is one project.
#[test]
fn deps_without_a_declaration_reports_candidates_without_installing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[tools]\n").unwrap();

    let output = run_isolated_in(root, &project, &["deps"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no `[deps]` section"), "{stdout}");
    assert!(stdout.contains("package.json"), "{stdout}");
    // Assert the *shape*, not just that the words appear: one manifest line, and
    // each provider named once. A per-provider loop would print the manifest four
    // times and still contain the right words somewhere.
    let manifest_lines = stdout
        .lines()
        .filter(|line| line.trim_end().ends_with("package.json"))
        .count();
    assert_eq!(
        manifest_lines, 1,
        "the one manifest must be listed once, not once per provider: {stdout}"
    );
    let candidate_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("candidates:"))
        .collect();
    assert_eq!(candidate_lines.len(), 1, "{stdout}");
    assert!(
        candidate_lines[0]
            .trim()
            .ends_with("candidates: bun, npm, pnpm, yarn"),
        "each provider exactly once, in a stable order: {stdout}"
    );
    // Nothing was installed: the read-only report must not have created a
    // dependency directory. Checking the filesystem rather than the exit code is
    // the point -- a successful exit says nothing about side effects.
    assert!(
        !project.join("node_modules").exists(),
        "reporting must not install"
    );
}

/// The frozen/non-frozen decision is osdk's, made by looking for the native
/// lockfile itself rather than by passing a flag and hoping.
///
/// This exists because two of the four Node installers do not fail without a
/// lockfile: `yarn@1` and `bun` accept a freeze-ish invocation and install
/// anyway. Delegating the check would therefore be silently wrong on half the
/// matrix, so osdk checks, and says so when it has to fall back.
/// A declared provider is automatic without saying so, and `auto = false` opts out.
///
/// Goes through the real binary because the default is a user-visible promise: a
/// unit test on the struct would pass even if the CLI never consulted the field.
/// `--dry-run` is what makes the observation safe -- no package manager is
/// required, yet the decision to act is still printed.
#[test]
fn a_declared_provider_is_automatic_unless_it_opts_out() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();

    // Default: the provider is listed, so an automatic run would cover it.
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(
        listed.contains("pnpm"),
        "a declared provider must be visible to deps: {listed}"
    );

    // `auto = false` must not remove it from an explicit run: naming the command
    // is its own opt-in, and a flag that silently disabled `osdk deps` itself
    // would be the worse failure.
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\nauto = false\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(
        listed.contains("pnpm"),
        "`auto = false` must not hide the provider from an explicit run: {listed}"
    );
}

/// `--no-deps` is accepted on every entry point that materializes dependencies.
///
/// Exists because the three flags are declared separately, so one can be dropped
/// while the other two keep the feature looking intact. A missing flag is a clap
/// parse error (exit 2), which this distinguishes from a command that ran.
#[test]
fn no_deps_is_accepted_by_install_run_and_exec() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();

    for args in [
        vec!["install", "--no-deps", "--help"],
        vec!["run", "--no-deps", "--help"],
        vec!["exec", "--no-deps", "--help"],
    ] {
        let output = run_isolated_in(root, &project, &args);
        // `--help` succeeds; an unknown argument would have failed first, so
        // success here is specifically evidence that the flag exists.
        assert!(
            output.status.success(),
            "`{}` must accept --no-deps: {output:?}",
            args.join(" ")
        );
    }
}

#[test]
fn deps_downgrades_from_frozen_only_when_it_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();

    // No lockfile: non-frozen, and the downgrade is reported rather than silent.
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("warning:"), "{stdout}");
    assert!(stdout.contains("no pnpm-lock.yaml"), "{stdout}");
    assert!(
        !stdout.contains("--frozen-lockfile"),
        "must not claim to freeze without a lockfile: {stdout}"
    );
    assert!(stdout.contains("--ignore-scripts"), "{stdout}");

    // `--frozen` turns that fallback into an error instead of a warning.
    let output = run_isolated_in(root, &project, &["deps", "--frozen", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires a native lockfile"), "{stderr}");

    // With a lockfile present, the frozen flag appears and the warning goes.
    std::fs::write(project.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--frozen-lockfile"), "{stdout}");
    assert!(
        !stdout.contains("warning:"),
        "nothing was downgraded: {stdout}"
    );
}

/// yarn is dispatched on its major version, because classic and berry disagree
/// on both flags that matter.
///
/// Berry 4.6.0 rejects `--ignore-scripts` outright (`Unknown Syntax Error`), so
/// it has to be `YARN_ENABLE_SCRIPTS=false`; classic accepts berry's
/// `--immutable` and then neither freezes nor blocks scripts, which is the worst
/// of the two failure modes because it looks like it worked.
#[test]
fn deps_dispatches_yarn_on_its_major_version() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.yarn]\n").unwrap();
    std::fs::write(project.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();

    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","packageManager":"yarn@4.6.0"}"#,
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("YARN_ENABLE_SCRIPTS=false"), "{stdout}");
    assert!(stdout.contains("--immutable"), "{stdout}");
    assert!(
        !stdout.contains("--ignore-scripts"),
        "berry rejects that flag: {stdout}"
    );

    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","packageManager":"yarn@1.22.19"}"#,
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--frozen-lockfile"), "{stdout}");
    assert!(stdout.contains("--ignore-scripts"), "{stdout}");
    assert!(
        !stdout.contains("--immutable"),
        "classic accepts it and ignores it, which is worse than refusing: {stdout}"
    );
    assert!(!stdout.contains("YARN_ENABLE_SCRIPTS"), "{stdout}");
}

/// Discovery is fail-closed in both directions it can go wrong.
///
/// A manifest that will not parse is an error, not a skip: silently walking past
/// it would install the wrong project's dependencies (or none) and report
/// success. And a declared package manager that disagrees with the lockfile on
/// disk is refused rather than guessed, the same judgement `npm_tools` already
/// makes.
#[test]
fn deps_discovery_is_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();

    std::fs::write(project.join("package.json"), "{ this is not json").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("package.json"), "{stderr}");

    // A declaration that contradicts the enabled provider: refused, because
    // running pnpm over a project whose manifest says yarn would produce a
    // second lockfile and a working tree nobody declared.
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","packageManager":"yarn@4.6.0"}"#,
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("declares package manager `yarn`"),
        "{stderr}"
    );
    assert!(stderr.contains("provider `pnpm`"), "{stderr}");

    // A declaration that contradicts the lockfile on disk: also refused, and
    // this is the case that needs both providers enabled to be visible at all.
    // osdk only looks for the lockfiles of providers the project turned on, so
    // the check is reported against the provider that owns the file.
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n\n[deps.yarn]\n").unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","packageManager":"pnpm@12.5.1"}"#,
    )
    .unwrap();
    std::fs::write(project.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("declares `pnpm`"), "{stderr}");
    assert!(stderr.contains("owned by `yarn`"), "{stderr}");
}

/// Both directions of the trust boundary, in one test so neither can drift.
///
/// Declaring a provider must be free: gating it would mean re-approving a config
/// for every ordinary line, which teaches nothing and trains the user to click
/// through the prompts that do matter. Redirecting the registry must not be free.
/// And neither may block tool dispatch -- the `[syspkg]` accident was exactly a
/// trust requirement leaking into `cargo --version`.
#[test]
fn deps_trust_gates_the_registry_but_not_the_declaration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();

    std::fs::write(
        project.join("osdk.toml"),
        "[deps.pnpm]\nauto = true\nsources = [\"package.json\"]\n",
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(
        output.status.success(),
        "an ordinary declaration must not need approval: {output:?}"
    );

    std::fs::write(
        project.join("osdk.toml"),
        "[deps.pnpm]\nindex = \"https://registry.example.com/\"\n",
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("deps.pnpm.index"), "{stderr}");

    // Same untrusted config: dispatching a tool must still work.
    let output = run_isolated_in(root, &project, &["current"]);
    assert!(
        output.status.success(),
        "a deps trust requirement must not reach tool dispatch: {output:?}"
    );
}

/// `--no-install-tools` refuses rather than acquiring a package manager, and
/// refuses *without side effects*.
///
/// This is the CI switch: a run there should use the tools an explicit
/// `osdk install` put in place, so that it cannot quietly acquire a different
/// version than the one that was reviewed. The assertions therefore check the
/// filesystem, not just the exit code -- "it failed" and "it failed after
/// installing half of something" look identical from the status alone.
#[test]
fn deps_refuses_to_acquire_tools_when_told_not_to() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();

    let output = run_isolated_in(root, &project, &["deps", "--no-install-tools"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--no-install-tools"), "{stderr}");
    // The message has to name the command that would fix it; "not installed" on
    // its own leaves the user to guess the spec.
    assert!(stderr.contains("osdk install"), "{stderr}");

    assert!(
        !root.join("installs").join("node").exists(),
        "nothing may be acquired"
    );
    assert!(
        !project.join("node_modules").exists(),
        "nothing may be installed"
    );
    // A failed run must not record freshness: doing so would make the *next*
    // run report "up to date" for a tree that was never populated, turning one
    // visible failure into a silently broken working tree.
    assert!(
        !root.join("cache").join("deps").exists(),
        "a failed run must not record freshness"
    );
}

/// Freshness distinguishes its reasons, and each reason is reachable.
///
/// Asserted together because "stale" on its own is not evidence of anything --
/// a decision function that always returned `Stale` would satisfy any single
/// case. The three paths here fail for three different, named reasons, and the
/// no-recorded-run case is what a first run must hit.
#[test]
fn deps_freshness_reports_a_distinguishable_reason() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pnpm]\n").unwrap();

    let output = run_isolated_in(root, &project, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stale"), "{stdout}");
    assert!(stdout.contains("no recorded successful run"), "{stdout}");

    // A provider whose declared sources match nothing must not be called fresh.
    // A predicate that matches no files is vacuously true, which would dress up
    // "nothing was checked" as "nothing changed".
    std::fs::write(
        project.join("osdk.toml"),
        "[deps.pnpm]\nsources = [\"does-not-exist.json\"]\n",
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stale"), "{stdout}");
}

/// The Python providers plan the commands their measured behaviour requires.
///
/// Two of those requirements are counter-intuitive enough to be worth pinning
/// from the CLI, not just from unit tests:
///
/// * `uv sync --frozen` does **not** verify the lock is current -- measured, it
///   exits 0 and installs a stale set. `--locked` is the flag that checks. A test
///   asserting merely "a freeze flag is present" would accept the weaker command.
/// * source builds must be denied with `--no-build`, never with `UV_NO_BUILD`:
///   the variable is silently ignored by `uv pip install`, so a plan that relied
///   on it would look safe while building every sdist locally.
#[test]
fn python_deps_plan_locked_and_deny_source_builds() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.uv]\n").unwrap();

    // No lock: reported, and without a freeze flag it does not have the right to.
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no uv.lock"), "{stdout}");
    // Assert flags against the command line itself. Reading them off the whole
    // output would let a word in a warning decide a claim about the command --
    // "no uv.lock in ..." and "--locked" are easy to confuse that way.
    let command = stdout
        .lines()
        .find(|line| line.contains("would run in"))
        .unwrap_or_else(|| panic!("no command line in output: {stdout}"));
    assert!(!command.contains("--locked"), "{command}");
    assert!(command.contains("--no-build"), "{command}");
    assert!(
        !stdout.contains("UV_NO_BUILD"),
        "the env var is ignored by `uv pip install`; a flag is required: {stdout}"
    );
    // The interpreter must come from osdk, never from uv reaching out.
    assert!(stdout.contains("UV_PYTHON_DOWNLOADS=never"), "{stdout}");

    // With a lock, `--locked` appears -- not just `--frozen`.
    std::fs::write(project.join("uv.lock"), "version = 1\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = stdout
        .lines()
        .find(|line| line.contains("would run in"))
        .unwrap_or_else(|| panic!("no command line in output: {stdout}"));
    assert!(
        command.contains("--locked"),
        "`--frozen` alone accepts a stale lock: {command}"
    );
    assert!(!stdout.contains("warning:"), "{stdout}");
}

/// A requirements file is an input to resolution, not an exact environment set.
///
/// Even `aiohttp==3.14.3` only pins a root package. `uv pip sync` installs that
/// one package and omits/removes multidict, yarl and the rest of its transitive
/// closure. The dry-run assertion is deliberately on the command line: it must
/// use resolver-backed `pip install -r`, must never use `pip sync`, and must not
/// claim the plan is frozen merely because root versions use `==`.
#[test]
fn pip_requirements_resolves_the_transitive_closure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.pip-requirements]\n").unwrap();
    std::fs::write(project.join("requirements.txt"), "aiohttp==3.14.3\n").unwrap();

    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = stdout
        .lines()
        .find(|line| line.contains("would run in"))
        .unwrap_or_else(|| panic!("no command line in output: {stdout}"));
    assert!(
        command.contains("pip install --requirements requirements.txt"),
        "{command}"
    );
    assert!(
        !command.contains("pip sync"),
        "sync treats roots as the complete set and drops transitive deps: {command}"
    );
    assert!(
        stdout.contains("not a complete dependency lock"),
        "{stdout}"
    );

    let output = run_isolated_in(root, &project, &["deps", "--frozen", "--dry-run"]);
    assert!(
        !output.status.success(),
        "top-level == pins must not masquerade as a complete lock: {output:?}"
    );
}

/// The whole reason `--verify` exists: freshness reports fresh while the
/// environment is broken.
///
/// This is the control that decides whether the layer is real or decoration.
/// Nothing in `sources` changes here -- only the installed tree is tampered with
/// -- so the hash still matches and L0 is satisfied. If `--verify` also passed,
/// it would be checking nothing worth checking.
///
/// The tampering is done directly rather than through a package manager so the
/// test needs no network: the receipt format is what is under test, and it is the
/// same file npm would have written.
#[test]
fn verify_finds_tampering_that_freshness_cannot_see() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("package-lock.json"), "{}\n").unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.npm]\n").unwrap();

    // A receipt exactly as npm writes it, with the tree it describes.
    let nm = project.join("node_modules");
    std::fs::create_dir_all(nm.join("is-odd")).unwrap();
    std::fs::create_dir_all(nm.join("is-number")).unwrap();
    std::fs::write(
        nm.join(".package-lock.json"),
        r#"{"packages":{"":{},"node_modules/is-odd":{"version":"3.0.1"},"node_modules/is-number":{"version":"6.0.0"}}}"#,
    )
    .unwrap();
    std::fs::write(
        nm.join("is-odd/package.json"),
        r#"{"name":"is-odd","version":"3.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        nm.join("is-number/package.json"),
        r#"{"name":"is-number","version":"6.0.0"}"#,
    )
    .unwrap();

    // Clean: passes, and says how much it looked at. "0 problems" and "nothing
    // examined" must not read the same.
    let output = run_isolated_in(root, &project, &["deps", "--verify"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("2 entries verified"), "{stdout}");

    // Tamper with the installed tree only. Nothing in `sources` moves.
    std::fs::write(
        nm.join("is-odd/package.json"),
        r#"{"name":"is-odd","version":"9.9.9"}"#,
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["deps", "--verify"]);
    assert!(
        !output.status.success(),
        "a swapped version must fail verification: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("is version 9.9.9 but was installed as 3.0.1"),
        "{stdout}"
    );

    // Deleting a package is caught too, and named.
    std::fs::remove_dir_all(nm.join("is-number")).unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--verify"]);
    assert!(!output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("node_modules/is-number is missing"),
        "{stdout}"
    );
}

/// An environment with no receipt is not silently a pass.
///
/// `checked == 0` with an empty finding list would be a vacuous pass -- the shape
/// AGENTS.md warns about, where "nothing was checked" is dressed up as "nothing
/// is wrong". So the absence of a receipt is itself reported.
#[test]
fn verify_refuses_to_pass_an_environment_it_cannot_check() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "[deps.npm]\n").unwrap();

    // No node_modules at all: unverifiable, and therefore not clean.
    let output = run_isolated_in(root, &project, &["deps", "--verify"]);
    assert!(!output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no install receipt"),
        "an unverifiable environment must not read as verified: {stdout}"
    );

    // `--verify` must not install anything on its way to checking.
    assert!(
        !project.join("node_modules").exists(),
        "verification is a check, not a command that changes the environment"
    );
}

/// go, cargo and deno each plan their own real frozen mode, and go is never
/// allowed to swap its toolchain.
///
/// These three share a property Node does not: their fetch step does not execute
/// dependency code (measured -- `cargo fetch` leaves no `target/`, so `build.rs`
/// never ran), so there is no `--ignore-scripts` equivalent to pass. The test
/// asserts that too, because adding one "for symmetry" would either be rejected
/// by the tool or quietly do nothing.
///
/// `GOTOOLCHAIN=local` is the load-bearing line: the default `auto` was measured
/// to attempt `go: downloading go1.99.0` when `go.mod` asks for a newer Go, which
/// would run the fetch under a toolchain osdk neither chose nor verified.
#[test]
fn native_providers_plan_real_freezes_and_pin_the_toolchain() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    for (provider, manifest, manifest_body, lock, expected_freeze) in [
        ("go", "go.mod", "module p\n\ngo 1.21\n", "go.sum", None),
        (
            "cargo",
            "Cargo.toml",
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
            "Cargo.lock",
            Some("--locked"),
        ),
        ("deno", "deno.json", "{}\n", "deno.lock", Some("--frozen")),
    ] {
        let project = root.join(provider);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(manifest), manifest_body).unwrap();
        std::fs::write(project.join("osdk.toml"), format!("[deps.{provider}]\n")).unwrap();

        // No lock: the downgrade is reported, never silent.
        let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
        assert!(output.status.success(), "{provider}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("warning:"), "{provider}: {stdout}");
        let command = stdout
            .lines()
            .find(|line| line.contains("would run in"))
            .unwrap_or_else(|| panic!("{provider}: no command in {stdout}"));
        if let Some(flag) = expected_freeze {
            assert!(
                !command.contains(flag),
                "{provider} must not claim to freeze without a lock: {command}"
            );
        }
        // No flag borrowed from another ecosystem.
        for foreign in ["--ignore-scripts", "--no-build", "--frozen-lockfile"] {
            assert!(
                !command.contains(foreign),
                "{provider} got `{foreign}` from elsewhere: {command}"
            );
        }
        if provider == "go" {
            assert!(
                command.contains("GOTOOLCHAIN=local"),
                "the default `auto` downloads a toolchain osdk did not choose: {command}"
            );
        }

        // With a lock, the real freeze flag appears.
        std::fs::write(project.join(lock), "").unwrap();
        let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
        assert!(output.status.success(), "{provider}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let command = stdout
            .lines()
            .find(|line| line.contains("would run in"))
            .unwrap_or_else(|| panic!("{provider}: no command in {stdout}"));
        if let Some(flag) = expected_freeze {
            assert!(command.contains(flag), "{provider}: {command}");
        }
        assert!(
            !stdout.contains("warning:"),
            "{provider} had nothing to downgrade: {stdout}"
        );
        if provider == "go" {
            // Pinned regardless of the lock: the hazard is unrelated to it.
            assert!(command.contains("GOTOOLCHAIN=local"), "{command}");
        }
    }
}

/// Discovery stays fail-closed for the new ecosystems.
///
/// A manifest that cannot be parsed must be an error rather than a skip -- for the
/// same reason as everywhere else: walking silently past a broken `Cargo.toml`
/// would install the wrong project's dependencies, or none, and report success.
///
/// This replaced an earlier test asserting "a valid go.mod is not parsed as JSON",
/// which turned out to be unfalsifiable: `node::declared_manager` returns early
/// when the ecosystem is not Node, so a misrouted `go.mod` was harmless and the
/// assertion passed either way. It was a probe outside the mechanism it claimed to
/// cover. This one fails when validation is absent.
#[test]
fn native_manifests_are_validated_rather_than_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    // Cargo.toml is TOML and deno.json is JSON, so both can be checked.
    for (provider, manifest, broken) in [
        ("cargo", "Cargo.toml", "[package\nname = broken"),
        ("deno", "deno.json", "{not json"),
    ] {
        let project = root.join(provider);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(manifest), broken).unwrap();
        std::fs::write(project.join("osdk.toml"), format!("[deps.{provider}]\n")).unwrap();

        let output = run_isolated_in(root, &project, &["deps", "--list"]);
        assert!(
            !output.status.success(),
            "{provider}: a broken manifest must not be skipped: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(manifest), "{provider}: {stderr}");
    }

    // And the valid versions are accepted, so the check above is discriminating
    // rather than "always fails".
    for (provider, manifest, valid) in [
        (
            "cargo",
            "Cargo.toml",
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
        ),
        ("deno", "deno.json", "{}\n"),
        ("go", "go.mod", "module p\n\ngo 1.21\n"),
    ] {
        let project = root.join(format!("{provider}-ok"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(manifest), valid).unwrap();
        std::fs::write(project.join("osdk.toml"), format!("[deps.{provider}]\n")).unwrap();

        let output = run_isolated_in(root, &project, &["deps", "--list"]);
        assert!(output.status.success(), "{provider}: {output:?}");
    }
}

/// A custom provider is declared, not discovered, and it runs.
///
/// There is no manifest to find: the declaration *is* the detection. Its root is
/// the directory of the config that declared it, so `sources` and `outputs` are
/// relative to the same place a built-in provider's would be.
///
/// Freshness is asserted in both directions, because "stale" alone proves nothing
/// -- a decision function that always said stale would satisfy half of this.
#[test]
fn a_custom_provider_runs_and_tracks_its_own_freshness() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("schema.graphql"), "type Q { a: String }\n").unwrap();

    let marker = project.join("generated");
    let run = if cfg!(windows) {
        "cmd /c mkdir generated"
    } else {
        "/bin/mkdir generated"
    };
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[deps.codegen]\nsources = [\"schema.graphql\"]\noutputs = [\"generated\"]\nrun = \"{run}\"\n"
        ),
    )
    .unwrap();

    // A custom provider's `run` is an arbitrary command, so it always needs
    // approval -- unlike declaring a built-in provider.
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(
        !output.status.success(),
        "an arbitrary command must be approved first: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("deps.codegen.run"), "{stderr}");

    let output = run_isolated_in(
        root,
        &project,
        &[
            "--yes",
            "trust",
            project.join("osdk.toml").to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{output:?}");

    // Now it is visible, with no manifest anywhere.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("codegen"), "{stdout}");
    assert!(stdout.contains("stale"), "{stdout}");

    // And it actually runs.
    let output = run_isolated_in(root, &project, &["deps"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        marker.is_dir(),
        "the command must really have run: {output:?}"
    );

    // Same inputs: fresh.
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fresh"), "{stdout}");

    // Changed source: stale again, with a reason.
    std::fs::write(
        project.join("schema.graphql"),
        "type Q { a: String, b: Int }\n",
    )
    .unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stale"), "{stdout}");

    // A missing declared output is staleness too: the step promised it.
    std::fs::write(project.join("schema.graphql"), "type Q { a: String }\n").unwrap();
    std::fs::remove_dir_all(&marker).unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stale"), "{stdout}");
    assert!(stdout.contains("generated"), "{stdout}");
}

/// `depends` decides the order, and a cycle is refused rather than resolved
/// arbitrarily.
///
/// The declaration order in the file is deliberately the opposite of the required
/// order, so a test that merely checked "both ran" would pass without any
/// ordering at all.
#[test]
fn depends_orders_providers_and_refuses_a_cycle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();

    let echo = |what: &str| {
        if cfg!(windows) {
            format!("cmd /c echo {what}")
        } else {
            format!("echo {what}")
        }
    };
    // `b` is declared first but depends on `a`, so ordering has to reverse it.
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[deps.b]\nrun = \"{}\"\ndepends = [\"a\"]\n\n[deps.a]\nrun = \"{}\"\n",
            echo("B"),
            echo("A")
        ),
    )
    .unwrap();
    let output = run_isolated_in(
        root,
        &project,
        &[
            "--yes",
            "trust",
            project.join("osdk.toml").to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let a_at = stdout.find(" A").unwrap_or_else(|| panic!("{stdout}"));
    let b_at = stdout.find(" B").unwrap_or_else(|| panic!("{stdout}"));
    assert!(
        a_at < b_at,
        "`a` must be planned before `b` despite being declared second: {stdout}"
    );

    // A cycle is an error. Choosing an order anyway would run a step before its
    // input existed and blame the wrong provider.
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[deps.a]\nrun = \"{}\"\ndepends = [\"b\"]\n\n[deps.b]\nrun = \"{}\"\ndepends = [\"a\"]\n",
            echo("A"),
            echo("B")
        ),
    )
    .unwrap();
    let output = run_isolated_in(
        root,
        &project,
        &[
            "--yes",
            "trust",
            project.join("osdk.toml").to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cycle"), "{stderr}");
    assert!(stderr.contains('a') && stderr.contains('b'), "{stderr}");
}

/// A monorepo's sub-projects come from `[deps].roots` and from nowhere else.
///
/// The undeclared `vendor/thirdparty` is the whole point of the test: it has a
/// perfectly good `package.json` sitting one level down, exactly where a subtree
/// walk would find it, and it must not appear. What can be acted on
/// automatically has to be what was declared -- otherwise `osdk deps` in a
/// monorepo installs dependencies for a package nobody asked about.
///
/// Each result is addressable as `//<path>:<provider>` and reports which pattern
/// produced it, because a repo with four `npm` packages would otherwise print
/// `npm` four times with no way to tell the lines apart.
/// `[task_config].roots` brings sub-project tasks in under `//<path>:<name>`.
///
/// The addressing is the same one `[deps].roots` uses for providers, and
/// deliberately so: one syntax for "a thing in a sub-project", not two.
#[test]
fn task_roots_expose_sub_project_tasks_under_a_rooted_name() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/api")).unwrap();
    std::fs::create_dir_all(project.join("packages/ui")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\", \"packages/*\"]\n\n[tasks.hello]\nrun = \"echo root\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("apps/api/osdk.toml"),
        "[tasks.build]\nrun = \"echo api-built\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("packages/ui/osdk.toml"),
        "[tasks.build]\nrun = \"echo ui-built\"\n",
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["task", "list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    for expected in ["//apps/api:build", "//packages/ui:build", "hello"] {
        assert!(listed.contains(expected), "missing {expected}: {listed}");
    }

    // Same-named tasks in different sub-projects stay distinct, which is the point
    // of the prefix: without it the second `build` would replace the first.
    //
    // Trusting first, because `roots` lives in `[task_config]` -- already gated as
    // `RedirectsExecution` -- so `osdk run` refuses until the config is approved.
    // That is the gate working, not an obstacle: declaring roots inherited the
    // existing protection instead of needing a new one. `task list` is exempt as a
    // read-only command, which is why the assertions above needed no trust.
    let output = run_isolated_in(
        root,
        &project,
        &["--yes", "trust", project.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");

    // `--dry-run` rather than a real run: this harness clears PATH, so a task whose
    // `run` names a program could not resolve it, and asserting on that would test
    // the harness instead of the addressing. Resolution is the property in question,
    // and dry-run shows it.
    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//apps/api:build"]);
    assert!(output.status.success(), "{output:?}");
    let planned = String::from_utf8_lossy(&output.stdout);
    assert!(
        planned.contains("api-built"),
        "the rooted name must resolve to the api sub-project's command: {planned}"
    );
    assert!(
        !planned.contains("ui-built"),
        "a rooted name must select exactly one sub-project's task: {planned}"
    );

    // And the sibling with the same task name resolves to its own command.
    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//packages/ui:build"]);
    assert!(output.status.success(), "{output:?}");
    let planned = String::from_utf8_lossy(&output.stdout);
    assert!(planned.contains("ui-built"), "{planned}");
}

/// Declaring `roots` inherits the existing `[task_config]` trust gate.
///
/// Worth pinning because it is the reason this feature needed no new trust surface.
/// `roots` lives in `[task_config]`, which is in `TRUST_REQUIRING_TABLES` as
/// `RedirectsExecution`, so `osdk run` refuses an unapproved config that declares
/// sub-projects -- for free, and for the same reason the table was gated to begin
/// with.
///
/// Found by a test failing for what looked like the wrong reason: an earlier version
/// of the test above ran a rooted task without trusting and was refused. The refusal
/// was correct.
#[test]
fn declaring_task_roots_requires_the_same_approval_as_other_runner_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/api")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("apps/api/osdk.toml"),
        "[tasks.build]\nrun = \"echo built\"\n",
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//apps/api:build"]);
    assert!(
        !output.status.success(),
        "running from an untrusted config that declares roots must be refused: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("task_config"),
        "the refusal must name the gated table: {stderr}"
    );

    // Approved, it runs.
    let output = run_isolated_in(
        root,
        &project,
        &["--yes", "trust", project.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");
    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//apps/api:build"]);
    assert!(output.status.success(), "{output:?}");
}

/// Only declared roots are read.
///
/// This guards the discovery boundary, not trust: a `run` line is an arbitrary
/// command, so finding one that nobody declared means finding code to execute that
/// nobody declared. Verified to fail by widening `expand_roots` to accept any
/// directory name.
#[test]
fn tasks_are_never_discovered_outside_the_declared_roots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api", "vendor/thirdparty"] {
        std::fs::create_dir_all(project.join(relative)).unwrap();
        std::fs::write(
            project.join(relative).join("osdk.toml"),
            "[tasks.build]\nrun = \"echo built\"\n",
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\"]\n",
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["task", "list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("//apps/api:build"), "{listed}");
    assert!(
        !listed.contains("vendor"),
        "`vendor/thirdparty` was never declared and must not be discovered: {listed}"
    );
}

/// `depends` crosses roots with `//`, and stays local without it.
///
/// Both halves were measured before being written down. The local half was a real
/// gap: a sub-project's `depends = ["prep"]` was looked up as a global name and
/// failed as `unknown task prep`, so a config that was correct on its own terms broke
/// purely by becoming a sub-project -- and the error pointed at the dependency rather
/// than at the rewrite that lost it.
#[test]
fn task_depends_crosses_roots_only_when_written_with_a_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/web")).unwrap();
    std::fs::create_dir_all(project.join("packages/ui")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\", \"packages/*\"]\n",
    )
    .unwrap();
    // `prep` exists in both sub-projects, which is what makes the local-resolution
    // assertion meaningful: with one copy, "resolved locally" and "resolved globally"
    // would look the same.
    std::fs::write(
        project.join("apps/web/osdk.toml"),
        "[tasks.prep]\nrun = \"echo web-prep\"\n\n[tasks.build]\nrun = \"echo web-built\"\ndepends = [\"//packages/ui:build\", \"prep\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("packages/ui/osdk.toml"),
        "[tasks.prep]\nrun = \"echo ui-prep\"\n\n[tasks.build]\nrun = \"echo ui-built\"\n",
    )
    .unwrap();
    let output = run_isolated_in(
        root,
        &project,
        &["--yes", "trust", project.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//apps/web:build"]);
    assert!(output.status.success(), "{output:?}");
    let planned = String::from_utf8_lossy(&output.stdout);

    // The `//`-prefixed dependency pulled in the other root's task.
    assert!(
        planned.contains("ui-built"),
        "a `//` dependency must cross roots: {planned}"
    );
    // The bare name resolved to this sub-project's own `prep`...
    assert!(
        planned.contains("web-prep"),
        "a bare dependency must resolve within its own root: {planned}"
    );
    // ...and not to the identically named task in the other one.
    assert!(
        !planned.contains("ui-prep"),
        "a bare dependency must not reach another root: {planned}"
    );
    // Dependencies before the task that declared them.
    let ui = planned.find("ui-built").expect("ui-built");
    let web = planned.find("web-built").expect("web-built");
    assert!(ui < web, "dependencies must be planned first: {planned}");
}

/// A cycle that spans two roots is still a cycle.
///
/// The existing detector works on names, so crossing roots does not exempt anything --
/// but that is worth pinning rather than assuming, because the rewrite that qualifies
/// bare names runs before the graph is built and could have produced two distinct
/// names for one task.
#[test]
fn a_cycle_across_roots_is_detected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/web")).unwrap();
    std::fs::create_dir_all(project.join("packages/ui")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\", \"packages/*\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("apps/web/osdk.toml"),
        "[tasks.build]\nrun = \"echo web\"\ndepends = [\"//packages/ui:build\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("packages/ui/osdk.toml"),
        "[tasks.build]\nrun = \"echo ui\"\ndepends = [\"//apps/web:build\"]\n",
    )
    .unwrap();
    let output = run_isolated_in(
        root,
        &project,
        &["--yes", "trust", project.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");

    let output = run_isolated_in(root, &project, &["run", "--dry-run", "//apps/web:build"]);
    assert!(
        !output.status.success(),
        "a cross-root cycle must be refused: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cycle"), "{stderr}");
    // Both ends named, so the reader can see which edge to remove.
    assert!(stderr.contains("//apps/web:build"), "{stderr}");
    assert!(stderr.contains("//packages/ui:build"), "{stderr}");
}

/// A partial pattern matches only what it names.
///
/// Separate from the test above because that one could not fail: its `apps/*` has a
/// bare `*` as its only wildcard segment, which is supposed to match every
/// directory, so replacing the matcher with `true` produced the same result and the
/// filter was never exercised. Mutation caught this -- the same way it caught the
/// identical gap on the deps side.
///
/// `api-*` makes the matcher load-bearing: two siblings match, one does not.
#[test]
fn a_partial_root_pattern_matches_only_the_named_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api-v1", "apps/api-v2", "apps/web-v1"] {
        std::fs::create_dir_all(project.join(relative)).unwrap();
        std::fs::write(
            project.join(relative).join("osdk.toml"),
            "[tasks.build]\nrun = \"echo built\"\n",
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/api-*\"]\n",
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["task", "list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    for expected in ["//apps/api-v1:build", "//apps/api-v2:build"] {
        assert!(listed.contains(expected), "missing {expected}: {listed}");
    }
    assert!(
        !listed.contains("web-v1"),
        "`apps/web-v1` does not match `apps/api-*` and must not be discovered: {listed}"
    );
}

/// A sub-project may not declare `[task_config]`.
///
/// This is the security-relevant assertion of this batch. `shell` decides which
/// interpreter every task in scope runs under, so a sub-config setting it would make
/// every later `osdk run` do something other than what the task text says, with
/// nothing at the call site to reveal it. The root's `[task_config]` is gated by
/// trust (`RedirectsExecution`) for exactly this reason; a sub-project's is refused
/// outright instead, because gating it would mean one approval per package.
///
/// Refused rather than ignored: a setting that draws no complaint and has no effect
/// leaves the author believing it worked.
///
/// Verified to fail by removing the check, which makes the sub-config load silently.
#[test]
fn a_sub_project_cannot_redirect_the_interpreter() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/api")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\"]\n\n[tasks.hello]\nrun = \"echo root\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("apps/api/osdk.toml"),
        "[task_config]\nshell = \"cmd /c echo HIJACKED &&\"\n\n[tasks.build]\nrun = \"echo built\"\n",
    )
    .unwrap();

    let output = run_isolated_in(root, &project, &["task", "list"]);
    assert!(
        !output.status.success(),
        "a sub-project declaring [task_config] must be refused: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("task_config"),
        "the error must name the offending table: {stderr}"
    );
    // The offending file, not the monorepo root: blaming the wrong file sends the
    // reader to the wrong place.
    assert!(
        stderr.contains("api"),
        "the error must name the sub-project's own config: {stderr}"
    );
}

/// A rooted name that does not exist is an error, not an empty run.
#[test]
fn an_unknown_rooted_task_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/api")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[task_config]\nroots = [\"apps/*\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("apps/api/osdk.toml"),
        "[tasks.build]\nrun = \"echo built\"\n",
    )
    .unwrap();

    // Right sub-project, wrong task name.
    let output = run_isolated_in(root, &project, &["run", "//apps/api:nope"]);
    assert!(
        !output.status.success(),
        "an unknown task must not succeed silently: {output:?}"
    );

    // Right task name, sub-project that was never declared.
    let output = run_isolated_in(root, &project, &["run", "//vendor/x:build"]);
    assert!(
        !output.status.success(),
        "an undeclared sub-project must not resolve: {output:?}"
    );
}

/// `--filter` selects sub-projects by path, and matching nothing is an error.
///
/// Selecting by location is orthogonal to selecting by provider name: one asks
/// "where", the other "what kind". Both halves are pinned here because the flag is
/// only useful if it actually narrows -- a filter that silently covered everything
/// would look like it worked.
#[test]
fn filter_selects_sub_projects_by_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api", "apps/web", "packages/ui"] {
        std::fs::create_dir_all(project.join(relative)).unwrap();
        std::fs::write(
            project.join(relative).join("package.json"),
            r#"{"name":"p","private":true}"#,
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\", \"packages/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    // Everything under apps/, regardless of package manager.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--filter", "apps/*"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    for expected in ["//apps/api:npm", "//apps/web:npm"] {
        assert!(listed.contains(expected), "missing {expected}: {listed}");
    }
    assert!(
        !listed.contains("packages/ui"),
        "`apps/*` must not select packages/: {listed}"
    );

    // A filter names a location, so it does not need `--all` to see sub-projects.
    assert!(
        !listed.contains("no matching"),
        "a filter implies the expansion it needs: {listed}"
    );

    // One exact path.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--filter", "apps/api"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("//apps/api:npm"), "{listed}");
    assert!(
        !listed.contains("//apps/web:npm"),
        "an exact path selects one sub-project: {listed}"
    );
}

/// A `--filter` that matches nothing fails.
///
/// Deliberately unlike pnpm, whose `failIfNoMatch` defaults to false. The reasoning
/// is the one `--verify` uses for `checked == 0`: "nothing was done" must not read
/// like "done, no problems". A CI step narrowed to a directory that has since been
/// renamed should fail, not pass having built nothing.
///
/// Verified to fail by removing the check, which makes the run report success.
#[test]
fn a_filter_that_matches_nothing_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    std::fs::create_dir_all(project.join("apps/api")).unwrap();
    std::fs::write(
        project.join("apps/api/package.json"),
        r#"{"name":"p","private":true}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    let output = run_isolated_in(
        root,
        &project,
        &["deps", "--list", "--filter", "services/*"],
    );
    assert!(
        !output.status.success(),
        "a filter matching nothing must fail: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("services/*"),
        "the error must quote the pattern that matched nothing: {stderr}"
    );
}

/// `--filter` patterns mean what the same text means in `roots`.
///
/// One dialect, not two. mise has `*` for declaring and `...` for addressing, and
/// pnpm still carries a `legacyDirFiltering` switch from changing its mind about
/// exactly this -- both are pure cognitive cost.
#[test]
fn filter_patterns_use_the_same_dialect_as_roots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api", "apps/group/nested"] {
        std::fs::create_dir_all(project.join(relative)).unwrap();
        std::fs::write(
            project.join(relative).join("package.json"),
            r#"{"name":"p","private":true}"#,
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\", \"apps/*/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    // A single `*` does not cross a separator, so `apps/*` excludes the nested one.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--filter", "apps/*"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("//apps/api:npm"), "{listed}");
    assert!(
        !listed.contains("group/nested"),
        "one `*` must not cross a `/`: {listed}"
    );

    // The depth has to be written out, exactly as in `roots`.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--filter", "apps/*/*"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("group/nested"), "{listed}");
    assert!(
        !listed.contains("//apps/api:npm"),
        "`apps/*/*` matches only that depth: {listed}"
    );

    // `**` is not supported, so it matches nothing and therefore fails.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--filter", "apps/**"]);
    assert!(
        !output.status.success(),
        "`**` must not quietly behave like a recursive glob: {output:?}"
    );
}

/// Listing is tiered; materializing is not.
///
/// Two halves, and the second is the one that matters. Defaulting `--list` to the
/// current config root is a readability choice -- a large monorepo's full provider
/// set scrolls the useful part away. Applying the same narrowing to materializing
/// would be a correctness bug: `osdk deps` has always covered every declared root,
/// and doing less without saying so skips work silently.
///
/// Verified to fail by making `wants_rooted` return true unconditionally (the
/// default-only assertions go red) and by making it return false for every listing
/// (the `--all` and rooted-operand assertions go red).
#[test]
fn listing_is_tiered_but_operand_free_materializing_is_not() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api", "packages/ui"] {
        let directory = project.join(relative);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("package.json"),
            r#"{"name":"p","private":true}"#,
        )
        .unwrap();
    }
    // A manifest at the config root too, so the default listing has something of
    // its own to show. Without it, "the default lists less" could not be told apart
    // from "the default lists nothing".
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"root","private":true}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\", \"packages/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    // Default: this config root only.
    let output = run_isolated_in(root, &project, &["deps", "--list"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(
        listed.contains("npm"),
        "the current config root's provider must still be listed: {listed}"
    );
    assert!(
        !listed.contains("//apps/api:npm") && !listed.contains("//packages/ui:npm"),
        "a plain --list must not expand roots: {listed}"
    );

    // `--all` widens it.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    for expected in ["//apps/api:npm", "//packages/ui:npm"] {
        assert!(
            listed.contains(expected),
            "--all must list {expected}: {listed}"
        );
    }

    // `--all` explicitly widens even a bare provider operand.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all", "npm"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("//apps/api:npm"), "{listed}");
    assert!(listed.contains("//packages/ui:npm"), "{listed}");

    // A rooted operand keeps working without `--all`: asking for a sub-project by
    // name and being told it does not exist would misreport the configuration.
    let output = run_isolated_in(root, &project, &["deps", "--list", "//apps/api:npm"]);
    assert!(output.status.success(), "{output:?}");
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(
        listed.contains("//apps/api:npm"),
        "a rooted operand must resolve without --all: {listed}"
    );
    assert_eq!(
        listed.lines().filter(|line| line.contains("stale")).count(),
        1,
        "a rooted operand must not also select the config root: {listed}"
    );

    // Materializing is not tiered. `--dry-run` prints the plan for every provider
    // it would act on, so the sub-projects must be there without `--all`.
    let output = run_isolated_in(root, &project, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let planned = String::from_utf8_lossy(&output.stdout);
    // --dry-run names the directory it would run in rather than the rooted id, so
    // the sub-project paths are what prove the expansion happened. Matching on the
    // id here would fail for a reason unrelated to the property under test.
    for expected in ["apps", "ui"] {
        assert!(
            planned.contains(expected),
            "materializing must cover the {expected} sub-project without --all: {planned}"
        );
    }
    // Three providers, not one: the config root plus both sub-projects.
    assert_eq!(
        planned.matches("would run in").count(),
        3,
        "every declared root must be planned without --all: {planned}"
    );
}

/// Provider operands are scoped: bare names select the nearest project, while
/// rooted ids select exactly one addressed project (including `//:` for root).
///
/// Counting actual dry-run plans makes this fail if an implementation merely
/// changes labels while still executing more than one root.
#[test]
fn provider_operands_select_exactly_one_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    let child = project.join("apps").join("api");
    std::fs::create_dir_all(&child).unwrap();
    for directory in [&project, &child] {
        std::fs::write(
            directory.join("package.json"),
            r#"{"name":"p","private":true}"#,
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    let assert_only = |cwd: &Path, selector: &str, expected: &Path| {
        let output = run_isolated_in(root, cwd, &["deps", selector, "--dry-run", "--force"]);
        assert!(output.status.success(), "{selector}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.matches("would run in").count(),
            1,
            "{selector} must select exactly one root: {stdout}"
        );
        assert!(
            stdout.contains(&expected.display().to_string()),
            "{selector} must select {}: {stdout}",
            expected.display()
        );
    };

    assert_only(&project, "npm", &project);
    assert_only(&child, "npm", &child);
    assert_only(&child, "//:npm", &project);
    assert_only(&project, "//apps/api:npm", &child);
}

#[test]
fn monorepo_roots_discover_only_what_was_declared() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    for relative in ["apps/api", "apps/web", "packages/ui", "vendor/thirdparty"] {
        let directory = project.join(relative);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("package.json"),
            r#"{"name":"p","private":true}"#,
        )
        .unwrap();
    }
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\", \"packages/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    // `--all` because listing now starts at the current config root; the tiering
    // itself is covered by `listing_is_tiered_but_materializing_is_not`.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    for expected in ["//apps/api:npm", "//apps/web:npm", "//packages/ui:npm"] {
        assert!(stdout.contains(expected), "missing {expected}: {stdout}");
    }
    assert!(
        !stdout.contains("vendor"),
        "`vendor/thirdparty` was never declared and must not be discovered: {stdout}"
    );
    // The originating pattern is reported, not just the path.
    assert!(stdout.contains("from root: apps/*"), "{stdout}");
    assert!(stdout.contains("from root: packages/*"), "{stdout}");

    // A rooted id addresses exactly one sub-project.
    let output = run_isolated_in(root, &project, &["deps", "--list", "//apps/api:npm"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//apps/api:npm"), "{stdout}");
    assert!(
        !stdout.contains("//apps/web:npm") && !stdout.contains("//packages/ui:npm"),
        "a rooted id must select one sub-project: {stdout}"
    );

    // Removing the declaration removes the sub-projects: nothing is remembered
    // from a previous run, and nothing is found without a pattern.
    std::fs::write(project.join("osdk.toml"), "[deps.npm]\n").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("//apps/"),
        "with no roots declared, no sub-project may be discovered: {stdout}"
    );
}

/// A broken manifest inside a declared root is an error, not a skipped package.
///
/// Being found through a root does not make the fail-closed rule weaker. Skipping
/// it would mean a monorepo installs some of its packages and reports success,
/// which is the failure mode that is hardest to notice.
#[test]
fn a_broken_manifest_inside_a_root_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("repo");
    let good = project.join("apps/api");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(good.join("package.json"), r#"{"name":"p"}"#).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[deps]\nroots = [\"apps/*\"]\n\n[deps.npm]\n",
    )
    .unwrap();

    // \--all\ because the broken sibling is only reached once roots expand; a
    // plain \--list\ stays at the config root and would never see it.
    // Control: the good one alone is fine.
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all"]);
    assert!(output.status.success(), "{output:?}");

    // A sibling whose manifest will not parse takes the whole run down.
    let bad = project.join("apps/web");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("package.json"), "{ not json").unwrap();
    let output = run_isolated_in(root, &project, &["deps", "--list", "--all"]);
    assert!(
        !output.status.success(),
        "a broken sub-project must not be silently skipped: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("package.json"), "{stderr}");
}
