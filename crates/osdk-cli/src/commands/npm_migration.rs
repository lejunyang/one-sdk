//! Version-local npm packages are legacy state, separate from Node ownership.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::runtimes::{managed_node_tools, npm_command, ManagedNodeTools};
use super::*;

#[derive(Debug)]
struct GlobalPackage {
    name: String,
    version: String,
    root: PathBuf,
    bins: BTreeMap<String, String>,
    native: bool,
}

impl GlobalPackage {
    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

fn modules(prefix: &Path) -> PathBuf {
    prefix.join(if cfg!(windows) {
        "node_modules"
    } else {
        "lib/node_modules"
    })
}

fn packages(prefix: &Path) -> Result<Vec<GlobalPackage>> {
    let modules = modules(prefix);
    let entries = match std::fs::read_dir(&modules) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || matches!(name.as_str(), "npm" | "corepack") {
            continue;
        }
        if name.starts_with('@') && entry.path().is_dir() {
            for scoped in std::fs::read_dir(entry.path())? {
                let scoped = scoped?;
                roots.push((
                    format!("{name}/{}", scoped.file_name().to_string_lossy()),
                    scoped.path(),
                ));
            }
        } else {
            roots.push((name, entry.path()));
        }
    }
    let mut out = Vec::new();
    for (expected_name, root) in roots {
        let manifest = root.join("package.json");
        if !manifest.is_file() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let name = value["name"].as_str().unwrap_or("");
        let version = value["version"].as_str().unwrap_or("");
        if name != expected_name
            || !matches!(VersionSpec::parse(version), VersionSpec::Exact(ref exact) if exact == version)
        {
            return Err(anyhow!(
                "invalid npm package identity at {}",
                manifest.display()
            ));
        }
        osdk_core::tool::ToolId::parse(&format!("npm:{name}"))?;
        let mut bins = BTreeMap::new();
        if let Some(bin) = value["bin"].as_str() {
            bins.insert(
                name.rsplit('/').next().unwrap().to_string(),
                bin.to_string(),
            );
        } else if let Some(entries) = value["bin"].as_object() {
            for (name, target) in entries {
                let target = target
                    .as_str()
                    .ok_or_else(|| anyhow!("invalid npm bin at {}", manifest.display()))?;
                bins.insert(name.clone(), target.to_string());
            }
        }
        for (name, target) in &bins {
            osdk_core::pipeline::validate_safe_filename("npm bin", name)?;
            let normalized = target.replace('\\', "/");
            if normalized.starts_with('/')
                || normalized.contains(':')
                || normalized.split('/').any(|part| part == "..")
            {
                return Err(anyhow!("unsafe npm bin target at {}", manifest.display()));
            }
        }
        let native = value["gypfile"].as_bool() == Some(true)
            || value["hasInstallScript"].as_bool() == Some(true)
            || root.join("binding.gyp").exists()
            || ["preinstall", "install", "postinstall"]
                .iter()
                .any(|key| value["scripts"][key].is_string());
        out.push(GlobalPackage {
            name: name.into(),
            version: version.into(),
            root,
            bins,
            native,
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}

pub(super) fn doctor_node_globals(app: &App) -> Result<()> {
    let shared = packages(&app.ctx.dirs.npm_global_prefix())?;
    let node = app.registry.get("node")?;
    for version in node.list_installed(&app.ctx)? {
        for package in packages(&app.ctx.dirs.install_path("node", &version))? {
            println!(
                "  {}",
                t!(
                    "doctor.node_global",
                    package = package.spec(),
                    version = version,
                    path = package.root.display()
                )
            );
            if let Some(other) = shared.iter().find(|other| other.name == package.name) {
                println!(
                    "    {}",
                    t!(
                        "doctor.node_global_duplicate",
                        package = other.spec(),
                        path = other.root.display()
                    )
                );
            }
            println!(
                "    osdk node migrate-packages --from {version} --to-global --package {}",
                package.name
            );
        }
    }
    Ok(())
}

/// Stage a complete shared prefix so a failed install cannot damage either
/// live copy. Applying a differing duplicate requires selection by package name.
pub(super) fn migrate_to_shared(
    app: &App,
    from: &str,
    selected: &[String],
    apply: bool,
) -> Result<()> {
    let source = managed_node_tools(app, from)?;
    let prefix = app.ctx.dirs.npm_global_prefix();
    let _lock = osdk_core::lock::FileLock::acquire(
        app.ctx.dirs.data.join("locks/npm-global-migration.lock"),
    )?;
    let source_packages = packages(&source.prefix)?;
    let target_packages = packages(&prefix)?;
    for name in selected {
        if !source_packages.iter().any(|package| &package.name == name) {
            return Err(anyhow!(
                "no source package `{name}` under {}",
                source.prefix.display()
            ));
        }
    }
    let mut planned = Vec::new();
    let mut conflicts = false;
    for package in &source_packages {
        if !selected.is_empty() && !selected.contains(&package.name) {
            continue;
        }
        if package.native {
            println!(
                "{}",
                t!("msg.node_migrate_skip_native", package = package.spec())
            );
            continue;
        }
        let existing = target_packages
            .iter()
            .find(|other| other.name == package.name);
        println!("{} -> {}", package.root.display(), prefix.display());
        if let Some(other) = existing.filter(|other| other.version != package.version) {
            println!(
                "{}",
                t!(
                    "msg.node_migrate_duplicate",
                    source = package.spec(),
                    target = other.spec(),
                    name = package.name
                )
            );
            if !selected.contains(&package.name) {
                conflicts = true;
                continue;
            }
        }
        println!("{}", t!("msg.node_migrate_plan", package = package.spec()));
        planned.push(package);
    }
    if !apply {
        println!("{}", t!("msg.node_migrate_dry_run"));
        return Ok(());
    }
    if conflicts {
        return Err(anyhow!(t!("err.node_migrate_select_duplicate")));
    }
    if planned.is_empty() {
        println!("{}", t!("msg.node_migrate_nothing"));
        return Ok(());
    }
    // Replacing one package must not overwrite another package's command.
    let replaced = planned
        .iter()
        .map(|package| package.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut owners = BTreeMap::new();
    for package in target_packages
        .iter()
        .filter(|package| !replaced.contains(package.name.as_str()))
        .chain(planned.iter().copied())
    {
        for name in package.bins.keys() {
            if let Some(previous) = owners.insert(name, &package.name) {
                return Err(anyhow!(
                    "npm command `{name}` is provided by {previous} and {}",
                    package.name
                ));
            }
        }
    }
    std::fs::create_dir_all(app.ctx.dirs.global_packages())?;
    let staging = tempfile::Builder::new()
        .prefix(".npm-migrate-")
        .tempdir_in(app.ctx.dirs.global_packages())?;
    let staged_prefix = staging.path().join("next");
    copy_prefix(&prefix, &staged_prefix)?;
    let target = ManagedNodeTools {
        bin: source.bin.clone(),
        npm: source.npm.clone(),
        prefix: staged_prefix.clone(),
    };
    let specs = planned
        .iter()
        .map(|package| package.spec())
        .collect::<Vec<_>>();
    let mut args = vec![
        "install",
        "-g",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
    ];
    args.extend(specs.iter().map(String::as_str));
    npm_command(&target, &args)?;
    verify_migrated(&staged_prefix, &planned)?;
    let previous = staging.path().join("previous");
    let had_previous = prefix.exists();
    if had_previous {
        std::fs::rename(&prefix, &previous)?;
    }
    if let Err(error) = std::fs::rename(&staged_prefix, &prefix) {
        if had_previous {
            if let Err(restore) = std::fs::rename(&previous, &prefix) {
                let backup = staging.keep();
                return Err(anyhow!("publishing npm prefix failed: {error}; restoring failed: {restore}; backup preserved at {}", backup.display()));
            }
        }
        return Err(error.into());
    }
    // Only now may npm remove the legacy files/launchers. Its prefix is pinned
    // explicitly, independent of all injected and persisted defaults.
    let mut args = vec![
        "uninstall",
        "-g",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
    ];
    args.extend(planned.iter().map(|package| package.name.as_str()));
    if let Err(error) = npm_command(&source, &args) {
        let backup = staging.keep();
        return Err(error.context(format!("shared packages were installed; source cleanup failed. Previous shared prefix backup: {}", backup.display())));
    }
    for package in &planned {
        if package.root.exists() {
            let backup = staging.keep();
            return Err(anyhow!(
                "shared package installed but legacy package still exists: {}; previous shared prefix backup: {}",
                package.root.display(), backup.display()
            ));
        }
    }
    println!(
        "{}",
        t!(
            "msg.node_migrate_shared",
            count = planned.len(),
            path = prefix.display()
        )
    );
    reshim(app)
}

fn verify_migrated(prefix: &Path, expected: &[&GlobalPackage]) -> Result<()> {
    let actual = packages(prefix)?;
    for package in expected {
        let installed = actual
            .iter()
            .find(|installed| {
                installed.name == package.name && installed.version == package.version
            })
            .ok_or_else(|| {
                anyhow!(
                    "npm did not install {} under {}",
                    package.spec(),
                    prefix.display()
                )
            })?;
        if installed.bins != package.bins {
            return Err(anyhow!("npm bin metadata changed for {}", package.spec()));
        }
        let root = std::fs::canonicalize(&installed.root)?;
        for (name, target) in &installed.bins {
            let target = std::fs::canonicalize(installed.root.join(target.replace('\\', "/")))?;
            if !target.starts_with(&root) || !target.is_file() {
                return Err(anyhow!("invalid migrated executable {}", target.display()));
            }
            let launcher = if cfg!(windows) {
                prefix.join(format!("{name}.cmd"))
            } else {
                prefix.join("bin").join(name)
            };
            if !launcher.is_file() {
                return Err(anyhow!(
                    "missing migrated npm launcher {}",
                    launcher.display()
                ));
            }
            #[cfg(unix)]
            if std::fs::canonicalize(&launcher)? != target {
                return Err(anyhow!(
                    "migrated launcher {} points at another package",
                    launcher.display()
                ));
            }
        }
    }
    Ok(())
}

fn copy_prefix(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if !source.exists() {
        return Ok(());
    }
    if std::fs::symlink_metadata(source)?.file_type().is_symlink() {
        return Err(anyhow!(
            "cannot stage a symlinked npm prefix {}",
            source.display()
        ));
    }
    let canonical_source = std::fs::canonicalize(source)?;
    for entry in walkdir::WalkDir::new(source)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry?;
        let to = target.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            // Absolute links would keep pointing at the live prefix while
            // staging. Linked global packages need a manual migration.
            if link.is_absolute() {
                return Err(anyhow!(
                    "cannot stage absolute npm link {}",
                    entry.path().display()
                ));
            }
            if !std::fs::canonicalize(entry.path())?.starts_with(&canonical_source) {
                return Err(anyhow!(
                    "npm link escapes prefix: {}",
                    entry.path().display()
                ));
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, to)?;
            #[cfg(windows)]
            if entry.path().is_dir() {
                std::os::windows::fs::symlink_dir(link, to)?;
            } else {
                std::os::windows::fs::symlink_file(link, to)?;
            }
        } else if entry.file_type().is_dir() {
            std::fs::create_dir_all(to)?;
        } else if entry.file_type().is_file() {
            std::fs::copy(entry.path(), to)?;
        } else {
            return Err(anyhow!("unsupported npm file {}", entry.path().display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_copies_package_bytes_without_aliasing_the_live_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        for name in ["first", "@scope/second"] {
            let root = modules(&source).join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("package.json"),
                serde_json::json!({"name":name,"version":"1.0.0"}).to_string(),
            )
            .unwrap();
            std::fs::write(root.join("payload"), "original").unwrap();
        }
        let target = temp.path().join("target");
        copy_prefix(&source, &target).unwrap();
        assert_eq!(packages(&target).unwrap().len(), 2);
        std::fs::write(modules(&target).join("first/payload"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(modules(&source).join("first/payload")).unwrap(),
            "original"
        );
    }

    #[test]
    fn npm_bin_validation_rejects_both_platforms_path_escapes() {
        for bin in ["../outside", "..\\outside", "/outside", "C:\\outside"] {
            let temp = tempfile::tempdir().unwrap();
            let root = modules(temp.path()).join("example");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("package.json"),
                serde_json::json!({"name":"example","version":"1.0.0","bin":{"example":bin}})
                    .to_string(),
            )
            .unwrap();
            assert!(packages(temp.path()).is_err(), "accepted {bin}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn staging_preserves_internal_launchers_but_rejects_links_outside_the_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(source.join("entry"), "original").unwrap();
        std::os::unix::fs::symlink("../entry", source.join("bin/example")).unwrap();
        let target = temp.path().join("target");
        copy_prefix(&source, &target).unwrap();
        assert_eq!(
            std::fs::read_link(target.join("bin/example")).unwrap(),
            PathBuf::from("../entry")
        );
        std::fs::write(temp.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("../outside", source.join("escape")).unwrap();
        assert!(copy_prefix(&source, &temp.path().join("rejected")).is_err());
    }
}
