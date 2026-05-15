use anyhow::{Context, Result};
use duralumin_core::{EpisodeId, EpisodeState};
use duralumin_storage::Db;
use owo_colors::OwoColorize;

pub async fn cmd_episode_delete(db: &Db, id: &str, delete_file: bool) -> Result<()> {
    let eid = EpisodeId::from(id.to_string());
    let ep = db
        .get_episode(&eid)
        .await?
        .with_context(|| format!("episode {id:?} not found"))?;

    if delete_file && let EpisodeState::Complete { path, .. } = &ep.state {
        if path.exists() {
            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("failed to delete {:?}", path))?;
            println!("  {} {}", "Deleted file:".dimmed(), path.display());
        } else {
            println!("  {}", "File already missing from disk.".dimmed());
        }
    }

    db.delete_episode(&ep.id).await?;
    println!("{} {} — {}", "Removed".red(), ep.id.short(), ep.title);
    Ok(())
}
