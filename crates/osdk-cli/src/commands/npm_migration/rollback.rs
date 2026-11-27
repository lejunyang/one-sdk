//! Local rollback for migration into another managed Node installation.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use osdk_core::store::dirlink;

use super::{modules, npm_command, ManagedNodeTools};

pub(in crate::commands) struct Snapshot {
    backup: tempfile::TempDir,
    directory_permissions: BTreeMap<PathBuf, Permissions>,
}

impl Snapshot {
    pub(in crate::commands) fn capture(tools: &ManagedNodeTools) -> Result<Self> {
        let backup = tempfile::Builder::new()
            .prefix(".node-migration-")
            .tempdir_in(tools.prefix.parent().context("Node prefix has no parent")?)
            .context("creating backup for Node package migration")?;
        let mut snapshot = Self {
            backup,
            directory_permissions: BTreeMap::new(),
        };

        let modules = modules(&tools.prefix);
        check_modules_parent(tools, &modules)?;
        if let Some(metadata) = inspect(&modules)? {
            if !real_directory(&metadata) {
                return Err(anyhow!(
                    "npm package directory is not a real directory: {}",
                    modules.display()
                ));
            }
            snapshot.copy_entry(&modules, &snapshot.backup.path().join("node_modules"))?;
        }
        check_bin(tools)?;
        let backup_bin = snapshot.backup.path().join("bin");
        fs::create_dir(&backup_bin)?;
        for entry in fs::read_dir(&tools.bin)? {
            let entry = entry?;
            if is_launcher(&entry.file_name(), &entry.path().symlink_metadata()?) {
                snapshot.copy_entry(&entry.path(), &backup_bin.join(entry.file_name()))?;
            }
        }
        Ok(snapshot)
    }

    fn copy_entry(&mut self, from: &Path, to: &Path) -> Result<()> {
        let metadata = from.symlink_metadata()?;
        if real_directory(&metadata) {
            fs::create_dir(to)?;
            self.directory_permissions
                .insert(to.to_owned(), metadata.permissions());
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                self.copy_entry(&entry.path(), &to.join(entry.file_name()))?;
            }
        } else {
            copy_leaf(from, to, &metadata)?;
        }
        Ok(())
    }

    pub(in crate::commands) fn restore<'a>(
        self,
        tools: &ManagedNodeTools,
        planned: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        if let Err(error) = self.restore_paths(tools, planned) {
            let backup = self.backup.keep();
            return Err(error.context(format!("Node package backup kept at {}", backup.display())));
        }
        Ok(())
    }

    fn restore_paths<'a>(
        &self,
        tools: &ManagedNodeTools,
        planned: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let modules = modules(&tools.prefix);
        let old_modules = self.backup.path().join("node_modules");
        check_modules_parent(tools, &modules)?;
        if inspect(&modules)?.is_some_and(|meta| real_directory(&meta)) {
            let mut args = vec!["uninstall", "-g", "--offline", "--ignore-scripts"];
            for name in planned {
                if added_package(name, &old_modules, &modules)? {
                    args.push(name);
                }
            }
            if args.len() > 4 {
                // npm can reject a partially installed tree. The filesystem
                // pass below removes whatever is left, including incomplete packages.
                let _ = npm_command(tools, &args);
            }
        }
        check_modules_parent(tools, &modules)?;
        if old_modules.exists() {
            fs::create_dir_all(modules.parent().unwrap())?;
            self.restore_entry(&old_modules, &modules)?;
        } else if let Some(metadata) = inspect(&modules)? {
            remove_entry(&modules, &metadata)?;
        }

        check_bin(tools)?;
        let old_bin = self.backup.path().join("bin");
        for entry in fs::read_dir(&tools.bin)? {
            let entry = entry?;
            let metadata = entry.path().symlink_metadata()?;
            if is_launcher(&entry.file_name(), &metadata)
                && inspect(&old_bin.join(entry.file_name()))?.is_none()
            {
                remove_entry(&entry.path(), &metadata)?;
            }
        }
        for entry in fs::read_dir(old_bin)? {
            let entry = entry?;
            self.restore_entry(&entry.path(), &tools.bin.join(entry.file_name()))?;
        }
        Ok(())
    }

    fn restore_entry(&self, from: &Path, to: &Path) -> Result<()> {
        let before = from.symlink_metadata()?;
        let current = inspect(to)?;
        if real_directory(&before) {
            if !current.as_ref().is_some_and(real_directory) {
                if let Some(metadata) = current {
                    remove_entry(to, &metadata)?;
                }
                fs::create_dir(to)?;
            }
            for entry in fs::read_dir(to)? {
                let entry = entry?;
                if inspect(&from.join(entry.file_name()))?.is_none() {
                    remove_entry(&entry.path(), &entry.path().symlink_metadata()?)?;
                }
            }
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                self.restore_entry(&entry.path(), &to.join(entry.file_name()))?;
            }
            let permissions = &self.directory_permissions[from];
            if to.symlink_metadata()?.permissions() != *permissions {
                fs::set_permissions(to, permissions.clone())?;
            }
            return Ok(());
        }

        if let Some(metadata) = current {
            if dirlink::is_link(&before)
                && dirlink::is_link(&metadata)
                && same_link(from, &before, to, &metadata)?
            {
                return Ok(());
            }
            if before.is_file()
                && metadata.is_file()
                && !dirlink::is_link(&metadata)
                && before.len() == metadata.len()
                && before.permissions() == metadata.permissions()
                && same_contents(from, to)?
            {
                return Ok(());
            }
            remove_entry(to, &metadata)?;
        }
        copy_leaf(from, to, &before)
    }
}

fn inspect(path: &Path) -> Result<Option<Metadata>> {
    match path.symlink_metadata() {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn real_directory(metadata: &Metadata) -> bool {
    metadata.is_dir() && !dirlink::is_link(metadata)
}

fn check_modules_parent(tools: &ManagedNodeTools, modules: &Path) -> Result<()> {
    for parent in [tools.prefix.as_path(), modules.parent().unwrap()] {
        if inspect(parent)?.is_some_and(|metadata| !real_directory(&metadata)) {
            return Err(anyhow!(
                "npm package parent is not a real directory: {}",
                parent.display()
            ));
        }
    }
    Ok(())
}

fn check_bin(tools: &ManagedNodeTools) -> Result<()> {
    if !real_directory(&tools.bin.symlink_metadata()?) {
        return Err(anyhow!(
            "npm bin path is not a real directory: {}",
            tools.bin.display()
        ));
    }
    Ok(())
}

fn is_launcher(name: &OsStr, metadata: &Metadata) -> bool {
    let runtime = if cfg!(windows) {
        name.as_encoded_bytes().eq_ignore_ascii_case(b"node.exe")
            || name
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b"node_modules")
            || name.as_encoded_bytes().starts_with(b".osdk-")
    } else {
        name == "node"
    };
    !runtime && !real_directory(metadata)
}

fn added_package(name: &str, before: &Path, current: &Path) -> Result<bool> {
    let Ok(id) = osdk_core::tool::ToolId::dynamic("npm", name) else {
        return Ok(false);
    };
    if id.subject() != name || name.starts_with('-') {
        return Ok(false);
    }
    if let Some((scope, _)) = name.split_once('/') {
        if !inspect(&current.join(scope))?.is_some_and(|meta| real_directory(&meta))
            || inspect(&before.join(scope))?.is_some_and(|meta| !real_directory(&meta))
        {
            return Ok(false);
        }
    }
    Ok(inspect(&before.join(name))?.is_none()
        && inspect(&current.join(name))?
            .is_some_and(|meta| meta.is_dir() || dirlink::is_link(&meta)))
}

fn same_link(from: &Path, before: &Metadata, to: &Path, current: &Metadata) -> Result<bool> {
    if before.file_type() != current.file_type() {
        return Ok(false);
    }
    #[cfg(windows)]
    if dirlink::reparse_tag(from)? != dirlink::reparse_tag(to)? {
        return Ok(false);
    }
    Ok(fs::read_link(from)? == fs::read_link(to)?)
}

fn copy_leaf(from: &Path, to: &Path, metadata: &Metadata) -> Result<()> {
    if dirlink::is_link(metadata) {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::{symlink_dir, symlink_file, FileTypeExt};

            match dirlink::reparse_tag(from)? {
                dirlink::IO_REPARSE_TAG_MOUNT_POINT => dirlink::recreate_junction(&target, to)?,
                dirlink::IO_REPARSE_TAG_SYMLINK if metadata.file_type().is_symlink_dir() => {
                    symlink_dir(target, to)?;
                }
                dirlink::IO_REPARSE_TAG_SYMLINK => symlink_file(target, to)?,
                tag => {
                    return Err(anyhow!(
                        "unsupported npm reparse tag {tag:#x} at {}",
                        from.display()
                    ))
                }
            }
        }
    } else if metadata.is_file() {
        fs::copy(from, to)?;
    } else {
        return Err(anyhow!("unsupported npm path {}", from.display()));
    }
    Ok(())
}

fn remove_entry(path: &Path, metadata: &Metadata) -> Result<()> {
    #[cfg(windows)]
    if dirlink::is_link(metadata) {
        use std::os::windows::fs::FileTypeExt;

        if metadata.is_dir() || metadata.file_type().is_symlink_dir() {
            return fs::remove_dir(path).with_context(|| format!("unlinking {}", path.display()));
        }
    }
    let result = if real_directory(metadata) {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.with_context(|| format!("removing {}", path.display()))
}

fn same_contents(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left = BufReader::new(File::open(left)?);
    let mut right = BufReader::new(File::open(right)?);
    loop {
        let a = left.fill_buf()?;
        let b = right.fill_buf()?;
        let size = a.len().min(b.len());
        if size == 0 {
            return Ok(a.is_empty() && b.is_empty());
        }
        if a[..size] != b[..size] {
            return Ok(false);
        }
        left.consume(size);
        right.consume(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_package_migration_restores_overwritten_links_and_launchers() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("target");
        let bin = if cfg!(windows) {
            prefix.clone()
        } else {
            prefix.join("bin")
        };
        let tools = ManagedNodeTools {
            npm: bin.join("unused"),
            bin,
            prefix,
        };
        let project = tmp.path().join("local project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("cli.js"), b"local source").unwrap();
        let modules = modules(&tools.prefix);
        let link = modules.join("@scope/linked");
        let unrelated = modules.join("local-tarball");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::create_dir(&unrelated).unwrap();
        let tarball_file = unrelated.join("cli.js");
        fs::write(&tarball_file, b"local tarball").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink("../../../../local project", &link).unwrap();
        #[cfg(windows)]
        dirlink::create(&project, &link).unwrap();
        let original_link = fs::read_link(&link).unwrap();
        #[cfg(windows)]
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );

        fs::create_dir_all(&tools.bin).unwrap();
        let launcher = tools.bin.join("example.cmd");
        fs::write(&launcher, b"original launcher").unwrap();
        let snapshot = Snapshot::capture(&tools).unwrap();
        #[cfg(windows)]
        assert_eq!(
            dirlink::reparse_tag(&snapshot.backup.path().join("node_modules/@scope/linked"))
                .unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );
        remove_entry(&link, &link.symlink_metadata().unwrap()).unwrap();
        fs::create_dir(&link).unwrap();
        fs::write(link.join("cli.js"), b"registry package").unwrap();
        fs::write(&launcher, b"replacement launcher").unwrap();
        fs::write(&tarball_file, b"registry tarball").unwrap();

        let added_bin = tools.bin.join("added.cmd");
        fs::write(&added_bin, b"new launcher").unwrap();
        let added_scope = modules.join("@new/partial");
        fs::create_dir_all(&added_scope).unwrap();
        fs::write(added_scope.join("package.json"), b"{").unwrap();
        let dangling_target = tmp.path().join("gone");
        fs::rename(&project, &dangling_target).unwrap();

        snapshot.restore(&tools, []).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), original_link);
        #[cfg(windows)]
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );
        assert_eq!(fs::read(&launcher).unwrap(), b"original launcher");
        assert_eq!(fs::read(&tarball_file).unwrap(), b"local tarball");
        assert!(!added_bin.exists());
        assert!(!modules.join("@new").exists());
        fs::rename(dangling_target, &project).unwrap();
        assert_eq!(
            fs::canonicalize(link).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(fs::read(project.join("cli.js")).unwrap(), b"local source");
    }

    #[cfg(windows)]
    #[test]
    fn node_package_migration_restores_junction_over_symlink_to_same_target() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("target");
        let tools = ManagedNodeTools {
            npm: prefix.join("unused"),
            bin: prefix.clone(),
            prefix,
        };
        let project = tmp.path().join("project");
        fs::create_dir(&project).unwrap();
        let modules = modules(&tools.prefix);
        fs::create_dir_all(&modules).unwrap();
        let link = modules.join("linked");
        dirlink::create(&project, &link).unwrap();
        let original_target = fs::read_link(&link).unwrap();
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );
        let snapshot = Snapshot::capture(&tools).unwrap();

        fs::remove_dir(&link).unwrap();
        std::os::windows::fs::symlink_dir(&original_target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), original_target);
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_SYMLINK
        );

        snapshot.restore(&tools, []).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), original_target);
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );
    }
}
