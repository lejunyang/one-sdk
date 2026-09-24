//! Agent Skill management: staging `SKILL.md` packages and linking them into
//! the skill directories that external AI coding agents read.
//!
//! This module is the counterpart to [`crate::model`] for a different asset
//! class. A *skill* is a directory containing a `SKILL.md` (YAML frontmatter
//! with `name` + `description`) plus any supporting files. osdk does not load a
//! skill itself -- that is what `plugins` is for; a skill is content osdk stages
//! under `<data>/skills/<id>/<hash>/` once and then links (or copies) into each
//! target agent's own `skills/` directory.
//!
//! Install-gated with the same reasoning as `deps`/`tasks`: the shim dispatches
//! an already-installed tool and never touches a skill, so none of this belongs
//! in its binary.
//!
//! # What lives here (P0-1)
//!
//! - [`AgentTarget`]: the static table mapping a `--agent` id to the project and
//!   global `skills/` directories that agent reads. Adding an agent is adding a
//!   row, never a `match` arm, matching how the rest of the crate grows
//!   ([`crate::tool::DYNAMIC_NAMESPACES`], the backend registry).
//! - [`SkillSource`]: parsing a source operand (`github:owner/repo`, a GitHub
//!   URL, or a local path) into a normalized shape, plus its stable id.
//! - [`content_hash`]: a deterministic BLAKE3 digest over a skill directory's
//!   file tree, used as the immutable identity written to `osdk.lock`.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub mod fetch;
pub mod install;
pub mod search;

/// One agent osdk knows how to install a skill into.
///
/// `project` is relative to a project root; `global` is relative to the user's
/// home directory. Both are the directory the agent scans for skills. A `None`
/// `global` marks a project-only agent (it has no user-level skill directory).
///
/// The paths are a snapshot of the `npx skills` "Supported Agents" table and can
/// drift as an agent changes its own layout, so the table is the single place to
/// correct them and callers must never hard-code a path of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentTarget {
    /// The `--agent` selector, e.g. `claude-code`.
    pub id: &'static str,
    /// Human-readable name for listings.
    pub name: &'static str,
    /// Project-scoped skills directory, relative to the project root.
    pub project: &'static str,
    /// Global (user-scoped) skills directory, relative to `$HOME`; `None` when
    /// the agent has no user-level location.
    pub global: Option<&'static str>,
    /// The command that starts this agent's CLI, for `osdk skills use --agent`.
    /// `None` when the agent has no launchable CLI osdk drives (it can still be a
    /// skill target). Looked up on PATH; osdk never bundles the agent.
    pub launch: Option<&'static str>,
}

/// Agents osdk can install skills into.
///
/// Deliberately a first-cut subset of the wider ecosystem: the point of P0 is
/// the mechanism, and every additional agent is one more row here plus its two
/// path facts. Several agents share `.agents/skills/` as their project path on
/// purpose -- that shared directory is a real design constraint (removal has to
/// count remaining owners), not a mistake to deduplicate away.
pub static AGENT_TARGETS: &[AgentTarget] = &[
    AgentTarget {
        id: "claude-code",
        name: "Claude Code",
        project: ".claude/skills",
        global: Some(".claude/skills"),
        launch: Some("claude"),
    },
    AgentTarget {
        id: "codex",
        name: "Codex",
        project: ".agents/skills",
        global: Some(".codex/skills"),
        launch: Some("codex"),
    },
    AgentTarget {
        id: "cursor",
        name: "Cursor",
        project: ".agents/skills",
        global: Some(".cursor/skills"),
        launch: Some("cursor"),
    },
    AgentTarget {
        id: "opencode",
        name: "OpenCode",
        project: ".agents/skills",
        global: Some(".config/opencode/skills"),
        launch: Some("opencode"),
    },
    AgentTarget {
        id: "gemini-cli",
        name: "Gemini CLI",
        project: ".agents/skills",
        global: Some(".gemini/skills"),
        launch: Some("gemini"),
    },
    AgentTarget {
        id: "github-copilot",
        name: "GitHub Copilot",
        project: ".agents/skills",
        global: Some(".copilot/skills"),
        launch: None,
    },
    AgentTarget {
        id: "universal",
        name: "Universal (.agents)",
        project: ".agents/skills",
        global: Some(".config/agents/skills"),
        launch: None,
    },
];

/// Look up an agent target by its `--agent` id.
pub fn agent_target(id: &str) -> Option<&'static AgentTarget> {
    AGENT_TARGETS.iter().find(|target| target.id == id)
}

impl AgentTarget {
    /// The skills directory this agent reads for a project rooted at `root`.
    pub fn project_dir(&self, root: &Path) -> PathBuf {
        join_relative(root, self.project)
    }

    /// The skills directory this agent reads at user scope, given `home`.
    ///
    /// `None` when the agent is project-only.
    pub fn global_dir(&self, home: &Path) -> Option<PathBuf> {
        self.global.map(|relative| join_relative(home, relative))
    }
}

/// Join a `/`-separated relative path onto a base, one component at a time.
///
/// The table stores forward slashes because that is how the source data reads;
/// pushing them through `join` component-wise produces native separators on
/// every host without a per-entry `\` variant.
fn join_relative(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
    }
    path
}

/// Where a skill's bytes come from.
///
/// P0 supports two: a GitHub repository (resolved to an immutable commit and a
/// tarball at install time) and a local directory. Any git host over SSH, and a
/// federated registry, are deliberately out of scope here (see the design doc's
/// §10.1 / §10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// A GitHub repository, optionally narrowed to a subtree that holds the
    /// skill(s). `subdir` is `None` for a repository root.
    GitHub {
        owner: String,
        repo: String,
        subdir: Option<String>,
    },
    /// A directory on the local filesystem.
    Local { path: PathBuf },
}

impl SkillSource {
    /// Parse a source operand.
    ///
    /// Accepted forms:
    /// - `github:owner/repo` / `github:owner/repo/path/to/skill`
    /// - `owner/repo` (GitHub shorthand)
    /// - `https://github.com/owner/repo[/tree/<ref>/path...]`
    /// - a local path: `./x`, `../x`, `/abs/x`, or `~/x`
    ///
    /// The distinction is by shape, not by a flag, so an honest error names the
    /// real problem rather than "did not match any variant".
    pub fn parse(input: &str) -> Result<SkillSource> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(Error::config("empty skill source"));
        }

        // Explicit local markers first: these can never be a GitHub shorthand.
        if trimmed.starts_with("./")
            || trimmed.starts_with("../")
            || trimmed.starts_with('/')
            || trimmed.starts_with('~')
            || trimmed.starts_with(".\\")
            || trimmed.starts_with("..\\")
            || is_windows_absolute(trimmed)
        {
            return Ok(SkillSource::Local {
                path: PathBuf::from(trimmed),
            });
        }

        if let Some(rest) = trimmed.strip_prefix("github:") {
            return Self::from_owner_repo_path(rest);
        }

        if let Some(rest) = trimmed
            .strip_prefix("https://github.com/")
            .or_else(|| trimmed.strip_prefix("http://github.com/"))
        {
            return Self::from_github_url(rest);
        }

        // A bare `owner/repo[/subdir]` with no scheme and no dot in the first
        // segment is GitHub shorthand. A leading segment that looks like a host
        // (contains a dot) is rejected rather than guessed.
        if !trimmed.contains("://") {
            let first = trimmed.split('/').next().unwrap_or_default();
            if !first.is_empty() && !first.contains('.') {
                return Self::from_owner_repo_path(trimmed);
            }
        }

        Err(Error::config(format!(
            "unrecognized skill source `{input}` (use github:owner/repo, owner/repo, a github.com URL, or a local path)"
        )))
    }

    fn from_github_url(rest: &str) -> Result<SkillSource> {
        // owner/repo[/tree/<ref>/<subdir...>] -- drop the tree/<ref> pair and
        // keep the remaining path as the subdir. The ref itself is a version
        // selector handled separately (`--ref`), not part of source identity.
        let rest = rest.trim_end_matches('/');
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            return Err(Error::config(format!(
                "github url must include owner and repo: `{rest}`"
            )));
        }
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        let subdir = if parts.len() > 2 {
            // Skip a leading `tree/<ref>` if present.
            let tail = if parts.get(2) == Some(&"tree") && parts.len() > 4 {
                &parts[4..]
            } else {
                &parts[2..]
            };
            join_subdir(tail)
        } else {
            None
        };
        Self::github(owner, repo, subdir)
    }

    fn from_owner_repo_path(rest: &str) -> Result<SkillSource> {
        let rest = rest.trim_matches('/');
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            return Err(Error::config(format!(
                "skill source must be owner/repo: `{rest}`"
            )));
        }
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        let subdir = join_subdir(&parts[2..]);
        Self::github(owner, repo, subdir)
    }

    fn github(owner: &str, repo: &str, subdir: Option<String>) -> Result<SkillSource> {
        if !is_github_segment(owner) || !is_github_segment(repo) {
            return Err(Error::config(format!(
                "invalid github owner/repo `{owner}/{repo}`"
            )));
        }
        Ok(SkillSource::GitHub {
            owner: owner.to_string(),
            repo: repo.to_string(),
            subdir,
        })
    }

    /// A stable, path-safe identifier for this source, used as the store key and
    /// the default lock `source` string.
    pub fn canonical(&self) -> String {
        match self {
            SkillSource::GitHub {
                owner,
                repo,
                subdir,
            } => match subdir {
                Some(sub) => format!("github:{owner}/{repo}/{sub}"),
                None => format!("github:{owner}/{repo}"),
            },
            SkillSource::Local { path } => {
                format!("local:{}", path.to_string_lossy().replace('\\', "/"))
            }
        }
    }
}

fn join_subdir(parts: &[&str]) -> Option<String> {
    let cleaned: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.join("/"))
    }
}

fn is_github_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && segment != "."
        && segment != ".."
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Validate a skill name (the frontmatter `name`, used as a directory leaf).
///
/// Same rule as [`crate::model::validate_model_name`]: letters, digits, dot,
/// dash, underscore. Spaces are rejected here even though `npx skills` allows a
/// display name with spaces, because the name doubles as a filesystem component;
/// a display name is a separate concern from the install id.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(Error::config(format!(
            "invalid skill name `{name}` (use letters, digits, dot, dash, or underscore)"
        )));
    }
    Ok(())
}

/// A single file within a staged skill, recorded for hashing and the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    /// Path relative to the skill root, always `/`-separated so the digest is
    /// identical whether it was produced on Windows or Unix.
    pub path: String,
    /// Raw bytes of the file.
    pub bytes: Vec<u8>,
}

/// Compute the immutable content hash of a skill from its files.
///
/// The digest covers each file's relative path and bytes, in a fixed order, so
/// the same tree always yields the same hash regardless of directory iteration
/// order or host. Returned as `b3-v2:<64 hex>`, matching the identity scheme the
/// rest of osdk uses (`install_id_component`).
///
/// This is osdk's own BLAKE3 digest and is intentionally *not* the same as any
/// SHA-256 a registry might publish: the two cannot be compared directly, each
/// side computes its own.
pub fn content_hash(files: &[SkillFile]) -> String {
    let mut ordered: Vec<&SkillFile> = files.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"osdk-skill-v1\0");
    for file in ordered {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&(file.bytes.len() as u64).to_le_bytes());
        hasher.update(&file.bytes);
        hasher.update(b"\0");
    }
    format!("b3-v2:{}", hasher.finalize().to_hex())
}

/// Normalize a relative path inside a skill directory, rejecting anything that
/// could escape it. Accepts both separators (the path may have been written on a
/// different host), then requires every component to be a plain name.
pub fn safe_relative_path(value: &str) -> Result<String> {
    let normalized = value.replace('\\', "/");
    if normalized.is_empty() {
        return Err(Error::config("empty skill file path"));
    }
    // Reject an absolute path before cleaning: a leading `/` would otherwise be
    // dropped as an empty segment and silently turn `/etc/passwd` into a
    // relative `etc/passwd`.
    if normalized.starts_with('/') {
        return Err(Error::config(format!("unsafe skill file path `{value}`")));
    }
    let mut cleaned = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(Error::config(format!("unsafe skill file path `{value}`")));
            }
            other => cleaned.push(other),
        }
    }
    if cleaned.is_empty() {
        return Err(Error::config(format!("empty skill file path `{value}`")));
    }
    // A Windows drive prefix (`C:`) or any other colon-bearing component is
    // never a plain in-tree name.
    if cleaned.iter().any(|segment| segment.contains(':')) {
        return Err(Error::config(format!("unsafe skill file path `{value}`")));
    }
    Ok(cleaned.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_are_unique_and_paths_are_relative() {
        let mut seen = std::collections::BTreeSet::new();
        for target in AGENT_TARGETS {
            assert!(seen.insert(target.id), "duplicate agent id {}", target.id);
            assert!(
                !target.project.starts_with('/') && !target.project.starts_with('~'),
                "{} project path must be relative",
                target.id
            );
            if let Some(global) = target.global {
                assert!(
                    !global.starts_with('/') && !global.starts_with('~'),
                    "{} global path must be relative to home",
                    target.id
                );
            }
        }
    }

    #[test]
    fn agent_lookup_resolves_known_and_rejects_unknown() {
        assert_eq!(agent_target("claude-code").unwrap().name, "Claude Code");
        assert!(agent_target("nope").is_none());
    }

    #[test]
    fn agent_launch_commands_are_bare_program_names() {
        // `use --agent` runs the launch command directly (no shell), so a launch
        // entry must be a plain program name found on PATH, never a path or a
        // command line with arguments. At least one agent must be launchable, or
        // `use --agent` could never work.
        let mut launchable = 0;
        for target in AGENT_TARGETS {
            if let Some(launch) = target.launch {
                launchable += 1;
                assert!(!launch.is_empty(), "{} launch is empty", target.id);
                assert!(
                    !launch.contains(['/', '\\', ' ']),
                    "{} launch `{launch}` must be a bare program name",
                    target.id
                );
            }
        }
        assert!(launchable >= 1, "at least one agent must be launchable");
        // Claude Code is the reference launchable agent.
        assert_eq!(agent_target("claude-code").unwrap().launch, Some("claude"));
    }

    #[test]
    fn several_agents_share_the_project_agents_directory() {
        // This is a real constraint, not an accident: install-once/link-many and
        // the removal ownership count both depend on it. Pin it so a well-meant
        // "dedup" does not quietly change behavior.
        let sharing: Vec<&str> = AGENT_TARGETS
            .iter()
            .filter(|t| t.project == ".agents/skills")
            .map(|t| t.id)
            .collect();
        assert!(
            sharing.len() >= 2,
            "expected multiple agents to share .agents/skills, got {sharing:?}"
        );
    }

    #[test]
    fn agent_dirs_join_with_native_separators() {
        let target = agent_target("claude-code").unwrap();
        let root = Path::new("proj");
        assert_eq!(
            target.project_dir(root),
            Path::new("proj").join(".claude").join("skills")
        );
        let home = Path::new("home");
        assert_eq!(
            target.global_dir(home),
            Some(Path::new("home").join(".claude").join("skills"))
        );
    }

    #[test]
    fn parses_github_shorthand() {
        assert_eq!(
            SkillSource::parse("vercel-labs/agent-skills").unwrap(),
            SkillSource::GitHub {
                owner: "vercel-labs".into(),
                repo: "agent-skills".into(),
                subdir: None,
            }
        );
    }

    #[test]
    fn parses_github_scheme_with_subdir() {
        assert_eq!(
            SkillSource::parse("github:vercel-labs/agent-skills/skills/web-design").unwrap(),
            SkillSource::GitHub {
                owner: "vercel-labs".into(),
                repo: "agent-skills".into(),
                subdir: Some("skills/web-design".into()),
            }
        );
    }

    #[test]
    fn parses_github_url_and_drops_tree_ref() {
        assert_eq!(
            SkillSource::parse(
                "https://github.com/vercel-labs/agent-skills/tree/main/skills/web-design"
            )
            .unwrap(),
            SkillSource::GitHub {
                owner: "vercel-labs".into(),
                repo: "agent-skills".into(),
                subdir: Some("skills/web-design".into()),
            }
        );
        // A `.git` suffix and trailing slash are tolerated.
        assert_eq!(
            SkillSource::parse("https://github.com/owner/repo.git/").unwrap(),
            SkillSource::GitHub {
                owner: "owner".into(),
                repo: "repo".into(),
                subdir: None,
            }
        );
    }

    #[test]
    fn parses_local_paths() {
        for input in ["./my-skills", "../x", "/abs/x", "~/skills"] {
            assert!(
                matches!(
                    SkillSource::parse(input).unwrap(),
                    SkillSource::Local { .. }
                ),
                "{input} should parse as local"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn parses_windows_absolute_local_path() {
        assert!(matches!(
            SkillSource::parse(r"C:\skills\mine").unwrap(),
            SkillSource::Local { .. }
        ));
    }

    #[test]
    fn rejects_hostlike_or_empty_sources() {
        assert!(SkillSource::parse("").is_err());
        // A dotted first segment looks like a host, not a github owner.
        assert!(SkillSource::parse("example.com/owner/repo").is_err());
        // Owner without repo.
        assert!(SkillSource::parse("github:owner").is_err());
    }

    #[test]
    fn canonical_is_stable_and_round_trips_through_parse() {
        for input in [
            "github:vercel-labs/agent-skills",
            "github:vercel-labs/agent-skills/skills/web-design",
        ] {
            let parsed = SkillSource::parse(input).unwrap();
            assert_eq!(parsed.canonical(), input);
            // Canonical form re-parses to the same source.
            assert_eq!(SkillSource::parse(&parsed.canonical()).unwrap(), parsed);
        }
    }

    #[test]
    fn content_hash_is_order_independent_and_prefixed() {
        let a = vec![
            SkillFile {
                path: "SKILL.md".into(),
                bytes: b"---\nname: x\n---\n".to_vec(),
            },
            SkillFile {
                path: "examples/a.txt".into(),
                bytes: b"hello".to_vec(),
            },
        ];
        let mut b = a.clone();
        b.reverse();
        let ha = content_hash(&a);
        let hb = content_hash(&b);
        assert_eq!(ha, hb, "hash must not depend on input order");
        assert!(ha.starts_with("b3-v2:"));
        assert_eq!(ha.len(), "b3-v2:".len() + 64);
    }

    #[test]
    fn content_hash_changes_with_any_byte() {
        let base = vec![SkillFile {
            path: "SKILL.md".into(),
            bytes: b"one".to_vec(),
        }];
        let mut changed = base.clone();
        changed[0].bytes = b"two".to_vec();
        assert_ne!(content_hash(&base), content_hash(&changed));

        // A length prefix guards the concatenation boundary: moving a byte from
        // one file's tail to the next file's head must still change the digest.
        let split = vec![
            SkillFile {
                path: "a".into(),
                bytes: b"xy".to_vec(),
            },
            SkillFile {
                path: "b".into(),
                bytes: b"z".to_vec(),
            },
        ];
        let moved = vec![
            SkillFile {
                path: "a".into(),
                bytes: b"x".to_vec(),
            },
            SkillFile {
                path: "b".into(),
                bytes: b"yz".to_vec(),
            },
        ];
        assert_ne!(content_hash(&split), content_hash(&moved));
    }

    #[test]
    fn safe_relative_path_accepts_plain_and_rejects_escape() {
        assert_eq!(
            safe_relative_path("examples/a.txt").unwrap(),
            "examples/a.txt"
        );
        assert_eq!(safe_relative_path("a\\b").unwrap(), "a/b");
        assert_eq!(safe_relative_path("./x/./y").unwrap(), "x/y");
        for bad in ["../x", "a/../../b", "/etc/passwd", "C:/x", "..", ""] {
            assert!(safe_relative_path(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn validate_skill_name_matches_model_rule() {
        for good in ["web-design", "skill_1", "a.b-c"] {
            assert!(validate_skill_name(good).is_ok(), "{good}");
        }
        for bad in ["", "has space", "slash/name", "utf8✓"] {
            assert!(validate_skill_name(bad).is_err(), "{bad}");
        }
    }
}
