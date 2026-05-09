use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use duralumin_core::{Episode, EpisodeState, ext_from_mime, sanitize_title};

// ---- Config ----------------------------------------------------------------

pub struct DownloaderConfig {
    pub concurrent_downloads: u32,
    pub attempt_timeout: Duration,
    pub max_retries: u8,
    pub backoff_base: Duration,
    pub user_agent: String,
    pub accept_invalid_certs: bool,
}

// ---- Error -----------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("download timed out after {0:?}")]
    Timeout(Duration),
    #[error("HTTP {0}")]
    Http(StatusCode),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("size mismatch: expected ~{expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}

impl DownloadError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "timeout",
            Self::Http(s) if s.as_u16() == 404 => "http_404",
            Self::Http(_) => "http",
            Self::Io(_) => "io",
            Self::SizeMismatch { .. } => "size_mismatch",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::Network(_) => "network",
        }
    }
}

// ---- Result ----------------------------------------------------------------

pub struct DownloadResult {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

// ---- Downloader ------------------------------------------------------------

pub struct Downloader {
    client: reqwest::Client,
    config: DownloaderConfig,
}

impl Downloader {
    pub fn new(config: DownloaderConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(config.attempt_timeout)
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .build()
            .expect("failed to build HTTP client");
        if config.accept_invalid_certs {
            tracing::warn!(
                "TLS certificate validation disabled — only use this for trusted local servers"
            );
        }
        Self { client, config }
    }

    pub async fn retry_download(
        &self,
        episode: &Episode,
        on_state: impl Fn(EpisodeState) + Send + Sync,
        attempt: u8,
        max: u8,
        e: DownloadError,
        last_err: &mut Option<DownloadError>,
    ) -> Option<DownloadError> {
        if attempt < max {
            tracing::warn!(
                episode_id = %episode.id,
                attempt,
                max,
                error = %e,
                "download attempt failed, will retry"
            );
            on_state(EpisodeState::Failed {
                last_error: e.to_string(),
                attempts: attempt,
            });
            let delay = backoff(self.config.backoff_base, attempt);
            tracing::debug!(
                episode_id = %episode.id,
                delay_secs = delay.as_secs_f64(),
                "backing off before next attempt"
            );
            tokio::time::sleep(delay).await;
            *last_err = Some(e);
            None
        } else {
            tracing::error!(
                episode_id = %episode.id,
                title = %episode.title,
                total_attempts = attempt,
                reason = e.reason(),
                error = %e,
                "download failed after all retries, quarantining"
            );
            let reason = e.reason().to_string();
            let detail = e.to_string();
            on_state(EpisodeState::Quarantined {
                reason,
                last_error: detail,
            });
            Some(e)
        }
    }
    /// Download one episode, calling `on_state` for each state transition.
    pub async fn download(
        &self,
        episode: &Episode,
        dest_dir: &Path,
        on_state: impl Fn(EpisodeState) + Send + Sync,
    ) -> Result<DownloadResult, DownloadError> {
        let max = self.config.max_retries;
        let mut last_err: Option<DownloadError> = None;

        for attempt in 1..=max {
            tracing::info!(
                episode_id = %episode.id,
                title = %episode.title,
                url  = %episode.enclosure_url,
                attempt,
                max,
                "starting download attempt"
            );
            on_state(EpisodeState::Downloading {
                attempt,
                started_at: Utc::now(),
            });

            let result = tokio::time::timeout(
                self.config.attempt_timeout,
                self.try_once(episode, dest_dir),
            )
            .await;

            match result {
                Ok(Ok(r)) => return Ok(r),
                Ok(Err(e)) => {
                    if let Some(err) = self
                        .retry_download(episode, &on_state, attempt, max, e, &mut last_err)
                        .await
                    {
                        return Err(err);
                    }
                }
                Err(_elapsed) => {
                    let timeout_err = DownloadError::Timeout(self.config.attempt_timeout);
                    if let Some(err) = self
                        .retry_download(
                            episode,
                            &on_state,
                            attempt,
                            max,
                            timeout_err,
                            &mut last_err,
                        )
                        .await
                    {
                        return Err(err);
                    }
                }
            }
        }

        // last_err is always Some here (max > 0), but the loop returned if max==0
        Err(last_err.unwrap_or(DownloadError::Io(std::io::Error::other(
            "no attempts made.\nTry setting config:\n[downloader]\nmax_retries ≥ 1",
        ))))
    }

    async fn try_once(
        &self,
        episode: &Episode,
        dest_dir: &Path,
    ) -> Result<DownloadResult, DownloadError> {
        let final_path = build_filename(episode, dest_dir);
        let part_path = part_path_for(&final_path);

        // Check for resumable partial download
        let existing_bytes = if part_path.exists() {
            tokio::fs::metadata(&part_path).await?.len()
        } else {
            0
        };

        // First probe: HEAD to check Accept-Ranges (only if we have a partial file)
        let resume_from = if existing_bytes > 0 {
            let head = self
                .client
                .head(episode.enclosure_url.as_str())
                .send()
                .await?;
            let accepts_ranges = head
                .headers()
                .get("Accept-Ranges")
                .and_then(|v| v.to_str().ok())
                .map(|v| v != "none")
                .unwrap_or(false);

            if accepts_ranges { existing_bytes } else { 0 }
        } else {
            0
        };

        // Build the GET request, with Range header if resuming
        let mut rb = self.client.get(episode.enclosure_url.as_str());
        if resume_from > 0 {
            rb = rb.header("Range", format!("bytes={}-", resume_from));
        }

        let resp = rb.send().await?;

        // If we sent a Range request but got 200 (not 206), server ignored it — start fresh
        let (resume_from, mut file) = if resume_from > 0 && resp.status() == StatusCode::OK {
            tracing::debug!(
                episode_id = %episode.id,
                "server returned 200 to Range request, restarting"
            );
            tokio::fs::remove_file(&part_path).await.ok();
            (0u64, File::create(&part_path).await?)
        } else if !resp.status().is_success() && resp.status() != StatusCode::PARTIAL_CONTENT {
            return Err(DownloadError::Http(resp.status()));
        } else if resume_from > 0 {
            // Append to existing .part
            let f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part_path)
                .await?;
            (resume_from, f)
        } else {
            (0u64, File::create(&part_path).await?)
        };

        // Stream body, computing SHA-256 incrementally over the complete file
        let mut hasher = Sha256::new();
        let mut bytes_written: u64 = resume_from;

        // Pre-load existing bytes into hasher if resuming
        if resume_from > 0 {
            let existing = tokio::fs::read(&part_path).await?;
            hasher.update(&existing);
        }

        let mut stream = resp;
        while let Some(chunk) = stream.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            bytes_written += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);

        let sha256 = hex::encode(hasher.finalize());

        // Size verification ±1% (spec §5.6)
        if let Some(expected) = episode.enclosure_size
            && expected > 0
        {
            let tolerance = (expected as f64 * 0.01).ceil() as u64;
            let lo = expected.saturating_sub(tolerance);
            let hi = expected + tolerance;
            if bytes_written < lo || bytes_written > hi {
                tokio::fs::remove_file(&part_path).await.ok();
                return Err(DownloadError::SizeMismatch {
                    expected,
                    actual: bytes_written,
                });
            }
        }

        // Atomic rename .part → final
        tokio::fs::rename(&part_path, &final_path).await?;

        tracing::info!(
            episode_id = %episode.id,
            path = %final_path.display(),
            bytes = bytes_written,
            "download complete"
        );

        Ok(DownloadResult {
            path: final_path,
            sha256,
            bytes: bytes_written,
        })
    }
}

// ---- Helpers ---------------------------------------------------------------

fn build_filename(episode: &Episode, dest_dir: &Path) -> PathBuf {
    let slug = sanitize_title(&episode.title);
    let ext = ext_from_mime(
        episode.enclosure_mime.as_deref(),
        episode.enclosure_url.as_str(),
    );
    let name = format!("{slug}.{ext}");
    let candidate = dest_dir.join(&name);
    if candidate.exists() {
        let short = episode.id.short();
        dest_dir.join(format!("{slug}-{short}.{ext}"))
    } else {
        candidate
    }
}

fn part_path_for(final_path: &Path) -> PathBuf {
    let mut p = final_path.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Exponential backoff with ±25% jitter.
fn backoff(base: Duration, attempt: u8) -> Duration {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1) as u32)
        .unwrap_or(u64::MAX);
    let millis = base.as_millis() as u64 * factor;
    // ±25% jitter using a simple hash of the attempt number as pseudo-random
    let jitter_range = millis / 4;
    let seed = (attempt as u64).wrapping_mul(6364136223846793005);
    let jitter = seed % (jitter_range * 2 + 1);
    let jittered = millis.saturating_sub(jitter_range) + jitter;
    Duration::from_millis(jittered)
}
