//! Streaming download with progress bar and resume support.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt;

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

/// Download `url` to `dest`, showing a progress bar labeled `label`.
///
/// Downloads to a `.partial` sibling then atomically renames on success. If a
/// complete file already exists at `dest`, returns early. Retries a few times on
/// transient errors (connect/timeout) with backoff.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err: Option<Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match download_once(client, url, dest, label, show_progress).await {
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
        Error::Http(re) => re.is_timeout() || re.is_connect() || re.is_request(),
        _ => false,
    }
}

async fn download_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        create_dir_all(parent)?;
    }
    let partial: PathBuf = dest.with_extension("partial");
    // Fresh download (resume across runs is best-effort; we restart partials to
    // keep checksum semantics simple and correct).
    let _ = std::fs::remove_file(&partial);

    let resp = client.get(url).send().await?.error_for_status()?;
    let total = resp.content_length();

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
        Some(pb)
    } else {
        None
    };

    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|e| Error::io(&partial, e))?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
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
    Ok(())
}
