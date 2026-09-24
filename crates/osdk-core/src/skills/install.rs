//! Staging and linking skills onto disk.
//!
//! The flow, for one skill from an already-materialized source directory:
//!
//! 1. [`read_skill_dir`] reads the directory into a [`SkillPackage`], parsing the
//!    `SKILL.md` frontmatter (`name` + `description`) and collecting every file.
//! 2. [`stage`] ingests that tree into the content-addressed store under
//!    `<data>/skills/<id>/<hash>/`, deduplicated like any other install.
//! 3. [`link_into`] points a target agent's `skills/<name>/` directory at the
//!    staged copy (a junction/symlink, or a copy when links are unavailable).
//!
//! Downloading a GitHub tarball into a source directory, resolving a commit, and
//! writing `osdk.lock` are the CLI's job (they need `App`/`Ctx` and the lock
//! type); this module is the on-disk mechanism they drive, and it is exercised
//! end to end with tempdirs and no network.

use std::path::{Path, PathBuf};

use crate::dirs::{sanitize_tool_id, sanitize_version_component, Dirs};
use crate::error::{Error, Result};
use crate::store::dirlink;
use crate::store::link::LinkMode;
use crate::store::Cas;

use super::{content_hash, safe_relative_path, SkillFile};

/// The name of the file every skill must contain.
pub const SKILL_MANIFEST: &str = "SKILL.md";

/// Cap on how many files a single skill may contain, and their total size.
///
/// Aligns with the wider ecosystem's defaults (`npx skills`: 1000 files, 25 MiB
/// extracted) and takes the stricter of the two where they differ. A skill is
/// instructions plus small assets; anything approaching these bounds is either a
/// mistake or hostile, and refusing loudly beats silently staging a repo dump.
pub const MAX_SKILL_FILES: usize = 1000;
/// Total uncompressed byte budget for one skill's files.
pub const MAX_SKILL_BYTES: u64 = 25 * 1024 * 1024;

/// A skill read from a source directory: its declared identity plus every file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    /// The `name` from `SKILL.md` frontmatter.
    pub name: String,
    /// The `description` from `SKILL.md` frontmatter (for the install preview).
    pub description: String,
    /// Every file in the skill, paths relative to the skill root and
    /// `/`-separated. Always includes `SKILL.md`.
    pub files: Vec<SkillFile>,
}

impl SkillPackage {
    /// The immutable content hash over this package's files.
    pub fn content_hash(&self) -> String {
        content_hash(&self.files)
    }

    /// Total byte size of all files.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes.len() as u64).sum()
    }

    /// A one-line summary for the pre-install preview: what the skill is, how
    /// many files it carries, and whether any of them is executable-ish.
    ///
    /// osdk never runs a skill's contents, but a skill *is* instructions a
    /// downstream agent will read, so surfacing "N files, includes scripts"
    /// before writing into an agent directory is the honest minimum.
    pub fn preview(&self) -> String {
        let scripts = self
            .files
            .iter()
            .filter(|f| looks_executable(&f.path))
            .count();
        let mut line = format!(
            "{} — {} ({} file{}, {})",
            self.name,
            self.description,
            self.files.len(),
            if self.files.len() == 1 { "" } else { "s" },
            human_bytes(self.total_bytes()),
        );
        if scripts > 0 {
            line.push_str(&format!(", {scripts} script-like"));
        }
        line
    }
}

/// Read a skill from a source directory into a [`SkillPackage`].
///
/// The directory must contain a `SKILL.md` with YAML frontmatter carrying a
/// `name` and `description`. Every regular file is collected; symlinks are
/// skipped (a skill is content, not a link farm) and paths that would escape the
/// root are rejected via [`safe_relative_path`].
pub fn read_skill_dir(root: &Path) -> Result<SkillPackage> {
    let manifest_path = root.join(SKILL_MANIFEST);
    if !manifest_path.is_file() {
        return Err(Error::config(format!(
            "no {SKILL_MANIFEST} in {}",
            root.display()
        )));
    }

    let mut files = Vec::new();
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|e| Error::other(format!("reading skill dir: {e}")))?;
        if !entry.file_type().is_file() {
            // Directories are implied by their files; a symlink is deliberately
            // not followed and not copied.
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::other("skill file escaped its root"))?;
        let rel_str = safe_relative_path(&rel.to_string_lossy())?;
        let bytes = std::fs::read(entry.path()).map_err(|e| Error::io(entry.path(), e))?;
        total += bytes.len() as u64;
        if total > MAX_SKILL_BYTES {
            return Err(Error::config(format!(
                "skill in {} exceeds the {} size limit",
                root.display(),
                human_bytes(MAX_SKILL_BYTES)
            )));
        }
        files.push(SkillFile {
            path: rel_str,
            bytes,
        });
        if files.len() > MAX_SKILL_FILES {
            return Err(Error::config(format!(
                "skill in {} exceeds the {MAX_SKILL_FILES}-file limit",
                root.display()
            )));
        }
    }

    let manifest = files
        .iter()
        .find(|f| f.path == SKILL_MANIFEST)
        .ok_or_else(|| Error::config(format!("no {SKILL_MANIFEST} in {}", root.display())))?;
    let (name, description) = parse_frontmatter(&manifest.bytes)?;
    super::validate_skill_name(&name)?;

    Ok(SkillPackage {
        name,
        description,
        files,
    })
}

/// Where a staged skill lives: `<data>/skills/<id>/<hash>/`.
///
/// `id` is the source's canonical string (path-sanitized), `hash` its content
/// digest, so two revisions of the same source coexist and identical content is
/// shared. Mirrors the tool layout's fingerprinted install root.
pub fn staged_root(dirs: &Dirs, id: &str, content_hash: &str) -> PathBuf {
    let hash_component = content_hash
        .strip_prefix("b3-v2:")
        .map(|digest| format!("b3-v2-{digest}"))
        .unwrap_or_else(|| sanitize_version_component(content_hash));
    dirs.skills()
        .join(sanitize_tool_id(id))
        .join(hash_component)
}

/// Stage a package into the store under its `<id>/<hash>` root, returning the
/// staged directory. Idempotent: re-staging identical content is a no-op copy
/// from the CAS.
pub fn stage(dirs: &Dirs, id: &str, package: &SkillPackage, mode: LinkMode) -> Result<PathBuf> {
    let hash = package.content_hash();
    let staged = staged_root(dirs, id, &hash);

    // Materialize the package into a scratch tree, then ingest that tree into
    // the CAS and out to the staged root. Writing to scratch first keeps the
    // staged root all-or-nothing.
    let scratch = dirs
        .tmp()
        .join("skills-stage")
        .join(sanitize_tool_id(id))
        .join(
            hash.strip_prefix("b3-v2:")
                .unwrap_or(&hash)
                .get(..16)
                .unwrap_or("stage"),
        );
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch).map_err(|e| Error::io(&scratch, e))?;
    }
    for file in &package.files {
        let dst = scratch.join(rel_to_native(&file.path));
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(&dst, &file.bytes).map_err(|e| Error::io(&dst, e))?;
    }

    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    // A clean staged root every time: content is immutable per hash, so a
    // pre-existing one is either identical or a partial from a crash.
    if staged.exists() {
        std::fs::remove_dir_all(&staged).map_err(|e| Error::io(&staged, e))?;
    }
    let cas = Cas::new(dirs.store.clone());
    cas.ingest_tree(&scratch, &staged, id, &hash, mode)?;
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(staged)
}

/// Link a staged skill into one agent's skills directory as `<agent_dir>/<name>`.
///
/// Prefers a directory link (junction on Windows, symlink on Unix); on failure,
/// or when `LinkMode::Copy` is requested, copies the tree. Refuses to overwrite
/// a real directory that osdk did not place there.
pub fn link_into(agent_skills_dir: &Path, name: &str, staged: &Path, mode: LinkMode) -> Result<()> {
    super::validate_skill_name(name)?;
    std::fs::create_dir_all(agent_skills_dir).map_err(|e| Error::io(agent_skills_dir, e))?;
    let dest = agent_skills_dir.join(name);

    // If a real (non-link) directory is already there, it is not ours to
    // clobber -- surface it rather than deleting a user's files.
    if let Ok(meta) = dest.symlink_metadata() {
        if !dirlink::is_link(&meta) && meta.is_dir() {
            return Err(Error::other(format!(
                "refusing to replace a real directory at {}",
                dest.display()
            )));
        }
    }

    if mode == LinkMode::Copy {
        return copy_tree(staged, &dest);
    }
    match dirlink::retarget(staged, &dest) {
        Ok(()) => Ok(()),
        // Links can be unavailable (no privilege on Windows without dev mode,
        // or a filesystem that lacks them). Copy is the always-works fallback,
        // matching how the file-level link mode degrades.
        Err(_) => copy_tree(staged, &dest),
    }
}

/// Remove a skill from one agent's skills directory.
///
/// Removes a link cheaply; removes a copied tree recursively. Missing is not an
/// error (removal is idempotent). Returns whether anything was removed.
pub fn unlink_from(agent_skills_dir: &Path, name: &str) -> Result<bool> {
    let dest = agent_skills_dir.join(name);
    let Ok(meta) = dest.symlink_metadata() else {
        return Ok(false);
    };
    if dirlink::is_link(&meta) {
        dirlink::remove(&dest).map_err(|e| Error::io(&dest, e))?;
    } else if meta.is_dir() {
        std::fs::remove_dir_all(&dest).map_err(|e| Error::io(&dest, e))?;
    } else {
        std::fs::remove_file(&dest).map_err(|e| Error::io(&dest, e))?;
    }
    Ok(true)
}

fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        // Replace an existing link or copy wholesale so a re-install cannot
        // leave stale files from a previous revision behind.
        if let Ok(meta) = dest.symlink_metadata() {
            if dirlink::is_link(&meta) {
                dirlink::remove(dest).map_err(|e| Error::io(dest, e))?;
            } else {
                std::fs::remove_dir_all(dest).map_err(|e| Error::io(dest, e))?;
            }
        }
    }
    std::fs::create_dir_all(dest).map_err(|e| Error::io(dest, e))?;
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|e| Error::other(format!("copying skill: {e}")))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| Error::other("copy strip_prefix failed"))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            std::fs::copy(entry.path(), &target).map_err(|e| Error::io(&target, e))?;
        }
    }
    Ok(())
}

/// Parse the minimal `SKILL.md` frontmatter: a `---` fenced block with `name`
/// and `description` keys.
///
/// A deliberately small parser rather than a YAML dependency: the frontmatter
/// osdk needs is two scalar strings, and pulling in a YAML crate for the shim's
/// sibling library would be weight for no gain. A block scalar (`>-`/`|`) folds
/// its following indented lines into the value.
fn parse_frontmatter(bytes: &[u8]) -> Result<(String, String)> {
    let text = String::from_utf8_lossy(bytes);
    // Tolerate a leading UTF-8 BOM: editors and some generators write one, and a
    // BOM before the `---` fence would otherwise make a perfectly valid skill
    // look like it has no frontmatter.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text.lines();
    // The first non-empty line must be the opening fence.
    let opened = lines
        .by_ref()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim() == "---")
        .unwrap_or(false);
    if !opened {
        return Err(Error::config(format!(
            "{SKILL_MANIFEST} must begin with a `---` frontmatter block"
        )));
    }

    let mut fields: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut pending_key: Option<String> = None;
    let mut pending_val = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        // Continuation of a block scalar: an indented line with no `key:` at
        // column zero.
        if pending_key.is_some() && (line.starts_with(' ') || line.starts_with('\t')) {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if !pending_val.is_empty() {
                    pending_val.push(' ');
                }
                pending_val.push_str(trimmed);
            }
            continue;
        }
        if let Some(key) = pending_key.take() {
            fields.insert(key, std::mem::take(&mut pending_val));
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if value == ">-" || value == ">" || value == "|" || value == "|-" {
                pending_key = Some(key);
                pending_val.clear();
            } else {
                fields.insert(key, unquote(value).to_string());
            }
        }
    }
    if let Some(key) = pending_key.take() {
        fields.insert(key, pending_val);
    }
    if !closed {
        return Err(Error::config(format!(
            "{SKILL_MANIFEST} frontmatter block is not closed with `---`"
        )));
    }

    let name = fields
        .get("name")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::config(format!("{SKILL_MANIFEST} frontmatter is missing `name`")))?
        .clone();
    let description = fields.get("description").cloned().unwrap_or_default();
    Ok((name, description))
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn looks_executable(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".sh", ".bash", ".zsh", ".ps1", ".py", ".js", ".rb", ".bat", ".cmd",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn rel_to_native(rel: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in rel.split('/') {
        path.push(segment);
    }
    path
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn dirs_in(root: &Path) -> Dirs {
        Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn reads_frontmatter_through_a_utf8_bom() {
        // Editors and PowerShell's `Set-Content -Encoding utf8` prepend a BOM; a
        // BOM before the `---` fence must not make a valid skill look unparsable.
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("skill");
        std::fs::create_dir_all(&src).unwrap();
        let body = "---\nname: bom\ndescription: has a leading BOM\n---\n";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(body.as_bytes());
        std::fs::write(src.join("SKILL.md"), bytes).unwrap();

        let package = read_skill_dir(&src).unwrap();
        assert_eq!(package.name, "bom");
        assert_eq!(package.description, "has a leading BOM");
    }

    #[test]
    fn reads_frontmatter_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("skill");
        write(
            &src,
            "SKILL.md",
            "---\nname: web-design\ndescription: >-\n  A guide to\n  web design.\n---\n# Body\n",
        );
        write(&src, "examples/a.ts", "export const x = 1;\n");

        let package = read_skill_dir(&src).unwrap();
        assert_eq!(package.name, "web-design");
        assert_eq!(package.description, "A guide to web design.");
        assert_eq!(package.files.len(), 2);
        assert!(package.files.iter().any(|f| f.path == "SKILL.md"));
        assert!(package.files.iter().any(|f| f.path == "examples/a.ts"));
        assert!(package.preview().contains("web-design"));
    }

    #[test]
    fn rejects_missing_manifest_and_missing_name() {
        let temp = tempfile::tempdir().unwrap();
        let no_manifest = temp.path().join("none");
        write(&no_manifest, "readme.md", "hi");
        assert!(read_skill_dir(&no_manifest).is_err());

        let no_name = temp.path().join("noname");
        write(&no_name, "SKILL.md", "---\ndescription: x\n---\n");
        assert!(read_skill_dir(&no_name).is_err());

        let unclosed = temp.path().join("unclosed");
        write(&unclosed, "SKILL.md", "---\nname: x\n");
        assert!(read_skill_dir(&unclosed).is_err());
    }

    #[test]
    fn stages_into_hash_root_and_dedups() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let src = temp.path().join("skill");
        write(&src, "SKILL.md", "---\nname: s\ndescription: d\n---\n");
        write(&src, "note.txt", "hello");
        let package = read_skill_dir(&src).unwrap();

        let staged = stage(&dirs, "github:o/r", &package, LinkMode::Copy).unwrap();
        assert!(staged.join("SKILL.md").is_file());
        assert!(staged.join("note.txt").is_file());
        // The staged root is under <data>/skills, keyed by the content hash.
        assert!(staged.starts_with(dirs.skills()));
        let leaf = staged.file_name().unwrap().to_string_lossy();
        assert!(leaf.starts_with("b3-v2-"), "{leaf}");

        // Re-staging identical content lands in the same root.
        let again = stage(&dirs, "github:o/r", &package, LinkMode::Copy).unwrap();
        assert_eq!(staged, again);
    }

    #[test]
    fn links_and_unlinks_into_agent_dir() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let src = temp.path().join("skill");
        write(&src, "SKILL.md", "---\nname: mine\ndescription: d\n---\n");
        write(&src, "data.txt", "payload");
        let package = read_skill_dir(&src).unwrap();
        let staged = stage(&dirs, "local:mine", &package, LinkMode::Copy).unwrap();

        let agent_dir = temp.path().join("proj").join(".claude").join("skills");
        // Copy mode is the portable path exercised on every platform.
        link_into(&agent_dir, "mine", &staged, LinkMode::Copy).unwrap();
        let landed = agent_dir.join("mine");
        assert!(landed.join("SKILL.md").is_file());
        assert!(landed.join("data.txt").is_file());

        assert!(unlink_from(&agent_dir, "mine").unwrap());
        assert!(!landed.exists());
        // Idempotent: removing again is not an error.
        assert!(!unlink_from(&agent_dir, "mine").unwrap());
    }

    #[test]
    fn refuses_to_clobber_a_real_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let src = temp.path().join("skill");
        write(&src, "SKILL.md", "---\nname: mine\ndescription: d\n---\n");
        let package = read_skill_dir(&src).unwrap();
        let staged = stage(&dirs, "local:mine", &package, LinkMode::Copy).unwrap();

        let agent_dir = temp.path().join(".claude").join("skills");
        // A user's own real directory sits where the skill would go.
        std::fs::create_dir_all(agent_dir.join("mine")).unwrap();
        std::fs::write(agent_dir.join("mine").join("keep.txt"), "user data").unwrap();

        let err = link_into(&agent_dir, "mine", &staged, LinkMode::Copy).unwrap_err();
        assert!(err.to_string().contains("refusing to replace"));
        // The user's file is untouched.
        assert!(agent_dir.join("mine").join("keep.txt").is_file());
    }

    #[test]
    fn enforces_file_count_limit() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("big");
        write(&src, "SKILL.md", "---\nname: big\ndescription: d\n---\n");
        for i in 0..(MAX_SKILL_FILES + 1) {
            write(&src, &format!("f{i}.txt"), "x");
        }
        assert!(read_skill_dir(&src).is_err());
    }
}
