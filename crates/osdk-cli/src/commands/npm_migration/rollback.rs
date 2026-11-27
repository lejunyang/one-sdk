//! Local rollback for migration into another managed Node installation.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use osdk_core::store::dirlink;

use super::{modules, npm_command, ManagedNodeTools};

#[cfg(unix)]
const MAN_DIR: &str = "share/man";

pub(in crate::commands) struct Snapshot {
    prefix: PathBuf,
    bin: PathBuf,
    backup: tempfile::TempDir,
    directory_permissions: BTreeMap<PathBuf, Permissions>,
    #[cfg(unix)]
    linked_man_sections: BTreeMap<String, PathBuf>,
}

impl Snapshot {
    pub(in crate::commands) fn capture(tools: &ManagedNodeTools) -> Result<Self> {
        let prefix = resolve_prefix(&tools.prefix)?;
        let bin = prefix.join(tools.bin.strip_prefix(&tools.prefix)?);
        // The resolved prefix can itself be a mount point. Create the
        // backup inside it so the backup is on the same volume.
        let backup = tempfile::Builder::new()
            .prefix(".node-migration-")
            .tempdir_in(&prefix)
            .context("creating backup for Node package migration")?;
        let mut snapshot = Self {
            prefix,
            bin,
            backup,
            directory_permissions: BTreeMap::new(),
            #[cfg(unix)]
            linked_man_sections: BTreeMap::new(),
        };

        let modules = modules(&snapshot.prefix);
        check_directory_if_present(modules.parent().unwrap())?;
        snapshot.capture_tree(&modules, "node_modules")?;
        #[cfg(unix)]
        {
            let man = snapshot.prefix.join(MAN_DIR);
            check_directory_if_present(man.parent().unwrap())?;
            snapshot.capture_tree(&man, "man")?;
            if man.exists() {
                snapshot.capture_linked_man_sections(&man)?;
            }
        }
        check_bin(&snapshot.bin)?;
        let backup_bin = snapshot.backup.path().join("bin");
        fs::create_dir(&backup_bin)?;
        for entry in fs::read_dir(&snapshot.bin)? {
            let entry = entry?;
            if is_launcher(&entry.file_name(), &entry.path().symlink_metadata()?) {
                snapshot.copy_entry(&entry.path(), &backup_bin.join(entry.file_name()))?;
            }
        }
        Ok(snapshot)
    }

    fn capture_tree(&mut self, path: &Path, backup_name: &str) -> Result<()> {
        if let Some(metadata) = inspect(path)? {
            if !real_directory(&metadata) {
                return Err(anyhow!(
                    "npm path is not a real directory: {}",
                    path.display()
                ));
            }
            self.copy_entry(path, &self.backup.path().join(backup_name))?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn capture_linked_man_sections(&mut self, man: &Path) -> Result<()> {
        for entry in fs::read_dir(man)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(section) = name
                .to_str()
                .and_then(|name| name.strip_prefix("man"))
                .filter(|section| {
                    !section.is_empty() && section.bytes().all(|b| b.is_ascii_digit())
                })
            else {
                continue;
            };
            if !entry.file_type()?.is_symlink() {
                continue;
            }
            let link = entry.path();
            let directory = match fs::canonicalize(&link) {
                Ok(directory) => directory,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("resolving {}", link.display()))
                }
            };
            if !directory.is_dir() {
                continue;
            }
            let backup = self.backup.path().join("linked-man").join(section);
            fs::create_dir_all(&backup)?;
            // npm writes direct entries ending in the section number or .<section>.gz.
            for page in fs::read_dir(&directory)? {
                let page = page?;
                if is_man_page(&page.file_name(), section) {
                    self.copy_entry(&page.path(), &backup.join(page.file_name()))?;
                }
            }
            self.linked_man_sections
                .insert(section.to_owned(), directory);
        }
        Ok(())
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
        self.verify_paths(&tools.prefix)?;
        if !real_directory(&self.backup.path().symlink_metadata()?) {
            return Err(anyhow!("Node package backup is not a real directory"));
        }
        let modules = modules(&self.prefix);
        let old_modules = self.backup.path().join("node_modules");
        check_directory_if_present(modules.parent().unwrap())?;
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
        self.verify_paths(&tools.prefix)?;
        check_directory_if_present(modules.parent().unwrap())?;
        self.restore_tree(&old_modules, &modules)?;
        #[cfg(unix)]
        {
            let man = self.prefix.join(MAN_DIR);
            check_directory_if_present(man.parent().unwrap())?;
            self.restore_tree(&self.backup.path().join("man"), &man)?;
            self.restore_linked_man_sections()?;
        }

        check_bin(&self.bin)?;
        let old_bin = self.backup.path().join("bin");
        for entry in fs::read_dir(&self.bin)? {
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
            self.restore_entry(&entry.path(), &self.bin.join(entry.file_name()))?;
        }
        Ok(())
    }

    fn verify_paths(&self, managed_prefix: &Path) -> Result<()> {
        let resolved = resolve_prefix(managed_prefix)?;
        if resolved != self.prefix {
            return Err(anyhow!(
                "target Node prefix {} now resolves to {} instead of {}",
                managed_prefix.display(),
                resolved.display(),
                self.prefix.display()
            ));
        }
        #[cfg(unix)]
        for (section, directory) in &self.linked_man_sections {
            let link = self.prefix.join(MAN_DIR).join(format!("man{section}"));
            let resolved = fs::canonicalize(&link)
                .with_context(|| format!("resolving npm man section {}", link.display()))?;
            if resolved != *directory {
                return Err(anyhow!(
                    "npm man section {} now resolves to {} instead of {}",
                    link.display(),
                    resolved.display(),
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn restore_linked_man_sections(&self) -> Result<()> {
        for (section, directory) in &self.linked_man_sections {
            let backup = self.backup.path().join("linked-man").join(section);
            for page in fs::read_dir(directory)? {
                let page = page?;
                if is_man_page(&page.file_name(), section)
                    && inspect(&backup.join(page.file_name()))?.is_none()
                {
                    remove_entry(&page.path(), &page.path().symlink_metadata()?)?;
                }
            }
            for page in fs::read_dir(backup)? {
                let page = page?;
                self.restore_entry(&page.path(), &directory.join(page.file_name()))?;
            }
        }
        Ok(())
    }

    fn restore_tree(&self, backup: &Path, path: &Path) -> Result<()> {
        if backup.exists() {
            fs::create_dir_all(path.parent().unwrap())?;
            self.restore_entry(backup, path)?;
        } else if let Some(metadata) = inspect(path)? {
            remove_entry(path, &metadata)?;
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

#[cfg(unix)]
fn is_man_page(name: &OsStr, section: &str) -> bool {
    name.to_str().is_some_and(|name| {
        let name = name.strip_suffix(".gz").unwrap_or(name);
        name.rsplit_once('.')
            .is_some_and(|(_, suffix)| suffix == section)
    })
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

fn resolve_prefix(path: &Path) -> Result<PathBuf> {
    dunce::canonicalize(path)
        .with_context(|| format!("resolving target Node prefix {}", path.display()))
}

fn check_directory_if_present(path: &Path) -> Result<()> {
    if inspect(path)?.is_some_and(|metadata| !real_directory(&metadata)) {
        return Err(anyhow!(
            "npm path parent is not a real directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn check_bin(bin: &Path) -> Result<()> {
    if !real_directory(&bin.symlink_metadata()?) {
        return Err(anyhow!(
            "npm bin path is not a real directory: {}",
            bin.display()
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
    fn node_package_migration_refuses_rollback_after_prefix_retarget() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let prefix = temp.path().join("target");
        let bin_path = if cfg!(windows) { "" } else { "bin" };
        for destination in [&original, &replacement] {
            fs::create_dir_all(destination.join(bin_path)).unwrap();
            fs::create_dir_all(modules(destination)).unwrap();
        }
        dirlink::create(&original, &prefix).unwrap();
        let bin = prefix.join(bin_path);
        let tools = ManagedNodeTools {
            npm: bin.join("unused"),
            bin,
            prefix,
        };
        let snapshot = Snapshot::capture(&tools).unwrap();
        let backup = snapshot.backup.path().to_owned();
        assert_eq!(
            backup.parent().unwrap(),
            dunce::canonicalize(original).unwrap()
        );
        let unrelated = modules(&replacement).join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        dirlink::retarget(&replacement, &tools.prefix).unwrap();

        let error = format!("{:#}", snapshot.restore(&tools, []).unwrap_err());
        assert!(error.contains("now resolves to"), "{error}");
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert!(backup.is_dir());
        assert!(unrelated.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn node_package_migration_refuses_rollback_after_man_section_retarget() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("target");
        let bin = prefix.join("bin");
        let section = prefix.join(MAN_DIR).join("man1");
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        for directory in [&bin, section.parent().unwrap(), &original, &replacement] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(original.join("cli.1.gz"), b"original page").unwrap();
        fs::write(replacement.join("other.1.gz"), b"unrelated page").unwrap();
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

        let error = format!("{:#}", snapshot.restore(&tools, []).unwrap_err());
        assert!(error.contains("now resolves to"), "{error}");
        assert!(error.contains(&section.display().to_string()), "{error}");
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert_eq!(fs::read_link(section).unwrap(), replacement);
        assert_eq!(
            fs::read(replacement.join("other.1.gz")).unwrap(),
            b"unrelated page"
        );
        assert_eq!(
            fs::read(backup.join("linked-man/1/cli.1.gz")).unwrap(),
            b"original page"
        );
    }

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
    fn node_package_migration_backs_up_junction_past_max_path() {
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Component, Prefix};
        use windows_sys::Win32::Foundation::MAX_PATH;

        const ORIGINAL_LINK_LEN: usize = 245;
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let wide_len = |path: &Path| path.as_os_str().encode_wide().count();
        let package = "linked";
        // The backup inserts a directory component, taking this 245-unit
        // package path past MAX_PATH without a verbatim drive prefix.
        let padding = ORIGINAL_LINK_LEN - (wide_len(&modules(&root).join(package)) + 1);
        let prefix = root.join("x".repeat(padding));
        let tools = ManagedNodeTools {
            npm: prefix.join("unused"),
            bin: prefix.clone(),
            prefix,
        };
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        let link = modules(&tools.prefix).join(package);
        assert_eq!(wide_len(&link), ORIGINAL_LINK_LEN);
        assert!(wide_len(&link) < MAX_PATH as usize);
        assert!(matches!(
            link.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
        ));
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        dirlink::create(&project, &link).unwrap();
        let target = fs::read_link(&link).unwrap();
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );

        let snapshot = Snapshot::capture(&tools).unwrap();
        let backup_link = snapshot.backup.path().join("node_modules").join(package);
        assert!(wide_len(&backup_link) > MAX_PATH as usize);
        assert!(matches!(
            backup_link.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
        ));
        assert_eq!(fs::read_link(&backup_link).unwrap(), target);
        assert_eq!(
            dirlink::reparse_tag(&backup_link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );

        fs::remove_dir(&link).unwrap();
        snapshot.restore(&tools, []).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(
            dirlink::reparse_tag(&link).unwrap(),
            dirlink::IO_REPARSE_TAG_MOUNT_POINT
        );
        assert_eq!(
            fs::canonicalize(link).unwrap(),
            fs::canonicalize(project).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn node_package_migration_restores_junction_over_symlink_to_same_target() {
        use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;

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
        match std::os::windows::fs::symlink_dir(&original_target, &link) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => {
                eprintln!("skipping symlink replacement: {error}");
                return;
            }
            Err(error) => panic!("creating directory symlink: {error}"),
        }
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
