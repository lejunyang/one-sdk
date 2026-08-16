use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osdk_core::platform::{Arch, Libc, Os, Platform};
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};
use serde::{Deserialize, Serialize};

pub const LOCKFILE_NAME: &str = "osdk.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default = "schema_version")]
    pub schema: u32,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformLock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlatformLock {
    #[serde(default)]
    pub tools: BTreeMap<String, LockedTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedTool {
    pub request: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<LockedArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedArtifact {
    pub url: String,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<osdk_core::verification::VerificationEvidence>,
}

fn schema_version() -> u32 {
    1
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
        }
    }
}

impl Default for LockedTool {
    fn default() -> Self {
        LockedTool {
            request: "latest".into(),
            version: String::new(),
            options: BTreeMap::new(),
            artifact: None,
        }
    }
}

pub fn platform_key(platform: Platform) -> String {
    let os = match platform.os {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
        Arch::X86 => "x86",
        Arch::Arm => "arm",
    };
    match platform.libc {
        Libc::Musl => format!("{os}-{arch}-musl"),
        _ => format!("{os}-{arch}"),
    }
}

pub fn platform_for_resolved(host: Platform, resolved: &[(ToolRequest, ToolVersion)]) -> Platform {
    let mut platform = host;
    for (request, version) in resolved {
        if request.backend != "node" {
            continue;
        }
        if let Some(arch) = version
            .options
            .get("arch")
            .and_then(|value| Arch::parse_node(value))
        {
            platform.arch = arch;
        }
    }
    platform
}

pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|directory| directory.join(LOCKFILE_NAME))
        .find(|path| path.is_file())
}

pub fn default_path(start: &Path) -> PathBuf {
    find(start).unwrap_or_else(|| start.join(LOCKFILE_NAME))
}

pub fn load(path: &Path) -> Result<Lockfile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading lockfile {}", path.display()))?;
    let lockfile: Lockfile =
        toml::from_str(&text).with_context(|| format!("parsing lockfile {}", path.display()))?;
    if lockfile.schema != schema_version() {
        anyhow::bail!(
            "unsupported lockfile schema {} in {}",
            lockfile.schema,
            path.display()
        );
    }
    Ok(lockfile)
}

pub fn locked_requests(path: &Path, platform: Platform) -> Result<Option<Vec<ToolRequest>>> {
    let lockfile = load(path)?;
    let Some(platform_lock) = lockfile.platforms.get(&platform_key(platform)) else {
        return Ok(None);
    };
    let requests = platform_lock
        .tools
        .iter()
        .map(|(backend, locked)| {
            let mut options = locked.options.clone();
            if let Some(artifact) = &locked.artifact {
                options.insert(
                    osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                    artifact.url.clone(),
                );
                options.insert(
                    osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                    artifact.file_name.clone(),
                );
                if let Some(checksum) = &artifact.checksum {
                    options.insert(
                        osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                        checksum.clone(),
                    );
                }
            }
            ToolRequest {
                backend: backend.clone(),
                spec: VersionSpec::Exact(locked.version.clone()),
                options,
            }
        })
        .collect();
    Ok(Some(requests))
}

pub fn merge_resolved(
    path: &Path,
    platform: Platform,
    dirs: &osdk_core::dirs::Dirs,
    resolved: &[(ToolRequest, ToolVersion)],
) -> Result<()> {
    let mut lockfile = if path.is_file() {
        load(path)?
    } else {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
        }
    };
    let platform_lock = lockfile
        .platforms
        .entry(platform_key(platform))
        .or_default();
    platform_lock.tools.clear();
    for (request, version) in resolved {
        platform_lock.tools.insert(
            request.backend.clone(),
            LockedTool {
                request: request.spec.to_string(),
                version: version.version.clone(),
                options: public_options(&version.options),
                artifact: osdk_core::pipeline::artifact_receipt(
                    dirs,
                    &version.backend,
                    &version.version,
                )
                .map(|receipt| LockedArtifact {
                    url: receipt.url,
                    file_name: receipt.file_name,
                    checksum: receipt.checksum,
                    evidence: receipt.evidence,
                }),
            },
        );
    }
    save(path, &lockfile)
}

fn public_options(options: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    options
        .iter()
        .filter(|(key, _)| !key.starts_with("__osdk_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn save(path: &Path, lockfile: &Lockfile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(lockfile)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> Platform {
        Platform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        }
    }

    #[test]
    fn merge_preserves_other_platforms() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut initial = Lockfile {
            schema: 1,
            platforms: BTreeMap::new(),
        };
        initial.platforms.insert(
            "windows-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "node".into(),
                    LockedTool {
                        request: "20".into(),
                        version: "20.19.0".into(),
                        options: BTreeMap::new(),
                        artifact: None,
                    },
                )]),
            },
        );
        save(&path, &initial).unwrap();

        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@20").unwrap(),
                ToolVersion::new("node", "20.20.0"),
            )],
        )
        .unwrap();
        let lockfile = load(&path).unwrap();
        assert_eq!(lockfile.platforms.len(), 2);
        assert_eq!(
            lockfile.platforms["linux-x64"].tools["node"].version,
            "20.20.0"
        );
        assert_eq!(
            lockfile.platforms["windows-x64"].tools["node"].version,
            "20.19.0"
        );
    }

    #[test]
    fn locked_requests_restore_exact_versions_and_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("rust", "stable");
        version.options.insert("profile".into(), "minimal".into());
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(ToolRequest::parse("rust@stable").unwrap(), version)],
        )
        .unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].spec, VersionSpec::Exact("stable".into()));
        assert_eq!(requests[0].options["profile"], "minimal");
    }

    #[test]
    fn node_arch_option_selects_the_target_platform_lock() {
        let host = linux();
        let request = ToolRequest::parse("node@20").unwrap();
        let mut version = ToolVersion::new("node", "20.20.0");
        version.options.insert("arch".into(), "arm64".into());
        let target = platform_for_resolved(host, &[(request, version)]);
        assert_eq!(platform_key(target), "linux-arm64");
    }

    #[test]
    fn legacy_artifact_without_evidence_still_loads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools."github:cli/cli"]
request = "2.96.0"
version = "2.96.0"

[platforms.linux-x64.tools."github:cli/cli".artifact]
url = "https://example.test/gh.tar.gz"
file_name = "gh.tar.gz"
checksum = "sha256:00"
"#,
        )
        .unwrap();

        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools["github:cli/cli"]
            .artifact
            .as_ref()
            .unwrap()
            .evidence
            .is_empty());
    }

    #[test]
    fn lock_persists_verification_evidence_without_trusting_it_as_an_option() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let install = dirs.install_path("github:cli/cli", "2.96.0");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(
            install.join(".osdk-artifact.json"),
            r#"{
  "url": "https://example.test/gh.tar.gz",
  "file_name": "gh.tar.gz",
  "checksum": "sha256:00",
  "evidence": [{
    "kind": "sigstore-bundle",
    "repository": "cli/cli",
    "issuer": "https://token.actions.githubusercontent.com",
    "digest": "sha256:00"
  }]
}"#,
        )
        .unwrap();
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("github:cli/cli@2.96.0").unwrap(),
                ToolVersion::new("github:cli/cli", "2.96.0"),
            )],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["github:cli/cli"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.evidence.len(), 1);
        assert_eq!(artifact.evidence[0].repository, "cli/cli");

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert!(requests[0]
            .options
            .keys()
            .all(|key| !key.contains("evidence")));
    }

    fn test_dirs(root: &Path) -> osdk_core::dirs::Dirs {
        osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap()
    }
}
