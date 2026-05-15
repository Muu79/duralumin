pub mod check;
pub mod list;

pub use check::cmd_rules_check;
pub use list::cmd_rules_list;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct RulesArgs {
    #[command(subcommand)]
    pub sub: RulesSub,
}

#[derive(Subcommand)]
pub enum RulesSub {
    /// Dry-run rule evaluation for all episodes in a feed.
    Check { slug: String },
    /// Print all configured rules (global and per-feed).
    List,
}
