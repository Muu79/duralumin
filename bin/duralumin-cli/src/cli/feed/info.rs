use anyhow::{Context, Result};
use comfy_table::{Cell, Color};
use duralumin_storage::{Db, EpisodeFilter};
use owo_colors::OwoColorize;

use crate::cli::helpers::{EpisodeCounts, make_table, state_cell};
use crate::config;
use crate::tui;

pub async fn cmd_feed_info(
    cfg: &config::Config,
    db: &Db,
    slug: &str,
    interactive: bool,
) -> Result<()> {
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
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "never".into());

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
            let date = ep
                .pub_date
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string();
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
