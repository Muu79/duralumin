use anyhow::Result;
use chrono::Utc;
use duralumin_core::{EpisodeState, ext_from_mime, sanitize_title};
use duralumin_storage::{Db, EpisodeFilter};
use owo_colors::OwoColorize;

use crate::config;

pub async fn cmd_feed_reimport(cfg: &config::Config, db: &Db, slug: Option<&str>) -> Result<()> {
    let library = cfg.storage.library();
    let feeds = db.list_feeds().await?;

    let targets: Vec<_> = if let Some(s) = slug {
        let found: Vec<_> = feeds.iter().filter(|f| f.slug == s).collect();
        if found.is_empty() {
            anyhow::bail!("feed {s:?} not found — run `dura sync` first");
        }
        found
    } else {
        feeds.iter().collect()
    };

    let mut total_matched = 0usize;
    let mut total_already = 0usize;

    for feed in targets {
        let feed_dir = library.join(&feed.slug);
        if !feed_dir.exists() {
            println!(
                "{} {}",
                feed.slug.dimmed(),
                "— directory not found, skipping".dimmed()
            );
            continue;
        }

        let eps = db
            .list_episodes(EpisodeFilter {
                feed_id: Some(feed.id),
                ..Default::default()
            })
            .await?;

        let mut matched = 0usize;
        let mut already = 0usize;

        for ep in &eps {
            if matches!(&ep.state, EpisodeState::Complete { path, .. } if path.exists()) {
                already += 1;
                continue;
            }

            let stem = sanitize_title(&ep.title);
            let ext = ext_from_mime(ep.enclosure_mime.as_deref(), ep.enclosure_url.as_str());

            let candidates = [
                feed_dir.join(format!("{stem}.{ext}")),
                feed_dir.join(format!("{stem}-{}.{ext}", ep.id.short())),
            ];

            let found_path = candidates.iter().find(|p| p.exists());
            let Some(path) = found_path else { continue };

            let downloaded_at = tokio::fs::metadata(path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .map(chrono::DateTime::<Utc>::from)
                .unwrap_or_else(Utc::now);

            let state = EpisodeState::Complete {
                path: path.clone(),
                downloaded_at,
                sha256: String::new(),
            };
            if let Err(e) = db.update_episode_state(&ep.id, &state).await {
                tracing::warn!(episode_id = %ep.id, error = %e, "failed to update state");
                continue;
            }
            matched += 1;
        }

        let name = feed.title.as_deref().unwrap_or(&feed.slug);
        println!(
            "{name}: {} matched, {} already complete, {} not found",
            matched.to_string().green(),
            already.to_string().dimmed(),
            (eps.len() - matched - already).to_string().dimmed(),
        );
        total_matched += matched;
        total_already += already;
    }

    if total_matched > 0 {
        println!();
        println!(
            "Marked {} episode(s) as complete.",
            total_matched.to_string().green().bold()
        );
    } else if total_already == 0 {
        println!("No matching files found.");
    }
    Ok(())
}
