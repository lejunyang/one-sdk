#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::{Backend, Ctx, InstallCtx};
use crate::config::Config;
use crate::dirs::Dirs;
use crate::error::Result;
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, PipelineCtx};
use crate::platform::Platform;
use crate::source::Source;
use crate::store::Cas;
use crate::version::{ToolRequest, ToolVersion, VersionInfo};

struct ContractBackend {
    id: &'static str,
}

#[async_trait]
impl Backend for ContractBackend {
    fn id(&self) -> &str {
        self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        Vec::new()
    }

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        None
    }

    async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        Ok(vec![VersionInfo::stable("1.0.0")])
    }

    async fn install(&self, install: &InstallCtx<'_>, version: &ToolVersion) -> Result<()> {
        let plan = pipeline::locked_install_plan(self.id(), version, true)?
            .expect("contract uses a locked artifact");
        let context = PipelineCtx {
            client: &install.ctx.client,
            dirs: &install.ctx.dirs,
            cas: &install.ctx.cas,
            link_mode: install.ctx.config.settings.link_mode,
            show_progress: false,
            offline: true,
            require_checksums: true,
        };
        pipeline::run(&plan, &context).await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, version: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &version.version)
            .join("bin")])
    }

    fn bin_names(&self, _ctx: &Ctx, _version: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["contract-tool".into()])
    }
}

fn context(root: &std::path::Path) -> Ctx {
    let dirs = Dirs::resolve_from(|key| match key {
        "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
        "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
        "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
        _ => None,
    })
    .unwrap();
    dirs.ensure().unwrap();
    Ctx {
        cas: Arc::new(Cas::new(dirs.store.clone())),
        dirs,
        platform: Platform::current(),
        config: Config {
            settings: crate::config::Settings {
                offline: true,
                require_checksums: true,
                ..Default::default()
            },
            sources: Default::default(),
            tools: Default::default(),
            aliases: Default::default(),
            project_config_path: None,
        },
        client: reqwest::Client::new(),
        show_progress: false,
    }
}

fn fixture_archive(path: &std::path::Path) {
    let file = std::fs::File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let contents = b"#!/bin/sh\nprintf contract\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "root/bin/contract-tool", &contents[..])
        .unwrap();
    builder.finish().unwrap();
}

#[cfg(unix)]
fn real_backend_fixture(id: &str, path: &std::path::Path) {
    let file = std::fs::File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let files: Vec<(&str, &[u8])> = match id {
        "node" => vec![("root/bin/node", b"#!/bin/sh\nprintf node\n")],
        "npm" => vec![("root/bin/npm-cli.js", b""), ("root/bin/npx-cli.js", b"")],
        "go" => vec![("root/bin/go", b"#!/bin/sh\nprintf go\n")],
        "python" => vec![("root/bin/python", b"#!/bin/sh\nprintf python\n")],
        "java" => vec![("root/bin/java", b"#!/bin/sh\nprintf java\n")],
        "maven" => vec![("root/bin/mvn", b"#!/bin/sh\nprintf maven\n")],
        "gradle" => vec![("root/bin/gradle", b"#!/bin/sh\nprintf gradle\n")],
        "kotlin" => vec![("root/bin/kotlinc", b"#!/bin/sh\nprintf kotlin\n")],
        "pnpm" => vec![("root/pnpm", b"#!/bin/sh\nprintf pnpm\n")],
        "yarn" => vec![("root/bin/yarn.js", b"")],
        "deno" => vec![("root/deno", b"#!/bin/sh\nprintf deno\n")],
        "bun" => vec![("root/bin/bun", b"#!/bin/sh\nprintf bun\n")],
        _ => vec![("root/bin/tool", b"#!/bin/sh\nprintf tool\n")],
    };
    for (entry, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, entry, contents).unwrap();
    }
    builder.finish().unwrap();
}

#[tokio::test]
async fn all_builtin_backend_ids_satisfy_the_lifecycle_contract() {
    let expected = [
        "node", "npm", "go", "python", "java", "maven", "gradle", "kotlin", "rust", "pnpm", "yarn",
        "deno", "bun",
    ];
    assert_eq!(crate::backend::registry::Registry::new().ids(), expected);

    for id in expected.into_iter().chain(["github:example/tool"]) {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path());
        let backend = ContractBackend { id };
        let request = ToolRequest::parse(&format!("{id}@1.0.0")).unwrap();
        let mut resolved = backend.resolve_version(&ctx, &request).await.unwrap();
        assert_eq!(resolved.version, "1.0.0");

        let file_name = "contract.tgz";
        let archive = pipeline::artifact_cache_path(&ctx.dirs, id, "1.0.0", file_name);
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        fixture_archive(&archive);
        let checksum = pipeline::verify::hash_file(&archive, HashAlgo::Sha256).unwrap();
        resolved.options.extend(std::collections::BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://invalid.example/contract.tgz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
        ]));
        backend
            .install(&InstallCtx { ctx: &ctx }, &resolved)
            .await
            .unwrap();

        let install = ctx.dirs.install_path(id, "1.0.0");
        assert!(install.join(".osdk-complete").is_file(), "{id}");
        assert!(pipeline::artifact_receipt(&ctx.dirs, id, "1.0.0").is_some());
        let binary = backend.bin_paths(&ctx, &resolved).unwrap()[0].join("contract-tool");
        assert!(binary.is_file(), "{id}");
        #[cfg(unix)]
        {
            let output = std::process::Command::new(&binary).output().unwrap();
            assert!(output.status.success(), "{id}");
            assert_eq!(output.stdout, b"contract", "{id}");
        }
        backend.uninstall(&ctx, &resolved).await.unwrap();
        assert!(!install.exists(), "{id}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn real_builtin_backends_install_and_uninstall_locked_fixtures() {
    for id in [
        "node", "npm", "go", "python", "java", "maven", "gradle", "kotlin", "pnpm", "yarn", "deno",
        "bun",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let ctx = context(temporary.path());
        let registry = crate::backend::registry::Registry::new();
        let backend = registry.get(id).unwrap();
        let fixture_version = match id {
            "maven" => "3.9.16",
            "gradle" => "9.7.0",
            "kotlin" => "2.4.10",
            _ => "1.0.0",
        };
        let file_name = format!("{id}.tgz");
        let archive = pipeline::artifact_cache_path(&ctx.dirs, id, fixture_version, &file_name);
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        real_backend_fixture(id, &archive);
        let checksum = pipeline::verify::hash_file(&archive, HashAlgo::Sha256).unwrap();
        let mut version = ToolVersion::new(id, fixture_version);
        version.options.extend(std::collections::BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                format!("https://invalid.example/{file_name}"),
            ),
            (pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(), file_name),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
        ]));
        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        let install = ctx.dirs.install_path(id, fixture_version);
        assert!(install.join(".osdk-complete").is_file(), "{id}");
        assert!(
            pipeline::artifact_receipt(&ctx.dirs, id, fixture_version).is_some(),
            "{id}"
        );
        assert!(
            !backend.bin_paths(&ctx, &version).unwrap().is_empty(),
            "{id}"
        );
        backend.uninstall(&ctx, &version).await.unwrap();
        assert!(!install.exists(), "{id}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn real_rust_backend_uses_isolated_fake_rustup() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let mut ctx = context(temporary.path());
    ctx.config.settings.offline = false;
    let rustup = ctx.dirs.cargo_home().join("bin/rustup");
    std::fs::create_dir_all(rustup.parent().unwrap()).unwrap();
    std::fs::write(
        &rustup,
        format!(
            "#!/bin/sh\nif [ \"$1 $2\" = 'toolchain uninstall' ]; then rm -rf '{0}/toolchains/'\"$3\"; exit 0; fi\nmkdir -p '{0}/toolchains/stable/bin'\nprintf '#!/bin/sh\\nexit 0\\n' > '{0}/toolchains/stable/bin/rustc'\nchmod +x '{0}/toolchains/stable/bin/rustc'\n",
            ctx.dirs.rustup_home().display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&rustup, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = crate::backend::registry::Registry::new()
        .get("rust")
        .unwrap();
    let version = ToolVersion::new("rust", "stable");
    backend
        .install(&InstallCtx { ctx: &ctx }, &version)
        .await
        .unwrap();
    assert!(ctx
        .dirs
        .install_path("rust", "stable")
        .join(".osdk-complete")
        .is_file());
    backend.uninstall(&ctx, &version).await.unwrap();
    assert!(!ctx.dirs.install_path("rust", "stable").exists());
}

#[test]
fn contract_archive_shape_is_supported() {
    assert_eq!(
        ArchiveKind::from_name("contract.tgz").unwrap(),
        ArchiveKind::TarGz
    );
    let checksum = Checksum {
        algo: HashAlgo::Sha256,
        hex: "0".repeat(64),
    };
    assert_eq!(checksum.hex.len(), 64);
}
