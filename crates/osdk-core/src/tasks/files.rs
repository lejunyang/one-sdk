//! The third tier: tasks that live as script files.
//!
//! A script outgrows a TOML string somewhere around twenty lines -- at that
//! point the editor stops highlighting it, the linter stops seeing it, and the
//! escaping starts to hurt. Moving it into `osdk-tasks/` costs nothing and
//! gets all of that back.
//!
//! Two rules here are worth stating because the obvious alternative is wrong:
//!
//! - **Discovery is a shallow read of declared directories, not a recursive
//!   walk.** mise started with filesystem traversal and deprecated it, citing
//!   discovery speed; the same measurement that produced the 46x freshness
//!   lesson applies here, since this runs on every `osdk task list`.
//! - **Windows visibility is decided by extension or shebang, never by an
//!   execute bit**, which NTFS does not have. A file with neither is usable on
//!   Unix and invisible on Windows, so [`FileTask::windows_visible`] records
//!   the verdict and `osdk task list` says so rather than letting the task
//!   quietly disappear.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Directories searched for file tasks, relative to the config file.
///
/// Both spellings exist because a dotted directory hides the tasks from casual
/// listing, which some projects want and others find obstructive.
pub const DEFAULT_TASK_DIRS: &[&str] = &["osdk-tasks", ".osdk-tasks"];

/// Extensions Windows will execute directly.
///
/// Mirrors what `cmd` treats as runnable. `.ps1` is included even though `cmd`
/// cannot launch it, because the runner special-cases it: see [`launch_argv`].
pub const WINDOWS_EXECUTABLE_EXTENSIONS: &[&str] = &["exe", "bat", "cmd", "com", "ps1", "vbs"];

/// One discovered script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTask {
    /// Task name, derived from the path (`test/units` -> `test:units`).
    pub name: String,
    /// Absolute path to the script.
    pub path: PathBuf,
    /// `description` from the `#OSDK` header, if present.
    pub description: Option<String>,
    /// `depends` from the header.
    pub depends: Vec<String>,
    /// Whether Windows can execute this file at all.
    ///
    /// Recorded rather than filtered so the listing can explain the absence.
    /// A task that simply vanishes sends the reader looking for a typo.
    pub windows_visible: bool,
}

/// Turn a path relative to a task directory into a task name.
///
/// `test/units.sh` becomes `test:units`. The extension is dropped so the same
/// task can be spelled `build` on Unix and `build.ps1` on Windows without the
/// name changing -- that pairing is how a file task gets a platform variant,
/// since unlike a TOML task it has nowhere to put a second command.
fn task_name_from(relative: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let text = component.as_os_str().to_str()?;
        if components.peek().is_none() {
            // Final component: strip the extension.
            let stem = Path::new(text).file_stem()?.to_str()?;
            // `_default` names the directory itself, matching mise: a
            // `test/_default` script is simply `test`.
            if stem == "_default" {
                break;
            }
            parts.push(stem.to_string());
        } else {
            parts.push(text.to_string());
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(":"))
}

/// Whether Windows can launch this file.
fn windows_can_execute(path: &Path, first_line: Option<&str>) -> bool {
    let has_known_extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| WINDOWS_EXECUTABLE_EXTENSIONS.contains(&ext.as_str()));
    // A shebang is enough: the runner reads it and launches the interpreter
    // itself, which is how an extensionless `build` script still works.
    has_known_extension || first_line.is_some_and(|line| line.starts_with("#!"))
}

/// Parse `#OSDK key=value` header lines.
///
/// Stops at the first line that is neither a comment nor blank, so the header
/// cannot be hidden in the middle of a script where nobody would look for it.
fn parse_header(contents: &str) -> (Option<String>, Vec<String>) {
    let mut description = None;
    let mut depends = Vec::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("#!") {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix('#') else {
            break;
        };
        let rest = rest.trim();
        let Some(directive) = rest.strip_prefix("OSDK") else {
            continue;
        };
        let Some((key, value)) = directive.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "description" => description = Some(value.to_string()),
            "depends" => {
                depends = value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    }
    (description, depends)
}

/// Discover file tasks under `root`, searching only `dirs`.
///
/// Missing directories are not an error: a project that declares neither is the
/// common case, and reporting it would make every `osdk task list` noisy.
pub fn discover(root: &Path, dirs: &[String]) -> Result<BTreeMap<String, FileTask>> {
    let mut found = BTreeMap::new();
    let search: Vec<&str> = if dirs.is_empty() {
        DEFAULT_TASK_DIRS.to_vec()
    } else {
        dirs.iter().map(String::as_str).collect()
    };

    for dir in search {
        let base = root.join(dir);
        if !base.is_dir() {
            continue;
        }
        collect_from(&base, &base, &mut found)?;
    }
    Ok(found)
}

fn collect_from(base: &Path, current: &Path, found: &mut BTreeMap<String, FileTask>) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|error| Error::io(current, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| Error::io(current, error))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| Error::io(&path, error))?;

        if file_type.is_dir() {
            // One level of nesting per `:` in the name; this is bounded by the
            // directory layout the user created, not by a glob that could walk
            // an unrelated tree.
            collect_from(base, &path, found)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let Some(name) = task_name_from(relative) else {
            continue;
        };

        // Read only the head: a task file can be large, and everything the
        // header can say is at the top by construction.
        let contents = read_head(&path)?;
        let first_line = contents.lines().next();
        let windows_visible = windows_can_execute(&path, first_line);
        let (description, depends) = parse_header(&contents);

        // A `build.ps1` next to a `build` shebang script is the platform
        // variant pairing; on Windows the native one wins, elsewhere it loses.
        //
        // Both candidates are scored rather than compared with a predicate: on
        // Windows *both* files are executable -- one by extension, one by
        // shebang -- so `windows_visible && !existing.windows_visible` is never
        // true, and the winner silently becomes whichever `read_dir` happened
        // to yield first.
        let prefer_this = match found.get(&name) {
            None => true,
            Some(existing) => variant_rank(&path) > variant_rank(&existing.path),
        };
        if !prefer_this {
            continue;
        }

        found.insert(
            name.clone(),
            FileTask {
                name,
                path: path.clone(),
                description,
                depends,
                windows_visible,
            },
        );
    }
    Ok(())
}

/// Which PowerShell to launch a `.ps1` with.
///
/// `pwsh` (7+) is preferred where it exists, but it is a separate download:
/// Windows ships `powershell.exe` (5.1) and nothing else. Hardcoding `pwsh`
/// would make every `.ps1` task fail on a stock Windows with a bare
/// "program not found", which points at the task rather than at the real cause.
///
/// Off Windows there is no fallback to offer -- a `.ps1` there needs PowerShell
/// installed -- so `pwsh` is returned and the spawn error speaks for itself.
fn powershell_program() -> String {
    if !cfg!(windows) {
        return "pwsh".into();
    }
    if which_exists("pwsh") {
        "pwsh".into()
    } else {
        // 5.1 understands `-File` too, so the rest of the argv is unchanged.
        "powershell".into()
    }
}

/// Whether a bare program name resolves on PATH.
///
/// Deliberately a PATH walk rather than spawning the program with `--version`:
/// this runs while building a plan, including under `--dry-run`, and launching
/// a shell just to ask whether it exists would be a visible cost on a path that
/// is supposed to be inert.
fn which_exists(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|entry| !entry.is_empty())
            .map(|entry| entry.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    std::env::split_paths(&paths).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        let candidate = dir.join(program);
        if candidate.is_file() {
            return true;
        }
        extensions.iter().any(|ext| {
            let mut with_ext = program.to_string();
            with_ext.push_str(ext);
            dir.join(with_ext).is_file()
        })
    })
}

/// Rank a candidate among same-named files, higher winning.
///
/// Only `.ps1` matters: it is the one extension whose desirability flips with
/// the platform. Everything else ties, and a tie keeps the incumbent.
fn variant_rank(path: &Path) -> i32 {
    let is_ps1 = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ps1"));
    match (cfg!(windows), is_ps1) {
        (true, true) => 1,
        (true, false) => 0,
        (false, true) => -1,
        (false, false) => 0,
    }
}

/// Read enough of a file to cover its header.
fn read_head(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|error| Error::io(path, error))?;
    let mut buffer = vec![0u8; 4096];
    let read = file
        .read(&mut buffer)
        .map_err(|error| Error::io(path, error))?;
    buffer.truncate(read);
    // A binary task file is legitimate (a compiled helper); it simply has no
    // header, so losing invalid UTF-8 here is the right trade.
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// The argv that launches `script`.
///
/// Three cases, and the middle one is the reason this is not just `[path]`:
/// `cmd` cannot launch a `.ps1`, so PowerShell scripts need an explicit
/// interpreter even though Windows "can execute" them in the listing sense.
pub fn launch_argv(script: &Path) -> Vec<String> {
    let path = script.to_string_lossy().to_string();
    let extension = script
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();

    if extension == "ps1" {
        // `-File` rather than `-Command` so arguments reach the script as
        // arguments instead of being re-parsed as PowerShell source.
        return vec![powershell_program(), "-File".into(), path];
    }

    if cfg!(windows) {
        // Everything else Windows recognises runs directly.
        return vec![path];
    }

    // On Unix the kernel honours the shebang, so exec the file itself.
    vec![path]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn names_come_from_the_path_with_colons_for_directories() {
        assert_eq!(task_name_from(Path::new("build")).as_deref(), Some("build"));
        assert_eq!(
            task_name_from(Path::new("build.sh")).as_deref(),
            Some("build")
        );
        assert_eq!(
            task_name_from(Path::new("test/units.sh")).as_deref(),
            Some("test:units")
        );
        // `_default` names the directory itself.
        assert_eq!(
            task_name_from(Path::new("test/_default")).as_deref(),
            Some("test")
        );
    }

    /// The extension is dropped so `build` and `build.ps1` are one task with a
    /// platform variant, not two tasks.
    #[test]
    fn a_ps1_sibling_is_the_same_task_not_a_second_one() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "osdk-tasks/build", "#!/bin/sh\necho unix\n");
        write(root, "osdk-tasks/build.ps1", "Write-Output 'windows'\n");

        let found = discover(root, &[]).unwrap();
        assert_eq!(found.len(), 1, "{:?}", found.keys().collect::<Vec<_>>());
        assert!(found.contains_key("build"));

        let chosen = &found["build"];
        if cfg!(windows) {
            assert!(
                chosen.path.extension().is_some_and(|e| e == "ps1"),
                "Windows should prefer the .ps1: {:?}",
                chosen.path
            );
        } else {
            assert!(
                chosen.path.extension().is_none(),
                "Unix should prefer the shebang script: {:?}",
                chosen.path
            );
        }
    }

    /// The pairing must not depend on directory order.
    ///
    /// The first implementation asked "is this one visible and the other not",
    /// which on Windows is never true because both files are executable -- so
    /// the winner was whichever `read_dir` yielded first. Creating the files in
    /// both orders is what exposes that; a single order passes either way.
    #[test]
    fn the_variant_choice_does_not_depend_on_directory_order() {
        for (first, second) in [("build", "build.ps1"), ("build.ps1", "build")] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let body = |name: &str| {
                if name.ends_with(".ps1") {
                    "Write-Output 'windows'\n"
                } else {
                    "#!/bin/sh\necho unix\n"
                }
            };
            write(root, &format!("osdk-tasks/{first}"), body(first));
            write(root, &format!("osdk-tasks/{second}"), body(second));

            let found = discover(root, &[]).unwrap();
            assert_eq!(found.len(), 1);
            let chosen = &found["build"];
            let chose_ps1 = chosen
                .path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
            assert_eq!(
                chose_ps1,
                cfg!(windows),
                "created {first} then {second}, picked {:?}",
                chosen.path
            );
        }
    }

    #[test]
    fn header_directives_are_parsed() {
        let (description, depends) = parse_header(
            "#!/bin/sh\n#OSDK description=\"Build everything\"\n#OSDK depends=fetch, lint\necho hi\n",
        );
        assert_eq!(description.as_deref(), Some("Build everything"));
        assert_eq!(depends, vec!["fetch", "lint"]);
    }

    /// A header below the first non-comment line would be somewhere nobody
    /// looks, so parsing stops before it.
    #[test]
    fn a_header_after_real_code_is_ignored() {
        let (description, _) = parse_header("#!/bin/sh\necho hi\n#OSDK description=\"late\"\n");
        assert_eq!(description, None);
    }

    #[test]
    fn windows_visibility_needs_an_extension_or_a_shebang() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let plain = write(root, "osdk-tasks/plain", "echo no shebang\n");
        let shebang = write(root, "osdk-tasks/withbang", "#!/bin/sh\necho hi\n");
        let batch = write(root, "osdk-tasks/script.bat", "@echo off\n");

        assert!(!windows_can_execute(&plain, Some("echo no shebang")));
        assert!(windows_can_execute(&shebang, Some("#!/bin/sh")));
        assert!(windows_can_execute(&batch, Some("@echo off")));
    }

    /// An invisible task must still be discovered, so the listing can explain
    /// why it cannot run instead of leaving the reader hunting for a typo.
    #[test]
    fn a_windows_invisible_task_is_recorded_rather_than_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "osdk-tasks/plain", "echo no shebang\n");

        let found = discover(root, &[]).unwrap();
        assert!(found.contains_key("plain"));
        assert!(!found["plain"].windows_visible);
    }

    #[test]
    fn nested_directories_become_namespaced_names() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "osdk-tasks/test/units.sh", "#!/bin/sh\necho units\n");
        write(root, "osdk-tasks/test/e2e.sh", "#!/bin/sh\necho e2e\n");

        let found = discover(root, &[]).unwrap();
        let mut names: Vec<&str> = found.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, vec!["test:e2e", "test:units"]);
    }

    #[test]
    fn a_missing_task_directory_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let found = discover(temp.path(), &[]).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn a_custom_directory_replaces_the_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "osdk-tasks/ignored", "#!/bin/sh\n");
        write(root, "mytasks/wanted", "#!/bin/sh\n");

        let found = discover(root, &["mytasks".to_string()]).unwrap();
        assert!(found.contains_key("wanted"));
        assert!(
            !found.contains_key("ignored"),
            "declaring a directory replaces the defaults rather than adding to them"
        );
    }

    /// `cmd` cannot launch a `.ps1`, so one is always run through PowerShell.
    #[test]
    fn a_ps1_is_launched_through_powershell_because_cmd_cannot() {
        let argv = launch_argv(Path::new("scripts/deploy.ps1"));
        // Which of the two it picks depends on what this machine has; both are
        // PowerShell and both accept `-File`.
        assert!(
            argv[0] == "pwsh" || argv[0] == "powershell",
            "unexpected interpreter: {argv:?}"
        );
        assert_eq!(argv[1], "-File");
        assert!(argv[2].ends_with("deploy.ps1"));
    }

    /// Windows ships only 5.1, so `pwsh` must not be assumed.
    ///
    /// Hardcoding it made every `.ps1` task fail on a stock Windows with a bare
    /// "program not found" -- an error pointing at the task rather than at the
    /// missing optional download.
    #[test]
    fn a_stock_windows_without_pwsh_falls_back_to_the_bundled_powershell() {
        if !cfg!(windows) {
            return;
        }
        // An empty PATH is the strongest form of "pwsh is not installed".
        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let chosen = powershell_program();
        match original {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(
            chosen, "powershell",
            "without pwsh on PATH the bundled 5.1 must be used"
        );
    }

    #[test]
    fn which_exists_agrees_with_reality_in_both_directions() {
        // Present on every platform this builds for.
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which_exists(shell), "{shell} should resolve on PATH");
        assert!(
            !which_exists("osdk-definitely-not-a-real-program-xyz"),
            "a nonexistent program must not resolve"
        );
    }

    #[test]
    fn other_scripts_are_launched_directly() {
        let argv = launch_argv(Path::new("scripts/build.sh"));
        assert_eq!(argv.len(), 1);
        assert!(argv[0].ends_with("build.sh"));
    }

    /// The header is the file task's only way to declare a dependency, so a
    /// dropped one would fail at run time with a missing prerequisite rather
    /// than at parse time.
    #[test]
    fn discovery_carries_the_header_through() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "osdk-tasks/release",
            "#!/bin/sh\n#OSDK description=\"Ship it\"\n#OSDK depends=build, test\necho go\n",
        );

        let found = discover(root, &[]).unwrap();
        let task = &found["release"];
        assert_eq!(task.description.as_deref(), Some("Ship it"));
        assert_eq!(task.depends, vec!["build", "test"]);
    }

    #[test]
    fn a_binary_file_is_discovered_without_a_header() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = root.join("osdk-tasks/helper.exe");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Invalid UTF-8 must not abort discovery.
        std::fs::write(&path, [0xFFu8, 0xFE, 0x00, 0x01, 0x02]).unwrap();

        let found = discover(root, &[]).unwrap();
        assert!(found.contains_key("helper"));
        assert_eq!(found["helper"].description, None);
    }
}
