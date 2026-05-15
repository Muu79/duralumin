use anyhow::Result;
use comfy_table::{Cell, Color};
use duralumin_storage::{Db, EpisodeFilter};
use owo_colors::OwoColorize;

use super::helpers::{EpisodeCounts, make_table};
use crate::config;

pub async fn cmd_status(cfg: &config::Config, db: &Db) -> Result<()> {
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
