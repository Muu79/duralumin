use anyhow::Result;
use duralumin_feed::FeedFetcher;
use duralumin_storage::Db;

use super::helpers::build_engine;
use crate::config;
use crate::feed_sync;

pub async fn cmd_purge(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    );

    let feeds: Vec<_> = if slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| slugs.contains(&f.slug))
            .collect()
    };

    for feed_cfg in feeds {
        feed_sync::purge_one_feed(feed_cfg, db, &engine, &fetcher).await;
    }
    Ok(())
}
