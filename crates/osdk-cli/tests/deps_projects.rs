use std::path::{Path, PathBuf};
use std::process::{Command, Output};
fn run(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(args)
        .current_dir(cwd)
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_INSTALL_DIR", root.join("state/installs"))
        .env("OSDK_STORE_DIR", root.join("state/store"))
        .output()
        .unwrap()
}
fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn native_test_cargo() -> PathBuf {
    let cargo = std::env::var_os("OSDK_TEST_CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO")));
    assert!(
        cargo.is_absolute() && cargo.is_file() && matching_rustc(&cargo).is_file(),
        "set OSDK_TEST_CARGO to the absolute path of a native Cargo next to rustc: {}",
        cargo.display()
    );
    cargo
}

fn matching_rustc(cargo: &Path) -> PathBuf {
    cargo.with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX))
}

struct CargoWorkspace {
    temp: tempfile::TempDir,
    cargo: PathBuf,
}

impl CargoWorkspace {
    fn new(cargo: PathBuf) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("osdk Cargo 工作区 ")
            .tempdir()
            .unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("state/home")).unwrap();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = ['crates/demo', 'crates/other']\nresolver = '3'\n",
        );
        write(
            root,
            "osdk.toml",
            "[tools]\nrust = '=trial-rust'\n[deps]\nroots = ['crates/demo']\n[deps.cargo]\n",
        );
        write(root, "crates/demo/src/lib.rs", "");
        write(root, "crates/other/src/lib.rs", "");
        write(
            root,
            "crates/demo/Cargo.toml",
            "[package]\nname = 'demo'\nversion = '0.1.0'\nedition = '2024'\n",
        );
        write(
            root,
            "crates/other/Cargo.toml",
            "[package]\nname = 'other'\nversion = '0.1.0'\nedition = '2024'\n",
        );
        Self { temp, cargo }
    }

    fn command(&self, program: &Path) -> Command {
        let root = self.temp.path();
        let mut command = Command::new(program);
        command
            .current_dir(root)
            .env("HOME", root.join("state/home"))
            .env("USERPROFILE", root.join("state/home"))
            .env("CARGO_HOME", root.join("state/cargo"))
            .env("RUSTUP_HOME", root.join("state/rustup"))
            .env("RUSTC", matching_rustc(&self.cargo))
            .env("CARGO_TARGET_DIR", root.join("state/target"))
            .env("CARGO_NET_OFFLINE", "true");
        command
    }

    fn link_rust(&self) {
        let root = self.temp.path();
        let toolchain = self.cargo.parent().unwrap().parent().unwrap();
        write(root, "state/installs/rust/trial-rust/.osdk-complete", "");
        write(
            root,
            "state/installs/rust/trial-rust/.osdk-linked",
            toolchain.to_str().unwrap(),
        );
    }

    fn native(&self, args: &[&str]) -> Output {
        self.command(&self.cargo).args(args).output().unwrap()
    }

    fn run(&self, provider: &str, extra: &[&str]) -> Output {
        let root = self.temp.path();
        self.command(Path::new(env!("CARGO_BIN_EXE_osdk")))
            .env("OSDK_OFFLINE", "true")
            .env("OSDK_CONFIG_DIR", root.join("state/config"))
            .env("OSDK_DATA_DIR", root.join("state/data"))
            .env("OSDK_CACHE_DIR", root.join("state/cache"))
            .env("OSDK_INSTALL_DIR", root.join("state/installs"))
            .env("OSDK_STORE_DIR", root.join("state/store"))
            .args(["deps", provider, "--frozen", "--no-install-tools"])
            .args(extra)
            .output()
            .unwrap()
    }

    fn member(&self, extra: &[&str]) -> Output {
        self.run("//crates/demo:cargo", extra)
    }

    fn generate_lock(&self) -> Vec<u8> {
        let output = self.native(&["generate-lockfile", "--offline"]);
        assert!(output.status.success(), "{output:?}");
        std::fs::read(self.temp.path().join("Cargo.lock")).unwrap()
    }

    fn assert_locks(&self, expected: Option<&[u8]>) {
        let root = self.temp.path();
        match expected {
            Some(bytes) => assert_eq!(std::fs::read(root.join("Cargo.lock")).unwrap(), bytes),
            None => assert!(!root.join("Cargo.lock").exists()),
        }
        assert!(!root.join("crates/demo/Cargo.lock").exists());
    }
}

#[test]
fn cargo_metadata_unavailable_explains_why_a_member_cannot_freeze() {
    let fixture = CargoWorkspace::new(PathBuf::from(env!("CARGO")));
    let output = fixture.member(&["--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("without an installed or linked Cargo"),
        "{error}"
    );
    assert!(error.contains("osdk install rust"), "{error}");
    fixture.assert_locks(None);
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_workspace_member_frozen_uses_the_native_root_lock() {
    let cargo = native_test_cargo();
    for scenario in ["valid", "missing", "stale", "excluded", "unlisted"] {
        let fixture = CargoWorkspace::new(cargo.clone());
        fixture.link_rust();
        let root = fixture.temp.path();
        if scenario == "excluded" || scenario == "unlisted" {
            let exclude = if scenario == "excluded" {
                "exclude = ['crates/demo']\n"
            } else {
                ""
            };
            write(
                root,
                "Cargo.toml",
                &format!("[workspace]\nmembers = ['crates/other']\nresolver = '3'\n{exclude}"),
            );
        }
        let lock = (scenario != "missing").then(|| fixture.generate_lock());
        if scenario == "stale" {
            let first = fixture.member(&[]);
            assert!(first.status.success(), "{first:?}");
            let unchanged = fixture.member(&[]);
            assert!(unchanged.status.success(), "{unchanged:?}");
            assert!(String::from_utf8_lossy(&unchanged.stdout).contains("is up to date"));
            eprintln!("unchanged member: successful frozen fetch reused the cache");
            write(
                root,
                "crates/other/Cargo.toml",
                "[package]\nname = 'other'\nversion = '0.2.0'\nedition = '2024'\n",
            );
            let native = fixture.native(&[
                "fetch",
                "--locked",
                "--offline",
                "--manifest-path",
                "crates/demo/Cargo.toml",
            ]);
            assert!(!native.status.success(), "{native:?}");
            let stderr = String::from_utf8_lossy(&native.stderr);
            assert!(
                stderr.contains("lock file") && stderr.contains("--locked"),
                "{stderr}"
            );
        }

        let preview = fixture.member(&["--dry-run"]);
        fixture.assert_locks(lock.as_deref());
        let actual = fixture.member(&[]);
        fixture.assert_locks(lock.as_deref());
        let statuses = (preview.status.code(), actual.status.code());
        if scenario == "valid" || scenario == "stale" {
            let expected = "cargo fetch --locked --offline";
            assert!(preview.status.success(), "{scenario}: {preview:?}");
            let stdout = String::from_utf8_lossy(&preview.stdout);
            assert!(
                stdout.contains("would run in") && stdout.contains(expected),
                "{stdout}"
            );
            assert!(
                String::from_utf8_lossy(&actual.stdout).contains(expected),
                "{actual:?}"
            );
            if scenario == "valid" {
                assert!(actual.status.success(), "{actual:?}");
                let explain = fixture.member(&["--list", "--explain"]);
                assert!(explain.status.success(), "{explain:?}");
                let text = String::from_utf8_lossy(&explain.stdout);
                assert!(text.contains("Cargo workspace query: "), "{text}");
                assert!(text.contains("Cargo workspace root: "), "{text}");
                assert!(
                    text.contains("native lock: ") && text.contains("Cargo.lock"),
                    "{text}"
                );
                assert!(text.contains(expected), "{text}");
                let root_plan = fixture.run("//:cargo", &["--dry-run"]);
                assert!(root_plan.status.success(), "{root_plan:?}");
                assert!(
                    String::from_utf8_lossy(&root_plan.stdout).contains(expected),
                    "{root_plan:?}"
                );
            } else {
                assert!(!actual.status.success(), "{actual:?}");
                let stderr = String::from_utf8_lossy(&actual.stderr);
                assert!(
                    stderr.contains("lock file") && stderr.contains("--locked"),
                    "{stderr}"
                );
            }
        } else {
            let reason = match scenario {
                "missing" => "no Cargo.lock in Cargo workspace root ",
                "excluded" => "Cargo uses this directory as its workspace root",
                "unlisted" => "current package believes it's in a workspace when it's not",
                _ => unreachable!(),
            };
            for output in [preview, actual] {
                assert!(!output.status.success(), "{scenario}: {output:?}");
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(stderr.contains(reason), "{scenario}: {stderr}");
                assert!(!String::from_utf8_lossy(&output.stdout).contains("cargo fetch"));
            }
        }
        eprintln!("{scenario}: preview/actual exit {statuses:?}; lock bytes and presence checked");
    }
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_workspace_member_ignores_a_leftover_local_lock() {
    let cargo = native_test_cargo();
    for remove_root_lock in [true, false] {
        let fixture = CargoWorkspace::new(cargo.clone());
        let root = fixture.temp.path();
        let member_lock = root.join("crates/demo/Cargo.lock");
        fixture.link_rust();
        let lock = fixture.generate_lock();
        std::fs::write(&member_lock, &lock).unwrap();
        let first = fixture.member(&[]);
        assert!(first.status.success(), "{first:?}");
        let unchanged = fixture.member(&[]);
        assert!(unchanged.status.success(), "{unchanged:?}");
        assert!(String::from_utf8_lossy(&unchanged.stdout).contains("is up to date"));

        let root_lock = root.join("Cargo.lock");
        let reason = if remove_root_lock {
            std::fs::remove_file(&root_lock).unwrap();
            let preview = fixture.member(&["--dry-run"]);
            assert!(!preview.status.success(), "{preview:?}");
            "no Cargo.lock in Cargo workspace root"
        } else {
            write(
                root,
                "crates/other/Cargo.toml",
                "[package]\nname = 'other'\nversion = '0.2.0'\nedition = '2024'\n",
            );
            "lock file"
        };
        let actual = fixture.member(&[]);
        assert!(!actual.status.success(), "{actual:?}");
        let stderr = String::from_utf8_lossy(&actual.stderr);
        assert!(
            stderr.contains(reason) && stderr.contains("--locked"),
            "{stderr}"
        );
        if remove_root_lock {
            assert!(!root_lock.exists());
        } else {
            assert_eq!(std::fs::read(&root_lock).unwrap(), lock);
        }
        assert_eq!(std::fs::read(&member_lock).unwrap(), lock);
    }
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_requires_native_membership_even_with_a_local_lock() {
    let cargo = native_test_cargo();
    let fixture = CargoWorkspace::new(cargo);
    fixture.link_rust();
    let root = fixture.temp.path();
    let workspace = "[workspace]\nmembers = ['crates/other']\nresolver = '3'\n";
    write(
        root,
        "Cargo.toml",
        &format!("{workspace}exclude = ['crates/demo']\n"),
    );
    let root_lock = fixture.generate_lock();
    let native = fixture.native(&[
        "generate-lockfile",
        "--offline",
        "--manifest-path",
        "crates/demo/Cargo.toml",
    ]);
    assert!(native.status.success(), "{native:?}");
    let member_lock = root.join("crates/demo/Cargo.lock");
    let local = std::fs::read(&member_lock).unwrap();
    let excluded = fixture.member(&[]);
    assert!(excluded.status.success(), "{excluded:?}");
    assert!(String::from_utf8_lossy(&excluded.stdout).contains("cargo fetch --locked"));

    write(root, "Cargo.toml", workspace);
    for extra in [&[][..], &["--dry-run"][..]] {
        let unlisted = fixture.member(extra);
        assert!(!unlisted.status.success(), "{unlisted:?}");
        let stderr = String::from_utf8_lossy(&unlisted.stderr);
        assert!(
            stderr.contains("current package believes it's in a workspace"),
            "{stderr}"
        );
    }
    assert_eq!(std::fs::read(root.join("Cargo.lock")).unwrap(), root_lock);
    assert_eq!(std::fs::read(member_lock).unwrap(), local);
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_workspace_frozen_tracks_the_effective_lock_path() {
    let cargo = native_test_cargo();
    for source in ["provider environment", "nested Cargo config"] {
        let fixture = CargoWorkspace::new(cargo.clone());
        fixture.link_rust();
        let root = fixture.temp.path();
        let lock = fixture.generate_lock();
        let configured = root.join("alternate/Cargo.lock");
        std::fs::create_dir_all(configured.parent().unwrap()).unwrap();
        std::fs::write(&configured, &lock).unwrap();
        if source == "provider environment" {
            let config = root.join("osdk.toml");
            let mut text = std::fs::read_to_string(&config).unwrap();
            text.push_str(
                "[deps.cargo.env]\nCARGO_RESOLVER_LOCKFILE_PATH = '../../alternate/Cargo.lock'\n",
            );
            std::fs::write(config, text).unwrap();
        } else {
            write(
                root,
                ".cargo/config.toml",
                "include = ['rules/lock.toml']\n",
            );
            write(
                root,
                ".cargo/rules/lock.toml",
                "include = ['../resolver.toml']\n",
            );
            write(
                root,
                ".cargo/resolver.toml",
                "[resolver]\nlockfile-path = 'alternate/Cargo.lock'\n",
            );
            let native = fixture.native(&[
                "fetch",
                "--locked",
                "--offline",
                "--manifest-path",
                "crates/demo/Cargo.toml",
            ]);
            assert!(native.status.success(), "{native:?}");
        }

        let first = fixture.member(&[]);
        assert!(first.status.success(), "{source}: {first:?}");
        let unchanged = fixture.member(&[]);
        assert!(unchanged.status.success(), "{source}: {unchanged:?}");
        assert!(String::from_utf8_lossy(&unchanged.stdout).contains("is up to date"));
        let stale = String::from_utf8(lock.clone())
            .unwrap()
            .replace("version = \"0.1.0\"", "version = \"0.9.0\"");
        std::fs::write(&configured, &stale).unwrap();
        let rejected = fixture.member(&[]);
        assert!(!rejected.status.success(), "{source}: {rejected:?}");
        assert!(String::from_utf8_lossy(&rejected.stdout).contains("cargo fetch --locked"));
        assert_eq!(std::fs::read(&configured).unwrap(), stale.as_bytes());
        std::fs::remove_file(&configured).unwrap();
        for extra in [&[][..], &["--dry-run"][..]] {
            let missing = fixture.member(extra);
            assert!(!missing.status.success(), "{source}: {missing:?}");
            let stderr = String::from_utf8_lossy(&missing.stderr);
            assert!(
                stderr.contains("alternate") && stderr.contains("Cargo.lock"),
                "{stderr}"
            );
            assert!(!configured.exists());
        }
        fixture.assert_locks(Some(&lock));

        std::fs::write(&configured, &lock).unwrap();
        std::fs::remove_file(root.join("Cargo.lock")).unwrap();
        let preview = fixture.member(&["--dry-run", "--force"]);
        assert!(preview.status.success(), "{source}: {preview:?}");
        assert!(String::from_utf8_lossy(&preview.stdout).contains("cargo fetch --locked"));
        let actual = fixture.member(&["--force"]);
        assert!(actual.status.success(), "{source}: {actual:?}");
        assert_eq!(std::fs::read(configured).unwrap(), lock);
        fixture.assert_locks(None);
    }
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_workspace_frozen_tracks_lock_under_provider_home() {
    let fixture = CargoWorkspace::new(native_test_cargo());
    fixture.link_rust();
    let root = fixture.temp.path();
    let lock = fixture.generate_lock();
    let home = root.join("provider-home");
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let config = root.join("osdk.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let home_value = toml::Value::String(home.to_str().unwrap().into());
    std::fs::write(
        config,
        format!("{text}[deps.cargo.env]\nCARGO_HOME = ''\n{home_key} = {home_value}\n"),
    )
    .unwrap();
    write(
        root,
        "provider-home/.cargo/config.toml",
        "[resolver]\nlockfile-path = 'alternate/Cargo.lock'\n",
    );
    let configured_lock = home.join("alternate/Cargo.lock");
    std::fs::create_dir_all(configured_lock.parent().unwrap()).unwrap();
    std::fs::write(&configured_lock, &lock).unwrap();
    let native = || {
        fixture
            .command(&fixture.cargo)
            .current_dir(root.join("crates/demo"))
            .env("CARGO_HOME", "")
            .env(home_key, &home)
            .args(["fetch", "--locked", "--offline"])
            .output()
            .unwrap()
    };
    let native_first = native();
    assert!(native_first.status.success(), "{native_first:?}");

    let first = fixture.member(&[]);
    assert!(first.status.success(), "{first:?}");
    let unchanged = fixture.member(&[]);
    assert!(unchanged.status.success(), "{unchanged:?}");
    assert!(String::from_utf8_lossy(&unchanged.stdout).contains("is up to date"));
    assert_eq!(std::fs::read(&configured_lock).unwrap(), lock);
    std::fs::remove_file(&configured_lock).unwrap();
    let native_missing = native();
    assert!(!native_missing.status.success(), "{native_missing:?}");
    assert!(String::from_utf8_lossy(&native_missing.stderr).contains("alternate"));

    let missing = fixture.member(&[]);
    assert!(!missing.status.success(), "{missing:?}");
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(
        error.contains("provider-home") && error.contains("alternate"),
        "{error}"
    );
    assert!(!configured_lock.exists());
    fixture.assert_locks(Some(&lock));
}

#[test]
#[cfg_attr(
    all(windows, target_env = "gnu"),
    ignore = "run with Windows Cargo and rustc via OSDK_TEST_CARGO"
)]
fn cargo_workspace_frozen_rechecks_nonmember_path_dependencies() {
    let cargo = native_test_cargo();
    let fixture = CargoWorkspace::new(cargo);
    fixture.link_rust();
    let root = fixture.temp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = ['crates/demo', 'crates/other']\nexclude = ['vendor/helper']\nresolver = '3'\n",
    );
    write(
        root,
        "crates/demo/Cargo.toml",
        "[package]\nname = 'demo'\nversion = '0.1.0'\nedition = '2024'\n[dependencies]\nhelper = { path = '../../vendor/helper' }\n",
    );
    write(root, "vendor/helper/src/lib.rs", "");
    let helper = |version: &str| {
        write(
            root,
            "vendor/helper/Cargo.toml",
            &format!("[package]\nname = 'helper'\nversion = '{version}'\nedition = '2024'\n"),
        );
    };
    helper("0.1.0");
    let lock = fixture.generate_lock();
    let first = fixture.member(&[]);
    assert!(first.status.success(), "{first:?}");
    helper("0.2.0");

    let native = fixture.native(&[
        "fetch",
        "--locked",
        "--offline",
        "--manifest-path",
        "crates/demo/Cargo.toml",
    ]);
    assert!(!native.status.success(), "{native:?}");
    let actual = fixture.member(&[]);
    assert!(!actual.status.success(), "{actual:?}");
    assert!(String::from_utf8_lossy(&actual.stdout).contains("cargo fetch --locked"));
    let stderr = String::from_utf8_lossy(&actual.stderr);
    assert!(
        stderr.contains("lock file") && stderr.contains("--locked"),
        "{stderr}"
    );
    fixture.assert_locks(Some(&lock));
}

#[test]
fn removed_installer_is_rejected_in_root_project_and_inherited_configuration() {
    for config in [
        "[deps.npm]\ninstaller='npm'\n",
        "[deps.projects.'apps/web'.npm]\ninstaller='npm'\n",
        "[deps]\nroots=['apps/*']\n[deps.npm]\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "package.json", "{}");
        write(root, "apps/web/package.json", "{}");
        write(root, "osdk.toml", config);
        let inherited = !config.contains("installer");
        if inherited {
            write(
                root,
                "apps/web/osdk.toml",
                "extends='../../osdk.toml'\n[deps.npm]\ninstaller='npm'\n",
            );
        }
        for cwd in [root.to_path_buf(), root.join("apps/web")] {
            let output = run(root, &cwd, &["deps", "--list", "--all"]);
            assert!(!output.status.success(), "{output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("unknown field `installer`"), "{error}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains("would run in"));
        }
    }
}
#[test]
fn filters_route_and_deduplicate_root_and_children_from_either_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "package.json", r#"{"packageManager":"npm@11.0.0"}"#);
    write(
        root,
        "apps/api/package.json",
        r#"{"packageManager":"pnpm@10.0.0"}"#,
    );
    write(
        root,
        "apps/web/package.json",
        r#"{"packageManager":"npm@11.0.0"}"#,
    );
    write(root,"osdk.toml","[deps]\nroots=['apps/*','apps/api']\n[deps.npm]\n[deps.pnpm]\n[deps.projects.'apps/web'.npm]\nauto=false\n");
    for cwd in [root.to_path_buf(), root.join("apps/api")] {
        let output = run(
            root,
            &cwd,
            &["deps", "--list", "--all", "--filter", "apps/*", "--explain"],
        );
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.matches("//apps/api:pnpm").count(), 1, "{text}");
        assert_eq!(text.matches("//apps/web:npm").count(), 1, "{text}");
        assert!(!text.contains("//:npm"), "root must be filtered: {text}");
    }
    let output = run(root, root, &["deps", "--list", "--all", "--filter", "."]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("//:npm"));
    write(root, "apps/api/package-lock.json", "{}");
    assert!(
        !run(root, root, &["deps", "--list", "--filter", "apps/web"])
            .status
            .success()
    );
}
#[test]
fn nested_project_commands_and_indexes_require_trust() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "apps/api/requirements.txt", "");
    write(
        root,
        "osdk.toml",
        "[deps.projects.'apps/api'.pip-requirements]\nrun=['custom','one argument']\n",
    );
    let output = run(root, root, &["deps", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("deps.projects.apps/api.pip-requirements.run"));
}

#[test]
fn workspace_member_selection_expands_to_one_shared_installation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "js/package.json",
        r#"{"name":"root","private":true,"packageManager":"pnpm@10.0.0"}"#,
    );
    write(
        root,
        "js/pnpm-workspace.yaml",
        "packages: ['packages/*', '!packages/excluded']",
    );
    write(root, "js/packages/a/package.json", r#"{"name":"a"}"#);
    write(root, "js/packages/b/package.json", r#"{"name":"b"}"#);
    write(root, "js/packages/excluded/package.json", "broken");
    write(
        root,
        "osdk.toml",
        "[deps.projects.'js'.pnpm]
",
    );
    let output = run(
        root,
        root,
        &["deps", "--dry-run", "--filter", "js/packages/a"],
    );
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("coverage expands"), "{text}");
    assert_eq!(text.matches("would run in").count(), 1, "{text}");
    assert!(text.contains("pnpm install"), "{text}");
    let output = run(root, &root.join("js/packages/a"), &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("would run in")
            .count(),
        1
    );
    write(
        root,
        "osdk.toml",
        "[deps.projects.'js'.pnpm]
[deps.projects.'js/packages/a'.pnpm]
auto=false",
    );
    let output = run(root, root, &["deps", "--dry-run"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("identical provider options"));
}
#[test]
fn a_member_lockfile_kept_for_a_separate_npm_deployment_does_not_block_the_pnpm_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "osdk.toml",
        "[tools]
pnpm = \"11\"

[deps.pnpm]
",
    );
    write(
        root,
        "package.json",
        r#"{"name":"dev-flow","private":true}"#,
    );
    write(
        root,
        "pnpm-workspace.yaml",
        "packages:
  - packages/*
",
    );
    write(
        root,
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'

importers:

  .: {}

  packages/dev-flow: {}

  packages/figma-service: {}
",
    );
    write(
        root,
        "packages/dev-flow/package.json",
        r#"{"name":"dev-flow-app"}"#,
    );
    write(
        root,
        "packages/figma-service/package.json",
        r#"{"name":"figma-service"}"#,
    );
    let npm_lock = r#"{"name":"figma-service","lockfileVersion":3,"packages":{}}"#;
    write(root, "packages/figma-service/package-lock.json", npm_lock);

    for cwd in [root.to_path_buf(), root.join("packages/figma-service")] {
        let output = run(root, &cwd, &["deps", "--dry-run"]);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.matches("would run in").count(), 1, "{text}");
        assert!(
            text.contains("pnpm install --frozen-lockfile --ignore-scripts"),
            "{text}"
        );
        let planned = text
            .lines()
            .find(|line| line.starts_with("would run in"))
            .unwrap();
        assert!(
            !planned.contains("figma-service"),
            "planned at the root: {text}"
        );
    }

    let output = run(root, root, &["deps", "--list", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("note: ")
            && text.contains("package-lock.json belongs to `npm` and is ignored"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("packages/figma-service/package-lock.json")).unwrap(),
        npm_lock
    );

    // Enabling npm for a separate project does not make the members ambiguous:
    // they stay with the shared pnpm installation.
    write(
        root,
        "osdk.toml",
        "[tools]
pnpm = \"11\"

[deps]
roots = [\"tools/deploy\"]

[deps.pnpm]

[deps.npm]
",
    );
    write(root, "tools/deploy/package.json", r#"{"name":"deploy"}"#);
    write(root, "tools/deploy/package-lock.json", npm_lock);
    let output = run(root, root, &["deps", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    let planned: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("would run in"))
        .collect();
    assert_eq!(planned.len(), 2, "{text}");
    assert!(
        planned
            .iter()
            .any(|line| line.contains("pnpm install --frozen-lockfile")
                && !line.contains("packages")),
        "{text}"
    );
    assert!(
        planned
            .iter()
            .any(|line| line.contains("deploy") && line.contains("npm ci")),
        "{text}"
    );
}
#[test]
fn uv_workspace_plan_uses_all_packages_and_records_member_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "py/pyproject.toml",
        "[tool.uv.workspace]
members=['packages/*']",
    );
    write(
        root,
        "py/packages/a/pyproject.toml",
        "[project]
name='a'
version='0.1.0'",
    );
    write(
        root,
        "osdk.toml",
        "[deps.projects.'py'.uv]
",
    );
    let output = run(root, root, &["deps", "--list", "--all", "--explain"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.matches("command:").count(), 1, "{text}");
    assert!(text.contains("--all-packages"), "{text}");
    assert!(text.contains("py/packages/a"), "{text}");
}

#[test]
fn explicit_inheritance_has_same_deps_options_and_commands_from_root_or_child() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "apps/api/package.json",
        r#"{"packageManager":"npm@11.0.0"}"#,
    );
    write(root,"osdk.toml","[tools]\nnode='24.19.0'\n[deps]\nroots=['apps/*']\n[deps.npm]\noutputs=['node_modules']\nenv={A='1',B='1'}\n");
    write(
        root,
        "apps/api/osdk.toml",
        "extends='../../osdk.toml'\n[deps.npm]\nauto=false\ndir='.'\noutputs=[]\nenv={B='2'}\n",
    );
    let mut results = Vec::new();
    for cwd in [root.to_path_buf(), root.join("apps/api")] {
        let output = run(root, &cwd, &["deps", "--list", "--all", "--explain"]);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("auto=false"), "{text}");
        assert!(text.contains("outputs=[]"), "{text}");
        assert!(text.contains("A=[redacted] B=[redacted]"), "{text}");
        results.push(text);
    }
    assert_eq!(results[0], results[1]);
}
#[test]
fn trusting_leaf_does_not_trust_inherited_executable_config() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "osdk.toml",
        "[deps.codegen]\nrun=['a-command-that-must-not-run']",
    );
    write(root, "child/osdk.toml", "extends='../osdk.toml'");
    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(["deps", "--dry-run"])
        .current_dir(root.join("child"))
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("child/osdk.toml"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("deps.codegen.run"), "{error}");
    assert!(
        error.contains(
            // Printed without the `\?\` prefix `canonicalize` adds on Windows.
            dunce::canonicalize(root.join("osdk.toml"))
                .unwrap()
                .to_str()
                .unwrap()
        ),
        "{error}"
    );
}

#[test]
fn inherited_tasks_keep_declaring_file_paths_in_root_and_child_calls() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root,"osdk.toml","[task]\nroots=['child']\n[tasks.shared]\nlua='write(\"marker\", \"done\"); return 0'\ndir='scripts'\n");
    write(
        root,
        "child/osdk.toml",
        "extends='../osdk.toml'\n[tasks.local]\nlua='write(\"marker\", \"done\"); return 0'\ndir='scripts'\n",
    );
    for (cwd, name, expected) in [
        (root.to_path_buf(), "//child:shared", root.join("scripts")),
        (root.join("child"), "shared", root.join("scripts")),
        (
            root.to_path_buf(),
            "//child:local",
            root.join("child/scripts"),
        ),
        (root.join("child"), "local", root.join("child/scripts")),
    ] {
        std::fs::create_dir_all(&expected).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
            .args(["run", "--no-deps", name])
            .current_dir(&cwd)
            .env("OSDK_CONFIG_DIR", root.join("state/config"))
            .env("OSDK_DATA_DIR", root.join("state/data"))
            .env("OSDK_CACHE_DIR", root.join("state/cache"))
            .env("OSDK_STORE_DIR", root.join("state/store"))
            .env("OSDK_INSTALL_DIR", root.join("state/installs"))
            .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("osdk.toml"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(expected.join("marker")).unwrap(),
            "done"
        );
    }
}

#[test]
fn inherited_offline_policy_and_workspace_tool_context_are_consistent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "osdk.toml", "[deps.projects.'api'.npm]\n");
    write(
        root,
        "api/package.json",
        r#"{"packageManager":"npm@11.0.0"}"#,
    );
    write(
        root,
        "api/osdk.toml",
        "extends='../osdk.toml'\n[settings]\noffline=true\n",
    );
    for cwd in [root.to_path_buf(), root.join("api")] {
        let output = run(root, &cwd, &["deps", "--list", "--all", "--explain"]);
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("--offline"),
            "{output:?}"
        );
    }
    write(
        root,
        "osdk.toml",
        "[settings]\noffline=true\n[deps.projects.'api'.npm]\n",
    );
    write(
        root,
        "api/osdk.toml",
        "extends='../osdk.toml'\n[settings]\noffline=false\n",
    );
    let output = run(
        root,
        root,
        &["--offline", "deps", "--list", "--all", "--explain"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--offline"),
        "CLI offline must override even a matching parent flag: {output:?}"
    );
    write(
        root,
        "js/package.json",
        r#"{"packageManager":"pnpm@11.0.0"}"#,
    );
    write(root, "js/pnpm-workspace.yaml", "packages: ['packages/*']");
    write(root, "js/packages/a/package.json", "{}");
    write(
        root,
        "js/packages/a/osdk.toml",
        "extends='../../../osdk.toml'\n[settings]\noffline=true\n",
    );
    write(root, "osdk.toml", "[deps.projects.'js'.pnpm]\n");
    let output = run(root, root, &["deps", "--list", "--all"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("conflicting effective tool settings or sources"),
        "{output:?}"
    );
}

#[cfg(unix)]
#[test]
fn execution_keeps_secrets_but_explain_lock_and_state_do_not_and_failure_is_not_retried() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "input", "manifest");
    write(root, "osdk.toml", "[deps.private]\nsources=['input']\noutputs=['received']\nenv={API_TOKEN='credential-sentinel-9382'}\nrun=['sh','-c','printf %s \"$API_TOKEN\" > received; echo call >> calls']\n");
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_osdk"))
            .args(args)
            .current_dir(root)
            .env("OSDK_CONFIG_DIR", root.join("state/config"))
            .env("OSDK_DATA_DIR", root.join("state/data"))
            .env("OSDK_CACHE_DIR", root.join("state/cache"))
            .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("osdk.toml"))
            .output()
            .unwrap()
    };
    for args in [
        vec!["deps", "--dry-run", "--explain"],
        vec!["deps", "--explain"],
    ] {
        let output = invoke(&args);
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("credential-sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("credential-sentinel"));
    }
    assert_eq!(
        std::fs::read_to_string(root.join("received")).unwrap(),
        "credential-sentinel-9382"
    );
    assert!(!std::fs::read_to_string(root.join("osdk.lock"))
        .unwrap()
        .contains("credential-sentinel"));
    let second = invoke(&["deps"]);
    assert!(second.status.success(), "{second:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("calls")).unwrap(),
        "call\n"
    );
    // A native config edit must invalidate even a custom command's opaque snapshot.
    write(root, ".npmrc", "registry=https://new-source.example/\n");
    let changed = invoke(&["deps"]);
    assert!(changed.status.success(), "{changed:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("calls")).unwrap(),
        "call\ncall\n"
    );
    for entry in std::fs::read_dir(root.join("state/cache/deps")).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains("credential-sentinel"));
    }
    write(
        root,
        "osdk.toml",
        "[deps.private]\nrun=['sh','-c','echo failure >> attempts; exit 17']\n",
    );
    let failed = invoke(&["deps"]);
    assert!(!failed.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("attempts")).unwrap(),
        "failure\n"
    );
}

#[test]
fn canonical_sources_inherit_replace_and_clear_and_unknown_fields_fail() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "osdk.toml", "[registries.python]\ndefault='https://default.example/simple/'\nindexes=['https://first.example/simple/']\n[settings.precedence]\ntools='native'\npackages='osdk'\n");
    write(
        root,
        "child/osdk.toml",
        "extends='../osdk.toml'\n[registries.python]\ndefault=''\nindexes=[]\n",
    );
    let config =
        osdk_core::config::Config::load(&root.join("absent"), &root.join("child")).unwrap();
    assert_eq!(config.registries().python.default.as_deref(), Some(""));
    assert_eq!(config.registries().python.indexes, Some(vec![]));
    assert_eq!(
        config.settings.precedence.tools,
        osdk_core::config::SourcePrecedence::Native
    );
    write(root, "package.json", "{}");
    for field in ["index", "extra_index", "registry"] {
        write(
            root,
            "osdk.toml",
            &format!("[deps.npm]\n{field}='https://removed.example/'\n"),
        );
        let output = run(root, root, &["deps", "--list"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field"));
    }
}

#[test]
fn workspace_rejects_conflicting_canonical_registries_between_members() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "package.json", r#"{"packageManager":"pnpm@11.0.0"}"#);
    write(root, "pnpm-workspace.yaml", "packages: ['packages/*']\n");
    write(root, "packages/a/package.json", "{}");
    write(
        root,
        "osdk.toml",
        "[deps.pnpm]\n[registries.npm]\ndefault='https://one.example/'\n",
    );
    write(
        root,
        "packages/a/osdk.toml",
        "extends='../../osdk.toml'\n[registries.npm]\ndefault='https://two.example/'\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(["deps", "--list"])
        .current_dir(root)
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env(
            "OSDK_TRUSTED_CONFIG_PATHS",
            std::env::join_paths([root.join("osdk.toml"), root.join("packages/a/osdk.toml")])
                .unwrap(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("conflicting effective tool settings or sources"),
        "{output:?}"
    );
}

#[test]
fn local_child_verification_reads_its_own_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "osdk.toml",
        "[deps.projects.a.npm]\n[deps.projects.b.npm]\n",
    );
    for name in ["a", "b"] {
        write(
            root,
            &format!("{name}/package.json"),
            r#"{"packageManager":"npm@11.0.0"}"#,
        );
        write(
            root,
            &format!("{name}/package-lock.json"),
            r#"{"lockfileVersion":3,"packages":{}}"#,
        );
    }
    write(root, "a/osdk.lock", &format!("schema=4\n[deps.npm]\ninstaller='npm'\nmanifest='package.json'\nmanifest_sha256='{}'\nrun='npm install'\n[deps.npm.native_lock]\nkind='package-lock'\npath='package-lock.json'\nsha256='{}'\n", "00".repeat(32), "00".repeat(32)));
    let output = run(root, root, &["deps", "--verify", "--filter", "a"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("L1"),
        "{output:?}"
    );
    // An unrelated parent lock must not become the default for another child.
    write(root, "osdk.lock", "not a lockfile");
    let output = run(root, root, &["deps", "--dry-run", "--filter", "b"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("osdk.lock")).unwrap(),
        "not a lockfile"
    );
}

// A custom `run` names its program bare, the way a user writes it. On Windows
// the program is often only a `.cmd` launcher -- pnpm's managed install ships
// `pnpm.cmd` and nothing else -- and `CreateProcess` searches PATH for `.exe`
// alone, so handing it the bare name failed with "program not found" for a
// launcher sitting on the PATH osdk had just built.
#[test]
fn custom_run_reaches_a_launcher_only_found_through_executable_extensions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bin = root.join("launchers");
    std::fs::create_dir_all(&bin).unwrap();
    if cfg!(windows) {
        write(&bin, "fixture-launcher.cmd", "@echo called>called.txt\r\n");
    } else {
        write(
            &bin,
            "fixture-launcher",
            "#!/bin/sh\necho called > called.txt\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = bin.join("fixture-launcher");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    write(
        root,
        "osdk.toml",
        "[deps.launcher]\nrun = \"fixture-launcher\"\n",
    );
    let mut path = std::ffi::OsString::from(&bin);
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push(if cfg!(windows) { ";" } else { ":" });
        path.push(inherited);
    }

    let output = Command::new(env!("CARGO_BIN_EXE_osdk"))
        .args(["deps"])
        .current_dir(root)
        .env("PATH", path)
        .env("OSDK_CONFIG_DIR", root.join("state/config"))
        .env("OSDK_DATA_DIR", root.join("state/data"))
        .env("OSDK_CACHE_DIR", root.join("state/cache"))
        .env("OSDK_TRUSTED_CONFIG_PATHS", root.join("osdk.toml"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("called.txt"))
            .unwrap()
            .trim(),
        "called"
    );
}
