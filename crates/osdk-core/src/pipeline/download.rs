//! Streaming download with progress bar and resume support.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

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
    if dest.exists() {
        return Ok(());
    }
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err: Option<Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match download_once(client, url, dest, label, show_progress, headers).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < MAX_ATTEMPTS && is_transient(&e) {
                    let backoff = std::time::Duration::from_millis(400 * attempt as u64);
                    tracing::debug!(url = %url, attempt, "transient download error, retrying: {e}");
                    tokio::time::sleep(backoff).await;
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::other("download failed")))
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
    let status = resp.status();
    let appending = resume_from > 0
        && status == reqwest::StatusCode::PARTIAL_CONTENT
        && content_range_starts_at(&resp, resume_from);
    if status == reqwest::StatusCode::PARTIAL_CONTENT && !appending {
        return Err(Error::other(format!(
            "invalid Content-Range while resuming {url}"
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
