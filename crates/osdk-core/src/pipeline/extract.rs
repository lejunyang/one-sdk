//! Archive extraction: tar.gz / tar.xz / tar.zst / zip / 7z, with optional
//! stripping of a single top-level directory (node/go/python archives wrap
//! everything in one root dir like `node-v20-linux-x64/`).
//!
//! `.7z` is only compiled into the install path. Unpacking one costs a second
//! LZMA implementation, and the shim never extracts anything, so both the
//! dependency and the `SevenZ` variant sit behind the `install` feature.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    TarXz,
    TarZst,
    Zip,
    #[cfg(feature = "install")]
    SevenZ,
}

impl ArchiveKind {
    /// Guess the archive kind from a filename/URL.
    pub fn from_name(name: &str) -> Result<ArchiveKind> {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
            Ok(ArchiveKind::TarGz)
        } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
            Ok(ArchiveKind::TarXz)
        } else if n.ends_with(".tar.zst") || n.ends_with(".tzst") {
            Ok(ArchiveKind::TarZst)
        } else if n.ends_with(".zip") {
            Ok(ArchiveKind::Zip)
        } else if n.ends_with(".7z") {
            // Recognized even without the `install` feature so the shim reports
            // an unsupported archive rather than a confusing parse failure.
            #[cfg(feature = "install")]
            {
                Ok(ArchiveKind::SevenZ)
            }
            #[cfg(not(feature = "install"))]
            {
                Err(Error::UnsupportedArchive(name.to_string()))
            }
        } else {
            Err(Error::UnsupportedArchive(name.to_string()))
        }
    }
}

/// Extract `archive` into `dest`. If `strip_root` is true and the archive has a
/// single top-level directory, its contents are lifted up one level.
pub fn extract(archive: &Path, dest: &Path, kind: ArchiveKind, strip_root: bool) -> Result<()> {
    create_dir_all(dest)?;
    // Extract into a scratch dir first, then optionally strip the root while
    // moving into `dest`.
    let scratch = dest.join(".osdk-extract-tmp");
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    create_dir_all(&scratch)?;

    match kind {
        ArchiveKind::TarGz => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = flate2::read::GzDecoder::new(BufReader::new(f));
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::TarXz => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = xz2::read::XzDecoder::new(BufReader::new(f));
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::TarZst => {
            let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
            let dec = zstd::stream::read::Decoder::new(BufReader::new(f))
                .map_err(|e| Error::io(archive, e))?;
            unpack_tar(dec, &scratch)?;
        }
        ArchiveKind::Zip => {
            unpack_zip(archive, &scratch, strip_root)?;
        }
        #[cfg(feature = "install")]
        ArchiveKind::SevenZ => {
            unpack_7z(archive, &scratch)?;
        }
    }

    // Move (with optional root strip) from scratch into dest.
    let source_root = if strip_root {
        match single_child_dir(&scratch)? {
            Some(child) => child,
            None => scratch.clone(),
        }
    } else {
        scratch.clone()
    };

    move_dir_contents(&source_root, dest)?;
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.unpack(dest).map_err(|e| Error::io(dest, e))?;
    Ok(())
}

/// Unpack a `.zip` into `dest`, restoring Unix symlink entries as symlinks.
///
/// A zip stores a symlink as an entry whose Unix mode says `S_IFLNK` and whose
/// content is the link target. Written out as an ordinary file, `clang -> clang-21`
/// in the Android NDK became an 8-byte text file `clang-21`, which a shell then
/// ran as a script that looked `clang-21` up on PATH.
///
/// Links are created only after every regular entry is written, so no file is
/// ever written *through* a link the archive planted (the zip-slip variant tar
/// extractors guard against), and each target is validated by
/// [`zip_link_target`] to resolve inside the archive. `strip_root` matters
/// because a link that stays inside the scratch directory can still climb out
/// of the single top-level directory the install keeps.
fn unpack_zip(archive: &Path, dest: &Path, strip_root: bool) -> Result<()> {
    let f = File::open(archive).map_err(|e| Error::io(archive, e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f))
        .map_err(|e| Error::other(format!("zip open: {e}")))?;
    let mut links = Vec::new();
    let mut top_level = std::collections::BTreeSet::new();
    let mut nested = false;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::other(format!("zip entry {i}: {e}")))?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let mut components = relative.components();
        if let Some(first) = components.next() {
            top_level.insert(first.as_os_str().to_os_string());
            nested |= components.next().is_some();
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            let mut target = String::new();
            (&mut entry)
                .take(MAX_LINK_TARGET_BYTES + 1)
                .read_to_string(&mut target)
                .map_err(|e| Error::other(format!("zip symlink `{}`: {e}", relative.display())))?;
            if target.len() as u64 > MAX_LINK_TARGET_BYTES {
                return Err(Error::other(format!(
                    "zip symlink `{}` has an oversized target",
                    relative.display()
                )));
            }
            links.push((relative, target));
            continue;
        }
        let out_path = dest.join(&relative);
        if entry.is_dir() {
            create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                create_dir_all(parent)?;
            }
            let mut out = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| Error::io(&out_path, e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    // Mirrors `single_child_dir`: one top-level name with entries below it is
    // the directory that `strip_root` keeps as the install root.
    let kept_root = match top_level.first() {
        Some(root) if strip_root && nested && top_level.len() == 1 => Some(PathBuf::from(root)),
        _ => None,
    };
    let mut validated = Vec::with_capacity(links.len());
    for (relative, target) in links {
        let within = match &kept_root {
            Some(root) => relative.strip_prefix(root).unwrap_or(&relative),
            None => relative.as_path(),
        };
        let resolved = zip_link_target(within, &target).ok_or_else(|| {
            Error::other(format!(
                "zip symlink `{}` -> `{target}` points outside the archive",
                relative.display()
            ))
        })?;
        validated.push((dest.join(&relative), resolved));
    }
    restore_zip_links(dest, validated)
}

/// Longest symlink target accepted from a zip; real ones are a path or two.
const MAX_LINK_TARGET_BYTES: u64 = 4096;

/// Validate the target of a zip symlink at `link` (relative to the directory
/// the install keeps) and return it as a relative path, or `None` if it could
/// resolve outside that directory.
///
/// Accepted: leading `..` that never climb above the link's own depth, then
/// plain names. Refused: absolute targets, drive or `\` syntax (a separator on
/// Windows, so it is refused everywhere rather than read differently per
/// host), and any `..` after a name. The last rule is what keeps the check
/// lexical: `a/b/..` is only `a/` if `b` is not itself a link, and once a name
/// may be a link, climbing back out of it could leave the tree.
fn zip_link_target(link: &Path, target: &str) -> Option<PathBuf> {
    if target.is_empty() || target.starts_with('/') || target.contains(['\\', ':', '\0']) {
        return None;
    }
    let depth = link.components().count().checked_sub(1)?;
    let mut climbs = 0;
    let mut named = false;
    let mut resolved = PathBuf::new();
    for component in target.split('/') {
        match component {
            "" | "." => continue,
            ".." if named => return None,
            ".." => {
                climbs += 1;
                if climbs > depth {
                    return None;
                }
                resolved.push("..");
            }
            name => {
                named = true;
                resolved.push(name);
            }
        }
    }
    // Only `..`s or nothing: a link to one of its own ancestors, i.e. a loop.
    if !named {
        return None;
    }
    Some(resolved)
}

/// Create the validated links, after every regular entry exists.
///
/// Unix gets real symlinks. A link must not land below another link either:
/// creating `a/l` through a planted `a -> ..` would put `l` wherever `a`
/// points, so every ancestor between `dest` and the link has to be a real
/// directory. A link whose path an ordinary entry already took fails on
/// `EEXIST` instead of replacing it.
#[cfg(unix)]
fn restore_zip_links(dest: &Path, links: Vec<(PathBuf, PathBuf)>) -> Result<()> {
    for (link, target) in links {
        let parent = link.parent().unwrap_or(dest);
        let mut ancestor = parent;
        while ancestor != dest && ancestor.starts_with(dest) {
            if ancestor
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(Error::other(format!(
                    "zip symlink `{}` is inside another symlink",
                    link.display()
                )));
            }
            ancestor = ancestor.parent().unwrap_or(dest);
        }
        create_dir_all(parent)?;
        std::os::unix::fs::symlink(&target, &link).map_err(|e| Error::io(&link, e))?;
    }
    Ok(())
}

/// Windows materializes each link as a copy of what it points to.
///
/// A symlink there needs `SeCreateSymbolicLinkPrivilege` (an elevated process
/// or Developer Mode), so an ordinary install cannot count on one; the CAS
/// falls back to copying for the same reason. For the executables these
/// archives carry a copy runs exactly like the link, while the text stub
/// written before was wrong on every platform. Links are copied in dependency
/// order -- a link to a link waits for the first one -- and one whose target
/// the archive does not contain is left out, as it would dangle on Unix too.
#[cfg(not(unix))]
fn restore_zip_links(_dest: &Path, mut links: Vec<(PathBuf, PathBuf)>) -> Result<()> {
    loop {
        let before = links.len();
        let mut waiting = Vec::new();
        for (link, target) in links {
            let source = link.parent().unwrap_or(&link).join(&target);
            if source.symlink_metadata().is_err() {
                waiting.push((link, target));
                continue;
            }
            if link.symlink_metadata().is_ok() {
                return Err(Error::other(format!(
                    "zip symlink `{}` collides with another entry",
                    link.display()
                )));
            }
            if let Some(parent) = link.parent() {
                create_dir_all(parent)?;
            }
            copy_recursive(&source, &link)?;
        }
        if waiting.is_empty() {
            return Ok(());
        }
        if waiting.len() == before {
            for (link, target) in &waiting {
                tracing::debug!(
                    link = %link.display(),
                    target = %target.display(),
                    "zip symlink target is not in the archive; skipped"
                );
            }
            return Ok(());
        }
        links = waiting;
    }
}

/// Unpack a `.7z` into `dest`.
///
/// Entry paths come from the archive, so each one is checked to stay inside
/// `dest` before anything is written: a `..` component or an absolute path would
/// otherwise let an archive write outside the extraction scratch directory.
#[cfg(feature = "install")]
fn unpack_7z(archive: &Path, dest: &Path) -> Result<()> {
    let dest = dest.to_path_buf();
    sevenz_rust2::decompress_file_with_extract_fn(archive, &dest, |entry, reader, _unused| {
        let Some(relative) = safe_relative_path(entry.name()) else {
            // Skip rather than abort: mirrors how the zip path ignores entries
            // whose names escape the destination.
            return Ok(true);
        };
        let out_path = dest.join(relative);
        if entry.is_directory() {
            create_dir_all(&out_path).map_err(sevenz_error)?;
            return Ok(true);
        }
        if let Some(parent) = out_path.parent() {
            create_dir_all(parent).map_err(sevenz_error)?;
        }
        let mut out = File::create(&out_path).map_err(|e| {
            sevenz_rust2::Error::Io(e, format!("creating {}", out_path.display()).into())
        })?;
        std::io::copy(reader, &mut out).map_err(|e| {
            sevenz_rust2::Error::Io(e, format!("writing {}", out_path.display()).into())
        })?;
        Ok(true)
    })
    .map_err(|e| Error::other(format!("7z extract: {e}")))?;
    Ok(())
}

/// Convert an archive-supplied entry name into a path that cannot escape the
/// destination, or `None` when it is unusable.
///
/// 7z stores `/`-separated names, but an archive built on Windows can carry
/// `\` too, so both are treated as separators rather than as filename
/// characters.
#[cfg(feature = "install")]
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let mut safe = PathBuf::new();
    for component in name.split(['/', '\\']) {
        match component {
            // Leading `/` yields an empty first component.
            "" | "." => continue,
            ".." => return None,
            other => {
                // A drive-relative or UNC-ish component would make the join
                // absolute on Windows.
                if other.contains(':') {
                    return None;
                }
                safe.push(other);
            }
        }
    }
    if safe.as_os_str().is_empty() {
        None
    } else {
        Some(safe)
    }
}

#[cfg(feature = "install")]
fn sevenz_error(error: Error) -> sevenz_rust2::Error {
    sevenz_rust2::Error::Other(error.to_string().into())
}

/// If `dir` contains exactly one entry and it is a directory, return it.
fn single_child_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let mut children = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        children.push(entry.path());
    }
    if children.len() == 1 && children[0].is_dir() {
        Ok(Some(children.remove(0)))
    } else {
        Ok(None)
    }
}

/// Move everything inside `from` into `to` (merging). Uses rename when possible.
fn move_dir_contents(from: &Path, to: &Path) -> Result<()> {
    create_dir_all(to)?;
    for entry in std::fs::read_dir(from).map_err(|e| Error::io(from, e))? {
        let entry = entry.map_err(|e| Error::io(from, e))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        // Skip our own scratch dir if source_root == dest (strip_root == false path)
        if src == *to {
            continue;
        }
        if dst.exists() {
            let _ = std::fs::remove_dir_all(&dst);
            let _ = std::fs::remove_file(&dst);
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => {}
            Err(_) => {
                // cross-dir fallback: recursive copy
                copy_recursive(&src, &dst)?;
            }
        }
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        create_dir_all(dst)?;
        for entry in std::fs::read_dir(src).map_err(|e| Error::io(src, e))? {
            let entry = entry.map_err(|e| Error::io(src, e))?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst).map_err(|e| Error::io(dst, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a zip from `(name, Ok(contents) | Err(symlink target))` entries.
    fn zip_fixture(path: &Path, entries: &[(&str, std::result::Result<&[u8], &str>)]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        for (name, entry) in entries {
            match entry {
                Ok(contents) => {
                    zip.start_file(*name, options).unwrap();
                    zip.write_all(contents).unwrap();
                }
                Err(target) => zip.add_symlink(*name, *target, options).unwrap(),
            }
        }
        zip.finish().unwrap();
    }

    /// Regression: the NDK zip stores `bin/clang -> clang-21` as a symlink
    /// entry, and it was written out as an 8-byte text file `clang-21`, which a
    /// shell then ran as a script looking `clang-21` up on PATH.
    #[test]
    fn zip_symlink_entries_resolve_to_their_targets() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("ndk.zip");
        zip_fixture(
            &archive,
            &[
                ("ndk-r29/bin/clang-21", Ok(b"real compiler")),
                ("ndk-r29/bin/clang", Err("clang-21")),
                ("ndk-r29/bin/clang++", Err("clang")),
                ("ndk-r29/lib/clang-link", Err("../bin/clang-21")),
                ("ndk-r29/dangling", Err("not-in-archive")),
            ],
        );
        let dest = temp.path().join("installed");
        extract(&archive, &dest, ArchiveKind::Zip, true).unwrap();

        for link in ["bin/clang", "bin/clang++", "lib/clang-link"] {
            assert_eq!(
                std::fs::read(dest.join(link)).unwrap(),
                b"real compiler",
                "{link}"
            );
        }
        #[cfg(unix)]
        {
            for (link, target) in [
                ("bin/clang", "clang-21"),
                ("bin/clang++", "clang"),
                ("lib/clang-link", "../bin/clang-21"),
                ("dangling", "not-in-archive"),
            ] {
                let path = dest.join(link);
                assert!(
                    path.symlink_metadata().unwrap().file_type().is_symlink(),
                    "{link}"
                );
                assert_eq!(std::fs::read_link(&path).unwrap(), Path::new(target));
            }
        }
        // Windows copies what a link points to; a target the archive lacks
        // leaves nothing rather than a stub file.
        #[cfg(not(unix))]
        assert!(dest.join("dangling").symlink_metadata().is_err());
    }

    /// Zip slip through a link: each of these would let an install point
    /// outside the directory it keeps, the last one only once `strip_root` has
    /// lifted the top-level directory.
    #[test]
    fn zip_symlinks_that_leave_the_install_are_refused() {
        for (link, target) in [
            ("pkg/bin/l", "../../../outside"),
            ("pkg/bin/l", "/etc/passwd"),
            ("pkg/bin/l", "..\\..\\outside"),
            ("pkg/bin/l", "C:/Windows/System32/cmd.exe"),
            ("pkg/bin/l", "sub/../../../outside"),
            ("pkg/l", "../sibling"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let archive = temp.path().join("evil.zip");
            zip_fixture(
                &archive,
                &[("pkg/bin/tool", Ok(b"tool")), (link, Err(target))],
            );
            let error = extract(&archive, &temp.path().join("out"), ArchiveKind::Zip, true)
                .expect_err(target);
            assert!(
                error.to_string().contains("outside the archive"),
                "{target}: {error}"
            );
        }
    }

    #[test]
    fn zip_link_targets_stay_inside_the_kept_root() {
        let link = Path::new("bin/sub/clang");
        assert_eq!(
            zip_link_target(link, "clang-21"),
            Some(PathBuf::from("clang-21"))
        );
        assert_eq!(
            zip_link_target(link, "./../../lib/./x"),
            Some(["..", "..", "lib", "x"].iter().collect())
        );
        for refused in [
            "",
            ".",
            "..",
            "../../..",
            "/abs",
            "a\\b",
            "c:x",
            "a/../b",
            "nul\0byte",
        ] {
            assert_eq!(zip_link_target(link, refused), None, "{refused:?}");
        }
        // A top-level link cannot climb at all.
        assert_eq!(zip_link_target(Path::new("l"), "../x"), None);
    }

    /// A link may not be created through another link the archive planted,
    /// whichever order the two entries come in.
    #[cfg(unix)]
    #[test]
    fn zip_symlinks_are_never_created_inside_another_symlink() {
        for entries in [
            [("pkg/a", Err("sub")), ("pkg/a/l", Err("x"))],
            [("pkg/a/l", Err("x")), ("pkg/a", Err("sub"))],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let archive = temp.path().join("nested.zip");
            let mut all = vec![("pkg/sub/x", Ok(&b"x"[..]))];
            all.extend(entries);
            zip_fixture(&archive, &all);
            assert!(
                extract(&archive, &temp.path().join("out"), ArchiveKind::Zip, true).is_err(),
                "{entries:?}"
            );
        }
    }

    #[test]
    fn kind_from_name() {
        assert_eq!(
            ArchiveKind::from_name("x.tar.gz").unwrap(),
            ArchiveKind::TarGz
        );
        assert_eq!(
            ArchiveKind::from_name("x.tar.xz").unwrap(),
            ArchiveKind::TarXz
        );
        assert_eq!(
            ArchiveKind::from_name("x.tar.zst").unwrap(),
            ArchiveKind::TarZst
        );
        assert_eq!(ArchiveKind::from_name("x.zip").unwrap(), ArchiveKind::Zip);
        assert!(ArchiveKind::from_name("x.rar").is_err());
    }

    /// Windows GCC toolchains publish `.7z` exclusively, so the name must map to
    /// the archive kind rather than falling through to "unsupported".
    #[cfg(feature = "install")]
    #[test]
    fn kind_from_name_recognizes_7z() {
        assert_eq!(
            ArchiveKind::from_name("x86_64-15.1.0-release-posix-seh-ucrt-rt_v12-rev0.7z").unwrap(),
            ArchiveKind::SevenZ
        );
        assert_eq!(ArchiveKind::from_name("X.7Z").unwrap(), ArchiveKind::SevenZ);
    }

    /// Entry names come from the archive, so a crafted `..`, absolute path, or
    /// Windows drive prefix must never resolve to a location outside the
    /// destination.
    #[cfg(feature = "install")]
    #[test]
    fn safe_relative_path_refuses_escapes() {
        for escape in [
            "../outside.txt",
            "a/../../outside.txt",
            "..\\outside.txt",
            "a\\..\\..\\outside.txt",
            "C:\\Windows\\System32\\evil.dll",
            "c:/windows/evil.dll",
            "",
            "/",
            "./",
        ] {
            assert!(
                safe_relative_path(escape).is_none(),
                "expected `{escape}` to be refused"
            );
        }

        // Ordinary entries survive, with both separators treated as such.
        assert_eq!(
            safe_relative_path("bin/gcc.exe").unwrap(),
            PathBuf::from("bin").join("gcc.exe")
        );
        assert_eq!(
            safe_relative_path("/lib/gcc/x.a").unwrap(),
            PathBuf::from("lib").join("gcc").join("x.a")
        );
        assert_eq!(
            safe_relative_path("bin\\ld.exe").unwrap(),
            PathBuf::from("bin").join("ld.exe")
        );
        assert_eq!(
            safe_relative_path("./bin/./as.exe").unwrap(),
            PathBuf::from("bin").join("as.exe")
        );
    }

    /// A real round trip: build a `.7z`, extract it through the pipeline, and
    /// confirm contents, nesting, and `strip_root` all behave like the other
    /// archive kinds.
    #[cfg(feature = "install")]
    #[test]
    fn extracts_a_real_7z_archive_with_strip_root() {
        let temp = tempfile::tempdir().unwrap();
        // `compress_to_path` stores the *contents* of the directory it is given,
        // so wrap the payload one level deeper to produce the single top-level
        // directory that real toolchain archives have.
        let staging = temp.path().join("staging");
        let source = staging.join("toolchain-1.0.0");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir_all(source.join("lib/gcc")).unwrap();
        std::fs::write(source.join("bin/gcc.exe"), b"fake compiler").unwrap();
        std::fs::write(source.join("lib/gcc/libgcc.a"), b"fake archive").unwrap();

        let archive = temp.path().join("toolchain.7z");
        sevenz_rust2::compress_to_path(&staging, &archive).expect("building fixture archive");

        // strip_root lifts the single `toolchain-1.0.0/` wrapper.
        let dest = temp.path().join("stripped");
        extract(&archive, &dest, ArchiveKind::SevenZ, true).unwrap();
        assert_eq!(
            std::fs::read(dest.join("bin/gcc.exe")).unwrap(),
            b"fake compiler"
        );
        assert_eq!(
            std::fs::read(dest.join("lib/gcc/libgcc.a")).unwrap(),
            b"fake archive"
        );
        // The scratch directory must not survive into the install root.
        assert!(!dest.join(".osdk-extract-tmp").exists());

        // Without stripping, the wrapper directory is preserved.
        let kept = temp.path().join("kept");
        extract(&archive, &kept, ArchiveKind::SevenZ, false).unwrap();
        assert!(kept.join("toolchain-1.0.0/bin/gcc.exe").is_file());
    }

    #[test]
    fn extract_targz_with_root_strip() {
        // Build a tar.gz with a single root dir: pkg/bin/tool, pkg/README
        let td = tempfile::tempdir().unwrap();
        let archive = td.path().join("a.tar.gz");
        {
            let f = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut add = |name: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, data).unwrap();
            };
            add("pkg/README", b"hi");
            add("pkg/bin/tool", b"binary");
            b.finish().unwrap();
        }
        let dest = td.path().join("out");
        extract(&archive, &dest, ArchiveKind::TarGz, true).unwrap();
        // root "pkg/" should be stripped
        assert!(dest.join("README").exists());
        assert!(dest.join("bin/tool").exists());
        assert_eq!(std::fs::read(dest.join("README")).unwrap(), b"hi");
    }
}
