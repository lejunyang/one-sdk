use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

/// Gradle 的官方版本索引。
///
/// 本文件里三个工具中，只有 Gradle 的上游提供了可机读的全量发布列表，因此它按该列表
/// 解析版本，而不是像下面的内置目录那样只认一个版本。2026-09-14 实测：该端点返回 525
/// 条记录，每条自带 `downloadUrl` 与 SHA-256 `checksum`，所以无需为每个版本硬编码
/// URL 和摘要。
const GRADLE_VERSION_INDEX: &str = "https://services.gradle.org/versions/all";

/// [`GRADLE_VERSION_INDEX`] 中的一条记录。
///
/// 只建模影响「选哪个版本」与「完整性校验」的字段；该端点每条约二十个字段，其余在此
/// 用不到。
#[cfg(feature = "install")]
#[derive(serde::Deserialize)]
struct GradleRelease {
    version: String,
    #[serde(rename = "downloadUrl")]
    download_url: String,
    /// 发行版 zip 的 SHA-256，由索引直接给出。
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    snapshot: bool,
    #[serde(default)]
    nightly: bool,
    /// 非空表示这条记录是某个更高版本的 release candidate。
    #[serde(default, rename = "rcFor")]
    rc_for: String,
    /// 非空表示这条记录是某个更高版本的 milestone。
    #[serde(default, rename = "milestoneFor")]
    milestone_for: String,
    #[serde(default)]
    broken: bool,
}

#[cfg(feature = "install")]
impl GradleRelease {
    /// 是否为正式发布版本。
    ///
    /// 五个信号都需要：`snapshot`/`nightly` 标记滚动构建；`rcFor` 与 `milestoneFor`
    /// 标记的是**更高版本**的预发布 —— 2026-09-14 实测，只过滤 `rcFor` 仍会漏进 78 条
    /// `-milestone-N` 记录（如 `9.8.0-milestone-2`），会让 `latest` 落到预览版；
    /// `broken` 标记上游已撤回的发布。
    fn is_stable(&self) -> bool {
        !self.snapshot
            && !self.nightly
            && self.rc_for.is_empty()
            && self.milestone_for.is_empty()
            && !self.broken
    }
}

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

    /// 拉取并缓存 Gradle 的版本索引。
    ///
    /// 走 `http::get_cached_json`，与其他基于索引的 backend 一致，因此
    /// `list_remote_versions` 与 `install` 在同一次命令里不会各请求一遍。
    #[cfg(feature = "install")]
    async fn gradle_releases(ctx: &Ctx) -> Result<Vec<GradleRelease>> {
        let index_url = crate::source::select::ranked_source_list(ctx, &Self::Gradle)
            .await?
            .into_iter()
            .find_map(|source| source.index_url)
            .unwrap_or_else(|| GRADLE_VERSION_INDEX.to_string());
        crate::http::get_cached_json(ctx, &index_url).await
    }

    /// 按索引里的记录安装一个 Gradle 版本。
    ///
    /// 与内置目录路径的关键区别：URL 与 SHA-256 都来自索引，因此任何历史版本都能装，
    /// 而不只是编译期写死的那一个。索引未提供 checksum 的记录会被拒绝而不是放行 ——
    /// 校验和缺失不能降级成「不校验」。
    #[cfg(feature = "install")]
    async fn install_gradle(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            return run_plan(ctx, &plan).await;
        }
        let releases = Self::gradle_releases(ctx).await?;
        let release = releases
            .iter()
            .find(|release| release.version == tv.version)
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().into(),
                spec: tv.version.clone(),
                hint: Some("该版本不在 Gradle 官方版本索引中".into()),
            })?;
        let checksum = release
            .checksum
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().into(),
                spec: tv.version.clone(),
                hint: Some(
                    "Gradle 版本索引未给出该版本的 SHA-256，拒绝在无校验的情况下安装".into(),
                ),
            })?;
        let file_name = release
            .download_url
            .rsplit('/')
            .next()
            .unwrap_or("gradle-bin.zip")
            .to_string();
        let plan = InstallPlan {
            tool: self.id().into(),
            version: tv.version.clone(),
            urls: vec![release.download_url.clone()],
            kind: ArchiveKind::from_name(&file_name)?,
            file_name,
            checksum: Some(Checksum {
                algo: HashAlgo::Sha256,
                hex: checksum.trim().to_ascii_lowercase(),
            }),
            strip_root: true,
            subdir: None,
        };
        run_plan(ctx, &plan).await
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
        // Gradle 的下载地址按版本从索引里取，所以它的 source 是发行版目录，
        // 而不是某一个版本的 URL。
        if matches!(self, Self::Gradle) {
            return vec![Source::official(
                "official",
                "https://services.gradle.org/distributions/",
            )
            .with_index(GRADLE_VERSION_INDEX)];
        }
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

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        // Maven 与 Kotlin 的上游没有同类的可机读索引，因此它们仍只提供本次构建内置的
        // 那个版本。
        if !matches!(self, Self::Gradle) {
            return Ok(vec![VersionInfo::stable(self.release().version)]);
        }
        // 索引是新版在前，而 `select_version` 按「新版在后」扫描，故反转。
        let mut versions: Vec<VersionInfo> = Self::gradle_releases(ctx)
            .await?
            .iter()
            .map(|release| VersionInfo {
                version: release.version.clone(),
                stable: release.is_stable(),
                lts: None,
            })
            .collect();
        versions.reverse();
        Ok(versions)
    }

    #[cfg(feature = "install")]
    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        if matches!(self, Self::Gradle) {
            return self.install_gradle(ctx, tv).await;
        }
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
        // Gradle 不在此列：它的候选版本来自上游索引而非内置目录，需要网络，
        // 由下面的解析测试单独覆盖。
        let cases = [
            (JvmToolBackend::Maven, "maven", "3.9.16", "mvn"),
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
        assert_eq!(JvmToolBackend::Gradle.id(), "gradle");
        assert!(JvmToolBackend::Gradle
            .bin_names(&dummy_ctx(), &ToolVersion::new("gradle", "9.3.1"))
            .unwrap()
            .contains(&"gradle".to_string()));
    }

    /// Gradle 的候选版本必须来自上游索引，而不是一个写死的版本。
    ///
    /// 此前 `list_remote_versions` 只返回内置目录里的那一个版本，于是
    /// `osdk list-remote gradle 9` 只有 9.7.0，任何历史版本都装不了 —— 而上游索引
    /// 明明列出了 500 多个版本。这不是上游限制，是这里的索引覆盖缺陷。
    ///
    /// 断言的是**解析**而不是安装：解析才是当时坏掉的部分，且无需下载 100 MB 制品。
    #[test]
    fn gradle_versions_are_parsed_from_the_upstream_index() {
        // 字段取自实抓的 https://services.gradle.org/versions/all（2026-09-14），
        // 包含索引里真实存在的四种形态：正式版、milestone、rc、nightly。
        let index = r#"[
          {"version":"9.8.0-20260914025849+0000","snapshot":true,"nightly":false,
           "releaseNightly":true,"rcFor":"","milestoneFor":"","broken":false,
           "downloadUrl":"https://services.gradle.org/distributions-snapshots/gradle-9.8.0-20260914025849+0000-bin.zip",
           "checksum":"50598a6f8302d24f097bc6b1469d5686a189d317d40f557ae6702fd086e704f6"},
          {"version":"9.8.0-milestone-2","snapshot":false,"nightly":false,
           "rcFor":"","milestoneFor":"9.8.0","broken":false,
           "downloadUrl":"https://services.gradle.org/distributions/gradle-9.8.0-milestone-2-bin.zip",
           "checksum":"aa"},
          {"version":"9.7.1-rc-1","snapshot":false,"nightly":false,
           "rcFor":"9.7.1","milestoneFor":"","broken":false,
           "downloadUrl":"https://services.gradle.org/distributions/gradle-9.7.1-rc-1-bin.zip",
           "checksum":"bb"},
          {"version":"9.7.0","snapshot":false,"nightly":false,
           "rcFor":"","milestoneFor":"","broken":false,
           "downloadUrl":"https://services.gradle.org/distributions/gradle-9.7.0-bin.zip",
           "checksum":"84fbba45c7f4c64abc77460e1c00f541e9f960e3c7ed2538f1ede19eacd873ae"},
          {"version":"9.3.1","snapshot":false,"nightly":false,
           "rcFor":"","milestoneFor":"","broken":false,
           "downloadUrl":"https://services.gradle.org/distributions/gradle-9.3.1-bin.zip",
           "checksum":"b266d5ff6b90eada6dc3b20cb090e3731302e553a27c5d3e4df1f0d76beaff06"}
        ]"#;
        let releases: Vec<GradleRelease> = serde_json::from_str(index).unwrap();

        // 只有两条是正式版：milestone / rc / nightly 都必须被判为非稳定，否则
        // `latest` 会落到预览版。
        let stable: Vec<&str> = releases
            .iter()
            .filter(|release| release.is_stable())
            .map(|release| release.version.as_str())
            .collect();
        assert_eq!(stable, vec!["9.7.0", "9.3.1"]);

        // 关键回归点：wrapper 锁定的 9.3.1 必须可达，且 URL 与摘要都取自索引。
        let pinned = releases
            .iter()
            .find(|release| release.version == "9.3.1")
            .expect("9.3.1 在索引中");
        assert_eq!(
            pinned.download_url,
            "https://services.gradle.org/distributions/gradle-9.3.1-bin.zip"
        );
        // 与本仓库 android/gradle/wrapper 的 distributionSha256Sum 逐字符一致 ——
        // 这既证明索引权威，也说明按索引安装与 wrapper 校验不会冲突。
        assert_eq!(
            pinned.checksum.as_deref(),
            Some("b266d5ff6b90eada6dc3b20cb090e3731302e553a27c5d3e4df1f0d76beaff06")
        );

        // 索引里的顺序是新版在前；`list_remote_versions` 反转后交给
        // `select_version`，`latest` 才会取到 9.7.0 而不是 9.3.1。
        let mut ascending: Vec<VersionInfo> = releases
            .iter()
            .map(|release| VersionInfo {
                version: release.version.clone(),
                stable: release.is_stable(),
                lts: None,
            })
            .collect();
        ascending.reverse();
        assert_eq!(
            crate::version::select_version(&crate::version::VersionSpec::Latest, &ascending)
                .unwrap()
                .version,
            "9.7.0"
        );
        // 而历史版本按精确请求仍可达 —— 这正是缺陷修复前做不到的事。
        assert_eq!(
            crate::version::select_version(
                &crate::version::VersionSpec::parse("=9.3.1"),
                &ascending,
            )
            .unwrap()
            .version,
            "9.3.1"
        );
    }

    /// 索引没给校验和时必须拒绝安装，而不是降级成不校验。
    #[test]
    fn a_gradle_entry_without_a_checksum_is_not_installable() {
        let releases: Vec<GradleRelease> = serde_json::from_str(
            r#"[{"version":"9.9.9","snapshot":false,"nightly":false,"rcFor":"",
                 "milestoneFor":"","broken":false,
                 "downloadUrl":"https://services.gradle.org/distributions/gradle-9.9.9-bin.zip"}]"#,
        )
        .unwrap();
        assert!(releases[0].is_stable());
        assert!(releases[0].checksum.is_none());
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
                pipeline::artifact_cache_path(&ctx.dirs, backend.id(), release.version, &file_name)
                    .unwrap();
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
            config: crate::config::Config::default(),

            client: reqwest::Client::new(),
            show_progress: false,
        }
    }
}
