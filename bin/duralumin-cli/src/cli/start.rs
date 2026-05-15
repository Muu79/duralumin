use std::sync::Arc;

use anyhow::{Context, Result};
use duralumin_downloader::{Downloader, DownloaderConfig};
use duralumin_feed::FeedFetcher;
use duralumin_storage::Db;
use tokio::sync::Semaphore;

use super::acquire_run_lock;
use super::download::drain_downloads;
use super::helpers::{build_engine, warn_missing_files};
use crate::config;
use crate::feed_sync;
use crate::rss_gen;

pub async fn cmd_start(cfg: &config::Config, db: &Db) -> Result<()> {
    let _lock = acquire_run_lock(&cfg.storage.db()).context("failed to acquire daemon lock")?;

    warn_missing_files(db).await;

    let rss_dir = cfg.storage.dir.join("rss");
    let images_dir = cfg.storage.dir.join("images");

    let rss_ctx: Option<Arc<rss_gen::RssContext>> = if cfg.feeds.iter().any(|f| f.restream) {
        match &cfg.server {
            Some(srv_cfg) => {
                let ctx = Arc::new(rss_gen::RssContext {
                    server_cfg: Arc::new(srv_cfg.clone()),
                    rss_dir: rss_dir.clone(),
                    images_dir: images_dir.clone(),
                    http: reqwest::Client::new(),
                });

                for feed_cfg in cfg.feeds.iter().filter(|f| f.restream && f.enabled) {
                    if let Err(e) = rss_gen::generate_rss_for_feed(feed_cfg, db, &ctx).await {
                        tracing::warn!(slug = %feed_cfg.slug, error = %e, "startup RSS generation failed");
                    }
                }

                let server_db = db.clone();
                let server_config = srv_cfg.clone();
                let rss_dir2 = rss_dir.clone();
                let images_dir2 = images_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        duralumin_server::serve(server_db, server_config, rss_dir2, images_dir2)
                            .await
                    {
                        tracing::error!(error = %e, "RSS server exited with error");
                    }
                });

                Some(ctx)
            }
            None => {
                tracing::warn!(
                    "some feeds have restream=true but no [server] block is configured — restreaming disabled"
                );
                None
            }
        }
    } else {
        None
    };

    let engine = Arc::new(build_engine(cfg, db).await?);
    let fetcher = Arc::new(FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    ));
    let downloader = Arc::new(Downloader::new(DownloaderConfig::from(&cfg.downloader)));
    let semaphore = Arc::new(Semaphore::new(cfg.downloader.concurrent_downloads as usize));
    let library = cfg.storage.library();

    let mut tasks = tokio::task::JoinSet::new();

    for feed_cfg in cfg.feeds.iter().filter(|f| f.enabled).cloned() {
        let db = db.clone();
        let engine = Arc::clone(&engine);
        let fetcher = Arc::clone(&fetcher);
        let rss_ctx = rss_ctx.clone();

        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(feed_cfg.poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                feed_sync::sync_one_feed(&feed_cfg, &db, &engine, &fetcher, false).await;
                if feed_cfg.restream
                    && let Some(ctx) = &rss_ctx
                        && let Err(e) = rss_gen::generate_rss_for_feed(&feed_cfg, &db, ctx).await {
                            tracing::warn!(slug = %feed_cfg.slug, error = %e, "RSS generation failed after sync");
                        }
            }
        });
    }

    {
        let db = db.clone();
        let downloader = Arc::clone(&downloader);
        let semaphore = Arc::clone(&semaphore);
        let library = library.clone();

        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                drain_downloads(&db, &downloader, &semaphore, &library).await;
            }
        });
    }

    while let Some(Err(e)) = tasks.join_next().await {
        tracing::error!(error = ?e, "a daemon task panicked");
    }
    Ok(())
}
