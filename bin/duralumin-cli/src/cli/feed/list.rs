use anyhow::Result;
use duralumin_feed::FeedFetcher;
use duralumin_storage::Db;
use owo_colors::OwoColorize;

use crate::cli::helpers::{build_engine, make_table};
use crate::config;
use crate::feed_sync;
use crate::tui;

pub async fn cmd_feed_list(cfg: &config::Config, db: &Db, interactive: bool) -> Result<()> {
    if interactive {
        let engine = build_engine(cfg, db).await?;
        let fetcher = FeedFetcher::new(
            &cfg.downloader.user_agent,
            cfg.downloader.accept_invalid_certs,
        );
        loop {
            match tui::run(cfg, db, None).await? {
                tui::Action::Quit => break,
                tui::Action::Sync(slug) => {
                    let feed_cfg = cfg.feeds.iter().find(|f| f.slug == slug);
                    if let Some(fc) = feed_cfg {
                        eprintln!("Syncing {}…", slug);
                        feed_sync::sync_one_feed(fc, db, &engine, &fetcher, false).await;
                    }
                }
            }
        }
        return Ok(());
    }

    let feeds = db.list_feeds().await?;
    if feeds.is_empty() {
        println!("{}", "No feeds in database.".dimmed());
        return Ok(());
    }
    let mut table = make_table();
    table.set_header(["SLUG", "NAME", "LAST FETCHED"]);
    for f in feeds {
        let last = f
            .last_fetched_at
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "never".into());
        let name = cfg
            .feeds
            .iter()
            .find(|c| c.slug == f.slug)
            .and_then(|c| c.display_name.as_deref())
            .or(f.title.as_deref())
            .unwrap_or("—");
        table.add_row([f.slug.as_str(), name, &last]);
    }
    println!("{table}");
    Ok(())
}
