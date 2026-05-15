use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clap::Args;
use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, ext_from_mime, sanitize_title};
use duralumin_downloader::{DownloadResult, Downloader, DownloaderConfig};
use duralumin_metadata::write_tags;
use duralumin_storage::Db;
use tokio::sync::Semaphore;
use tracing::info;

use super::helpers::fetch_cover;
use crate::config;

#[derive(Args)]
pub struct DownloadArgs {
    /// Episode IDs to download (download queue if omitted).
    pub ids: Vec<String>,
}

pub async fn cmd_download(cfg: &config::Config, db: &Db, ids: Vec<String>) -> Result<()> {
    let downloader = Arc::new(Downloader::new(DownloaderConfig::from(&cfg.downloader)));
    let semaphore = Arc::new(Semaphore::new(cfg.downloader.concurrent_downloads as usize));
    let library = cfg.storage.library();

    let episodes = if ids.is_empty() {
        db.get_queue().await?
    } else {
        let mut eps = Vec::new();
        for id in &ids {
            let eid = EpisodeId::from(id.clone());
            if let Some(ep) = db.get_episode(&eid).await? {
                eps.push(ep);
            } else {
                tracing::warn!(id, "episode not found, skipping");
            }
        }
        eps
    };

    if episodes.is_empty() {
        use owo_colors::OwoColorize;
        println!("{}", "Nothing to download.".dimmed());
        return Ok(());
    }

    run_downloads(db, &downloader, &semaphore, &library, episodes).await;
    Ok(())
}

/// Fetch the download queue and process it. Used by the daemon loop.
pub async fn drain_downloads(
    db: &Db,
    downloader: &Arc<Downloader>,
    semaphore: &Arc<Semaphore>,
    library: &std::path::Path,
) {
    let episodes = match db.get_queue().await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(error = %e, "failed to load download queue");
            return;
        }
    };
    if !episodes.is_empty() {
        run_downloads(db, downloader, semaphore, library, episodes).await;
    }
}

/// Spawn one download task per episode and wait for all to finish.
pub async fn run_downloads(
    db: &Db,
    downloader: &Arc<Downloader>,
    semaphore: &Arc<Semaphore>,
    library: &std::path::Path,
    episodes: Vec<Episode>,
) {
    if !library.exists() {
        if let Err(e) = std::fs::create_dir_all(library) {
            tracing::error!(path = %library.display(), error = %e, "failed to create library directory");
            return;
        }
        info!(path = %library.display(), "created library directory");
    }

    let mut handles = Vec::new();

    for ep in episodes {
        let dl = Arc::clone(downloader);
        let sem = Arc::clone(semaphore);
        let dest = library.to_path_buf();
        let db = db.clone();
        let feed_id = ep.feed_id;

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            let feed = match db.get_feed(feed_id).await {
                Ok(Some(f)) => f,
                _ => {
                    tracing::error!(episode_id = %ep.id, "feed not found for episode");
                    return;
                }
            };

            let feed_dir = dest.join(&feed.slug);
            if let Err(e) = tokio::fs::create_dir_all(&feed_dir).await {
                tracing::error!(
                    episode_id = %ep.id,
                    path = %feed_dir.display(),
                    error = %e,
                    "failed to create feed directory"
                );
                return;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perm = std::fs::Permissions::from_mode(0o775);
                if let Err(e) = tokio::fs::set_permissions(&feed_dir, perm).await {
                    tracing::warn!(path = %feed_dir.display(), error = %e, "could not set feed directory permissions");
                }
            }

            // Guard against re-downloading a file that already exists on disk
            // (happens when a previous DB write succeeded for the file but the
            // state update failed, leaving the episode still in the queue).
            let stem = sanitize_title(&ep.title);
            let ext = ext_from_mime(ep.enclosure_mime.as_deref(), ep.enclosure_url.as_str());
            let existing = feed_dir.join(format!("{stem}.{ext}"));
            if existing.exists() {
                let downloaded_at = tokio::fs::metadata(&existing)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(chrono::DateTime::<Utc>::from)
                    .unwrap_or_else(Utc::now);
                let state = match &ep.state {
                    EpisodeState::Matched(Action::Dynamic) => EpisodeState::Dynamic {
                        path: existing.clone(),
                        downloaded_at,
                        sha256: String::new(),
                    },
                    _ => EpisodeState::Complete {
                        path: existing.clone(),
                        downloaded_at,
                        sha256: String::new(),
                    },
                };
                match db.update_episode_state(&ep.id, &state).await {
                    Ok(()) => {
                        tracing::info!(
                            episode_id = %ep.id,
                            path = %existing.display(),
                            "file already on disk, marked complete without re-downloading"
                        );
                        db.dequeue(&ep.id).await.ok();
                    }
                    Err(e) => tracing::error!(
                        episode_id = %ep.id,
                        error = %e,
                        "found pre-existing file but failed to mark complete"
                    ),
                }
                return;
            }

            let ep_id = ep.id.clone();
            let result = dl
                .download(&ep, &feed_dir, |state| {
                    tracing::debug!(episode_id = %ep_id, %state, "state update");
                })
                .await;

            match result {
                Ok(DownloadResult { path, sha256, .. }) => {
                    let new_state = match &ep.state {
                        EpisodeState::Matched(Action::Dynamic) => EpisodeState::Dynamic {
                            path: path.clone(),
                            downloaded_at: Utc::now(),
                            sha256,
                        },
                        _ => EpisodeState::Complete {
                            path: path.clone(),
                            downloaded_at: Utc::now(),
                            sha256,
                        },
                    };
                    if let Err(e) = db.update_episode_state(&ep.id, &new_state).await {
                        tracing::error!(episode_id = %ep.id, error = %e, "failed to persist download state");
                        return;
                    }
                    db.dequeue(&ep.id).await.ok();
                    let cover = fetch_cover(&ep, &feed).await;
                    if let Err(e) = write_tags(&path, &ep, &feed, cover.as_deref()) {
                        tracing::warn!(episode_id = %ep.id, error = %e, "tag write failed");
                    }
                }
                Err(e) => {
                    tracing::error!(episode_id = %ep.id, error = %e, "download failed after retries");
                    let quarantine = EpisodeState::Quarantined {
                        reason: e.reason().to_string(),
                        last_error: e.to_string(),
                    };
                    if let Err(db_err) = db.update_episode_state(&ep.id, &quarantine).await {
                        tracing::error!(episode_id = %ep.id, error = %db_err, "failed to quarantine episode");
                    }
                    db.dequeue(&ep.id).await.ok();
                }
            }
        }));
    }

    for h in handles {
        if let Err(e) = h.await {
            tracing::error!(error = ?e, "download task panicked");
        }
    }
}
