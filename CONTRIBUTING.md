# Contributing to dura

Thanks for your interest. This document explains how the codebase is structured,
how to get a development build running, and where meaningful contributions could
be made.

## Architecture

`dura` is a Cargo workspace. Each crate has a single responsibility and depends
only on the crates below it in this diagram:

```
duralumin-cli  (bin/duralumin-cli — the `dura` binary)
    │
    ├── duralumin-server   (axum HTTP server, RSS rewriting)
    ├── duralumin-storage  (SQLite via sqlx, all DB access)
    ├── duralumin-downloader (HTTP download, retry, back-off)
    ├── duralumin-metadata  (audio tag writing via lofty)
    ├── duralumin-feed      (RSS/Atom parsing via feed-rs)
    ├── duralumin-rules     (rule engine + config types)
    ├── duralumin-transcode (audio transcoding — currently a stub)
    └── duralumin-core      (shared types: Episode, Feed, EpisodeState, IDs)
```

**`duralumin-core`** is the shared vocabulary. Everything else imports from it.
No crate below the CLI should depend on another crate at the same level or
above — this keeps the dependency graph a DAG and prevents circular deps.

### Key types

| Type | Crate | Purpose |
|------|-------|---------|
| `Episode` | core | An RSS enclosure item with all its metadata |
| `Feed` | core | A single RSS/Atom source |
| `EpisodeState` | core | State machine: Discovered → Matched → Complete/Failed/Quarantined |
| `Action` | core | `Download`, `Skip`, or `Quarantine` |
| `EpisodeId` | core | Stable, content-addressed ID derived from the feed URL + GUID |
| `FeedConfig` | rules | Deserialised `[[feeds]]` config block including rules |
| `RuleEngine` | rules | Evaluates one episode against the rule set, returns an `Action` |
| `Db` | storage | `Clone`-able handle to the SQLite pool; all queries live here |

### Episode lifecycle

```
RSS fetch → upsert_episode() → Discovered
                                    │
                               rule engine
                                    │
                        ┌───────────┴───────────┐
                   Matched(Download)        Matched(Skip)
                        │
                   download task
                        │
               ┌────────┴────────┐
            Complete          Failed{attempts}
                                    │ (> max_retries)
                               Quarantined
```

`upsert_episode` uses `INSERT OR IGNORE` so re-fetching a feed never overwrites
an existing state. Only mutable metadata (title, description, size, image) is
updated on subsequent syncs.

### Daemon task structure

`dura start` spawns three categories of async tasks inside a `JoinSet`:

1. **One feed-sync task per enabled feed** — runs on the feed's `poll_interval`
   using `tokio::time::interval` with `MissedTickBehavior::Delay`.
2. **One download drain task** — runs every 30 s, fetches `download_queue()`
   and runs all pending episodes concurrently behind a shared `Semaphore`.
3. **One HTTP server task** (optional) — started if any feed has `restream=true`
   and a `[server]` block is configured.

All tasks share a `Db` (cheap clone — it's an `Arc<Pool>`), an `Arc<RuleEngine>`,
and an `Arc<Semaphore>`. There is no shared mutable state.

### Server routing

```
GET /rss/{slug}              → rss_handler   (returns RSS XML)
GET /rss/{slug}/{episode_id} → audio_handler (serves file or proxies origin)
```

Auth is checked by `AuthGuard`, an `axum::extract::FromRequestParts` extractor
that validates `Authorization: Bearer <token>` or `?key=<token>`. Both are
accepted so that podcast apps (which can't set headers for audio fetches) can
use the token-in-URL form.

## Building and testing

```bash
# Build the binary
cargo build -p duralumin-cli

# Run all tests
cargo test --workspace

# Run the rule-engine tests (fastest feedback loop)
cargo test -p duralumin-rules

# Run the RSS parser tests
cargo test -p duralumin-feed

# Lint
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --all --check
```

The test suite uses real SQLite databases (in-memory) and parses real fixture
XML files from `tests/fixtures/`. There are no mocks; if you add a new storage
method, test it against the actual DB.

## Where contributions would be most useful

### New rule types

Add a new variant to `RuleKind` in `crates/rules/src/config.rs`, then implement
the match logic in `crates/rules/src/lib.rs` (the `evaluate` method). Include a
regex-compile check in `validate_rules` if the new kind has a pattern field.
Add a test in `crates/rules/tests/rule_engine.rs`.

Good candidates:
- `KeepLatest { count: usize }` — download only the N most recent episodes
- `EpisodeSizeMin` — skip episodes smaller than a threshold
- `TitleNotRegex` — inverse regex (skip if title matches)

### Hot-reload (SIGHUP)

Currently `dura start` reads config once and never revisits it. A `SIGHUP`
handler would re-parse the config, diff the feed list, cancel tasks for removed
feeds, and spawn tasks for new ones. The tricky part is draining in-flight
downloads gracefully. The `JoinSet` in `cmd_start` is the right place to wire
this up.

### Keep-latest / auto-purge

A `keep_latest: Option<usize>` field on `FeedConfig` that, after each sync,
sets `Matched(Skip)` on episodes ranked beyond position N (sorted by `pub_date`)
that are not yet `Complete`. Optionally paired with an `auto_delete: bool` flag
that also removes files for `Complete` episodes outside the window.

### Web UI / REST API

There is no HTTP API for management today — the `dura` binary is the only
interface. A REST or GraphQL layer could be added as a new handler group in
`crates/server` (or a separate crate) and serve a React/HTMX frontend. The
`Db` type is already `Clone` and `Send`, so it can be shared with new axum
state.

Useful endpoints to start with: `GET /api/feeds`, `GET /api/feeds/{slug}/episodes`,
`POST /api/feeds/{slug}/sync`, `DELETE /api/episodes/{id}`.

### Per-user / multi-user support

The current auth model is a single global token. A per-user model would need:
- A `users` table in storage with hashed tokens
- Per-user feed subscriptions or visibility rules
- The RSS handler returning a filtered feed based on which user authenticated

This would make `dura` useful as a household or small-team server.

### Image downloading and restreaming

Feed and episode images are stored as URLs in the DB (`image_url` field) but
never downloaded. To serve them locally:
1. Download images after each feed sync into a per-feed image cache directory
2. Add routes `/rss/{slug}/image` and `/rss/{slug}/image/{episode_id}`
3. Update `rss.rs` to rewrite `<itunes:image>` and `<image><url>` to point
   at the local routes

### `.deb` packaging (v0.1.1)

`cargo-deb` can generate a Debian package from metadata in `Cargo.toml`. The
`[package.metadata.deb]` section in `bin/duralumin-cli/Cargo.toml` needs to be
filled in, and a build step added to the GitHub Actions release workflow.

### Transcoding

`crates/transcode` is a stub crate. The intended use is post-download
transcoding (e.g., lossy → opus for space savings). It would integrate as an
optional step in `run_downloads`, after the file is written and tagged.

## Pull request guidelines

- One logical change per PR. A bug fix doesn't need surrounding cleanup.
- New rule types need a test in `rule_engine.rs`.
- New storage methods need a test against a real SQLite DB.
- Run `cargo fmt --all` before pushing.
- The CI workflow (`ci.yml`) runs tests, clippy, and fmt-check on every PR.
  All three must pass.
- If you're unsure whether a feature fits the project scope, open an issue first.
