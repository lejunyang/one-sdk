use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

#[derive(Clone, Copy)]
pub enum JvmToolBackend {
    Maven,
    Gradle,
    Kotlin,
}

struct Release {
    version: &'static str,
    file: &'static str,
    url: &'static str,
    checksum: &'static str,
    algorithm: HashAlgo,
}

impl JvmToolBackend {
    fn release(self) -> Release {
        match self {
            Self::Maven => Release {
                version: "3.9.16",
                file: "apache-maven-3.9.16-bin.tar.gz",
                url: "https://downloads.apache.org/maven/maven-3/3.9.16/binaries/apache-maven-3.9.16-bin.tar.gz",
                checksum: "831a8591fe20c8243b1dbe7d71e3244f31d1665b0804b2e825e38cbbe5ce0cafb8338851f90780735568773e0a6cd07bbec107cda0b896b008b861075358b6f6",
                algorithm: HashAlgo::Sha512,
            },
            Self::Gradle => Release {
                version: "9.7.0",
                file: "gradle-9.7.0-bin.zip",
                url: "https://services.gradle.org/distributions/gradle-9.7.0-bin.zip",
                checksum: "84fbba45c7f4c64abc77460e1c00f541e9f960e3c7ed2538f1ede19eacd873ae",
                algorithm: HashAlgo::Sha256,
            },
            Self::Kotlin => Release {
                version: "2.4.10",
                file: "kotlin-compiler-2.4.10.zip",
                url: "https://github.com/JetBrains/kotlin/releases/download/v2.4.10/kotlin-compiler-2.4.10.zip",
                checksum: "473dd66c7a3ef4b182065b3da670466c1bf2773a9dbb0ed8b33a39fe9d4f876d",
                algorithm: HashAlgo::Sha256,
            },
        }
    }

    fn bin_path(self) -> &'static str {
        match self {
            Self::Kotlin => "bin",
            Self::Maven | Self::Gradle => "bin",
        }
    }
}

#[async_trait]
impl Backend for JvmToolBackend {
    fn id(&self) -> &str {
        match self {
            Self::Maven => "maven",
            Self::Gradle => "gradle",
            Self::Kotlin => "kotlin",
        }
    }

    fn aliases(&self) -> &[&str] {
        match self {
            Self::Maven => &["mvn"],
            Self::Gradle => &[],
            Self::Kotlin => &["kotlinc"],
        }
    }

    fn default_sources(&self) -> Vec<Source> {
        let release = self.release();
        let mut sources = vec![Source::official("official", release.url)];
        if release.url.contains("github.com") {
            sources.insert(
                0,
                Source::mirror(
                    "ghproxy",
                    &format!("https://gh-proxy.com/{}", release.url),
                    10,
                ),
            );
        }
        sources
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        Some(source.download_url.clone())
    }

    async fn list_remote_versions(&self, _ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        Ok(vec![VersionInfo::stable(self.release().version)])
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        if tv.version != self.release().version {
            return Err(Error::VersionResolve {
                tool: self.id().into(),
                spec: tv.version.clone(),
                hint: Some("version is not in the built-in JVM tool catalog".into()),
            });
        }
        if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            return run_plan(ctx, &plan).await;
        }

        let release = self.release();
        let urls = self
            .default_sources()
            .into_iter()
            .map(|source| source.download_url)
            .collect();
        let plan = InstallPlan {
            tool: self.id().into(),
            version: tv.version.clone(),
            urls,
            file_name: release.file.into(),
            kind: ArchiveKind::from_name(release.file)?,
            checksum: Some(Checksum {
                algo: release.algorithm,
                hex: release.checksum.into(),
            }),
            strip_root: true,
            subdir: None,
        };
        run_plan(ctx, &plan).await
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join(self.bin_path())])
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(match self {
            Self::Maven => vec!["mvn".into(), "mvnDebug".into()],
            Self::Gradle => vec!["gradle".into()],
            Self::Kotlin => vec![
                "kotlin".into(),
                "kotlinc".into(),
                "kotlinc-js".into(),
                "kotlinc-jvm".into(),
            ],
        })
    }

    fn idiomatic_files(&self) -> &[&str] {
        match self {
            Self::Maven => &[".mvn-version"],
            Self::Gradle => &[".gradle-version"],
            Self::Kotlin => &[".kotlin-version"],
        }
    }
}

async fn run_plan(ctx: &Ctx, plan: &InstallPlan) -> Result<()> {
    let pipeline_ctx = PipelineCtx {
        client: &ctx.client,
        dirs: &ctx.dirs,
        cas: &ctx.cas,
        link_mode: ctx.config.settings.link_mode,
        show_progress: ctx.show_progress,
        offline: ctx.config.settings.offline,
        require_checksums: true,
    };
    pipeline::run(plan, &pipeline_ctx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn candidates_are_independent_and_checksums_are_required() {
        let cases = [
            (JvmToolBackend::Maven, "maven", "3.9.16", "mvn"),
            (JvmToolBackend::Gradle, "gradle", "9.7.0", "gradle"),
            (JvmToolBackend::Kotlin, "kotlin", "2.4.10", "kotlinc"),
        ];
        for (backend, id, version, binary) in cases {
            assert_eq!(backend.id(), id);
            assert_eq!(
                backend.list_remote_versions(&dummy_ctx()).await.unwrap()[0].version,
                version
            );
            assert!(backend
                .bin_names(&dummy_ctx(), &ToolVersion::new(id, version))
                .unwrap()
                .contains(&binary.to_string()));
            assert!(!backend.release().checksum.is_empty());
        }
    }

    #[tokio::test]
    async fn every_jvm_tool_installs_from_a_verified_offline_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = dummy_ctx_at(temp.path());
        ctx.config.settings.offline = true;
        let cases = [
            (JvmToolBackend::Maven, "mvn"),
            (JvmToolBackend::Gradle, "gradle"),
            (JvmToolBackend::Kotlin, "kotlinc"),
        ];
        for (backend, binary) in cases {
            let release = backend.release();
            let file_name = format!("{}-fixture.tar.gz", backend.id());
            let archive = temp.path().join(&file_name);
            write_fixture_archive(&archive, binary);
            let checksum = pipeline::verify::hash_file(&archive, HashAlgo::Sha256).unwrap();
            let cached =
                pipeline::artifact_cache_path(&ctx.dirs, backend.id(), release.version, &file_name);
            std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
            std::fs::copy(&archive, &cached).unwrap();
            let mut version = ToolVersion::new(backend.id(), release.version);
            version.options.extend(std::collections::BTreeMap::from([
                (
                    pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                    "https://invalid.example/fixture.tar.gz".into(),
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
                .unwrap();
            assert!(ctx
                .dirs
                .install_path(backend.id(), release.version)
                .join("bin")
                .join(binary)
                .is_file());
        }
    }

    fn write_fixture_archive(path: &std::path::Path, binary: &str) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let contents = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("fixture/bin/{binary}"), &contents[..])
            .unwrap();
        archive.finish().unwrap();
    }

    fn dummy_ctx() -> Ctx {
        let temp = tempfile::tempdir().unwrap().keep();
        dummy_ctx_at(&temp)
    }

    fn dummy_ctx_at(temp: &std::path::Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }
}
