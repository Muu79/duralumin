use anyhow::Result;
use clap::Args;
use comfy_table::{Cell, Color};
use duralumin_core::{Action, EpisodeState};
use duralumin_storage::{Db, EpisodeFilter};
use owo_colors::OwoColorize;

use super::helpers::make_table;

#[derive(Args)]
pub struct CheckArgs {
    /// Re-queue any missing episodes for download.
    #[arg(long)]
    pub fix: bool,
}

pub async fn cmd_check(db: &Db, fix: bool) -> Result<()> {
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
