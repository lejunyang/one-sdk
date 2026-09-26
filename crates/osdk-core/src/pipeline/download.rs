//! Streaming download with progress bar and resume support.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: std::time::Duration,
    pub max_delay: std::time::Duration,
    pub visible: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(400),
            max_delay: std::time::Duration::from_millis(800),
            visible: false,
        }
    }
}

impl RetryPolicy {
    fn delay_after(self, failed_attempt: u32) -> std::time::Duration {
        let shift = failed_attempt.saturating_sub(1).min(31);
        self.base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PartialMetadata {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Download `url` to `dest`, showing a progress bar labeled `label`.
///
/// Downloads to a `.partial` sibling then atomically renames on success. If a
/// valid partial download has an ETag or Last-Modified validator, retries resume
/// it with Range + If-Range. Servers that ignore ranges or changed the object
/// cause a safe restart. Transient failures are retried with backoff.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
) -> Result<()> {
    download_with_headers(
        client,
        url,
        dest,
        label,
        show_progress,
        &reqwest::header::HeaderMap::new(),
    )
    .await
}

/// Download with caller-supplied headers. Sensitive headers are attached only
/// to the initial request; reqwest's redirect policy removes them when the
/// redirect crosses hosts.
pub async fn download_with_headers(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
    headers: &reqwest::header::HeaderMap,
) -> Result<()> {
    download_with_headers_and_policy(
        client,
        url,
        dest,
        label,
        show_progress,
        headers,
        RetryPolicy::default(),
    )
    .await
}

pub async fn download_with_headers_and_policy(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
    headers: &reqwest::header::HeaderMap,
    policy: RetryPolicy,
) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err: Option<Error> = None;
    for attempt in 1..=max_attempts {
        match download_once(client, url, dest, label, show_progress, headers).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if attempt < max_attempts && is_transient(&error) {
                    let delay = policy.delay_after(attempt);
                    let partial_bytes = partial_length(dest);
                    if policy.visible {
                        tracing::warn!(url = %url, attempt, next_attempt = attempt + 1, max_attempts, delay_ms = delay.as_millis(), partial_bytes, error = %error, "model download interrupted; retrying with resume when supported");
                    } else {
                        tracing::debug!(url = %url, attempt, next_attempt = attempt + 1, max_attempts, delay_ms = delay.as_millis(), partial_bytes, error = %error, "transient download error; retrying");
                    }
                    tokio::time::sleep(delay).await;
                    last_err = Some(error);
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::other("download failed")))
}

fn partial_length(dest: &Path) -> u64 {
    std::fs::metadata(sibling_with_suffix(dest, ".partial"))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

/// Whether an error looks transient (worth retrying).
fn is_transient(e: &Error) -> bool {
    match e {
        Error::Network { kind, .. } => matches!(
            kind,
            crate::error::NetworkErrorKind::RateLimited
                | crate::error::NetworkErrorKind::Server
                | crate::error::NetworkErrorKind::Timeout
                | crate::error::NetworkErrorKind::Interrupted
                | crate::error::NetworkErrorKind::Connect
        ),
        Error::Http(re) => {
            re.is_timeout()
                || re.is_connect()
                || re.is_request()
                || re.is_body()
                || re.is_decode()
                || re
                    .status()
                    .map(|status| {
                        status.is_server_error()
                            || status == reqwest::StatusCode::REQUEST_TIMEOUT
                            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    })
                    .unwrap_or(false)
        }
        _ => false,
    }
}

async fn download_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
    headers: &reqwest::header::HeaderMap,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        create_dir_all(parent)?;
    }
    let partial = sibling_with_suffix(dest, ".partial");
    let metadata_path = sibling_with_suffix(dest, ".partial.json");
    let existing_len = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    let metadata = read_partial_metadata(&metadata_path);
    let validator = metadata
        .as_ref()
        .filter(|metadata| metadata.url == url)
        .and_then(|metadata| {
            metadata
                .etag
                .clone()
                .or_else(|| metadata.last_modified.clone())
        });
    let resume_from = if existing_len > 0 && validator.is_some() {
        existing_len
    } else {
        if existing_len > 0 {
            let _ = std::fs::remove_file(&partial);
        }
        let _ = std::fs::remove_file(&metadata_path);
        0
    };

    let mut request = client.get(url).headers(headers.clone());
    if resume_from > 0 {
        request = request
            .header(RANGE, format!("bytes={resume_from}-"))
            .header(IF_RANGE, validator.as_deref().unwrap_or_default());
    }
    let mut resp = request
        .send()
        .await
        .map_err(|error| Error::network(url, error))?;
    if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(&metadata_path);
        resp = client
            .get(url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| Error::network(url, error))?;
    }
    let mut status = resp.status();
    let mut appending = resume_from > 0
        && status == reqwest::StatusCode::PARTIAL_CONTENT
        && content_range_starts_at(&resp, resume_from);
    if resume_from > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT && !appending {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(&metadata_path);
        resp = client
            .get(url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| Error::network(url, error))?;
        status = resp.status();
        appending = false;
    }
    if status == reqwest::StatusCode::PARTIAL_CONTENT && !appending {
        return Err(Error::other(format!(
            "invalid Content-Range while downloading {url}"
        )));
    }
    let resp = resp
        .error_for_status()
        .map_err(|error| Error::network(url, error))?;
    let downloaded_before = if appending { resume_from } else { 0 };
    let total = resp
        .content_length()
        .map(|remaining| remaining.saturating_add(downloaded_before));
    let response_metadata = PartialMetadata {
        url: url.to_string(),
        etag: header_string(&resp, ETAG),
        last_modified: header_string(&resp, LAST_MODIFIED),
    };
    write_partial_metadata(&metadata_path, &response_metadata)?;

    let pb = if show_progress {
        let pb = match total {
            Some(t) => ProgressBar::new(t),
            None => ProgressBar::new_spinner(),
        };
        pb.set_style(
            ProgressStyle::with_template(
                "{msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
        );
        pb.set_message(label.to_string());
        pb.set_position(downloaded_before);
        Some(pb)
    } else {
        None
    };

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if appending {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(&partial)
        .await
        .map_err(|e| Error::io(&partial, e))?;
    let mut stream = resp.bytes_stream();
    let mut downloaded = downloaded_before;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::network(url, error))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::io(&partial, e))?;
        downloaded += chunk.len() as u64;
        if let Some(pb) = &pb {
            pb.set_position(downloaded);
        }
    }
    file.flush().await.map_err(|e| Error::io(&partial, e))?;
    drop(file);

    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    std::fs::rename(&partial, dest).map_err(|e| Error::io(dest, e))?;
    let _ = std::fs::remove_file(&metadata_path);
    Ok(())
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn read_partial_metadata(path: &Path) -> Option<PartialMetadata> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_partial_metadata(path: &Path, metadata: &PartialMetadata) -> Result<()> {
    let bytes = serde_json::to_vec(metadata)?;
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))
}

fn header_string(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn content_range_starts_at(resp: &reqwest::Response, offset: u64) -> bool {
    let expected = format!("bytes {offset}-");
    resp.headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with(&expected))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn resumes_interrupted_download_with_if_range() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for request_number in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap();
                if request_number == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabcde",
                        )
                        .unwrap();
                } else {
                    let request = request.to_ascii_lowercase();
                    assert!(request.contains("range: bytes=5-"));
                    assert!(request.contains("if-range: \"v1\""));
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 5-9/10\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nfghij",
                        )
                        .unwrap();
                }
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("artifact.bin");
        let url = format!("http://{address}/artifact.bin");
        download(&reqwest::Client::new(), &url, &dest, "test", false)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(std::fs::read(dest).unwrap(), b"abcdefghij");
        assert!(!sibling_with_suffix(&temp.path().join("artifact.bin"), ".partial").exists());
    }

    // Bug 007: a connection that stops sending mid-body without closing used to
    // hang `stream.next()` forever. With a read (no-progress) timeout on the
    // client, the stalled read fails instead, so the download returns an error
    // the retry+resume loop can act on rather than blocking indefinitely.
    //
    // The isolation that makes this test meaningful: the server sends part of the
    // body and then stays silent AND open, never closing. So the only thing that
    // can end the read is the client's own read timeout. The outer guard (3s) is
    // far longer than the 300ms read timeout but the server never acts within it,
    // so a passing run proves the timeout fired -- not a server-side close. If the
    // timeout were absent the read would hang, the outer guard would elapse, and
    // `outcome` would be `Err`, failing the test. The server thread is detached
    // (never joined) precisely because it is meant to stay blocked.
    #[tokio::test]
    async fn read_timeout_fails_a_stalled_stream_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            // Promise 10 bytes, deliver 5, then stay silent without closing: hold
            // the socket by blocking on a read that never returns data.
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabcde")
                .unwrap();
            stream.flush().unwrap();
            let mut sink = [0u8; 64];
            let _ = stream.read(&mut sink);
            // Keep the connection object alive so it is not dropped/closed.
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(stream);
        });

        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("stalled.bin");
        let url = format!("http://{address}/stalled.bin");
        let client = reqwest::Client::builder()
            .read_timeout(std::time::Duration::from_millis(300))
            .build()
            .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            download_once(
                &client,
                &url,
                &dest,
                "stalled",
                false,
                &reqwest::header::HeaderMap::new(),
            ),
        )
        .await;
        // `Ok(..)` means the call returned on its own inside the guard -- i.e. the
        // read timeout fired. A hang would make this the outer timeout's `Err`.
        let result = outcome.expect("download_once hung: the read timeout did not fire");
        assert!(
            result.is_err(),
            "a stalled stream should fail, not complete: {result:?}"
        );
    }

    #[tokio::test]
    async fn invalid_content_range_restarts_without_range() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for request_number in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                if request_number == 0 {
                    assert!(request.contains("range: bytes=5-"));
                    stream
                        .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 4-8/10\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nwrong")
                        .unwrap();
                } else {
                    assert!(!request.contains("range:"));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabcdefghij")
                        .unwrap();
                }
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("model.bin");
        let url = format!("http://{address}/model.bin");
        std::fs::write(sibling_with_suffix(&destination, ".partial"), b"abcde").unwrap();
        write_partial_metadata(
            &sibling_with_suffix(&destination, ".partial.json"),
            &PartialMetadata {
                url: url.clone(),
                etag: Some("\"v1\"".into()),
                last_modified: None,
            },
        )
        .unwrap();
        download(&reqwest::Client::new(), &url, &destination, "model", false)
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"abcdefghij");
    }

    #[tokio::test]
    async fn custom_policy_recovers_after_more_than_three_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for request_number in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                if request_number < 5 {
                    stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"v1\"\r\nConnection: close\r\n\r\ndone")
                        .unwrap();
                }
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("model.bin");
        let url = format!("http://{address}/model.bin");
        download_with_headers_and_policy(
            &reqwest::Client::new(),
            &url,
            &destination,
            "model",
            false,
            &reqwest::header::HeaderMap::new(),
            RetryPolicy {
                max_attempts: 6,
                base_delay: std::time::Duration::from_millis(1),
                max_delay: std::time::Duration::from_millis(2),
                visible: true,
            },
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"done");
    }

    #[tokio::test]
    async fn different_source_url_discards_old_partial_instead_of_resuming_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(!String::from_utf8(request)
                .unwrap()
                .to_ascii_lowercase()
                .contains("range:"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"new\"\r\nConnection: close\r\n\r\nfresh")
                .unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("model.bin");
        std::fs::write(sibling_with_suffix(&destination, ".partial"), b"stale").unwrap();
        write_partial_metadata(
            &sibling_with_suffix(&destination, ".partial.json"),
            &PartialMetadata {
                url: "https://old.invalid/model.bin".into(),
                etag: Some("\"old\"".into()),
                last_modified: None,
            },
        )
        .unwrap();
        let url = format!("http://{address}/model.bin");
        download(&reqwest::Client::new(), &url, &destination, "model", false)
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"fresh");
    }

    #[tokio::test]
    async fn interrupted_download_never_publishes_final_artifact() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nETag: \"v1\"\r\nConnection: close\r\n\r\npartial",
                    )
                    .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("artifact.bin");
        let url = format!("http://{address}/artifact.bin");
        assert!(
            download(&reqwest::Client::new(), &url, &destination, "test", false)
                .await
                .is_err()
        );
        server.join().unwrap();
        assert!(!destination.exists());
        assert!(sibling_with_suffix(&destination, ".partial").is_file());
    }
}
