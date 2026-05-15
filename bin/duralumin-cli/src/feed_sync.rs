use std::collections::HashMap;

use chrono::Utc;
use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, Feed, FeedId};
use duralumin_feed::FeedFetcher;
use duralumin_rules::{RuleEngine, config::FeedConfig};
use duralumin_storage::{Db, EpisodeFilter};
use tracing::info;

use crate::cli::helpers::feed_from_config;

/// Fetch one feed, upsert new episodes, evaluate rules, enqueue downloads,
/// then run the purge cycle for any existing Dynamic episodes.
///
/// If `recheck` is true, all pending (`Discovered` / `Matched`) episodes are
/// re-evaluated against the current rules using the fresh episode positions
/// from the just-fetched RSS data, so `LastNEpisodes` comparisons are correct.
pub async fn sync_one_feed(
    feed_cfg: &FeedConfig,
    db: &Db,
    engine: &RuleEngine,
    fetcher: &FeedFetcher,
    recheck: bool,
) {
    let mut feed = feed_from_config(feed_cfg);

    let feed_id = match db.upsert_feed(&feed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(slug = %feed.slug, error = %e, "failed to upsert feed");
            return;
        }
    };
    feed.id = feed_id;

    info!(slug = %feed.slug, "syncing feed");

    let (meta, mut episodes) = match fetcher.fetch(&feed).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(slug = %feed.slug, error = %e, "feed fetch failed");
            return;
        }
    };

    feed.title = meta.title.or(feed.title);
    feed.etag = meta.etag;
    feed.last_modified = meta.last_modified;
    feed.image_url = meta.image_url;
    feed.last_fetched_at = Some(Utc::now());
    if let Err(e) = db.upsert_feed(&feed).await {
        tracing::error!(slug = %feed.slug, error = %e, "failed to update feed metadata");
    }

    let mut new_count = 0usize;
    for ep in &mut episodes {
        ep.feed_id = feed_id;
        let is_new = match db.upsert_episode(ep).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    slug = %feed.slug,
                    episode_id = %ep.id,
                    error = %e,
                    "failed to upsert episode"
                );
                continue;
            }
        };

        if !is_new {
            continue;
        }

        new_count += 1;
        let action = engine.evaluate(ep, &feed);
        info!(episode_id = %ep.id, title = %ep.title, ?action, "new episode evaluated");

        if matches!(action, Action::Download | Action::Dynamic) {
            // Enqueue before updating state so a crash between the two writes
            // leaves the episode Discovered (re-evaluated next sync) rather than
            // Matched-but-missing-from-queue.
            if let Err(e) = db.enqueue(&ep.id, action).await {
                tracing::error!(
                    slug = %feed.slug,
                    episode_id = %ep.id,
                    error = %e,
                    "failed to enqueue episode"
                );
                continue;
            }
        }

        if let Err(e) = db
            .update_episode_state(&ep.id, &EpisodeState::Matched(action))
            .await
        {
            tracing::error!(
                slug = %feed.slug,
                episode_id = %ep.id,
                error = %e,
                "failed to set episode state"
            );
        }
    }

    info!(
        slug = %feed.slug,
        total = episodes.len(),
        new = new_count,
        "feed sync complete"
    );

    // Re-evaluate Dynamic episodes to enforce the window (promote or purge).
    purge_cycle(feed_id, &feed, &episodes, db, engine).await;

    // Re-evaluate pending episodes against updated rules, using the fresh
    // episode positions from this parse so LastNEpisodes is accurate.
    if recheck {
        recheck_feed(feed_id, &feed, &episodes, db, engine).await;
    }
}

/// Re-evaluate all `Discovered` and `Matched` episodes for a feed using the
/// freshly-parsed episode list so that position-based dynamic rules
/// (`LastNEpisodes`) see correct `episode_no` values.  Episodes outside the
/// current RSS feed get `episode_no = usize::MAX` (guaranteed out-of-window).
async fn recheck_feed(
    feed_id: FeedId,
    feed: &Feed,
    parsed_episodes: &[Episode],
    db: &Db,
    engine: &RuleEngine,
) {
    let episode_no_map: HashMap<&EpisodeId, usize> = parsed_episodes
        .iter()
        .map(|ep| (&ep.id, ep.episode_no))
        .collect();

    let all_eps = match db
        .list_episodes(EpisodeFilter {
            feed_id: Some(feed_id),
            ..Default::default()
        })
        .await
    {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(feed_id = ?feed_id, error = %e, "recheck: failed to load episodes");
            return;
        }
    };

    let mut reassigned = 0usize;
    for mut ep in all_eps {
        let old_action = match &ep.state {
            EpisodeState::Discovered => None,
            EpisodeState::Matched(a) => Some(*a),
            _ => continue, // Complete, Dynamic, Quarantined, Purged are not touched
        };

        ep.episode_no = episode_no_map.get(&ep.id).copied().unwrap_or(usize::MAX);
        let new_action = engine.evaluate(&ep, feed);

        if old_action == Some(new_action) {
            continue;
        }

        if let Err(e) = db
            .update_episode_state(&ep.id, &EpisodeState::Matched(new_action))
            .await
        {
            tracing::error!(episode_id = %ep.id, error = %e, "recheck: failed to update state");
            continue;
        }

        if matches!(new_action, Action::Download | Action::Dynamic) {
            db.enqueue(&ep.id, new_action).await.ok();
        } else {
            db.dequeue(&ep.id).await.ok();
        }

        info!(
            episode_id = %ep.id,
            title = %ep.title,
            old = ?old_action,
            new = ?new_action,
            "recheck: rule reassigned"
        );
        reassigned += 1;
    }

    if reassigned > 0 {
        info!(feed = %feed.slug, reassigned, "recheck complete");
    }
}

/// Fetch a feed's current episode list and run only the purge cycle —
/// re-evaluating Dynamic episodes without syncing or evaluating new ones.
/// Useful for the `dura purge` command.
pub async fn purge_one_feed(
    feed_cfg: &FeedConfig,
    db: &Db,
    engine: &RuleEngine,
    fetcher: &FeedFetcher,
) {
    let mut feed = feed_from_config(feed_cfg);

    let feed_id = match db.upsert_feed(&feed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(slug = %feed.slug, error = %e, "purge: failed to upsert feed");
            return;
        }
    };
    feed.id = feed_id;

    let (_, episodes) = match fetcher.fetch(&feed).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(slug = %feed.slug, error = %e, "purge: feed fetch failed");
            return;
        }
    };

    purge_cycle(feed_id, &feed, &episodes, db, engine).await;
}

/// For each `Dynamic`-state episode on this feed, re-run the rule engine using
/// the current feed snapshot (which has correct `episode_no` values).  Episodes
/// that have left the window are deleted from disk and transitioned to `Purged`;
/// episodes that a static rule would permanently keep are promoted to `Complete`.
async fn purge_cycle(
    feed_id: FeedId,
    feed: &Feed,
    parsed_episodes: &[Episode],
    db: &Db,
    engine: &RuleEngine,
) {
    // Build episode_no lookup from the freshly-parsed feed.  Episodes that
    // have aged out of the RSS feed entirely get usize::MAX, guaranteeing they
    // fail any LastNEpisodes threshold.
    let episode_no_map: HashMap<&EpisodeId, usize> = parsed_episodes
        .iter()
        .map(|ep| (&ep.id, ep.episode_no))
        .collect();

    let dynamic_eps = match db.list_dynamic_episodes(feed_id).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(
                feed_id = ?feed_id,
                error = %e,
                "failed to load dynamic episodes for purge cycle"
            );
            return;
        }
    };

    if dynamic_eps.is_empty() {
        return;
    }

    for mut ep in dynamic_eps {
        ep.episode_no = episode_no_map.get(&ep.id).copied().unwrap_or(usize::MAX);

        let action = engine.evaluate(&ep, feed);

        match action {
            Action::Dynamic => {
                // Still within the window — leave as-is.
            }
            Action::Download => {
                // A static rule claims this episode permanently — promote to
                // Complete so it survives future purge cycles.
                if let EpisodeState::Dynamic {
                    path,
                    sha256,
                    downloaded_at,
                } = ep.state
                {
                    let promoted = EpisodeState::Complete {
                        path,
                        sha256,
                        downloaded_at,
                    };
                    match db.update_episode_state(&ep.id, &promoted).await {
                        Ok(()) => info!(
                            episode_id = %ep.id,
                            title = %ep.title,
                            "dynamic episode promoted to complete"
                        ),
                        Err(e) => tracing::error!(
                            episode_id = %ep.id,
                            error = %e,
                            "failed to promote dynamic episode"
                        ),
                    }
                }
            }
            _ => {
                // Outside the window — delete the file and mark Purged.
                if let EpisodeState::Dynamic { ref path, .. } = ep.state {
                    if let Err(e) = tokio::fs::remove_file(path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(
                                episode_id = %ep.id,
                                path = %path.display(),
                                error = %e,
                                "could not delete dynamic episode file during purge"
                            );
                        }
                    }
                }
                let new_state = EpisodeState::Purged {
                    purged_at: Utc::now(),
                };
                match db.update_episode_state(&ep.id, &new_state).await {
                    Ok(()) => info!(
                        episode_id = %ep.id,
                        title = %ep.title,
                        "dynamic episode purged (outside window)"
                    ),
                    Err(e) => tracing::error!(
                        episode_id = %ep.id,
                        error = %e,
                        "failed to transition dynamic episode to purged"
                    ),
                }
            }
        }
    }
}
