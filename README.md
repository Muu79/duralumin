# dura

> Podcast download daemon — sync, archive, and restream your feeds.

`dura` is a self-hosted podcast manager that runs on a server or NAS. It fetches
RSS feeds on a schedule, applies configurable rules to decide what to download,
archives audio files to disk, and optionally restreams feeds over HTTP so any
podcast app can subscribe to your private archive.

## Features

- **Rule-based downloading** — download everything, only recent episodes, only
  episodes matching a title regex, by size, by duration, or any combination
- **Per-feed poll intervals** — each feed runs on its own schedule
- **RSS restreaming** — serve a rewritten RSS feed to any podcast app; audio
  plays from your server with range-request support for scrubbing
- **Concurrent downloads** with configurable back-off and retry
- **Audio tagging** — ID3/MP4 tags written automatically after download
- **Quarantine** — episodes that fail repeatedly are set aside for review rather
  than retried forever
- **Shell completions** for bash, zsh, fish, and powershell
- Runs as a systemd service or a one-shot cron job

## Installation

### Pre-built binaries

Download the archive for your platform from the
[releases page](https://github.com/muu79/duralumin/releases), extract it, and
place `dura` somewhere on your `$PATH`.

### From source

```bash
cargo install --git https://github.com/muu79/duralumin
```

Requires a stable Rust toolchain (1.80+). The build links against `ring`
(bundled) and SQLite (bundled) so no system libraries are needed.

## Quick start

```bash
# 1. Generate a default config and open it in your editor
dura config validate   # creates ~/.config/duralumin/config.toml if missing
$EDITOR ~/.config/duralumin/config.toml

# 2. Add a feed and do a first sync
#    (fetches the feed, evaluates rules, downloads matched episodes)
dura sync

# 3. Check what was found
dura status
dura feed info my-podcast

# 4. Run the daemon
dura start
```

## Configuration

Config lives at `~/.config/duralumin/config.toml` by default. Override with
`--config <path>` or `$DURALUMIN_CONFIG`. A fully-annotated example is at
[`examples/config.toml`](examples/config.toml).

The minimal config to get started:

```toml
[storage]
dir = "/home/you/podcasts"

[[feeds]]
url           = "https://feeds.example.com/my-show.rss"
slug          = "my-show"
poll_interval = "1h"

[[feeds.rules]]
name       = "download-all"
priority   = 0
action     = "download"
match.kind = "always"
```

### Rule engine

Every new episode is passed through the rule engine when a feed is synced. Rules
are evaluated in **ascending priority order** — lower numbers run first, and the
first rule that matches determines the action (`download`, `skip`, or `quarantine`).
Priority `0` has the highest precedence; use a high number (e.g. `100`) for a
catch-all that only fires when nothing more specific matched.
If no rule matches, `[defaults] action_on_no_match` applies (default: `skip`).

**Rule scope priority** (highest to lowest):

1. Per-feed rules (defined inside a `[[feeds]]` block)
2. Global rules (`[[global_rules]]`)
3. The feed's `default_action` (optional catch-all)
4. `[defaults] action_on_no_match`

> **TOML note:** Always use dotted keys for the `match` sub-table (`match.kind = ...`),
> not a `[feeds.rules.match]` header. TOML forbids redefining the same sub-table
> path across multiple array elements, so the header form breaks the moment a feed
> has more than one rule.

**Available match kinds:**

| Kind | Field | Example |
|------|-------|---------|
| `always` | — | match every episode |
| `title_regex` | `pattern` | `'^\d+:'` (use single-quoted TOML strings) |
| `description_regex` | `pattern` | `"(?i)interview"` |
| `duration_min` / `duration_max` | `value` | `"30m"`, `"2h"` |
| `episode_size_max` | `value` | `"500 MB"` |
| `published_after` / `published_before` | `date` | `"2024-01-01T00:00:00Z"` |

**Example — opt-in model (recommended):**
```toml
[defaults]
action_on_no_match = "skip"   # don't download unless a rule says so

# Priority 0 fires first: skip anything with "bonus" in the title.
[[feeds.rules]]
name          = "skip-bonus"
priority      = 0
action        = "skip"
match.kind    = "title_regex"
match.pattern = '(?i)bonus'

# Priority 10 fires next (only reached if the episode wasn't already skipped).
[[feeds.rules]]
name       = "long-episodes"
priority   = 10
action     = "download"
match.kind = "duration_min"
match.value = "20m"
```

**Example — opt-out model:**
```toml
[defaults]
action_on_no_match = "download"   # download everything by default

[[global_rules]]
name        = "skip-trailers"
priority    = 0
action      = "skip"
match.kind  = "duration_max"
match.value = "5m"
```

## Commands

### Everyday usage

```bash
dura status                     # overview of all feeds: counts by state
dura feed list                  # feeds and last-sync time
dura feed info <slug>           # detailed view: metadata + recent episodes
dura episode list               # recent episodes across all feeds
dura episode list --feed <slug> # episodes for one feed
dura episode list --state complete
```

Any command that accepts a `<slug>` also accepts any alias configured for that
feed. For example, if `aliases = ["mfp"]` is set on the `my-favourite-podcast`
feed, `dura feed info mfp` works identically to `dura feed info my-favourite-podcast`.

### Syncing and downloading

```bash
dura sync                       # sync all enabled feeds + drain download queue
dura sync <slug> [slug...]      # sync specific feeds only
dura sync --feeds-only          # refresh feed metadata, skip downloading
dura sync --recheck             # also re-evaluate all pending episodes
                                # against current rules (useful after rule changes)
dura download                   # drain the download queue without syncing feeds
dura download <id> [id...]      # download specific episodes by ID
```

### Rules

```bash
dura rules list                 # print all rules (global and per-feed), including dynamic rules
dura rules check <slug>         # dry-run: show what action each episode would get
```

### Dynamic windows

Dynamic rules keep a rolling set of episodes downloaded and automatically delete episodes that age out of the window. They run before static rules on every sync.

```toml
[[feeds.dynamic]]
name           = "keep-10-most-recent"
match.kind     = "last_n_episodes"
match.last_n_episodes = 10

[[feeds.dynamic]]
name           = "rolling-2-weeks"
match.kind     = "duration_ago"
match.duration = "14d"
```

**How it works:**

1. **New episodes** — if a dynamic rule matches, the episode is queued as `dynamic` and downloaded.  If a static rule also matches, it is downloaded permanently as `complete` instead.
2. **Each sync cycle** — the purge cycle re-evaluates every `dynamic` episode.  Episodes still in the window stay.  Episodes that have fallen outside the window have their file deleted and are marked `purged`.  Episodes that a static rule now claims are promoted to `complete` (file kept).

Dynamic rules are defined per-feed (`[[feeds.dynamic]]`) or globally (`[[global_dynamics]]`). Per-feed rules are evaluated before global ones.

### Episode management

```bash
dura episode requeue <id>       # re-queue an episode for download
dura episode delete <id>        # remove from database
dura episode delete <id> --delete-file  # also delete the file from disk
dura check                      # list Complete episodes with missing files
dura check --fix                # requeue them for re-download
```

### RSS restream

```bash
dura feed rebuild-rss                   # regenerate XML for all restream-enabled feeds
dura feed rebuild-rss <slug> [slug...]  # regenerate for specific feeds
```

### Dynamic episode purge

```bash
dura purge                      # run the purge cycle for all enabled feeds
dura purge <slug> [slug...]     # purge specific feeds only
```

### Quarantine

```bash
dura quarantine list            # episodes that failed too many times
dura quarantine retry <id>      # re-queue a quarantined episode
```

## Typical workflow after changing rules

```bash
# 1. Edit config, add/modify rules
$EDITOR ~/.config/duralumin/config.toml

# 2. Validate syntax
dura config validate

# 3. Dry-run: see what existing episodes would be assigned
dura rules check my-podcast

# 4. Apply to all pending episodes and trigger downloads
dura sync --recheck

# 5. Reload the daemon (if running)
systemctl reload dura   # or: systemctl restart dura
```

`--recheck` is safe: it only reassigns episodes in `Matched(Download/Skip)` or
`Discovered` states. `Complete` and `Quarantined` episodes are never touched.

## RSS Restreaming

`dura` can rewrite an RSS feed so that enclosure URLs point to your server
instead of the original CDN. Podcast apps then stream audio from your machine,
and locally-archived episodes are served directly from disk.

**Setup:**

```toml
[server]
bind      = "127.0.0.1:3000"
base_url  = "https://podcasts.example.com"  # public URL of your server
auth_token = "changeme"                      # optional bearer token

[[feeds]]
url      = "https://feeds.example.com/my-show.rss"
slug     = "my-show"
restream = true   # expose this feed through the server
```

The restreamed RSS feed is available at:

```
https://podcasts.example.com/rss/my-show
```

Add `?key=<token>` (or `Authorization: Bearer <token>`) to authenticate. The
token is embedded in enclosure URLs automatically so podcast apps can fetch
audio without separate auth headers.

**Static file model:** `dura` pre-generates a `{storage.dir}/rss/<slug>.xml`
file after each sync. The server serves this file directly with correct ETag
and `Last-Modified` headers so podcast apps receive proper `304 Not Modified`
responses and don't re-download the full feed on every poll.

**Episode filtering** (`restream_only_matched`, default `true`)

When `true` (default), the restreamed feed only includes episodes that are
downloaded (`Complete`, `Dynamic`) or queued. Skipped, discovered, and
quarantined episodes are excluded so the feed presented to podcast apps is
always clean.

Set `restream_only_matched = false` to expose the full feed regardless of
download state. Episodes without a local file are proxied transparently from
the original CDN, so any podcast app can subscribe and the full back-catalogue
is accessible even before `dura` has downloaded everything.

```toml
[[feeds]]
url                   = "https://feeds.example.com/my-show.rss"
slug                  = "my-show"
restream              = true
restream_only_matched = false   # stream everything; proxy origin for undownloaded episodes
```

**Cover art:** Feed artwork is downloaded and cached at
`{storage.dir}/images/<slug>/cover.<ext>` and served from your server so
podcast apps see a stable URL even if the upstream CDN changes.

**With a reverse proxy** (nginx example):
```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
}
```

Set `base_url` to your public hostname. Sub-paths work too:
`base_url = "https://example.com/podcasts"`.

Rebuild RSS files after changing `base_url`, `auth_token`, or feed rules:

```bash
dura feed rebuild-rss              # all restream feeds
dura feed rebuild-rss my-show      # one feed
```

## Running as a service

A systemd unit is provided at [`contrib/dura.service`](contrib/dura.service).

```bash
# Install
sudo cp contrib/dura.service /etc/systemd/system/
sudo useradd --system --shell /sbin/nologin dura
sudo mkdir -p /etc/dura /var/lib/dura
sudo cp examples/config.toml /etc/dura/config.toml
# edit /etc/dura/config.toml, set storage.dir = "/var/lib/dura"

# Enable and start
sudo systemctl daemon-reload
sudo systemctl enable --now dura

# Add a new feed and reload
sudo $EDITOR /etc/dura/config.toml
sudo systemctl reload dura   # sends SIGTERM; systemd restarts automatically
```

> **Note:** Hot-reload (config re-read without stopping) is not yet supported.
> `systemctl reload` stops the daemon; systemd restarts it immediately. Any
> in-progress downloads are resumed on the next start.

## Shell completions

```bash
# Zsh
dura completions zsh > ~/.zfunc/_dura
# Add to ~/.zshrc (before compinit):
# fpath=(~/.zfunc $fpath)
# autoload -U compinit && compinit

# Bash
dura completions bash >> ~/.bash_completion

# Fish
dura completions fish > ~/.config/fish/completions/dura.fish
```

For dynamic episode-ID completion in fish, add after generating the script:

```fish
complete -c dura -n '__fish_seen_subcommand_from episode; and __fish_seen_subcommand_from delete requeue' \
    -a '(dura episode list --completions)'
```

## Configuration reference

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `[storage]` | `dir` | — | **Required.** Base directory for DB and library. |
| `[storage]` | `library_path` | `{dir}/podcasts` | Root for downloaded audio. |
| `[storage]` | `state_db` | `{dir}/db/duralumin.db` | SQLite database path. |
| `[downloader]` | `concurrent_downloads` | `2` | Parallel download limit. |
| `[downloader]` | `max_retries` | `3` | Attempts before quarantine. |
| `[downloader]` | `attempt_timeout` | `"20m"` | Per-attempt HTTP timeout. |
| `[downloader]` | `max_bytes_per_sec` | `0` | Per-download bandwidth cap in bytes/sec; `0` = uncapped. |
| `[logging]` | `level` | `"info"` | `error` / `warn` / `info` / `debug` / `trace` |
| `[logging]` | `format` | `"pretty"` | `"pretty"` or `"json"` |
| `[defaults]` | `action_on_no_match` | `"skip"` | `"download"` or `"skip"` |
| `[server]` | `bind` | — | `"127.0.0.1:3000"` — enables restreaming |
| `[server]` | `base_url` | — | Public URL for enclosure links |
| `[server]` | `auth_token` | — | Optional bearer token |
| `[[feeds]]` | `url` | — | RSS/Atom feed URL |
| `[[feeds]]` | `slug` | — | Unique identifier, used in paths and logs |
| `[[feeds]]` | `display_name` | — | Human-readable label for CLI output (falls back to RSS title) |
| `[[feeds]]` | `aliases` | `[]` | Alternative identifiers accepted by slug CLI arguments |
| `[[feeds]]` | `poll_interval` | `"1h"` | How often `dura start` re-checks this feed |
| `[[feeds]]` | `enabled` | `true` | Set `false` to pause without removing |
| `[[feeds]]` | `restream` | `false` | Expose via the restream server |
| `[[feeds]]` | `restream_only_matched` | `true` | `true`: only Complete/Dynamic/queued episodes; `false`: full feed with origin proxy |
| `[[feeds]]` | `default_action` | — | Feed-level fallback after per-feed and global rules |
| `[[feeds.rules]]` | `name` | — | Rule label for logs |
| `[[feeds.rules]]` | `priority` | `0` | Evaluation order; lower = earlier |
| `[[feeds.rules]]` | `action` | — | `download` / `skip` / `quarantine` |
| `[[feeds.rules]]` | `match.kind` | — | `title_regex`, `duration_min/max`, `episode_size_max`, `published_before/after`, `always` |
| `[[feeds.dynamic]]` | `name` | — | Rule label for logs |
| `[[feeds.dynamic]]` | `match.kind` | — | `last_n_episodes` or `duration_ago` |
| `[[feeds.dynamic]]` | `match.last_n_episodes` | — | Keep the N most recent episodes (used with `last_n_episodes`) |
| `[[feeds.dynamic]]` | `match.duration` | — | Rolling window size, e.g. `"14d"` (used with `duration_ago`) |
| `[[global_dynamics]]` | — | — | Same fields as `[[feeds.dynamic]]`, applied across all feeds |

See [`examples/config.toml`](examples/config.toml) for annotated examples of
every option.

## License

MIT — see [LICENSE](LICENSE).
