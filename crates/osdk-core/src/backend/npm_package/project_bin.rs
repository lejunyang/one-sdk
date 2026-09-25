//! `project_bin` free functions split from npm_package.rs.

use super::*;

pub(crate) fn package_install_dir(project_dir: &Path, package: &str) -> PathBuf {
    let node_modules = project_dir.join("node_modules");
    if let Some(rest) = package.strip_prefix('@') {
        let (scope, name) = rest.split_once('/').expect("validated scoped package");
        return node_modules.join(format!("@{scope}")).join(name);
    }
    node_modules.join(package)
}

pub fn package_bin_entries(
    manifest: &serde_json::Value,
    package: &str,
) -> Result<Vec<(String, PathBuf)>> {
    let mut entries = match manifest.get("bin") {
        Some(serde_json::Value::String(path)) if !path.is_empty() => vec![(
            package.rsplit('/').next().unwrap_or(package).to_string(),
            PathBuf::from(path),
        )],
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .map(|(name, path)| {
                let path = path.as_str().filter(|path| !path.is_empty()).ok_or_else(|| {
                    Error::other(format!(
                        "npm package {package} declares a non-string or empty bin target for `{name}`"
                    ))
                })?;
                Ok((name.clone(), PathBuf::from(path)))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) | None => Vec::new(),
    };
    for (name, _) in &entries {
        if !valid_project_bin_name(name) {
            return Err(Error::other(format!(
                "npm package {package} declares unsafe bin name `{name}`"
            )));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    #[cfg(windows)]
    {
        let mut folded = BTreeSet::new();
        if entries
            .iter()
            .any(|(name, _)| !folded.insert(name.to_ascii_lowercase()))
        {
            return Err(Error::other(format!(
                "npm package {package} declares colliding Windows bin names"
            )));
        }
    }
    if entries.is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_dynamic_no_validated_executables",
            tool = format!("npm:{package}")
        )));
    }
    Ok(entries)
}

pub(crate) fn valid_project_bin_name(name: &str) -> bool {
    // The last clause folds in the cmd/PowerShell metacharacters the global
    // install path used to reject on its own: a bin name reaching a Windows
    // .cmd launcher must not carry shell operators. Keeping it here means both
    // the project and global paths share one rule instead of two that drift.
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && !is_windows_reserved_component(name)
        && !name.contains(['%', '!', '^', '&', '|', '<', '>', '(', ')'])
}

#[cfg(not(windows))]
pub(crate) fn validate_unix_project_launcher(
    launcher: &Path,
    name: &str,
    package: &str,
    declared_target: &Path,
) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(launcher).map_err(|error| Error::io(launcher, error))?;
    if metadata.file_type().is_symlink() {
        let actual = dunce::canonicalize(launcher).map_err(|error| Error::io(launcher, error))?;
        if actual == declared_target {
            return Ok(());
        }
        return Err(Error::other(format!(
            "project launcher `{name}` does not point at the bin declared by {package}"
        )));
    }
    if !metadata.is_file() || metadata.len() > PROJECT_NPM_LAUNCHER_MAX_BYTES {
        return Err(Error::other(format!(
            "project launcher `{name}` for {package} is not a small regular wrapper"
        )));
    }
    let text = std::fs::read_to_string(launcher).map_err(|error| Error::io(launcher, error))?;
    if let Some(relative) = text
        .lines()
        .find_map(|line| line.strip_prefix("# aube-bin-shim v2 target="))
    {
        let relative = PathBuf::from(relative);
        if relative.is_absolute()
            || relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(Error::other(format!(
                "project launcher `{name}` for {package} has an unsafe shim target"
            )));
        }
        let target = launcher.parent().unwrap_or(Path::new("")).join(relative);
        let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
        return (actual == declared_target).then_some(()).ok_or_else(|| {
            Error::other(format!(
                "project launcher `{name}` does not execute the bin declared by {package}"
            ))
        });
    }
    let target = parse_unix_exec_wrapper(&text, launcher.parent().unwrap_or(Path::new("")))
        .ok_or_else(|| {
            Error::other(format!(
                "project launcher `{name}` for {package} cannot be resolved safely"
            ))
        })?;
    let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
    if actual == declared_target {
        Ok(())
    } else {
        Err(Error::other(format!(
            "project launcher `{name}` does not execute the bin declared by {package}"
        )))
    }
}

#[cfg(not(windows))]
pub(crate) fn parse_unix_exec_wrapper(text: &str, wrapper_dir: &Path) -> Option<PathBuf> {
    let mut command = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("#!") || line.starts_with('#') {
            continue;
        }
        if line.contains([';', '&', '|', '>', '<', '`']) || line.contains("$(") {
            return None;
        }
        let words = shell_words(line)?;
        if words.len() != 4
            || words[0] != "exec"
            || words[1] != "node"
            || !matches!(words[3].as_str(), "$@" | "${@}")
            || command.replace(words[2].clone()).is_some()
        {
            return None;
        }
    }
    let raw = command?;
    let expanded = raw
        .strip_prefix("$basedir/")
        .or_else(|| raw.strip_prefix("${basedir}/"))
        .map(|suffix| wrapper_dir.join(suffix))
        .unwrap_or_else(|| PathBuf::from(raw));
    Some(expanded)
}

#[cfg(not(windows))]
pub(crate) fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Some(expected), actual) if actual == expected => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, actual) if actual.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (Some('\''), actual) => current.push(actual),
            (_, '\\') => current.push(chars.next()?),
            (_, actual) => current.push(actual),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

#[cfg(windows)]
pub(crate) fn validate_windows_project_launcher(
    bin_dir: &Path,
    launcher: &Path,
    name: &str,
    package: &str,
    declared_target: &Path,
) -> Result<()> {
    let extension = launcher
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), Some("cmd") | Some("bat")) {
        return Err(Error::other(format!(
            "project launcher `{name}` for {package} is an opaque Windows executable"
        )));
    }
    for shadow in [bin_dir.join(format!("{name}.exe")), bin_dir.join(name)] {
        if shadow.exists() {
            return Err(Error::other(format!(
                "project launcher `{name}` for {package} has an opaque Windows shadow"
            )));
        }
    }
    let target = parse_windows_project_wrapper(launcher)?;
    let actual = dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
    if actual != declared_target {
        return Err(Error::other(format!(
            "project launcher `{name}` does not execute the bin declared by {package}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn parse_windows_project_wrapper(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_file() || metadata.len() > PROJECT_NPM_LAUNCHER_MAX_BYTES {
        return Err(Error::other(format!(
            "project npm wrapper is not a small regular file: {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    if text.contains(OSDK_PROJECT_NPM_CMD_MARKER) {
        let relative = parse_osdk_project_cmd_wrapper(&text).ok_or_else(|| {
            Error::other(format!(
                "invalid osdk project npm wrapper template: {}",
                path.display()
            ))
        })?;
        return path
            .parent()
            .map(|parent| parent.join(relative.replace('\\', "/")))
            .ok_or_else(|| {
                Error::other(format!(
                    "project npm wrapper does not execute a declared target: {}",
                    path.display()
                ))
            });
    }
    let lower = text.to_ascii_lowercase();
    if text.contains('\0') || lower.contains("powershell") || lower.contains("cmd /") {
        return Err(Error::other(format!(
            "project npm wrapper contains an unsupported command: {}",
            path.display()
        )));
    }
    let recognized = is_relative_target_windows_wrapper(&text) || is_npm_windows_wrapper(&text);
    if !recognized {
        return Err(Error::other(format!(
            "project npm wrapper does not match a recognized safe template: {}",
            path.display()
        )));
    }
    let normalized = text.replace("%dp0%", "%~dp0");
    let marker = "\"%~dp0\\";
    let mut targets = normalized
        .match_indices(marker)
        .filter_map(|(start, _)| {
            let suffix = &normalized[start + marker.len()..];
            let end = suffix.find('\"')?;
            let relative = &suffix[..end];
            let rest = suffix[end + 1..].trim_start();
            rest.starts_with("%*").then(|| relative.to_string())
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return Err(Error::other(format!(
            "project npm wrapper cannot be resolved unambiguously: {}",
            path.display()
        )));
    }
    let relative = PathBuf::from(
        targets
            .pop_first()
            .expect("validated one target")
            .replace('\\', "/"),
    );
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::other(format!(
            "project npm wrapper has an unsafe target: {}",
            path.display()
        )));
    }
    path.parent()
        .map(|parent| parent.join(relative))
        .ok_or_else(|| {
            Error::other(format!(
                "project npm wrapper does not execute a declared target: {}",
                path.display()
            ))
        })
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn render_osdk_project_cmd_wrapper(relative_target: &str) -> Result<String> {
    if !valid_osdk_project_cmd_target(relative_target) {
        return Err(Error::other(
            "project npm bin path cannot be represented safely in cmd.exe",
        ));
    }
    Ok(format!(
        "@echo off\r\n{OSDK_PROJECT_NPM_CMD_MARKER}\r\nnode \"%~dp0{relative_target}\" %*\r\n"
    ))
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn parse_osdk_project_cmd_wrapper(text: &str) -> Option<&str> {
    let prefix = format!("@echo off\r\n{OSDK_PROJECT_NPM_CMD_MARKER}\r\nnode \"%~dp0");
    let relative_target = text.strip_prefix(&prefix)?.strip_suffix("\" %*\r\n")?;
    valid_osdk_project_cmd_target(relative_target).then_some(relative_target)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn valid_osdk_project_cmd_target(relative_target: &str) -> bool {
    !relative_target.is_empty()
        && !relative_target.starts_with(['/', '\\'])
        && relative_target.as_bytes().get(1) != Some(&b':')
        && !relative_target.contains('/')
        && !relative_target.chars().any(|character| {
            character.is_control()
                || matches!(character, '%' | '!' | '"' | '&' | '|' | '<' | '>' | '^')
        })
}

#[cfg(windows)]
/// Recognize a `.cmd` wrapper that dispatches through a relative target.
///
/// npm writes its own wrapper shape, handled by [`is_npm_windows_wrapper`]; this
/// one is still accepted so launchers written by earlier versions keep
/// validating instead of being reported as tampered.
pub(crate) fn is_relative_target_windows_wrapper(text: &str) -> bool {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    if lines.next() != Some("@SETLOCAL") {
        return false;
    }
    let mut line = lines.next();
    if line.is_some_and(|line| line.starts_with("@SET NODE_PATH=")) {
        line = lines.next();
    }
    let Some(if_line) = line else {
        return false;
    };
    if if_line.starts_with("@\"%~dp0\\") && if_line.ends_with("\" %*") {
        return lines.next().is_none();
    }
    if !if_line.starts_with("@IF EXIST \"%~dp0\\") || !if_line.ends_with(".exe\" (") {
        return false;
    }
    let Some(local) = lines.next() else {
        return false;
    };
    if !local.starts_with("  \"%~dp0\\") || !local.ends_with("\" %*") {
        return false;
    }
    if lines.next() != Some(") ELSE (") || lines.next() != Some("  @SET PATHEXT=%PATHEXT:;.JS;=;%")
    {
        return false;
    }
    let Some(fallback) = lines.next() else {
        return false;
    };
    fallback.starts_with("  ")
        && fallback.contains(" \"%~dp0\\")
        && fallback.ends_with("\" %*")
        && lines.next() == Some(")")
        && lines.next().is_none()
}

#[cfg(windows)]
pub(crate) fn is_npm_windows_wrapper(text: &str) -> bool {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    for expected in [
        "@ECHO off",
        "GOTO start",
        ":find_dp0",
        "SET dp0=%~dp0",
        "EXIT /b",
        ":start",
        "SETLOCAL",
        "CALL :find_dp0",
    ] {
        if lines.next() != Some(expected) {
            return false;
        }
    }
    let remaining = lines.collect::<Vec<_>>().join("\n");
    if remaining.contains("\nGOTO ")
        || remaining.contains("\nCALL ")
        || remaining.contains("\nSTART ")
    {
        return false;
    }
    if remaining.contains("IF EXIST \"%dp0%\\") {
        remaining.contains("SET \"_prog=")
            && remaining.contains("SET PATHEXT=%PATHEXT:;.JS;=;%")
            && remaining
                .contains("endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"")
    } else {
        remaining
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            == 1
            && remaining.contains("\"%dp0%\\")
            && remaining.trim_end().ends_with("\" %*")
    }
}

pub(crate) fn discover_bins(install_root: &Path, bin_dir: &Path) -> Result<Vec<DynamicToolBin>> {
    let canonical_root =
        std::fs::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    let mut bins = Vec::new();
    for name in discover_bin_names(bin_dir)? {
        let absolute = resolve_bin_target(bin_dir, &name)?;
        let relative = absolute
            .strip_prefix(&canonical_root)
            .map_err(|_| {
                Error::other(crate::t!(
                    "err.npm_bin_outside_install_root",
                    name = name,
                    path = install_root.display()
                ))
            })?
            .to_path_buf();
        bins.push(DynamicToolBin {
            name,
            path: relative.to_string_lossy().replace('\\', "/"),
            ..Default::default()
        });
    }
    if bins.is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_bins_not_discovered",
            path = bin_dir.display()
        )));
    }
    Ok(bins)
}

pub(crate) fn discover_global_bins(
    install_root: &Path,
    bin_dir: &Path,
) -> Result<Vec<DynamicToolBin>> {
    let canonical_root =
        std::fs::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    let canonical_bin_dir =
        std::fs::canonicalize(bin_dir).map_err(|error| Error::io(bin_dir, error))?;
    let relative_bin_dir = canonical_bin_dir
        .strip_prefix(&canonical_root)
        .map_err(|_| {
            Error::other(crate::t!(
                "err.npm_bin_outside_install_root",
                name = bin_dir.display(),
                path = install_root.display()
            ))
        })?;
    let mut bins = Vec::new();
    for name in discover_bin_names(bin_dir)? {
        let absolute = resolve_bin_target(bin_dir, &name)?;
        absolute.strip_prefix(&canonical_root).map_err(|_| {
            Error::other(crate::t!(
                "err.npm_bin_outside_install_root",
                name = name,
                path = install_root.display()
            ))
        })?;
        let entry = global_bin_entry(bin_dir, &name).ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_bin_target_unresolved",
                name = name,
                path = bin_dir.display()
            ))
        })?;
        let file_name = entry.file_name().ok_or_else(|| {
            Error::other(crate::t!(
                "err.npm_bin_target_unresolved",
                name = name,
                path = bin_dir.display()
            ))
        })?;
        let relative = relative_bin_dir.join(file_name);
        bins.push(DynamicToolBin {
            name,
            path: relative.to_string_lossy().replace('\\', "/"),
            ..Default::default()
        });
    }
    if bins.is_empty() {
        return Err(Error::other(crate::t!(
            "err.npm_bins_not_discovered",
            path = bin_dir.display()
        )));
    }
    Ok(bins)
}

pub fn global_bin_entry(bin_dir: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.exe")),
        bin_dir.join(format!("{name}.bat")),
        bin_dir.join(name),
    ];
    #[cfg(not(windows))]
    let candidates = [bin_dir.join(name)];
    candidates.into_iter().find(|candidate| candidate.exists())
}

pub(crate) fn discover_bin_names(bin_dir: &Path) -> Result<Vec<String>> {
    let read_dir = std::fs::read_dir(bin_dir).map_err(|error| Error::io(bin_dir, error))?;
    let mut names = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|error| Error::io(bin_dir, error))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        #[cfg(windows)]
        let name = {
            let lower = file_name.to_ascii_lowercase();
            let Some(stripped) = lower
                .strip_suffix(".cmd")
                .or_else(|| lower.strip_suffix(".exe"))
                .or_else(|| lower.strip_suffix(".bat"))
            else {
                continue;
            };
            stripped.to_string()
        };
        #[cfg(not(windows))]
        let name = file_name.to_string();
        names.push(name);
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(crate) fn resolve_bin_target(bin_dir: &Path, name: &str) -> Result<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.exe")),
        bin_dir.join(format!("{name}.bat")),
    ];
    #[cfg(not(windows))]
    let candidates = [bin_dir.join(name)];

    for candidate in candidates {
        if candidate.exists() {
            let target =
                std::fs::canonicalize(&candidate).map_err(|error| Error::io(&candidate, error))?;
            if target.is_file() {
                return Ok(target);
            }
        }
    }
    Err(Error::other(crate::t!(
        "err.npm_bin_target_unresolved",
        name = name,
        path = bin_dir.display()
    )))
}

/// Path to the atomically replaced pointer for the active curated project npm
/// bin generation. Callers that need a rollback snapshot should read this
/// file before publishing a new generation.
pub fn project_bin_current_path(project_root: &Path) -> PathBuf {
    project_root
        .join(PROJECT_NPM_BIN_ROOT)
        .join(PROJECT_NPM_BIN_CURRENT)
}

/// Publish an immutable, osdk-owned bin generation for a project npm tool.
/// Existing selections are retained only while their exact configured specs
/// still match the trusted configuration supplied by the caller.
pub fn publish_project_bin_generation(
    project_root: &Path,
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    validate_project_npm_selection_for_project(selection, configured_specs, project_root)?;

    let canonical_project =
        dunce::canonicalize(project_root).map_err(|error| Error::io(project_root, error))?;
    let root = prepare_project_bin_root(project_root, &canonical_project)?;
    let _lock = crate::lock::FileLock::acquire(root.join("publish.lock"))?;
    let existing_manifest = read_current_project_bin_manifest(project_root)?;
    let mut selections = match existing_manifest {
        Some((_, manifest)) => manifest
            .selections
            .into_iter()
            .filter(|existing| {
                existing.backend != selection.backend
                    && configured_specs.get(&existing.backend) == Some(&existing.configured_spec)
            })
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    selections.push(selection.clone());
    selections.sort_by(|left, right| left.backend.cmp(&right.backend));
    selections.dedup_by(|left, right| left.backend == right.backend);
    let mut bins = Vec::new();
    let mut names = BTreeSet::new();
    for selected in &selections {
        validate_project_npm_selection_for_project(selected, configured_specs, project_root)?;
        let backend = NpmPackageBackend::from_id(&selected.backend).ok_or_else(|| {
            Error::other(format!(
                "invalid npm backend in project bin selection: {}",
                selected.backend
            ))
        })?;
        for bin in backend.validated_project_package_bins(project_root, &selected.version)? {
            let conflict_key = project_bin_conflict_key(&bin.name);
            if !names.insert(conflict_key) {
                return Err(Error::other(format!(
                    "project npm bin `{}` is declared by more than one configured package",
                    bin.name
                )));
            }
            bins.push(ProjectNpmBinManifestEntry {
                name: bin.name,
                backend: selected.backend.clone(),
                target: portable_relative_path(&bin.project_relative_target)?,
            });
        }
    }
    bins.sort_by(|left, right| left.name.cmp(&right.name));

    let generation = project_bin_generation_id(&selections, &bins)?;
    let manifest = ProjectNpmBinManifest {
        schema: PROJECT_NPM_BIN_SCHEMA,
        generation: generation.clone(),
        platform: project_bin_platform().into(),
        selections,
        bins,
    };

    let generations = root.join(PROJECT_NPM_BIN_GENERATIONS);
    ensure_project_bin_directory(&generations, &canonical_project, true)?;
    let generation_dir = generations.join(&generation);
    if generation_dir.exists() {
        validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
    } else {
        let staging = unique_project_bin_path(&generations, "stage");
        std::fs::create_dir(&staging).map_err(|error| Error::io(&staging, error))?;
        let build_result = (|| {
            let bin_dir = staging.join(PROJECT_NPM_BIN_BIN_DIR);
            std::fs::create_dir(&bin_dir).map_err(|error| Error::io(&bin_dir, error))?;
            for entry in &manifest.bins {
                let target = canonical_project.join(path_from_portable(&entry.target)?);
                write_curated_project_launcher(&bin_dir, &entry.name, &target)?;
            }
            write_project_bin_json(&staging.join(PROJECT_NPM_BIN_MANIFEST), &manifest)
        })();
        if let Err(error) = build_result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&staging, &generation_dir) {
            if !generation_dir.exists() {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(Error::io(&generation_dir, error));
            }
            let _ = std::fs::remove_dir_all(&staging);
            validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
        }
    }
    validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;

    let current = ProjectNpmBinCurrent {
        schema: PROJECT_NPM_BIN_SCHEMA,
        generation,
    };
    write_project_bin_json(&root.join(PROJECT_NPM_BIN_CURRENT), &current)?;
    Ok(generation_dir.join(PROJECT_NPM_BIN_BIN_DIR))
}

/// Resolve the active curated project bin directory after validating it
/// against trusted configuration and the current filesystem. No state is
/// changed; a missing current pointer means no curated generation is active.
pub fn validated_project_bin_dir(
    project_root: &Path,
    configured_specs: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    let Some((generation_dir, manifest)) = read_current_project_bin_manifest(project_root)? else {
        return Ok(None);
    };
    for selection in &manifest.selections {
        validate_project_npm_selection_for_project(selection, configured_specs, project_root)?;
    }
    if manifest.selections.iter().any(|selection| {
        configured_specs.get(&selection.backend) != Some(&selection.configured_spec)
    }) {
        return Err(Error::other(
            "project npm bin manifest does not match trusted project configuration",
        ));
    }
    let canonical_project =
        dunce::canonicalize(project_root).map_err(|error| Error::io(project_root, error))?;
    let root = ensure_project_bin_directory(
        &project_root.join(PROJECT_NPM_BIN_ROOT),
        &canonical_project,
        false,
    )?;
    let generations = ensure_project_bin_directory(
        &root.join(PROJECT_NPM_BIN_GENERATIONS),
        &canonical_project,
        false,
    )?;
    if !generation_dir.starts_with(&generations) {
        return Err(Error::other(
            "project npm bin generation resolves outside its owned directory",
        ));
    }
    validate_project_bin_generation(&canonical_project, &generation_dir, &manifest)?;
    Ok(Some(generation_dir.join(PROJECT_NPM_BIN_BIN_DIR)))
}

pub(crate) fn validate_project_npm_selection(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<()> {
    validate_project_npm_selection_identity(selection, configured_specs)?;
    if !matches!(
        npm_spec_satisfaction(&selection.configured_spec, &selection.version),
        Some(true)
    ) {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

pub(crate) fn validate_project_npm_selection_for_project(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
    project_root: &Path,
) -> Result<()> {
    validate_project_npm_selection_identity(selection, configured_specs)?;
    if validate_project_npm_selection(selection, configured_specs).is_ok() {
        return Ok(());
    }
    if !npm_channel_spec(&selection.configured_spec)
        || !project_lock_binds_selection(project_root, selection)?
    {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

pub(crate) fn validate_project_npm_selection_identity(
    selection: &ProjectNpmBinSelection,
    configured_specs: &BTreeMap<String, String>,
) -> Result<()> {
    if NpmPackageBackend::from_id(&selection.backend)
        .is_none_or(|backend| backend.id() != selection.backend)
        || selection.configured_spec.trim().is_empty()
        || selection.version.trim().is_empty()
        || configured_specs.get(&selection.backend) != Some(&selection.configured_spec)
    {
        return Err(Error::other(format!(
            "project npm bin selection does not match trusted configuration: {}",
            selection.backend
        )));
    }
    Ok(())
}

pub(crate) fn npm_channel_spec(spec: &str) -> bool {
    let spec = spec.trim();
    !spec.is_empty()
        && spec
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        && npm_spec_satisfaction(spec, "0.0.0").is_none()
}

pub(crate) fn project_lock_binds_selection(
    project_root: &Path,
    selection: &ProjectNpmBinSelection,
) -> Result<bool> {
    let path = project_root.join(PROJECT_NPM_LOCKFILE);
    let bytes = match crate::inventory::read_stable_regular_file(&path, PROJECT_NPM_LOCK_MAX_BYTES)
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io(&path, error)),
    };
    let lock: ProjectNpmLockfile = toml::from_str(std::str::from_utf8(&bytes).map_err(|_| {
        Error::other(format!("project npm lock is not UTF-8: {}", path.display()))
    })?)?;
    if lock.schema != 3 {
        return Ok(false);
    }
    let package = NpmPackageBackend::from_id(&selection.backend)
        .expect("selection backend was validated before lock lookup")
        .package;
    Ok(lock.platforms.values().any(|platform| {
        platform
            .tools
            .get(&selection.backend)
            .is_some_and(|locked| {
                locked.request == selection.configured_spec
                    && locked.version == selection.version
                    && locked.npm.as_ref().is_some_and(|npm| {
                        npm.package == package && npm.scope == ToolScope::Project.as_str()
                    })
            })
    }))
}

pub(crate) fn npm_spec_satisfaction(configured_spec: &str, exact_version: &str) -> Option<bool> {
    let configured_spec = configured_spec.trim();
    let exact_version = exact_version
        .trim()
        .strip_prefix('v')
        .unwrap_or(exact_version.trim());
    let Ok(version) = semver::Version::parse(exact_version) else {
        return Some(false);
    };
    if let Ok(exact) =
        semver::Version::parse(configured_spec.strip_prefix('v').unwrap_or(configured_spec))
    {
        return Some(exact == version);
    }
    let numeric_prefix = configured_spec
        .strip_prefix('v')
        .unwrap_or(configured_spec)
        .split('.')
        .collect::<Vec<_>>();
    if !numeric_prefix.is_empty()
        && numeric_prefix.len() < 3
        && numeric_prefix
            .iter()
            .all(|component| !component.is_empty() && component.chars().all(|c| c.is_ascii_digit()))
    {
        let exact_components = [version.major.to_string(), version.minor.to_string()];
        return Some(
            numeric_prefix
                .iter()
                .zip(exact_components.iter())
                .all(|(expected, actual)| *expected == actual),
        );
    }

    // Config entries use npm's version vocabulary. A successful semver
    // requirement parse covers caret/tilde/comparator ranges and numeric
    // prefixes such as `3` and `3.6`. Symbolic dist-tags (`latest`, `beta`,
    // custom channels) are deliberately rejected here: their meaning cannot
    // be reconstructed from an exact installed version alone.
    npm_semver_requirements(configured_spec).map(|requirements| {
        requirements
            .iter()
            .any(|requirement| requirement.matches(&version))
    })
}

pub(crate) fn npm_semver_requirements(spec: &str) -> Option<Vec<semver::VersionReq>> {
    spec.split("||")
        .map(|alternative| {
            let alternative = alternative.trim();
            if alternative.is_empty() || !npm_range_shape(alternative) {
                return None;
            }
            let normalized = alternative
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(", ");
            semver::VersionReq::parse(&normalized).ok()
        })
        .collect()
}

pub(crate) fn npm_range_shape(spec: &str) -> bool {
    spec.chars().all(|character| {
        character.is_ascii_digit()
            || character.is_ascii_whitespace()
            || matches!(
                character,
                '.' | '-' | '+' | '*' | 'x' | 'X' | '^' | '~' | '<' | '>' | '='
            )
    })
}

pub(crate) fn read_current_project_bin_manifest(
    project_root: &Path,
) -> Result<Option<(PathBuf, ProjectNpmBinManifest)>> {
    let current_path = project_bin_current_path(project_root);
    let current = match read_project_bin_json::<ProjectNpmBinCurrent>(&current_path) {
        Ok(current) => current,
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if current.schema != PROJECT_NPM_BIN_SCHEMA || !valid_generation_id(&current.generation) {
        return Err(Error::other(format!(
            "invalid project npm bin pointer at {}",
            current_path.display()
        )));
    }
    let generation_dir = project_root
        .join(PROJECT_NPM_BIN_ROOT)
        .join(PROJECT_NPM_BIN_GENERATIONS)
        .join(&current.generation);
    let generation_metadata = std::fs::symlink_metadata(&generation_dir)
        .map_err(|error| Error::io(&generation_dir, error))?;
    if !generation_metadata.is_dir() || generation_metadata.file_type().is_symlink() {
        return Err(Error::other(format!(
            "project npm bin generation is not an owned directory: {}",
            generation_dir.display()
        )));
    }
    let generation_dir =
        dunce::canonicalize(&generation_dir).map_err(|error| Error::io(&generation_dir, error))?;
    let manifest_path = generation_dir.join(PROJECT_NPM_BIN_MANIFEST);
    let manifest = read_project_bin_json::<ProjectNpmBinManifest>(&manifest_path)?;
    if manifest.schema != PROJECT_NPM_BIN_SCHEMA
        || manifest.generation != current.generation
        || manifest.platform != project_bin_platform()
    {
        return Err(Error::other(format!(
            "project npm bin manifest identity mismatch at {}",
            manifest_path.display()
        )));
    }
    Ok(Some((generation_dir, manifest)))
}

pub(crate) fn prepare_project_bin_root(
    project_root: &Path,
    canonical_project: &Path,
) -> Result<PathBuf> {
    let osdk = project_root.join(".osdk");
    ensure_project_bin_directory(&osdk, canonical_project, true)?;
    ensure_project_bin_directory(
        &project_root.join(PROJECT_NPM_BIN_ROOT),
        canonical_project,
        true,
    )
}

pub(crate) fn ensure_project_bin_directory(
    path: &Path,
    canonical_project: &Path,
    create: bool,
) -> Result<PathBuf> {
    if create {
        std::fs::create_dir(path)
            .or_else(|error| {
                (error.kind() == std::io::ErrorKind::AlreadyExists)
                    .then_some(())
                    .ok_or(error)
            })
            .map_err(|error| Error::io(path, error))?;
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::other(format!(
            "project npm bin path is not an owned directory: {}",
            path.display()
        )));
    }
    let canonical = dunce::canonicalize(path).map_err(|error| Error::io(path, error))?;
    if !canonical.starts_with(canonical_project) {
        return Err(Error::other(format!(
            "project npm bin directory escapes project: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

pub(crate) fn validate_project_bin_generation(
    canonical_project: &Path,
    generation_dir: &Path,
    expected_manifest: &ProjectNpmBinManifest,
) -> Result<()> {
    if expected_manifest.selections.is_empty()
        || expected_manifest.bins.is_empty()
        || !valid_generation_id(&expected_manifest.generation)
        || expected_manifest.schema != PROJECT_NPM_BIN_SCHEMA
        || expected_manifest.platform != project_bin_platform()
        || generation_dir.file_name().and_then(|name| name.to_str())
            != Some(expected_manifest.generation.as_str())
        || project_bin_generation_id(&expected_manifest.selections, &expected_manifest.bins)?
            != expected_manifest.generation
    {
        return Err(Error::other("invalid project npm bin generation identity"));
    }
    let actual_manifest = read_project_bin_json::<ProjectNpmBinManifest>(
        &generation_dir.join(PROJECT_NPM_BIN_MANIFEST),
    )?;
    if &actual_manifest != expected_manifest {
        return Err(Error::other(format!(
            "project npm bin generation manifest mismatch at {}",
            generation_dir.display()
        )));
    }

    let resolved_bins =
        declared_project_bin_entries(canonical_project, &expected_manifest.selections)?;
    let declared_manifest_entries = resolved_bins
        .iter()
        .map(|(entry, _)| entry.clone())
        .collect::<Vec<_>>();
    if declared_manifest_entries != expected_manifest.bins {
        return Err(Error::other(
            "project npm bin manifest no longer matches installed package declarations",
        ));
    }

    let mut expected_files = BTreeSet::new();
    expected_files.insert(PROJECT_NPM_BIN_MANIFEST.to_string());
    expected_files.insert(PROJECT_NPM_BIN_BIN_DIR.to_string());
    for (entry, target) in resolved_bins {
        let launcher_name = curated_launcher_name(&entry.name);
        expected_files.insert(format!("{PROJECT_NPM_BIN_BIN_DIR}/{launcher_name}"));
        validate_curated_project_launcher(
            &generation_dir
                .join(PROJECT_NPM_BIN_BIN_DIR)
                .join(launcher_name),
            &target,
        )?;
    }

    let actual_files = project_bin_file_set(generation_dir)?;
    if actual_files != expected_files {
        return Err(Error::other(format!(
            "project npm bin generation contains unexpected or missing files at {}",
            generation_dir.display()
        )));
    }
    Ok(())
}

pub(crate) fn project_bin_generation_id(
    selections: &[ProjectNpmBinSelection],
    bins: &[ProjectNpmBinManifestEntry],
) -> Result<String> {
    let identity = serde_json::to_vec(&(
        PROJECT_NPM_BIN_SCHEMA,
        project_bin_platform(),
        selections,
        bins,
    ))?;
    Ok(pipeline::verify::hash_bytes(
        &identity,
        pipeline::HashAlgo::Sha256,
    ))
}

pub(crate) fn declared_project_bin_entries(
    canonical_project: &Path,
    selections: &[ProjectNpmBinSelection],
) -> Result<Vec<(ProjectNpmBinManifestEntry, PathBuf)>> {
    let mut resolved = Vec::new();
    let mut seen_backends = BTreeSet::new();
    let mut seen_bins = BTreeSet::new();
    let mut previous_backend = None;
    for selection in selections {
        if selection.configured_spec.trim().is_empty()
            || selection.version.trim().is_empty()
            || !seen_backends.insert(selection.backend.clone())
            || previous_backend
                .as_ref()
                .is_some_and(|previous| previous >= &selection.backend)
        {
            return Err(Error::other("invalid project npm bin selection manifest"));
        }
        previous_backend = Some(selection.backend.clone());
        let backend = NpmPackageBackend::from_id(&selection.backend)
            .filter(|backend| backend.id() == selection.backend)
            .ok_or_else(|| Error::other("invalid npm backend in project bin manifest"))?;
        let package_dir = package_install_dir(canonical_project, backend.package());
        let canonical_package =
            dunce::canonicalize(&package_dir).map_err(|error| Error::io(&package_dir, error))?;
        if !canonical_package.starts_with(canonical_project) {
            return Err(Error::other(format!(
                "installed npm package {} resolves outside project",
                backend.package()
            )));
        }
        let package_json = package_dir.join("package.json");
        let bytes = crate::inventory::read_stable_regular_file(
            &package_json,
            NPM_PACKAGE_MANIFEST_MAX_BYTES,
        )
        .map_err(|error| Error::io(&package_json, error))?;
        let package_manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
        if package_manifest
            .get("name")
            .and_then(serde_json::Value::as_str)
            != Some(backend.package())
            || package_manifest
                .get("version")
                .and_then(serde_json::Value::as_str)
                != Some(selection.version.as_str())
        {
            return Err(Error::other(format!(
                "installed project package identity mismatch: expected {}@{}",
                backend.package(),
                selection.version
            )));
        }
        for (name, relative) in package_bin_entries(&package_manifest, backend.package())? {
            if relative.is_absolute()
                || relative.as_os_str().is_empty()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(Error::other(format!(
                    "npm package {} declares unsafe bin path {}",
                    backend.package(),
                    relative.display()
                )));
            }
            if !seen_bins.insert(project_bin_conflict_key(&name)) {
                return Err(Error::other(format!(
                    "project npm bin `{name}` is declared by more than one configured package"
                )));
            }
            let declared = package_dir.join(relative);
            let target =
                dunce::canonicalize(&declared).map_err(|error| Error::io(&declared, error))?;
            if !target.is_file()
                || !target.starts_with(&canonical_package)
                || !target.starts_with(canonical_project)
            {
                return Err(Error::other(format!(
                    "project npm bin target escapes its package: {}",
                    declared.display()
                )));
            }
            let entry = ProjectNpmBinManifestEntry {
                name,
                backend: selection.backend.clone(),
                target: portable_relative_path(
                    target
                        .strip_prefix(canonical_project)
                        .map_err(|_| Error::other("project npm bin target escapes project"))?,
                )?,
            };
            resolved.push((entry, target));
        }
    }
    resolved.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    Ok(resolved)
}

pub(crate) fn project_bin_file_set(root: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|error| {
            Error::other(format!(
                "reading project npm bin generation {}: {error}",
                root.display()
            ))
        })?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::other("project npm bin entry escaped its generation"))?;
        let portable = portable_relative_path(relative)?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| Error::io(entry.path(), error))?;
        if metadata.is_dir() {
            if portable != PROJECT_NPM_BIN_BIN_DIR {
                return Err(Error::other(format!(
                    "unexpected directory in project npm bin generation: {portable}"
                )));
            }
        } else if !metadata.is_file() && !metadata.file_type().is_symlink() {
            return Err(Error::other(format!(
                "unsupported file type in project npm bin generation: {portable}"
            )));
        }
        files.insert(portable);
    }
    Ok(files)
}

pub(crate) fn valid_generation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn project_bin_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else {
        "unix"
    }
}

pub(crate) fn project_bin_conflict_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

pub(crate) fn portable_relative_path(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(Error::other(format!(
            "unsafe project npm bin relative path: {}",
            path.display()
        )));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn path_from_portable(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\\') {
        return Err(Error::other("invalid project npm bin target path"));
    }
    let path = PathBuf::from(value);
    portable_relative_path(&path)?;
    Ok(path)
}

pub(crate) fn curated_launcher_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    }
}

#[cfg(not(windows))]
pub(crate) fn write_curated_project_launcher(
    bin_dir: &Path,
    name: &str,
    target: &Path,
) -> Result<()> {
    use std::os::unix::fs::symlink;

    let launcher = bin_dir.join(name);
    let relative = relative_path_from(bin_dir, target).ok_or_else(|| {
        Error::other(format!(
            "cannot construct relative project npm bin target for {}",
            target.display()
        ))
    })?;
    symlink(&relative, &launcher).map_err(|error| Error::io(&launcher, error))
}

#[cfg(windows)]
pub(crate) fn write_curated_project_launcher(
    bin_dir: &Path,
    name: &str,
    target: &Path,
) -> Result<()> {
    let launcher = bin_dir.join(format!("{name}.cmd"));
    let relative = relative_path_from(bin_dir, target).ok_or_else(|| {
        Error::other(format!(
            "cannot construct relative project npm bin target for {}",
            target.display()
        ))
    })?;
    let target = relative.to_string_lossy().replace('/', "\\");
    let contents = render_osdk_project_cmd_wrapper(&target)?;
    std::fs::write(&launcher, contents).map_err(|error| Error::io(&launcher, error))
}

pub(crate) fn validate_curated_project_launcher(launcher: &Path, target: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        let metadata =
            std::fs::symlink_metadata(launcher).map_err(|error| Error::io(launcher, error))?;
        if !metadata.file_type().is_symlink()
            || dunce::canonicalize(launcher).map_err(|error| Error::io(launcher, error))? != target
        {
            return Err(Error::other(format!(
                "invalid curated project npm launcher at {}",
                launcher.display()
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let actual = parse_windows_project_wrapper(launcher)?;
        let actual = dunce::canonicalize(&actual).map_err(|error| Error::io(&actual, error))?;
        if actual != target {
            return Err(Error::other(format!(
                "invalid curated project npm launcher at {}",
                launcher.display()
            )));
        }
        Ok(())
    }
}

pub(crate) fn relative_path_from(base: &Path, target: &Path) -> Option<PathBuf> {
    let base = base.components().collect::<Vec<_>>();
    let target = target.components().collect::<Vec<_>>();
    let shared = base
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

pub(crate) fn read_project_bin_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = crate::inventory::read_stable_regular_file(path, PROJECT_NPM_BIN_MAX_JSON_BYTES)
        .map_err(|error| Error::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

pub(crate) fn write_project_bin_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::other(format!(
            "project npm bin path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    let temporary = unique_project_bin_path(parent, "metadata");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        file.write_all(&bytes)
            .map_err(|error| Error::io(&temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io(&temporary, error))?;
    }
    if let Err(error) = atomic_replace_project_bin(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn unique_project_bin_path(parent: &Path, label: &str) -> PathBuf {
    loop {
        let nonce = NEXT_PROJECT_NPM_BIN_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{label}-{}-{nonce}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace_project_bin(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|error| Error::io(destination, error))
}

#[cfg(windows)]
pub(crate) fn atomic_replace_project_bin(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(Error::io(destination, std::io::Error::last_os_error()));
    }
    Ok(())
}
