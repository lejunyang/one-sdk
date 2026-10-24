use super::*;

fn configure(root: &Path, order: &str, conflicts: &str) {
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(
        root.join("config/config.toml"),
        format!("[settings.precedence]\ntool_order={order}\ntool_conflicts='{conflicts}'\n"),
    )
    .unwrap();
}

#[cfg(unix)]
fn host_node(root: &Path) -> PathBuf {
    let bin = root.join("external tools/bin");
    write_executable(&bin.join("node"), "#!/bin/sh\nif [ \"$1\" = '--version' ]; then printf 'v22.3.0\\n'; else printf 'EXTERNAL_NODE\\n'; fi\n");
    bin
}

#[cfg(unix)]
#[test]
fn system_source_is_opt_in_and_current_does_not_probe() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    let path = bin.to_str().unwrap();
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='20'\n").unwrap();
    configure(root, "[]", "prefer");
    let run = |args: &[&str]| run_isolated_in_with_env(root, root, args, &[("PATH", path)]);
    let before = run(&["current", "node"]);
    assert!(before.status.success());
    assert!(String::from_utf8_lossy(&before.stdout).contains("node 20"));
    configure(root, "['system', 'osdk']", "prefer");
    // Discovery must not execute even a broken executable.
    write_executable(&bin.join("node"), "#!/bin/sh\nexit 71\n");
    let current = run(&["current", "node"]);
    assert!(
        current.status.success(),
        "{}",
        String::from_utf8_lossy(&current.stderr)
    );
    assert!(String::from_utf8_lossy(&current.stdout).contains("unknown; not probed"));
    let execution = run(&["exec", "-t", "node", "--", "node"]);
    assert!(!execution.status.success());
    assert!(String::from_utf8_lossy(&execution.stderr).contains("version probe failed"));
    std::fs::write(
        root.join("osdk.toml"),
        "[tools]\nnode='20'\n[settings.precedence]\ntool_order=[]\n",
    )
    .unwrap();
    let cleared = run(&["current", "node"]);
    assert!(cleared.status.success());
    assert!(String::from_utf8_lossy(&cleared.stdout).contains("node 20"));
}

#[cfg(unix)]
#[test]
fn system_source_exec_never_records_or_deletes_external_installations() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    let path = bin.to_str().unwrap();
    configure(root, "['system']", "prefer");
    let run = |args: &[&str]| run_isolated_in_with_env(root, root, args, &[("PATH", path)]);
    let execution = run(&["--offline", "exec", "-t", "node", "--", "node"]);
    assert!(
        execution.status.success(),
        "{}",
        String::from_utf8_lossy(&execution.stderr)
    );
    assert!(String::from_utf8_lossy(&execution.stdout).contains("EXTERNAL_NODE"));
    assert!(!root.join("installs/node").exists());
    assert!(!root.join("osdk.lock").exists());
    let lock = run(&["--offline", "lock", "node"]);
    assert!(!lock.status.success());
    assert!(String::from_utf8_lossy(&lock.stderr).contains("external system tool"));
    let _ = run(&["uninstall", "node@22.3.0"]);
    let clean = run(&["--yes", "cache", "clean"]);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(bin.join("node").is_file());
}

#[cfg(unix)]
#[test]
fn system_source_strict_ranges_and_explicit_managed_version() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    let path = bin.to_str().unwrap();
    configure(root, "['system']", "error");
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='22'\n").unwrap();
    let run = |args: &[&str]| run_isolated_in_with_env(root, root, args, &[("PATH", path)]);
    assert!(run(&["current", "node"]).status.success());
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='20'\n").unwrap();
    let conflict = run(&["current", "node"]);
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("incompatible system version"));
    let install = root.join("installs/node/20.0.0");
    write_executable(
        &install.join("bin/node"),
        "#!/bin/sh\nprintf 'MANAGED_NODE\\n'\n",
    );
    std::fs::write(install.join(".osdk-complete"), "").unwrap();
    let explicit = run(&["--offline", "exec", "-t", "node@20.0.0", "--", "node"]);
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("MANAGED_NODE"));
    assert!(!String::from_utf8_lossy(&explicit.stdout).contains("EXTERNAL_NODE"));
}

#[cfg(unix)]
#[test]
fn system_source_managed_pnpm_uses_external_node_without_installing_node() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    let path = bin.to_str().unwrap();
    configure(root, "['system']", "prefer");
    let manager = root.join("installs/pnpm/10.0.0");
    write_executable(&manager.join("bin/pnpm"), "#!/bin/sh\nexec node fixture\n");
    std::fs::write(manager.join(".osdk-complete"), "").unwrap();
    let output = run_isolated_in_with_env(
        root,
        root,
        &[
            "--offline",
            "exec",
            "-t",
            "pnpm@10.0.0",
            "--",
            "pnpm",
            "--version",
        ],
        &[("PATH", path)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("EXTERNAL_NODE"));
    assert!(!root.join("installs/node").exists());
}

#[test]
fn system_source_excludes_osdk_copies_hardlinks_and_relative_path() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = root.join("host");
    std::fs::create_dir_all(&bin).unwrap();
    configure(root, "['system']", "prefer");
    let node = bin.join(format!("node{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(osdk(), &node).unwrap();
    let run =
        |path: &str| run_isolated_in_with_env(root, root, &["current", "node"], &[("PATH", path)]);
    let copied = run(bin.to_str().unwrap());
    assert!(copied.status.success());
    assert!(!String::from_utf8_lossy(&copied.stdout).contains("\"status\":\"winner\""));
    std::fs::remove_file(&node).unwrap();
    let known_shim = root
        .join("data/bin")
        .join(format!("osdk-shim{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(known_shim.parent().unwrap()).unwrap();
    std::fs::copy(osdk(), &known_shim).unwrap();
    std::fs::hard_link(&known_shim, &node).unwrap();
    let linked = run(bin.to_str().unwrap());
    assert!(linked.status.success());
    assert!(!String::from_utf8_lossy(&linked.stdout).contains("\"status\":\"winner\""));
    let relative = run("host");
    assert!(relative.status.success());
    assert!(!String::from_utf8_lossy(&relative.stdout).contains("\"status\":\"winner\""));
}

#[cfg(unix)]
#[test]
fn system_source_home_precedes_path_and_python_prefix_is_verified() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    configure(root, "['system']", "prefer");
    let home = root.join("venv with spaces");
    let other = root.join("other/bin");
    write_executable(&home.join("bin/python3"), "#!/bin/sh\ncase \"$1\" in --version) echo 'Python 3.13.0';; -c) echo \"$VIRTUAL_ENV\";; *) echo \"HOME=$VIRTUAL_ENV\";; esac\n");
    write_executable(&other.join("python3"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'Python 3.12.0'; else echo PATH_PYTHON; fi\n");
    let env = [
        ("PATH", other.to_str().unwrap()),
        ("VIRTUAL_ENV", home.to_str().unwrap()),
    ];
    let output =
        run_isolated_in_with_env(root, root, &["exec", "-t", "python", "--", "python"], &env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(home.to_str().unwrap()));
    let stale = run_isolated_in_with_env(
        root,
        root,
        &["exec", "-t", "python", "--", "python"],
        &[
            ("PATH", other.to_str().unwrap()),
            ("VIRTUAL_ENV", home.to_str().unwrap()),
            ("OSDK_MANAGED_ENV", "VIRTUAL_ENV"),
            ("OSDK_ORIG_VIRTUAL_ENV_PRESENT", "0"),
        ],
    );
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(String::from_utf8_lossy(&stale.stdout).contains("PATH_PYTHON"));
    write_executable(&home.join("bin/python3"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'Python 3.13.0'; else echo /not-the-venv; fi\n");
    let invalid =
        run_isolated_in_with_env(root, root, &["exec", "-t", "python", "--", "python"], &env);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Python prefix"));
}

#[cfg(unix)]
#[test]
fn system_source_jdk_home_preserves_native_cache_and_companions() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    configure(root, "['system']", "prefer");
    let home = root.join("jdk");
    write_executable(&home.join("bin/java"), "#!/bin/sh\nif [ \"$1\" = -version ]; then echo 'openjdk version \"21.0.1\"'; else echo \"JAVA=$JAVA_HOME CACHE=$GRADLE_USER_HOME\"; fi\n");
    write_executable(&home.join("bin/javac"), "#!/bin/sh\necho COMPANION\n");
    let env = [
        ("JAVA_HOME", home.to_str().unwrap()),
        ("GRADLE_USER_HOME", "/native/cache"),
    ];
    let output = run_isolated_in_with_env(root, root, &["exec", "-t", "java", "--", "java"], &env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(home.to_str().unwrap()));
    assert!(stdout.contains("/native/cache"));
    let compiler =
        run_isolated_in_with_env(root, root, &["exec", "-t", "java", "--", "javac"], &env);
    assert!(
        compiler.status.success(),
        "{}",
        String::from_utf8_lossy(&compiler.stderr)
    );
    assert!(String::from_utf8_lossy(&compiler.stdout).contains("COMPANION"));
}

#[cfg(unix)]
#[test]
fn system_source_reshim_activation_and_clear_do_not_touch_external_files() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    configure(root, "['system']", "prefer");
    let shim = root.join("data/bin/osdk-shim");
    std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
    std::fs::copy(osdk(), &shim).unwrap();
    let env = [("PATH", bin.to_str().unwrap())];
    let resh = run_isolated_in_with_env(root, root, &["reshim"], &env);
    assert!(
        resh.status.success(),
        "{}",
        String::from_utf8_lossy(&resh.stderr)
    );
    assert!(root.join("data/shims/node").exists());
    let hook = run_isolated_in_with_env(root, root, &["hook-env", "--shell", "bash"], &env);
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let text = String::from_utf8_lossy(&hook.stdout);
    assert!(text.contains(root.join("data/shims").to_str().unwrap()));
    assert!(text.contains(bin.to_str().unwrap()));
    configure(root, "[]", "prefer");
    let cleared = run_isolated_in_with_env(root, root, &["reshim"], &env);
    assert!(cleared.status.success());
    assert!(!root.join("data/shims/node").exists());
    assert!(bin.join("node").exists());
}

#[cfg(unix)]
#[test]
fn system_source_deps_uses_external_node_and_pnpm_without_portable_receipts() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    configure(root, "['system']", "prefer");
    write_executable(&bin.join("pnpm"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 10.0.0; else node fixture; echo RUN >> calls; fi\n");
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fixture","private":true}"#,
    )
    .unwrap();
    std::fs::write(root.join("osdk.toml"), "[deps.pnpm]\n").unwrap();
    let env = [("PATH", bin.to_str().unwrap())];
    let explained = run_isolated_in_with_env(
        root,
        root,
        &["--offline", "deps", "--dry-run", "--explain"],
        &env,
    );
    assert!(
        explained.status.success(),
        "{}",
        String::from_utf8_lossy(&explained.stderr)
    );
    let explanation = String::from_utf8_lossy(&explained.stdout);
    assert!(explanation.contains("external executable; version not probed"));
    assert!(!root.join("calls").exists());
    for _ in 0..2 {
        let output = run_isolated_in_with_env(
            root,
            root,
            &["--offline", "deps", "--no-install-tools"],
            &env,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("EXTERNAL_NODE"));
    }
    assert_eq!(
        std::fs::read_to_string(root.join("calls")).unwrap(),
        "RUN\nRUN\n"
    );
    assert!(!root.join("osdk.lock").exists());
    assert!(!root.join("installs/node").exists());
    std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    let frozen = run_isolated_in_with_env(
        root,
        root,
        &["--offline", "deps", "--frozen", "--no-install-tools"],
        &env,
    );
    assert!(!frozen.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("calls")).unwrap(),
        "RUN\nRUN\n"
    );
}

#[cfg(unix)]
#[test]
fn system_source_aliases_use_the_selected_external_launcher_once() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    configure(root, "['system']", "prefer");
    for (tool, alias, prefix) in [("pnpm", "pnpx", "dlx"), ("bun", "bunx", "x")] {
        write_executable(&bin.join(tool), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 10.0.0; else printf '%s\\n' \"$*\"; fi\n");
        let output = run_isolated_in_with_env(
            root,
            root,
            &["--offline", "exec", "-t", tool, "--", alias, "example"],
            &[("PATH", bin.to_str().unwrap())],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("{prefix} example")));
    }
}

#[cfg(unix)]
#[test]
fn system_source_unavailable_falls_back_but_opaque_conflicts_fail_closed() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = host_node(root);
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='lts'\n").unwrap();
    configure(root, "['system']", "error");
    let output = run_isolated_in_with_env(
        root,
        root,
        &["current", "node"],
        &[("PATH", bin.to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unverifiable system version"));
    std::fs::remove_file(bin.join("node")).unwrap();
    let output = run_isolated_in_with_env(
        root,
        root,
        &["current", "node"],
        &[("PATH", bin.to_str().unwrap())],
    );
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("node lts"));
    assert!(text.contains("no eligible system executable"));
}

#[cfg(windows)]
#[test]
fn system_source_windows_search_is_case_insensitive_and_rejects_pathext_wrappers() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = root.join("external tools 空格");
    std::fs::create_dir_all(&bin).unwrap();
    configure(root, "['system']", "prefer");
    std::fs::write(bin.join("node.cmd"), "@echo SHOULD_NOT_EXECUTE\r\n").unwrap();
    let env = [("PATH", bin.to_str().unwrap()), ("PATHEXT", ".CMD;.EXE")];
    let absent = run_isolated_in_with_env(root, root, &["current", "node"], &env);
    assert!(absent.status.success());
    assert!(String::from_utf8_lossy(&absent.stdout).contains("no eligible system executable"));
    let command = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\windows\\system32\\cmd.exe"));
    std::fs::copy(command, bin.join("NODE.EXE")).unwrap();
    let found = run_isolated_in_with_env(root, root, &["current", "node"], &env);
    assert!(
        found.status.success(),
        "{}",
        String::from_utf8_lossy(&found.stderr)
    );
    assert!(String::from_utf8_lossy(&found.stdout).contains("unknown; not probed"));
    // A PE executable with the wrong identity must not pass a Node probe.
    let bad = run_isolated_in_with_env(root, root, &["exec", "-t", "node", "--", "node"], &env);
    assert!(!bad.status.success());
    assert!(!root.join("installs/node").exists());
}

#[cfg(unix)]
#[test]
fn system_source_remaining_adapters_probe_native_version_formats() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let bin = root.join("native/bin");
    configure(root, "['system']", "prefer");
    for (tool, version) in [
        ("go", "go version go1.24.2 linux/amd64"),
        (
            "deno",
            "deno 2.2.3 (stable, release, x86_64-unknown-linux-gnu)",
        ),
        ("zig", "0.14.0"),
        ("bun", "1.2.3"),
        ("npm", "10.1.0"),
        ("yarn", "4.2.0"),
    ] {
        write_executable(&bin.join(tool), &format!("#!/bin/sh\ncase \"$1\" in --version|version) echo '{version}' ;; *) echo 'SELECTED_{tool}' ;; esac\n"));
        write_executable(&bin.join("node"), "#!/bin/sh\necho v22.3.0\n");
        let output = run_isolated_in_with_env(
            root,
            root,
            &["--offline", "exec", "-t", tool, "--", tool, "fixture"],
            &[("PATH", bin.to_str().unwrap()), ("GOROOT", "")],
        );
        assert!(
            output.status.success(),
            "{tool}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("SELECTED_{tool}")));
    }
}

#[cfg(unix)]
#[test]
fn system_source_mixed_jvm_runtime_and_java_companions_use_the_same_jdk() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    configure(root, "['system']", "prefer");
    let bin = host_node(root);
    let jdk = root.join("jdk");
    write_executable(
        &jdk.join("bin/java"),
        "#!/bin/sh\necho 'openjdk version \"21.0.2\"' >&2\n",
    );
    write_executable(&jdk.join("bin/javac"), "#!/bin/sh\necho SELECTED_JAVAC\n");
    std::os::unix::fs::symlink(jdk.join("bin/java"), bin.join("java")).unwrap();
    write_executable(&bin.join("javac"), "#!/bin/sh\necho WRONG_JAVAC\n");
    let manager = root.join("installs/kotlin/2.4.10");
    write_executable(
        &manager.join("bin/kotlinc"),
        "#!/bin/sh\nprintf 'JAVA_HOME=%s\\n' \"$JAVA_HOME\"\n",
    );
    std::fs::write(manager.join(".osdk-complete"), "").unwrap();
    let env = [("PATH", bin.to_str().unwrap()), ("JAVA_HOME", "")];
    let mixed = run_isolated_in_with_env(
        root,
        root,
        &[
            "--offline",
            "exec",
            "-t",
            "kotlin@2.4.10",
            "-t",
            "node",
            "--",
            "kotlinc",
        ],
        &env,
    );
    assert!(
        mixed.status.success(),
        "{}",
        String::from_utf8_lossy(&mixed.stderr)
    );
    assert!(String::from_utf8_lossy(&mixed.stdout).contains(jdk.to_str().unwrap()));
    let companion = run_isolated_in_with_env(
        root,
        root,
        &["--offline", "exec", "-t", "java", "--", "javac"],
        &env,
    );
    assert!(
        companion.status.success(),
        "{}",
        String::from_utf8_lossy(&companion.stderr)
    );
    assert!(String::from_utf8_lossy(&companion.stdout).contains("SELECTED_JAVAC"));
    assert!(!String::from_utf8_lossy(&companion.stdout).contains("WRONG_JAVAC"));
}

#[test]
fn system_source_managed_lock_replay_never_probes_or_switches_to_host() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    configure(root, "['system']", "error");
    std::fs::write(root.join("osdk.toml"), "[tools]\nnode='22'\n").unwrap();
    let bin = root.join("host/bin");
    std::fs::create_dir_all(&bin).unwrap();
    #[cfg(unix)]
    write_executable(&bin.join("node"), "#!/bin/sh\nexit 71\n");
    #[cfg(windows)]
    std::fs::write(
        bin.join("node.exe"),
        b"invalid executable: must never be probed",
    )
    .unwrap();
    let lock = format!(
        "schema = 4\n[platforms.{}.tools.node]\nrequest='20'\nversion='20.0.0'\n",
        platform_key()
    );
    std::fs::write(root.join("osdk.lock"), &lock).unwrap();
    let install = root.join("installs/node/20.0.0");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".osdk-complete"), "").unwrap();
    let output = run_isolated_in_with_env(
        root,
        root,
        &["--offline", "install"],
        &[("PATH", bin.to_str().unwrap())],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(root.join("osdk.lock")).unwrap(),
        lock
    );
    assert!(!root.join("installs/node/22").exists());
}
