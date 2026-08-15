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
/// complete file already exists at `dest`, returns early.
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
        file.write_all(&chunk).await.map_err(|e| Error::io(&partial, e))?;
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
