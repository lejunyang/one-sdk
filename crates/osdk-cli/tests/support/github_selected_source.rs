use super::*;

#[cfg(not(windows))]
#[test]
fn github_token_official_download_does_not_fall_back_to_enabled_relay() {
    for persistent in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("osdk.toml"), "").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = std::thread::spawn({
            let stop = stop.clone();
            let seen = seen.clone();
            move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
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
                    seen.lock().unwrap().push(String::from_utf8(bytes).unwrap());
                    stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            }
        });
        let tool = "github:fixture-owner/fixture-repo";
        let added = run_isolated(
            root,
            &[
                "source",
                "add",
                tool,
                "--id",
                "relay",
                "--download-url",
                &format!("{endpoint}/relay/"),
                "--index-url",
                &format!("{endpoint}/relay/"),
            ],
        );
        assert!(added.status.success());
        if persistent {
            assert!(run_isolated(root, &["source", "pin", tool, "github"])
                .status
                .success());
        }
        let catalog=serde_json::json!({"schema":1,"releases":[{"tag":"v1.2.3","assets":[{"name":"fixture-tool","url":"https://github.com/fixture-owner/fixture-repo/releases/download/v1.2.3/fixture-tool","checksum":format!("sha256:{}","a".repeat(64)),"os":"linux","arch":"x64","libc":"gnu"}]}]}).to_string();
        let path = root.join("catalog.json");
        std::fs::write(&path, &catalog).unwrap();
        let digest = osdk_core::pipeline::verify::hash_bytes(
            catalog.as_bytes(),
            osdk_core::pipeline::HashAlgo::Sha256,
        );
        let option = format!("catalog-url={}", path.display());
        let checksum = format!("catalog-sha256={digest}");
        let args = if persistent {
            vec![
                "install",
                "github:fixture-owner/fixture-repo@1.2.3",
                "-o",
                &option,
                "-o",
                &checksum,
            ]
        } else {
            vec![
                "--source",
                "github",
                "install",
                "github:fixture-owner/fixture-repo@1.2.3",
                "-o",
                &option,
                "-o",
                &checksum,
            ]
        };
        let out = run_isolated_in_with_env(
            root,
            root,
            &args,
            &[
                ("GITHUB_TOKEN", "fixture-token"),
                ("HTTPS_PROXY", endpoint.as_str()),
                ("HTTP_PROXY", endpoint.as_str()),
                ("NO_PROXY", "127.0.0.1,localhost"),
            ],
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let requests = seen.lock().unwrap();
        assert!(!out.status.success());
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|r| r.starts_with("CONNECT github.com:443 ")),
            "{requests:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(not(windows))]
#[test]
fn github_token_explicit_official_and_relay_keep_the_selected_actual_route() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    for (pin, persistent) in [
        ("github", false),
        ("github", true),
        ("relay", false),
        ("relay", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("osdk.toml"), "").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = std::thread::spawn({
            let stop = stop.clone();
            let seen = seen.clone();
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
                    let mut b = [0];
                    while !bytes.ends_with(b"\r\n\r\n") {
                        stream.read_exact(&mut b).unwrap();
                        bytes.push(b[0]);
                    }
                    let request = String::from_utf8(bytes).unwrap();
                    seen.lock().unwrap().push(request.clone());
                    let (status, body) = if request.starts_with("CONNECT ") {
                        ("502 Bad Gateway", "")
                    } else {
                        ("200 OK", r#"[{"tag_name":"v1.2.3"}]"#)
                    };
                    write!(stream,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            }
        });
        let tool = "github:fixture-owner/fixture-repo";
        let out = run_isolated(
            root,
            &[
                "source",
                "add",
                tool,
                "--id",
                "relay",
                "--index-url",
                &format!("{endpoint}/relay/"),
                "--download-url",
                &format!("{endpoint}/relay/"),
            ],
        );
        assert!(out.status.success());
        if persistent {
            let out = run_isolated(root, &["source", "pin", tool, pin]);
            assert!(out.status.success());
        }
        let args = if persistent {
            vec!["list-remote", tool]
        } else {
            vec!["--source", pin, "list-remote", tool]
        };
        let out = run_isolated_in_with_env(
            root,
            root,
            &args,
            &[
                ("GITHUB_TOKEN", "fixture-token"),
                ("HTTPS_PROXY", endpoint.as_str()),
                ("HTTP_PROXY", endpoint.as_str()),
                ("NO_PROXY", "127.0.0.1,localhost"),
            ],
        );
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        let requests = seen.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests
            .iter()
            .all(|r| !r.to_ascii_lowercase().contains("\r\nauthorization:")));
        if pin == "github" {
            assert!(
                !out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            assert!(
                requests[0].starts_with("CONNECT api.github.com:443 "),
                "{requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .all(|r| r.starts_with("CONNECT api.github.com:443 ")),
                "{requests:?}"
            );
        } else {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                requests[0].starts_with("GET /relay/repos/fixture-owner/fixture-repo/releases?"),
                "{requests:?}"
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("1.2.3"));
        }
    }
}
