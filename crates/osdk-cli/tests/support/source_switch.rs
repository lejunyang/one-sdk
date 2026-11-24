use super::*;

#[cfg(unix)]
#[test]
fn cargo_and_npm_source_switches_use_the_selected_provider_and_preserve_failed_installs() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .arg(repository.join("scripts/test-source-switch.py"))
        .arg(osdk())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("7 source-switch scenarios passed"));
}

fn fixture_dirs(root: &Path) -> osdk_core::dirs::Dirs {
    osdk_core::dirs::Dirs::resolve_from(|key| match key {
        "HOME" | "USERPROFILE" => Some(root.join("home").display().to_string()),
        "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
        "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
        "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
        _ => None,
    })
    .unwrap()
}

#[cfg(not(windows))]
#[test]
fn conda_explicit_source_switch_checks_origin_before_reuse() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("osdk.toml"), "").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn({
        let seen = seen.clone();
        let stop = stop.clone();
        move || {
            while !stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(FIXTURE_SERVER_TIMEOUT))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request.split_whitespace().nth(1).unwrap().to_string();
                seen.lock().unwrap().push(path.clone());
                let origin = path.split('/').nth(1).unwrap_or("");
                let archive = conda_archive(origin);
                let checksum = osdk_core::pipeline::verify::hash_bytes(
                    &archive,
                    osdk_core::pipeline::HashAlgo::Sha256,
                );
                let body = if path.ends_with("/noarch/repodata.json") {
                    serde_json::json!({"packages":{"fixture-tool-1.2.3-0.tar.bz2":{"name":"fixture-tool","version":"1.2.3","build":"0","build_number":0,"subdir":"noarch","depends":[],"sha256":checksum,"size":archive.len()}}}).to_string().into_bytes()
                } else if path.ends_with("/repodata.json") {
                    b"{\"packages\":{}}".to_vec()
                } else if path.ends_with(".tar.bz2") {
                    archive
                } else {
                    Vec::new()
                };
                let status = if origin == "broken" || body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                if !request.starts_with("HEAD ") {
                    stream.write_all(&body).unwrap();
                }
            }
        }
    });
    std::fs::create_dir(root.join("home")).unwrap();
    let condarc = root.join("home/.condarc");
    std::fs::write(&condarc, format!("channel_alias: {endpoint}/native\n")).unwrap();
    for (id, path) in [("selected", "selected"), ("broken", "broken")] {
        let out = run_isolated(
            root,
            &[
                "source",
                "add",
                "conda:fixture-tool",
                "--id",
                id,
                "--download-url",
                &format!("{endpoint}/{path}"),
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let env = [("CONDARC", condarc.to_str().unwrap())];
    let run = |args: &[&str]| run_isolated_in_with_env(root, root, args, &env);
    let out = run(&["install", "conda:fixture-tool@1.2.3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let old = conda_origins(root);
    assert_eq!(old.len(), 1);
    assert_eq!(old[0].1, "native");
    seen.lock().unwrap().clear();
    let out = run(&["source", "pin", "conda:fixture-tool", "selected"]);
    assert!(out.status.success());
    let out = run(&["install", "conda:fixture-tool@1.2.3"]);
    let requests = seen.lock().unwrap().clone();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        requests
            .iter()
            .any(|p| p.starts_with("/selected/") && p.ends_with(".tar.bz2")),
        "{requests:?}; {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let installs = conda_origins(root);
    assert!(
        installs.iter().any(|(_, origin)| origin == "selected"),
        "{installs:?}"
    );
    assert!(old[0].0.join(".osdk-complete").is_file());
    let out = run(&["--offline", "install", "conda:fixture-tool@1.2.3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("already installed"));
    let out = run(&["--source", "broken", "install", "conda:fixture-tool@1.2.3"]);
    assert!(!out.status.success());
    assert!(installs
        .iter()
        .all(|(p, _)| p.join(".osdk-complete").is_file()));
}

#[cfg(not(windows))]
fn conda_archive(origin: &str) -> Vec<u8> {
    let encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, bytes) in [
        ("info/index.json", br#"{"name":"fixture-tool","version":"1.2.3","build":"0","build_number":0,"subdir":"noarch","depends":[]}"#.to_vec()),
        ("info/files", b"bin/fixture-tool\norigin.txt\n".to_vec()),
        ("bin/fixture-tool", b"#!/bin/sh\nexit 0\n".to_vec()),
        ("origin.txt", origin.as_bytes().to_vec()),
    ] {
        let mut header = tar::Header::new_gnu(); header.set_size(bytes.len() as u64); header.set_mode(0o755); header.set_cksum();
        builder.append_data(&mut header,path,bytes.as_slice()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[cfg(not(windows))]
fn conda_origins(root: &Path) -> Vec<(PathBuf, String)> {
    walkdir::WalkDir::new(root.join("installs/conda/fixture-tool"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_name() == "origin.txt")
        .map(|e| {
            (
                e.path().parent().unwrap().to_path_buf(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn explicit_pin_does_not_reuse_dynamic_inventory_without_source_evidence() {
    use osdk_core::{
        inventory::{DynamicToolBin, DynamicToolManifest},
        tool::{InstallIdentity, InstallScope},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let dirs = fixture_dirs(root);
    std::fs::write(root.join("osdk.toml"), "").unwrap();
    let identity = InstallIdentity::new(
        "conda:fixture-tool",
        "1.2.3",
        osdk_core::platform::Platform::current().to_string(),
        InstallScope::Isolated,
        &Default::default(),
        Vec::new(),
        std::collections::BTreeMap::from([
            ("artifact-file".into(), "conda-closure-1.json".into()),
            (
                "artifact-checksum".into(),
                format!("blake3:{}", "a".repeat(64)),
            ),
        ]),
    )
    .unwrap();
    let locator = osdk_core::dirs::InstallLocator::new(&dirs, identity.clone()).unwrap();
    std::fs::create_dir_all(locator.install_root().join("bin")).unwrap();
    std::fs::write(
        locator.install_root().join("bin/fixture-tool"),
        "old artifact",
    )
    .unwrap();
    let mut manifest = DynamicToolManifest::from_identity(identity).unwrap();
    manifest.bins.push(DynamicToolBin {
        name: "fixture-tool".into(),
        path: "bin/fixture-tool".into(),
        owned: true,
    });
    manifest.write_atomic(locator.install_root()).unwrap();
    std::fs::write(locator.install_root().join(".osdk-complete"), "").unwrap();
    osdk_core::pipeline::write_artifact_receipt_at(
        locator.install_root(),
        &osdk_core::pipeline::ArtifactReceipt {
            url: String::new(),
            file_name: "conda-closure-1.json".into(),
            checksum: Some(format!("blake3:{}", "a".repeat(64))),
            evidence: Vec::new(),
        },
    )
    .unwrap();
    let add = run_isolated(
        root,
        &[
            "source",
            "add",
            "conda:fixture-tool",
            "--id",
            "new",
            "--download-url",
            "http://127.0.0.1:1/new",
        ],
    );
    assert!(add.status.success());
    let out = run_isolated_in_with_env(
        root,
        root,
        &[
            "--source",
            "new",
            "--offline",
            "install",
            "conda:fixture-tool@1.2.3",
        ],
        &[("CONDA_CHANNEL_ALIAS", "http://127.0.0.1:1/native")],
    );
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(locator.install_root().join("bin/fixture-tool")).unwrap(),
        "old artifact"
    );
}
