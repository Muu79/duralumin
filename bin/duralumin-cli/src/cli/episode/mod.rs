pub mod delete;
pub mod list;
pub mod requeue;

pub use delete::cmd_episode_delete;
pub use list::cmd_episode_list;
pub use requeue::cmd_requeue;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct EpisodeArgs {
    #[command(subcommand)]
    pub sub: EpisodeSub,
}

#[derive(Subcommand)]
pub enum EpisodeSub {
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
