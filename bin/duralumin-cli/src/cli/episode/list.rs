use crate::cli::helpers::{make_table, state_cell};
use anyhow::{Context, Result};
use comfy_table::Cell;
use duralumin_core::FeedId;
use duralumin_storage::{Db, EpisodeFilter};

pub async fn cmd_episode_list(
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
        FeedId(0)
    };

    let filter = EpisodeFilter {
        feed_id: feed_slug.map(|_| feed_id),
        state_kind: state_kind.map(str::to_owned),
        limit: Some(limit),
    };
    let episodes = db.list_episodes(filter).await?;

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
