pub mod info;
pub mod list;
pub mod rebuild_rss;
pub mod reimport;

pub use info::cmd_feed_info;
pub use list::cmd_feed_list;
pub use rebuild_rss::cmd_rebuild_rss;
pub use reimport::cmd_feed_reimport;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct FeedArgs {
    #[command(subcommand)]
    pub sub: FeedSub,
}

#[derive(Subcommand)]
pub enum FeedSub {
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
