use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteRow},
};
use url::Url;

use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, Feed, FeedId};

// ---- Error type ----------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("serialisation error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T> = std::result::Result<T, StorageError>;

// ---- Public filter / supporting types ------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EpisodeFilter {
    pub feed_id: Option<FeedId>,
    /// Match against `EpisodeState::kind_name()` — e.g. "complete", "failed".
    pub state_kind: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DownloadAttempt {
    /// `0` = not yet persisted.
    pub id: i64,
    pub episode_id: EpisodeId,
    pub attempt_number: u8,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub bytes_received: Option<u64>,
    pub http_status: Option<u16>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
}

// ---- Db ------------------------------------------------------------------------

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        tracing::info!(path = %path.display(), "opening database");
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(opts).await?;
        sqlx::migrate!().run(&pool).await?;
        tracing::info!(path = %path.display(), "database ready (migrations applied)");
        Ok(Self { pool })
    }

    /// Open an in-memory database (for tests).
    ///
    /// Uses a single-connection pool so all operations share the same database
    /// instance (multiple `:memory:` connections each get their own blank DB).
    pub async fn open_in_memory() -> Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ---- Feed operations -------------------------------------------------------

    pub async fn upsert_feed(&self, feed: &Feed) -> Result<FeedId> {
        // ON CONFLICT(url) DO UPDATE preserves created_at; RETURNING id works
        // for both the insert path and the update path (SQLite ≥ 3.35).
        let row = sqlx::query(
            "INSERT INTO feeds (url, slug, title, enabled, last_fetched_at, etag, last_modified, image_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(url) DO UPDATE SET
               slug            = excluded.slug,
               title           = excluded.title,
               enabled         = excluded.enabled,
               last_fetched_at = excluded.last_fetched_at,
               etag            = excluded.etag,
               last_modified   = excluded.last_modified,
               image_url       = excluded.image_url
             RETURNING id",
        )
        .bind(feed.url.as_str())
        .bind(&feed.slug)
        .bind(&feed.title)
        .bind(feed.enabled as i64)
        .bind(feed.last_fetched_at)
        .bind(&feed.etag)
        .bind(&feed.last_modified)
        .bind(feed.image_url.as_ref().map(Url::as_str))
        .fetch_one(&self.pool)
        .await?;
        Ok(FeedId(row.try_get("id")?))
    }

    pub async fn get_feed(&self, id: FeedId) -> Result<Option<Feed>> {
        let row = sqlx::query("SELECT * FROM feeds WHERE id = ?")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| row_to_feed(&r)).transpose()
    }

    pub async fn get_feed_by_slug(&self, slug: &str) -> Result<Option<Feed>> {
        let row = sqlx::query("SELECT * FROM feeds WHERE slug = ?")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| row_to_feed(&r)).transpose()
    }

    pub async fn list_feeds(&self) -> Result<Vec<Feed>> {
        let rows = sqlx::query("SELECT * FROM feeds ORDER BY slug ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_feed).collect()
    }

    pub async fn delete_feed(&self, id: FeedId) -> Result<()> {
        sqlx::query("DELETE FROM feeds WHERE id = ?")
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Episode operations ----------------------------------------------------

    /// Insert a newly-discovered episode, returning `true` if the row was
    /// created for the first time.  On conflict (same id), only mutable
    /// metadata fields are updated — `state` is intentionally left alone so
    /// we never regress a Complete/Failed episode back to Discovered.
    pub async fn upsert_episode(&self, ep: &Episode) -> Result<bool> {
        let state_json = serde_json::to_string(&ep.state)?;

        // INSERT OR IGNORE: skip if already present; rows_affected tells us
        // whether this was a genuine new episode.
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO episodes
               (id, feed_id, guid, title, description, pub_date, duration_secs,
                enclosure_url, enclosure_size, enclosure_mime, state, image_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(ep.id.as_str())
        .bind(ep.feed_id.0)
        .bind(&ep.guid)
        .bind(&ep.title)
        .bind(&ep.description)
        .bind(ep.pub_date)
        .bind(ep.duration_secs.map(|s| s as i64))
        .bind(ep.enclosure_url.as_str())
        .bind(ep.enclosure_size.map(|s| s as i64))
        .bind(&ep.enclosure_mime)
        .bind(&state_json)
        .bind(ep.image_url.as_ref().map(Url::as_str))
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;

        if !inserted {
            // Update only mutable metadata for known episodes.
            sqlx::query(
                "UPDATE episodes SET
                   title          = ?,
                   description    = ?,
                   enclosure_size = ?,
                   enclosure_mime = ?,
                   image_url      = ?
                 WHERE id = ?",
            )
            .bind(&ep.title)
            .bind(&ep.description)
            .bind(ep.enclosure_size.map(|s| s as i64))
            .bind(&ep.enclosure_mime)
            .bind(ep.image_url.as_ref().map(Url::as_str))
            .bind(ep.id.as_str())
            .execute(&self.pool)
            .await?;
        }

        Ok(inserted)
    }

    pub async fn delete_episode(&self, id: &EpisodeId) -> Result<bool> {
        let rows = sqlx::query("DELETE FROM episodes WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(rows > 0)
    }

    pub async fn get_episode(&self, id: &EpisodeId) -> Result<Option<Episode>> {
        let row = sqlx::query("SELECT * FROM episodes WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| row_to_episode(&r)).transpose()
    }

    pub async fn update_episode_state(&self, id: &EpisodeId, state: &EpisodeState) -> Result<()> {
        let state_json = serde_json::to_string(state)?;
        let (file_path, sha256) = match state {
            EpisodeState::Complete { path, sha256, .. } => (
                Some(path.to_string_lossy().into_owned()),
                Some(sha256.clone()),
            ),
            EpisodeState::Dynamic { path, sha256, .. } => (
                Some(path.to_string_lossy().into_owned()),
                Some(sha256.clone()),
            ),
            _ => (None, None),
        };
        sqlx::query("UPDATE episodes SET state = ?, file_path = ?, sha256 = ? WHERE id = ?")
            .bind(&state_json)
            .bind(&file_path)
            .bind(&sha256)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_episodes(&self, filter: EpisodeFilter) -> Result<Vec<Episode>> {
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM episodes WHERE 1=1");
        if let Some(fid) = filter.feed_id {
            qb.push(" AND feed_id = ").push_bind(fid.0);
        }
        qb.push(" ORDER BY pub_date DESC");

        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut episodes: Vec<Episode> = rows.iter().map(row_to_episode).collect::<Result<_>>()?;

        if let Some(ref kind) = filter.state_kind {
            episodes.retain(|ep| ep.state.kind_name() == kind);
        }
        if let Some(limit) = filter.limit {
            episodes.truncate(limit);
        }
        Ok(episodes)
    }

    // ---- Download queue --------------------------------------------------------

    /// Add an episode to the download queue. Returns `true` if the episode was
    /// newly enqueued; `false` if it was already present (idempotent).
    pub async fn enqueue(&self, episode_id: &EpisodeId, action: Action) -> Result<bool> {
        let rows = sqlx::query("INSERT OR IGNORE INTO dl_queue (episode_id, action) VALUES (?, ?)")
            .bind(episode_id.as_str())
            .bind(action.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(rows > 0)
    }

    /// Remove an episode from the download queue. No-op if not present.
    pub async fn dequeue(&self, episode_id: &EpisodeId) -> Result<()> {
        sqlx::query("DELETE FROM dl_queue WHERE episode_id = ?")
            .bind(episode_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return all queued episodes joined with their episode rows, ordered by
    /// enqueue time (oldest first) so the queue drains in arrival order.
    pub async fn get_queue(&self) -> Result<Vec<Episode>> {
        let rows = sqlx::query(
            "SELECT e.* FROM episodes e
             JOIN dl_queue q ON q.episode_id = e.id
             ORDER BY q.added_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_episode).collect()
    }

    /// Return all `Dynamic`-state episodes for a feed (used by the purge cycle).
    pub async fn list_dynamic_episodes(&self, feed_id: FeedId) -> Result<Vec<Episode>> {
        let rows = sqlx::query("SELECT * FROM episodes WHERE feed_id = ? ORDER BY pub_date DESC")
            .bind(feed_id.0)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(row_to_episode)
            .collect::<Result<Vec<_>>>()
            .map(|eps| {
                eps.into_iter()
                    .filter(|ep| matches!(ep.state, EpisodeState::Dynamic { .. }))
                    .collect()
            })
    }

    /// Count episodes currently in the Quarantined state (used at startup).
    pub async fn count_quarantined(&self) -> Result<usize> {
        let row = sqlx::query(
            "SELECT COUNT(*) as n FROM episodes
             WHERE json_extract(state, '$.Quarantined') IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        let n: i64 = row.try_get("n")?;
        Ok(n as usize)
    }

    // ---- Attempt operations ----------------------------------------------------

    pub async fn record_attempt(&self, attempt: &DownloadAttempt) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO download_attempts
               (episode_id, attempt_number, started_at, ended_at,
                bytes_received, http_status, error_kind, error_detail)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(attempt.episode_id.as_str())
        .bind(attempt.attempt_number as i64)
        .bind(attempt.started_at)
        .bind(attempt.ended_at)
        .bind(attempt.bytes_received.map(|b| b as i64))
        .bind(attempt.http_status.map(|s| s as i64))
        .bind(&attempt.error_kind)
        .bind(&attempt.error_detail)
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    pub async fn attempts_for(&self, episode_id: &EpisodeId) -> Result<Vec<DownloadAttempt>> {
        let rows = sqlx::query(
            "SELECT * FROM download_attempts
             WHERE episode_id = ?
             ORDER BY attempt_number ASC",
        )
        .bind(episode_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_attempt).collect()
    }
}

// ---- Row → domain type helpers -------------------------------------------------

fn row_to_feed(row: &SqliteRow) -> Result<Feed> {
    let id: i64 = row.try_get("id")?;
    let url: String = row.try_get("url")?;
    let slug: String = row.try_get("slug")?;
    let title: Option<String> = row.try_get("title")?;
    let enabled: i64 = row.try_get("enabled")?;
    let last_fetched_at: Option<DateTime<Utc>> = row.try_get("last_fetched_at")?;
    let etag: Option<String> = row.try_get("etag")?;
    let last_modified: Option<String> = row.try_get("last_modified")?;
    let image_url_str: Option<String> = row.try_get("image_url")?;
    Ok(Feed {
        id: FeedId(id),
        url: Url::parse(&url)?,
        slug,
        title,
        last_fetched_at,
        etag,
        last_modified,
        enabled: enabled != 0,
        image_url: image_url_str.as_deref().and_then(|s| Url::parse(s).ok()),
    })
}

fn row_to_episode(row: &SqliteRow) -> Result<Episode> {
    let id: String = row.try_get("id")?;
    let feed_id: i64 = row.try_get("feed_id")?;
    let guid: String = row.try_get("guid")?;
    let title: String = row.try_get("title")?;
    let description: Option<String> = row.try_get("description")?;
    let pub_date: DateTime<Utc> = row.try_get("pub_date")?;
    let duration_secs: Option<i64> = row.try_get("duration_secs")?;
    let enclosure_url: String = row.try_get("enclosure_url")?;
    let enclosure_size: Option<i64> = row.try_get("enclosure_size")?;
    let enclosure_mime: Option<String> = row.try_get("enclosure_mime")?;
    let state_json: String = row.try_get("state")?;
    let image_url_str: Option<String> = row.try_get("image_url")?;
    Ok(Episode {
        id: EpisodeId::from(id),
        feed_id: FeedId(feed_id),
        guid,
        title,
        description,
        pub_date,
        duration_secs: duration_secs.map(|s| s as u64),
        enclosure_url: Url::parse(&enclosure_url)?,
        enclosure_size: enclosure_size.map(|s| s as u64),
        enclosure_mime,
        state: serde_json::from_str(&state_json)?,
        // Swallow URL parse errors for image_url — a bad art URL shouldn't
        // surface as a storage failure.
        image_url: image_url_str.as_deref().and_then(|s| Url::parse(s).ok()),
        episode_no: 0, // episode_no is not persisted; it's only used transiently during feed parsing
    })
}

fn row_to_attempt(row: &SqliteRow) -> Result<DownloadAttempt> {
    let id: i64 = row.try_get("id")?;
    let episode_id: String = row.try_get("episode_id")?;
    let attempt_number: i64 = row.try_get("attempt_number")?;
    let started_at: DateTime<Utc> = row.try_get("started_at")?;
    let ended_at: Option<DateTime<Utc>> = row.try_get("ended_at")?;
    let bytes_received: Option<i64> = row.try_get("bytes_received")?;
    let http_status: Option<i64> = row.try_get("http_status")?;
    let error_kind: Option<String> = row.try_get("error_kind")?;
    let error_detail: Option<String> = row.try_get("error_detail")?;
    Ok(DownloadAttempt {
        id,
        episode_id: EpisodeId::from(episode_id),
        attempt_number: attempt_number as u8,
        started_at,
        ended_at,
        bytes_received: bytes_received.map(|b| b as u64),
        http_status: http_status.map(|s| s as u16),
        error_kind,
        error_detail,
    })
}
