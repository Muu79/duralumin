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
name   = "download-all"
priority = 0
action = "download"
[feeds.rules.match]
kind = "always"
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
name     = "skip-bonus"
priority = 0
action   = "skip"
[feeds.rules.match]
kind    = "title_regex"
pattern = '(?i)bonus'

# Priority 10 fires next (only reached if the episode wasn't already skipped).
[[feeds.rules]]
name     = "long-episodes"
priority = 10
action   = "download"
[feeds.rules.match]
kind  = "duration_min"
value = "20m"
```

**Example — opt-out model:**
```toml
[defaults]
action_on_no_match = "download"   # download everything by default

[[global_rules]]
name     = "skip-trailers"
priority = 0
action   = "skip"
[global_rules.match]
kind  = "duration_max"
value = "5m"
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
dura rules list                 # print all rules (global and per-feed)
dura rules check <slug>         # dry-run: show what action each episode would get
```

### Episode management

```bash
dura episode requeue <id>       # re-queue an episode for download
dura episode delete <id>        # remove from database
dura episode delete <id> --delete-file  # also delete the file from disk
dura check                      # list Complete episodes with missing files
dura check --fix                # requeue them for re-download
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

**With a reverse proxy** (nginx example):
```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
}
```

Set `base_url` to your public hostname. Sub-paths work too:
`base_url = "https://example.com/podcasts"`.

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
| `[logging]` | `level` | `"info"` | `error` / `warn` / `info` / `debug` / `trace` |
| `[logging]` | `format` | `"pretty"` | `"pretty"` or `"json"` |
| `[defaults]` | `action_on_no_match` | `"skip"` | `"download"` or `"skip"` |
| `[server]` | `bind` | — | `"127.0.0.1:3000"` — enables restreaming |
| `[server]` | `base_url` | — | Public URL for enclosure links |
| `[server]` | `auth_token` | — | Optional bearer token |
| `[[feeds]]` | `url` | — | RSS/Atom feed URL |
| `[[feeds]]` | `slug` | — | Unique identifier, used in paths and logs |
| `[[feeds]]` | `poll_interval` | `"1h"` | How often `dura start` re-checks this feed |
| `[[feeds]]` | `enabled` | `true` | Set `false` to pause without removing |
| `[[feeds]]` | `restream` | `false` | Expose via the restream server |
| `[[feeds]]` | `default_action` | — | Feed-level fallback after per-feed and global rules |

See [`examples/config.toml`](examples/config.toml) for annotated examples of
every option.

## License

MIT — see [LICENSE](LICENSE).
