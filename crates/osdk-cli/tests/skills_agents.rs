use std::path::Path;
use std::process::{Command, Output};

fn osdk(root: &Path, project: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_osdk"))
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
        .env("OSDK_TRUSTED_CONFIG_PATHS", project.join("osdk.toml"))
        .output()
        .unwrap()
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
    }
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
    assert!(!sync.status.success(), "{sync:?}");
    assert!(String::from_utf8_lossy(&sync.stderr).contains("confirmation required"));
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
    assert!(identical.status.success(), "{identical:?}");
    assert_demo_installed(&project, &read_lock(&lock));
}

#[test]
fn skills_sync_adopts_an_identical_unlocked_skill_added_by_absolute_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, DEMO_DECLARATION);
    let absolute = project.join("fixture/demo");
    let add = osdk(
        root,
        &project,
        &[
            "--yes",
            "skills",
            "add",
            absolute.to_str().unwrap(),
            "-a",
            "codex",
            "--no-lock",
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let lock = project.join("osdk.lock");
    assert!(!lock.exists());
    let manifest =
        osdk_core::store::manifest::Manifest::load(&project.join(".agents/skills/demo")).unwrap();
    assert_ne!(manifest.tool, "local:./fixture/demo");

    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(sync.status.success(), "{sync:?}");
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
}

#[test]
fn skills_sync_reports_missing_agents_in_a_non_interactive_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let project = local_project(root, "[skills.demo]\nsource = \"./fixture/demo\"\n");
    let sync = osdk(root, &project, &["skills", "sync"]);
    assert!(!sync.status.success(), "{sync:?}");
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
         resolved_commit = \"{old}\"\ninstall_mode = \"copy\"\nagents = [\"codex\"]\n"
    );
    std::fs::write(&lock, &locked).unwrap();
    let declaration = |commit| {
        format!(
            "[skills.demo]\nsource = \"github:acme/demo\"\n\
             ref = \"rev:{commit}\"\nagents = [\"codex\"]\n"
        )
    };
    let args = ["--offline", "skills", "update", "demo"];

    std::fs::write(&config, declaration(old)).unwrap();
    let unchanged = osdk(root, &project, &args);
    assert!(unchanged.status.success(), "{unchanged:?}");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), locked);

    std::fs::write(&config, declaration(new)).unwrap();
    let changed = osdk(root, &project, &args);
    assert!(!changed.status.success(), "{changed:?}");
    let stderr = String::from_utf8_lossy(&changed.stderr);
    assert!(stderr.contains("offline metadata cache miss"), "{stderr}");
    assert!(stderr.contains(new), "{stderr}");
    assert_eq!(std::fs::read_to_string(lock).unwrap(), locked);
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
