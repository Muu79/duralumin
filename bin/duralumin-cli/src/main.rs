mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::Semaphore;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use duralumin_core::{Action, EpisodeState};
use duralumin_downloader::{DownloadResult, Downloader, DownloaderConfig};
use duralumin_feed::FeedFetcher;
use duralumin_metadata::write_tags;
use duralumin_rules::{RuleEngine, config::FeedConfig};
use duralumin_server::ServerConfig;
use duralumin_storage::{Db, EpisodeFilter};
use rustls::crypto::aws_lc_rs;

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
    /// Start the RSS restream HTTP server (requires [server] in config).
    Serve,
    /// Show a summary of all feeds and their episode counts.
    Status,
    /// Check for Complete episodes whose local file has been deleted.
    Check(CheckArgs),
}

// ---- check subcommand ------------------------------------------------------

#[derive(Args)]
struct CheckArgs {
    /// Re-queue any missing episodes for download.
    #[arg(long)]
    fix: bool,
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
    /// Show detailed info and recent episodes for one feed.
    Info { slug: String },
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
    Requeue { id: String },
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
    /// Print all configured rules (global and per-feed).
    List,
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
    // rustls 0.23 requires an explicit crypto provider; install aws-lc-rs before
    // any TLS connection is attempted (reqwest 0.13 does not do this automatically).
    aws_lc_rs::default_provider().install_default().ok();

    let cli = Cli::parse();

    // Config validate is special — no DB needed
    if let Command::Config(ConfigArgs {
        sub: ConfigSub::Validate,
    }) = &cli.command
    {
        match config::load(cli.config.as_deref()) {
            Ok((_, path)) => {
                println!("Config OK ({})", path.display());
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

    let (cfg, config_path) = match config::load(cli.config.as_deref()) {
        Ok(pair) => pair,
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

    // Daemon commands use the configured log level; interactive commands default
    // to warn so info-level chatter from DB opens, migrations, etc. stays quiet.
    // --log-level always overrides regardless of command.
    let is_daemon = matches!(&cli.command, Command::Run(_) | Command::Serve);
    let default_level = if is_daemon { cfg.logging.level.as_str() } else { "warn" };
    let log_level = cli.log_level.as_deref().unwrap_or(default_level).to_string();
    let log_format = cli.log_format.as_ref();

    let filter = EnvFilter::try_new(&log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    match log_format {
        Some(LogFormat::Json) => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        _ => {
            fmt().with_env_filter(filter).init();
        }
    }

    info!(path = %config_path.display(), "loaded config");

    let db = match Db::open(&cfg.storage.db()).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to open database");
            std::process::exit(1);
        }
    };

    match db.count_quarantined().await {
        Ok(0) => {}
        Ok(n) => info!(
            count = n,
            "quarantined episodes present — run `quarantine list` to review"
        ),
        Err(e) => tracing::warn!(error = %e, "could not count quarantined episodes"),
    }

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
            FeedSub::Info { slug } => cmd_feed_info(db, &slug).await,
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
            RulesSub::List => cmd_rules_list(cfg),
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
        Command::Serve => cmd_serve(cfg, db).await,
        Command::Status => cmd_status(db).await,
        Command::Check(CheckArgs { fix }) => cmd_check(db, fix).await,
    }
}

// ---- feed sync -------------------------------------------------------------

async fn cmd_feed_sync(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    );

    let feeds_to_sync: Vec<&FeedConfig> = if slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds.iter().filter(|f| slugs.contains(&f.slug)).collect()
    };

    for feed_cfg in feeds_to_sync {
        sync_one_feed(feed_cfg, db, &engine, &fetcher).await;
    }
    Ok(())
}

/// Sync a single feed: fetch, upsert episodes, evaluate rules on new ones.
/// Errors are logged and swallowed so one bad feed never stops the others.
async fn sync_one_feed(
    feed_cfg: &FeedConfig,
    db: &Db,
    engine: &RuleEngine,
    fetcher: &FeedFetcher,
) {
    let mut feed = feed_cfg.to_feed();

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

    let mut new_episodes = 0;
    for ep in &mut episodes {
        ep.feed_id = feed_id;
        let is_new = match db.upsert_episode(ep).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(slug = %feed.slug, episode_id = %ep.id, error = %e, "failed to upsert episode");
                continue;
            }
        };
        if is_new {
            new_episodes += 1;
            let action = engine.evaluate(ep, &feed);
            info!(episode_id = %ep.id, title = %ep.title, ?action, "new episode, rule evaluated");
            if let Err(e) = db.update_episode_state(&ep.id, &EpisodeState::Matched(action)).await {
                tracing::error!(slug = %feed.slug, episode_id = %ep.id, error = %e, "failed to set episode state");
            }
        }
    }
    info!(slug = %feed.slug, total = episodes.len(), new = new_episodes, "feed sync complete");
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
    // Spawn the RSS server in the background if configured.
    if let Some(srv_cfg) = &cfg.server {
        let server_config = ServerConfig {
            bind: srv_cfg.bind,
            base_url: srv_cfg.base_url.clone(),
            auth_token: srv_cfg.auth_token.clone(),
        };
        let server_db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = duralumin_server::serve(server_db, server_config).await {
                tracing::error!(error = %e, "RSS server exited with error");
            }
        });
    }

    // --once: single linear pass then exit (for cron / systemd timer usage).
    if once {
        cmd_feed_sync(cfg, db, vec![]).await?;
        cmd_download(cfg, db, vec![]).await?;
        return Ok(());
    }

    // Daemon mode: build shared infrastructure once, then spawn independent tasks.
    let engine = Arc::new(build_engine(cfg, db).await?);
    let fetcher = Arc::new(FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    ));
    let downloader = Arc::new(Downloader::new(DownloaderConfig {
        concurrent_downloads: cfg.downloader.concurrent_downloads,
        attempt_timeout: cfg.downloader.attempt_timeout,
        max_retries: cfg.downloader.max_retries,
        backoff_base: cfg.downloader.backoff_base,
        user_agent: cfg.downloader.user_agent.clone(),
        accept_invalid_certs: cfg.downloader.accept_invalid_certs,
    }));
    let semaphore = Arc::new(Semaphore::new(cfg.downloader.concurrent_downloads as usize));
    let library = cfg.storage.library();
    let max_retries = cfg.downloader.max_retries;

    let mut tasks = tokio::task::JoinSet::new();

    // One task per enabled feed, each running on its own poll_interval.
    for feed_cfg in cfg.feeds.iter().filter(|f| f.enabled).cloned() {
        let db = db.clone();
        let engine = Arc::clone(&engine);
        let fetcher = Arc::clone(&fetcher);

        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(feed_cfg.poll_interval);
            // Skip missed ticks — if a sync runs long, don't burst on the next wake.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                sync_one_feed(&feed_cfg, &db, &engine, &fetcher).await;
            }
        });
    }

    // Download drain task — runs more frequently than the slowest feed interval
    // so new episodes get picked up promptly after a sync.
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
                drain_downloads(&db, &downloader, &semaphore, &library, max_retries).await;
            }
        });
    }

    while let Some(Err(e)) = tasks.join_next().await {
        tracing::error!(error = ?e, "a run task panicked");
    }
    Ok(())
}

// ---- serve -----------------------------------------------------------------

async fn cmd_serve(cfg: &config::Config, db: &Db) -> Result<()> {
    let srv_cfg = cfg
        .server
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no [server] block in config — add bind and base_url"))?;

    let server_config = ServerConfig {
        bind: srv_cfg.bind,
        base_url: srv_cfg.base_url.clone(),
        auth_token: srv_cfg.auth_token.clone(),
    };

    duralumin_server::serve(db.clone(), server_config).await?;
    Ok(())
}

// ---- download --------------------------------------------------------------

async fn cmd_download(cfg: &config::Config, db: &Db, ids: Vec<String>) -> Result<()> {
    use duralumin_core::EpisodeId;

    let dl_cfg = DownloaderConfig {
        concurrent_downloads: cfg.downloader.concurrent_downloads,
        attempt_timeout: cfg.downloader.attempt_timeout,
        max_retries: cfg.downloader.max_retries,
        backoff_base: cfg.downloader.backoff_base,
        user_agent: cfg.downloader.user_agent.clone(),
        accept_invalid_certs: cfg.downloader.accept_invalid_certs,
    };
    let downloader = Arc::new(Downloader::new(dl_cfg));
    let semaphore = Arc::new(Semaphore::new(cfg.downloader.concurrent_downloads as usize));
    let library = cfg.storage.library();

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

    run_downloads(db, &downloader, &semaphore, &library, episodes).await;
    Ok(())
}

/// Fetch the full download queue and process it. Used by the daemon loop.
async fn drain_downloads(
    db: &Db,
    downloader: &Arc<Downloader>,
    semaphore: &Arc<Semaphore>,
    library: &std::path::Path,
    max_retries: u8,
) {
    let episodes = match db.download_queue(max_retries).await {
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
async fn run_downloads(
    db: &Db,
    downloader: &Arc<Downloader>,
    semaphore: &Arc<Semaphore>,
    library: &std::path::Path,
    episodes: Vec<duralumin_core::Episode>,
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

            let ep_id = ep.id.clone();
            let result = dl
                .download(&ep, &feed_dir, |state| {
                    tracing::debug!(episode_id = %ep_id, %state, "state update");
                })
                .await;

            match result {
                Ok(DownloadResult { path, sha256, .. }) => {
                    let complete = EpisodeState::Complete {
                        path: path.clone(),
                        downloaded_at: Utc::now(),
                        sha256,
                    };
                    if let Err(e) = db.update_episode_state(&ep.id, &complete).await {
                        tracing::error!(episode_id = %ep.id, error = %e, "failed to persist Complete state");
                        return;
                    }
                    let cover = fetch_cover(&ep, &feed).await;
                    if let Err(e) = write_tags(&path, &ep, &feed, cover.as_deref()) {
                        tracing::warn!(episode_id = %ep.id, error = %e, "tag write failed");
                    }
                }
                Err(e) => {
                    tracing::error!(episode_id = %ep.id, error = %e, "download failed after retries");
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

// ---- status ----------------------------------------------------------------

async fn cmd_status(db: &Db) -> Result<()> {
    let feeds = db.list_feeds().await?;
    if feeds.is_empty() {
        println!("No feeds in database. Run `dura feed sync` first.");
        return Ok(());
    }

    println!(
        "{:<28} {:>5} {:>5} {:>6} {:>5} {:>5}",
        "FEED", "TOTAL", "DL", "QUEUED", "SKIP", "QUAR"
    );
    println!("{}", "-".repeat(58));

    for feed in &feeds {
        let eps = db
            .list_episodes(EpisodeFilter { feed_id: Some(feed.id), ..Default::default() })
            .await?;

        let mut dl = 0usize;
        let mut queued = 0usize;
        let mut skipped = 0usize;
        let mut quarantined = 0usize;

        for ep in &eps {
            match &ep.state {
                EpisodeState::Complete { .. } => dl += 1,
                EpisodeState::Matched(Action::Download) | EpisodeState::Failed { .. } => {
                    queued += 1
                }
                EpisodeState::Matched(Action::Skip) => skipped += 1,
                EpisodeState::Quarantined { .. } => quarantined += 1,
                _ => {}
            }
        }

        let name = feed.title.as_deref().unwrap_or(&feed.slug);
        let truncated = if name.len() > 27 { &name[..27] } else { name };
        println!(
            "{:<28} {:>5} {:>5} {:>6} {:>5} {:>5}",
            truncated,
            eps.len(),
            dl,
            queued,
            skipped,
            quarantined,
        );
    }
    Ok(())
}

// ---- feed info -------------------------------------------------------------

async fn cmd_feed_info(db: &Db, slug: &str) -> Result<()> {
    let feed = db
        .get_feed_by_slug(slug)
        .await?
        .with_context(|| format!("feed {slug:?} not found — run `dura feed sync` first"))?;

    let all_eps = db
        .list_episodes(EpisodeFilter { feed_id: Some(feed.id), ..Default::default() })
        .await?;

    let mut dl = 0usize;
    let mut queued = 0usize;
    let mut skipped = 0usize;
    let mut quarantined = 0usize;
    let mut missing = 0usize;

    for ep in &all_eps {
        match &ep.state {
            EpisodeState::Complete { path, .. } => {
                if path.exists() { dl += 1; } else { missing += 1; }
            }
            EpisodeState::Matched(Action::Download) | EpisodeState::Failed { .. } => queued += 1,
            EpisodeState::Matched(Action::Skip) => skipped += 1,
            EpisodeState::Quarantined { .. } => quarantined += 1,
            _ => {}
        }
    }

    let last = feed
        .last_fetched_at
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "never".into());

    println!("Feed:         {}", feed.title.as_deref().unwrap_or(&feed.slug));
    println!("Slug:         {}", feed.slug);
    println!("URL:          {}", feed.url);
    println!("Last fetched: {last}");
    println!();
    println!(
        "Episodes: {} total | {} downloaded | {} queued | {} skipped | {} quarantined{}",
        all_eps.len(), dl, queued, skipped, quarantined,
        if missing > 0 { format!(" | {missing} MISSING") } else { String::new() }
    );

    let recent: Vec<_> = all_eps.iter().take(20).collect();
    if !recent.is_empty() {
        println!();
        println!("{:<10} {:<12} {:<16} {}", "ID", "DATE", "STATE", "TITLE");
        println!("{}", "-".repeat(72));
        for ep in recent {
            let date = ep.pub_date.format("%Y-%m-%d").to_string();
            let state = ep.state.kind_name();
            let title = if ep.title.len() > 36 { &ep.title[..36] } else { &ep.title };
            println!("{:<10} {:<12} {:<16} {}", ep.id.short(), date, state, title);
        }
    }
    Ok(())
}

// ---- rules list ------------------------------------------------------------

fn cmd_rules_list(cfg: &config::Config) -> Result<()> {
    use duralumin_rules::config::RuleKind;

    fn fmt_kind(k: &RuleKind) -> String {
        match k {
            RuleKind::Always => "always".into(),
            RuleKind::TitleRegex { pattern } => format!("title ~ /{pattern}/"),
            RuleKind::DescriptionRegex { pattern } => format!("description ~ /{pattern}/"),
            RuleKind::DurationMin { value } => {
                format!("duration >= {}", humantime::format_duration(*value))
            }
            RuleKind::DurationMax { value } => {
                format!("duration <= {}", humantime::format_duration(*value))
            }
            RuleKind::PublishedAfter { date } => {
                format!("published after {}", date.format("%Y-%m-%d"))
            }
            RuleKind::PublishedBefore { date } => {
                format!("published before {}", date.format("%Y-%m-%d"))
            }
            RuleKind::EpisodeSizeMax { value } => format!("size <= {value}"),
        }
    }

    fn print_rules(rules: &[duralumin_rules::config::RuleConfig]) {
        for r in rules {
            println!(
                "  [pri {:>4}]  {:<28}  {:<40}  → {}",
                r.priority,
                r.name,
                fmt_kind(&r.match_),
                r.action,
            );
        }
    }

    println!("Global rules ({}):", cfg.global_rules.len());
    if cfg.global_rules.is_empty() {
        println!("  (none)");
    } else {
        print_rules(&cfg.global_rules);
    }

    for feed in &cfg.feeds {
        println!();
        println!("Feed: {} ({} rule(s)):", feed.slug, feed.rules.len());
        if feed.rules.is_empty() {
            println!("  (none — global rules apply)");
        } else {
            print_rules(&feed.rules);
        }
        if let Some(action) = feed.default_action {
            println!("  [catch-all]  default action: {action}");
        }
    }

    println!();
    println!("Default action (no match): {}", cfg.defaults.action_on_no_match);
    Ok(())
}

// ---- check -----------------------------------------------------------------

async fn cmd_check(db: &Db, fix: bool) -> Result<()> {
    let all = db
        .list_episodes(EpisodeFilter::default())
        .await?;

    let complete: Vec<_> = all
        .iter()
        .filter(|ep| matches!(&ep.state, EpisodeState::Complete { .. }))
        .collect();

    println!("Checking {} complete episode(s) for missing files...", complete.len());

    let mut missing = Vec::new();
    for ep in &complete {
        if let EpisodeState::Complete { path, .. } = &ep.state {
            if !path.exists() {
                missing.push((ep, path.clone()));
            }
        }
    }

    if missing.is_empty() {
        println!("All files present.");
        return Ok(());
    }

    println!();
    for (ep, path) in &missing {
        println!("  MISSING  {}  {}  {:?}", ep.id.short(), path.display(), ep.title);
    }
    println!();
    println!("{} missing file(s).", missing.len());

    if fix {
        for (ep, _) in &missing {
            db.update_episode_state(&ep.id, &EpisodeState::Matched(Action::Download))
                .await?;
        }
        println!("Re-queued {} episode(s) — run `dura download` to fetch.", missing.len());
    } else {
        println!("Run with --fix to re-queue them for download.");
    }
    Ok(())
}

// ---- Helpers ---------------------------------------------------------------

/// Build a `RuleEngine` from the loaded config, upserting feeds first so they
/// have real IDs.
async fn build_engine(cfg: &config::Config, db: &Db) -> Result<RuleEngine> {
    let mut per_feed: Vec<(
        duralumin_core::FeedId,
        Vec<duralumin_rules::config::RuleConfig>,
    )> = Vec::new();

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

    let pairs: Vec<(
        duralumin_core::FeedId,
        &[duralumin_rules::config::RuleConfig],
    )> = per_feed
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
