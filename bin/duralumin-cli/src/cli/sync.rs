use anyhow::Result;
use clap::Args;
use duralumin_feed::FeedFetcher;
use duralumin_storage::Db;

use super::download::cmd_download;
use super::helpers::{build_engine, resolve_slug};
use crate::config;
use crate::feed_sync;
use crate::rss_gen;
use crate::rss_gen::build_rss_ctx;

#[derive(Args)]
pub struct SyncArgs {
    /// Feed slugs to sync (all enabled feeds if omitted).
    pub slugs: Vec<String>,
    /// Only refresh feed metadata; do not drain the download queue.
    #[arg(long)]
    pub feeds_only: bool,
    /// Re-evaluate all pending episodes against current rules.
    /// Safely re-assigns Matched(Skip) ↔ Matched(Download) without touching
    /// Complete or Quarantined episodes.
    #[arg(long)]
    pub recheck: bool,
}

pub async fn cmd_sync(cfg: &config::Config, db: &Db, args: SyncArgs) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    );

    let rss_ctx = build_rss_ctx(cfg);

    let resolved_slugs: Vec<String> = args.slugs.iter().map(|s| resolve_slug(cfg, s)).collect();

    let feeds_to_sync: Vec<_> = if resolved_slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| resolved_slugs.contains(&f.slug))
            .collect()
    };

    for feed_cfg in feeds_to_sync {
        feed_sync::sync_one_feed(feed_cfg, db, &engine, &fetcher, args.recheck).await;
        if feed_cfg.restream
            && let Some(ctx) = &rss_ctx
                && let Err(e) = rss_gen::generate_rss_for_feed(feed_cfg, db, ctx).await {
                    tracing::warn!(slug = %feed_cfg.slug, error = %e, "RSS generation failed after sync");
                }
    }

    if !args.feeds_only {
        cmd_download(cfg, db, vec![]).await?;
    }
    Ok(())
}
