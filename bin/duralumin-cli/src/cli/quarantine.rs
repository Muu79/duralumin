use anyhow::Result;
use clap::{Args, Subcommand};
use comfy_table::{Cell, Color};
use duralumin_core::EpisodeState;
use duralumin_storage::{Db, EpisodeFilter};
use owo_colors::OwoColorize;

use super::helpers::make_table;

#[derive(Args)]
pub struct QuarantineArgs {
    #[command(subcommand)]
    pub sub: QuarantineSub,
}

#[derive(Subcommand)]
pub enum QuarantineSub {
    /// List all quarantined episodes.
    List,
    /// Re-queue a quarantined episode.
    Retry { id: String },
}

pub async fn cmd_quarantine_list(db: &Db) -> Result<()> {
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
