//! Exercise command lookup against each backend's real installation contract.

use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use osdk_core::backend::native_tool::{
    NativeToolFamily, NativeToolLifecycle, NativeToolPreparation, NativeToolProvider,
};
use osdk_core::config::{Config, StructuredToolConfig, ToolConfigEntry, ToolConfigValue};
use osdk_core::inventory::{DynamicToolBin, DynamicToolManifest};
use osdk_core::tool::{InstallIdentity, InstallScope};

fn fixture(id: &str, options: BTreeMap<String, String>) -> (tempfile::TempDir, App, ToolVersion) {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.settings.offline = true;
    config.global_tools.insert(id.into(), "1.2.3".into());
    config.global_tool_configs.insert(
        id.into(),
        ToolConfigEntry::Structured(StructuredToolConfig {
            version: "1.2.3".into(),
            lazy: false,
            when: None,
            options: options
                .iter()
                .map(|(k, v)| (k.clone(), ToolConfigValue::String(v.clone())))
                .collect(),
        }),
    );
    config.tools = config.global_tools.clone();
    config.tool_configs = config.global_tool_configs.clone();
    let app = crate::commands::command_flow_tests::app_with_config(&temporary, config);
    let mut version = ToolVersion::new(id, "1.2.3");
    version.options = options;
    (temporary, app, version)
}

fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn executable(path: &Path) {
    write_file(path, b"fixture executable; must never execute");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn publish_manifest(app: &App, identity: InstallIdentity, command: &str) -> PathBuf {
    let root = osdk_core::dirs::InstallLocator::new(&app.ctx.dirs, identity.clone())
        .unwrap()
        .install_root()
        .to_path_buf();
    let directory = if cfg!(windows) && identity.tool.starts_with("pypi:") {
        "Scripts"
    } else {
        "bin"
    };
    let relative = format!("{directory}/{command}{}", app.ctx.platform.os.exe_suffix());
    executable(&root.join(&relative));
    let mut manifest = DynamicToolManifest::from_identity(identity).unwrap();
    manifest.bins.push(DynamicToolBin {
        name: command.into(),
        path: relative,
        ..Default::default()
    });
    manifest.write_atomic(&root).unwrap();
    write_file(&root.join(".osdk-complete"), b"");
    root
}

fn assert_command(app: &App, id: &str, command: &str) {
    let requests = global_upgrade_requests(app, &[command.into()])
        .unwrap_or_else(|error| panic!("{id} command {command}: {error:#}"));
    assert_eq!(requests.len(), 1, "{id}: {command}");
    assert_eq!(requests[0].backend, id, "{command}");
    assert_eq!(requests[0].spec, VersionSpec::Exact("1.2.3".into()));
}

#[test]
fn global_upgrade_command_pypi_requires_its_venv_interpreter() {
    let id = "pypi:fixture-package";
    let (_temporary, app, version) = fixture(id, BTreeMap::new());
    let identity = app
        .registry
        .get(id)
        .unwrap()
        .dynamic_install_identity(&app.ctx, &version)
        .unwrap()
        .unwrap();
    let root = publish_manifest(&app, identity, "fixture-python-command");
    let interpreter = osdk_core::backend::pypi::venv_python(&root);
    executable(&interpreter);
    assert_command(&app, id, "fixture-python-command");
    std::fs::remove_file(interpreter).unwrap();
    app.invalidate_dynamic_scan();
    assert!(global_upgrade_requests(&app, &["fixture-python-command".into()]).is_err());
}

#[test]
fn global_upgrade_command_conda_requires_its_artifact_receipt() {
    let id = "conda:fixture-package";
    let (_temporary, app, _) = fixture(id, BTreeMap::new());
    let root = conda_command(&app, id, "fixture-conda-command");
    assert_command(&app, id, "fixture-conda-command");
    std::fs::remove_file(root.join(".osdk-artifact.json")).unwrap();
    app.invalidate_dynamic_scan();
    assert!(global_upgrade_requests(&app, &["fixture-conda-command".into()]).is_err());
}

fn conda_command(app: &App, id: &str, command: &str) -> PathBuf {
    let checksum = format!("sha256:{}", "a".repeat(64));
    let identity = InstallIdentity::new(
        id,
        "1.2.3",
        app.ctx.platform.to_string(),
        InstallScope::Isolated,
        &BTreeMap::new(),
        Vec::new(),
        BTreeMap::from([
            ("artifact-file".into(), "fixture.conda".into()),
            ("artifact-checksum".into(), checksum.clone()),
        ]),
    )
    .unwrap();
    let root = publish_manifest(app, identity, command);
    let receipt = osdk_core::pipeline::ArtifactReceipt {
        url: "https://example.test/fixture.conda".into(),
        file_name: "fixture.conda".into(),
        checksum: Some(checksum),
        evidence: Vec::new(),
    };
    write_file(
        &root.join(".osdk-artifact.json"),
        &serde_json::to_vec(&receipt).unwrap(),
    );
    root
}

#[tokio::test]
async fn global_upgrade_command_http_requires_its_bound_url_and_checksum() {
    let id = "http:https://example.test/tool-{version}";
    let digest = osdk_core::pipeline::verify::hash_bytes(
        b"fixture executable; must never execute",
        osdk_core::pipeline::HashAlgo::Sha256,
    );
    let options = BTreeMap::from([
        ("sha256".into(), digest.clone()),
        ("kind".into(), "file".into()),
        ("rename".into(), "fixture-http-command".into()),
    ]);
    let (_temporary, mut app, version) = fixture(id, options);
    let locator = osdk_core::backend::http::HttpBackend::install_locator_for(
        &app.ctx.dirs,
        app.ctx.platform,
        id,
        &version,
    )
    .unwrap();
    let root = publish_manifest(&app, locator.identity().clone(), "fixture-http-command");
    let mut receipt = osdk_core::pipeline::ArtifactReceipt {
        url: "https://example.test/tool-1.2.3".into(),
        file_name: "tool-1.2.3".into(),
        checksum: Some(format!("sha256:{digest}")),
        evidence: Vec::new(),
    };
    write_file(
        &root.join(".osdk-artifact.json"),
        &serde_json::to_vec(&receipt).unwrap(),
    );
    assert_command(&app, id, "fixture-http-command");
    assert_command(&app, id, id);
    let config = toml::to_string(&BTreeMap::from([(
        "tools",
        app.ctx.config.global_tool_configs.clone(),
    )]))
    .unwrap();
    write_file(&app.ctx.dirs.user_config_file(), config.as_bytes());
    app.global_only = true;
    upgrade_global(&mut app, vec!["fixture-http-command".into()])
        .await
        .unwrap();
    let lock = crate::lockfile::load(&app.ctx.dirs.user_lock_file()).unwrap();
    assert_eq!(
        lock.platforms[&app.ctx.platform.to_string()].tools[id].version,
        "1.2.3"
    );
    assert_eq!(
        std::fs::read_to_string(app.ctx.dirs.user_config_file()).unwrap(),
        config
    );
    // Required HTTP options also belong to an indirect user selection, and
    // must be bound before the alias's version expression is parsed.
    let entry = app.ctx.config.global_tool_configs.remove(id).unwrap();
    let ToolConfigEntry::Structured(mut entry) = entry else {
        unreachable!()
    };
    entry.version = format!("{id}@1.2.3");
    app.ctx
        .config
        .global_tool_configs
        .insert("http-tool".into(), ToolConfigEntry::Structured(entry));
    app.ctx.config.global_tools.remove(id);
    app.ctx
        .config
        .global_tools
        .insert("http-tool".into(), format!("{id}@1.2.3"));
    app.ctx.config.tools = app.ctx.config.global_tools.clone();
    app.ctx.config.tool_configs = app.ctx.config.global_tool_configs.clone();
    assert_command(&app, id, "http-tool");
    assert_command(&app, id, "fixture-http-command");
    receipt.url = "https://example.test/a-different-artifact".into();
    write_file(
        &root.join(".osdk-artifact.json"),
        &serde_json::to_vec(&receipt).unwrap(),
    );
    app.invalidate_dynamic_scan();
    assert!(global_upgrade_requests(&app, &["fixture-http-command".into()]).is_err());
}

#[test]
fn global_upgrade_command_rejects_ambiguity_across_backend_namespaces() {
    let pypi = "pypi:fixture-package";
    let conda = "conda:fixture-package";
    let (_temporary, mut app, version) = fixture(pypi, BTreeMap::new());
    let entry = app.ctx.config.global_tool_configs[pypi].clone();
    app.ctx
        .config
        .global_tool_configs
        .insert(conda.into(), entry);
    app.ctx
        .config
        .global_tools
        .insert(conda.into(), "1.2.3".into());
    app.ctx.config.tool_configs = app.ctx.config.global_tool_configs.clone();
    app.ctx.config.tools = app.ctx.config.global_tools.clone();
    let identity = app
        .registry
        .get(pypi)
        .unwrap()
        .dynamic_install_identity(&app.ctx, &version)
        .unwrap()
        .unwrap();
    let root = publish_manifest(&app, identity, "shared-command");
    executable(&osdk_core::backend::pypi::venv_python(&root));
    conda_command(&app, conda, "shared-command");
    let error = global_upgrade_requests(&app, &["shared-command".into()])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("ambiguous") && error.contains(pypi) && error.contains(conda),
        "{error}"
    );
    assert_command(&app, pypi, pypi);
    assert_command(&app, conda, conda);
}

fn managed_runtime(app: &App, family: NativeToolFamily) {
    let suffix = app.ctx.platform.os.exe_suffix();
    let marker = app.ctx.dirs.install_path(family.runtime(), "1.2.3");
    write_file(&marker.join(".osdk-complete"), b"");
    match family {
        NativeToolFamily::Cargo => {
            let root = app.ctx.dirs.rustup_home().join("toolchains/1.2.3");
            for name in ["cargo", "rustc"] {
                executable(&root.join(format!("bin/{name}{suffix}")));
            }
            write_file(
                &root.join(format!(
                    "lib/rustlib/{}/lib/libstd-fixture.rlib",
                    app.ctx.platform.llvm_triple()
                )),
                b"std",
            );
            write_file(&root.join("lib/librustc_driver-fixture.so"), b"driver");
            for (component, path) in [
                ("rustc", format!("bin/rustc{suffix}")),
                ("cargo", format!("bin/cargo{suffix}")),
                ("rust-std", "libstd-fixture.rlib".into()),
            ] {
                write_file(
                    &root.join(format!("lib/rustlib/manifest-{component}-fixture")),
                    format!("file:{path}").as_bytes(),
                );
            }
        }
        NativeToolFamily::Go => {
            for name in ["go", "gofmt"] {
                executable(&marker.join(format!("bin/{name}{suffix}")));
            }
            executable(&marker.join(format!("pkg/tool/compile{suffix}")));
            write_file(&marker.join("src/runtime/runtime.go"), b"package runtime");
            write_file(&marker.join("VERSION"), b"go1.2.3\n");
            write_file(&marker.join("go.env"), b"GOTOOLCHAIN=local\n");
        }
    }
}

#[tokio::test]
async fn global_upgrade_command_cargo_and_go_require_sealed_installs_and_managed_runtimes() {
    for (id, family, provider) in [
        (
            "cargo:fixture-package",
            NativeToolFamily::Cargo,
            NativeToolProvider::CargoInstall,
        ),
        (
            "go:example.com/acme/tool/cmd/fixture",
            NativeToolFamily::Go,
            NativeToolProvider::GoInstall,
        ),
    ] {
        let (_temporary, app, mut version) = fixture(id, BTreeMap::new());
        managed_runtime(&app, family);
        version.options.extend(BTreeMap::from([
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), family.runtime().into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), "1.2.3".into()),
        ]));
        let (resolution_file, resolution) = match family {
            NativeToolFamily::Cargo => {
                version.options.insert(
                    osdk_core::backend::cargo_package::LOCKED_CARGO_INDEX_OPTION.into(),
                    "sparse+https://index.crates.io/".into(),
                );
                (
                    "cargo-resolution.json",
                    serde_json::json!({"schema":1,"backend":id,"version":"1.2.3","source_kind":"registry","source":"sparse+https://index.crates.io/","replay":"version-only"}),
                )
            }
            NativeToolFamily::Go => (
                "go-resolution.json",
                serde_json::json!({"schema":1,"backend":id,"version":"1.2.3","proxy":"https://proxy.golang.org","module_root":"example.com/acme/tool/cmd/fixture","replay":"version-only"}),
            ),
        };
        let identity = app
            .registry
            .get(id)
            .unwrap()
            .dynamic_install_identity(&app.ctx, &version)
            .unwrap()
            .unwrap();
        let lifecycle =
            NativeToolLifecycle::from_identity(&app.ctx.dirs, family, identity).unwrap();
        let NativeToolPreparation::Staged(stage) = lifecycle.prepare(&app.ctx.dirs).await.unwrap()
        else {
            panic!("fresh fixture must stage");
        };
        executable(&stage.bin_dir().join(format!(
            "fixture-native-command{}",
            app.ctx.platform.os.exe_suffix()
        )));
        write_file(
            &stage.path().join(resolution_file),
            &serde_json::to_vec(&resolution).unwrap(),
        );
        let root = stage.publish(provider).unwrap();
        assert_command(&app, id, "fixture-native-command");
        let runtime_marker = app
            .ctx
            .dirs
            .install_path(family.runtime(), "1.2.3")
            .join(".osdk-complete");
        std::fs::remove_file(&runtime_marker).unwrap();
        app.invalidate_dynamic_scan();
        assert!(
            global_upgrade_requests(&app, &["fixture-native-command".into()]).is_err(),
            "{id}"
        );
        write_file(&runtime_marker, b"");
        app.invalidate_dynamic_scan();
        assert_command(&app, id, "fixture-native-command");
        // The lookup must still reject changed native bytes, even though its
        // inventory continues to claim the same command and tool identity.
        let bin = root.join(format!(
            "bin/fixture-native-command{}",
            app.ctx.platform.os.exe_suffix()
        ));
        write_file(&bin, b"tampered native command");
        app.invalidate_dynamic_scan();
        assert!(
            global_upgrade_requests(&app, &["fixture-native-command".into()]).is_err(),
            "{id}"
        );
    }
}

#[test]
fn global_upgrade_command_every_registered_static_backend_and_declarative_backends() {
    let ids = osdk_core::Registry::new()
        .ids()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    for id in ids.into_iter().chain(["acme".into()]) {
        let (_temporary, mut app, version) = fixture(&id, BTreeMap::new());
        if id == "acme" {
            let definition =
                include_str!("../../../osdk-core/tests/fixtures/declarative/static-backend.toml");
            write_file(
                &app.ctx.dirs.config.join("plugins/acme.toml"),
                definition.as_bytes(),
            );
            app.registry = osdk_core::Registry::load(&app.ctx.dirs).unwrap();
        }
        let backend = app.registry.get(&id).unwrap();
        write_file(
            &app.ctx
                .dirs
                .install_path(&id, &version.version)
                .join(".osdk-complete"),
            b"",
        );
        let paths = backend.bin_paths(&app.ctx, &version).unwrap();
        let names = backend.bin_names(&app.ctx, &version).unwrap();
        if let Some(bin) = paths.first() {
            for name in names
                .iter()
                .map(String::as_str)
                .chain(["fixture-extra-command"])
            {
                executable(&bin.join(format!("{name}{}", app.ctx.platform.os.exe_suffix())));
            }
        }
        let names = backend.bin_names(&app.ctx, &version).unwrap();
        if names.is_empty() {
            assert!(
                paths.is_empty(),
                "{id} should publish its installed commands"
            );
        }
        for name in names {
            if app.registry.get(&name).is_ok_and(|owner| owner.id() != id) {
                // A reserved tool name still denotes its explicit backend.
                assert!(global_upgrade_requests(&app, &[name]).is_err());
            } else {
                assert_command(&app, &id, &name);
            }
        }
        assert_command(&app, &id, &id);
    }
}
