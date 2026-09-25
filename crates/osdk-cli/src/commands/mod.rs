//! Command handlers.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use osdk_core::backend::native_tool::{
    LOCKED_NATIVE_RUNTIME_OPTION, LOCKED_NATIVE_RUNTIME_VERSION_OPTION,
};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::inventory::ScanReport;
use osdk_core::package_registry::{self, PackageManager, RegistryPlan, RegistryProbe};
use osdk_core::source::select;
use osdk_core::t;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::app::App;
use crate::cli::{
    AliasCommand, AndroidAvdCommand, AndroidCommand, AndroidLicensesCommand, AndroidSdkRootCommand,
    ConfigCommand, ModelCommand, ModelEnvCommand, NodeCommand, PythonCommand, RegistryCommand,
    RustCommand, RustItemCommand, RustOverrideCommand, RustToolchainCommand, SourceCommand,
    TrustCommand,
};

mod install;
mod list;
mod maintenance;
mod registry;
mod runtimes;
mod shims;
mod uninstall;
mod use_cmd;

pub(crate) use install::*;
pub(crate) use list::*;
pub(crate) use maintenance::*;
pub(crate) use registry::*;
pub(crate) use runtimes::*;
pub(crate) use shims::*;
pub(crate) use uninstall::*;
pub(crate) use use_cmd::*;

#[cfg(test)]
mod command_flow_tests {
    use super::*;
    use std::sync::Arc;

    use crate::prompt::TerminalPrompt;

    #[test]
    fn every_writable_setting_can_also_be_read_back() {
        // `set` and `get` are driven by two separate lists. If they drift, a
        // key accepts a value and then reports `unknown setting` on read.
        let defaults = osdk_core::config::Settings::default();
        let registries = osdk_core::config::RegistriesConfig::default();
        let sources = osdk_core::config::SourcesConfig::default();
        for setting in crate::config_edit::SETTINGS {
            // Three independent read paths now exist: `Settings`, the registry
            // tables, and the `[sources]` table. A key is readable if any renders
            // it.
            let rendered = registry_setting_display(&registries, setting.key)
                .or_else(|| sources_setting_display(&sources, setting.key))
                .or_else(|| setting_display(&defaults, setting.key));
            assert!(
                rendered.is_some(),
                "`{}` is settable but `config get` cannot render it",
                setting.key
            );
        }

        // And an unconfigured registry list must say "default" rather than look
        // like a configured-but-empty set.
        assert_eq!(
            registry_setting_display(&registries, "registries.python.urls").as_deref(),
            Some("default")
        );
    }

    fn layered_npm_tool_config() -> (tempfile::TempDir, osdk_core::config::Config) {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project/nested");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"npm:fixture-cli" = { version = "1", installer = "pnpm", allow_builds = ["global-build"] }
"#,
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("project/osdk.toml"),
            r#"
[tools]
"npm:fixture-cli" = { version = "2", installer = "npm", allow_builds = ["project-build"] }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        (temporary, config)
    }

    fn app_with_config(
        temporary: &tempfile::TempDir,
        config: osdk_core::config::Config,
    ) -> crate::app::App {
        let dirs = osdk_core::dirs::Dirs {
            data: temporary.path().join("data"),
            cache: temporary.path().join("cache"),
            config: temporary.path().join("config"),
            store: temporary.path().join("store"),
            installs: temporary.path().join("installs"),
        };
        for directory in [
            &dirs.data,
            &dirs.cache,
            &dirs.config,
            &dirs.store,
            &dirs.installs,
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let client = osdk_core::http::client().unwrap();
        let cas = Arc::new(osdk_core::store::Cas::new(dirs.store.clone()));
        let registry = osdk_core::Registry::load(&dirs).unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs,
            platform: osdk_core::platform::Platform::current(),
            config,
            client,
            cas,
            show_progress: false,
        };
        crate::app::App::from_parts(
            ctx,
            registry,
            Arc::new(TerminalPrompt::new(false)),
            None,
            false,
        )
    }

    /// A named operand without `@` must inherit the project's pin.
    ///
    /// This is what `osdk exec -t java -- ...` does. Before the fix the absent
    /// selector became `VersionSpec::Latest` and the backend resolved it against
    /// the remote index, so a project pinning JDK 21 got whatever the newest
    /// published JDK was and its Gradle build failed on the toolchain check.
    /// `-t android-platforms` behaved the same way and landed on `android-37.2`.
    ///
    /// The assertions are on the *spec*, not on an installed version: resolution
    /// is what the bug was, and specs need no network.
    #[test]
    fn bare_named_operand_inherits_the_project_pin() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        // A global pin that differs from the project's, so "took the global one"
        // and "took the project one" are distinguishable outcomes.
        std::fs::write(&user_config, "[tools]\njava = \"26.0.2.1+1\"\n").unwrap();
        std::fs::write(
            project.join("osdk.toml"),
            "[tools]\njava = \"21.0.12.1+1\"\nandroid-platforms = \"android-36-ext19\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let bind = |operand: &str| {
            let mut request = resolve_explicit_request(
                &app,
                operand,
                &app.ctx.config.tool_configs,
                &app.ctx.config.tools,
            )
            .unwrap();
            bind_configured_spec_for_bare_operand_at(&app, operand, &mut request, &project);
            request.spec
        };

        // `21.0.12.1+1` is a four-part Java PSU, which is not valid semver, so
        // `VersionSpec::parse` classifies it as a prefix. That is the same
        // classification the shim and `osdk current` give the identical string,
        // and `select_exact`'s dotted-prefix tier still resolves it to exactly
        // that release. What matters here is that the pin's text arrived at all.
        assert_eq!(bind("java"), VersionSpec::Prefix("21.0.12.1+1".into()));
        assert_eq!(
            bind("android-platforms"),
            VersionSpec::Prefix("android-36-ext19".into())
        );
        // An explicit `@latest` is a deliberate request for the newest release
        // and must NOT be rewritten into the pin.
        assert_eq!(bind("java@latest"), VersionSpec::Latest);
        // An explicit different version stays untouched too.
        assert_eq!(
            bind("java@17.0.13+11"),
            VersionSpec::Exact("17.0.13+11".into())
        );
    }

    /// With nothing configured anywhere, a bare operand still means `latest`.
    ///
    /// The fix must not turn "no pin" into a failure or into some arbitrary
    /// installed version; `osdk install <tool>` on a fresh machine has to keep
    /// working.
    #[test]
    fn bare_named_operand_without_any_pin_still_means_latest() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\nnode = \"20.11.1\"\n").unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let mut request = resolve_explicit_request(
            &app,
            "go",
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )
        .unwrap();
        bind_configured_spec_for_bare_operand_at(&app, "go", &mut request, &project);

        assert_eq!(request.spec, VersionSpec::Latest);
    }

    #[test]
    fn android_r8_tools_resolve_to_build_tools_instead_of_conflicting() {
        // Google ships the same R8 launchers in two families. Without a
        // precedence rule this refused every shim of whichever family was
        // installed second, including sdkmanager, which nothing else provides.
        let both = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
        ]);
        for name in ["d8", "r8", "retrace", "resourceshrinker"] {
            assert!(!is_real_shim_conflict(name, &both), "{name}");
            assert_eq!(
                osdk_core::shim::precedence_winner(name, &both),
                Some("android-build-tools"),
                "{name}"
            );
        }
    }

    #[test]
    fn names_owned_by_one_android_family_have_no_precedence_winner() {
        // `sdkmanager` and `aapt2` are single-owner; they must stay ordinary.
        let cmdline = std::collections::BTreeSet::from(["android-cmdline-tools".to_string()]);
        assert!(!is_real_shim_conflict("sdkmanager", &cmdline));
        assert_eq!(
            osdk_core::shim::precedence_winner("sdkmanager", &cmdline),
            None
        );
        // A name outside the curated set stays a real conflict.
        let unrelated = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "node".to_string(),
        ]);
        assert!(is_real_shim_conflict("aapt2", &unrelated));
    }

    #[test]
    fn an_unexpected_third_owner_is_still_a_real_conflict() {
        // Precedence only decides among the known Android families; anything
        // else must still be surfaced to the user.
        let with_outsider = std::collections::BTreeSet::from([
            "android-build-tools".to_string(),
            "android-cmdline-tools".to_string(),
            "npm:some-d8-clone".to_string(),
        ]);
        assert_eq!(
            osdk_core::shim::precedence_winner("d8", &with_outsider),
            None
        );
        assert!(is_real_shim_conflict("d8", &with_outsider));
    }
    #[test]
    fn global_npm_use_options_ignore_project_scope_and_apply_cli_last() {
        let (_temporary, config) = layered_npm_tool_config();
        let mut configured = ToolRequest::parse("npm:fixture-cli@3").unwrap();

        apply_use_options(&config, &mut configured, true, &[], None).unwrap();

        assert_eq!(configured.options["installer"], "pnpm");
        assert_eq!(configured.options["allow_builds"], "global-build");

        let mut overridden = ToolRequest::parse("npm:fixture-cli@3").unwrap();
        apply_use_options(
            &config,
            &mut overridden,
            true,
            &["installer=pnpm".into(), "allow_builds=cli-build".into()],
            None,
        )
        .unwrap();

        assert_eq!(overridden.options["installer"], "pnpm");
        assert_eq!(overridden.options["allow_builds"], "cli-build");
    }

    #[test]
    fn local_npm_use_options_keep_project_merged_values() {
        let (_temporary, config) = layered_npm_tool_config();
        let mut request = ToolRequest::parse("npm:fixture-cli@3").unwrap();

        apply_use_options(&config, &mut request, false, &[], None).unwrap();

        assert_eq!(request.options["installer"], "npm");
        assert_eq!(request.options["allow_builds"], "project-build");
    }

    #[test]
    fn npm_lifecycle_hint_preserves_request_identity_options() {
        let mut request =
            ToolRequest::parse("npm:fixture-cli[installer=pnpm,allow_builds='sharp,esbuild']@3")
                .unwrap();
        request
            .options
            .insert("__osdk_npm_node_version".into(), "22.1.0".into());

        let project = npm_scope_hint(&request, false);
        assert_eq!(project.options, request.options);

        let global = npm_scope_hint(&request, true);
        assert_eq!(global.options["installer"], "pnpm");
        assert_eq!(global.options["allow_builds"], "esbuild,sharp");
        assert_eq!(global.options["__osdk_npm_node_version"], "22.1.0");
        assert_eq!(
            global.options[osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION],
            osdk_core::npm_tools::ToolScope::Global.as_str()
        );
    }

    #[test]
    fn explicit_operand_resolves_through_configured_indirect_key() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = { version = "node@20.10.0", corepack = true }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let request = resolve_explicit_request(
            &app,
            "tool.node",
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )
        .unwrap();

        assert_eq!(request.backend, "node");
        assert_eq!(request.spec, VersionSpec::Exact("20.10.0".into()));
        assert_eq!(request.options["corepack"], "true");
    }

    #[test]
    fn explicit_operand_keeps_explicit_spec_when_resolving_indirect_key() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = { version = "node@20.10.0", corepack = true }
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let request = resolve_explicit_request(
            &app,
            "tool.node@22.0.0",
            &app.ctx.config.tool_configs,
            &app.ctx.config.tools,
        )
        .unwrap();

        assert_eq!(request.backend, "node");
        assert_eq!(request.spec, VersionSpec::Exact("22.0.0".into()));
        assert_eq!(request.options["corepack"], "true");
    }

    #[test]
    fn select_use_persist_target_reports_ambiguous_indirect_backend() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            r#"
[tools]
"tool.node" = "node@20.10.0"
"tool.node.lts" = "node@22.0.0"
"#,
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let error = select_use_persist_target(&app, "node", false, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple configured tool keys resolve to `node`"),
            "{error}"
        );
    }

    #[test]
    fn project_npm_specs_reject_conflicting_aliases_and_accept_identical_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        let error = project_npm_configured_specs(&app, &config_path, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("npm:fixture-cli"), "{message}");
        assert!(message.contains("tool.alpha"), "{message}");
        assert!(message.contains("1.2.3"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");
        assert!(message.contains("2.0.0"), "{message}");

        std::fs::write(
            &config_path,
            "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let error = project_npm_configured_specs(&app, &config_path, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("npm:fixture-cli"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");

        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.2.3\"\n\"tool.beta\" = \"npm:fixture-cli@1.2.3\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        assert_eq!(
            project_npm_configured_specs(&app, &config_path, None).unwrap(),
            std::collections::BTreeMap::from([("npm:fixture-cli".into(), "1.2.3".into())])
        );
    }

    #[test]
    fn project_npm_specs_replace_the_current_alias_before_conflict_checking() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        let config_path = project.join("osdk.toml");
        std::fs::write(
            &config_path,
            "[tools]\n\"tool.alpha\" = \"npm:fixture-cli@1.0.0\"\n\"tool.beta\" = \"npm:fixture-cli@2.0.0\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        assert_eq!(
            project_npm_configured_specs(
                &app,
                &config_path,
                Some(("tool.alpha", "npm:fixture-cli", "2.0.0")),
            )
            .unwrap(),
            std::collections::BTreeMap::from([("npm:fixture-cli".into(), "2.0.0".into())])
        );

        let error = project_npm_configured_specs(
            &app,
            &config_path,
            Some(("tool.alpha", "npm:fixture-cli", "3.0.0")),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("tool.alpha"), "{message}");
        assert!(message.contains("3.0.0"), "{message}");
        assert!(message.contains("tool.beta"), "{message}");
        assert!(message.contains("2.0.0"), "{message}");
    }

    #[test]
    fn project_dependency_section_preserves_existing_section_and_defaults_to_dev() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("package.json");
        for (manifest, expected) in [
            (
                r#"{"dependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::Dependencies,
            ),
            (
                r#"{"devDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::DevDependencies,
            ),
            (
                r#"{"optionalDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::OptionalDependencies,
            ),
            (
                r#"{"dependencies":{"prettier":"^2"},"optionalDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::OptionalDependencies,
            ),
            (
                r#"{"peerDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::PeerDependencies { also_dev: false },
            ),
            (
                r#"{"peerDependencies":{"prettier":"^2"},"devDependencies":{"prettier":"^2"}}"#,
                ProjectDependencySection::PeerDependencies { also_dev: true },
            ),
            (
                r#"{"dependencies":{}}"#,
                ProjectDependencySection::DevDependencies,
            ),
        ] {
            std::fs::write(&path, manifest).unwrap();
            assert_eq!(
                project_dependency_section(&path, "prettier").unwrap(),
                expected
            );
        }
    }

    #[test]
    fn project_file_snapshot_restores_old_bytes_and_removes_new_files() {
        let temporary = tempfile::tempdir().unwrap();
        let existing = temporary.path().join("package.json");
        let created = temporary.path().join("package-lock.json");
        std::fs::write(&existing, b"old manifest").unwrap();
        let existing_snapshot = snapshot_project_file(&existing).unwrap();
        let created_snapshot = snapshot_project_file(&created).unwrap();

        std::fs::write(&existing, b"new manifest").unwrap();
        std::fs::write(&created, b"new lock").unwrap();
        existing_snapshot.restore().unwrap();
        created_snapshot.restore().unwrap();

        assert_eq!(std::fs::read(&existing).unwrap(), b"old manifest");
        assert!(!created.exists());
    }

    #[test]
    fn expected_project_native_lock_preserves_incumbent_and_defaults_to_installer() {
        use osdk_core::npm_tools::{NativeLock, NativeLockKind, NpmInstaller, NpmProject};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let project = NpmProject {
            root: root.to_path_buf(),
            package_json: root.join("package.json"),
            declared_manager: None,
            native_lock: Some(NativeLock {
                kind: NativeLockKind::Pnpm,
                path: root.join("pnpm-lock.yaml"),
                version: 9,
                format: "pnpm-v9".into(),
                supported: true,
            }),
        };
        // An incumbent lock wins over what the installer would pick.
        assert_eq!(
            expected_project_native_lock(&project, NpmInstaller::Npm),
            (NativeLockKind::Pnpm, root.join("pnpm-lock.yaml"))
        );

        let without_lock = NpmProject {
            native_lock: None,
            ..project
        };
        assert_eq!(
            expected_project_native_lock(&without_lock, NpmInstaller::Npm),
            (NativeLockKind::PackageLock, root.join("package-lock.json"))
        );
        assert_eq!(
            expected_project_native_lock(&without_lock, NpmInstaller::Pnpm),
            (NativeLockKind::Pnpm, root.join("pnpm-lock.yaml"))
        );
        let switched = NativeLock {
            kind: NativeLockKind::Pnpm,
            path: root.join("pnpm-lock.yaml"),
            version: 9,
            format: "pnpm-v9".into(),
            supported: true,
        };
        let error = validate_installed_project_native_lock(
            NpmInstaller::Npm,
            &(NativeLockKind::PackageLock, root.join("package-lock.json")),
            &switched,
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed native lock identity"));
    }

    #[test]
    fn native_project_args_preserve_sections_and_disable_scripts() {
        use osdk_core::npm_tools::NpmInstaller;

        let cases = [
            (
                NpmInstaller::Npm,
                ProjectDependencySection::Dependencies,
                vec!["install", "--save-prod", "--ignore-scripts", "prettier@3"],
            ),
            (
                NpmInstaller::Npm,
                ProjectDependencySection::DevDependencies,
                vec!["install", "--save-dev", "--ignore-scripts", "prettier@3"],
            ),
            (
                NpmInstaller::Npm,
                ProjectDependencySection::OptionalDependencies,
                vec![
                    "install",
                    "--save-optional",
                    "--ignore-scripts",
                    "prettier@3",
                ],
            ),
            (
                NpmInstaller::Pnpm,
                ProjectDependencySection::PeerDependencies { also_dev: true },
                vec!["add", "--save-peer", "-D", "--ignore-scripts", "prettier@3"],
            ),
        ];
        for (installer, section, expected) in cases {
            assert_eq!(
                project_manager_args(installer, "prettier@3", section),
                expected
            );
        }
    }

    #[test]
    fn project_package_spec_preserves_user_request_and_defaults_to_exact() {
        assert_eq!(
            project_package_spec("prettier", Some("3"), "3.6.2"),
            "prettier@3"
        );
        assert_eq!(
            project_package_spec("@antfu/ni", None, "0.21.12"),
            "@antfu/ni@0.21.12"
        );
    }

    #[test]
    fn exact_resolved_node_is_bound_to_npm_requests() {
        let resolved = vec![(
            ToolRequest::parse("node@20.10.0").unwrap(),
            ToolVersion::new("node", "20.10.0"),
        )];
        let mut requests = vec![
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolRequest::parse("python@3.12.0").unwrap(),
        ];

        bind_request_node_version(&mut requests, &resolved);

        assert_eq!(
            requests[0].options[osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION],
            "20.10.0"
        );
        assert!(requests[1].options.is_empty());
    }

    fn request(backend: &str, spec: VersionSpec) -> ToolRequest {
        ToolRequest {
            backend: backend.into(),
            spec,
            options: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn cargo_requests_inject_exactly_one_configured_rust_request() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            "[tools]\nrust = { version = \"1.91.1\", profile = \"minimal\" }\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request(
            "cargo:https://github.com/BurntSushi/ripgrep.git",
            VersionSpec::Exact("rev:0123456789abcdef0123456789abcdef01234567".into()),
        );

        let requests = inject_rust_dependency_at(&app, vec![cargo], &project).unwrap();
        let rust = requests
            .iter()
            .filter(|request| request.backend == "rust")
            .collect::<Vec<_>>();

        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].spec, VersionSpec::Exact("1.91.1".into()));
        assert_eq!(rust[0].options["profile"], "minimal");
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn cargo_requests_preserve_one_explicit_rust_and_reject_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));
        let rust = ToolRequest::parse("rust@1.91.1").unwrap();

        let requests = inject_rust_dependency(&app, vec![cargo.clone(), rust.clone()]).unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.backend == "rust")
                .count(),
            1
        );
        let error = inject_rust_dependency(&app, vec![cargo, rust.clone(), rust]).unwrap_err();
        assert!(error.to_string().contains("exactly one managed Rust"));
    }

    #[test]
    fn cargo_requests_reject_missing_or_floating_rust_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));

        let missing = inject_rust_dependency_at(&app, vec![cargo.clone()], &project).unwrap_err();
        assert!(missing.to_string().contains("configure `rust"), "{missing}");

        for rust in [
            ToolRequest::parse("rust@stable").unwrap(),
            request("rust", VersionSpec::Latest),
        ] {
            let floating =
                inject_rust_dependency_at(&app, vec![cargo.clone(), rust], &project).unwrap_err();
            assert!(
                floating.to_string().contains("exact managed Rust version"),
                "{floating}"
            );
        }
    }

    #[test]
    fn non_cargo_requests_do_not_inject_or_require_rust() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let npm = ToolRequest::parse("npm:prettier@3.6.2").unwrap();

        let requests = inject_rust_dependency_at(&app, vec![npm.clone()], &project).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].backend, npm.backend);
        assert!(requests[0].options.is_empty());
    }

    #[test]
    fn cargo_runtime_partition_and_binding_are_exact_and_npm_neutral() {
        let cargo = request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into()));
        let npm = ToolRequest::parse("npm:prettier@3.6.2").unwrap();
        let rust = ToolRequest::parse("rust@1.91.1").unwrap();
        let node = ToolRequest::parse("node@20.10.0").unwrap();
        let (runtime, mut remaining) =
            partition_runtime_dependency(vec![cargo, npm, rust, node], "rust", "cargo:");
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].backend, "rust");
        assert!(remaining.iter().all(|request| request.backend != "rust"));

        let resolved = vec![(runtime[0].clone(), ToolVersion::new("rust", "1.91.1"))];
        bind_request_rust_version(&mut remaining, &resolved).unwrap();
        let cargo = remaining
            .iter()
            .find(|request| request.backend.starts_with("cargo:"))
            .unwrap();
        assert_eq!(cargo.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert_eq!(
            cargo.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        let npm = remaining
            .iter()
            .find(|request| request.backend.starts_with("npm:"))
            .unwrap();
        assert!(npm.options.is_empty());
    }

    #[test]
    fn resolved_cargo_lock_metadata_uses_the_same_exact_rust_version() {
        let mut resolved = vec![
            (
                request("cargo:ripgrep", VersionSpec::Exact("14.1.1".into())),
                ToolVersion::new("cargo:ripgrep", "14.1.1"),
            ),
            (
                ToolRequest::parse("rust@1.91.1").unwrap(),
                ToolVersion::new("rust", "1.91.1"),
            ),
        ];

        bind_resolved_rust_version(&mut resolved).unwrap();

        assert_eq!(
            resolved[0].1.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        assert_eq!(resolved[0].1.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert!(resolved[1].1.options.is_empty());
    }

    #[test]
    fn go_requests_inject_configured_fuzzy_runtime_and_bind_exact_resolution() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\ngo = \"1.24\"\n").unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();

        let requests = inject_go_dependency_at(&app, vec![tool], &project).unwrap();
        let runtime = requests
            .iter()
            .find(|request| request.backend == "go")
            .unwrap();
        assert_eq!(runtime.spec, VersionSpec::Prefix("1.24".into()));

        let (runtime, mut remaining) = partition_runtime_dependency(requests, "go", "go:");
        let resolved = vec![(runtime[0].clone(), ToolVersion::new("go", "1.24.6"))];
        bind_request_go_version(&mut remaining, &resolved).unwrap();
        assert_eq!(remaining[0].options[LOCKED_NATIVE_RUNTIME_OPTION], "go");
        assert_eq!(
            remaining[0].options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.24.6"
        );
    }

    #[test]
    fn consent_only_opts_still_allow_lockfile_replay() {
        // Consent is absent from the lock by design, so demanding it must not
        // make a committed lock file unusable.
        assert!(opts_are_only_consent(&[]));
        assert!(opts_are_only_consent(&["accept-licenses=true".into()]));
        assert!(opts_are_only_consent(&[
            "accept-licenses=true".into(),
            "accept-license=android-sdk-license".into(),
        ]));

        // Anything that actually selects an artifact keeps the old behaviour of
        // bypassing the lock.
        assert!(!opts_are_only_consent(&["channel=beta".into()]));
        assert!(!opts_are_only_consent(&[
            "accept-licenses=true".into(),
            "profile=minimal".into(),
        ]));
        // A malformed option is not consent either.
        assert!(!opts_are_only_consent(&["accept-licenses".into()]));
    }

    #[test]
    fn generic_opts_do_not_pollute_injected_go_runtime() {
        let mut tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();
        for (key, value) in parse_opts(&["tags=netgo".into()]).unwrap() {
            tool.options.insert(key, value);
        }
        let requests = [tool, ToolRequest::parse("go@1.24").unwrap()];
        let runtime = requests
            .iter()
            .find(|request| request.backend == "go")
            .unwrap();
        let tool = requests
            .iter()
            .find(|request| request.backend.starts_with("go:"))
            .unwrap();
        assert!(runtime.options.is_empty());
        assert_eq!(tool.options["tags"], "netgo");
    }

    #[test]
    fn go_requests_preserve_one_explicit_runtime_and_reject_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let tool = ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap();
        let runtime = ToolRequest::parse("go@latest").unwrap();

        let requests =
            inject_go_dependency_at(&app, vec![tool.clone(), runtime.clone()], &project).unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.backend == "go")
                .count(),
            1
        );
        let error = inject_go_dependency_at(&app, vec![tool, runtime.clone(), runtime], &project)
            .unwrap_err();
        assert!(error.to_string().contains("exactly one managed Go"));
    }

    #[test]
    fn resolved_go_lock_metadata_requires_an_exact_runtime_result() {
        let mut resolved = vec![
            (
                ToolRequest::parse("go:example.com/acme/tool@1.2.3").unwrap(),
                ToolVersion::new("go:example.com/acme/tool", "1.2.3"),
            ),
            (
                ToolRequest::parse("go@1.24").unwrap(),
                ToolVersion::new("go", "1.24.6"),
            ),
        ];
        bind_resolved_go_version(&mut resolved).unwrap();
        assert_eq!(resolved[0].1.options[LOCKED_NATIVE_RUNTIME_OPTION], "go");
        assert_eq!(
            resolved[0].1.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.24.6"
        );

        resolved[1].1.version = "latest".into();
        assert!(bind_resolved_go_version(&mut resolved).is_err());
    }

    #[test]
    fn user_options_cannot_inject_private_go_replay_metadata() {
        for key in [
            "__osdk_native_replay",
            "__osdk_native_runtime",
            "__osdk_native_runtime_version",
            "__osdk_go_proxy",
            "__osdk_go_module",
        ] {
            let option = format!("{key}=value");
            let error = parse_opts(&[option]).unwrap_err();
            assert!(error.to_string().contains("internal option"), "{key}");
            let error = reject_public_internal_options(&std::collections::BTreeMap::from([(
                key.to_string(),
                "value".into(),
            )]))
            .unwrap_err();
            assert!(error.to_string().contains("internal option"), "{key}");
        }
    }

    #[test]
    fn global_go_dependency_uses_global_selection_not_project_override() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(&user_config, "[tools]\ngo = \"1.24\"\n").unwrap();
        std::fs::write(project.join("osdk.toml"), "[tools]\ngo = \"1.23\"\n").unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);

        assert_eq!(app.ctx.config.tools["go"], "1.23");
        let request = global_go_dependency_request(&app).unwrap();
        assert_eq!(request.backend, "go");
        assert_eq!(request.spec, VersionSpec::Prefix("1.24".into()));
    }

    #[test]
    fn go_use_persists_canonical_public_options_only() {
        let request = ToolRequest {
            backend: "go:example.com/acme/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([
                ("tags".into(), "sqlite,netgo,sqlite".into()),
                ("env".into(), "GOAMD64=v3;CGO_ENABLED=0".into()),
            ]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved.options.extend(std::collections::BTreeMap::from([
            ("tags".into(), "netgo,sqlite".into()),
            ("env".into(), "CGO_ENABLED=0;GOAMD64=v3".into()),
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), "1.24.6".into()),
            (
                osdk_core::backend::native_tool::LOCKED_NATIVE_REPLAY_OPTION.into(),
                "version-only".into(),
            ),
            (
                osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
                "https://proxy.golang.org".into(),
            ),
            (
                osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION.into(),
                "example.com/acme/tool".into(),
            ),
        ]));
        bind_dynamic_request_options(&request, &mut resolved);
        let canonical =
            osdk_core::backend::dynamic::identity_options(&request.backend, &resolved.options)
                .unwrap();
        assert_eq!(canonical["tags"], "netgo,sqlite");
        assert_eq!(canonical["env"], "CGO_ENABLED=0;GOAMD64=v3");
        assert!(canonical.keys().all(|key| !key.starts_with("__osdk_")));
    }

    #[test]
    fn compatibility_requests_are_explicitly_bound_to_isolated_scope() {
        let mut requests = vec![
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolRequest::parse("node@20.10.0").unwrap(),
        ];
        mark_isolated_npm_scope(&mut requests);

        assert_eq!(
            requests[0].options[osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION],
            osdk_core::npm_tools::ToolScope::Project.as_str()
        );
        assert!(!requests[1]
            .options
            .contains_key(osdk_core::npm_tools::LOCKED_NPM_SCOPE_OPTION));
    }

    #[test]
    fn dynamic_request_options_are_bound_to_resolved_version() {
        let request = ToolRequest {
            backend: "github:example/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([(
                "rename".into(),
                "configured-name".into(),
            )]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved
            .options
            .insert("rename".into(), "backend-name".into());
        resolved.options.insert(
            "__osdk_artifact_url".into(),
            "https://example.invalid/tool".into(),
        );

        bind_dynamic_request_options(&request, &mut resolved);

        assert_eq!(resolved.options["rename"], "configured-name");
        assert_eq!(
            resolved.options["__osdk_artifact_url"],
            "https://example.invalid/tool"
        );
    }

    #[test]
    fn dynamic_request_cannot_overwrite_backend_locked_metadata() {
        let request = ToolRequest {
            backend: "go:example.com/acme/tool".into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([(
                osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
                "https://attacker.example".into(),
            )]),
        };
        let mut resolved = ToolVersion::new(&request.backend, "1.2.3");
        resolved.options.insert(
            osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
            "https://proxy.golang.org".into(),
        );
        bind_dynamic_request_options(&request, &mut resolved);
        assert_eq!(
            resolved.options[osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION],
            "https://proxy.golang.org"
        );
    }

    #[test]
    fn reshim_selects_only_the_configured_dynamic_version() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let user_config = temporary.path().join("config.toml");
        std::fs::write(
            &user_config,
            "[tools]\n\"github:example/tool\" = { version = \"1.2.3\", rename = \"configured\" }\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load(&user_config, &project).unwrap();
        let app = app_with_config(&temporary, config);
        let backend = app.registry.get("github:example/tool").unwrap();
        let request =
            osdk_core::shim::dynamic_request_from_config(&app.ctx, "github:example/tool").unwrap();

        assert!(
            request_selects_installed_version(&app, backend.as_ref(), &request, "1.2.3").unwrap()
        );
        assert!(
            !request_selects_installed_version(&app, backend.as_ref(), &request, "1.2.4").unwrap()
        );
        assert_eq!(request.options["rename"], "configured");
    }

    #[test]
    fn exact_reshim_selection_does_not_require_a_validated_installed_listing() {
        let spec = VersionSpec::Exact("1.2.3".into());
        let selected =
            request_selects_version_from_candidates("github:example/tool", &spec, "1.2.3", || {
                anyhow::bail!("inventory listing rejected legacy identity")
            })
            .unwrap();

        assert!(selected);
    }

    fn write_dynamic_install(
        app: &App,
        backend: &str,
        version: &str,
        bin_name: &str,
        options: &std::collections::BTreeMap<String, String>,
    ) {
        let checksum = format!("sha256:{}", "a".repeat(64));
        let materials = if backend.starts_with("github:") {
            std::collections::BTreeMap::from([
                ("artifact-file".into(), "fixture.bin".into()),
                ("artifact-checksum".into(), checksum.clone()),
            ])
        } else {
            std::collections::BTreeMap::new()
        };
        let identity = osdk_core::tool::InstallIdentity::new(
            backend,
            version,
            app.ctx.platform.to_string(),
            osdk_core::tool::InstallScope::Isolated,
            options,
            Vec::new(),
            materials,
        )
        .unwrap();
        let root = osdk_core::dirs::InstallLocator::new(&app.ctx.dirs, identity.clone())
            .unwrap()
            .install_root()
            .to_path_buf();
        let bin = root.join("bin").join(bin_name);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        if backend.starts_with("github:") {
            let receipt = osdk_core::pipeline::ArtifactReceipt {
                url: "https://example.test/fixture.bin".into(),
                file_name: "fixture.bin".into(),
                checksum: Some(checksum),
                evidence: Vec::new(),
            };
            std::fs::write(
                root.join(".osdk-artifact.json"),
                serde_json::to_vec_pretty(&receipt).unwrap(),
            )
            .unwrap();
        }
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
            ..Default::default()
        }];
        manifest.write_atomic(&root).unwrap();
    }

    #[test]
    fn managed_dynamic_paths_require_matching_request_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        let backend = app.registry.get("github:example/tool").unwrap();
        let installed_options =
            std::collections::BTreeMap::from([("rename".into(), "installed".into())]);
        write_dynamic_install(&app, backend.id(), "1.2.3", "installed", &installed_options);
        let version = ToolVersion::new(backend.id(), "1.2.3");
        let request = ToolRequest {
            backend: backend.id().into(),
            spec: VersionSpec::Exact("1.2.3".into()),
            options: std::collections::BTreeMap::from([("rename".into(), "configured".into())]),
        };

        let error =
            managed_bin_paths(&app.ctx, backend.as_ref(), &version, Some(&request)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no complete install matching its unlocked request"),
            "{error}"
        );
    }

    #[test]
    fn unconfigured_dynamic_inventory_is_not_a_runtime_shim_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config =
            osdk_core::config::Config::load(&temporary.path().join("config.toml"), &project)
                .unwrap();
        let app = app_with_config(&temporary, config);
        write_dynamic_install(
            &app,
            "github:example/tool",
            "1.2.3",
            "example-tool",
            &std::collections::BTreeMap::new(),
        );

        let owners = installed_shim_owners(&app).unwrap();

        assert!(!owners.contains_key("example-tool"));
    }

    #[test]
    fn lock_tuple_uses_the_same_exact_node_as_graph_generation() {
        let mut npm = ToolVersion::new("npm:prettier", "3.6.2");
        npm.options.insert(
            osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "24.0.0".into(),
        );
        let mut resolved = vec![
            (ToolRequest::parse("npm:prettier@3.6.2").unwrap(), npm),
            (
                ToolRequest::parse("node@20.10.0").unwrap(),
                ToolVersion::new("node", "20.10.0"),
            ),
        ];

        bind_resolved_node_version(&mut resolved);

        assert_eq!(
            resolved[0].1.options[osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION],
            "20.10.0"
        );
    }

    #[test]
    fn stale_shim_cleanup_preserves_names_owned_by_another_backend() {
        let owners = std::collections::BTreeMap::from([
            (
                "shared".to_string(),
                std::collections::BTreeSet::from(["npm:old".to_string(), "npm:other".to_string()]),
            ),
            (
                "only-old".to_string(),
                std::collections::BTreeSet::from(["npm:old".to_string()]),
            ),
        ]);
        assert!(has_other_shim_owner(&owners, "shared", "npm:old"));
        assert!(!has_other_shim_owner(&owners, "only-old", "npm:old"));
    }

    #[test]
    fn installed_version_selection_is_scope_candidate_bounded() {
        let global = vec!["2.0.0".to_string(), "3.0.0".to_string()];
        assert_eq!(
            select_installed_version(
                "npm:fixture",
                &VersionSpec::Prefix("3".into()),
                global.clone(),
            )
            .unwrap(),
            "3.0.0"
        );
        assert!(select_installed_version(
            "npm:fixture",
            &VersionSpec::Exact("1.0.0".into()),
            global,
        )
        .is_err());
    }

    #[test]
    fn global_snapshot_restores_files_symlinks_and_absence() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("config.toml");
        let absent = temporary.path().join("new-shim");
        std::fs::write(&file, b"old").unwrap();
        let file_snapshot = GlobalNpmPathSnapshot::capture(file.clone()).unwrap();
        let absent_snapshot = GlobalNpmPathSnapshot::capture(absent.clone()).unwrap();

        std::fs::write(&file, b"new").unwrap();
        std::fs::write(&absent, b"generated").unwrap();
        file_snapshot.restore().unwrap();
        absent_snapshot.restore().unwrap();

        assert_eq!(std::fs::read(file).unwrap(), b"old");
        assert!(!absent.exists());
    }

    fn write_global_npm_uninstall_fixture(
        app: &App,
        version: &mut ToolVersion,
        bin_name: &str,
    ) -> std::path::PathBuf {
        let backend =
            osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend).unwrap();
        version
            .options
            .entry(osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into())
            .or_insert_with(|| "22.1.0".into());
        let root = backend.global_install_root_for(&app.ctx, version).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin").join(bin_name), b"fixture").unwrap();
        let identity = backend
            .install_identity(&app.ctx, version, osdk_core::npm_tools::ToolScope::Global)
            .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: bin_name.into(),
            path: format!("bin/{bin_name}"),
            ..Default::default()
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }

    #[test]
    fn interrupted_global_npm_uninstall_before_commit_restores_install() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let journal_path = transaction.path.clone();
        let backup = transaction.journal.roots[0].backup.clone();
        std::mem::forget(transaction);
        std::fs::rename(&root, &backup).unwrap();

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(root.is_dir());
        assert!(!backup.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert_eq!(config.global_tools["npm:fixture-cli"], "1.2.3");
    }

    #[test]
    fn committed_global_npm_uninstall_recovery_preserves_newer_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let journal_path = transaction.path.clone();
        let backup = transaction.journal.roots[0].backup.clone();
        std::fs::rename(&root, &backup).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);
        crate::config_edit::set_global_tool_unlocked(&app.ctx, &version.backend, "2.0.0").unwrap();

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(!root.exists());
        assert!(!backup.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert_eq!(config.global_tools["npm:fixture-cli"], "2.0.0");
    }

    #[test]
    fn global_npm_uninstall_journal_rejects_unsafe_backup_path() {
        let temporary = tempfile::tempdir().unwrap();
        let config =
            osdk_core::config::Config::load_user(&temporary.path().join("config/config.toml"))
                .unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        version.options.insert(
            osdk_core::backend::npm_package::LOCKED_NPM_NODE_VERSION_OPTION.into(),
            "22.1.0".into(),
        );
        let backend =
            osdk_core::backend::npm_package::NpmPackageBackend::from_id(&version.backend).unwrap();
        let path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);
        let journal = GlobalNpmUninstallJournal {
            backend: version.backend.clone(),
            version: version.version.clone(),
            options: version.options.clone(),
            roots: vec![GlobalNpmUninstallRoot {
                original: backend.global_install_root_for(&app.ctx, &version).unwrap(),
                backup: temporary.path().join("outside"),
            }],
            bin_names: vec!["fixture-cli".into()],
            config_entry: None,
            lock_entry: None,
            committed: false,
        };
        write_global_npm_uninstall_journal(&path, &journal).unwrap();

        let error = recover_interrupted_global_npm_uninstalls(&app).unwrap_err();

        assert!(
            error.to_string().contains("unsafe backup path"),
            "{error:#}"
        );
        assert!(path.exists());
    }

    #[test]
    fn committed_global_npm_uninstall_recovery_removes_matching_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[tools]\nnode = \"20.0.0\"\n\"npm:fixture-cli\" = \"1.2.3\"\n",
        )
        .unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let mut transaction = GlobalNpmUninstallTransaction::prepare(
            &app,
            &version,
            std::slice::from_ref(&root),
            &bins,
            true,
            false,
        )
        .unwrap();
        let backup = transaction.journal.roots[0].backup.clone();
        std::fs::rename(&root, &backup).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert!(!config.global_tools.contains_key("npm:fixture-cli"));
        assert_eq!(config.global_tools["node"], "20.0.0");
        assert!(!root.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn panicking_after_global_npm_uninstall_commit_leaves_recoverable_journal() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tools]\n\"npm:fixture-cli\" = \"1.2.3\"\n").unwrap();
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        let app = app_with_config(&temporary, config);
        let mut version = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let root = write_global_npm_uninstall_fixture(&app, &mut version, "fixture-cli");
        let bins = std::collections::BTreeSet::from(["fixture-cli".to_string()]);
        let journal_path = global_npm_uninstall_journal_path(&app.ctx.dirs, &version);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut transaction = GlobalNpmUninstallTransaction::prepare(
                &app,
                &version,
                std::slice::from_ref(&root),
                &bins,
                true,
                false,
            )
            .unwrap();
            let backup = transaction.journal.roots[0].backup.clone();
            std::fs::rename(&root, backup).unwrap();
            transaction.mark_committed().unwrap();
            panic!("simulated abrupt termination");
        }));
        assert!(panic.is_err());
        assert!(!root.exists());
        assert!(journal_path.exists());

        recover_interrupted_global_npm_uninstalls(&app).unwrap();

        assert!(!root.exists());
        assert!(!journal_path.exists());
        let config = osdk_core::config::Config::load_user(&config_path).unwrap();
        assert!(!config.global_tools.contains_key("npm:fixture-cli"));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A shell-split operand is reported as such, and nothing else is.
    ///
    /// The three cases must stay distinguishable. PowerShell splits an unquoted
    /// comma inside one argument, so the fragments arrive as separate operands and
    /// the first fails as "unterminated option block" -- true of the fragment, but
    /// it sends the user to count brackets that were balanced as typed. A genuine
    /// syntax mistake must keep the original message, or this hint would start
    /// blaming the shell for the user's own typo.
    #[test]
    fn only_a_shell_split_operand_is_reported_as_one() {
        let split = vec![
            "npm:esbuild[installer=pnpm".to_string(),
            "allow_builds=a]@0.21".to_string(),
        ];
        let error =
            report_shell_split_operands(&split).expect_err("a split operand must be reported");
        let message = error.to_string();
        assert!(message.contains("quote"), "{message}");
        // The message has to show the command that would have worked, comma and
        // all, so it can be copied instead of reconstructed.
        assert!(
            message.contains("npm:esbuild[installer=pnpm,allow_builds=a]@0.21"),
            "{message}"
        );

        // A value containing commas splits into more than two fragments; the
        // closing half is not necessarily the next operand.
        let three = vec![
            "npm:esbuild[allow_builds=a".to_string(),
            "b".to_string(),
            "c]@0.21".to_string(),
        ];
        let error = report_shell_split_operands(&three).expect_err("three fragments");
        assert!(
            error
                .to_string()
                .contains("npm:esbuild[allow_builds=a,b,c]@0.21"),
            "{error}"
        );

        // Everything else must pass through untouched.
        for ok in [
            // Balanced, quoted properly: the normal case.
            vec!["npm:esbuild[allow_builds=\"a,b\"]@0.21".to_string()],
            // A real typo: opens and never closes, with no closing fragment.
            vec!["npm:esbuild[installer=pnpm@0.21".to_string()],
            // A stray closing bracket only.
            vec!["npm:esbuild]@0.21".to_string()],
            // Two unrelated tools, both well formed.
            vec!["node@20".to_string(), "npm:prettier@3".to_string()],
            // Two separate broken operands that are not two halves of one.
            vec!["npm:a]@1".to_string(), "npm:b]@2".to_string()],
        ] {
            assert!(
                report_shell_split_operands(&ok).is_ok(),
                "must not be reported as a split: {ok:?}"
            );
        }
    }
}
