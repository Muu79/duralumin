mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::Semaphore;
use tracing_subscriber::{EnvFilter, fmt};

use duralumin_core::{Action, EpisodeState};
use duralumin_downloader::{DownloadResult, Downloader, DownloaderConfig};
use duralumin_feed::FeedFetcher;
use duralumin_metadata::write_tags;
use duralumin_rules::RuleEngine;
use duralumin_storage::{Db, EpisodeFilter};

// ---- CLI types -------------------------------------------------------------

#[derive(Parser)]
#[command(name = "duralumin", version, about = "Podcast download daemon")]
struct Cli {
    #[arg(long, global = true, env = "DURALUMIN_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    log_format: Option<LogFormat>,
    #[arg(long, global = true)]
    log_level: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

#[derive(Subcommand)]
enum Command {
    Feed(FeedArgs),
    Episode(EpisodeArgs),
    Run(RunArgs),
    Download(DownloadArgs),
    Quarantine(QuarantineArgs),
    Rules(RulesArgs),
    Config(ConfigArgs),
    Db(DbArgs),
}

// ---- feed subcommand -------------------------------------------------------

#[derive(Args)]
struct FeedArgs {
    #[command(subcommand)]
    sub: FeedSub,
}

#[derive(Subcommand)]
enum FeedSub {
    /// Fetch one or more feeds and evaluate rules on new episodes.
    Sync {
        /// Feed slugs to sync (all enabled feeds if omitted).
        slugs: Vec<String>,
    },
    /// List all configured feeds and their last-fetched status.
    List,
}

// ---- episode subcommand ----------------------------------------------------

#[derive(Args)]
struct EpisodeArgs {
    #[command(subcommand)]
    sub: EpisodeSub,
}

#[derive(Subcommand)]
enum EpisodeSub {
    /// List episodes, optionally filtered by feed or state.
    List {
        #[arg(long)]
        feed: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Re-queue an episode for download (sets state to Matched(Download)).
    Requeue {
        id: String,
    },
}

// ---- run subcommand --------------------------------------------------------

#[derive(Args)]
struct RunArgs {
    /// Run once then exit (instead of looping).
    #[arg(long)]
    once: bool,
}

// ---- download subcommand ---------------------------------------------------

#[derive(Args)]
struct DownloadArgs {
    /// Episode IDs to download (download queue if omitted).
    ids: Vec<String>,
}

// ---- quarantine subcommand -------------------------------------------------

#[derive(Args)]
struct QuarantineArgs {
    #[command(subcommand)]
    sub: QuarantineSub,
}

#[derive(Subcommand)]
enum QuarantineSub {
    /// List all quarantined episodes.
    List,
    /// Re-queue a quarantined episode.
    Retry { id: String },
}

// ---- rules subcommand ------------------------------------------------------

#[derive(Args)]
struct RulesArgs {
    #[command(subcommand)]
    sub: RulesSub,
}

#[derive(Subcommand)]
enum RulesSub {
    /// Dry-run rule evaluation for all episodes in a feed.
    Check { slug: String },
}

// ---- config subcommand -----------------------------------------------------

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    sub: ConfigSub,
}

#[derive(Subcommand)]
enum ConfigSub {
    /// Parse and validate the config file, print OK or errors.
    Validate,
}

// ---- db subcommand ---------------------------------------------------------

#[derive(Args)]
struct DbArgs {
    #[command(subcommand)]
    sub: DbSub,
}

#[derive(Subcommand)]
enum DbSub {
    /// Run any pending database migrations.
    Migrate,
}

// ---- Entry point -----------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Config validate is special — no DB needed
    if let Command::Config(ConfigArgs { sub: ConfigSub::Validate }) = &cli.command {
        match config::load(cli.config.as_deref()) {
            Ok(_) => {
                println!("Config OK");
                std::process::exit(0);
            }
            Err(config::ConfigError::Generated(path)) => {
                println!("No config found — created a default at:");
                println!("  {}", path.display());
                println!("Edit it to add your feeds, then run duralumin again.");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Config error: {e}");
                std::process::exit(2);
            }
        }
    }

    let cfg = match config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(config::ConfigError::Generated(path)) => {
            println!("No config found — created a default at:");
            println!("  {}", path.display());
            println!();
            println!("Edit it to add your feeds, then run duralumin again.");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Config error: {e}");
            std::process::exit(2);
        }
    };

    let log_level = cli
        .log_level
        .as_deref()
        .unwrap_or(&cfg.logging.level)
        .to_string();
    let log_format = cli.log_format.as_ref();

    let filter = EnvFilter::try_new(&log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    match log_format {
        Some(LogFormat::Json) => {
            tracing_subscriber::fmt().json().with_env_filter(filter).init();
        }
        _ => {
            fmt().with_env_filter(filter).init();
        }
    }

    let db = match Db::open(&cfg.storage.state_db).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to open database");
            std::process::exit(1);
        }
    };

    if let Err(e) = run(cli.command, &cfg, &db).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

// ---- Command dispatch ------------------------------------------------------

async fn run(command: Command, cfg: &config::Config, db: &Db) -> Result<()> {
    match command {
        Command::Feed(FeedArgs { sub }) => match sub {
            FeedSub::Sync { slugs } => cmd_feed_sync(cfg, db, slugs).await,
            FeedSub::List => cmd_feed_list(db).await,
        },
        Command::Episode(EpisodeArgs { sub }) => match sub {
            EpisodeSub::List { feed, state, limit } => {
                cmd_episode_list(db, feed.as_deref(), state.as_deref(), limit).await
            }
            EpisodeSub::Requeue { id } => cmd_requeue(db, &id).await,
        },
        Command::Run(RunArgs { once }) => cmd_run(cfg, db, once).await,
        Command::Download(DownloadArgs { ids }) => cmd_download(cfg, db, ids).await,
        Command::Quarantine(QuarantineArgs { sub }) => match sub {
            QuarantineSub::List => cmd_quarantine_list(db).await,
            QuarantineSub::Retry { id } => cmd_requeue(db, &id).await,
        },
        Command::Rules(RulesArgs { sub }) => match sub {
            RulesSub::Check { slug } => cmd_rules_check(cfg, db, &slug).await,
        },
        Command::Config(ConfigArgs { sub }) => match sub {
            ConfigSub::Validate => unreachable!("handled before DB open"),
        },
        Command::Db(DbArgs { sub }) => match sub {
            DbSub::Migrate => {
                println!("Migrations applied.");
                Ok(())
            }
        },
    }
}

// ---- feed sync -------------------------------------------------------------

async fn cmd_feed_sync(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(&cfg.downloader.user_agent);

    let feeds_to_sync: Vec<&duralumin_rules::config::FeedConfig> = if slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| slugs.contains(&f.slug))
            .collect()
    };

    for feed_cfg in feeds_to_sync {
        let mut feed = feed_cfg.to_feed();

        // Ensure feed is persisted and has a real ID
        let feed_id = db
            .upsert_feed(&feed)
            .await
            .with_context(|| format!("upsert feed {}", feed_cfg.slug))?;
        feed.id = feed_id;

        tracing::info!(slug = %feed.slug, "syncing feed");

        let (meta, episodes) = match fetcher.fetch(&feed).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(slug = %feed.slug, error = %e, "feed fetch failed");
                continue;
            }
        };

        // Update feed metadata
        feed.title = meta.title.or(feed.title);
        feed.etag = meta.etag;
        feed.last_modified = meta.last_modified;
        feed.image_url = meta.image_url;
        feed.last_fetched_at = Some(Utc::now());
        db.upsert_feed(&feed).await?;

        tracing::info!(slug = %feed.slug, count = episodes.len(), "fetched episodes");

        for mut ep in episodes {
            ep.feed_id = feed_id;
            db.upsert_episode(&ep).await?;

            // Only evaluate rules on Discovered episodes
            if matches!(ep.state, EpisodeState::Discovered) {
                let action = engine.evaluate(&ep, &feed);
                let new_state = EpisodeState::Matched(action);
                db.update_episode_state(&ep.id, &new_state).await?;
                tracing::debug!(episode_id = %ep.id, ?action, "rule evaluated");
            }
        }
    }

    Ok(())
}

// ---- feed list -------------------------------------------------------------

async fn cmd_feed_list(db: &Db) -> Result<()> {
    let feeds = db.list_feeds().await?;
    if feeds.is_empty() {
        println!("No feeds in database.");
        return Ok(());
    }
    println!("{:<30} {:<50} {}", "SLUG", "URL", "LAST FETCHED");
    for f in feeds {
        let last = f
            .last_fetched_at
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".into());
        println!("{:<30} {:<50} {}", f.slug, f.url, last);
    }
    Ok(())
}

// ---- episode list ----------------------------------------------------------

async fn cmd_episode_list(
    db: &Db,
    feed_slug: Option<&str>,
    state_kind: Option<&str>,
    limit: usize,
) -> Result<()> {
    let feed_id = if let Some(slug) = feed_slug {
        db.get_feed_by_slug(slug)
            .await?
            .map(|f| f.id)
            .with_context(|| format!("feed {slug:?} not found"))?
    } else {
        // We'll filter later
        duralumin_core::FeedId(0)
    };

    let filter = duralumin_storage::EpisodeFilter {
        feed_id: feed_slug.map(|_| feed_id),
        state_kind: state_kind.map(str::to_owned),
        limit: Some(limit),
    };
    let episodes = db.list_episodes(filter).await?;

    println!("{:<12} {:<40} {}", "ID", "TITLE", "STATE");
    for ep in episodes {
        println!("{:<12} {:<40} {}", ep.id.short(), ep.title, ep.state);
    }
    Ok(())
}

// ---- requeue (shared by episode requeue + quarantine retry) ----------------

async fn cmd_requeue(db: &Db, id: &str) -> Result<()> {
    use duralumin_core::EpisodeId;
    let eid = EpisodeId::from(id.to_string());
    let ep = db
        .get_episode(&eid)
        .await?
        .with_context(|| format!("episode {id:?} not found"))?;
    db.update_episode_state(&ep.id, &EpisodeState::Matched(Action::Download))
        .await?;
    println!("Re-queued episode {}", ep.id.short());
    Ok(())
}

// ---- run -------------------------------------------------------------------

async fn cmd_run(cfg: &config::Config, db: &Db, once: bool) -> Result<()> {
    loop {
        cmd_feed_sync(cfg, db, vec![]).await?;
        cmd_download(cfg, db, vec![]).await?;
        if once {
            break;
        }
        // TODO: configurable poll interval / SIGHUP reload
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
    Ok(())
}

// ---- download --------------------------------------------------------------

async fn cmd_download(cfg: &config::Config, db: &Db, ids: Vec<String>) -> Result<()> {
    use duralumin_core::EpisodeId;

    let episodes = if ids.is_empty() {
        db.download_queue(cfg.downloader.max_retries).await?
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
        println!("Nothing to download.");
        return Ok(());
    }

    let dl_cfg = DownloaderConfig {
        concurrent_downloads: cfg.downloader.concurrent_downloads,
        attempt_timeout: cfg.downloader.attempt_timeout,
        max_retries: cfg.downloader.max_retries,
        backoff_base: cfg.downloader.backoff_base,
        user_agent: cfg.downloader.user_agent.clone(),
    };
    let downloader = Arc::new(Downloader::new(dl_cfg));
    let semaphore = Arc::new(Semaphore::new(cfg.downloader.concurrent_downloads as usize));
    let library = cfg.storage.library_path.clone();

    let mut handles = Vec::new();

    for episode in episodes {
        let dl = Arc::clone(&downloader);
        let sem = Arc::clone(&semaphore);
        let dest = library.clone();
        let ep = episode.clone();
        let db_pool = db.pool().clone();
        let feed_id = ep.feed_id;

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let local_db = Db::from_pool(db_pool);

            let feed = match local_db.get_feed(feed_id).await {
                Ok(Some(f)) => f,
                _ => {
                    tracing::error!(episode_id = %ep.id, "feed not found for episode");
                    return;
                }
            };

            let ep_id = ep.id.clone();
            let local_db2 = local_db;
            let result = dl
                .download(&ep, &dest, |state| {
                    // Fire-and-forget state updates from the closure
                    let id = ep_id.clone();
                    tracing::debug!(episode_id = %id, %state, "state update");
                })
                .await;

            match result {
                Ok(DownloadResult { path, sha256, .. }) => {
                    let complete = EpisodeState::Complete {
                        path: path.clone(),
                        downloaded_at: Utc::now(),
                        sha256,
                    };
                    if let Err(e) = local_db2.update_episode_state(&ep.id, &complete).await {
                        tracing::error!(episode_id = %ep.id, error = %e, "failed to persist Complete state");
                        return;
                    }

                    // Fetch cover art then write tags
                    let cover = fetch_cover(&ep, &feed).await;
                    if let Err(e) = write_tags(&path, &ep, &feed, cover.as_deref()) {
                        tracing::warn!(episode_id = %ep.id, error = %e, "tag write failed");
                    }
                }
                Err(e) => {
                    tracing::error!(episode_id = %ep.id, error = %e, "download failed after retries");
                }
            }
        });
        handles.push(handle);
    }

    let mut had_error = false;
    for h in handles {
        if let Err(e) = h.await {
            tracing::error!(error = ?e, "download task panicked");
            had_error = true;
        }
    }

    if had_error {
        bail!("one or more downloads failed");
    }

    Ok(())
}

// ---- quarantine list -------------------------------------------------------

async fn cmd_quarantine_list(db: &Db) -> Result<()> {
    let filter = EpisodeFilter {
        state_kind: Some("quarantined".into()),
        ..Default::default()
    };
    let episodes = db.list_episodes(filter).await?;
    if episodes.is_empty() {
        println!("No quarantined episodes.");
        return Ok(());
    }
    println!("{:<12} {:<40} {}", "ID", "TITLE", "REASON");
    for ep in episodes {
        let reason = if let EpisodeState::Quarantined { reason, .. } = &ep.state {
            reason.as_str()
        } else {
            "?"
        };
        println!("{:<12} {:<40} {}", ep.id.short(), ep.title, reason);
    }
    Ok(())
}

// ---- rules check -----------------------------------------------------------

async fn cmd_rules_check(cfg: &config::Config, db: &Db, slug: &str) -> Result<()> {
    let engine = build_engine(cfg, db).await?;

    let feed = db
        .get_feed_by_slug(slug)
        .await?
        .with_context(|| format!("feed {slug:?} not found in database — run `feed sync` first"))?;

    let filter = EpisodeFilter {
        feed_id: Some(feed.id),
        ..Default::default()
    };
    let episodes = db.list_episodes(filter).await?;

    println!("{:<12} {:<40} {}", "ID", "TITLE", "ACTION");
    for ep in &episodes {
        let action = engine.evaluate(ep, &feed);
        println!("{:<12} {:<40} {}", ep.id.short(), ep.title, action);
    }
    Ok(())
}

// ---- Helpers ---------------------------------------------------------------

/// Build a `RuleEngine` from the loaded config, upserting feeds first so they
/// have real IDs.
async fn build_engine(cfg: &config::Config, db: &Db) -> Result<RuleEngine> {
    let mut per_feed: Vec<(duralumin_core::FeedId, Vec<duralumin_rules::config::RuleConfig>)> =
        Vec::new();

    for feed_cfg in &cfg.feeds {
        let feed = feed_cfg.to_feed();
        let id = db
            .upsert_feed(&feed)
            .await
            .with_context(|| format!("upsert feed {}", feed_cfg.slug))?;

        let mut rules = feed_cfg.rules.clone();
        if let Some(action) = feed_cfg.default_action {
            // Append a synthetic catch-all that short-circuits global rules.
            rules.push(duralumin_rules::config::RuleConfig {
                name: format!("__feed_default_{}", feed_cfg.slug),
                priority: i32::MAX,
                match_: duralumin_rules::config::RuleKind::Always,
                action,
            });
        }
        per_feed.push((id, rules));
    }

    let pairs: Vec<(duralumin_core::FeedId, &[duralumin_rules::config::RuleConfig])> = per_feed
        .iter()
        .map(|(id, rules)| (*id, rules.as_slice()))
        .collect();

    let engine = RuleEngine::build(&pairs, &cfg.global_rules, cfg.defaults.action_on_no_match)
        .context("building rule engine")?;

    Ok(engine)
}

/// Fetch cover art bytes from `episode.image_url`, falling back to `feed.image_url`.
async fn fetch_cover(
    episode: &duralumin_core::Episode,
    feed: &duralumin_core::Feed,
) -> Option<Vec<u8>> {
    let url = episode.image_url.as_ref().or(feed.image_url.as_ref())?;
    let resp = match reqwest::get(url.as_str()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "cover art request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(url = %url, status = %resp.status(), "cover art fetch failed");
        return None;
    }
    match resp.bytes().await {
        Ok(b) => Some(b.to_vec()),
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "failed to read cover art bytes");
            None
        }
    }
}
