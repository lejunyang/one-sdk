use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

fn osdk(root: &Path, project: &Path, args: &[&str]) -> Output {
    osdk_command(root, project, args).output().unwrap()
}

fn osdk_command(root: &Path, project: &Path, args: &[&str]) -> Command {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_osdk"));
    command
        .args(args)
        .current_dir(project)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CARGO_HOME", root.join("cargo"))
        .env("RUSTUP_HOME", root.join("rustup"))
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_STORE_DIR", root.join("state/store"))
        .env("OSDK_INSTALL_DIR", root.join("state/installs"))
        .env("OSDK_TRUSTED_CONFIG_PATHS", project.join("osdk.toml"));
    command
}

fn declare(project: &Path, agents: &str) {
    // A pinned `rev:` never re-resolves, so the run stays offline; the agents
    // are what is under test.
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[skills.demo]\nsource = \"github:acme/demo\"\n\
             ref = \"rev:0123456789abcdef0123456789abcdef01234567\"\nagents = [{agents}]\n"
        ),
    )
    .unwrap();
}

fn locked_agents(project: &Path) -> Vec<String> {
    let lock = read_lock(&project.join("osdk.lock"));
    lock["skills"]["demo"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent.as_str().unwrap().to_string())
        .collect()
}

fn read_lock(path: &Path) -> toml::Value {
    toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const DEMO_SKILL: &str = "---\nname: demo\ndescription: fixture\n---\n# demo\n";
const DEMO_DECLARATION: &str = "[skills.demo]\nsource = \"./fixture/demo\"\nagents = [\"codex\"]\n";

fn local_project(root: &Path, config: &str) -> std::path::PathBuf {
    let project = root.join("project");
    let source = project.join("fixture/demo");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), DEMO_SKILL).unwrap();
    std::fs::write(project.join("osdk.toml"), config).unwrap();
    project
}

fn assert_demo_installed(project: &Path, lock: &toml::Value) {
    assert_eq!(
        std::fs::read(project.join(".agents/skills/demo/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    let demo = &lock["skills"]["demo"];
    assert_eq!(demo["source"].as_str(), Some("local:./fixture/demo"));
    assert_eq!(demo["install_mode"].as_str(), Some("copy"));
    assert_eq!(
        demo["agents"].as_array().unwrap(),
        &[toml::Value::from("codex")]
    );
    let package =
        osdk_core::skills::install::read_skill_dir(&project.join("fixture/demo")).unwrap();
    assert_eq!(
        demo["content_hash"].as_str(),
        Some(package.content_hash().as_str())
    );
}

#[test]
fn skills_sync_installs_a_declaration_with_no_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let child = project.join("child");
    std::fs::create_dir(&child).unwrap();

    let sync = osdk(root, &child, &["--yes", "skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    assert_demo_installed(&project, &read_lock(&project.join("osdk.lock")));
    assert!(!child.join("osdk.lock").exists());
}

#[test]
fn skills_sync_adds_to_a_tools_only_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &format!(
            "{DEMO_DECLARATION}\n[tools]\n\
             rust = {{ version = \"1.98.0\", components = \"rustfmt,clippy\", \
             targets = \"x86_64-pc-windows-gnu,x86_64-unknown-linux-musl\" }}\n"
        ),
    );
    let lock_path = project.join("osdk.lock");
    std::fs::write(
        &lock_path,
        "schema = 4\n\n[platforms.linux-x64.tools.rust]\n\
         request = \"1.98.0\"\nversion = \"1.98.0\"\n\n[platforms.linux-x64.tools.rust.options]\n\
         components = \"rustfmt,clippy\"\ntargets = \"x86_64-pc-windows-gnu,x86_64-unknown-linux-musl\"\n",
    )
    .unwrap();
    let before = read_lock(&lock_path);

    let sync = osdk(root, &project, &["--yes", "skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    let after = read_lock(&lock_path);
    assert_demo_installed(&project, &after);
    assert_eq!(after["platforms"], before["platforms"]);
}

#[test]
fn skills_sync_uses_the_selected_project_lock() {
    for parent_has_demo in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = local_project(root, "[skills]\n");
        let parent_lock = parent.join("osdk.lock");
        if parent_has_demo {
            std::fs::write(
                parent.join("fixture/demo/SKILL.md"),
                format!("{DEMO_SKILL}\nParent content\n"),
            )
            .unwrap();
            let add = osdk(
                root,
                &parent,
                &["--yes", "skills", "add", "./fixture/demo", "-a", "codex"],
            );
            assert!(add.status.success(), "{add:?}");
        } else {
            std::fs::write(&parent_lock, "schema = 4\n").unwrap();
        }
        let before = std::fs::read(&parent_lock).unwrap();
        let child = local_project(&parent, DEMO_DECLARATION);

        let sync = osdk(root, &child, &["--yes", "skills", "sync"]);
        assert!(sync.status.success(), "{sync:?}");
        assert_eq!(std::fs::read(&parent_lock).unwrap(), before);
        assert_demo_installed(&child, &read_lock(&child.join("osdk.lock")));
        let list = osdk(root, &child, &["skills", "list"]);
        assert!(list.status.success(), "{list:?}");
        assert!(!String::from_utf8_lossy(&list.stderr).contains("ancestor lock"));
    }
}

#[test]
fn skills_list_and_remove_find_legacy_entries_within_the_repository() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let parent = local_project(root, "[skills]\n");
    std::fs::create_dir(parent.join(".git")).unwrap();
    let middle = local_project(&parent, "[skills]\n");
    let child = local_project(&middle, "[skills]\n");
    let parent_lock = parent.join("osdk.lock");
    let middle_lock = middle.join("osdk.lock");
    let child_lock = child.join("osdk.lock");
    let add = osdk(
        root,
        &child,
        &[
            "--yes",
            "skills",
            "add",
            "./fixture/demo",
            "-a",
            "codex",
            "--copy",
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let generated = std::fs::read_to_string(&child_lock).unwrap();
    let (schema, skill_table) = generated.split_once("\n\n[skills.demo]").unwrap();
    // Older versions wrote this same entry into the nearest existing lock.
    std::fs::rename(&child_lock, &parent_lock).unwrap();
    let installed = child.join(".agents/skills/demo");
    std::fs::remove_dir_all(&installed).unwrap();

    let unrelated = osdk(root, &child, &["skills", "remove", "other"]);
    assert_eq!(unrelated.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&unrelated.stderr).contains("ancestor lock"));

    let check_recovery = || {
        let list = osdk(root, &child, &["skills", "list"]);
        let remove = osdk(root, &child, &["--yes", "skills", "remove", "demo"]);
        assert!(list.status.success(), "{list:?}");
        assert_eq!(remove.status.code(), Some(1), "{remove:?}");
        for output in [list, remove] {
            let hint = String::from_utf8_lossy(&output.stderr);
            assert!(
                hint.contains(&format!(
                    "[skills.\"demo\"] in ancestor lock `{}`",
                    parent_lock.display()
                )),
                "{hint}"
            );
            assert!(!hint.contains("Copy"), "{hint}");
            assert!(!hint.contains("is installed here"), "{hint}");
        }
        assert!(!child_lock.exists());
        assert_eq!(std::fs::read_to_string(&parent_lock).unwrap(), generated);
        assert!(!installed.exists());
    };
    check_recovery();

    // A newer release can create a closer lock unrelated to the old skill.
    std::fs::write(
        middle.join("fixture/demo/SKILL.md"),
        DEMO_SKILL.replace("name: demo", "name: middle"),
    )
    .unwrap();
    let add = osdk(
        root,
        &middle,
        &["--yes", "skills", "add", "./fixture/demo", "-a", "codex"],
    );
    assert!(add.status.success(), "{add:?}");
    let middle_before = std::fs::read(&middle_lock).unwrap();
    assert!(read_lock(&middle_lock)["skills"].get("middle").is_some());
    check_recovery();
    assert_eq!(std::fs::read(&middle_lock).unwrap(), middle_before);

    let empty_lock = format!("{schema}\n");
    std::fs::write(&child_lock, &empty_lock).unwrap();
    let remove = osdk(root, &child, &["skills", "remove", "demo"]);
    assert_eq!(remove.status.code(), Some(1), "{remove:?}");
    assert!(String::from_utf8_lossy(&remove.stderr).contains("ancestor lock"));
    assert_eq!(std::fs::read_to_string(&child_lock).unwrap(), empty_lock);

    // A Git worktree marker also stops lookup before the enclosing repository.
    std::fs::write(middle.join(".git"), "gitdir: /unused\n").unwrap();
    let list = osdk(root, &child, &["skills", "list"]);
    assert!(list.status.success(), "{list:?}");
    assert!(!String::from_utf8_lossy(&list.stderr).contains("[skills.\"demo\"]"));
    let remove = osdk(root, &child, &["skills", "remove", "demo"]);
    assert_eq!(remove.status.code(), Some(1), "{remove:?}");
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(
        stderr.contains("skill `demo` is not recorded in the lock"),
        "{stderr}"
    );
    assert!(!stderr.contains("ancestor lock"), "{stderr}");
    std::fs::remove_file(middle.join(".git")).unwrap();

    std::fs::write(
        &child_lock,
        format!("{schema}\n\n[skills.demo]{skill_table}"),
    )
    .unwrap();
    let list = osdk(root, &child, &["skills", "list"]);
    assert!(list.status.success(), "{list:?}");
    assert!(!String::from_utf8_lossy(&list.stderr).contains("[skills.\"demo\"]"));
    let remove = osdk(root, &child, &["skills", "remove", "demo"]);
    assert!(remove.status.success(), "{remove:?}");
    assert!(!installed.exists());
    assert_eq!(std::fs::read_to_string(&parent_lock).unwrap(), generated);
}

#[test]
fn skills_list_and_remove_report_local_unlocked_skills_before_ancestor_entries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let parent = local_project(root, "[skills]\n");
    std::fs::create_dir(parent.join(".git")).unwrap();
    std::fs::write(
        parent.join("fixture/demo/SKILL.md"),
        format!("{DEMO_SKILL}\nParent content\n"),
    )
    .unwrap();
    let add_args = [
        "--yes",
        "skills",
        "add",
        "./fixture/demo",
        "-a",
        "codex",
        "--copy",
    ];
    let add = osdk(root, &parent, &add_args);
    assert!(add.status.success(), "{add:?}");
    let parent_lock = parent.join("osdk.lock");
    let parent_before = std::fs::read(&parent_lock).unwrap();
    let child = local_project(&parent, "[skills]\n");
    let add = osdk(root, &child, &add_args);
    assert!(add.status.success(), "{add:?}");
    let child_lock = child.join("osdk.lock");
    assert_ne!(
        read_lock(&child_lock)["skills"]["demo"]["content_hash"],
        read_lock(&parent_lock)["skills"]["demo"]["content_hash"]
    );
    std::fs::remove_file(&child_lock).unwrap();
    let installed = child.join(".agents/skills/demo");

    let list = osdk(root, &child, &["skills", "list"]);
    let remove = osdk(root, &child, &["skills", "remove", "demo"]);
    assert!(list.status.success(), "{list:?}");
    assert_eq!(remove.status.code(), Some(1), "{remove:?}");
    for output in [&list.stdout, &remove.stderr] {
        let message = String::from_utf8_lossy(output);
        assert!(
            message.contains("skill `demo` is not recorded in the lock"),
            "{message}"
        );
        assert!(
            message.contains(&installed.display().to_string()),
            "{message}"
        );
    }
    for output in [&list.stdout, &list.stderr, &remove.stderr] {
        assert!(!String::from_utf8_lossy(output).contains("ancestor lock"));
    }
    assert_eq!(
        std::fs::read(installed.join("SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    assert_eq!(std::fs::read(parent_lock).unwrap(), parent_before);
    assert!(!child_lock.exists());
}

#[test]
fn skills_sync_resolves_an_inherited_source_from_its_declaring_config() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let parent = local_project(
        root,
        "[skills.demo]\nsource = \"./fixture/demo\"\nagents = [\"claude-code\"]\n",
    );
    let child = local_project(
        &parent,
        "extends = \"../osdk.toml\"\n\n[skills.demo]\nagents = [\"codex\"]\n",
    );
    std::fs::write(
        child.join("fixture/demo/SKILL.md"),
        format!("{DEMO_SKILL}\nDifferent child content\n"),
    )
    .unwrap();

    let sync = osdk(root, &child, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    assert_eq!(
        std::fs::read(child.join(".agents/skills/demo/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    let lock = read_lock(&child.join("osdk.lock"));
    let demo = &lock["skills"]["demo"];
    assert_eq!(demo["source"].as_str(), Some("local:./fixture/demo"));
    assert_eq!(locked_agents(&child), ["codex"]);
    let package = osdk_core::skills::install::read_skill_dir(&parent.join("fixture/demo")).unwrap();
    assert_eq!(
        demo["content_hash"].as_str(),
        Some(package.content_hash().as_str())
    );
}

#[test]
fn skills_sync_confirms_before_replacing_unlocked_contents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let installed = project.join(".agents/skills/demo");
    let manifest = installed.join("SKILL.md");
    let notes = installed.join("notes.txt");
    let old_manifest = format!("{DEMO_SKILL}\nLocal edits\n");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(&manifest, &old_manifest).unwrap();
    std::fs::write(&notes, "Keep my notes").unwrap();

    let sync = osdk(root, &project, &["--lang", "en", "skills", "sync"]);
    assert_eq!(sync.status.code(), Some(1), "{sync:?}");
    let stderr = String::from_utf8_lossy(&sync.stderr);
    for expected in ["confirmation required", "--yes", "move"] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), old_manifest);
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "Keep my notes");
    let lock = project.join("osdk.lock");
    assert!(!lock.exists());

    let confirmed = osdk(root, &project, &["--yes", "skills", "sync"]);
    assert!(confirmed.status.success(), "{confirmed:?}");
    assert_demo_installed(&project, &read_lock(&lock));
    assert!(!notes.exists());

    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&notes, "Still useful").unwrap();
    let extra_file = osdk(root, &project, &["skills", "sync"]);
    assert!(!extra_file.status.success(), "{extra_file:?}");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "Still useful");
    assert!(!lock.exists());

    std::fs::remove_file(&notes).unwrap();
    let identical = osdk(root, &project, &["skills", "sync"]);
    assert_eq!(identical.status.code(), Some(1), "{identical:?}");
    assert_eq!(std::fs::read(&manifest).unwrap(), DEMO_SKILL.as_bytes());
    assert!(!lock.exists());
}

#[cfg(unix)]
#[test]
fn skills_sync_preserves_unlocked_executable_scripts_without_confirmation() {
    use std::fs::File;
    use std::io::{IsTerminal, Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::time::Instant;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let source = project.join("fixture/demo/run.sh");
    std::fs::write(&source, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
    let add = osdk(
        root,
        &project,
        &[
            "--yes",
            "skills",
            "add",
            "./fixture/demo",
            "-a",
            "codex",
            "--copy",
            "--no-lock",
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let lock = project.join("osdk.lock");
    assert!(!lock.exists());
    let installed = project.join(".agents/skills/demo");
    let script = installed.join("run.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        std::fs::read(&script).unwrap(),
        std::fs::read(&source).unwrap()
    );

    let assert_preserved = || {
        assert_eq!(
            std::fs::read(&script).unwrap(),
            std::fs::read(&source).unwrap()
        );
        assert_eq!(
            script.metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read(installed.join("SKILL.md")).unwrap(),
            DEMO_SKILL.as_bytes()
        );
        assert!(!lock.exists());
    };
    let sync = osdk_command(root, &project, &["skills", "sync"])
        .env_remove("CI")
        .output()
        .unwrap();
    assert_eq!(sync.status.code(), Some(1), "{sync:?}");
    assert_preserved();

    let run_in_terminal = |ci: &str, args: &[&str], answer: Option<&[u8]>| {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let opened = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        assert!(slave.is_terminal());
        assert_eq!(
            unsafe { libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let mut command = osdk_command(root, &project, args);
        command
            .env("CI", ci)
            .env("TERM", "xterm")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        let mut child = command.spawn().unwrap();
        if let Some(answer) = answer {
            master.write_all(answer).unwrap();
        }
        let mut stderr = Vec::new();
        let mut drain_terminal = || {
            // Keep a slave handle open until capture is complete, including on macOS.
            if let Err(error) = master.read_to_end(&mut stderr) {
                assert_eq!(error.kind(), ErrorKind::WouldBlock, "{error}");
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            drain_terminal();
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "sync waited for input with CI={ci}: {}",
                    String::from_utf8_lossy(&stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drain_terminal();
        Output {
            stderr,
            ..child.wait_with_output().unwrap()
        }
    };
    let sync_args = ["--lang", "en", "skills", "sync"];
    let ci_sync = run_in_terminal("woodpecker", &sync_args, None);
    assert_eq!(ci_sync.status.code(), Some(1), "{ci_sync:?}");
    let stderr = String::from_utf8_lossy(&ci_sync.stderr);
    for expected in ["confirmation required", "--yes", "move"] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert_preserved();

    let cancelled = run_in_terminal("false", &sync_args, Some(b"n\n"));
    assert!(cancelled.status.success(), "{cancelled:?}");
    assert!(String::from_utf8_lossy(&cancelled.stderr)
        .contains("Replace existing unlocked skill files?"));
    assert!(String::from_utf8_lossy(&cancelled.stdout).contains("Installation cancelled."));
    assert_preserved();

    // The shared Codex directory alone does not identify a detected Agent.
    let config = project.join("osdk.toml");
    std::fs::write(&config, "[skills.demo]\nsource = \"./fixture/demo\"\n").unwrap();
    let unresolved = run_in_terminal("woodpecker", &sync_args, None);
    assert_eq!(unresolved.status.code(), Some(1), "{unresolved:?}");
    let stderr = String::from_utf8_lossy(&unresolved.stderr);
    for expected in [
        "non-interactive",
        "[skills.demo]",
        "`agents`",
        "[skills].default_agents",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(!stderr.contains("Select the agents"), "{stderr}");
    assert!(!stderr.contains("-a/--agent"), "{stderr}");
    assert_preserved();
    std::fs::write(config, DEMO_DECLARATION).unwrap();

    let confirmed = run_in_terminal("woodpecker", &["--yes", "skills", "sync"], None);
    assert!(confirmed.status.success(), "{confirmed:?}");
    assert_demo_installed(&project, &read_lock(&lock));
}

#[test]
fn skills_sync_replays_cached_bytes_and_ignores_changed_declaration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let add = osdk(
        root,
        &project,
        &[
            "--yes",
            "skills",
            "add",
            "./fixture/demo",
            "-a",
            "codex",
            "--copy",
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let lock = project.join("osdk.lock");
    let before = std::fs::read(&lock).unwrap();
    std::fs::remove_dir_all(project.join(".agents/skills")).unwrap();
    std::fs::remove_dir_all(project.join("fixture")).unwrap();
    std::fs::write(
        project.join("osdk.toml"),
        "[skills.demo]\nsource = \"./missing\"\nskill = \"other\"\n\
         ref = \"branch:next\"\nagents = [\"claude-code\"]\n",
    )
    .unwrap();

    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    let stdout = String::from_utf8_lossy(&sync.stdout);
    assert!(stdout.contains("osdk skills remove demo"), "{stdout}");
    assert!(stdout.contains("osdk skills update demo"), "{stdout}");
    assert_eq!(
        std::fs::read(project.join(".agents/skills/demo/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    assert!(!project.join(".claude/skills/demo").exists());
    assert_eq!(std::fs::read(lock).unwrap(), before);
}

#[test]
fn skills_sync_filters_platforms_and_uses_default_agents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let current = std::env::consts::OS;
    let other = if cfg!(windows) { "linux" } else { "windows" };
    let project = local_project(
        root,
        &format!(
            "[skills]\ndefault_agents = [\"codex\"]\n\n\
             [skills.alias]\nsource = \"./fixture\"\nskill = \"demo\"\nwhen = {{ os = [\"{current}\"] }}\n\n\
             [skills.skipped]\nsource = \"./missing\"\nwhen = {{ os = [\"{other}\"] }}\n"
        ),
    );
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    let stdout = String::from_utf8_lossy(&sync.stdout);
    assert!(
        stdout.contains("skipping declaration `skipped`"),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("os={other}")), "{stdout}");
    let installed = project.join(".agents/skills/alias/SKILL.md");
    assert_eq!(std::fs::read(&installed).unwrap(), DEMO_SKILL.as_bytes());
    let lock_path = project.join("osdk.lock");
    let before = std::fs::read(&lock_path).unwrap();
    let lock = read_lock(&lock_path);
    assert_eq!(lock["skills"]["alias"]["skill"].as_str(), Some("demo"));
    assert!(lock["skills"].get("skipped").is_none());

    std::fs::remove_file(&installed).unwrap();
    let replay = osdk(root, &project, &["skills", "sync"]);
    assert!(replay.status.success(), "{replay:?}");
    assert_eq!(std::fs::read(installed).unwrap(), DEMO_SKILL.as_bytes());
    assert_eq!(std::fs::read(lock_path).unwrap(), before);
}

#[test]
fn skills_sync_skips_replaying_a_locked_skill_on_another_platform() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    let lock = project.join("osdk.lock");
    let before = std::fs::read(&lock).unwrap();
    assert_demo_installed(&project, &read_lock(&lock));
    std::fs::remove_dir_all(project.join(".agents/skills/demo")).unwrap();

    let other = if cfg!(windows) { "linux" } else { "windows" };
    std::fs::write(
        project.join("osdk.toml"),
        format!("{DEMO_DECLARATION}when = {{ os = [\"{other}\"] }}\n"),
    )
    .unwrap();

    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    assert!(!project.join(".agents/skills/demo").exists());
    assert_eq!(std::fs::read(lock).unwrap(), before);
    let stdout = String::from_utf8_lossy(&sync.stdout);
    assert!(stdout.contains("skipping locked skill `demo`"), "{stdout}");
    assert!(stdout.contains("keeping its lock entry"), "{stdout}");
    assert!(stdout.contains(&format!("os={other}")), "{stdout}");
}

#[test]
fn skills_update_respects_platform_filters() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &format!(
            "{DEMO_DECLARATION}\n[skills.secondary]\nsource = \"./fixture/demo\"\n\
             skill = \"demo\"\nagents = [\"codex\"]\n"
        ),
    );
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    let lock_path = project.join("osdk.lock");
    let before = std::fs::read(&lock_path).unwrap();
    let original = read_lock(&lock_path);
    assert_demo_installed(&project, &original);

    let current = std::env::consts::OS;
    let other = if cfg!(windows) { "linux" } else { "windows" };
    std::fs::write(
        project.join("osdk.toml"),
        format!(
            "[skills.demo]\nsource = \"./fixture/demo\"\nagents = [\"codex\", \"claude-code\"]\n\
             when = {{ os = [\"{other}\"] }}\n\n\
             [skills.secondary]\nsource = \"./fixture/demo\"\nskill = \"demo\"\n\
             agents = [\"codex\", \"claude-code\"]\nwhen = {{ os = [\"{current}\"] }}\n"
        ),
    )
    .unwrap();

    for args in [
        &["skills", "update", "demo"][..],
        &["skills", "update", "secondary", "demo"][..],
    ] {
        let named = osdk(root, &project, args);
        assert_eq!(named.status.code(), Some(1), "{named:?}");
        let stderr = String::from_utf8_lossy(&named.stderr);
        assert!(
            stderr.contains(&format!(
                "skill `demo` is not available on this platform (os={other})"
            )),
            "{stderr}"
        );
        assert!(!project.join(".claude/skills/demo").exists());
        assert!(!project.join(".claude/skills/secondary").exists());
        assert_eq!(std::fs::read(&lock_path).unwrap(), before);
    }

    let bulk = osdk(root, &project, &["skills", "update"]);
    assert!(bulk.status.success(), "{bulk:?}");
    let stdout = String::from_utf8_lossy(&bulk.stdout);
    for expected in [
        "skipping locked skill `demo`",
        &format!("os={other}"),
        "keeping its lock entry",
    ] {
        assert!(stdout.contains(expected), "{stdout}");
    }
    assert!(!project.join(".claude/skills/demo").exists());
    let updated = read_lock(&lock_path);
    assert_eq!(updated["skills"]["demo"], original["skills"]["demo"]);
    assert_demo_installed(&project, &updated);
    assert_eq!(
        std::fs::read(project.join(".claude/skills/secondary/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    assert_eq!(
        updated["skills"]["secondary"]["agents"].as_array().unwrap(),
        &[toml::Value::from("codex"), toml::Value::from("claude-code")]
    );

    std::fs::remove_file(&lock_path).unwrap();
    let named = osdk(root, &project, &["skills", "update", "demo"]);
    assert_eq!(named.status.code(), Some(1), "{named:?}");
    let stderr = String::from_utf8_lossy(&named.stderr);
    assert!(stderr.contains(&format!("os={other}")), "{stderr}");
    assert!(!lock_path.exists());
}

#[test]
fn skills_sync_reports_missing_agents_in_a_non_interactive_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, "[skills.demo]\nsource = \"./fixture/demo\"\n");
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert_eq!(sync.status.code(), Some(1), "{sync:?}");
    let stderr = String::from_utf8_lossy(&sync.stderr);
    for expected in [
        "[skills.demo]",
        "agents",
        "[skills].default_agents",
        "--yes",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(!project.join(".agents/skills").exists());
    assert!(!project.join("osdk.lock").exists());
}

#[test]
fn skills_sync_rejects_an_unimplemented_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &format!("{DEMO_DECLARATION}endpoint = \"https://example.invalid\"\n"),
    );
    let sync = osdk(root, &project, &["--yes", "skills", "sync"]);
    assert!(!sync.status.success(), "{sync:?}");
    let stderr = String::from_utf8_lossy(&sync.stderr);
    assert!(stderr.contains("[skills.demo].endpoint"), "{stderr}");
    assert!(!project.join(".agents/skills").exists());
    assert!(!project.join("osdk.lock").exists());
}

#[test]
fn skills_commands_use_the_configured_global_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let global_config = format!("[skills]\nscope = \"global\"\n\n{DEMO_DECLARATION}");
    let project = local_project(root, &global_config);
    let installed = root.join("home/.codex/skills/demo/SKILL.md");
    let user_lock = root.join("state/config/osdk.lock");

    let add = osdk(
        root,
        &project,
        &["--yes", "skills", "add", "./fixture/demo", "-a", "codex"],
    );
    assert!(add.status.success(), "{add:?}");
    assert_eq!(std::fs::read(&installed).unwrap(), DEMO_SKILL.as_bytes());
    assert!(read_lock(&user_lock)["skills"].get("demo").is_some());
    assert!(!project.join("osdk.lock").exists());
    assert!(!project.join(".agents/skills").exists());

    for (args, message) in [
        (&["skills", "list"][..], "demo  <- local:./fixture/demo"),
        (&["skills", "update", "demo"][..], "local source"),
    ] {
        let output = osdk(root, &project, args);
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(message));
    }
    let config_path = project.join("osdk.toml");
    std::fs::write(&config_path, "[skills]\nscope = \"project\"\n").unwrap();
    let local_bytes = format!("{DEMO_SKILL}\nProject copy\n");
    std::fs::write(project.join("fixture/demo/SKILL.md"), &local_bytes).unwrap();
    let project_add = osdk(
        root,
        &project,
        &["--yes", "skills", "add", "./fixture/demo", "-a", "codex"],
    );
    assert!(project_add.status.success(), "{project_add:?}");
    std::fs::write(config_path, global_config).unwrap();

    let path = osdk(root, &project, &["skills", "path", "demo"]);
    assert!(path.status.success(), "{path:?}");
    assert_eq!(
        std::fs::read(Path::new(String::from_utf8_lossy(&path.stdout).trim()).join("SKILL.md"))
            .unwrap(),
        DEMO_SKILL.as_bytes()
    );

    std::fs::remove_file(&installed).unwrap();
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    assert_eq!(std::fs::read(&installed).unwrap(), DEMO_SKILL.as_bytes());
    let remove = osdk(root, &project, &["skills", "remove", "demo"]);
    assert!(remove.status.success(), "{remove:?}");
    assert!(!installed.exists());
    assert!(read_lock(&user_lock).get("skills").is_none());
    assert_eq!(
        std::fs::read(project.join(".agents/skills/demo/SKILL.md")).unwrap(),
        local_bytes.as_bytes()
    );
}

#[test]
fn skills_global_flag_overrides_project_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &format!("[skills]\nscope = \"project\"\n\n{DEMO_DECLARATION}"),
    );
    let sync = osdk(root, &project, &["skills", "sync", "-g"]);
    assert!(sync.status.success(), "{sync:?}");
    assert_eq!(
        std::fs::read(root.join("home/.codex/skills/demo/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    assert!(read_lock(&root.join("state/config/osdk.lock"))["skills"]
        .get("demo")
        .is_some());
    assert!(!project.join("osdk.lock").exists());
}

#[test]
fn skills_rejects_invalid_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &format!("[skills]\nscope = \"invalid\"\n\n{DEMO_DECLARATION}"),
    );
    let sync = osdk(root, &project, &["skills", "sync", "-g"]);
    assert!(!sync.status.success(), "{sync:?}");
    let stderr = String::from_utf8_lossy(&sync.stderr);
    for name in ["[skills].scope", "invalid", "project", "global"] {
        assert!(stderr.contains(name), "{stderr}");
    }
    assert!(!project.join("osdk.lock").exists());
    assert!(!root.join("state/config/osdk.lock").exists());
}

#[test]
fn skills_update_resolves_a_changed_pin_before_writing_the_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, "");
    let old = "0123456789abcdef0123456789abcdef01234567";
    let new = "abcdef0123456789abcdef0123456789abcdef01";
    let config = project.join("osdk.toml");
    let lock = project.join("osdk.lock");
    let locked = format!(
        "schema = 4\n\n[skills.demo]\nsource = \"github:acme/demo\"\n\
         content_hash = \"b3-v2:old\"\nrequested_ref = \"rev:{old}\"\n\
         resolved_commit = \"{old}\"\ninstall_mode = \"copy\"\nagents = [\"codex\", \"claude-code\"]\n"
    );
    std::fs::write(&lock, &locked).unwrap();
    let installed = [
        project.join(".agents/skills/demo/SKILL.md"),
        project.join(".claude/skills/demo/SKILL.md"),
    ];
    for path in &installed {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, DEMO_SKILL).unwrap();
    }
    let declaration = |commit, agents| {
        format!(
            "[skills.demo]\nsource = \"github:acme/demo\"\n\
             ref = \"rev:{commit}\"\nagents = [{agents}]\n"
        )
    };
    let args = ["--offline", "skills", "update", "demo"];

    std::fs::write(&config, declaration(old, "\"codex\", \"claude-code\"")).unwrap();
    let unchanged = osdk(root, &project, &args);
    assert!(unchanged.status.success(), "{unchanged:?}");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), locked);

    std::fs::write(&config, declaration(new, "\"claude-code\"")).unwrap();
    let changed = osdk(root, &project, &args);
    assert!(!changed.status.success(), "{changed:?}");
    let stderr = String::from_utf8_lossy(&changed.stderr);
    assert!(stderr.contains("offline metadata cache miss"), "{stderr}");
    assert!(stderr.contains(new), "{stderr}");
    let assert_original_installation = || {
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), locked);
        for path in &installed {
            assert_eq!(std::fs::read(path).unwrap(), DEMO_SKILL.as_bytes());
        }
    };
    assert_original_installation();

    // Warm only the commit lookup, leaving the new archive and staged tree absent.
    let url = root.join("commit-url");
    std::fs::write(
        &url,
        format!("https://api.github.com/repos/acme/demo/commits/{new}"),
    )
    .unwrap();
    let cache_key = osdk_core::store::hash_file(&url).unwrap();
    let metadata_cache = root.join("state/cache/remote/http");
    std::fs::create_dir_all(&metadata_cache).unwrap();
    std::fs::write(
        metadata_cache.join(cache_key),
        format!(r#"{{"sha":"{new}"}}"#),
    )
    .unwrap();

    // An archive request would open a connection to this proxy. Close any
    // connection immediately so the regression test never reaches GitHub.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (done, running) = mpsc::channel::<()>();
    let proxy = std::thread::spawn(move || {
        let mut connections = 0;
        loop {
            match listener.accept() {
                Ok(_) => connections += 1,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if running.recv_timeout(Duration::from_millis(10))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        return connections;
                    }
                }
                Err(error) => panic!("accepting proxy connection: {error}"),
            }
        }
    });
    let cached_update = osdk_command(root, &project, &args)
        .env_remove("https_proxy")
        .env_remove("no_proxy")
        .env_remove("NO_PROXY")
        .env("HTTPS_PROXY", proxy_url)
        .output()
        .unwrap();
    drop(done);
    let connections = proxy.join().unwrap();
    assert_original_installation();
    assert_eq!(connections, 0, "offline update contacted the proxy");
    assert!(!cached_update.status.success(), "{cached_update:?}");
    let stderr = String::from_utf8_lossy(&cached_update.stderr);
    assert!(
        stderr.contains("offline skill archive cache miss"),
        "{stderr}"
    );
    assert!(stderr.contains(&format!("acme/demo@{new}")), "{stderr}");
}

#[test]
fn skills_update_keeps_an_unchanged_short_pin_offline() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        "[skills.demo]\nsource = \"github:acme/demo\"\nref = \"rev:0123456\"\nagents = [\"codex\"]\n",
    );
    let lock = project.join("osdk.lock");
    let locked = "schema = 4\n\n[skills.demo]\nsource = \"github:acme/demo\"\n\
        content_hash = \"b3-v2:old\"\nrequested_ref = \"rev:0123456\"\n\
        resolved_commit = \"0123456789abcdef0123456789abcdef01234567\"\n\
        install_mode = \"copy\"\nagents = [\"codex\"]\n";
    std::fs::write(&lock, locked).unwrap();

    let update = osdk(root, &project, &["--offline", "skills", "update", "demo"]);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(std::fs::read_to_string(lock).unwrap(), locked);
}

#[test]
fn skills_update_leaves_existing_contents_and_lock_when_a_target_is_blocked() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");

    let codex = project.join(".agents/skills/demo/SKILL.md");
    let lock = project.join("osdk.lock");
    let before = std::fs::read(&lock).unwrap();
    let edited = format!("{DEMO_SKILL}\nLocal edit\n");
    std::fs::write(&codex, &edited).unwrap();
    let blocker = project.join(".claude/skills");
    std::fs::create_dir(blocker.parent().unwrap()).unwrap();
    std::fs::write(&blocker, b"blocked").unwrap();
    // Cursor uses the same path as Codex, which sorts before the blocked path.
    std::fs::write(
        project.join("osdk.toml"),
        DEMO_DECLARATION.replace("\"codex\"", "\"codex\", \"cursor\", \"claude-code\""),
    )
    .unwrap();

    let update = osdk(root, &project, &["--offline", "skills", "update", "demo"]);
    assert!(!update.status.success(), "{update:?}");
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(
        stderr.contains(".claude") && stderr.contains("skills"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&codex).unwrap(), edited.as_bytes());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"blocked");
    assert_eq!(std::fs::read(&lock).unwrap(), before);
}

#[test]
fn skills_update_drops_an_agent_whose_skills_directory_is_now_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(
        root,
        &DEMO_DECLARATION.replace("\"codex\"", "\"codex\", \"claude-code\""),
    );
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
    assert_eq!(locked_agents(&project), ["codex", "claude-code"]);

    let codex = project.join(".agents/skills/demo/SKILL.md");
    let claude = project.join(".claude/skills");
    assert_eq!(std::fs::read(&codex).unwrap(), DEMO_SKILL.as_bytes());
    assert_eq!(
        std::fs::read(claude.join("demo/SKILL.md")).unwrap(),
        DEMO_SKILL.as_bytes()
    );
    std::fs::remove_dir_all(&claude).unwrap();
    std::fs::write(&claude, b"replacement file\n").unwrap();
    std::fs::write(project.join("osdk.toml"), DEMO_DECLARATION).unwrap();

    let update = osdk(root, &project, &["--offline", "skills", "update", "demo"]);
    assert!(
        update.status.success(),
        "{}: {}",
        update.status,
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(locked_agents(&project), ["codex"]);
    assert_eq!(std::fs::read(&codex).unwrap(), DEMO_SKILL.as_bytes());
    assert_eq!(std::fs::read(&claude).unwrap(), b"replacement file\n");

    let replay = osdk(root, &project, &["--offline", "skills", "sync"]);
    assert!(replay.status.success(), "{replay:?}");
    assert_eq!(std::fs::read(codex).unwrap(), DEMO_SKILL.as_bytes());
    assert_eq!(std::fs::read(claude).unwrap(), b"replacement file\n");
}

/// Editing a skill's `agents` in osdk.toml must reach the lock and the disk on
/// `skills update`. It used to re-resolve only the source, so an agent removed
/// from the declaration stayed installed and recorded.
#[test]
fn skills_update_follows_the_declared_agents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = root.join("project");
    let source = root.join("source");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(source.join("demo")).unwrap();
    std::fs::write(
        source.join("demo/SKILL.md"),
        "---\nname: demo\ndescription: fixture\n---\n# demo\n",
    )
    .unwrap();
    std::fs::write(project.join("osdk.toml"), "").unwrap();

    let source_arg = source.to_string_lossy().into_owned();
    let add = osdk(
        root,
        &project,
        &[
            "skills",
            "add",
            &source_arg,
            "-a",
            "universal",
            "-a",
            "claude-code",
            "--yes",
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let claude = project.join(".claude/skills/demo/SKILL.md");
    let shared = project.join(".agents/skills/demo/SKILL.md");
    assert!(claude.is_file() && shared.is_file());

    declare(&project, "\"universal\"");
    let update = osdk(root, &project, &["skills", "update"]);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(locked_agents(&project), ["universal"]);
    assert!(!claude.exists(), "dropped agent must be unlinked");
    assert!(shared.is_file(), "the agent still declared keeps its copy");

    declare(&project, "\"universal\", \"claude-code\"");
    let update = osdk(root, &project, &["skills", "update"]);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(locked_agents(&project), ["universal", "claude-code"]);
    assert!(
        claude.is_file(),
        "a re-added agent gets the locked copy back"
    );

    // Nothing left to reconcile: the lock is not rewritten.
    let before = std::fs::read_to_string(project.join("osdk.lock")).unwrap();
    let update = osdk(root, &project, &["skills", "update"]);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(
        std::fs::read_to_string(project.join("osdk.lock")).unwrap(),
        before
    );
}
