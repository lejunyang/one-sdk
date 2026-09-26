use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use osdk_core::backend::native_tool::{
    LOCKED_NATIVE_REPLAY_OPTION, LOCKED_NATIVE_RUNTIME_OPTION, LOCKED_NATIVE_RUNTIME_VERSION_OPTION,
};
use osdk_core::backend::npm_package::{
    LOCKED_NPM_LOCKFILE_OPTION, LOCKED_NPM_LOCK_FORMAT_OPTION, LOCKED_NPM_LOCK_SHA256_OPTION,
    LOCKED_NPM_NODE_VERSION_OPTION, LOCKED_NPM_PACKAGE_OPTION,
};
use osdk_core::pipeline::HashAlgo;
use osdk_core::platform::{Arch, Libc, Os, Platform};
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};
use serde::{Deserialize, Serialize};

pub const LOCKFILE_NAME: &str = "osdk.lock";
const NPM_LOCK_FORMAT: &str = "package-lock-v3";
const NPM_LOCKFILE_PATH: &str = "project/package-lock.json";
const NPM_GRAPH_DIRECTORY: &str = "osdk.lock.d/npm";
const LOCKED_NPM_INSTALLER_OPTION: &str = "__osdk_npm_installer";
/// Installer a `pypi:` entry was locked with, passed to the backend so a replay
/// uses the same resolver rather than whichever one happens to be present.
///
/// Re-exported from the backend rather than spelled out again here. Two separate
/// definitions of the same option name would let the writer and the reader drift
/// apart, and nothing would fail to compile -- the replay would just quietly
/// stop honouring the lockfile.
pub use osdk_core::backend::pypi::LOCKED_INSTALLER_OPTION as LOCKED_PYPI_INSTALLER_OPTION;
/// uv version a `pypi:` entry was locked with, so a replay pins the same
/// resolver release rather than the newest one available.
pub const LOCKED_PYPI_UV_VERSION_OPTION: &str = "__osdk_pypi_uv_version";
/// Interpreter version a `pypi:` entry was locked against, carried through a
/// replay so re-locking preserves it rather than re-deriving it from this machine.
pub const LOCKED_PYPI_PYTHON_VERSION_OPTION: &str = "__osdk_pypi_python_version";
const LOCKED_NPM_SCOPE_OPTION: &str = "__osdk_npm_scope";
const LOCKED_NPM_NATIVE_LOCK_KIND_OPTION: &str = "__osdk_npm_native_lock_kind";
const LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION: &str = "__osdk_npm_native_lock_format";
const LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION: &str = "__osdk_npm_native_lock_sha256";
const MAX_LOCKFILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NPM_GRAPH_BYTES: u64 = 16 * 1024 * 1024;
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub schema: u32,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformLock>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, LockedModel>,
    /// Application dependency environments, keyed by provider id. Skipped when
    /// empty so existing locks serialize byte-identically and an older build
    /// ignores the section instead of failing (no deny_unknown_fields here).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deps: BTreeMap<String, LockedDeps>,
    /// Installed agent skills, keyed by skill name. Skipped when empty so
    /// existing locks serialize byte-identically and an older build ignores the
    /// section instead of failing (no deny_unknown_fields here).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, LockedSkill>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<LockedNpmGraph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<LockedNativeTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pypi: Option<LockedPypiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conda: Option<LockedCondaTool>,
}

/// What a `pypi:` install needs recorded to be reproducible elsewhere.
///
/// The version alone is not enough. `pypi:cowsay = "6.1"` can be satisfied by
/// two different resolvers -- uv when it is present, pip when it is not -- and
/// they do not agree: uv publishes a list of deliberate deviations from pip's
/// resolution behaviour. So the same lockfile could produce different transitive
/// dependencies on two machines while both honestly reported "6.1".
///
/// This mirrors what `LockedNpmMetadata` records for npm (`installer` plus
/// `node_version`): not just what was installed, but what installed it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedPypiTool {
    /// Which tool built the environment and resolved the dependencies.
    pub installer: PypiInstaller,
    /// uv's own version, when uv did the work.
    ///
    /// Recorded because uv is a resolver whose behaviour changes between
    /// releases, so "installed with uv" is only half an answer. Absent for the
    /// pip path, where the resolver ships with the interpreter instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uv_version: Option<String>,
    /// Interpreter version the environment was built against (`3.14.7`).
    ///
    /// A venv is bound to its interpreter's minor version; replaying against a
    /// different one silently produces a different environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python_version: Option<String>,
}

/// What a `conda:` install needs recorded to be reproducible elsewhere.
///
/// The version alone is not enough, and for conda it is further from enough than
/// for any other backend. `conda:ninja = "1.13.2"` does not name an artifact: it
/// names a *solve*, whose result is a closure of packages -- five for ninja, a
/// dozen for a compiler -- each with its own URL and digest. The same version
/// resolved a week later, or against a different channel set, legitimately
/// produces different builds. So a lock carrying only `version = "1.13.2"`
/// promises far less than it appears to: it pins a request, not an environment.
///
/// Every other backend records enough to detect that. `go` locks a URL plus a
/// SHA-256; `pypi:` locks which resolver built the environment. A conda entry
/// used to carry `request` and `version` and nothing else -- no artifact section
/// at all -- because `installed_artifact_receipt` looked for the receipt at the
/// flat `<installs>/<tool>/<version>` path, while a conda prefix lives one level
/// deeper under its install id. The receipt was on disk the whole time; the lock
/// writer was looking in the wrong place and, finding nothing, recorded nothing.
///
/// The digest recorded here is the one the backend already computes to decide
/// *which prefix a solve belongs in*, over each package's URL and sha256, sorted
/// so solver iteration order cannot change it. That makes it exactly the value
/// that answers "is this the same environment": if a replay solves to a
/// different closure, its digest differs and the divergence is visible instead
/// of silent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedCondaTool {
    /// Digest over the solved closure's package URLs and SHA-256s
    /// (`blake3:<hex>`), as recorded in the install identity's materials.
    pub closure: String,
    /// Number of packages the solve produced.
    ///
    /// Not redundant with the digest: it is what makes a mismatch legible. A
    /// differing digest alone says only "not the same"; `5 -> 11` says the
    /// closure grew, which is usually a changed channel set or `with` list.
    pub packages: usize,
}

/// The tool that resolved and installed a `pypi:` environment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PypiInstaller {
    Uv,
    Pip,
}

impl PypiInstaller {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uv => "uv",
            Self::Pip => "pip",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "uv" => Ok(Self::Uv),
            "pip" => Ok(Self::Pip),
            other => anyhow::bail!("unsupported pypi installer `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNativeTool {
    pub runtime: String,
    pub runtime_version: String,
    pub replay: NativeReplay,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NativeReplay {
    VersionOnly,
    ImmutableRevision,
    FloatingRef,
}

impl NativeReplay {
    fn as_str(self) -> &'static str {
        match self {
            Self::VersionOnly => "version-only",
            Self::ImmutableRevision => "immutable-revision",
            Self::FloatingRef => "floating-ref",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "version-only" => Ok(Self::VersionOnly),
            "immutable-revision" => Ok(Self::ImmutableRevision),
            "floating-ref" => Ok(Self::FloatingRef),
            _ => anyhow::bail!("unsupported native lock replay mode `{value}`"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LockedNpmGraph {
    Metadata(LockedNpmMetadata),
    Sidecar(LockedNpmSidecar),
    Legacy(LegacyLockedNpmGraph),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNpmMetadata {
    pub package: String,
    pub installer: NpmInstaller,
    pub scope: LockScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_lock: Option<LockedNativeLock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNativeLock {
    pub kind: NpmInstaller,
    pub format: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NpmInstaller {
    #[serde(alias = "package-lock", alias = "npm-shrinkwrap")]
    Npm,
    Pnpm,
}

impl NpmInstaller {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "npm" | "package-lock" | "npm-shrinkwrap" => Ok(Self::Npm),
            "pnpm" => Ok(Self::Pnpm),
            _ => anyhow::bail!(osdk_core::t!(
                "err.lock_npm_installer_unsupported",
                installer = value
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockScope {
    Project,
    Global,
}

impl LockScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedNpmSidecar {
    pub package: String,
    pub node_version: String,
    pub lock_format: String,
    pub sha256: String,
    pub graph: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyLockedNpmGraph {
    pub package: String,
    pub lock_format: String,
    pub lock_sha256: String,
    pub lockfile: String,
}

impl LockedNpmGraph {
    fn sidecar(&self, backend: &str) -> Result<&LockedNpmSidecar> {
        match self {
            Self::Sidecar(sidecar) => Ok(sidecar),
            Self::Metadata(_) | Self::Legacy(_) => anyhow::bail!(osdk_core::t!(
                "err.lock_npm_legacy_inline_regenerate",
                backend = backend
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedArtifact {
    pub url: String,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<osdk_core::verification::VerificationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedModel {
    pub provider: osdk_core::model::ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub files: Vec<LockedModelFile>,
    /// Consumer views declared for this model (research §6.3): consumer ->
    /// profile -> (repo path prefix -> category). Skip when empty so existing
    /// locks serialize byte-identically and an older build ignores the field
    /// rather than failing (no deny_unknown_fields on LockedModel).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub views: BTreeMap<String, LockedModelView>,
}

/// One consumer view declaration carried in the lock. Only the identity
/// (profile + category mapping) is recorded -- never the local view path or
/// the link mode, which are machine-specific (research §6.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedModelView {
    #[serde(default)]
    pub profile: String,
    /// Repo-relative path prefix (normalized to `/`) -> consumer category.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
}

/// A skill recorded in the lock so `osdk skills sync` can reproduce it.
///
/// The bytes are not stored (like models and http artifacts); the `content_hash`
/// pins the exact staged content, `resolved_commit` records the immutable commit
/// a floating GitHub ref resolved to, and `agents` records where it was linked.
/// No `deny_unknown_fields`, matching the other lock entries, so a newer field
/// does not make an older build reject the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedSkill {
    /// Canonical source string, e.g. `github:vercel-labs/agent-skills`.
    pub source: String,
    /// osdk's BLAKE3 content digest of the staged skill (`b3-v2:...`).
    pub content_hash: String,
    /// Immutable commit a GitHub source resolved to, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    /// Selected skill name within a multi-skill repository, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Agent ids this skill is linked into.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedModelFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Identity of one materialized application dependency environment.
///
/// What is recorded is what another machine needs to reproduce *the same*
/// install: which installer at which version, driven by which runtime, against
/// which manifest and native lockfile, from which index. Deliberately **not**
/// recorded: absolute paths (machine locations), the contents of
/// `node_modules` (that is the native lockfile's job -- osdk does not keep a
/// second dependency graph), and credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedDeps {
    /// Installer id actually used (`npm`, `pnpm`, ...).
    pub installer: String,
    /// Exact installer version, when osdk resolved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installer_version: Option<String>,
    /// Runtime the installer ran under, as `id@version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Manifest path relative to the lock, normalized to `/` because this value
    /// is committed and read on other platforms.
    pub manifest: String,
    pub manifest_sha256: String,
    /// The native lockfile this install consumed, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_lock: Option<LockedDepsNativeLock>,
    /// Effective command, kept so a reviewer can see what ran and so a changed
    /// command invalidates freshness.
    pub run: String,
    /// Recorded as the provider's canonical registry; a mirror is folded to it
    /// for the same reason model endpoints are -- one machine's fastest host is
    /// not part of the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Whether the entry opted into running build scripts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_build_from_source: bool,
}

/// The native lockfile an application environment was installed from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedDepsNativeLock {
    /// Stable kind label, e.g. `package-lock` or `pnpm-lock`.
    pub kind: String,
    /// Path relative to the lock, normalized to `/`.
    pub path: String,
    pub sha256: String,
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
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
            npm: None,
            native: None,
            // Reconstructed from a request rather than from an install, so there
            // is no environment to read an installer from, and no solved prefix
            // to read a closure digest from.
            pypi: None,
            conda: None,
        }
    }
}

/// What `merge_deps` needs to record one application environment.
pub struct DepsRecord<'a> {
    pub installer: String,
    pub installer_version: Option<String>,
    pub runtime: Option<String>,
    pub project_root: &'a Path,
    pub manifest: &'a Path,
    pub native_lock: Option<&'a Path>,
    pub run: String,
    pub index: Option<String>,
    pub allow_build_from_source: bool,
}

/// Options that record a human decision rather than an artifact input. A lock
/// file is committed and replayed on other machines, so persisting these would
/// let one person's agreement silently stand in for everybody else's. They are
/// dropped on write and must be supplied again per machine.
pub(crate) const CONSENT_OPTIONS: &[&str] = &["accept-licenses", "accept-license"];

mod read;
mod write;
pub use read::*;
pub use write::*;

#[cfg(test)]
mod tests {
    /// The interpreter version is parsed out of an osdk-managed path.
    ///
    /// This is what makes a lock entry replayable: a venv is bound to its
    /// interpreter's minor version, so replaying against a different one
    /// produces a different environment while still reporting the same tool
    /// version.
    ///
    /// Every case here runs on every platform on purpose. The parse used to go
    /// through `Path::components`, which splits on the host's separator only, so
    /// the Windows-shaped path below was a single opaque component on Unix and its
    /// version came back as `None` -- a receipt written on Windows and read on
    /// Linux lost the very fact the entry exists to record. Asserting both shapes
    /// on both platforms is what makes that regression fail where it happens,
    /// instead of only on the machine that wrote the path.
    #[test]
    fn python_version_is_read_from_the_managed_interpreter_path() {
        assert_eq!(
            python_version_from_interpreter(r"E:\osdk-data\data\installs\python\3.14.7\python.exe")
                .as_deref(),
            Some("3.14.7")
        );
        assert_eq!(
            python_version_from_interpreter("/home/u/.osdk/installs/python/3.11.16/bin/python3")
                .as_deref(),
            Some("3.11.16")
        );
        // Mixed separators do occur: osdk joins its own layout onto a root that
        // may already be spelled either way.
        assert_eq!(
            python_version_from_interpreter(r"E:\osdk-data/data/installs\python/3.13.2\python.exe")
                .as_deref(),
            Some("3.13.2")
        );

        // An interpreter outside osdk's layout yields nothing rather than a
        // guess: recording a wrong version is worse than recording none, since
        // a wrong one would be replayed as though it had been verified.
        assert_eq!(python_version_from_interpreter("/usr/bin/python3"), None);
        // Now that both separators are split everywhere, this path is segmented
        // on Unix as well, so the "looks like a version" guard is what has to
        // reject it -- which is why it is asserted on every platform.
        assert_eq!(
            python_version_from_interpreter(r"C:\Python311\python.exe"),
            None
        );

        // A directory merely named `python` must not be mistaken for the
        // version segment.
        assert_eq!(
            python_version_from_interpreter("/opt/python/bin/python"),
            None
        );
        assert_eq!(
            python_version_from_interpreter(r"C:\tools\python\bin\python.exe"),
            None
        );
    }

    /// A replay's recorded installer outranks whatever this machine has.
    ///
    /// Re-running `lock` after replaying on a machine without uv used to re-derive
    /// the installer from disk, rewriting `installer = "uv"` to `"pip"` and
    /// committing the weaker fact -- the recorded promise silently replaced by the
    /// local situation.
    ///
    /// clippy pointed straight at this: `PypiInstaller::parse` was reported as
    /// never used. Deleting it would have silenced the warning and kept the bug;
    /// the read-back path was what was actually missing.
    #[test]
    fn a_replayed_entry_keeps_its_recorded_installer() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temp.path());

        // A request as it arrives from a replay: no environment on disk at all,
        // so anything derived locally would come back empty or wrong.
        let mut version = ToolVersion::new("pypi:requests", "2.34.2".to_string());
        version
            .options
            .insert(LOCKED_PYPI_INSTALLER_OPTION.into(), "uv".into());
        version
            .options
            .insert(LOCKED_PYPI_UV_VERSION_OPTION.into(), "0.12.14".into());
        version
            .options
            .insert(LOCKED_PYPI_PYTHON_VERSION_OPTION.into(), "3.14.7".into());

        let metadata = locked_pypi_metadata(&dirs, Platform::current(), &version)
            .expect("a replayed entry must keep its recorded installer");
        assert_eq!(metadata.installer, PypiInstaller::Uv);
        assert_eq!(metadata.uv_version.as_deref(), Some("0.12.14"));
        assert_eq!(metadata.python_version.as_deref(), Some("3.14.7"));

        // Without a recorded installer there is nothing to preserve, and with no
        // environment on disk either, the honest answer is nothing rather than a
        // default that would be written into the lockfile as though observed.
        let bare = ToolVersion::new("pypi:requests", "2.34.2".to_string());
        assert!(locked_pypi_metadata(&dirs, Platform::current(), &bare).is_none());
    }

    /// The installer enum round-trips through its serialized spelling.
    #[test]
    fn pypi_installer_round_trips() {
        for installer in [PypiInstaller::Uv, PypiInstaller::Pip] {
            let text = installer.as_str();
            assert_eq!(PypiInstaller::parse(text).unwrap(), installer);
        }
        assert!(PypiInstaller::parse("poetry").is_err());
    }

    /// A pypi lock entry serializes with its installer, and survives a reload.
    ///
    /// The version alone cannot distinguish the two resolvers, and uv documents
    /// deliberate deviations from pip's resolution, so an entry that omits this
    /// can be satisfied differently on another machine while both honestly
    /// report the same version.
    #[test]
    fn pypi_installer_identity_survives_a_save_and_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.lock");

        let mut lockfile = Lockfile::default();
        let mut platform_lock = PlatformLock::default();
        platform_lock.tools.insert(
            "pypi:requests".to_string(),
            LockedTool {
                request: "latest".into(),
                version: "2.34.2".into(),
                options: Default::default(),
                artifact: None,
                npm: None,
                native: None,
                pypi: Some(LockedPypiTool {
                    installer: PypiInstaller::Uv,
                    uv_version: Some("0.12.14".into()),
                    python_version: Some("3.14.7".into()),
                }),
                conda: None,
            },
        );
        lockfile
            .platforms
            .insert("windows-x64".to_string(), platform_lock);
        save(&path, &lockfile).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("installer = \"uv\""), "{text}");
        assert!(text.contains("uv_version = \"0.12.14\""), "{text}");
        assert!(text.contains("python_version = \"3.14.7\""), "{text}");

        let reloaded = load(&path).unwrap();
        let entry = reloaded.platforms["windows-x64"].tools["pypi:requests"]
            .pypi
            .clone()
            .expect("installer identity must survive a reload");
        assert_eq!(entry.installer, PypiInstaller::Uv);
        assert_eq!(entry.uv_version.as_deref(), Some("0.12.14"));
    }

    /// An entry without the pypi section still loads.
    ///
    /// Every lockfile written before this existed lacks the section, and a
    /// lockfile that stops loading after an upgrade would be a breaking change
    /// dressed up as a new feature.
    #[test]
    fn a_lockfile_without_installer_identity_still_loads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("osdk.lock");
        std::fs::write(
            &path,
            "schema = 4\n\n[platforms.windows-x64.tools.\"pypi:cowsay\"]\nrequest = \"6.1\"\nversion = \"6.1\"\n",
        )
        .unwrap();

        let loaded = load(&path).unwrap();
        let entry = &loaded.platforms["windows-x64"].tools["pypi:cowsay"];
        assert_eq!(entry.version, "6.1");
        assert!(entry.pypi.is_none());
    }
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
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
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
                        npm: None,
                        native: None,
                        pypi: None,
                        conda: None,
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
        assert_eq!(lockfile.schema, schema_version());
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
    fn schema_one_non_npm_lock_is_read_and_migrated_on_merge() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools.node]
request = "20"
version = "20.19.0"
"#,
        )
        .unwrap();

        let legacy = load(&path).unwrap();
        assert_eq!(legacy.schema, 1);
        assert!(legacy.platforms["linux-x64"].tools["node"].npm.is_none());
        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].backend, "node");
        assert_eq!(requests[0].spec, VersionSpec::Exact("20.19.0".into()));

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
        assert_eq!(load(&path).unwrap().schema, schema_version());
    }

    #[test]
    fn unsupported_schema_versions_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        for schema in [0, schema_version() + 1] {
            std::fs::write(&path, format!("schema = {schema}\n")).unwrap();
            let error = load(&path).unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("unsupported lockfile schema {schema}")));
        }
    }

    #[test]
    fn locked_requests_reject_path_traversal_before_constructing_install_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 2

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "../../../../victim"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "package-lock-v3"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
graph = "osdk.lock.d/npm/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.yaml"
"#,
        )
        .unwrap();

        let error = locked_requests(&path, linux()).unwrap_err();
        assert!(error.to_string().contains("unsafe version"));
    }

    #[test]
    fn saving_a_schema_one_lock_without_merging_upgrades_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let legacy = Lockfile {
            schema: 1,
            platforms: BTreeMap::new(),
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
        };
        save(&path, &legacy).unwrap();
        assert_eq!(load(&path).unwrap().schema, schema_version());
    }

    #[test]
    fn direct_save_migrates_schema_one_npm_to_metadata_only() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, b"sentinel").unwrap();
        let legacy = Lockfile {
            schema: 1,
            platforms: BTreeMap::from([(
                "linux-x64".into(),
                PlatformLock {
                    tools: BTreeMap::from([(
                        "npm:prettier".into(),
                        LockedTool {
                            request: "3".into(),
                            version: "3.6.2".into(),
                            options: BTreeMap::new(),
                            artifact: None,
                            npm: None,
                            native: None,
                            pypi: None,
                            conda: None,
                        },
                    )]),
                },
            )]),
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
        };
        save(&path, &legacy).unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(lock.schema, schema_version());
        assert!(matches!(
            lock.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));
    }

    #[test]
    fn schema_is_required_instead_of_defaulting_to_current() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            "[platforms.linux-x64.tools.node]\nrequest = \"20\"\nversion = \"20.0.0\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err();
        assert!(error.to_string().contains("parsing lockfile"));
    }

    #[test]
    fn schema_one_npm_without_inline_payload_is_readable_and_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.windows-x64.tools.node]
request = "20"
version = "20.19.0"

[platforms.windows-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"
"#,
        )
        .unwrap();

        assert_eq!(load(&path).unwrap().schema, 1);
        let error = locked_requests(
            &path,
            Platform {
                os: Os::Windows,
                arch: Arch::X64,
                libc: Libc::None,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));

        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(
                ToolRequest::parse("node@24").unwrap(),
                ToolVersion::new("node", "24.1.0"),
            )],
        )
        .unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.schema, schema_version());
        assert!(matches!(
            migrated.platforms["windows-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));

        merge_model(&path, &test_model_manifest()).unwrap();
        assert!(load(&path).unwrap().models.contains_key("qwen"));
    }

    #[test]
    fn load_rejects_oversized_main_lock_before_toml_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_LOCKFILE_BYTES + 1).unwrap();
        let error = load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds maximum size"));
    }

    #[test]
    fn save_rejects_oversized_main_lock_without_replacing_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, b"sentinel").unwrap();
        let lockfile = Lockfile {
            schema: schema_version(),
            platforms: BTreeMap::new(),
            models: BTreeMap::from([(
                "oversized".into(),
                LockedModel {
                    provider: osdk_core::model::ProviderId::HuggingFace,
                    repository: "x".repeat(MAX_LOCKFILE_BYTES as usize),
                    requested_revision: "main".into(),
                    revision: "abc123".into(),
                    endpoint: "https://huggingface.co".into(),
                    variant: None,
                    files: Vec::new(),
                    views: BTreeMap::new(),
                },
            )]),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
        };

        let error = save(&path, &lockfile).unwrap_err();
        assert!(error.to_string().contains("exceeds maximum size"));
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
    }

    #[test]
    fn schema_one_inline_npm_is_readable_but_cannot_be_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[platforms.linux-x64.tools.node]
request = "20"
version = "20.19.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
lock_format = "package-lock-v3"
lock_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
lockfile = "lockfileVersion: '9.0'"
"#,
        )
        .unwrap();

        let legacy = load(&path).unwrap();
        assert!(matches!(
            legacy.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Legacy(_))
        ));
        let before = std::fs::read(&path).unwrap();
        let error = save(&path, &legacy).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be migrated without a dependency graph"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn scoped_npm_graph_round_trips_exact_payload_and_injects_private_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let backend = "npm:@antfu/ni";
        let version_number = "0.21.12";
        let lockfile = "lockfileVersion: '9.0'\r\nimporters:\r\n  .:\r\n    dependencies:\r\n      '@antfu/ni':\r\n        version: 0.21.12\r\n# caf\u{e9}\r\n";
        let installed_lock = dirs
            .install_path(backend, version_number)
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, lockfile.as_bytes()).unwrap();

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("npm:@antfu/ni@0.21.12").unwrap(),
                    ToolVersion::new(backend, version_number),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();

        let expected_sha256 =
            osdk_core::pipeline::verify::hash_bytes(lockfile.as_bytes(), HashAlgo::Sha256);
        assert_eq!(
            expected_sha256,
            "23c7efec1baea86b6efc78164c175fc6cfe3458e5f70f2ec018a94045f94d7b3"
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.schema, schema_version());
        assert!(loaded.platforms["linux-x64"].tools[backend]
            .artifact
            .is_none());
        let npm = loaded.platforms["linux-x64"].tools[backend]
            .npm
            .as_ref()
            .unwrap();
        let LockedNpmGraph::Metadata(npm) = npm else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.package, "@antfu/ni");
        assert_eq!(npm.installer, NpmInstaller::Npm);
        assert_eq!(npm.scope, LockScope::Project);
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
        assert_eq!(
            npm.native_lock,
            Some(LockedNativeLock {
                kind: NpmInstaller::Npm,
                format: NPM_LOCK_FORMAT.into(),
                sha256: expected_sha256.clone(),
            })
        );
        let main_lock = std::fs::read_to_string(&path).unwrap();
        assert!(!main_lock.contains("lockfile ="));
        assert!(!main_lock.contains("lock_sha256"));
        assert!(!main_lock.contains("graph ="));
        assert!(!main_lock.contains("# caf"));
        assert!(!temp.path().join(NPM_GRAPH_DIRECTORY).exists());

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let options = &requests
            .iter()
            .find(|request| request.backend == backend)
            .unwrap()
            .options;
        assert_eq!(options[LOCKED_NPM_PACKAGE_OPTION], "@antfu/ni");
        assert_eq!(options[LOCKED_NPM_INSTALLER_OPTION], "npm");
        assert_eq!(options[LOCKED_NPM_SCOPE_OPTION], "project");
        assert_eq!(options[LOCKED_NPM_NATIVE_LOCK_KIND_OPTION], "npm");
        assert_eq!(
            options[LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION],
            NPM_LOCK_FORMAT
        );
        assert_eq!(
            options[LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION],
            expected_sha256
        );
        assert_eq!(options[LOCKED_NPM_NODE_VERSION_OPTION], "24.1.0");
        assert!(!options.contains_key(LOCKED_NPM_LOCKFILE_OPTION));
        assert!(!options.contains_key(LOCKED_NPM_LOCK_FORMAT_OPTION));
        assert!(!options.contains_key(LOCKED_NPM_LOCK_SHA256_OPTION));
    }

    #[test]
    fn npm_lock_records_optional_node_and_ignores_stale_artifact_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let backend = "npm:prettier";
        let version = "3.6.2";
        let install = dirs.install_path(backend, version);
        std::fs::create_dir_all(install.join("project")).unwrap();
        std::fs::write(install.join(NPM_LOCKFILE_PATH), b"lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(
            install.join(".osdk-artifact.json"),
            r#"{"url":"https://evil.invalid/fake.tgz","file_name":"fake.tgz"}"#,
        )
        .unwrap();

        let npm_only = [(
            ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
            ToolVersion::new(backend, version),
        )];
        merge_resolved(&path, linux(), &dirs, &npm_only).unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools[backend]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.node_version.is_none());

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                npm_only[0].clone(),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();
        assert!(load(&path).unwrap().platforms["linux-x64"].tools[backend]
            .artifact
            .is_none());
    }

    #[test]
    fn npm_lock_binds_to_the_final_resolved_node_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n").unwrap();

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("node@20.19.0").unwrap(),
                    ToolVersion::new("node", "20.19.0"),
                ),
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let npm = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap();
        let LockedNpmGraph::Metadata(npm) = npm else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
        assert_eq!(lock.platforms["linux-x64"].tools["node"].version, "24.1.0");
    }

    #[test]
    fn scoped_writer_records_global_scope_without_a_graph_reference() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();

        let resolved = [
            (
                ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                ToolVersion::new("npm:prettier", "3.6.2"),
            ),
            (
                ToolRequest::parse("node@24.1.0").unwrap(),
                ToolVersion::new("node", "24.1.0"),
            ),
        ];
        std::fs::write(&installed_lock, b"lockfileVersion: '9.0'\n").unwrap();
        merge_resolved_with_scope(&path, linux(), &dirs, &resolved, LockScope::Global).unwrap();

        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.scope, LockScope::Global);
        assert!(!std::fs::read_to_string(path).unwrap().contains("graph ="));
    }

    #[test]
    fn scoped_upsert_preserves_other_tools_platforms_and_models() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let mut initial = Lockfile::default();
        initial.platforms.insert(
            "windows-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "node".into(),
                    LockedTool {
                        request: "20".into(),
                        version: "20.19.0".into(),
                        ..LockedTool::default()
                    },
                )]),
            },
        );
        initial.platforms.insert(
            "linux-x64".into(),
            PlatformLock {
                tools: BTreeMap::from([(
                    "rust".into(),
                    LockedTool {
                        request: "stable".into(),
                        version: "1.98.0".into(),
                        ..LockedTool::default()
                    },
                )]),
            },
        );
        initial.models.insert("qwen".into(), test_locked_model());
        save(&path, &initial).unwrap();

        let request = ToolRequest::parse("npm:prettier@3.6.2").unwrap();
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "24.1.0".into());
        upsert_resolved_with_scope(&path, linux(), &dirs, &request, &version, LockScope::Global)
            .unwrap();

        let lock = load(&path).unwrap();
        assert!(lock.platforms["windows-x64"].tools.contains_key("node"));
        assert!(lock.platforms["linux-x64"].tools.contains_key("rust"));
        assert!(lock.models.contains_key("qwen"));
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.scope, LockScope::Global);
        assert_eq!(npm.node_version.as_deref(), Some("24.1.0"));
    }

    #[test]
    fn removing_one_tool_preserves_other_lock_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
                (
                    ToolRequest::parse("go@1.24.0").unwrap(),
                    ToolVersion::new("go", "1.24.0"),
                ),
            ],
        )
        .unwrap();
        merge_model(&path, &test_model_manifest()).unwrap();

        assert!(remove_tool(&path, linux(), "go").unwrap());
        assert!(!remove_tool(&path, linux(), "missing").unwrap());
        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools.contains_key("node"));
        assert!(!lock.platforms["linux-x64"].tools.contains_key("go"));
        assert!(lock.models.contains_key("qwen"));
    }

    #[test]
    fn schema_two_sidecar_remains_frozen_read_compatible_and_migrates_without_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let payload = b"lockfileVersion: '9.0'\n";
        let sha256 = osdk_core::pipeline::verify::hash_bytes(payload, HashAlgo::Sha256);
        let text = valid_schema_two_npm_lock_with_sha256(&sha256);
        std::fs::write(&path, text).unwrap();
        let sidecar = npm_graph_path(&path, &npm_graph_relative_path(&sha256));
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, payload).unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let options = &requests
            .iter()
            .find(|request| request.backend == "npm:prettier")
            .unwrap()
            .options;
        assert_eq!(options[LOCKED_NPM_LOCKFILE_OPTION].as_bytes(), payload);

        merge_model(&path, &test_model_manifest()).unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(lock.schema, schema_version());
        assert!(matches!(
            lock.platforms["linux-x64"].tools["npm:prettier"].npm,
            Some(LockedNpmGraph::Metadata(_))
        ));
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(!rewritten.contains("graph ="));
        assert!(sidecar.is_file());
    }

    #[test]
    fn schema_two_write_refuses_missing_sidecar_before_replacing_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let before = std::fs::read(&path).unwrap();

        let error = merge_model(&path, &test_model_manifest()).unwrap_err();
        assert!(format!("{error:#}").contains("reading npm graph sidecar"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn schema_three_rejects_payload_paths_and_non_exact_node_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let valid = valid_schema_three_npm_lock();

        std::fs::write(
            &path,
            valid.replace(
                "scope = \"project\"",
                "scope = \"project\"\ngraph = \"legacy.yaml\"",
            ),
        )
        .unwrap();
        assert!(format!("{:#}", load(&path).unwrap_err()).contains("did not match any variant"));

        std::fs::write(
            &path,
            valid.replace("node_version = \"24.1.0\"", "node_version = \"24\""),
        )
        .unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("non-exact node version"));
    }

    #[test]
    fn schema_three_native_lock_owner_can_differ_from_selected_installer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_three_npm_lock()
            .replace("kind = \"npm\"", "kind = \"pnpm\"")
            .replace("format = \"package-lock-v3\"", "format = \"pnpm-v9\"");
        std::fs::write(&path, text).unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 reads npm metadata");
        };
        assert_eq!(npm.installer, NpmInstaller::Npm);
        assert_eq!(npm.native_lock.as_ref().unwrap().kind, NpmInstaller::Pnpm);
    }

    #[test]
    fn schema_three_rejects_future_native_lock_formats() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        for (kind, format) in [
            ("npm", "package-lock-v99"),
            ("pnpm", "pnpm-v10"),
            ("npm", "package-lock-v4"),
            ("npm", "npm-shrinkwrap-v4"),
        ] {
            let text = valid_schema_three_npm_lock()
                .replace("kind = \"npm\"", &format!("kind = \"{kind}\""))
                .replace(
                    "format = \"package-lock-v3\"",
                    &format!("format = \"{format}\""),
                );
            std::fs::write(&path, text).unwrap();
            let error = load(&path).unwrap_err().to_string();
            assert!(
                error.contains(format),
                "unexpected error for {format}: {error}"
            );
        }
    }

    #[test]
    fn schema_three_errors_render_in_chinese() {
        let missing = osdk_core::i18n::interpolate(
            &osdk_core::i18n::trl(
                osdk_core::i18n::Lang::Zh,
                "err.lock_schema3_npm_metadata_missing",
            ),
            &[("backend", "npm:prettier"), ("platform", "linux-x64")],
        );
        assert_eq!(
            missing,
            "平台 `linux-x64` 上的 schema 3 npm 条目 `npm:prettier` 缺少 npm 元数据"
        );

        let format = osdk_core::i18n::interpolate(
            &osdk_core::i18n::trl(
                osdk_core::i18n::Lang::Zh,
                "err.lock_npm_native_format_unsupported",
            ),
            &[
                ("format", "future-v10"),
                ("backend", "npm:prettier"),
                ("owner", "pnpm"),
            ],
        );
        assert!(format.contains("原生 npm 锁文件格式 `future-v10` 不受支持"));
        assert!(format.contains("`npm:prettier`"));
    }

    #[test]
    fn schema_two_npm_invariants_are_rejected_before_sidecar_read() {
        let cases = [
            ("package = \"other\"", "package mismatch"),
            ("node_version = \"23.0.0\"", "node version mismatch"),
            (
                "lock_format = \"future-v10\"",
                "unsupported npm graph format",
            ),
            (
                "sha256 = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"",
                "invalid SHA-256",
            ),
            ("graph = \"../outside.yaml\"", "unsafe npm graph path"),
        ];
        for (replacement, expected) in cases {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(LOCKFILE_NAME);
            let base = valid_schema_two_npm_lock();
            let text = if replacement.starts_with("package") {
                base.replace("package = \"prettier\"", replacement)
            } else if replacement.starts_with("node_version") {
                base.replace("node_version = \"24.1.0\"", replacement)
            } else if replacement.starts_with("lock_format") {
                base.replace("lock_format = \"package-lock-v3\"", replacement)
            } else if replacement.starts_with("sha256") {
                base.replace(&format!("sha256 = \"{}\"", "a".repeat(64)), replacement)
            } else {
                base.replace(
                    &format!("graph = \"osdk.lock.d/npm/{}.yaml\"", "a".repeat(64)),
                    replacement,
                )
            };
            std::fs::write(&path, text).unwrap();
            let error = load(&path).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn schema_two_rejects_missing_graph_non_npm_metadata_and_generic_npm_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let missing = valid_schema_two_npm_lock()
            .split("[platforms.linux-x64.tools.\"npm:prettier\".npm]")
            .next()
            .unwrap()
            .to_string();
        std::fs::write(&path, missing).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("missing its dependency graph"));

        let non_npm =
            valid_schema_two_npm_lock().replace("tools.\"npm:prettier\"", "tools.prettier");
        std::fs::write(&path, non_npm).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot carry npm graph metadata"));

        let with_artifact = valid_schema_two_npm_lock().replace(
            "[platforms.linux-x64.tools.\"npm:prettier\".npm]",
            "[platforms.linux-x64.tools.\"npm:prettier\".artifact]\nurl = \"https://evil.invalid/fake.tgz\"\nfile_name = \"fake.tgz\"\n\n[platforms.linux-x64.tools.\"npm:prettier\".npm]",
        );
        std::fs::write(&path, with_artifact).unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot carry a generic artifact receipt"));
    }

    #[test]
    fn schema_two_npm_table_rejects_inline_and_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock()
            .replace("graph =", "lockfile = \"inline graph forbidden\"\ngraph =");
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("did not match any variant"));
    }

    #[test]
    fn schema_two_npm_requires_a_matching_node_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock().replace(
            "[platforms.linux-x64.tools.node]\nrequest = \"24\"\nversion = \"24.1.0\"\n\n",
            "",
        );
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(error.to_string().contains("requires a locked `node` entry"));
    }

    #[test]
    fn schema_two_rejects_npm_metadata_without_an_npm_backend() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let text = valid_schema_two_npm_lock().replace("tools.\"npm:prettier\"", "tools.prettier");
        std::fs::write(&path, text).unwrap();
        let error = load(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot carry npm graph metadata"));
    }

    #[test]
    fn locked_requests_rejects_missing_tampered_and_oversized_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let missing = locked_requests(&path, linux()).unwrap_err();
        assert!(format!("{missing:#}").contains("reading npm graph sidecar"));

        let sidecar = npm_graph_path(&path, &npm_graph_relative_path(&"a".repeat(64)));
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"tampered").unwrap();
        let tampered = locked_requests(&path, linux()).unwrap_err();
        assert!(tampered.to_string().contains("checksum mismatch"));

        let oversized = std::fs::File::create(&sidecar).unwrap();
        oversized.set_len(MAX_NPM_GRAPH_BYTES + 1).unwrap();
        drop(oversized);
        let oversized = locked_requests(&path, linux()).unwrap_err();
        assert!(format!("{oversized:#}").contains("exceeds maximum size"));
    }

    #[cfg(unix)]
    #[test]
    fn locked_requests_rejects_symlinked_sidecar_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, valid_schema_two_npm_lock()).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("npm")).unwrap();
        std::fs::create_dir_all(temp.path().join("osdk.lock.d")).unwrap();
        symlink(&outside, temp.path().join("osdk.lock.d/npm")).unwrap();

        let error = locked_requests(&path, linux()).unwrap_err();
        assert!(error.to_string().contains("path contains symlink"));
    }

    #[test]
    fn npm_merge_allows_metadata_without_an_installed_native_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.native_lock.is_none());
    }

    #[test]
    fn npm_merge_hashes_non_utf8_native_lock_without_persisting_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let installed_lock = dirs
            .install_path("npm:prettier", "3.6.2")
            .join(NPM_LOCKFILE_PATH);
        std::fs::create_dir_all(installed_lock.parent().unwrap()).unwrap();
        std::fs::write(&installed_lock, [0xff, 0xfe]).unwrap();

        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[
                (
                    ToolRequest::parse("npm:prettier@3.6.2").unwrap(),
                    ToolVersion::new("npm:prettier", "3.6.2"),
                ),
                (
                    ToolRequest::parse("node@24.1.0").unwrap(),
                    ToolVersion::new("node", "24.1.0"),
                ),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(char::REPLACEMENT_CHARACTER));
        assert!(!text.contains("lockfile ="));
        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert!(npm.native_lock.is_some());
    }

    #[test]
    fn npm_metadata_accepts_declared_pnpm_native_lock_without_npm_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_INSTALLER_OPTION.into(), "pnpm".into());
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(), "pnpm".into());
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            "pnpm-v9".into(),
        );
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(), "b".repeat(64));
        merge_resolved_with_scope(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(ToolRequest::parse("npm:prettier@3.6.2").unwrap(), version)],
            LockScope::Global,
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let LockedNpmGraph::Metadata(npm) = lock.platforms["linux-x64"].tools["npm:prettier"]
            .npm
            .as_ref()
            .unwrap()
        else {
            panic!("schema 3 writes npm metadata");
        };
        assert_eq!(npm.installer, NpmInstaller::Pnpm);
        assert_eq!(npm.scope, LockScope::Global);
        assert_eq!(npm.native_lock.as_ref().unwrap().format, "pnpm-v9");
    }

    #[test]
    fn package_lock_kind_alias_is_normalized_to_npm_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_INSTALLER_OPTION.into(), "npm".into());
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            "package-lock".into(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            "package-lock-v2".into(),
        );
        version
            .options
            .insert(LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(), "c".repeat(64));
        merge_resolved_with_scope(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[(ToolRequest::parse("npm:prettier@3.6.2").unwrap(), version)],
            LockScope::Project,
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"npm\""));
        assert!(!text.contains("kind = \"package-lock\""));
        assert!(text.contains("format = \"package-lock-v2\""));
    }

    #[test]
    fn model_lock_preserves_platforms_and_records_file_digests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("node@20").unwrap(),
                ToolVersion::new("node", "20.20.0"),
            )],
        )
        .unwrap();
        let manifest = test_model_manifest();
        merge_model(&path, &manifest).unwrap();
        let lock = load(&path).unwrap();
        assert!(lock.platforms["linux-x64"].tools.contains_key("node"));
        assert_eq!(lock.models["qwen"].revision, "abc123");
        assert_eq!(lock.models["qwen"].files[0].sha256, "sha256");
    }

    /// A `[deps]` section round-trips, and an empty one is omitted so existing
    /// locks keep serializing byte-identically.
    ///
    /// The read side is asserted on purpose: AGENTS.md records that `pypi` once
    /// wrote an `installer` nothing read back, so re-locking silently discarded
    /// it. A write-only `[deps]` section would repeat that exactly -- the lock
    /// would promise an installer and index that no replay consults.
    #[test]
    fn deps_section_round_trips_and_is_omitted_when_empty() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);

        // Empty: not serialized at all.
        let mut lockfile = Lockfile::default();
        save(&path, &lockfile).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[deps"), "{text}");

        lockfile.deps.insert(
            "pnpm".into(),
            LockedDeps {
                installer: "pnpm".into(),
                installer_version: Some("12.5.1".into()),
                runtime: Some("node@22.23.2".into()),
                manifest: "package.json".into(),
                manifest_sha256: "aa".repeat(32),
                native_lock: Some(LockedDepsNativeLock {
                    kind: "pnpm-lock".into(),
                    path: "pnpm-lock.yaml".into(),
                    sha256: "bb".repeat(32),
                }),
                run: "pnpm install --frozen-lockfile --ignore-scripts".into(),
                index: Some("https://registry.npmjs.org/".into()),
                allow_build_from_source: false,
            },
        );
        save(&path, &lockfile).unwrap();

        // Read it back: this is the path that keeps the section from being
        // write-only.
        let reloaded = load(&path).unwrap();
        let entry = &reloaded.deps["pnpm"];
        assert_eq!(entry.installer, "pnpm");
        assert_eq!(entry.installer_version.as_deref(), Some("12.5.1"));
        assert_eq!(entry.runtime.as_deref(), Some("node@22.23.2"));
        assert_eq!(entry.native_lock.as_ref().unwrap().kind, "pnpm-lock");
        assert_eq!(entry.run, "pnpm install --frozen-lockfile --ignore-scripts");
        assert!(!entry.allow_build_from_source);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[deps.pnpm]"), "{text}");
        // A false flag stays out of the file, so the common case adds no noise.
        assert!(!text.contains("allow_build_from_source"), "{text}");
    }

    /// A lock written by a newer osdk (one that knows `[deps]`) must remain
    /// readable by a build that does not: no `deny_unknown_fields` anywhere on
    /// the way in. This is the same property `[models.*.views]` relies on, and
    /// it is asserted rather than assumed because the whole point of the section
    /// being optional is that older binaries keep working.
    #[test]
    fn an_unknown_section_does_not_stop_the_lock_from_loading() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            concat!(
                "schema = 4\n",
                "\n",
                // A section this build does not know at all.
                "[future_capability.thing]\n",
                "value = 1\n",
                "\n",
                "[deps.pnpm]\n",
                "installer = \"pnpm\"\n",
                "manifest = \"package.json\"\n",
                "manifest_sha256 = \"aa\"\n",
                "run = \"pnpm install\"\n",
                // A field inside a known section that this build does not know.
                "unknown_future_field = true\n",
            ),
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.deps["pnpm"].installer, "pnpm");
    }

    #[test]
    fn model_views_round_trip_and_empty_views_are_omitted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        merge_model(&path, &test_model_manifest()).unwrap();

        // A model with no declared views serializes no `views` key at all.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[models.qwen.views"), "{text}");

        // Write views from a declaration shape.
        let declared = std::collections::BTreeMap::from([(
            "comfyui".to_string(),
            osdk_core::config::ModelViewDeclaration {
                profile: "default".to_string(),
                map: std::collections::BTreeMap::from([(
                    "unet/".to_string(),
                    "diffusion_models".to_string(),
                )]),
            },
        )]);
        let locked = locked_views_from_declaration(&declared);
        assert!(set_model_views(&path, "qwen", locked).unwrap());

        // Read it back: the read path AGENTS.md demands for a new field, so
        // re-lock cannot silently drop the declaration.
        let lock = load(&path).unwrap();
        let view = &lock.models["qwen"].views["comfyui"];
        assert_eq!(view.profile, "default");
        assert_eq!(view.map["unet/"], "diffusion_models");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[models.qwen.views.comfyui.map]"), "{text}");

        // Idempotent: setting the same views again changes nothing.
        let again = locked_views_from_declaration(&declared);
        assert!(!set_model_views(&path, "qwen", again).unwrap());

        // Unknown model and missing file are clean misses, not errors.
        assert!(!set_model_views(&path, "ghost", BTreeMap::new()).unwrap());
        let missing = temp.path().join("nope.lock");
        assert!(!set_model_views(&missing, "qwen", BTreeMap::new()).unwrap());
    }

    #[test]
    fn legacy_model_provider_spelling_remains_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"
schema = 1

[models.fixture]
provider = "hugging-face"
repository = "owner/repo"
requested_revision = "main"
revision = "abc123"
endpoint = "https://huggingface.co"
files = []
"#,
        )
        .unwrap();
        let lock = load(&path).unwrap();
        assert_eq!(
            lock.models["fixture"].provider,
            osdk_core::model::ProviderId::HuggingFace
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
    fn locked_requests_accept_canonical_http_keys_and_reject_unsafe_ones() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let digest = "a".repeat(64);
        let backend = "http:https://downloads.example.test/tool-{version}.zip";
        std::fs::write(
            &path,
            format!(
                r#"schema = 3

[platforms.linux-x64.tools.{backend:?}]
request = "1.2.3"
version = "1.2.3"
options = {{ sha256 = {digest:?}, kind = "zip", bins = "bin/tool" }}

[platforms.linux-x64.tools.{backend:?}.artifact]
url = "https://downloads.example.test/tool-1.2.3.zip"
file_name = "tool-1.2.3.zip"
checksum = "sha256:{digest}"
"#
            ),
        )
        .unwrap();
        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(requests[0].backend, backend);

        for unsafe_backend in [
            "http:https://example.test/../tool-{version}.zip",
            "http:https://example.test/%2e%2e/tool-{version}.zip",
            r"http:https:\\example.test\tool-{version}.zip",
            "http:https://user@example.test/tool-{version}.zip",
            "http:https://example.test/tool-{version}.zip?token=x",
            "http:https://example.test/tool.zip",
        ] {
            let mut lock = Lockfile::default();
            lock.platforms.insert(
                "linux-x64".into(),
                PlatformLock {
                    tools: BTreeMap::from([(unsafe_backend.into(), LockedTool::default())]),
                },
            );
            let error = save(&path, &lock).unwrap_err();
            assert!(
                error.to_string().contains("unsafe backend id"),
                "{unsafe_backend}: {error}"
            );
        }
    }

    #[test]
    fn lock_backend_validation_keeps_npm_ids_unchanged() {
        for backend in ["npm:prettier", "npm:@antfu/ni"] {
            validate_locked_tool_identity(
                backend,
                &LockedTool {
                    version: "1.2.3".into(),
                    ..LockedTool::default()
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn lock_only_native_id_validation_is_strict_without_registering_backends() {
        for valid in [
            "cargo:ripgrep",
            "cargo:cargo_edit",
            "cargo:https://github.com/BurntSushi/ripgrep.git",
            "cargo:https://git.example.test/Team/tool",
            "go:example.com/Acme/tool",
        ] {
            let locked = LockedTool {
                version: "1.2.3".into(),
                ..LockedTool::default()
            };
            validate_locked_tool_identity(valid, &locked).unwrap();
        }
        for invalid in [
            "cargo:",
            "cargo:RipGrep",
            "cargo:foo/bar",
            "cargo:../tool",
            "cargo:http://example.test/tool.git",
            "cargo:https://user@example.test/tool.git",
            "cargo:https://example.test/tool.git?token=secret",
            "cargo:https://example.test/tool.git#main",
            "cargo:https://example.test/a/../tool.git",
            "cargo:https://example.test/%2e%2e/tool.git",
            "cargo:https://example.test/tool.git/",
            "go:",
            "go:tool",
            "go:example.com//tool",
            "go:example.com/../tool",
            "go:example.com/tool@v1",
            "go:exa$mple.com/tool",
            "go:-example.com/tool",
            "go:example!.com/tool",
        ] {
            let locked = LockedTool {
                version: "1.2.3".into(),
                ..LockedTool::default()
            };
            assert!(
                validate_locked_tool_identity(invalid, &locked).is_err(),
                "{invalid}"
            );
        }
        assert!(matches!(
            ToolRequest::parse("cargo:ripgrep@1.2.3"),
            Ok(request) if request.backend == "cargo:ripgrep"
        ));
    }

    #[test]
    fn schema_four_cargo_git_metadata_round_trips_with_matching_rust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let backend = "cargo:https://github.com/BurntSushi/ripgrep.git";
        std::fs::write(
            &path,
            format!(
                r#"schema = 4

[platforms.linux-x64.tools.rust]
request = "1.91.1"
version = "1.91.1"

[platforms.linux-x64.tools.{backend:?}]
request = "rev:0123456789abcdef0123456789abcdef01234567"
version = "rev:0123456789abcdef0123456789abcdef01234567"

[platforms.linux-x64.tools.{backend:?}.native]
runtime = "rust"
runtime_version = "1.91.1"
replay = "immutable-revision"
"#
            ),
        )
        .unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let cargo = requests
            .iter()
            .find(|request| request.backend == backend)
            .unwrap();
        assert_eq!(
            cargo.spec,
            VersionSpec::Exact("rev:0123456789abcdef0123456789abcdef01234567".into())
        );
        assert_eq!(cargo.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert_eq!(
            cargo.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        assert_eq!(
            cargo.options[LOCKED_NATIVE_REPLAY_OPTION],
            "immutable-revision"
        );
    }

    #[test]
    fn schema_four_cargo_requires_the_same_locked_rust_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let valid = r#"schema = 4

[platforms.linux-x64.tools.rust]
request = "1.91.1"
version = "1.91.1"

[platforms.linux-x64.tools."cargo:ripgrep"]
request = "14.1.1"
version = "14.1.1"

[platforms.linux-x64.tools."cargo:ripgrep".native]
runtime = "rust"
runtime_version = "1.91.1"
replay = "version-only"
source = "sparse+https://index.crates.io/"
"#;
        std::fs::write(&path, valid).unwrap();
        load(&path).unwrap();

        std::fs::write(
            &path,
            valid.replace(
                "[platforms.linux-x64.tools.rust]\nrequest = \"1.91.1\"\nversion = \"1.91.1\"\n\n",
                "",
            ),
        )
        .unwrap();
        let missing = load(&path).unwrap_err();
        assert!(missing.to_string().contains("requires `rust`"), "{missing}");

        std::fs::write(
            &path,
            valid.replace(
                "runtime_version = \"1.91.1\"",
                "runtime_version = \"1.90.0\"",
            ),
        )
        .unwrap();
        let mismatched = load(&path).unwrap_err();
        assert!(
            mismatched
                .to_string()
                .contains("does not match `rust` entry"),
            "{mismatched}"
        );

        std::fs::write(
            &path,
            valid
                .replace("request = \"1.91.1\"", "request = \"stable\"")
                .replace("version = \"1.91.1\"", "version = \"stable\"")
                .replace(
                    "runtime_version = \"1.91.1\"",
                    "runtime_version = \"stable\"",
                ),
        )
        .unwrap();
        let floating = load(&path).unwrap_err();
        assert!(
            floating.to_string().contains("exact Rust runtime version"),
            "{floating}"
        );

        std::fs::write(
            &path,
            valid.replace("replay = \"version-only\"", "replay = \"floating-ref\""),
        )
        .unwrap();
        let replay = load(&path).unwrap_err();
        assert!(replay.to_string().contains("does not match"), "{replay}");

        std::fs::write(
            &path,
            valid.replace("source = \"sparse+https://index.crates.io/\"\n", ""),
        )
        .unwrap();
        let missing_source = load(&path).unwrap_err();
        assert!(
            missing_source
                .to_string()
                .contains("missing its registry source"),
            "{missing_source}"
        );
    }

    #[test]
    fn schema_four_native_metadata_round_trips_into_private_replay_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        std::fs::write(
            &path,
            r#"schema = 4

[platforms.linux-x64.tools.rust]
request = "1.91.1"
version = "1.91.1"

[platforms.linux-x64.tools."cargo:ripgrep"]
request = "14"
version = "14.1.1"
options = { locked = "true" }

[platforms.linux-x64.tools."cargo:ripgrep".native]
runtime = "rust"
runtime_version = "1.91.1"
replay = "version-only"
source = "sparse+https://index.crates.io/"
"#,
        )
        .unwrap();

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        let cargo = requests
            .iter()
            .find(|request| request.backend == "cargo:ripgrep")
            .unwrap();
        assert_eq!(cargo.spec, VersionSpec::Exact("14.1.1".into()));
        assert_eq!(cargo.options["locked"], "true");
        assert_eq!(cargo.options[LOCKED_NATIVE_RUNTIME_OPTION], "rust");
        assert_eq!(
            cargo.options[LOCKED_NATIVE_RUNTIME_VERSION_OPTION],
            "1.91.1"
        );
        assert_eq!(cargo.options[LOCKED_NATIVE_REPLAY_OPTION], "version-only");
        assert_eq!(
            cargo.options[osdk_core::backend::cargo_package::LOCKED_CARGO_INDEX_OPTION],
            "sparse+https://index.crates.io/"
        );
    }

    #[test]
    fn native_writer_emits_typed_metadata_without_private_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let request = ToolRequest {
            backend: "cargo:ripgrep".into(),
            spec: VersionSpec::Exact("14.1.1".into()),
            options: BTreeMap::new(),
        };
        let mut version = ToolVersion::new("cargo:ripgrep", "14.1.1");
        version.options.extend(BTreeMap::from([
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "rust".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), "1.91.1".into()),
            (LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into()),
            (
                osdk_core::backend::cargo_package::LOCKED_CARGO_INDEX_OPTION.into(),
                "sparse+https://index.crates.io/".into(),
            ),
            ("locked".into(), "true".into()),
        ]));
        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[
                (
                    ToolRequest::parse("rust@1.91.1").unwrap(),
                    ToolVersion::new("rust", "1.91.1"),
                ),
                (request, version),
            ],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let cargo = &lock.platforms["linux-x64"].tools["cargo:ripgrep"];
        assert_eq!(
            cargo.native,
            Some(LockedNativeTool {
                runtime: "rust".into(),
                runtime_version: "1.91.1".into(),
                replay: NativeReplay::VersionOnly,
                source: Some("sparse+https://index.crates.io/".into()),
                module: None,
            })
        );
        assert_eq!(
            cargo.options,
            BTreeMap::from([("locked".into(), "true".into())])
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("schema = 4"));
        assert!(!text.contains("__osdk_native"));
    }

    #[test]
    fn schema_four_native_metadata_requires_matching_runtime_and_no_other_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let valid = r#"schema = 4

[platforms.linux-x64.tools.go]
request = "1.24.0"
version = "1.24.0"

[platforms.linux-x64.tools."go:example.com/acme/tool"]
request = "1.2.3"
version = "1.2.3"

[platforms.linux-x64.tools."go:example.com/acme/tool".native]
runtime = "go"
runtime_version = "1.24.0"
replay = "version-only"
source = "https://proxy.golang.org"
module = "example.com/acme/tool"
"#;
        std::fs::write(&path, valid).unwrap();
        load(&path).unwrap();

        std::fs::write(
            &path,
            valid.replace(
                "runtime_version = \"1.24.0\"",
                "runtime_version = \"1.23.0\"",
            ),
        )
        .unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("does not match"));

        std::fs::write(
            &path,
            valid.replace("runtime = \"go\"", "runtime = \"rust\""),
        )
        .unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("requires runtime"));

        std::fs::write(
            &path,
            valid.replace(
                "version = \"1.2.3\"\n\n[platforms.linux-x64.tools.\"go:example.com/acme/tool\".native]",
                "version = \"1.2.3\"\nartifact = { url = \"https://example.test/tool\", file_name = \"tool\" }\n\n[platforms.linux-x64.tools.\"go:example.com/acme/tool\".native]",
            ),
        )
        .unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("cannot carry"));
    }

    #[test]
    fn license_consent_is_never_persisted_into_the_lockfile() {
        // A lock file travels to other machines. Recording one person's
        // agreement would accept Google's terms for everyone who replays it.
        let mut version = ToolVersion::new("android-platform-tools", "37.0.1");
        version.options.extend(BTreeMap::from([
            ("accept-licenses".to_string(), "true".to_string()),
            (
                "accept-license".to_string(),
                "android-sdk-license".to_string(),
            ),
            ("channel".to_string(), "beta".to_string()),
        ]));

        let locked = lock_options(&version).unwrap();

        assert!(!locked.contains_key("accept-licenses"));
        assert!(!locked.contains_key("accept-license"));
        // A genuine artifact-selecting option still round-trips.
        assert_eq!(locked.get("channel").map(String::as_str), Some("beta"));
    }

    #[test]
    fn go_native_writer_round_trips_proxy_module_and_public_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let request = ToolRequest::parse(
            "go:example.com/acme/tool/cmd/tool[tags='netgo,sqlite',env='CGO_ENABLED=0']@1.2.3",
        )
        .unwrap();
        let mut version = ToolVersion::new(&request.backend, "1.2.3");
        version.options.extend(request.options.clone());
        version.options.extend(BTreeMap::from([
            (LOCKED_NATIVE_RUNTIME_OPTION.into(), "go".into()),
            (LOCKED_NATIVE_RUNTIME_VERSION_OPTION.into(), "1.24.6".into()),
            (LOCKED_NATIVE_REPLAY_OPTION.into(), "version-only".into()),
            (
                osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION.into(),
                "https://proxy.golang.org".into(),
            ),
            (
                osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION.into(),
                "example.com/acme/tool".into(),
            ),
        ]));
        merge_resolved(
            &path,
            linux(),
            &test_dirs(temp.path()),
            &[
                (
                    ToolRequest::parse("go@1.24.6").unwrap(),
                    ToolVersion::new("go", "1.24.6"),
                ),
                (request, version),
            ],
        )
        .unwrap();

        let lock = load(&path).unwrap();
        let tool = &lock.platforms["linux-x64"].tools["go:example.com/acme/tool/cmd/tool"];
        assert_eq!(tool.options["tags"], "netgo,sqlite");
        assert_eq!(tool.options["env"], "CGO_ENABLED=0");
        assert_eq!(
            tool.native,
            Some(LockedNativeTool {
                runtime: "go".into(),
                runtime_version: "1.24.6".into(),
                replay: NativeReplay::VersionOnly,
                source: Some("https://proxy.golang.org".into()),
                module: Some("example.com/acme/tool".into()),
            })
        );
        let replayed = locked_requests(&path, linux()).unwrap().unwrap();
        let tool = replayed
            .iter()
            .find(|request| request.backend == "go:example.com/acme/tool/cmd/tool")
            .unwrap();
        assert_eq!(
            tool.options[osdk_core::backend::go_package::LOCKED_GO_PROXY_OPTION],
            "https://proxy.golang.org"
        );
        assert_eq!(
            tool.options[osdk_core::backend::go_package::LOCKED_GO_MODULE_OPTION],
            "example.com/acme/tool"
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("__osdk_"));
    }

    #[test]
    fn go_native_lock_rejects_untruthful_replay_proxy_and_module() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let valid = r#"schema = 4

[platforms.linux-x64.tools.go]
request = "1.24"
version = "1.24.6"

[platforms.linux-x64.tools."go:example.com/acme/tool/cmd/tool"]
request = "latest"
version = "1.2.3"

[platforms.linux-x64.tools."go:example.com/acme/tool/cmd/tool".native]
runtime = "go"
runtime_version = "1.24.6"
replay = "version-only"
source = "https://proxy.golang.org"
module = "example.com/acme/tool"
"#;
        std::fs::write(&path, valid).unwrap();
        load(&path).unwrap();

        for invalid in [
            valid.replace("replay = \"version-only\"", "replay = \"floating-ref\""),
            valid.replace("https://proxy.golang.org", "http://evil.example.test"),
            valid.replace(
                "module = \"example.com/acme/tool\"",
                "module = \"example.com/other\"",
            ),
            valid.replace("source = \"https://proxy.golang.org\"\n", ""),
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(load(&path).is_err());
        }
    }

    #[test]
    fn schemas_one_through_three_remain_readable_and_upgrade_on_save() {
        let temp = tempfile::tempdir().unwrap();
        for schema in [1, 3] {
            let path = temp.path().join(format!("schema-{schema}.lock"));
            std::fs::write(
                &path,
                format!(
                    "schema = {schema}\n\n[platforms.linux-x64.tools.node]\nrequest = \"20\"\nversion = \"20.19.0\"\n"
                ),
            )
            .unwrap();
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.schema, schema);
            save(&path, &loaded).unwrap();
            assert_eq!(load(&path).unwrap().schema, 4);
        }

        let path = temp.path().join("schema-2.lock");
        std::fs::write(&path, "schema = 2\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.schema, 2);
        save(&path, &loaded).unwrap();
        assert_eq!(load(&path).unwrap().schema, 4);
    }

    #[test]
    fn schemas_one_through_three_reject_native_tools_without_replay_metadata() {
        let temp = tempfile::tempdir().unwrap();
        for (backend, request, version) in [
            ("cargo:ripgrep", "14", "14.1.1"),
            ("go:example.com/acme/tool", "1", "1.2.3"),
        ] {
            for schema in [1, 2, 3] {
                let path = temp.path().join(format!("native-schema-{schema}.lock"));
                std::fs::write(
                    &path,
                    format!(
                        "schema = {schema}\n\n[platforms.linux-x64.tools.{backend:?}]\nrequest = {request:?}\nversion = {version:?}\n"
                    ),
                )
                .unwrap();
                let error = load(&path).unwrap_err();
                assert!(
                    error.to_string().contains("schema 4"),
                    "{backend}, {schema}: {error}"
                );
            }
        }
    }

    #[test]
    fn schemas_one_and_two_reject_schema_four_metadata_on_other_tools() {
        let temp = tempfile::tempdir().unwrap();
        for schema in [1, 2] {
            let path = temp.path().join(format!("metadata-schema-{schema}.lock"));
            std::fs::write(
                &path,
                format!(
                    r#"schema = {schema}

[platforms.linux-x64.tools.node]
request = "20"
version = "20.19.0"

[platforms.linux-x64.tools.node.native]
runtime = "go"
runtime_version = "1.24.0"
replay = "version-only"
"#
                ),
            )
            .unwrap();
            let error = load(&path).unwrap_err();
            assert!(error.to_string().contains("native metadata"), "{error}");
        }
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
    fn unresolved_catalog_artifact_is_persisted_with_subdir() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let request = ToolRequest::parse("python@pyodide-3.14.2").unwrap();
        let mut version = ToolVersion::new("python", "pyodide-3.14.2");
        version.options.extend(BTreeMap::from([
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://example.test/pyodide.tar.gz".into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "pyodide.tar.gz".into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{}", "a".repeat(64)),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION.into(),
                "pyodide-root/dist".into(),
            ),
            ("catalog-subdir".into(), "pyodide-root/dist".into()),
        ]));
        merge_resolved(&path, linux(), &dirs, &[(request, version)]).unwrap();
        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["python"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.file_name, "pyodide.tar.gz");
        assert_eq!(artifact.subdir.as_deref(), Some("pyodide-root/dist"));
        let restored = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(
            restored[0].options[osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION],
            "pyodide-root/dist"
        );
    }

    #[test]
    fn merge_recovers_unique_fingerprinted_github_receipt_and_replays_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let request = ToolRequest::parse("github:example/tool[rename=tool]@1.2.3").unwrap();
        let mut version = ToolVersion::new(&request.backend, "1.2.3");
        version.options = request.options.clone();
        let checksum = format!("sha256:{}", "a".repeat(64));
        write_github_install_fixture(
            &dirs,
            linux(),
            &version,
            "https://example.test/tool",
            "tool",
            Some(&checksum),
        );

        merge_resolved(&path, linux(), &dirs, &[(request, version)]).unwrap();
        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["github:example/tool"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.url, "https://example.test/tool");
        assert_eq!(artifact.file_name, "tool");
        assert_eq!(artifact.checksum.as_deref(), Some(checksum.as_str()));

        let replayed = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(
            replayed[0].options[osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION],
            "tool"
        );
        assert_eq!(
            replayed[0].options[osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION],
            checksum
        );
    }

    #[test]
    fn merge_recovers_fingerprinted_http_receipt_and_replays_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let digest = "a".repeat(64);
        let request = ToolRequest::parse(&format!(
            "http:https://downloads.example.test/tool-{{version}}[sha256={digest},kind=file,rename=fixture]@1.2.3"
        ))
        .unwrap();
        let mut version = ToolVersion::new(&request.backend, "1.2.3");
        version.options = request.options.clone();
        let receipt_url = "https://downloads.example.test/tool-1.2.3";
        let mut locked = version.clone();
        locked.options.extend(BTreeMap::from([
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                receipt_url.into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "tool-1.2.3".into(),
            ),
            (
                osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{digest}"),
            ),
        ]));
        let locator = osdk_core::backend::http::HttpBackend::install_locator_for(
            &dirs,
            linux(),
            &request.backend,
            &locked,
        )
        .unwrap();
        let root = locator.install_root();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/fixture"), b"fixture").unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(locator.identity().clone())
                .unwrap();
        manifest.bins.push(osdk_core::inventory::DynamicToolBin {
            name: "fixture".into(),
            path: "bin/fixture".into(),
            ..Default::default()
        });
        manifest.write_atomic(root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&osdk_core::pipeline::ArtifactReceipt {
                url: receipt_url.into(),
                file_name: "tool-1.2.3".into(),
                checksum: Some(format!("sha256:{digest}")),
                evidence: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();

        merge_resolved(&path, linux(), &dirs, &[(request, version)]).unwrap();
        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools
            ["http:https://downloads.example.test/tool-{version}"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.url, receipt_url);
        assert_eq!(
            artifact.checksum.as_deref(),
            Some(format!("sha256:{digest}").as_str())
        );
        let replayed = locked_requests(&path, linux()).unwrap().unwrap();
        assert_eq!(
            replayed[0].options[osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION],
            receipt_url
        );
    }

    #[test]
    fn upsert_rejects_ambiguous_fingerprinted_github_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let request = ToolRequest::parse("github:example/tool@1.2.3").unwrap();
        let version = ToolVersion::new(&request.backend, "1.2.3");
        for suffix in ["a", "b"] {
            write_github_install_fixture(
                &dirs,
                linux(),
                &version,
                &format!("https://example.test/tool-{suffix}"),
                "tool",
                None,
            );
        }

        let error = upsert_resolved_with_scope(
            &path,
            linux(),
            &dirs,
            &request,
            &version,
            LockScope::Project,
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple complete GitHub"));
        assert!(!path.exists());
    }

    #[test]
    fn linked_rust_toolchain_is_rejected_from_reproducible_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let marker = dirs.install_path("rust", "local-dev");
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join(".osdk-linked"), "/local/toolchain").unwrap();
        let error = merge_resolved(
            &path,
            linux(),
            &dirs,
            &[(
                ToolRequest::parse("rust@local-dev").unwrap(),
                ToolVersion::new("rust", "local-dev"),
            )],
        )
        .unwrap_err();
        assert!(error.to_string().contains("local-only"));
        assert!(!path.exists());
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
subdir = "install"
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
        assert_eq!(
            lock.platforms["linux-x64"].tools["github:cli/cli"]
                .artifact
                .as_ref()
                .unwrap()
                .subdir
                .as_deref(),
            Some("install")
        );
    }

    #[test]
    fn lock_persists_verification_evidence_without_trusting_it_as_an_option() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCKFILE_NAME);
        let dirs = test_dirs(temp.path());
        let request = ToolRequest::parse("github:cli/cli@2.96.0").unwrap();
        let version = ToolVersion::new("github:cli/cli", "2.96.0");
        let install = write_github_install_fixture(
            &dirs,
            linux(),
            &version,
            "https://example.test/gh.tar.gz",
            "gh.tar.gz",
            Some("sha256:00"),
        );
        let mut receipt = osdk_core::pipeline::artifact_receipt_at(&install).unwrap();
        receipt
            .evidence
            .push(osdk_core::verification::VerificationEvidence {
                kind: "sigstore-bundle+rekor".into(),
                repository: "cli/cli".into(),
                issuer: "https://token.actions.githubusercontent.com".into(),
                digest: "sha256:00".into(),
            });
        std::fs::write(
            install.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        merge_resolved(&path, linux(), &dirs, &[(request, version)]).unwrap();

        let lock = load(&path).unwrap();
        let artifact = lock.platforms["linux-x64"].tools["github:cli/cli"]
            .artifact
            .as_ref()
            .unwrap();
        assert_eq!(artifact.evidence.len(), 1);
        assert_eq!(artifact.evidence[0].kind, "sigstore-bundle+rekor");
        assert_eq!(artifact.evidence[0].repository, "cli/cli");

        let requests = locked_requests(&path, linux()).unwrap().unwrap();
        assert!(requests[0]
            .options
            .keys()
            .all(|key| !key.contains("evidence")));
    }

    fn valid_schema_two_npm_lock() -> String {
        valid_schema_two_npm_lock_with_sha256(&"a".repeat(64))
    }

    fn valid_schema_two_npm_lock_with_sha256(sha256: &str) -> String {
        format!(
            r#"
schema = 2

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "package-lock-v3"
sha256 = "{sha256}"
graph = "osdk.lock.d/npm/{sha256}.yaml"
"#
        )
    }

    fn valid_schema_three_npm_lock() -> String {
        let sha256 = "a".repeat(64);
        format!(
            r#"
schema = 3

[platforms.linux-x64.tools.node]
request = "24"
version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "npm"
scope = "project"
node_version = "24.1.0"

[platforms.linux-x64.tools."npm:prettier".npm.native_lock]
kind = "npm"
format = "package-lock-v3"
sha256 = "{sha256}"
"#
        )
    }

    fn test_model_manifest() -> osdk_core::model::SnapshotManifest {
        osdk_core::model::SnapshotManifest {
            schema: 1,
            name: "qwen".into(),
            provider: osdk_core::model::ProviderId::HuggingFace,
            repository: "Qwen/Qwen2.5-7B-Instruct".into(),
            requested_revision: "main".into(),
            revision: "abc123".into(),
            endpoint: "https://huggingface.co".into(),
            variant: Some("safetensors".into()),
            files: vec![osdk_core::model::ModelFile {
                path: "config.json".into(),
                size: 10,
                cas_hash: "blake3".into(),
                sha256: Some("sha256".into()),
                etag: None,
            }],
            created_at: 0,
        }
    }

    fn test_locked_model() -> LockedModel {
        LockedModel {
            provider: osdk_core::model::ProviderId::HuggingFace,
            repository: "Qwen/Qwen2.5-7B-Instruct".into(),
            requested_revision: "main".into(),
            revision: "abc123".into(),
            endpoint: "https://huggingface.co".into(),
            variant: Some("safetensors".into()),
            files: vec![LockedModelFile {
                path: "config.json".into(),
                size: 10,
                sha256: "sha256".into(),
            }],
            views: BTreeMap::new(),
        }
    }

    /// Materialize a conda prefix the way an install leaves it: under the
    /// install-id directory, with the manifest recording the solved closure.
    ///
    /// The flat `<installs>/<tool>/<version>` path is deliberately left empty --
    /// that is exactly where the lock writer used to look, and finding nothing
    /// there is what made it record a version and nothing else.
    fn write_conda_install_fixture(
        dirs: &osdk_core::dirs::Dirs,
        platform: Platform,
        version: &ToolVersion,
        digest: &str,
        packages: usize,
    ) {
        let identity = osdk_core::tool::InstallIdentity::new(
            &version.backend,
            version.version.clone(),
            platform.to_string(),
            osdk_core::tool::InstallScope::Isolated,
            &version.options,
            Vec::new(),
            std::collections::BTreeMap::from([
                (
                    "artifact-file".to_string(),
                    format!("conda-closure-{packages}.json"),
                ),
                ("artifact-checksum".to_string(), digest.to_string()),
            ]),
        )
        .unwrap();
        let locator = osdk_core::dirs::InstallLocator::new(dirs, identity.clone()).unwrap();
        let root = locator.install_root().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        let manifest = osdk_core::inventory::DynamicToolManifest {
            schema: 1,
            identity,
            bins: Vec::new(),
        };
        std::fs::write(
            osdk_core::inventory::DynamicToolManifest::manifest_path(&root),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn the_models_section_can_be_read_back_and_pruned() {
        // The section had one writer and no readers outside tests: `model pull`
        // recorded a snapshot and nothing ever consulted it, so a committed lock
        // described a state no command could restore.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("osdk.lock");

        // Nothing on disk is an empty declaration, not an error: `sync` may run
        // in a project that has never pulled a model.
        assert!(locked_models(&path).unwrap().is_empty());
        assert!(!remove_model(&path, "qwen").unwrap());

        merge_model(&path, &test_model_manifest()).unwrap();
        let models = locked_models(&path).unwrap();
        assert_eq!(models.len(), 1);
        let (name, entry) = &models[0];
        assert_eq!(name, "qwen");
        // The immutable revision is what a restore must replay -- replaying
        // `requested_revision` would resolve a branch to whatever it points at
        // now, which is the opposite of what a lock is for.
        assert_eq!(entry.revision, "abc123");
        assert_eq!(entry.requested_revision, "main");
        assert_eq!(entry.files.len(), 1);
        assert!(!entry.files[0].sha256.is_empty());

        // Pruning one entry must not disturb the rest of the file.
        let mut lockfile = load(&path).unwrap();
        lockfile.platforms.insert(
            platform_key(linux()),
            PlatformLock {
                tools: BTreeMap::from([(
                    "go".to_string(),
                    LockedTool {
                        version: "1.26.5".into(),
                        ..Default::default()
                    },
                )]),
            },
        );
        lockfile.models.insert("other".into(), test_locked_model());
        save(&path, &lockfile).unwrap();

        assert!(remove_model(&path, "qwen").unwrap());
        let after = load(&path).unwrap();
        assert!(!after.models.contains_key("qwen"));
        assert!(
            after.models.contains_key("other"),
            "pruning one model removed another"
        );
        assert!(
            after.platforms[&platform_key(linux())]
                .tools
                .contains_key("go"),
            "pruning a model removed a platform's tools"
        );
        // Removing an entry that is already gone is not an error, so `model
        // remove` need not care whether the lock still mentioned it.
        assert!(!remove_model(&path, "qwen").unwrap());
    }

    #[test]
    fn a_mirror_url_is_locked_as_its_upstream() {
        // The receipt on disk records the host that actually served the bytes,
        // which on a mirrored network is a mirror. The lock is committed and
        // replayed elsewhere, so it must name the upstream instead -- otherwise
        // one machine's fastest host becomes everybody's locked source, including
        // for people who cannot reach it.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("osdk.lock");
        let dirs = test_dirs(temporary.path());
        let platform = linux();

        let mut version = ToolVersion::new("go", "1.26.5");
        version.options.insert(
            osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.to_string(),
            "https://golang.google.cn/dl/go1.26.5.linux-amd64.tar.gz".to_string(),
        );
        version.options.insert(
            osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.to_string(),
            "go1.26.5.linux-amd64.tar.gz".to_string(),
        );
        version.options.insert(
            osdk_core::pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.to_string(),
            "sha256:abc".to_string(),
        );
        let request = ToolRequest {
            backend: "go".into(),
            spec: osdk_core::version::VersionSpec::Exact("1.26.5".into()),
            options: Default::default(),
        };

        merge_resolved(&path, platform, &dirs, &[(request, version)]).unwrap();

        let artifact = load(&path).unwrap().platforms[&platform_key(platform)].tools["go"]
            .artifact
            .clone()
            .expect("a go entry carries its artifact");
        assert_eq!(
            artifact.url, "https://go.dev/dl/go1.26.5.linux-amd64.tar.gz",
            "the lock kept this machine's mirror instead of the upstream"
        );
        // The checksum is deliberately untouched: mirrors serve the same bytes,
        // and if one does not, that is what the checksum is for.
        assert_eq!(artifact.checksum.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn a_custom_source_url_is_locked_unchanged() {
        // osdk cannot know which upstream a user's own host corresponds to, so
        // rewriting it would put an invented origin into a committed file. This
        // is the other direction of the same rule, and the one an over-eager
        // prefix match would break.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("osdk.lock");
        let dirs = test_dirs(temporary.path());
        let platform = linux();

        let mut version = ToolVersion::new("go", "1.26.5");
        version.options.insert(
            osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.to_string(),
            "https://nexus.internal/golang/go1.26.5.linux-amd64.tar.gz".to_string(),
        );
        version.options.insert(
            osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.to_string(),
            "go1.26.5.linux-amd64.tar.gz".to_string(),
        );
        let request = ToolRequest {
            backend: "go".into(),
            spec: osdk_core::version::VersionSpec::Exact("1.26.5".into()),
            options: Default::default(),
        };
        merge_resolved(&path, platform, &dirs, &[(request, version)]).unwrap();
        assert_eq!(
            load(&path).unwrap().platforms[&platform_key(platform)].tools["go"]
                .artifact
                .as_ref()
                .unwrap()
                .url,
            "https://nexus.internal/golang/go1.26.5.linux-amd64.tar.gz"
        );
    }

    #[test]
    fn conda_lock_records_the_solved_closure_not_just_the_version() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("osdk.lock");
        let dirs = test_dirs(temporary.path());
        let platform = linux();
        let digest = format!("blake3:{}", "ab".repeat(32));

        let request = ToolRequest {
            backend: "conda:ninja".into(),
            spec: osdk_core::version::VersionSpec::Exact("1.13.2".into()),
            options: Default::default(),
        };
        let version = ToolVersion::new("conda:ninja", "1.13.2");
        write_conda_install_fixture(&dirs, platform, &version, &digest, 5);

        merge_resolved(
            &path,
            platform,
            &dirs,
            std::slice::from_ref(&(request, version)),
        )
        .unwrap();

        let locked =
            load(&path).unwrap().platforms[&platform_key(platform)].tools["conda:ninja"].clone();
        let conda = locked.conda.expect(
            "a conda entry must carry its solved closure; version alone pins a request, not an environment",
        );
        assert_eq!(conda.closure, digest);
        assert_eq!(conda.packages, 5);
    }

    #[test]
    fn conda_lock_omits_the_closure_when_nothing_is_installed() {
        // `lock` may legitimately run before install. Recording a placeholder
        // digest would be worse than recording none: it would claim a closure
        // that was never observed.
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("osdk.lock");
        let dirs = test_dirs(temporary.path());
        let platform = linux();
        let request = ToolRequest {
            backend: "conda:ninja".into(),
            spec: osdk_core::version::VersionSpec::Exact("1.13.2".into()),
            options: Default::default(),
        };
        merge_resolved(
            &path,
            platform,
            &dirs,
            &[(request, ToolVersion::new("conda:ninja", "1.13.2"))],
        )
        .unwrap();
        assert!(
            load(&path).unwrap().platforms[&platform_key(platform)].tools["conda:ninja"]
                .conda
                .is_none()
        );
    }

    #[test]
    fn conda_closure_metadata_is_rejected_where_it_cannot_belong() {
        let digest = format!("blake3:{}", "cd".repeat(32));
        let valid = |backend: &str, closure: &str, packages: usize| {
            let mut lockfile = Lockfile::default();
            lockfile.platforms.insert(
                platform_key(linux()),
                PlatformLock {
                    tools: BTreeMap::from([(
                        backend.to_string(),
                        LockedTool {
                            version: "1.13.2".into(),
                            conda: Some(LockedCondaTool {
                                closure: closure.to_string(),
                                packages,
                            }),
                            ..Default::default()
                        },
                    )]),
                },
            );
            validate_schema_four(&lockfile)
        };

        assert!(valid("conda:ninja", &digest, 5).is_ok());
        // A closure section on a tool that has no solve is meaningless.
        assert!(valid("go", &digest, 5)
            .unwrap_err()
            .to_string()
            .contains("non-conda entry"));
        // Only the digest the backend actually computes can be compared later.
        assert!(valid("conda:ninja", "sha256:abcd", 5)
            .unwrap_err()
            .to_string()
            .contains("blake3:<hex>"));
        assert!(valid("conda:ninja", "blake3:xyz", 5)
            .unwrap_err()
            .to_string()
            .contains("64-character hex"));
        assert!(valid("conda:ninja", &digest, 0)
            .unwrap_err()
            .to_string()
            .contains("empty closure"));

        // Older schemas predate the section entirely.
        let old = Lockfile {
            schema: 3,
            platforms: BTreeMap::from([(
                platform_key(linux()),
                PlatformLock {
                    tools: BTreeMap::from([(
                        "conda:ninja".to_string(),
                        LockedTool {
                            version: "1.13.2".into(),
                            conda: Some(LockedCondaTool {
                                closure: digest.clone(),
                                packages: 5,
                            }),
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            models: BTreeMap::new(),
            deps: BTreeMap::new(),
            skills: BTreeMap::new(),
        };
        assert!(validate_schema_three(&old)
            .unwrap_err()
            .to_string()
            .contains("cannot carry conda closure metadata"));
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

    fn write_github_install_fixture(
        dirs: &osdk_core::dirs::Dirs,
        platform: Platform,
        version: &ToolVersion,
        url: &str,
        file_name: &str,
        checksum: Option<&str>,
    ) -> PathBuf {
        let mut materials = BTreeMap::from([("artifact-file".into(), file_name.into())]);
        if let Some(checksum) = checksum {
            materials.insert("artifact-checksum".into(), checksum.into());
        } else {
            let mut locked_version = version.clone();
            locked_version.options.insert(
                osdk_core::pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                url.into(),
            );
            locked_version.options.insert(
                osdk_core::pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.into(),
            );
            materials = osdk_core::backend::github::github_install_locator_for(
                dirs,
                platform,
                &version.backend,
                &locked_version,
            )
            .unwrap()
            .identity()
            .materials
            .clone();
            assert!(!materials.contains_key("artifact-url"));
            assert!(materials.contains_key("artifact-url-blake3"));
            assert!(!materials.values().any(|value| value == url));
        }
        let identity = osdk_core::tool::InstallIdentity::new(
            &version.backend,
            &version.version,
            platform.to_string(),
            osdk_core::tool::InstallScope::Isolated,
            &version.options,
            Vec::new(),
            materials,
        )
        .unwrap();
        let locator = osdk_core::dirs::InstallLocator::new(dirs, identity.clone()).unwrap();
        let root = locator.install_root().to_path_buf();
        let bin = root.join("bin/tool");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fixture").unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins.push(osdk_core::inventory::DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
            ..Default::default()
        });
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&osdk_core::pipeline::ArtifactReceipt {
                url: url.into(),
                file_name: file_name.into(),
                checksum: checksum.map(str::to_owned),
                evidence: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        root
    }
}
