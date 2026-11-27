use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use osdk_core::pipeline::validate_safe_filename;

use super::{check_directory_if_present, inspect, modules, Snapshot};

const MAN: &str = "share/man";

#[derive(PartialEq, Eq)]
pub(super) struct SectionDirectory {
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
        let sections = man_sections(&self.prefix)?;
        for (section, directory) in &sections {
            let backup = self.backup.path().join("man").join(section);
            fs::create_dir_all(&backup)?;
            for page in fs::read_dir(&directory.path)? {
                let page = page?;
                if is_man_page(&page.file_name(), section) {
                    self.copy_entry(&page.path(), &backup.join(page.file_name()))?;
                }
            }
        }
        self.man_sections = sections;
        Ok(())
    }

    pub(super) fn restore_manpages<'a>(
        &self,
        planned: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let package_dirs = planned
            .into_iter()
            .map(man_link_package_dir)
            .collect::<Result<Vec<_>>>()?;
        let sections = man_sections(&self.prefix)?;
        for (name, directory) in &self.man_sections {
            if sections.get(name) != Some(directory) {
                return Err(anyhow!(
                    "npm man section {} no longer points to the snapshotted directory {}",
                    self.prefix.join(MAN).join(name).display(),
                    directory.path.display()
                ));
            }
        }

        for (section, directory) in sections {
            let logical = self.prefix.join(MAN).join(&section);
            let backup = self.backup.path().join("man").join(&section);
            if self.man_sections.contains_key(&section) {
                for page in fs::read_dir(&backup)? {
                    let original = directory.path.join(page?.file_name());
                    if inspect(&original)?.is_none() {
                        return Err(anyhow!(
                            "cannot attribute the missing npm man page {} to this migration",
                            original.display()
                        ));
                    }
                }
            } else if directory.path != logical {
                // npm creates real section directories. A new link to another
                // directory leaves us without a baseline for those entries.
                return Err(anyhow!(
                    "npm man section {} became a link to an unsnapshotted directory",
                    logical.display()
                ));
            }

            let mut changed_links = Vec::new();
            for page in fs::read_dir(&directory.path)? {
                let page = page?;
                let name = page.file_name();
                if !is_man_page(&name, &section) || !page.file_type()?.is_symlink() {
                    continue;
                }
                let path = page.path();
                let target = fs::read_link(&path)?;
                if !package_dirs.iter().any(|package| {
                    target.strip_prefix(package).is_ok_and(|file| {
                        file.file_name() == Some(&name)
                            && file
                                .components()
                                .all(|part| matches!(part, Component::Normal(_)))
                    })
                }) {
                    continue;
                }
                changed_links.push((path, name, target));
            }

            if !changed_links.is_empty() && SectionDirectory::resolve(&logical)? != Some(directory)
            {
                return Err(anyhow!(
                    "npm man section {} changed while preparing rollback",
                    logical.display()
                ));
            }
            for (path, name, target) in changed_links {
                if fs::read_link(&path)? != target {
                    return Err(anyhow!(
                        "npm man page {} changed while preparing rollback",
                        path.display()
                    ));
                }
                let original = backup.join(&name);
                if inspect(&original)?.is_some() {
                    self.restore_entry(&original, &path)?;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("unlinking {}", path.display()))?;
                }
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

fn man_link_package_dir(name: &str) -> Result<PathBuf> {
    if let Some((scope, leaf)) = name.split_once('/') {
        if !scope.starts_with('@') {
            return Err(anyhow!("invalid scoped npm installation key {name:?}"));
        }
        validate_safe_filename("npm scope", scope)?;
        validate_safe_filename("npm package", leaf)?;
    } else {
        validate_safe_filename("npm package", name)?;
    }

    // npm computes this relative to the logical share/man/manN path, even
    // when that section is a symlink. Match the original installation key
    // byte-for-byte, including case, without resolving the possibly dead link.
    Ok(modules(Path::new("../../..")).join(name))
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
