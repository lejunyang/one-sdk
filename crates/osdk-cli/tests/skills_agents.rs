use std::path::Path;
use std::process::{Command, Output};

fn osdk(root: &Path, project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(args)
        .current_dir(project)
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
    let lock: toml::Value =
        toml::from_str(&std::fs::read_to_string(project.join("osdk.lock")).unwrap()).unwrap();
    lock["skills"]["demo"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent.as_str().unwrap().to_string())
        .collect()
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
