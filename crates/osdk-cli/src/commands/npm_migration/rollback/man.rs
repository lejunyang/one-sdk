use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::{check_directory_if_present, inspect, Snapshot};

const MAN: &str = "share/man";
const JOURNAL: &str = "man-writes.jsonl";
const PRELOAD: &str = "record-man-links.cjs";

pub(super) struct SectionSnapshot {
    directory: SectionDirectory,
    pages: BTreeMap<OsString, String>,
}

#[derive(Deserialize)]
struct LinkWrite {
    path: PathBuf,
    target: PathBuf,
    identity: String,
}

impl LinkWrite {
    fn is_current(&self, metadata: Option<&Metadata>) -> Result<bool> {
        if metadata.is_some_and(|entry| entry.is_symlink() && identity(entry) == self.identity) {
            return Ok(fs::read_link(&self.path)?.as_os_str() == self.target.as_os_str());
        }
        Ok(false)
    }
}

fn identity(metadata: &Metadata) -> String {
    let changed_ns =
        i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
    format!("{}:{}:{}", metadata.dev(), metadata.ino(), changed_ns)
}

#[derive(PartialEq, Eq)]
struct SectionDirectory {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SectionDirectory {
    fn resolve(path: &Path) -> Result<Option<Self>> {
        let resolved = match fs::canonicalize(path) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", path.display()))
            }
        };
        let metadata = resolved.metadata()?;
        Ok(metadata.is_dir().then_some(Self {
            path: resolved,
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
}

impl Snapshot {
    pub(super) fn capture_manpages(&mut self) -> Result<()> {
        fs::File::create(self.backup.path().join(JOURNAL))?;
        for (section, directory) in man_sections(&self.prefix)? {
            let backup = self.backup.path().join("man").join(&section);
            fs::create_dir_all(&backup)?;
            let mut pages = BTreeMap::new();
            for page in fs::read_dir(&directory.path)? {
                let page = page?;
                let name = page.file_name();
                if is_man_page(&name, &section) {
                    let stamp = identity(&page.path().symlink_metadata()?);
                    self.copy_entry(&page.path(), &backup.join(&name))?;
                    pages.insert(name, stamp);
                }
            }
            self.man_sections
                .insert(section, SectionSnapshot { directory, pages });
        }
        Ok(())
    }

    pub(in crate::commands) fn observe_man_installs(
        &self,
        command: &mut Command,
        prefix: &Path,
    ) -> Result<()> {
        let preload = self.backup.path().join(PRELOAD);
        fs::write(&preload, include_bytes!("record-man-links.cjs"))?;
        let path = preload
            .to_str()
            .context("npm man journal path is not UTF-8")?;
        let path = path.replace('\\', r"\\").replace('"', r#"\""#);
        let mut options = OsString::from(format!("--require=\"{path}\""));
        let original = std::env::var_os("NODE_OPTIONS");
        if let Some(original) = &original {
            options.push(" ");
            options.push(original);
        }
        command
            .env("NODE_OPTIONS", options)
            .env("OSDK_NPM_MAN_JOURNAL", self.backup.path().join(JOURNAL))
            .env("OSDK_NPM_MAN_PREFIX", prefix)
            .env("OSDK_NPM_MAN_PARENT", std::process::id().to_string());
        if let Some(original) = original {
            command.env("OSDK_NPM_ORIGINAL_NODE_OPTIONS", original);
        } else {
            command.env_remove("OSDK_NPM_ORIGINAL_NODE_OPTIONS");
        }
        Ok(())
    }

    pub(super) fn restore_manpages(&self) -> Result<()> {
        let sections = man_sections(&self.prefix)?;
        for (name, before) in &self.man_sections {
            if sections.get(name) != Some(&before.directory) {
                return Err(anyhow!(
                    "npm man section {} no longer points to the snapshotted directory {}",
                    self.prefix.join(MAN).join(name).display(),
                    before.directory.path.display()
                ));
            }
        }

        let journal = fs::read_to_string(self.backup.path().join(JOURNAL))?;
        let mut writes = BTreeMap::new();
        for line in journal.lines() {
            let write: LinkWrite =
                serde_json::from_str(line).context("reading npm man link journal")?;
            writes.insert(write.path.clone(), write);
        }

        let mut plan = Vec::new();
        for (section, directory) in &sections {
            let logical = self.prefix.join(MAN).join(section);
            let before = self.man_sections.get(section);
            if before.is_none() && directory.path != logical {
                return Err(anyhow!(
                    "npm man section {} became a link to an unsnapshotted directory",
                    logical.display()
                ));
            }

            if let Some(before) = before {
                let backup = self.backup.path().join("man").join(section);
                for (name, stamp) in &before.pages {
                    let path = directory.path.join(name);
                    let current = inspect(&path)?;
                    if current
                        .as_ref()
                        .is_some_and(|entry| identity(entry) == *stamp)
                    {
                        continue;
                    }
                    let write = match writes.get(&path) {
                        Some(write) if write.is_current(current.as_ref())? => write,
                        _ => {
                            return Err(anyhow!(
                                "cannot attribute the change to npm man page {} to this migration",
                                path.display()
                            ))
                        }
                    };
                    plan.push((logical.clone(), directory, write, Some(backup.join(name))));
                }
            }

            for write in writes.values() {
                let Some(name) = write.path.file_name() else {
                    continue;
                };
                if write.path.parent() == Some(directory.path.as_path())
                    && is_man_page(name, section)
                    && !before.is_some_and(|before| before.pages.contains_key(name))
                    && write.is_current(inspect(&write.path)?.as_ref())?
                {
                    plan.push((logical.clone(), directory, write, None));
                }
            }
        }

        for (logical, directory, write, original) in plan {
            if SectionDirectory::resolve(&logical)?.as_ref() != Some(directory)
                || !write.is_current(inspect(&write.path)?.as_ref())?
            {
                return Err(anyhow!(
                    "npm man page {} changed while preparing rollback",
                    write.path.display()
                ));
            }
            fs::remove_file(&write.path)
                .with_context(|| format!("unlinking {}", write.path.display()))?;
            if let Some(original) = original {
                self.restore_entry(&original, &write.path)?;
            }
        }
        Ok(())
    }
}

fn man_sections(prefix: &Path) -> Result<BTreeMap<String, SectionDirectory>> {
    let man = prefix.join(MAN);
    check_directory_if_present(man.parent().unwrap())?;
    check_directory_if_present(&man)?;
    let mut sections = BTreeMap::new();
    if inspect(&man)?.is_some() {
        for entry in fs::read_dir(&man)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| {
                name.strip_prefix("man").is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
            }) else {
                continue;
            };
            if let Some(directory) = SectionDirectory::resolve(&entry.path())? {
                sections.insert(name.to_owned(), directory);
            }
        }
    }
    Ok(sections)
}

fn is_man_page(name: &OsStr, section: &str) -> bool {
    name.to_str().is_some_and(|name| {
        let name = name.strip_suffix(".gz").unwrap_or(name);
        name.rsplit_once('.')
            .is_some_and(|(_, suffix)| suffix == &section["man".len()..])
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::commands::npm_migration::ManagedNodeTools;

    #[test]
    fn node_package_migration_keeps_backup_after_man_section_retarget() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("target");
        let bin = prefix.join("bin");
        let section = prefix.join(MAN).join("man1");
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        for path in [&bin, section.parent().unwrap(), &original, &replacement] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(original.join("cli.1.gz"), b"original page").unwrap();
        symlink(
            "../../../lib/node_modules/incoming/cli.1.gz",
            replacement.join("cli.1.gz"),
        )
        .unwrap();
        symlink(&original, &section).unwrap();
        let tools = ManagedNodeTools {
            npm: bin.join("unused"),
            bin,
            prefix,
        };
        let snapshot = Snapshot::capture(&tools).unwrap();
        let backup = snapshot.backup.path().to_owned();
        fs::remove_file(&section).unwrap();
        symlink(&replacement, &section).unwrap();

        let error = format!("{:#}", snapshot.restore(&tools, ["incoming"]).unwrap_err());
        assert!(
            error.contains("no longer points to the snapshotted directory"),
            "{error}"
        );
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert_eq!(fs::read_link(section).unwrap(), replacement);
        assert_eq!(
            fs::read_link(replacement.join("cli.1.gz")).unwrap(),
            Path::new("../../../lib/node_modules/incoming/cli.1.gz")
        );
        assert_eq!(
            fs::read(backup.join("man/man1/cli.1.gz")).unwrap(),
            b"original page"
        );
    }
}
