use anyhow::{Context, Result};
use duralumin_core::{Action, EpisodeId, EpisodeState};
use duralumin_storage::Db;
use owo_colors::OwoColorize;

pub async fn cmd_requeue(db: &Db, id: &str) -> Result<()> {
    let eid = EpisodeId::from(id.to_string());
    let ep = db
        .get_episode(&eid)
        .await?
        .with_context(|| format!("episode {id:?} not found"))?;
    db.update_episode_state(&ep.id, &EpisodeState::Matched(Action::Download))
        .await?;
    db.enqueue(&ep.id, Action::Download).await?;
    println!("{} episode {}", "Re-queued".green(), ep.id.short());
    Ok(())
}
