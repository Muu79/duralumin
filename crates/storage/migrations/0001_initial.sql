CREATE TABLE IF NOT EXISTS feeds (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    url             TEXT NOT NULL UNIQUE,
    slug            TEXT NOT NULL UNIQUE,
    title           TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    last_fetched_at TEXT,
    etag            TEXT,
    last_modified   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS episodes (
    id              TEXT PRIMARY KEY,           -- sha256 hex
    feed_id         INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    guid            TEXT NOT NULL,
    title           TEXT NOT NULL,
    description     TEXT,
    pub_date        TEXT NOT NULL,
    duration_secs   INTEGER,
    enclosure_url   TEXT NOT NULL,
    enclosure_size  INTEGER,
    enclosure_mime  TEXT,
    state           TEXT NOT NULL,              -- JSON-encoded EpisodeState
    image_url       TEXT,
    file_path       TEXT,                       -- set when Complete
    sha256          TEXT,                       -- set when Complete
    discovered_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(feed_id, guid)
);

CREATE INDEX IF NOT EXISTS idx_episodes_feed_state ON episodes(feed_id, state);
CREATE INDEX IF NOT EXISTS idx_episodes_pub_date   ON episodes(pub_date DESC);

CREATE TABLE IF NOT EXISTS download_attempts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id      TEXT NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    attempt_number  INTEGER NOT NULL,
    started_at      TEXT NOT NULL,
    ended_at        TEXT,
    bytes_received  INTEGER,
    http_status     INTEGER,
    error_kind      TEXT,                       -- 'timeout','http','io','parse','size'
    error_detail    TEXT
);

CREATE INDEX IF NOT EXISTS idx_attempts_episode ON download_attempts(episode_id);
