//! Installer selection for npm-backed developer tools.
//!
//! Planning is deliberately side-effect free. The selected installer is fixed
//! before a project is mutated, so callers must never retry a failed install
//! with a different installer.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{NpmDefaultInstaller, ToolConfigEntry, ToolConfigValue};
use crate::error::{Error, Result};
use crate::version::resolver::{package_manager_from_package_json, PackageManagerRequest};

/// Public structured-tool/request option used to select an installer.
pub const INSTALLER_OPTION: &str = "installer";

/// Private resolved-plan options persisted through lock/install requests.
pub const LOCKED_NPM_INSTALLER_OPTION: &str = "__osdk_npm_installer";
pub const LOCKED_NPM_SCOPE_OPTION: &str = "__osdk_npm_scope";
pub const LOCKED_NPM_NATIVE_LOCK_KIND_OPTION: &str = "__osdk_npm_native_lock_kind";
pub const LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION: &str = "__osdk_npm_native_lock_format";
pub const LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION: &str = "__osdk_npm_native_lock_sha256";

/// Installer requested for an npm-backed tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NpmInstaller {
    /// Select an installer from project declarations and lockfile compatibility.
    #[default]
    Auto,
    /// Delegate once to the managed npm executable.
    Npm,
    /// Delegate once to the managed pnpm executable.
    Pnpm,
}

impl From<NpmDefaultInstaller> for NpmInstaller {
    /// Widen the configurable default into the requestable set. This is
    /// intentionally one-way: every configurable default is a concrete
    /// installer, so the result is never `Auto`.
    fn from(value: NpmDefaultInstaller) -> Self {
        match value {
            NpmDefaultInstaller::Npm => Self::Npm,
            NpmDefaultInstaller::Pnpm => Self::Pnpm,
        }
    }
}

impl NpmInstaller {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
        }
    }

    /// Executable name for a concrete installer. `auto` has no executable.
    pub const fn executable(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Npm => Some("npm"),
            Self::Pnpm => Some("pnpm"),
        }
    }

    /// Canonical lockfile created by a concrete installer.
    pub const fn lockfile_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Npm => Some("package-lock.json"),
            Self::Pnpm => Some("pnpm-lock.yaml"),
        }
    }

    /// Canonical format created by the current concrete installer. Existing
    /// compatible locks can have a different supported format (for example,
    /// npm's package-lock v2); use [`NativeLock::format`] when inspecting one.
    pub const fn lock_format(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Npm => Some("package-lock-v3"),
            Self::Pnpm => Some("pnpm-v9"),
        }
    }

    const fn is_native(self) -> bool {
        matches!(self, Self::Npm | Self::Pnpm)
    }
}

impl fmt::Display for NpmInstaller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NpmInstaller {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "npm" => Ok(Self::Npm),
            "pnpm" => Ok(Self::Pnpm),
            other => Err(Error::config(crate::t!(
                "err.npm_installer_invalid",
                installer = other
            ))),
        }
    }
}

/// Whether a tool declaration belongs to the current project or the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolScope {
    #[default]
    Project,
    Global,
}

impl ToolScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

impl fmt::Display for ToolScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ToolScope {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "project" => Ok(Self::Project),
            "global" => Ok(Self::Global),
            other => Err(Error::config(crate::t!(
                "err.npm_scope_invalid",
                scope = other
            ))),
        }
    }
}

/// Which on-disk lockfile was found. Package-lock and shrinkwrap stay distinct
/// so diagnostics and integrity metadata identify the file that was consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeLockKind {
    Pnpm,
    PackageLock,
    NpmShrinkwrap,
}

impl NativeLockKind {
    pub const fn installer(self) -> NpmInstaller {
        match self {
            Self::Pnpm => NpmInstaller::Pnpm,
            Self::PackageLock | Self::NpmShrinkwrap => NpmInstaller::Npm,
        }
    }

    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm-lock.yaml",
            Self::PackageLock => "package-lock.json",
            Self::NpmShrinkwrap => "npm-shrinkwrap.json",
        }
    }

    const fn format_prefix(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm-v",
            Self::PackageLock => "package-lock-v",
            Self::NpmShrinkwrap => "npm-shrinkwrap-v",
        }
    }
}

impl fmt::Display for NativeLockKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.file_name())
    }
}

/// A recognized lockfile next to the nearest `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLock {
    pub kind: NativeLockKind,
    pub path: PathBuf,
    /// Parsed major lockfile version.
    pub version: u64,
    /// Stable format identity such as `pnpm-v9` or `package-lock-v2`.
    pub format: String,
    /// Whether osdk can safely read and round-trip this format.
    pub supported: bool,
}

impl NativeLock {
    /// Concrete installer that owns this native lockfile.
    pub const fn installer(&self) -> NpmInstaller {
        self.kind.installer()
    }

    /// Serialized owner label used by lock metadata.
    pub const fn installer_name(&self) -> &'static str {
        self.installer().as_str()
    }
}

/// The nearest Node project and the package-manager signals found at its hard
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmProject {
    pub root: PathBuf,
    pub package_json: PathBuf,
    pub declared_manager: Option<PackageManagerRequest>,
    pub native_lock: Option<NativeLock>,
}

/// Fully preflighted installer choice. `installer` is always concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmInstallPlan {
    pub scope: ToolScope,
    pub requested: NpmInstaller,
    pub installer: NpmInstaller,
    pub project: Option<NpmProject>,
}

/// Read `installer` from one structured `[tools.<name>]` entry. Legacy entries
/// and structured entries without the option mean `auto`. Non-string values
/// are rejected rather than being silently stringified.
pub fn installer_from_tool_config(entry: Option<&ToolConfigEntry>) -> Result<NpmInstaller> {
    let Some(value) = entry
        .and_then(ToolConfigEntry::options)
        .and_then(|options| options.get(INSTALLER_OPTION))
    else {
        return Ok(NpmInstaller::Auto);
    };
    match value {
        ToolConfigValue::String(value) => value.parse(),
        _ => Err(Error::config(crate::t!("err.npm_installer_option_type"))),
    }
}

/// Read `installer` after CLI and config options have been merged into a tool
/// request. Missing means `auto`; unknown or empty values are errors.
pub fn installer_from_request_options(options: &BTreeMap<String, String>) -> Result<NpmInstaller> {
    options
        .get(INSTALLER_OPTION)
        .map(|value| value.parse())
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Find the closest ancestor `package.json`. A manifest is a hard boundary: an
/// unsafe symlink or non-regular entry fails instead of falling through to an
/// outer project.
pub fn find_nearest_package_json(start_dir: &Path) -> Result<Option<PathBuf>> {
    for directory in start_dir.ancestors() {
        let path = directory.join("package.json");
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => return Ok(Some(path)),
            Ok(_) => {
                return Err(Error::config(crate::t!(
                    "err.npm_project_file_not_regular",
                    path = path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path, error)),
        }
    }
    Ok(None)
}

/// Inspect the nearest package boundary without choosing an installer.
pub fn inspect_npm_project(start_dir: &Path) -> Result<Option<NpmProject>> {
    let Some(package_json) = find_nearest_package_json(start_dir)? else {
        return Ok(None);
    };
    let declared_manager =
        package_manager_from_package_json(&package_json).map_err(Error::config)?;
    let root = package_json
        .parent()
        .expect("a package.json path always has its containing directory")
        .to_path_buf();
    let native_lock = inspect_native_lock(&root)?;
    Ok(Some(NpmProject {
        root,
        package_json,
        declared_manager,
        native_lock,
    }))
}

/// Select an installer without running it. Global tools deliberately ignore
/// the current project.
///
/// Project auto-selection consults, in order, the project's declared
/// `packageManager`, the installer that owns an incumbent lockfile, and finally
/// `default_installer`. Because the configured default is only the last resort,
/// changing it never takes a project away from the installer it already
/// declares or already has a lockfile for.
pub fn plan_npm_installer(
    start_dir: &Path,
    requested: NpmInstaller,
    scope: ToolScope,
    default_installer: NpmDefaultInstaller,
) -> Result<NpmInstallPlan> {
    let fallback = NpmInstaller::from(default_installer);
    if scope == ToolScope::Global {
        return Ok(NpmInstallPlan {
            scope,
            requested,
            installer: concrete_or_default(requested, fallback),
            project: None,
        });
    }

    let project = inspect_npm_project(start_dir)?;
    let installer = match requested {
        NpmInstaller::Auto => select_automatic_installer(project.as_ref(), fallback)?,
        concrete => {
            validate_explicit_installer(concrete, project.as_ref())?;
            concrete
        }
    };
    debug_assert_ne!(installer, NpmInstaller::Auto);
    Ok(NpmInstallPlan {
        scope,
        requested,
        installer,
        project,
    })
}

fn concrete_or_default(requested: NpmInstaller, fallback: NpmInstaller) -> NpmInstaller {
    match requested {
        NpmInstaller::Auto => fallback,
        concrete => concrete,
    }
}

fn select_automatic_installer(
    project: Option<&NpmProject>,
    fallback: NpmInstaller,
) -> Result<NpmInstaller> {
    let Some(project) = project else {
        return Ok(fallback);
    };

    // A declared package manager is authoritative: it is the project's own
    // statement of which installer owns the tree.
    if let Some(declared) = &project.declared_manager {
        let declared_installer = declared_installer(declared)?;
        if let Some(native_lock) = &project.native_lock {
            if native_lock.installer() != declared_installer {
                return Err(Error::config(crate::t!(
                    "err.npm_manager_lock_owner_conflict",
                    manifest = project.package_json.display(),
                    manager = declared.manager,
                    lock = native_lock.path.display(),
                    owner = native_lock.installer()
                )));
            }
        }
        return Ok(declared_installer);
    }

    // Otherwise an incumbent lockfile decides, so the installer that wrote it
    // keeps owning it. With no signal at all, the configured default applies.
    match &project.native_lock {
        Some(native_lock) => Ok(native_lock.installer()),
        None => Ok(fallback),
    }
}

fn declared_installer(declared: &PackageManagerRequest) -> Result<NpmInstaller> {
    match declared.manager.as_str() {
        "npm" => Ok(NpmInstaller::Npm),
        "pnpm" => Ok(NpmInstaller::Pnpm),
        other => Err(Error::config(crate::t!(
            "err.npm_declared_installer_unsupported",
            source = declared.source.display(),
            installer = other
        ))),
    }
}

fn validate_explicit_installer(
    requested: NpmInstaller,
    project: Option<&NpmProject>,
) -> Result<()> {
    debug_assert_ne!(requested, NpmInstaller::Auto);
    let Some(native_lock) = project.and_then(|project| project.native_lock.as_ref()) else {
        return Ok(());
    };

    if requested.is_native() && native_lock.installer() != requested {
        return Err(Error::config(crate::t!(
            "err.npm_installer_lock_conflict",
            installer = requested,
            path = native_lock.path.display(),
            owner = native_lock.installer()
        )));
    }
    Ok(())
}

fn inspect_native_lock(project_root: &Path) -> Result<Option<NativeLock>> {
    let mut present = Vec::new();
    for kind in [
        NativeLockKind::Pnpm,
        NativeLockKind::PackageLock,
        NativeLockKind::NpmShrinkwrap,
    ] {
        let path = project_root.join(kind.file_name());
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => present.push((kind, path)),
            Ok(_) => {
                return Err(Error::config(crate::t!(
                    "err.npm_project_file_not_regular",
                    path = path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path, error)),
        }
    }

    if present.len() > 1 {
        return Err(Error::config(crate::t!(
            "err.npm_project_lock_ambiguous",
            paths = present
                .iter()
                .map(|(_, path)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let Some((kind, path)) = present.pop() else {
        return Ok(None);
    };
    parse_native_lock(kind, path).map(Some)
}

fn parse_native_lock(kind: NativeLockKind, path: PathBuf) -> Result<NativeLock> {
    let text = std::fs::read_to_string(&path).map_err(|error| Error::io(&path, error))?;
    let version = match kind {
        NativeLockKind::PackageLock | NativeLockKind::NpmShrinkwrap => {
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                Error::config(crate::t!(
                    "err.npm_native_lock_parse",
                    path = path.display(),
                    error = error
                ))
            })?;
            value
                .get("lockfileVersion")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    Error::config(crate::t!(
                        "err.npm_native_lock_version_numeric_required",
                        path = path.display()
                    ))
                })?
        }
        NativeLockKind::Pnpm => {
            let value: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|error| {
                Error::config(crate::t!(
                    "err.npm_native_lock_parse",
                    path = path.display(),
                    error = error
                ))
            })?;
            let value = value.get("lockfileVersion").ok_or_else(|| {
                Error::config(crate::t!(
                    "err.npm_native_lock_version_missing",
                    path = path.display()
                ))
            })?;
            yaml_lock_major(value).ok_or_else(|| {
                Error::config(crate::t!(
                    "err.npm_native_lock_version_malformed",
                    path = path.display()
                ))
            })?
        }
    };

    let supported = match kind {
        NativeLockKind::Pnpm => version == 9,
        NativeLockKind::PackageLock | NativeLockKind::NpmShrinkwrap => {
            matches!(version, 2 | 3)
        }
    };

    Ok(NativeLock {
        kind,
        path,
        version,
        format: format!("{}{version}", kind.format_prefix()),
        supported,
    })
}

fn yaml_lock_major(value: &serde_yaml::Value) -> Option<u64> {
    let raw = match value {
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => return None,
    };
    let mut components = raw.trim().split('.');
    let major = components.next()?.parse().ok()?;
    if components.any(|component| component.parse::<u64>().is_err()) {
        return None;
    }
    Some(major)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_package(root: &Path, json: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("package.json"), json).unwrap();
    }

    #[test]
    fn installer_parse_display_and_serde_round_trip() {
        for (text, expected) in [
            ("auto", NpmInstaller::Auto),
            ("npm", NpmInstaller::Npm),
            ("pnpm", NpmInstaller::Pnpm),
        ] {
            assert_eq!(text.parse::<NpmInstaller>().unwrap(), expected);
            assert_eq!(expected.to_string(), text);
            assert_eq!(
                serde_json::to_string(&expected).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(
                serde_json::from_str::<NpmInstaller>(&format!("\"{text}\"")).unwrap(),
                expected
            );
        }
        assert!("yarn".parse::<NpmInstaller>().is_err());
        assert!("".parse::<NpmInstaller>().is_err());
    }

    #[test]
    fn parses_installer_from_structured_and_merged_options() {
        let entry = ToolConfigEntry::structured(
            "3",
            BTreeMap::from([(
                INSTALLER_OPTION.into(),
                ToolConfigValue::String("pnpm".into()),
            )]),
        );
        assert_eq!(
            installer_from_tool_config(Some(&entry)).unwrap(),
            NpmInstaller::Pnpm
        );
        assert_eq!(
            installer_from_tool_config(Some(&ToolConfigEntry::legacy("3"))).unwrap(),
            NpmInstaller::Auto
        );
        assert_eq!(
            installer_from_request_options(&BTreeMap::from([(
                INSTALLER_OPTION.into(),
                "npm".into()
            )]))
            .unwrap(),
            NpmInstaller::Npm
        );
        assert!(installer_from_request_options(&BTreeMap::from([(
            INSTALLER_OPTION.into(),
            "yarn".into()
        )]))
        .is_err());

        let invalid = ToolConfigEntry::structured(
            "3",
            BTreeMap::from([(INSTALLER_OPTION.into(), ToolConfigValue::Bool(true))]),
        );
        assert!(installer_from_tool_config(Some(&invalid)).is_err());
    }

    #[test]
    fn nearest_manifest_is_a_hard_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let outer = temporary.path().join("outer");
        let inner = outer.join("packages/app");
        let nested = inner.join("src");
        write_package(&outer, r#"{"packageManager":"npm@11.0.0"}"#);
        write_package(&inner, "{}");
        std::fs::create_dir_all(&nested).unwrap();

        let project = inspect_npm_project(&nested).unwrap().unwrap();
        assert_eq!(project.root, inner);
        assert!(project.declared_manager.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_nearest_manifest_fails_closed() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(temporary.path().join("manifest.json"), "{}").unwrap();
        symlink(
            temporary.path().join("manifest.json"),
            project.join("package.json"),
        )
        .unwrap();

        assert!(find_nearest_package_json(&project).is_err());
    }

    #[test]
    fn package_manager_wins_over_dev_engines_and_auto_honors_the_declaration() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(
            temporary.path(),
            r#"{"packageManager":"pnpm@9.15.0","devEngines":{"packageManager":{"name":"npm","version":"11.0.0"}}}"#,
        );
        let plan = plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Project,
            NpmDefaultInstaller::Npm,
        )
        .unwrap();
        assert_eq!(plan.installer, NpmInstaller::Pnpm);
        assert_eq!(
            plan.project.unwrap().declared_manager.unwrap().manager,
            "pnpm"
        );
    }

    #[test]
    fn dev_engines_array_uses_first_manager() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(
            temporary.path(),
            r#"{"devEngines":{"packageManager":[{"name":"npm","version":"11.0.0"},{"name":"pnpm","version":"9.0.0"}]}}"#,
        );
        let project = inspect_npm_project(temporary.path()).unwrap().unwrap();
        assert_eq!(project.declared_manager.unwrap().manager, "npm");
    }

    #[test]
    fn detects_each_supported_native_lock_format() {
        let cases = [
            (NativeLockKind::Pnpm, "lockfileVersion: 9.0\n", "pnpm-v9"),
            (
                NativeLockKind::PackageLock,
                r#"{"lockfileVersion":2}"#,
                "package-lock-v2",
            ),
            (
                NativeLockKind::NpmShrinkwrap,
                r#"{"lockfileVersion":3}"#,
                "npm-shrinkwrap-v3",
            ),
        ];
        for (kind, contents, format) in cases {
            let temporary = tempfile::tempdir().unwrap();
            write_package(temporary.path(), "{}");
            std::fs::write(temporary.path().join(kind.file_name()), contents).unwrap();
            let lock = inspect_npm_project(temporary.path())
                .unwrap()
                .unwrap()
                .native_lock
                .unwrap();
            assert_eq!(lock.kind, kind);
            assert_eq!(lock.format, format);
            assert!(lock.supported);
        }
    }

    #[test]
    fn multiple_recognized_locks_are_ambiguous_even_for_one_owner() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), "{}");
        std::fs::write(
            temporary.path().join("package-lock.json"),
            r#"{"lockfileVersion":3}"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("npm-shrinkwrap.json"),
            r#"{"lockfileVersion":3}"#,
        )
        .unwrap();

        let error = inspect_npm_project(temporary.path()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("package-lock.json"));
        assert!(message.contains("npm-shrinkwrap.json"));
    }

    #[test]
    fn auto_lets_the_incumbent_lock_owner_keep_the_project() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), "{}");
        let lock = temporary.path().join("pnpm-lock.yaml");
        for version in ["9.0", "8.0"] {
            std::fs::write(&lock, format!("lockfileVersion: '{version}'\n")).unwrap();
            assert_eq!(
                plan_npm_installer(
                    temporary.path(),
                    NpmInstaller::Auto,
                    ToolScope::Project,
                    NpmDefaultInstaller::Npm
                )
                .unwrap()
                .installer,
                NpmInstaller::Pnpm
            );
        }
    }

    #[test]
    fn auto_defaults_to_npm_without_a_declaration_or_lock() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), "{}");
        assert_eq!(
            plan_npm_installer(
                temporary.path(),
                NpmInstaller::Auto,
                ToolScope::Project,
                NpmDefaultInstaller::Npm
            )
            .unwrap()
            .installer,
            NpmInstaller::Npm
        );
    }

    #[test]
    fn auto_defaults_to_npm_outside_a_project() {
        let temporary = tempfile::tempdir().unwrap();
        let plan = plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Project,
            NpmDefaultInstaller::Npm,
        )
        .unwrap();
        assert_eq!(plan.installer, NpmInstaller::Npm);
        assert!(plan.project.is_none());
    }

    #[test]
    fn auto_rejects_declared_manager_and_lock_owner_conflict() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), r#"{"packageManager":"npm@11.0.0"}"#);
        std::fs::write(
            temporary.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
        assert!(plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Project,
            NpmDefaultInstaller::Npm
        )
        .is_err());
    }

    #[test]
    fn declared_manager_with_unsupported_owned_lock_uses_native_owner() {
        for (manager, file, contents, expected) in [
            (
                "npm",
                "package-lock.json",
                r#"{"lockfileVersion":4}"#,
                NpmInstaller::Npm,
            ),
            (
                "pnpm",
                "pnpm-lock.yaml",
                "lockfileVersion: '8.0'\n",
                NpmInstaller::Pnpm,
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            write_package(
                temporary.path(),
                &format!(r#"{{"packageManager":"{manager}@9.0.0"}}"#),
            );
            std::fs::write(temporary.path().join(file), contents).unwrap();
            assert_eq!(
                plan_npm_installer(
                    temporary.path(),
                    NpmInstaller::Auto,
                    ToolScope::Project,
                    NpmDefaultInstaller::Npm
                )
                .unwrap()
                .installer,
                expected
            );
        }
    }

    #[test]
    fn explicit_choice_overrides_declaration_but_respects_lock_ownership() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), r#"{"packageManager":"pnpm@9.0.0"}"#);
        std::fs::write(
            temporary.path().join("package-lock.json"),
            r#"{"lockfileVersion":3}"#,
        )
        .unwrap();

        assert_eq!(
            plan_npm_installer(
                temporary.path(),
                NpmInstaller::Npm,
                ToolScope::Project,
                NpmDefaultInstaller::Npm
            )
            .unwrap()
            .installer,
            NpmInstaller::Npm
        );
        assert!(plan_npm_installer(
            temporary.path(),
            NpmInstaller::Pnpm,
            ToolScope::Project,
            NpmDefaultInstaller::Npm
        )
        .is_err());
    }

    #[test]
    fn unsupported_npm_lock_is_preclassified_for_native_execution() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), "{}");
        std::fs::write(
            temporary.path().join("package-lock.json"),
            r#"{"lockfileVersion":4}"#,
        )
        .unwrap();

        let plan = plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Project,
            NpmDefaultInstaller::Npm,
        )
        .unwrap();
        assert_eq!(plan.installer, NpmInstaller::Npm);
        let lock = plan.project.unwrap().native_lock.unwrap();
        assert!(!lock.supported);
        assert_eq!(lock.installer_name(), "npm");
        assert_eq!(lock.format, "package-lock-v4");
    }

    #[test]
    fn unsupported_declared_managers_fail_in_auto_but_explicit_choice_wins() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), r#"{"packageManager":"yarn@4.10.3"}"#);

        assert!(plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Project,
            NpmDefaultInstaller::Npm
        )
        .is_err());
        assert_eq!(
            plan_npm_installer(
                temporary.path(),
                NpmInstaller::Npm,
                ToolScope::Project,
                NpmDefaultInstaller::Npm
            )
            .unwrap()
            .installer,
            NpmInstaller::Npm
        );
    }

    #[test]
    fn configured_default_installer_only_applies_as_the_last_resort() {
        // No declaration and no lockfile: the configured default decides, both
        // for a project and for a global install.
        let bare = tempfile::tempdir().unwrap();
        write_package(bare.path(), "{}");
        for scope in [ToolScope::Project, ToolScope::Global] {
            assert_eq!(
                plan_npm_installer(
                    bare.path(),
                    NpmInstaller::Auto,
                    scope,
                    NpmDefaultInstaller::Pnpm
                )
                .unwrap()
                .installer,
                NpmInstaller::Pnpm,
                "configured default ignored for {scope:?}"
            );
        }

        // A declared packageManager outranks the configured default.
        let declared = tempfile::tempdir().unwrap();
        write_package(declared.path(), r#"{"packageManager":"npm@11.0.0"}"#);
        assert_eq!(
            plan_npm_installer(
                declared.path(),
                NpmInstaller::Auto,
                ToolScope::Project,
                NpmDefaultInstaller::Pnpm
            )
            .unwrap()
            .installer,
            NpmInstaller::Npm
        );

        // An incumbent lockfile also outranks the configured default, so
        // flipping the setting cannot orphan an existing tree.
        let incumbent = tempfile::tempdir().unwrap();
        write_package(incumbent.path(), "{}");
        std::fs::write(
            incumbent.path().join("package-lock.json"),
            r#"{"lockfileVersion":3}"#,
        )
        .unwrap();
        assert_eq!(
            plan_npm_installer(
                incumbent.path(),
                NpmInstaller::Auto,
                ToolScope::Project,
                NpmDefaultInstaller::Pnpm
            )
            .unwrap()
            .installer,
            NpmInstaller::Npm
        );
    }

    #[test]
    fn configured_default_installer_parses_only_concrete_installers() {
        assert_eq!(
            "npm".parse::<NpmDefaultInstaller>().unwrap(),
            NpmDefaultInstaller::Npm
        );
        assert_eq!(
            " PNPM ".parse::<NpmDefaultInstaller>().unwrap(),
            NpmDefaultInstaller::Pnpm
        );
        // `auto` would make the fallback circular, and yarn is unsupported.
        assert!("auto".parse::<NpmDefaultInstaller>().is_err());
        assert!("yarn".parse::<NpmDefaultInstaller>().is_err());
    }

    #[test]
    fn missing_or_malformed_lock_versions_fail_closed() {
        let cases = [
            ("package-lock.json", "{}"),
            ("package-lock.json", r#"{"lockfileVersion":"3"}"#),
            ("pnpm-lock.yaml", "packages: {}\n"),
            ("pnpm-lock.yaml", "lockfileVersion: nope\n"),
        ];
        for (name, contents) in cases {
            let temporary = tempfile::tempdir().unwrap();
            write_package(temporary.path(), "{}");
            std::fs::write(temporary.path().join(name), contents).unwrap();
            assert!(inspect_npm_project(temporary.path()).is_err(), "{name}");
        }
    }

    #[test]
    fn global_plan_does_not_inspect_current_project() {
        let temporary = tempfile::tempdir().unwrap();
        write_package(temporary.path(), "not json");
        std::fs::write(temporary.path().join("pnpm-lock.yaml"), "not yaml: [").unwrap();

        let plan = plan_npm_installer(
            temporary.path(),
            NpmInstaller::Auto,
            ToolScope::Global,
            NpmDefaultInstaller::Npm,
        )
        .unwrap();
        assert_eq!(plan.installer, NpmInstaller::Npm);
        assert!(plan.project.is_none());
    }

    #[test]
    fn npm_scope_errors_render_in_chinese() {
        let invalid_installer = crate::i18n::interpolate(
            &crate::i18n::trl(crate::i18n::Lang::Zh, "err.npm_installer_invalid"),
            &[("installer", "yarn")],
        );
        assert_eq!(
            invalid_installer,
            "无效的 npm 安装器 `yarn`（应为 auto|npm|pnpm）"
        );

        let conflict = crate::i18n::interpolate(
            &crate::i18n::trl(crate::i18n::Lang::Zh, "err.npm_manager_lock_owner_conflict"),
            &[
                ("manifest", "/repo/package.json"),
                ("manager", "npm"),
                ("lock", "/repo/pnpm-lock.yaml"),
                ("owner", "pnpm"),
            ],
        );
        assert_eq!(
            conflict,
            "/repo/package.json 声明的包管理器为 `npm`，但 /repo/pnpm-lock.yaml 属于 `pnpm`"
        );
    }
}
