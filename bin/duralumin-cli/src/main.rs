mod config;
mod rss_gen;
mod sync;
mod tui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use comfy_table::{Cell, Color, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};
use owo_colors::{OwoColorize};
use tokio::sync::Semaphore;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, FeedId, ext_from_mime, sanitize_title};
use duralumin_downloader::{DownloadResult, Downloader, DownloaderConfig};
use duralumin_feed::FeedFetcher;
use duralumin_metadata::write_tags;
use duralumin_rules::config::{DynamicRuleConfig, DynamicRuleKind, RuleConfig};
use duralumin_rules::{RuleEngine, config::FeedConfig};
use duralumin_storage::{Db, EpisodeFilter};
use rustls::crypto::ring;
// ---- Config conversions ----------------------------------------------------

impl From<&config::DownloaderConfig> for DownloaderConfig {
    fn from(c: &config::DownloaderConfig) -> Self {
        Self {
            concurrent_downloads: c.concurrent_downloads,
            attempt_timeout: c.attempt_timeout,
            max_retries: c.max_retries,
            backoff_base: c.backoff_base,
            user_agent: c.user_agent.clone(),
            accept_invalid_certs: c.accept_invalid_certs,
            max_bytes_per_sec: c.max_bytes_per_sec,
        }
    }
}

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
    /// Force interactive (TUI) output even when stdout is not a terminal.
    #[arg(long, short = 'i', global = true)]
    interactive: bool,
    /// Force plain-text output even when stdout is a terminal (e.g. for scripts).
    #[arg(long, global = true)]
    no_interactive: bool,
    #[command(subcommand)]
    command: Command,
}

fn is_interactive(cli: &Cli) -> bool {
    use std::io::IsTerminal;
    if cli.no_interactive {
        false
    } else if cli.interactive {
        true
    } else {
        std::io::stdout().is_terminal()
    }
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
    /// Start the daemon: sync feeds on schedule, download matched episodes,
    /// and (if any feed has restream=true) serve the RSS restream server.
    Start,
    /// One-shot sync: refresh feeds and drain the download queue, then exit.
    /// Safe to run alongside a live `dura start` daemon.
    Sync(SyncArgs),
    Download(DownloadArgs),
    Quarantine(QuarantineArgs),
    Rules(RulesArgs),
    Config(ConfigArgs),
    Db(DbArgs),
    /// Show a summary of all feeds and their episode counts.
    Status,
    /// Check for Complete episodes whose local file has been deleted.
    Check(CheckArgs),
    /// Run the purge cycle for one or all feeds, deleting Dynamic episodes that
    /// have fallen outside their rolling window.
    Purge {
        /// Feed slugs to purge (all enabled feeds if omitted).
        slugs: Vec<String>,
    },
    /// Print a shell completion script to stdout.
    ///
    /// Usage: dura completions fish > ~/.config/fish/completions/dura.fish
    Completions {
        shell: Shell,
    },
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
    /// List all configured feeds and their last-fetched status.
    List,
    /// Show detailed info and recent episodes for one feed.
    Info { slug: String },
    /// Scan the library directory for existing files and mark matching
    /// episodes as Complete in the database. Useful after restoring a backup,
    /// migrating from another config, or recovering from a database mismatch.
    Reimport {
        /// Feed slug to scan, or omit to scan all feeds.
        slug: Option<String>,
    },
    /// Regenerate the static RSS file(s) served by the restream server.
    /// Useful after changing base_url, auth_token, or cover_image in config.
    RebuildRss {
        /// Feed slugs to rebuild (all restream-enabled feeds if omitted).
        slugs: Vec<String>,
    },
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
        /// Output id<TAB>title pairs for shell completion scripts.
        #[arg(long, hide = true)]
        completions: bool,
    },
    /// Re-queue an episode for download (sets state to Matched(Download)).
    Requeue { id: String },
    /// Remove an episode from the database.
    Delete {
        id: String,
        /// Also delete the downloaded file from disk.
        #[arg(long)]
        delete_file: bool,
    },
}

// ---- sync subcommand -------------------------------------------------------

#[derive(Args)]
struct SyncArgs {
    /// Feed slugs to sync (all enabled feeds if omitted).
    slugs: Vec<String>,
    /// Only refresh feed metadata; do not drain the download queue.
    #[arg(long)]
    feeds_only: bool,
    /// Re-evaluate all pending episodes against current rules.
    /// Safely re-assigns Matched(Skip) ↔ Matched(Download) without touching
    /// Complete or Quarantined episodes.
    #[arg(long)]
    recheck: bool,
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
    ring::default_provider().install_default().ok();

    let cli = Cli::parse();

    // Completions and config-validate need no DB or config.
    if let Command::Completions { shell } = &cli.command {
        use clap::CommandFactory;
        use clap_complete::generate;
        generate(*shell, &mut Cli::command(), "dura", &mut std::io::stdout());
        std::process::exit(0);
    }

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
    let is_daemon = matches!(&cli.command, Command::Start);
    let default_level = if is_daemon {
        cfg.logging.level.as_str()
    } else {
        "warn"
    };
    let log_level = cli
        .log_level
        .as_deref()
        .unwrap_or(default_level)
        .to_string();
    // CLI --log-format wins; fall back to config file format.
    let config_format = match cfg.logging.format.as_str() {
        "json" => Some(LogFormat::Json),
        _ => Some(LogFormat::Pretty),
    };
    let log_format = cli.log_format.as_ref().or(config_format.as_ref());

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

    let db_path = cfg.storage.db();
    // Create the database directory if it doesn't exist. We use create_dir_all
    // because the default path ({storage.dir}/db/) is two levels deep. If the
    // user has a typo in storage.dir the created path will be wrong — it will
    // be obvious immediately from the info log below.
    if let Some(db_dir) = db_path.parent()
        && !db_dir.exists()
    {
        if let Err(e) = std::fs::create_dir_all(db_dir) {
            eprintln!(
                "Error: failed to create database directory {}: {e}",
                db_dir.display()
            );
            std::process::exit(1);
        }
        info!(path = %db_dir.display(), "created database directory");
    }

    let db = match Db::open(&db_path).await {
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

    let interactive = is_interactive(&cli);
    if let Err(e) = run(cli.command, &cfg, &db, interactive).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

// ---- Slug resolution -------------------------------------------------------

/// Resolve a slug or alias to the canonical feed slug.
/// If the input matches a configured alias, returns the owning feed's slug.
/// If the input matches no feed at all, returns it unchanged (DB lookup will fail naturally).
fn resolve_slug(cfg: &config::Config, input: &str) -> String {
    for feed in &cfg.feeds {
        if feed.slug == input {
            return feed.slug.clone();
        }
        if feed.aliases.iter().any(|a| a == input) {
            return feed.slug.clone();
        }
    }
    input.to_owned()
}

// ---- Command dispatch ------------------------------------------------------

async fn run(command: Command, cfg: &config::Config, db: &Db, interactive: bool) -> Result<()> {
    match command {
        Command::Feed(FeedArgs { sub }) => match sub {
            FeedSub::List => cmd_feed_list(cfg, db, interactive).await,
            FeedSub::Info { slug } => {
                cmd_feed_info(cfg, db, &resolve_slug(cfg, &slug), interactive).await
            }
            FeedSub::Reimport { slug } => {
                let resolved = slug.as_deref().map(|s| resolve_slug(cfg, s));
                cmd_feed_reimport(cfg, db, resolved.as_deref()).await
            }
            FeedSub::RebuildRss { slugs } => {
                let resolved: Vec<String> = slugs.iter().map(|s| resolve_slug(cfg, s)).collect();
                cmd_rebuild_rss(cfg, db, resolved).await
            }
        },
        Command::Episode(EpisodeArgs { sub }) => match sub {
            EpisodeSub::List {
                feed,
                state,
                limit,
                completions,
            } => {
                let resolved_feed = feed.as_deref().map(|s| resolve_slug(cfg, s));
                cmd_episode_list(
                    db,
                    resolved_feed.as_deref(),
                    state.as_deref(),
                    limit,
                    completions,
                )
                .await
            }
            EpisodeSub::Requeue { id } => cmd_requeue(db, &id).await,
            EpisodeSub::Delete { id, delete_file } => {
                cmd_episode_delete(db, &id, delete_file).await
            }
        },
        Command::Start => cmd_start(cfg, db).await,
        Command::Sync(args) => cmd_sync(cfg, db, args).await,
        Command::Download(DownloadArgs { ids }) => cmd_download(cfg, db, ids).await,
        Command::Quarantine(QuarantineArgs { sub }) => match sub {
            QuarantineSub::List => cmd_quarantine_list(db).await,
            QuarantineSub::Retry { id } => cmd_requeue(db, &id).await,
        },
        Command::Rules(RulesArgs { sub }) => match sub {
            RulesSub::Check { slug } => cmd_rules_check(cfg, db, &resolve_slug(cfg, &slug)).await,
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
        Command::Status => cmd_status(cfg, db).await,
        Command::Check(CheckArgs { fix }) => cmd_check(db, fix).await,
        Command::Purge { slugs } => {
            let resolved: Vec<String> = slugs.iter().map(|s| resolve_slug(cfg, s)).collect();
            cmd_purge(cfg, db, resolved).await
        }
        Command::Completions { .. } => unreachable!("handled before DB open"),
    }
}

// ---- sync (one-shot pipeline) ----------------------------------------------

async fn cmd_sync(cfg: &config::Config, db: &Db, args: SyncArgs) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    );

    let rss_ctx = build_rss_ctx(cfg);

    // Resolve any aliases to canonical slugs before filtering.
    let resolved_slugs: Vec<String> = args.slugs.iter().map(|s| resolve_slug(cfg, s)).collect();

    let feeds_to_sync: Vec<&FeedConfig> = if resolved_slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| resolved_slugs.contains(&f.slug))
            .collect()
    };

    for feed_cfg in feeds_to_sync {
        sync::sync_one_feed(feed_cfg, db, &engine, &fetcher, args.recheck).await;
        if feed_cfg.restream {
            if let Some(ctx) = &rss_ctx {
                if let Err(e) = rss_gen::generate_rss_for_feed(feed_cfg, db, ctx).await {
                    tracing::warn!(slug = %feed_cfg.slug, error = %e, "RSS generation failed after sync");
                }
            }
        }
    }

    if !args.feeds_only {
        cmd_download(cfg, db, vec![]).await?;
    }
    Ok(())
}


// ---- feed list -------------------------------------------------------------

async fn cmd_feed_list(cfg: &config::Config, db: &Db, interactive: bool) -> Result<()> {
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
                        sync::sync_one_feed(fc, db, &engine, &fetcher, false).await;
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
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
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

// ---- episode list ----------------------------------------------------------

async fn cmd_episode_list(
    db: &Db,
    feed_slug: Option<&str>,
    state_kind: Option<&str>,
    limit: usize,
    completions: bool,
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

    // Hidden completions mode: print id<TAB>title for shell scripts.
    if completions {
        for ep in &episodes {
            println!("{}\t{}", ep.id.short(), ep.title);
        }
        return Ok(());
    }

    let mut table = make_table();
    table.set_header(["ID", "TITLE", "STATE"]);
    for ep in episodes {
        let kind = ep.state.kind_name().to_lowercase();
        table.add_row([
            Cell::new(ep.id.short()),
            Cell::new(&ep.title),
            state_cell(&kind),
        ]);
    }
    println!("{table}");
    Ok(())
}

// ---- requeue (shared by episode requeue + quarantine retry) ----------------

async fn cmd_requeue(db: &Db, id: &str) -> Result<()> {
    let eid = EpisodeId::from(id.to_string());
    let ep = db
        .get_episode(&eid)
        .await?
        .with_context(|| format!("episode {id:?} not found"))?;
    db.update_episode_state(&ep.id, &EpisodeState::Matched(Action::Download))
        .await?;
    db.enqueue(&ep.id, Action::Download).await?;
    println!("{} episode {}", "Re-queued".green(), ep.id.short());
    Ok(())
}

// ---- start (daemon) --------------------------------------------------------

async fn cmd_start(cfg: &config::Config, db: &Db) -> Result<()> {
    // Prevent two daemons running at once.
    let _lock = acquire_run_lock(&cfg.storage.db()).context("failed to acquire daemon lock")?;

    // Warn about Complete episodes whose files have gone missing.
    warn_missing_files(db).await;

    let rss_dir = cfg.storage.dir.join("rss");
    let images_dir = cfg.storage.dir.join("images");

    // Build RssContext if the server is configured and any feed uses restream.
    let rss_ctx: Option<Arc<rss_gen::RssContext>> =
        if cfg.feeds.iter().any(|f| f.restream) {
            match &cfg.server {
                Some(srv_cfg) => {
                    let ctx = Arc::new(rss_gen::RssContext {
                        server_cfg: Arc::new(srv_cfg.clone()),
                        rss_dir: rss_dir.clone(),
                        images_dir: images_dir.clone(),
                        http: reqwest::Client::new(),
                    });

                    // Pre-generate RSS for all restream feeds on startup.
                    for feed_cfg in cfg.feeds.iter().filter(|f| f.restream && f.enabled) {
                        if let Err(e) = rss_gen::generate_rss_for_feed(feed_cfg, db, &ctx).await {
                            tracing::warn!(slug = %feed_cfg.slug, error = %e, "startup RSS generation failed");
                        }
                    }

                    // Start the HTTP server.
                    let server_db = db.clone();
                    let server_config = srv_cfg.clone();
                    let rss_dir2 = rss_dir.clone();
                    let images_dir2 = images_dir.clone();
                    tokio::spawn(async move {
                        if let Err(e) = duralumin_server::serve(server_db, server_config, rss_dir2, images_dir2).await {
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

    // One task per enabled feed, each running on its own poll_interval.
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
                sync::sync_one_feed(&feed_cfg, &db, &engine, &fetcher, false).await;
                if feed_cfg.restream {
                    if let Some(ctx) = &rss_ctx {
                        if let Err(e) = rss_gen::generate_rss_for_feed(&feed_cfg, &db, ctx).await {
                            tracing::warn!(slug = %feed_cfg.slug, error = %e, "RSS generation failed after sync");
                        }
                    }
                }
            }
        });
    }

    // Download drain task — runs every 30s so new episodes get picked up quickly.
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

// ---- download --------------------------------------------------------------

async fn cmd_download(cfg: &config::Config, db: &Db, ids: Vec<String>) -> Result<()> {
    use duralumin_core::EpisodeId;

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
        println!("{}", "Nothing to download.".dimmed());
        return Ok(());
    }

    run_downloads(db, &downloader, &semaphore, &library, episodes).await;
    Ok(())
}

/// Fetch the download queue and process it. Used by the daemon loop.
async fn drain_downloads(
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
async fn run_downloads(
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

// ---- quarantine list -------------------------------------------------------

async fn cmd_quarantine_list(db: &Db) -> Result<()> {
    let filter = EpisodeFilter {
        state_kind: Some("quarantined".into()),
        ..Default::default()
    };
    let episodes = db.list_episodes(filter).await?;
    if episodes.is_empty() {
        println!("{}", "No quarantined episodes.".dimmed());
        return Ok(());
    }
    let mut table = make_table();
    table.set_header(["ID", "TITLE", "REASON"]);
    for ep in episodes {
        let reason = if let EpisodeState::Quarantined { reason, .. } = &ep.state {
            reason.clone()
        } else {
            "?".into()
        };
        table.add_row([
            Cell::new(ep.id.short()),
            Cell::new(&ep.title),
            Cell::new(&reason).fg(Color::Red),
        ]);
    }
    println!("{table}");
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

    let mut table = make_table();
    table.set_header(["ID", "TITLE", "ACTION"]);
    for ep in &episodes {
        let action = engine.evaluate(ep, &feed);
        let action_str = action.to_string();
        let action_cell = match action_str.as_str() {
            "download" => Cell::new(&action_str).fg(Color::Green),
            "skip" => Cell::new(&action_str).fg(Color::DarkGrey),
            _ => Cell::new(&action_str),
        };
        table.add_row([Cell::new(ep.id.short()), Cell::new(&ep.title), action_cell]);
    }
    println!("{table}");
    Ok(())
}

// ---- status ----------------------------------------------------------------

async fn cmd_status(cfg: &config::Config, db: &Db) -> Result<()> {
    let feeds = db.list_feeds().await?;
    if feeds.is_empty() {
        println!(
            "{}",
            "No feeds in database. Run `dura feed sync` first.".dimmed()
        );
        return Ok(());
    }

    let mut table = make_table();
    table.set_header(["FEED", "TOTAL", "DL(DYN)", "QUEUED", "SKIP", "QUAR"]);

    for feed in &feeds {
        let eps = db
            .list_episodes(EpisodeFilter {
                feed_id: Some(feed.id),
                ..Default::default()
            })
            .await?;

        let c = EpisodeCounts::tally(&eps);
        // Display name: config display_name > RSS title > slug
        let name = cfg
            .feeds
            .iter()
            .find(|c| c.slug == feed.slug)
            .and_then(|c| c.display_name.as_deref())
            .or(feed.title.as_deref())
            .unwrap_or(&feed.slug);
        let quar_cell = if c.quarantined > 0 {
            Cell::new(c.quarantined).fg(Color::Red)
        } else {
            Cell::new(c.quarantined)
        };
        let queued_cell = if c.queued > 0 {
            Cell::new(c.queued).fg(Color::Yellow)
        } else {
            Cell::new(c.queued)
        };
        table.add_row([
            Cell::new(name),
            Cell::new(eps.len()),
            Cell::new(format!("{}({})", c.dl.green(), c.dynamic.blue())),
            queued_cell,
            Cell::new(c.skipped).fg(Color::DarkGrey),
            quar_cell,
        ]);
    }
    println!("{table}");
    Ok(())
}

// ---- feed info -------------------------------------------------------------

async fn cmd_feed_info(cfg: &config::Config, db: &Db, slug: &str, interactive: bool) -> Result<()> {
    if interactive {
        tui::run(cfg, db, Some(slug.to_string())).await?;
        return Ok(());
    }

    let feed = db
        .get_feed_by_slug(slug)
        .await?
        .with_context(|| format!("feed {slug:?} not found — run `dura feed sync` first"))?;

    let config_feed = cfg.feeds.iter().find(|c| c.slug == slug);

    let all_eps = db
        .list_episodes(EpisodeFilter {
            feed_id: Some(feed.id),
            ..Default::default()
        })
        .await?;

    let c = EpisodeCounts::tally(&all_eps);

    let last = feed
        .last_fetched_at
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "never".into());

    // Display name: config display_name > RSS title > slug
    let display = config_feed
        .and_then(|c| c.display_name.as_deref())
        .or(feed.title.as_deref())
        .unwrap_or(&feed.slug);

    println!("{} {}", display.bold(), format!("({})", feed.slug).dimmed());
    println!("  {} {}", "URL:".dimmed(), feed.url);
    println!("  {} {}", "Last fetched:".dimmed(), last);
    if let Some(cf) = config_feed
        && !cf.aliases.is_empty()
    {
        println!("  {} {}", "Aliases:".dimmed(), cf.aliases.join(", "));
    }
    println!();

    // Summary counts
    let mut summary = make_table();
    summary.set_header(["TOTAL", "DL", "QUEUED", "SKIP", "QUAR", "MISSING"]);
    let miss_cell = if c.missing > 0 {
        Cell::new(c.missing).fg(Color::Red)
    } else {
        Cell::new(c.missing)
    };
    let quar_cell = if c.quarantined > 0 {
        Cell::new(c.quarantined).fg(Color::Red)
    } else {
        Cell::new(c.quarantined)
    };
    let queued_cell = if c.queued > 0 {
        Cell::new(c.queued).fg(Color::Yellow)
    } else {
        Cell::new(c.queued)
    };
    summary.add_row([
        Cell::new(all_eps.len()),
        Cell::new(c.dl).fg(Color::Green),
        queued_cell,
        Cell::new(c.skipped).fg(Color::DarkGrey),
        quar_cell,
        miss_cell,
    ]);
    println!("{summary}");

    let recent: Vec<_> = all_eps.iter().take(20).collect();
    if !recent.is_empty() {
        println!();
        println!("{}", "Recent episodes:".bold());
        let mut table = make_table();
        table.set_header(["ID", "DATE", "STATE", "TITLE"]);
        for ep in recent {
            let date = ep.pub_date.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
            let kind = ep.state.kind_name().to_lowercase();
            table.add_row([
                Cell::new(ep.id.short()),
                Cell::new(&date),
                state_cell(&kind),
                Cell::new(&ep.title),
            ]);
        }
        println!("{table}");
    }
    Ok(())
}

// ---- feed reimport ---------------------------------------------------------

async fn cmd_feed_reimport(cfg: &config::Config, db: &Db, slug: Option<&str>) -> Result<()> {
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

            // Primary filename, then the dedup variant with short ID suffix.
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

    fn fmt_dynamic_kind(dk: &DynamicRuleKind) -> String {
        match dk {
            DynamicRuleKind::DurationAgo { duration } => {
                format!(
                    "published within the last {}",
                    humantime::format_duration(duration.to_std().unwrap())
                )
            }
            DynamicRuleKind::LastNEpisodes { last_n_episodes: n } => {
                format!("among the {} most recent episodes", n)
            }
        }
    }

    fn rules_table(rules: &[RuleConfig]) -> Table {
        let mut t = make_table();
        t.set_header(["PRI", "NAME", "MATCH", "ACTION"]);
        for r in rules {
            let action_str = r.action.to_string();
            let action_cell = match action_str.as_str() {
                "download" => Cell::new(&action_str).fg(Color::Green),
                "skip" => Cell::new(&action_str).fg(Color::DarkGrey),
                _ => Cell::new(&action_str),
            };
            t.add_row([
                Cell::new(r.priority),
                Cell::new(&r.name),
                Cell::new(fmt_kind(&r.match_)),
                action_cell,
            ]);
        }
        t
    }

    fn dynamic_rules_table(dyn_rules: &[DynamicRuleConfig]) -> Table {
        let mut t = make_table();
        t.set_header(["NAME", "MATCH"]);
        for dr in dyn_rules {
            t.add_row([Cell::new(&dr.name), Cell::new(fmt_dynamic_kind(&dr.match_))]);
        }
        t
    }

    println!("{}", "Global Dynamic Rules".bold());
    if cfg.global_dynamics.is_empty() {
        println!("\t{}", "(none)".dimmed())
    } else {
        println!("{}", dynamic_rules_table(&cfg.global_dynamics))
    }

    println!("{}", "Global rules:".bold());
    if cfg.global_rules.is_empty() {
        println!("\t{}", "(none)".dimmed());
    } else {
        println!("{}", rules_table(&cfg.global_rules));
    }

    for feed in &cfg.feeds {
        println!();
        let feed_label = feed.display_name.as_deref().unwrap_or(&feed.slug);
        if feed.display_name.is_some() {
            println!(
                "{} {} {}",
                "Feed:".bold(),
                feed_label,
                format!("({})", feed.slug).dimmed()
            );
        } else {
            println!("{} {}", "Feed:".bold(), feed_label);
        }

        if feed.rules.is_empty() {
            println!("\t{}", "(none — global rules apply)".dimmed());
        } else {
            println!("{}", rules_table(&feed.rules));
        }

        println!("{}", "Dynamic rules:".bold());
        if feed.dynamic.is_empty() {
            println!("\t{}", "(none)".dimmed())
        } else {
            println!("{}", dynamic_rules_table(&feed.dynamic))
        }
        if let Some(action) = feed.default_action {
            println!("  {} {}", "catch-all:".dimmed(), action);
        }
    }

    println!();
    println!(
        "{} {}",
        "Default (no match):".dimmed(),
        cfg.defaults.action_on_no_match
    );
    Ok(())
}

// ---- check -----------------------------------------------------------------

async fn cmd_check(db: &Db, fix: bool) -> Result<()> {
    let all = db.list_episodes(EpisodeFilter::default()).await?;

    let complete: Vec<_> = all
        .iter()
        .filter(|ep| {
            matches!(
                &ep.state,
                EpisodeState::Complete { .. } | EpisodeState::Dynamic { .. }
            )
        })
        .collect();

    println!(
        "Checking {} complete episode(s) for missing files...",
        complete.len()
    );

    let mut missing = Vec::new();
    for ep in &complete {
        if let EpisodeState::Complete { path, .. } = &ep.state
            && !path.exists()
        {
            missing.push((ep, path.clone()));
        }
    }

    if missing.is_empty() {
        println!("{}", "All files present.".green());
        return Ok(());
    }

    let mut table = make_table();
    table.set_header(["ID", "TITLE", "PATH"]);
    for (ep, path) in &missing {
        table.add_row([
            Cell::new(ep.id.short()).fg(Color::Red),
            Cell::new(&ep.title),
            Cell::new(path.display().to_string()).fg(Color::DarkGrey),
        ]);
    }
    println!("{table}");
    println!("{} missing file(s).", missing.len().to_string().red());

    if fix {
        for (ep, _) in &missing {
            db.update_episode_state(&ep.id, &EpisodeState::Matched(Action::Download))
                .await?;
            db.enqueue(&ep.id, Action::Download).await?;
        }
        println!(
            "{} {} episode(s) — run `dura download` to fetch.",
            "Re-queued".green(),
            missing.len()
        );
    } else {
        println!(
            "{}",
            "Run with --fix to re-queue them for download.".dimmed()
        );
    }
    Ok(())
}

// ---- Startup file check ----------------------------------------------------

async fn warn_missing_files(db: &Db) {
    let all = match db.list_episodes(EpisodeFilter::default()).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::warn!(error = %e, "could not load episodes for file check");
            return;
        }
    };
    let mut missing = 0usize;
    for ep in &all {
        if let EpisodeState::Complete { path, .. } = &ep.state
            && !path.exists()
        {
            tracing::warn!(
                episode_id = %ep.id,
                path = %path.display(),
                "file missing for Complete episode"
            );
            missing += 1;
        }
    }
    if missing > 0 {
        tracing::warn!(
            count = missing,
            "run `dura check --fix` to requeue missing episodes for re-download"
        );
    }
}

// ---- Episode delete --------------------------------------------------------

async fn cmd_episode_delete(db: &Db, id: &str, delete_file: bool) -> Result<()> {
    use duralumin_core::EpisodeId;
    let eid = EpisodeId::from(id.to_string());
    let ep = db
        .get_episode(&eid)
        .await?
        .with_context(|| format!("episode {id:?} not found"))?;

    if delete_file && let EpisodeState::Complete { path, .. } = &ep.state {
        if path.exists() {
            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("failed to delete {:?}", path))?;
            println!("  {} {}", "Deleted file:".dimmed(), path.display());
        } else {
            println!("  {}", "File already missing from disk.".dimmed());
        }
    }

    db.delete_episode(&ep.id).await?;
    println!("{} {} — {}", "Removed".red(), ep.id.short(), ep.title);
    Ok(())
}

// ---- Single-instance lock --------------------------------------------------

/// Write a PID file next to the DB. Returns a guard that removes it on drop.
/// Errors if another `dura start` process appears to be running.
fn acquire_run_lock(db_path: &std::path::Path) -> Result<RunLockGuard> {
    let pid_path = db_path.with_file_name("dura.pid");

    if let Ok(content) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = content.trim().parse::<u32>()
        && pid != std::process::id()
        && is_running(pid)
    {
        anyhow::bail!(
            "`dura start` is already running (PID {pid}).\n  \
                     Stop it first, or remove {:?} if the file is stale.",
            pid_path
        );
    }

    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("failed to write PID file {:?}", pid_path))?;

    Ok(RunLockGuard { path: pid_path })
}

struct RunLockGuard {
    path: std::path::PathBuf,
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn is_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // signal 0: no signal sent, just checks if the process exists
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let _ = pid;
        false // conservative: stale PID files won't block restart on Windows
    }
}

// ---- Output helpers --------------------------------------------------------

#[derive(Default)]
struct EpisodeCounts {
    dl: usize,
    dynamic: usize,
    queued: usize,
    skipped: usize,
    quarantined: usize,
    missing: usize,
}

impl EpisodeCounts {
    fn tally(episodes: &[Episode]) -> Self {
        let mut c = Self::default();
        for ep in episodes {
            match &ep.state {
                EpisodeState::Complete { path, .. } => {
                    if path.exists() {
                        c.dl += 1;
                    } else {
                        c.missing += 1;
                    }
                },
                EpisodeState::Dynamic { path, .. } => {
                    if path.exists() {
                        c.dl += 1;
                        c.dynamic += 1;
                    } else {
                        c.missing += 1;
                    }
                },
                EpisodeState::Matched(Action::Download | Action::Dynamic) | EpisodeState::Failed { .. } => {
                    c.queued += 1
                }
                EpisodeState::Matched(Action::Skip | Action::Purge) | EpisodeState::Purged { .. } => c.skipped += 1,
                EpisodeState::Quarantined { .. } => c.quarantined += 1,
                _ => {}
            }
        }
        c
    }
}

fn make_table() -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    t
}

fn state_cell(label: &str) -> Cell {
    match label {
        "complete" => Cell::new(label).fg(Color::Green),
        "quarantined" => Cell::new(label).fg(Color::Red),
        "failed" => Cell::new(label).fg(Color::Red),
        "matched" => Cell::new(label).fg(Color::Yellow),
        "downloading" => Cell::new(label).fg(Color::Cyan),
        _ => Cell::new(label),
    }
}

// ---- RSS generation helpers ------------------------------------------------

/// Build an `RssContext` if the config has a `[server]` block; returns `None` otherwise.
fn build_rss_ctx(cfg: &config::Config) -> Option<rss_gen::RssContext> {
    cfg.server.as_ref().map(|srv_cfg| rss_gen::RssContext {
        server_cfg: std::sync::Arc::new(srv_cfg.clone()),
        rss_dir: cfg.storage.dir.join("rss"),
        images_dir: cfg.storage.dir.join("images"),
        http: reqwest::Client::new(),
    })
}

async fn cmd_rebuild_rss(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
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

async fn cmd_purge(cfg: &config::Config, db: &Db, slugs: Vec<String>) -> Result<()> {
    let engine = build_engine(cfg, db).await?;
    let fetcher = FeedFetcher::new(
        &cfg.downloader.user_agent,
        cfg.downloader.accept_invalid_certs,
    );

    let feeds: Vec<&FeedConfig> = if slugs.is_empty() {
        cfg.feeds.iter().filter(|f| f.enabled).collect()
    } else {
        cfg.feeds
            .iter()
            .filter(|f| slugs.contains(&f.slug))
            .collect()
    };

    for feed_cfg in feeds {
        sync::purge_one_feed(feed_cfg, db, &engine, &fetcher).await;
    }
    Ok(())
}

// ---- Helpers ---------------------------------------------------------------

/// Construct a placeholder `Feed` (id = 0, no metadata) from config for initial upsert.
fn feed_from_config(fc: &FeedConfig) -> duralumin_core::Feed {
    duralumin_core::Feed {
        id: duralumin_core::FeedId(0),
        url: fc.url.clone(),
        slug: fc.slug.clone(),
        title: None,
        last_fetched_at: None,
        etag: None,
        last_modified: None,
        enabled: fc.enabled,
        image_url: None,
    }
}

/// Build a `RuleEngine` from the loaded config, upserting feeds first so they
/// have real IDs.
async fn build_engine(cfg: &config::Config, db: &Db) -> Result<RuleEngine> {
    let mut per_feed: Vec<(duralumin_core::FeedId, Vec<RuleConfig>)> = Vec::new();

    let mut dyn_per_feed = Vec::new();

    for feed_cfg in &cfg.feeds {
        let feed = feed_from_config(feed_cfg);
        let id = db
            .upsert_feed(&feed)
            .await
            .with_context(|| format!("upsert feed {}", feed_cfg.slug))?;

        let mut rules = feed_cfg.rules.clone();
        let dyn_rules = feed_cfg.dynamic.clone();
        if let Some(action) = feed_cfg.default_action {
            // Append a synthetic catch-all that short-circuits global rules.
            rules.push(RuleConfig {
                name: format!("__feed_default_{}", feed_cfg.slug),
                priority: i32::MAX,
                match_: duralumin_rules::config::RuleKind::Always,
                action,
            });
        }
        per_feed.push((id, rules));
        dyn_per_feed.push((id, dyn_rules));
    }

    let pairs: Vec<(FeedId, &[RuleConfig])> = per_feed
        .iter()
        .map(|(id, rules)| (*id, rules.as_slice()))
        .collect();

    let dyn_pairs: Vec<(FeedId, &[DynamicRuleConfig])> = dyn_per_feed
        .iter()
        .map(|(id, rules)| (*id, rules.as_slice()))
        .collect();

    let engine = RuleEngine::build(
        &pairs,
        &dyn_pairs,
        &cfg.global_rules,
        &cfg.global_dynamics,
        cfg.defaults.action_on_no_match,
    )
    .context("building rule engine")?;

    Ok(engine)
}

/// Fetch cover art bytes from `episode.image_url`, falling back to `feed.image_url`.
async fn fetch_cover(
    episode: &Episode,
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
