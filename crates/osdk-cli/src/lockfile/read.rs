//! `read` free functions split from lockfile.rs.

use super::*;

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn schema_version() -> u32 {
    4
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
    let bytes = read_bounded(path, MAX_LOCKFILE_BYTES)
        .with_context(|| format!("reading lockfile {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| osdk_core::t!("err.lockfile_not_utf8", path = path.display()))?;
    let lockfile: Lockfile =
        toml::from_str(&text).with_context(|| format!("parsing lockfile {}", path.display()))?;
    if !(1..=schema_version()).contains(&lockfile.schema) {
        anyhow::bail!(
            "unsupported lockfile schema {} in {}",
            lockfile.schema,
            path.display()
        );
    }
    match lockfile.schema {
        1 => {
            reject_conda_metadata_before_schema_four(&lockfile)?;
            reject_native_backends_before_schema_four(&lockfile)?;
            reject_native_metadata_before_schema_four(&lockfile)?;
        }
        2 => validate_schema_two(path, &lockfile, false)?,
        3 => validate_schema_three(&lockfile)?,
        4 => validate_schema_four(&lockfile)?,
        _ => {}
    }
    Ok(lockfile)
}

/// The models a project's lock declares, as replayable references.
///
/// The `[models]` section had exactly one writer and no readers outside tests:
/// `model pull` recorded a snapshot and nothing ever consulted it again. A lock
/// that cannot be read back is not a lock -- it is a log. This is the reader, and
/// `model sync` is what acts on it.
///
/// Returns the logical name alongside the reference so a caller can report which
/// entry it is working on, and the file digests so a restore can be verified
/// against what was committed rather than against whatever the provider serves
/// today.
pub fn locked_models(path: &Path) -> Result<Vec<(String, LockedModel)>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    Ok(load(path)?.models.into_iter().collect())
}

pub fn locked_requests(path: &Path, platform: Platform) -> Result<Option<Vec<ToolRequest>>> {
    let lockfile = load(path)?;
    if lockfile.schema == 1 {
        reject_legacy_npm_entries(&lockfile)?;
    }
    let Some(platform_lock) = lockfile.platforms.get(&platform_key(platform)) else {
        return Ok(None);
    };
    let requests = platform_lock
        .tools
        .iter()
        .map(|(backend, locked)| {
            validate_locked_tool_identity(backend, locked)?;
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
                if let Some(subdir) = &artifact.subdir {
                    options.insert(
                        osdk_core::pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION.into(),
                        subdir.clone(),
                    );
                }
            }
            if let Some(npm) = &locked.npm {
                match npm {
                    LockedNpmGraph::Metadata(npm) => inject_npm_metadata(&mut options, npm),
                    LockedNpmGraph::Sidecar(npm) => {
                        let lockfile = read_npm_graph_sidecar(path, backend, npm)?;
                        options.insert(LOCKED_NPM_PACKAGE_OPTION.into(), npm.package.clone());
                        options.insert(
                            LOCKED_NPM_LOCK_FORMAT_OPTION.into(),
                            npm.lock_format.clone(),
                        );
                        options.insert(LOCKED_NPM_LOCK_SHA256_OPTION.into(), npm.sha256.clone());
                        options.insert(LOCKED_NPM_LOCKFILE_OPTION.into(), lockfile);
                        options.insert(
                            LOCKED_NPM_NODE_VERSION_OPTION.into(),
                            npm.node_version.clone(),
                        );
                    }
                    LockedNpmGraph::Legacy(_) => {
                        npm.sidecar(backend)?;
                        unreachable!("legacy npm metadata is rejected above")
                    }
                }
            }
            if let Some(native) = &locked.native {
                inject_native_metadata(backend, &mut options, native);
            }
            if let Some(pypi) = &locked.pypi {
                // Carried into the request so the backend can honour the
                // recorded resolver. Without this the entry replays with
                // whatever installer the new machine happens to have, which is
                // exactly the divergence the field was added to prevent.
                options.insert(
                    LOCKED_PYPI_INSTALLER_OPTION.into(),
                    pypi.installer.as_str().into(),
                );
                if let Some(uv_version) = &pypi.uv_version {
                    options.insert(LOCKED_PYPI_UV_VERSION_OPTION.into(), uv_version.clone());
                }
                if let Some(python_version) = &pypi.python_version {
                    options.insert(
                        LOCKED_PYPI_PYTHON_VERSION_OPTION.into(),
                        python_version.clone(),
                    );
                }
            }
            Ok(ToolRequest {
                backend: backend.clone(),
                spec: VersionSpec::Exact(locked.version.clone()),
                options,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(requests))
}

pub(crate) fn validate_schema_four(lockfile: &Lockfile) -> Result<()> {
    validate_schema_three_npm(lockfile)?;
    for (platform, platform_lock) in &lockfile.platforms {
        for (backend, locked) in &platform_lock.tools {
            let native_namespace = native_runtime_for_backend(backend);
            match (native_namespace, locked.native.as_ref()) {
                (Some(expected_runtime), Some(native)) => {
                    if locked.artifact.is_some() || locked.npm.is_some() {
                        anyhow::bail!(
                            "schema 4 native entry `{backend}` for platform `{platform}` cannot carry artifact or npm metadata"
                        );
                    }
                    if native.runtime != expected_runtime {
                        anyhow::bail!(
                            "schema 4 native entry `{backend}` requires runtime `{expected_runtime}`, got `{}`",
                            native.runtime
                        );
                    }
                    validate_version_identity(&native.runtime, &native.runtime_version)?;
                    if expected_runtime == "rust" {
                        validate_exact_rust_version(backend, &native.runtime_version)?;
                        validate_cargo_native_replay(backend, locked, native)?;
                        match (backend.starts_with("cargo:https://"), native.source.as_deref()) {
                            (true, Some(_)) => anyhow::bail!(
                                "schema 4 Cargo Git entry `{backend}` cannot carry a registry source"
                            ),
                            (false, Some(source)) => {
                                validate_cargo_registry_source(backend, source)?
                            }
                            _ => {}
                        }
                        if native.module.is_some() {
                            anyhow::bail!(
                                "schema 4 Cargo entry `{backend}` cannot carry a Go module root"
                            );
                        }
                    } else {
                        validate_exact_go_version(backend, &native.runtime_version)?;
                        validate_go_native_replay(backend, locked, native)?;
                    }
                    let runtime = platform_lock.tools.get(expected_runtime).ok_or_else(|| {
                        anyhow::anyhow!(
                            "schema 4 native entry `{backend}` for platform `{platform}` requires `{expected_runtime}` in the same platform lock"
                        )
                    })?;
                    if runtime.version != native.runtime_version {
                        anyhow::bail!(
                            "schema 4 native entry `{backend}` runtime version `{}` does not match `{expected_runtime}` entry `{}`",
                            native.runtime_version,
                            runtime.version
                        );
                    }
                    if let Some(key) = locked.options.keys().find(|key| {
                        key.starts_with("__osdk_")
                    }) {
                        anyhow::bail!(
                            "schema 4 native entry `{backend}` cannot persist internal option `{key}`"
                        );
                    }
                }
                (Some(_), None) => anyhow::bail!(
                    "schema 4 native entry `{backend}` for platform `{platform}` is missing native replay metadata"
                ),
                (None, Some(_)) => anyhow::bail!(
                    "schema 4 non-native entry `{backend}` for platform `{platform}` cannot carry native replay metadata"
                ),
                (None, None) => {}
            }
            validate_locked_conda(backend, locked)?;
        }
    }
    Ok(())
}

/// A conda section belongs only to a `conda:` entry, and only in a shape that can
/// actually be compared against a future solve.
pub(crate) fn validate_locked_conda(backend: &str, locked: &LockedTool) -> Result<()> {
    let Some(conda) = locked.conda.as_ref() else {
        return Ok(());
    };
    if !backend.starts_with("conda:") {
        anyhow::bail!("non-conda entry `{backend}` cannot carry conda closure metadata");
    }
    // Only blake3 is accepted: this is the digest the backend computes to pick a
    // prefix, so a lock naming another algorithm could never be compared against
    // an install and would silently verify nothing.
    let Some(hex) = conda.closure.strip_prefix("blake3:") else {
        anyhow::bail!(
            "conda entry `{backend}` closure digest must be `blake3:<hex>`, got `{}`",
            conda.closure
        );
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("conda entry `{backend}` closure digest is not a 64-character hex blake3");
    }
    if conda.packages == 0 {
        anyhow::bail!("conda entry `{backend}` cannot record an empty closure");
    }
    Ok(())
}

/// Older schemas predate the conda closure section, so a file claiming one is
/// either hand-edited or written by a newer osdk that lied about its schema.
pub(crate) fn reject_conda_metadata_before_schema_four(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock
            .tools
            .iter()
            .find(|(_, locked)| locked.conda.is_some())
        {
            anyhow::bail!(
                "lock schema {} entry `{backend}` for platform `{platform}` cannot carry conda closure metadata",
                lockfile.schema
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_schema_three(lockfile: &Lockfile) -> Result<()> {
    validate_schema_three_npm(lockfile)?;
    reject_conda_metadata_before_schema_four(lockfile)?;
    reject_native_backends_before_schema_four(lockfile)?;
    reject_native_metadata_before_schema_four(lockfile)
}

pub(crate) fn validate_schema_three_npm(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        let node = platform_lock.tools.get("node");
        for (backend, locked) in &platform_lock.tools {
            validate_locked_tool_identity(backend, locked)?;
            match (backend.strip_prefix("npm:"), locked.npm.as_ref()) {
                (Some(package), Some(LockedNpmGraph::Metadata(npm))) => {
                    if locked.artifact.is_some() {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema3_npm_artifact_forbidden",
                            backend = backend,
                            platform = platform
                        ));
                    }
                    if let Some(key) = locked.options.keys().find(|key| {
                        key.starts_with("__osdk_npm_")
                            || key.as_str() == LOCKED_NPM_NODE_VERSION_OPTION
                    }) {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema3_npm_private_option_forbidden",
                            backend = backend,
                            platform = platform,
                            key = key
                        ));
                    }
                    if package.is_empty() || npm.package != package {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_package_mismatch",
                            backend = backend,
                            platform = platform,
                            expected = package,
                            actual = npm.package
                        ));
                    }
                    if let Some(node_version) = &npm.node_version {
                        validate_exact_node_version(backend, node_version)?;
                        if let Some(node) = node {
                            if node_version != &node.version {
                                anyhow::bail!(osdk_core::t!(
                                    "err.lock_npm_node_version_mismatch",
                                    backend = backend,
                                    platform = platform,
                                    expected = node.version,
                                    actual = node_version
                                ));
                            }
                        }
                    }
                    if let Some(native_lock) = &npm.native_lock {
                        validate_native_lock(backend, native_lock)?;
                    }
                }
                (Some(_), None) => anyhow::bail!(osdk_core::t!(
                    "err.lock_schema3_npm_metadata_missing",
                    backend = backend,
                    platform = platform
                )),
                (Some(_), Some(_)) => anyhow::bail!(osdk_core::t!(
                    "err.lock_schema3_npm_legacy_metadata",
                    backend = backend,
                    platform = platform
                )),
                (None, Some(_)) => anyhow::bail!(osdk_core::t!(
                    "err.lock_non_npm_graph_metadata",
                    backend = backend,
                    platform = platform
                )),
                (None, None) => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn reject_native_metadata_before_schema_four(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock
            .tools
            .iter()
            .find(|(_, locked)| locked.native.is_some())
        {
            anyhow::bail!(
                "lock schema {} entry `{backend}` for platform `{platform}` cannot carry schema 4 native metadata",
                lockfile.schema
            );
        }
    }
    Ok(())
}

pub(crate) fn native_runtime_for_backend(backend: &str) -> Option<&'static str> {
    let id = osdk_core::tool::ToolId::parse(backend).ok()?;
    if id.to_string() != backend {
        return None;
    }
    match id.namespace() {
        Some("cargo") => Some("rust"),
        Some("go") => Some("go"),
        _ => None,
    }
}

pub(crate) fn has_native_prefix(backend: &str) -> bool {
    backend.starts_with("cargo:") || backend.starts_with("go:")
}

pub(crate) fn validate_native_lock(backend: &str, native_lock: &LockedNativeLock) -> Result<()> {
    let valid_format = match native_lock.kind {
        NpmInstaller::Npm => matches!(
            native_lock.format.as_str(),
            "package-lock-v2" | "package-lock-v3" | "npm-shrinkwrap-v2" | "npm-shrinkwrap-v3"
        ),
        NpmInstaller::Pnpm => native_lock.format == "pnpm-v9",
    };
    if !valid_format {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_native_format_unsupported",
            format = native_lock.format,
            backend = backend,
            owner = native_lock.kind.as_str()
        ));
    }
    validate_sha256(backend, &native_lock.sha256)
}

pub(crate) fn validate_schema_two(
    path: &Path,
    lockfile: &Lockfile,
    read_graphs: bool,
) -> Result<()> {
    validate_complete_npm_entries(path, lockfile, read_graphs)?;
    reject_conda_metadata_before_schema_four(lockfile)?;
    reject_native_backends_before_schema_four(lockfile)?;
    reject_native_metadata_before_schema_four(lockfile)
}

pub(crate) fn reject_native_backends_before_schema_four(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock
            .tools
            .iter()
            .find(|(backend, _)| has_native_prefix(backend))
        {
            anyhow::bail!(
                "lock schema {} cannot represent native tool `{backend}` for platform `{platform}`; regenerate it as schema 4",
                lockfile.schema
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_complete_npm_entries(
    path: &Path,
    lockfile: &Lockfile,
    read_graphs: bool,
) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        let node = platform_lock.tools.get("node");
        for (backend, locked) in &platform_lock.tools {
            validate_locked_tool_identity(backend, locked)?;
            match (backend.strip_prefix("npm:"), locked.npm.as_ref()) {
                (Some(package), Some(LockedNpmGraph::Sidecar(npm))) => {
                    if locked.artifact.is_some() {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_schema2_npm_artifact_forbidden",
                            backend = backend,
                            platform = platform
                        ));
                    }
                    if package.is_empty() || npm.package != package {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_package_mismatch",
                            backend = backend,
                            platform = platform,
                            expected = package,
                            actual = npm.package
                        ));
                    }
                    if npm.lock_format != NPM_LOCK_FORMAT {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_graph_format_unsupported",
                            format = npm.lock_format,
                            backend = backend,
                            platform = platform,
                            expected = NPM_LOCK_FORMAT
                        ));
                    }
                    validate_sha256(backend, &npm.sha256)?;
                    let expected_graph = npm_graph_relative_path(&npm.sha256);
                    if npm.graph != expected_graph {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_graph_path_unsafe_platform",
                            graph = npm.graph,
                            backend = backend,
                            platform = platform,
                            expected = expected_graph
                        ));
                    }
                    let node = node.ok_or_else(|| {
                        anyhow::anyhow!(osdk_core::t!(
                            "err.lock_npm_node_entry_required",
                            backend = backend,
                            platform = platform
                        ))
                    })?;
                    if npm.node_version != node.version {
                        anyhow::bail!(osdk_core::t!(
                            "err.lock_npm_node_version_mismatch",
                            backend = backend,
                            platform = platform,
                            expected = node.version,
                            actual = npm.node_version
                        ));
                    }
                    if read_graphs {
                        read_npm_graph_sidecar(path, backend, npm)?;
                    }
                }
                (Some(_), None) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_npm_graph_missing",
                        backend = backend,
                        platform = platform
                    ));
                }
                (Some(_), Some(LockedNpmGraph::Legacy(_))) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_schema2_npm_legacy_inline",
                        backend = backend,
                        platform = platform
                    ));
                }
                (Some(_), Some(LockedNpmGraph::Metadata(_))) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_schema2_npm_schema3_metadata",
                        backend = backend,
                        platform = platform
                    ));
                }
                (None, Some(_)) => {
                    anyhow::bail!(osdk_core::t!(
                        "err.lock_non_npm_graph_metadata",
                        backend = backend,
                        platform = platform
                    ));
                }
                (None, None) => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_sha256(backend: &str, sha256: &str) -> Result<()> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_graph_sha256_invalid",
            backend = backend,
            sha256 = sha256
        ));
    }
    Ok(())
}

pub(crate) fn npm_graph_relative_path(sha256: &str) -> String {
    format!("{NPM_GRAPH_DIRECTORY}/{sha256}.yaml")
}

pub(crate) fn npm_graph_path(lock_path: &Path, graph: &str) -> PathBuf {
    lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(graph)
}

pub(crate) fn read_npm_graph_sidecar(
    lock_path: &Path,
    backend: &str,
    npm: &LockedNpmSidecar,
) -> Result<String> {
    validate_sha256(backend, &npm.sha256)?;
    let expected_graph = npm_graph_relative_path(&npm.sha256);
    if npm.graph != expected_graph {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_graph_path_unsafe",
            graph = npm.graph,
            backend = backend,
            expected = expected_graph
        ));
    }
    let graph_path = npm_graph_path(lock_path, &npm.graph);
    reject_symlinked_graph_path(lock_path, &graph_path)?;
    let bytes = read_bounded(&graph_path, MAX_NPM_GRAPH_BYTES)
        .with_context(|| osdk_core::t!("err.lock_npm_sidecar_read", path = graph_path.display()))?;
    let actual = osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256);
    if actual != npm.sha256 {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_sidecar_checksum_mismatch",
            backend = backend,
            expected = npm.sha256,
            actual = actual
        ));
    }
    String::from_utf8(bytes).with_context(|| {
        osdk_core::t!("err.lock_npm_sidecar_not_utf8", path = graph_path.display())
    })
}

pub(crate) fn reject_symlinked_graph_path(lock_path: &Path, graph_path: &Path) -> Result<()> {
    let parent = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut current = parent.to_path_buf();
    for component in ["osdk.lock.d", "npm"] {
        current.push(component);
        if std::fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_npm_graph_path_symlink",
                path = current.display()
            ));
        }
    }
    if std::fs::symlink_metadata(graph_path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_sidecar_symlink",
            path = graph_path.display()
        ));
    }
    Ok(())
}

pub(crate) fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > maximum {
        anyhow::bail!(osdk_core::t!(
            "err.file_size_limit_exceeded",
            maximum = maximum
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        anyhow::bail!(osdk_core::t!(
            "err.file_size_limit_exceeded",
            maximum = maximum
        ));
    }
    Ok(bytes)
}

pub(crate) fn validate_locked_tool_identity(backend: &str, locked: &LockedTool) -> Result<()> {
    let dynamic = backend.contains(':');
    let canonical_dynamic = dynamic
        && ((has_native_prefix(backend) && native_runtime_for_backend(backend).is_some())
            || osdk_core::tool::ToolId::parse(backend)
                .is_ok_and(|id| id.is_dynamic() && id.to_string() == backend));
    if (dynamic && !canonical_dynamic)
        || (!dynamic
            && (backend.trim().is_empty()
                || backend.split(['/', '\\']).any(|part| {
                    part.is_empty()
                        || part == "."
                        || part == ".."
                        || part.chars().any(char::is_whitespace)
                })))
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_backend_id_unsafe",
            backend = backend
        ));
    }
    validate_version_identity(backend, &locked.version)?;
    if let Ok(id) = osdk_core::tool::ToolId::parse(backend) {
        if id.namespace() == Some("go") {
            let canonical = osdk_core::tool::canonicalize_dynamic_options(&id, &locked.options)?;
            if canonical.as_map() != &locked.options {
                anyhow::bail!("lock entry `{backend}` contains non-canonical options");
            }
        }
    }
    if let Some(artifact) = &locked.artifact {
        let file = std::path::Path::new(&artifact.file_name);
        if file.components().count() != 1
            || !matches!(
                file.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_artifact_filename_unsafe",
                file_name = artifact.file_name,
                backend = backend
            ));
        }
        if let Some(subdir) = &artifact.subdir {
            let path = std::path::Path::new(subdir);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                anyhow::bail!(osdk_core::t!(
                    "err.lock_artifact_subdir_unsafe",
                    subdir = subdir,
                    backend = backend
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_version_identity(backend: &str, value: &str) -> Result<()> {
    let version = value.trim();
    if version.is_empty()
        || version == "."
        || version == ".."
        || version.contains(['/', '\\'])
        || std::path::Path::new(version).is_absolute()
    {
        anyhow::bail!(osdk_core::t!(
            "err.lock_version_unsafe",
            version = value,
            backend = backend
        ));
    }
    Ok(())
}

pub(crate) fn validate_exact_node_version(backend: &str, value: &str) -> Result<()> {
    validate_version_identity(backend, value)?;
    if !matches!(VersionSpec::parse(value), VersionSpec::Exact(version) if version == value) {
        anyhow::bail!(osdk_core::t!(
            "err.lock_npm_node_version_not_exact",
            backend = backend,
            version = value
        ));
    }
    Ok(())
}

pub(crate) fn validate_exact_rust_version(backend: &str, value: &str) -> Result<()> {
    validate_version_identity(backend, value)?;
    if !matches!(VersionSpec::parse(value), VersionSpec::Exact(version) if version == value) {
        anyhow::bail!(
            "schema 4 native entry `{backend}` requires an exact Rust runtime version, got `{value}`"
        );
    }
    Ok(())
}

pub(crate) fn validate_exact_go_version(backend: &str, value: &str) -> Result<()> {
    validate_version_identity(backend, value)?;
    let validator = osdk_core::tool::ToolId::parse("go:example.com/runtime/check")?;
    if osdk_core::tool::validate_dynamic_selector(&validator, Some(value)).is_err()
        || !matches!(VersionSpec::parse(value), VersionSpec::Exact(version) if version == value)
    {
        anyhow::bail!(
            "schema 4 native entry `{backend}` requires an exact Go runtime version, got `{value}`"
        );
    }
    Ok(())
}

pub(crate) fn validate_cargo_registry_source(backend: &str, value: &str) -> Result<()> {
    osdk_core::backend::cargo_package::validate_registry_index(value).map_err(|error| {
        anyhow::anyhow!("Cargo registry source for `{backend}` is invalid: {error}")
    })
}

pub(crate) fn validate_cargo_native_replay(
    backend: &str,
    locked: &LockedTool,
    native: &LockedNativeTool,
) -> Result<()> {
    let id = osdk_core::tool::ToolId::parse(backend)?;
    validate_cargo_requested_selector(&id, &locked.request)?;
    osdk_core::tool::validate_dynamic_selector(&id, Some(&locked.version))?;
    let git = backend.starts_with("cargo:https://");
    let expected = if git {
        if locked.version.strip_prefix("rev:").is_some_and(|revision| {
            revision.len() == 40
                && revision
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        }) {
            NativeReplay::ImmutableRevision
        } else if locked.version == "latest"
            || locked.version.starts_with("tag:")
            || locked.version.starts_with("branch:")
        {
            NativeReplay::FloatingRef
        } else {
            anyhow::bail!("schema 4 Cargo Git entry `{backend}` has an invalid selector");
        }
    } else {
        if !matches!(
            VersionSpec::parse(&locked.version),
            VersionSpec::Exact(version) if version == locked.version
        ) {
            anyhow::bail!(
                "schema 4 Cargo registry entry `{backend}` requires an exact semantic version"
            );
        }
        NativeReplay::VersionOnly
    };
    if native.replay != expected {
        anyhow::bail!(
            "schema 4 Cargo entry `{backend}` replay `{}` does not match its source and selector",
            native.replay.as_str()
        );
    }
    if !git && native.source.is_none() {
        anyhow::bail!("schema 4 Cargo registry entry `{backend}` is missing its registry source");
    }
    Ok(())
}

pub(crate) fn validate_cargo_requested_selector(
    id: &osdk_core::tool::ToolId,
    selector: &str,
) -> Result<()> {
    osdk_core::tool::validate_dynamic_selector(id, Some(selector)).map_err(anyhow::Error::from)
}

pub(crate) fn validate_go_native_replay(
    backend: &str,
    locked: &LockedTool,
    native: &LockedNativeTool,
) -> Result<()> {
    let id = osdk_core::tool::ToolId::parse(backend)?;
    osdk_core::tool::validate_dynamic_selector(&id, Some(&locked.request))?;
    if !matches!(
        VersionSpec::parse(&locked.version),
        VersionSpec::Exact(version) if version == locked.version
    ) || osdk_core::tool::validate_dynamic_selector(&id, Some(&locked.version)).is_err()
    {
        anyhow::bail!(
            "schema 4 Go entry `{backend}` requires an exact resolved semantic or pseudo-version"
        );
    }
    if native.replay != NativeReplay::VersionOnly {
        anyhow::bail!("schema 4 Go entry `{backend}` must use version-only replay");
    }
    let source = native.source.as_deref().ok_or_else(|| {
        anyhow::anyhow!("schema 4 Go entry `{backend}` is missing its proxy source")
    })?;
    osdk_core::backend::go_package::validate_go_proxy(source)?;
    let module = native.module.as_deref().ok_or_else(|| {
        anyhow::anyhow!("schema 4 Go entry `{backend}` is missing its module root")
    })?;
    let module_id = osdk_core::tool::ToolId::parse(&format!("go:{module}"))?;
    let suffix = id.subject().strip_prefix(module).unwrap_or("!");
    if module_id.subject() != module || (!suffix.is_empty() && !suffix.starts_with('/')) {
        anyhow::bail!("schema 4 Go entry `{backend}` has an invalid module root");
    }
    Ok(())
}

pub(crate) fn reject_linked_rust(
    dirs: &osdk_core::dirs::Dirs,
    version: &ToolVersion,
) -> Result<()> {
    if version.backend == "rust"
        && dirs
            .install_path("rust", &version.version)
            .join(".osdk-linked")
            .is_file()
    {
        anyhow::bail!(
            "linked Rust toolchain `{}` is local-only and cannot be written as a reproducible lock artifact",
            version.version
        );
    }
    Ok(())
}

pub(crate) fn reject_legacy_npm_entries(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock
            .tools
            .iter()
            .find(|(backend, _)| backend.starts_with("npm:"))
        {
            anyhow::bail!(osdk_core::t!(
                "err.lock_schema1_npm_migration_requires_graph",
                backend = backend,
                platform = platform
            ));
        }
    }
    Ok(())
}

pub(crate) fn reject_unmigratable_schema_one_npm_entries(lockfile: &Lockfile) -> Result<()> {
    for (platform, platform_lock) in &lockfile.platforms {
        if let Some((backend, _)) = platform_lock.tools.iter().find(|(backend, locked)| {
            backend.starts_with("npm:") && matches!(locked.npm, Some(LockedNpmGraph::Legacy(_)))
        }) {
            anyhow::bail!(osdk_core::t!(
                "err.lock_schema1_npm_migration_requires_graph",
                backend = backend,
                platform = platform
            ));
        }
    }
    Ok(())
}
