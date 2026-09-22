//! Go, Cargo and Deno deps providers.
//!
//! These three differ from Node and Python in a way that simplifies them: their
//! dependency-fetch step does not execute the dependency's code. Measured
//! 2026-09-22 (go 1.27.1, cargo 1.98.0, deno 2.9.6), recorded in
//! `docs/research/osdk-deps-design-2026-09-22.zh-CN.md` §5.4.4:
//!
//! * `go mod download` only fetches -- no artifact appears in the project.
//! * `cargo fetch` leaves no `target/`, so `build.rs` did not run. Build scripts
//!   are a `cargo build` concern, not a fetch concern.
//! * `deno install` downloads without creating `node_modules`.
//!
//! So there is no `--ignore-scripts` equivalent to pass here; the "declaring a
//! dependency does not run code" premise holds without help.
//!
//! All three also have a *real* frozen mode, unlike yarn classic and bun which
//! exit 0 and install anyway with no lockfile. One of them is stricter than
//! anything else in this subsystem: `cargo fetch --locked` fails when the lock is
//! merely *stale*, where `uv sync --frozen` would silently install the old set.
//! That is why each ecosystem's freeze semantics had to be measured separately
//! rather than assumed uniform.

use std::collections::BTreeMap;
use std::path::Path;

use super::{
    DeclaredManager, DepsProviderSchema, DetectedProject, Ecosystem, InstallerChoice,
    ProviderConfig, RequiredTool, RunPlan, ToolRole, DEFAULT_TRUST_PROFILE,
};
use crate::error::{Error, Result};

/// Go's downloads land in the module cache, not the project, so there is no
/// project-local output to require. `vendor/` is the exception but it is opt-in,
/// and treating its absence as staleness would report every non-vendored project
/// as permanently stale.
pub static GO: DepsProviderSchema = DepsProviderSchema {
    id: "go",
    ecosystem: Ecosystem::Go,
    manifests: &["go.mod"],
    native_locks: &["go.sum"],
    default_sources: &["go.mod", "go.sum"],
    default_outputs: &[],
    required_tools: &[RequiredTool {
        id: "go",
        role: ToolRole::Installer,
    }],
    trust: DEFAULT_TRUST_PROFILE,
};

/// Same reasoning: cargo's downloads go to `CARGO_HOME/registry`. `target/` is a
/// build artifact, and `cargo fetch` deliberately does not create it.
pub static CARGO: DepsProviderSchema = DepsProviderSchema {
    id: "cargo",
    ecosystem: Ecosystem::Rust,
    manifests: &["Cargo.toml"],
    native_locks: &["Cargo.lock"],
    default_sources: &["Cargo.toml", "Cargo.lock"],
    default_outputs: &[],
    required_tools: &[RequiredTool {
        id: "rust",
        role: ToolRole::Installer,
    }],
    trust: DEFAULT_TRUST_PROFILE,
};

pub static DENO: DepsProviderSchema = DepsProviderSchema {
    id: "deno",
    ecosystem: Ecosystem::Deno,
    manifests: &["deno.json", "deno.jsonc"],
    native_locks: &["deno.lock"],
    default_sources: &["deno.json", "deno.jsonc", "deno.lock"],
    default_outputs: &[],
    required_tools: &[RequiredTool {
        id: "deno",
        role: ToolRole::Installer,
    }],
    trust: DEFAULT_TRUST_PROFILE,
};

/// Which provider owns a lockfile, by file name.
pub fn lock_owner(file_name: &str) -> Option<&'static str> {
    match file_name {
        "go.sum" => Some("go"),
        "Cargo.lock" => Some("cargo"),
        "deno.lock" => Some("deno"),
        _ => None,
    }
}

/// None of these manifests declares which installer manages it.
///
/// There is only one installer per ecosystem here, so unlike `packageManager` in
/// `package.json` there is nothing to declare. The manifest is still read, so an
/// unparseable one is an error rather than a silent "no declaration" that falls
/// through to a different provider.
pub fn declared_manager(
    _schema: &DepsProviderSchema,
    manifest: &Path,
) -> Result<Option<DeclaredManager>> {
    let name = manifest.file_name().and_then(|name| name.to_str());
    match name {
        // TOML and JSON can be validated cheaply. `go.mod` has its own grammar
        // that osdk does not parse, so it is only checked for readability --
        // claiming to validate it would overstate what happens here.
        Some("Cargo.toml") => {
            let text =
                std::fs::read_to_string(manifest).map_err(|error| Error::io(manifest, error))?;
            toml::from_str::<toml::Value>(&text)
                .map_err(|error| Error::config(format!("{}: {error}", manifest.display())))?;
        }
        Some("deno.json") => {
            let text =
                std::fs::read_to_string(manifest).map_err(|error| Error::io(manifest, error))?;
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|error| Error::config(format!("{}: {error}", manifest.display())))?;
        }
        // deno.jsonc allows comments, which serde_json rejects; reading is the
        // most that can be checked without a jsonc parser.
        _ => {
            std::fs::read(manifest).map_err(|error| Error::io(manifest, error))?;
        }
    }
    Ok(None)
}

/// Build the install command for a Go, Rust or Deno project.
pub fn plan(
    project: &DetectedProject,
    _choice: &InstallerChoice,
    config: &ProviderConfig,
    _tool_versions: &BTreeMap<String, String>,
) -> Result<RunPlan> {
    let has_lock = project.native_lock.is_some();
    let mut args: Vec<String> = Vec::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut downgraded_reason = None;
    let mut frozen = false;
    let program_candidates;
    let tool;

    match project.provider.as_ref() {
        "go" => {
            tool = "go";
            program_candidates = vec!["go.exe".to_string(), "go".to_string()];
            args.push("mod".into());
            args.push("download".into());
            // `go.sum` is go's own checksum database, and `go mod download`
            // verifies against it, so a present go.sum already means a verified
            // fetch -- there is no extra flag to ask for.
            if has_lock {
                frozen = true;
            } else {
                downgraded_reason = Some(format!(
                    "no go.sum in {}; `go mod download` will create one",
                    project.root.display()
                ));
            }
            // The default `GOTOOLCHAIN=auto` makes go fetch and switch to a
            // different toolchain when go.mod asks for a newer one (measured: it
            // prints `go: downloading go1.99.0`). That would run the build under
            // a Go osdk did not select and did not verify, which is the same
            // hazard `UV_PYTHON_DOWNLOADS=never` closes on the Python side.
            env.insert("GOTOOLCHAIN".into(), "local".into());
            if let Some(index) = &config.index {
                env.insert("GOPROXY".into(), index.clone());
            }
        }
        "cargo" => {
            tool = "rust";
            program_candidates = vec!["cargo.exe".to_string(), "cargo".to_string()];
            args.push("fetch".into());
            if has_lock {
                // Stricter than every other provider here: measured to fail
                // (exit 101) both when the lock is absent and when it is merely
                // out of date. That is the guarantee `uv sync --frozen` does not
                // give, so it is worth asking for explicitly.
                args.push("--locked".into());
                frozen = true;
            } else {
                downgraded_reason = Some(format!(
                    "no Cargo.lock in {}; running `cargo fetch` without --locked, \
                     which will create one",
                    project.root.display()
                ));
            }
        }
        "deno" => {
            tool = "deno";
            program_candidates = vec!["deno.exe".to_string(), "deno".to_string()];
            args.push("install".into());
            if has_lock {
                args.push("--frozen".into());
                frozen = true;
            } else {
                downgraded_reason = Some(format!(
                    "no deno.lock in {}; running `deno install` without --frozen, \
                     which will create one",
                    project.root.display()
                ));
            }
        }
        other => {
            return Err(Error::other(format!(
                "`{other}` is not a go/cargo/deno deps provider"
            )))
        }
    }

    for (key, value) in &config.env {
        env.insert(key.clone(), value.clone());
    }

    Ok(RunPlan {
        tool: tool.into(),
        program_candidates,
        args,
        env,
        cwd: super::effective_cwd(&project.root, config.dir.as_deref()),
        frozen,
        downgraded_reason,
        // None of the three needs an environment created first.
        prelude: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(
        provider: &'static str,
        ecosystem: Ecosystem,
        manifest: &Path,
        lock: Option<&Path>,
    ) -> DetectedProject {
        DetectedProject {
            provider: provider.into(),
            ecosystem,
            root: manifest.parent().unwrap().to_path_buf(),
            manifest: manifest.to_path_buf(),
            native_lock: lock.map(Path::to_path_buf),
            declared_manager: None,
        }
    }

    fn choice(provider: &'static str) -> InstallerChoice {
        InstallerChoice {
            provider: provider.into(),
            version: None,
            origin: super::super::InstallerOrigin::Default,
        }
    }

    fn build(
        provider: &'static str,
        ecosystem: Ecosystem,
        manifest_name: &str,
        lock_name: Option<&str>,
    ) -> (RunPlan, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join(manifest_name);
        std::fs::write(&manifest, "").unwrap();
        let lock = lock_name.map(|name| {
            let lock = temp.path().join(name);
            std::fs::write(&lock, "").unwrap();
            lock
        });
        let got = plan(
            &project(provider, ecosystem, &manifest, lock.as_deref()),
            &choice(provider),
            &ProviderConfig::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        (got, temp)
    }

    /// go never gets to pick its own toolchain.
    ///
    /// `GOTOOLCHAIN=auto` (the default) was measured to attempt
    /// `go: downloading go1.99.0` when `go.mod` asks for a newer Go. That would
    /// run the fetch under a toolchain osdk neither selected nor verified -- the
    /// same hazard as uv fetching an interpreter, and the reason this is pinned
    /// rather than left at its default.
    #[test]
    fn go_never_switches_its_own_toolchain() {
        let (got, _temp) = build("go", Ecosystem::Go, "go.mod", Some("go.sum"));
        assert_eq!(got.args, vec!["mod", "download"]);
        assert_eq!(
            got.env.get("GOTOOLCHAIN").map(String::as_str),
            Some("local")
        );
        assert!(
            got.frozen,
            "a present go.sum is go's verified-fetch guarantee"
        );
        assert!(got.downgraded_reason.is_none());

        let (got, _temp) = build("go", Ecosystem::Go, "go.mod", None);
        assert!(!got.frozen);
        assert!(got.downgraded_reason.is_some());
        // Pinned in both cases: the hazard does not depend on the lock.
        assert_eq!(
            got.env.get("GOTOOLCHAIN").map(String::as_str),
            Some("local")
        );
    }

    /// cargo asks for `--locked`, which is the strictest freeze in this
    /// subsystem.
    ///
    /// Measured to fail at exit 101 both with no lock and with a stale one. The
    /// flag is named explicitly because a weaker spelling (plain `cargo fetch`)
    /// would silently update the lock instead.
    #[test]
    fn cargo_asks_for_the_strict_freeze() {
        let (got, _temp) = build("cargo", Ecosystem::Rust, "Cargo.toml", Some("Cargo.lock"));
        assert_eq!(got.args, vec!["fetch", "--locked"]);
        assert!(got.frozen);
        assert_eq!(got.tool, "rust", "cargo comes from the rust toolchain");

        let (got, _temp) = build("cargo", Ecosystem::Rust, "Cargo.toml", None);
        assert_eq!(got.args, vec!["fetch"]);
        assert!(!got.frozen);
        assert!(got.downgraded_reason.is_some());
    }

    /// deno's frozen mode, present and reported when it cannot be used.
    #[test]
    fn deno_freezes_only_with_a_lock() {
        let (got, _temp) = build("deno", Ecosystem::Deno, "deno.json", Some("deno.lock"));
        assert_eq!(got.args, vec!["install", "--frozen"]);
        assert!(got.frozen);

        let (got, _temp) = build("deno", Ecosystem::Deno, "deno.json", None);
        assert_eq!(got.args, vec!["install"]);
        assert!(!got.frozen);
        assert!(got.downgraded_reason.is_some());
    }

    /// No `--ignore-scripts` equivalent is passed, because there is nothing to
    /// suppress.
    ///
    /// Measured: `cargo fetch` leaves no `target/` (so `build.rs` did not run),
    /// `go mod download` produces no artifact, and `deno install` creates no
    /// `node_modules`. Adding a flag here "for symmetry" with npm would either be
    /// rejected by the tool or quietly do nothing -- both worse than not passing
    /// it. This test exists so a later change cannot add one unnoticed.
    #[test]
    fn no_script_suppressing_flag_is_invented() {
        for (provider, ecosystem, manifest, lock) in [
            ("go", Ecosystem::Go, "go.mod", "go.sum"),
            ("cargo", Ecosystem::Rust, "Cargo.toml", "Cargo.lock"),
            ("deno", Ecosystem::Deno, "deno.json", "deno.lock"),
        ] {
            let (got, _temp) = build(provider, ecosystem, manifest, Some(lock));
            assert!(
                !got.args.iter().any(|arg| arg.contains("ignore-scripts")
                    || arg.contains("no-build")
                    || arg.contains("frozen-lockfile")),
                "{provider} got a flag from another ecosystem: {:?}",
                got.args
            );
            assert!(got.prelude.is_empty(), "{provider} needs no prelude");
        }
    }

    /// An index override maps to the ecosystem's own variable, and only for the
    /// ecosystem that has one.
    #[test]
    fn an_index_maps_only_where_it_exists() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("go.mod");
        std::fs::write(&manifest, "").unwrap();
        let config = ProviderConfig {
            index: Some("https://goproxy.example.com".into()),
            ..ProviderConfig::default()
        };
        let got = plan(
            &project("go", Ecosystem::Go, &manifest, None),
            &choice("go"),
            &config,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            got.env.get("GOPROXY").map(String::as_str),
            Some("https://goproxy.example.com")
        );
        // GOTOOLCHAIN must survive an index override.
        assert_eq!(
            got.env.get("GOTOOLCHAIN").map(String::as_str),
            Some("local")
        );
    }

    /// A manifest that will not parse is an error, not "no declaration".
    #[test]
    fn an_unparseable_manifest_is_an_error() {
        let temp = tempfile::tempdir().unwrap();

        let cargo = temp.path().join("Cargo.toml");
        std::fs::write(&cargo, "[package\nname = broken").unwrap();
        assert!(declared_manager(&CARGO, &cargo).is_err());
        std::fs::write(&cargo, "[package]\nname = \"p\"\n").unwrap();
        assert!(declared_manager(&CARGO, &cargo).unwrap().is_none());

        let deno = temp.path().join("deno.json");
        std::fs::write(&deno, "{not json").unwrap();
        assert!(declared_manager(&DENO, &deno).is_err());
        std::fs::write(&deno, "{}").unwrap();
        assert!(declared_manager(&DENO, &deno).unwrap().is_none());

        // `deno.jsonc` allows comments, so it is only checked for readability --
        // asserting it parses as JSON would reject a valid file.
        let jsonc = temp.path().join("deno.jsonc");
        std::fs::write(&jsonc, "{ // a comment\n}").unwrap();
        assert!(declared_manager(&DENO, &jsonc).unwrap().is_none());

        // go.mod has its own grammar osdk does not parse; a missing file is
        // still an error rather than a silent pass.
        let gomod = temp.path().join("go.mod");
        assert!(declared_manager(&GO, &gomod).is_err());
    }

    /// Lock ownership is one-to-one here, and unknown names are not claimed.
    #[test]
    fn lock_ownership_is_exact() {
        assert_eq!(lock_owner("go.sum"), Some("go"));
        assert_eq!(lock_owner("Cargo.lock"), Some("cargo"));
        assert_eq!(lock_owner("deno.lock"), Some("deno"));
        assert_eq!(lock_owner("package-lock.json"), None);
        assert_eq!(lock_owner("go.mod"), None);
    }
}
