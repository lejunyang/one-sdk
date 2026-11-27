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

/// Install a staged skill into one agent's `<agent_dir>/<name>` path.
///
/// The replacement is prepared beside the destination before the existing path
/// is touched. An existing link, file, or real directory is renamed to a backup,
/// the prepared tree/link is activated with a same-directory rename, and any
/// activation failure restores the backup. This permits confirmed copy-mode
/// updates without exposing a half-written destination.
pub fn link_into(agent_skills_dir: &Path, name: &str, staged: &Path, mode: LinkMode) -> Result<()> {
    let mut targets = SkillTargetChanges::new(name)?;
    targets.replace(agent_skills_dir, staged, mode)?;
    targets.apply(|| Ok(()))
}

/// Prepared directory changes for one skill. Old destinations remain live until
/// every replacement is ready, and backups are kept until the lock is saved.
pub struct SkillTargetChanges<'a> {
    name: &'a str,
    targets: Vec<PreparedTarget>,
}

impl<'a> SkillTargetChanges<'a> {
    pub fn new(name: &'a str) -> Result<Self> {
        super::validate_skill_name(name)?;
        Ok(Self {
            name,
            targets: Vec::new(),
        })
    }

    pub fn replace(
        &mut self,
        agent_skills_dir: &Path,
        staged: &Path,
        mode: LinkMode,
    ) -> Result<()> {
        std::fs::create_dir_all(agent_skills_dir).map_err(|e| Error::io(agent_skills_dir, e))?;
        let dest = agent_skills_dir.join(self.name);
        let staged = dunce::canonicalize(staged).map_err(|e| Error::io(staged, e))?;
        let canonical_parent =
            dunce::canonicalize(agent_skills_dir).map_err(|e| Error::io(agent_skills_dir, e))?;

        if let Ok(meta) = dest.symlink_metadata() {
            if let Ok(existing) = dunce::canonicalize(&dest) {
                if dirlink::is_link(&meta) {
                    if mode != LinkMode::Copy && existing == staged {
                        return Ok(());
                    }
                } else {
                    reject_overlapping_trees(&staged, &existing)?;
                }
            }
        } else {
            reject_overlapping_trees(&staged, &canonical_parent.join(self.name))?;
        }

        let mut target = PreparedTarget::new(agent_skills_dir, dest)?;
        let prepared = target.scratch.path().join("prepared");
        prepare_destination(&staged, &prepared, mode)?;
        target.prepared = Some(prepared);
        self.targets.push(target);
        Ok(())
    }

    pub fn remove(&mut self, agent_skills_dir: &Path) -> Result<bool> {
        let dest = agent_skills_dir.join(self.name);
        match dest.symlink_metadata() {
            Ok(_) => self
                .targets
                .push(PreparedTarget::new(agent_skills_dir, dest)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(Error::io(&dest, error)),
        }
        Ok(true)
    }

    /// Activate all prepared changes, then persist their metadata. A failure
    /// reverses every completed rename, including those for removed targets.
    pub fn apply<E>(
        self,
        persist: impl FnOnce() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<Error> + std::fmt::Display,
    {
        let mut renames = Vec::new();
        let result = self
            .targets
            .iter()
            .try_for_each(|target| target.activate(&mut renames))
            .map_err(E::from)
            .and_then(|()| persist());
        if let Err(error) = result {
            let restore_errors: Vec<_> = renames
                .into_iter()
                .rev()
                .filter_map(|(from, to)| {
                    std::fs::rename(to, from).err().map(|err| {
                        format!("restoring {} from {}: {err}", from.display(), to.display())
                    })
                })
                .collect();
            if !restore_errors.is_empty() {
                // Leave the backups available if the filesystem refused to restore them.
                for target in self.targets {
                    let _ = target.scratch.keep();
                }
                return Err(Error::other(format!("{error}; {}", restore_errors.join("; "))).into());
            }
            return Err(error);
        }
        for target in self.targets {
            let _ = remove_path_if_exists(&target.backup);
        }
        Ok(())
    }
}

struct PreparedTarget {
    dest: PathBuf,
    scratch: tempfile::TempDir,
    backup: PathBuf,
    prepared: Option<PathBuf>,
}

impl PreparedTarget {
    fn new(parent: &Path, dest: PathBuf) -> Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix(".osdk-skill-replace-")
            .tempdir_in(parent)
            .map_err(|e| Error::io(parent, e))?;
        let backup = scratch.path().join("backup");
        Ok(Self {
            dest,
            scratch,
            backup,
            prepared: None,
        })
    }

    fn activate<'a>(&'a self, renames: &mut Vec<(&'a Path, &'a Path)>) -> Result<()> {
        match self.dest.symlink_metadata() {
            Ok(_) => {
                std::fs::rename(&self.dest, &self.backup).map_err(|e| Error::io(&self.dest, e))?;
                renames.push((&self.dest, &self.backup));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&self.dest, error)),
        }
        if let Some(prepared) = &self.prepared {
            std::fs::rename(prepared, &self.dest).map_err(|e| Error::io(&self.dest, e))?;
            renames.push((prepared, &self.dest));
        }
        Ok(())
    }
}

/// Reject a source tree that is also the install destination (or contains it).
/// This is checked before staging so a local source cannot be deleted by the
/// replacement it asked osdk to perform.
pub fn validate_source_destination(source: &Path, destination: &Path) -> Result<()> {
    let source = canonicalize_with_missing_tail(source)?;
    let destination = canonicalize_with_missing_tail(destination)?;
    reject_overlapping_trees(&source, &destination)
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match dunce::canonicalize(cursor) {
            Ok(mut existing) => {
                for component in missing.iter().rev() {
                    existing.push(component);
                }
                return Ok(existing);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = cursor.file_name().ok_or_else(|| Error::io(path, error))?;
                missing.push(component.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    Error::other(format!("cannot resolve path {}", path.display()))
                })?;
            }
            Err(error) => return Err(Error::io(path, error)),
        }
    }
}

fn reject_overlapping_trees(staged: &Path, destination: &Path) -> Result<()> {
    if staged == destination || staged.starts_with(destination) || destination.starts_with(staged) {
        return Err(Error::other(format!(
            "refusing to install a skill from overlapping source and destination paths: {} and {}",
            staged.display(),
            destination.display()
        )));
    }
    Ok(())
}

fn prepare_destination(staged: &Path, prepared: &Path, mode: LinkMode) -> Result<()> {
    if mode == LinkMode::Copy {
        return copy_tree(staged, prepared);
    }
    match dirlink::create(staged, prepared) {
        Ok(()) => Ok(()),
        Err(_) => {
            remove_path_if_exists(prepared)?;
            copy_tree(staged, prepared)
        }
    }
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    let Ok(meta) = path.symlink_metadata() else {
        return Ok(());
    };
    if dirlink::is_link(&meta) {
        dirlink::remove(path).map_err(|e| Error::io(path, e))
    } else if meta.is_dir() {
        dirlink::remove_tree_links_first(path).map_err(|e| Error::io(path, e))?;
        std::fs::remove_dir_all(path).map_err(|e| Error::io(path, e))
    } else {
        std::fs::remove_file(path).map_err(|e| Error::io(path, e))
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
    remove_path_if_exists(dest)?;
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
    fn replaces_a_real_directory_without_leaving_stale_files() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let src = temp.path().join("skill");
        write(&src, "SKILL.md", "---\nname: mine\ndescription: d\n---\n");
        write(&src, "new.txt", "new data");
        let package = read_skill_dir(&src).unwrap();
        let staged = stage(&dirs, "local:mine", &package, LinkMode::Copy).unwrap();

        let agent_dir = temp.path().join(".claude").join("skills");
        let destination = agent_dir.join("mine");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("stale.txt"), "old data").unwrap();

        link_into(&agent_dir, "mine", &staged, LinkMode::Copy).unwrap();
        assert!(destination.join("SKILL.md").is_file());
        assert!(destination.join("new.txt").is_file());
        assert!(!destination.join("stale.txt").exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("new.txt")).unwrap(),
            "new data"
        );
        let siblings = std::fs::read_dir(&agent_dir).unwrap().count();
        assert_eq!(siblings, 1, "successful replacement must remove its backup");
    }

    #[test]
    fn source_destination_overlap_is_rejected_before_destination_exists() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("skill");
        std::fs::create_dir_all(&source).unwrap();
        let nested_destination = source.join(".agents/skills/mine");
        assert!(validate_source_destination(&source, &nested_destination).is_err());

        let sibling_destination = temp.path().join("project/.agents/skills/mine");
        validate_source_destination(&source, &sibling_destination).unwrap();
    }

    #[test]
    fn copy_mode_converts_an_existing_link_to_a_real_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let src = temp.path().join("skill");
        write(&src, "SKILL.md", "---\nname: mine\ndescription: d\n---\n");
        let package = read_skill_dir(&src).unwrap();
        let staged = stage(&dirs, "local:mine", &package, LinkMode::Copy).unwrap();
        let agent_dir = temp.path().join(".claude").join("skills");

        link_into(&agent_dir, "mine", &staged, LinkMode::Auto).unwrap();
        assert!(dirlink::is_link(
            &agent_dir.join("mine").symlink_metadata().unwrap()
        ));
        link_into(&agent_dir, "mine", &staged, LinkMode::Copy).unwrap();
        assert!(!dirlink::is_link(
            &agent_dir.join("mine").symlink_metadata().unwrap()
        ));
        assert!(agent_dir.join("mine/SKILL.md").is_file());
    }

    #[test]
    fn later_activation_failure_restores_replaced_and_removed_targets() {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join(".agents/skills");
        let dropped = temp.path().join("dropped/skills");
        let claude = temp.path().join(".claude/skills");
        let staged = temp.path().join("staged");
        for dir in [&codex, &dropped, &claude] {
            write(&dir.join("mine"), "SKILL.md", "old data");
        }
        write(&staged, "SKILL.md", "new data");
        let mut targets = SkillTargetChanges::new("mine").unwrap();
        targets.replace(&codex, &staged, LinkMode::Copy).unwrap();
        assert!(targets.remove(&dropped).unwrap());
        targets.replace(&claude, &staged, LinkMode::Auto).unwrap();

        // Fail the second replacement after its old directory was moved aside.
        remove_path_if_exists(targets.targets[2].prepared.as_ref().unwrap()).unwrap();
        let error = targets
            .apply::<Error>(|| panic!("a failed activation must not save the lock"))
            .unwrap_err();
        assert!(matches!(error, Error::Io { path, .. } if path == claude.join("mine")));
        for dir in [codex, dropped, claude] {
            assert_eq!(
                std::fs::read_to_string(dir.join("mine/SKILL.md")).unwrap(),
                "old data"
            );
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
        }
        assert_eq!(
            std::fs::read_to_string(staged.join("SKILL.md")).unwrap(),
            "new data"
        );
    }

    #[test]
    fn lock_save_failure_restores_existing_target_and_removes_new_target() {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join(".agents/skills");
        let claude = temp.path().join(".claude/skills");
        let staged = temp.path().join("staged");
        write(&codex.join("mine"), "SKILL.md", "old data");
        write(&staged, "SKILL.md", "new data");
        let mut targets = SkillTargetChanges::new("mine").unwrap();
        targets.replace(&codex, &staged, LinkMode::Copy).unwrap();
        targets.replace(&claude, &staged, LinkMode::Auto).unwrap();

        let error = targets
            .apply(|| {
                for dir in [&codex, &claude] {
                    assert_eq!(
                        std::fs::read_to_string(dir.join("mine/SKILL.md")).unwrap(),
                        "new data"
                    );
                }
                Err(Error::other("could not save lock"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "could not save lock");
        assert_eq!(
            std::fs::read_to_string(codex.join("mine/SKILL.md")).unwrap(),
            "old data"
        );
        assert_eq!(std::fs::read_dir(&claude).unwrap().count(), 0);
    }

    #[test]
    fn rejects_overlapping_source_and_destination_trees() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("skills").join("mine");
        std::fs::create_dir_all(&destination).unwrap();
        let error = link_into(
            &temp.path().join("skills"),
            "mine",
            &destination,
            LinkMode::Copy,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("overlapping source and destination"));
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
