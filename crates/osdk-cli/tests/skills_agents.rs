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
