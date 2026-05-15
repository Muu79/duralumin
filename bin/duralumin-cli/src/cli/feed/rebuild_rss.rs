use anyhow::{Context, Result};
use duralumin_rules::config::FeedConfig;
use duralumin_storage::Db;
use owo_colors::OwoColorize;

use crate::config;
use crate::rss_gen;
use crate::rss_gen::build_rss_ctx;

pub async fn cmd_rebuild_rss(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
    let ctx = build_rss_ctx(cfg)
        .with_context(|| "no [server] block in config — RSS restreaming not configured")?;

    let feeds: Vec<&FeedConfig> = if slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.restream).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| slugs.contains(&f.slug))
            .collect()
    };

    if feeds.is_empty() {
        println!("{}", "No restream-enabled feeds to rebuild.".dimmed());
        return Ok(());
    }

    for feed_cfg in feeds {
        rss_gen::generate_rss_for_feed(feed_cfg, db, &ctx)
            .await
            .with_context(|| format!("failed to regenerate RSS for {}", feed_cfg.slug))?;
        println!("{} {}", "Rebuilt".green(), feed_cfg.slug);
    }
    Ok(())
}
