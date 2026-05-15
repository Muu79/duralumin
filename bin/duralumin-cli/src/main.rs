mod cli;
mod config;
mod feed_sync;
mod rss_gen;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use duralumin_storage::Db;
use rustls::crypto::ring;
use tracing_subscriber::{EnvFilter, fmt};

use cli::check::CheckArgs;
use cli::download::DownloadArgs;
use cli::episode::{EpisodeArgs, EpisodeSub};
use cli::feed::{FeedArgs, FeedSub};
use cli::helpers::resolve_slug;
use cli::quarantine::{QuarantineArgs, QuarantineSub};
use cli::rules::{RulesArgs, RulesSub};
use cli::sync::SyncArgs;

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

    // Shell completions need no config or DB.
    if let Command::Completions { shell } = &cli.command {
        use clap::CommandFactory;
        use clap_complete::generate;
        generate(*shell, &mut Cli::command(), "dura", &mut std::io::stdout());
        std::process::exit(0);
    }

    // Config validate is handled before opening the DB.
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
    // to warn so DB/migration chatter stays quiet.
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

    let config_format = match cfg.logging.format.as_str() {
        "json" => Some(LogFormat::Json),
        _ => Some(LogFormat::Pretty),
    };
    let log_format = cli.log_format.as_ref().or(config_format.as_ref());

    let filter = EnvFilter::try_new(&log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    match log_format {
        Some(LogFormat::Json) => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init(),
        _ => fmt().with_env_filter(filter).init(),
    }

    tracing::info!(path = %config_path.display(), "loaded config");

    let db_path = cfg.storage.db();
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
        tracing::info!(path = %db_dir.display(), "created database directory");
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
        Ok(n) => tracing::info!(
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

// ---- Command dispatch ------------------------------------------------------

async fn run(command: Command, cfg: &config::Config, db: &Db, interactive: bool) -> Result<()> {
    match command {
        Command::Feed(FeedArgs { sub }) => match sub {
            FeedSub::List => cli::feed::cmd_feed_list(cfg, db, interactive).await,
            FeedSub::Info { slug } => {
                cli::feed::cmd_feed_info(cfg, db, &resolve_slug(cfg, &slug), interactive).await
            }
            FeedSub::Reimport { slug } => {
                let resolved = slug.as_deref().map(|s| resolve_slug(cfg, s));
                cli::feed::cmd_feed_reimport(cfg, db, resolved.as_deref()).await
            }
            FeedSub::RebuildRss { slugs } => {
                let resolved = slugs.iter().map(|s| resolve_slug(cfg, s)).collect();
                cli::feed::cmd_rebuild_rss(cfg, db, resolved).await
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
                cli::episode::cmd_episode_list(
                    db,
                    resolved_feed.as_deref(),
                    state.as_deref(),
                    limit,
                    completions,
                )
                .await
            }
            EpisodeSub::Requeue { id } => cli::episode::cmd_requeue(db, &id).await,
            EpisodeSub::Delete { id, delete_file } => {
                cli::episode::cmd_episode_delete(db, &id, delete_file).await
            }
        },
        Command::Start => cli::cmd_start(cfg, db).await,
        Command::Sync(args) => cli::cmd_sync(cfg, db, args).await,
        Command::Download(DownloadArgs { ids }) => cli::cmd_download(cfg, db, ids).await,
        Command::Quarantine(QuarantineArgs { sub }) => match sub {
            QuarantineSub::List => cli::cmd_quarantine_list(db).await,
            QuarantineSub::Retry { id } => cli::episode::cmd_requeue(db, &id).await,
        },
        Command::Rules(RulesArgs { sub }) => match sub {
            RulesSub::Check { slug } => {
                cli::rules::cmd_rules_check(cfg, db, &resolve_slug(cfg, &slug)).await
            }
            RulesSub::List => cli::rules::cmd_rules_list(cfg),
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
        Command::Status => cli::cmd_status(cfg, db).await,
        Command::Check(CheckArgs { fix }) => cli::cmd_check(db, fix).await,
        Command::Purge { slugs } => {
            let resolved = slugs.iter().map(|s| resolve_slug(cfg, s)).collect();
            cli::cmd_purge(cfg, db, resolved).await
        }
        Command::Completions { .. } => unreachable!("handled before DB open"),
    }
}
