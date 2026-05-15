-- Download queue: decoupled from episode state so the downloader just drains
-- a small table rather than scanning all episodes.
--
-- UNIQUE(episode_id) prevents double-enqueueing when sync runs before the
-- downloader drains. ON DELETE CASCADE removes queue entries automatically
-- if an episode is hard-deleted.
CREATE TABLE IF NOT EXISTS dl_queue (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id  TEXT    NOT NULL UNIQUE REFERENCES episodes(id) ON DELETE CASCADE,
    action      TEXT    NOT NULL,       -- 'download' or 'dynamic' (informational)
    added_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
