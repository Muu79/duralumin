use anyhow::{Context, Result};
use comfy_table::{Cell, Color};
use duralumin_storage::{Db, EpisodeFilter};

use crate::cli::helpers::{build_engine, make_table};
use crate::config;

pub async fn cmd_rules_check(cfg: &config::Config, db: &Db, slug: &str) -> Result<()> {
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
