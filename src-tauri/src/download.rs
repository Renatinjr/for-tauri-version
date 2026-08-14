//! Fetching a campaign video and proving it arrived intact.
//!
//! A port of `net/Downloader.kt`. The parts that matter and are easy to get subtly wrong:
//!
//! - **A digest cannot be resumed.** On picking up a partial file, the bytes already on
//!   disk are re-read through the hasher before the transfer continues, or the check would
//!   fail every time on a resumed download.
//! - **A `200` in reply to a `Range` request** means the server ignored it. Reset the
//!   digest and start from zero; never splice.
//! - **Truncate at the resume point** before appending, discarding anything past what the
//!   digest actually covered.
//! - A hash mismatch **deletes** the partial — the bytes are wrong, so it is poison to
//!   resume onto. An incomplete transfer **keeps** it; that is the whole point.

use std::io::SeekFrom;
use std::path::PathBuf;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::store::{MediaStore, Space};
use crate::{ldebug, linfo, lwarn};

pub const HASH_MISMATCH: &str = "HASH_MISMATCH";
pub const SIZE_MISMATCH: &str = "SIZE_MISMATCH";
pub const DISK_FULL: &str = "DISK_FULL";
pub const HTTP_ERROR: &str = "HTTP_ERROR";
pub const NETWORK_ERROR: &str = "NETWORK_ERROR";
pub const INCOMPLETE: &str = "INCOMPLETE";
pub const RANGE_REJECTED: &str = "RANGE_REJECTED";
pub const PROMOTE_FAILED: &str = "PROMOTE_FAILED";

const BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub video_id: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub detail: String,
    /// Whether the partial file is still worth resuming onto.
    ///
    /// The Android player computes this and then never reads it, so a failed download only
    /// recovers when the server sends another assign. Here it drives a retry.
    pub resumable: bool,
}

impl Failure {
    fn new(code: &'static str, detail: impl Into<String>, resumable: bool) -> Self {
        Self {
            code,
            detail: detail.into(),
            resumable,
        }
    }
}

pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            // Deliberately no overall timeout: a 500 MB file over a store's ADSL link
            // legitimately takes a long time. The read timeout is what catches a stall.
            .read_timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { client })
    }

    /// Fetches, verifies and promotes. `on_progress` is called with whole percentages.
    pub async fn download(
        &self,
        store: &MediaStore,
        request: &Request,
        active_video_id: Option<&str>,
        mut on_progress: impl FnMut(u8),
    ) -> Result<PathBuf, Failure> {
        store.clear_stale_pending(&request.video_id);

        match store.ensure_space_for(&request.video_id, request.size_bytes, active_video_id) {
            Space::Ok => {}
            Space::Insufficient {
                required_bytes,
                free_bytes,
            } => {
                return Err(Failure::new(
                    DISK_FULL,
                    format!("need {required_bytes} bytes, {free_bytes} free"),
                    false,
                ));
            }
        }

        let partial = store.pending_file(&request.video_id);
        let mut digest = Sha256::new();
        let mut have = prepare_partial(&partial, request.size_bytes, &mut digest).await?;

        let mut builder = self
            .client
            .get(&request.url)
            // The tunnel debugging sessions on the Android side depended on this; an ngrok
            // free tunnel otherwise answers the first request with an HTML interstitial.
            .header("ngrok-skip-browser-warning", "true");
        if have > 0 {
            linfo!("Resuming {} at {} bytes", request.video_id, have);
            builder = builder.header("Range", format!("bytes={have}-"));
        }

        let response = builder
            .send()
            .await
            .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), true))?;

        let status = response.status().as_u16();
        match status {
            206 => {}
            200 if have > 0 => {
                lwarn!("Server ignored the Range header — restarting from zero");
                digest = Sha256::new();
                have = 0;
            }
            200 => {}
            416 => {
                // The partial is stale: the file behind this URL changed size.
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(Failure::new(
                    RANGE_REJECTED,
                    format!("server rejected range at {have}"),
                    true,
                ));
            }
            other => {
                return Err(Failure::new(
                    HTTP_ERROR,
                    format!("HTTP {other} for {}", request.url),
                    other >= 500,
                ));
            }
        }

        let written = stream_to_disk(
            response,
            &partial,
            have,
            &mut digest,
            request,
            &mut on_progress,
        )
        .await?;

        verify_and_promote(store, request, &partial, written, digest).await
    }
}

/// Re-reads an existing partial through the digest, so a resumed transfer can still be
/// verified against the whole-file hash.
async fn prepare_partial(
    partial: &std::path::Path,
    expected_size: u64,
    digest: &mut Sha256,
) -> Result<u64, Failure> {
    let mut have = match tokio::fs::metadata(partial).await {
        Ok(meta) => meta.len(),
        Err(_) => return Ok(0),
    };
    if have == 0 {
        return Ok(0);
    }
    if have > expected_size {
        lwarn!("Partial is {have} bytes, larger than the expected {expected_size} — discarding");
        let _ = tokio::fs::remove_file(partial).await;
        return Ok(0);
    }

    let mut file = File::open(partial)
        .await
        .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), false))?;
    let mut buffer = vec![0u8; BUFFER_BYTES];
    let mut remaining = have;
    while remaining > 0 {
        let want = BUFFER_BYTES.min(remaining as usize);
        match file.read(&mut buffer[..want]).await {
            Ok(0) | Err(_) => {
                // The file shrank underneath us. Trust only what was actually hashed.
                have -= remaining;
                break;
            }
            Ok(read) => {
                digest.update(&buffer[..read]);
                remaining -= read as u64;
            }
        }
    }
    ldebug!(
        "Replayed {have} bytes of {} through the digest",
        partial.display()
    );
    Ok(have)
}

async fn stream_to_disk(
    response: reqwest::Response,
    partial: &std::path::Path,
    start_at: u64,
    digest: &mut Sha256,
    request: &Request,
    on_progress: &mut impl FnMut(u8),
) -> Result<u64, Failure> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(partial)
        .await
        .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), false))?;

    // Anything past the resume point was never hashed, so it cannot be trusted.
    file.set_len(start_at)
        .await
        .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), false))?;
    file.seek(SeekFrom::Start(start_at))
        .await
        .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), false))?;

    let mut written = start_at;
    let mut last_percent = u8::MAX;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), true))?;
        file.write_all(&chunk)
            .await
            .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), true))?;
        digest.update(&chunk);
        written += chunk.len() as u64;

        // A zero-length assign would be nonsense, but it must not panic a store screen.
        if let Some(percent) =
            (written.min(request.size_bytes) * 100).checked_div(request.size_bytes)
        {
            let percent = percent as u8;
            if percent != last_percent {
                last_percent = percent;
                on_progress(percent);
            }
        }
    }

    file.flush()
        .await
        .map_err(|err| Failure::new(NETWORK_ERROR, err.to_string(), true))?;
    Ok(written)
}

async fn verify_and_promote(
    store: &MediaStore,
    request: &Request,
    partial: &std::path::Path,
    written: u64,
    digest: Sha256,
) -> Result<PathBuf, Failure> {
    if written < request.size_bytes {
        // Keep the partial: this is precisely the case resume exists for.
        return Err(Failure::new(
            INCOMPLETE,
            format!("got {written} of {} bytes", request.size_bytes),
            true,
        ));
    }
    if written > request.size_bytes {
        let _ = tokio::fs::remove_file(partial).await;
        return Err(Failure::new(
            SIZE_MISMATCH,
            format!("got {written}, expected {}", request.size_bytes),
            false,
        ));
    }

    let actual = hex(&digest.finalize());
    if !actual.eq_ignore_ascii_case(&request.sha256) {
        // The bytes are wrong, so the partial is poison — never resume onto it.
        let _ = tokio::fs::remove_file(partial).await;
        return Err(Failure::new(
            HASH_MISMATCH,
            format!("expected {}, got {actual}", request.sha256),
            false,
        ));
    }

    store.promote(&request.video_id).map_err(|err| {
        Failure::new(
            PROMOTE_FAILED,
            format!("could not move {} into media/: {err}", request.video_id),
            true,
        )
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[tokio::test]
    async fn replaying_a_partial_produces_the_same_digest_as_hashing_it_whole() {
        let dir = std::env::temp_dir().join(format!("signage-dl-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("partial.mp4");

        // 150 KB so the replay loop runs over several buffers.
        let body: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&path, &body).await.unwrap();

        let mut replayed = Sha256::new();
        let have = prepare_partial(&path, 300_000, &mut replayed)
            .await
            .unwrap();
        assert_eq!(have, body.len() as u64);

        let mut whole = Sha256::new();
        whole.update(&body);
        assert_eq!(hex(&replayed.finalize()), hex(&whole.finalize()));

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_partial_bigger_than_the_expected_file_is_discarded() {
        let dir = std::env::temp_dir().join(format!("signage-dl-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("partial.mp4");
        tokio::fs::write(&path, vec![0u8; 500]).await.unwrap();

        let mut digest = Sha256::new();
        assert_eq!(prepare_partial(&path, 400, &mut digest).await.unwrap(), 0);
        assert!(!path.exists(), "the oversized partial should be gone");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_missing_partial_simply_starts_at_zero() {
        let mut digest = Sha256::new();
        let missing = std::env::temp_dir().join(format!("nope-{}", uuid::Uuid::new_v4()));
        assert_eq!(
            prepare_partial(&missing, 100, &mut digest).await.unwrap(),
            0
        );
    }
}
