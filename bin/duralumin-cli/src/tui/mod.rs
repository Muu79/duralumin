pub mod episode_browser;
pub mod feed_browser;

use std::io::Stdout;

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};

use duralumin_rules::config::FeedConfig;
use duralumin_server::ServerConfig;
use duralumin_storage::Db;

use crate::config;

// ---- Public action returned from the session --------------------------------

/// Returned by [`run`] to tell the caller what to do next.
pub enum Action {
    Quit,
    /// User pressed `r` on a feed — caller should sync that slug then re-enter.
    Sync(String),
}

// ---- Entry point ------------------------------------------------------------

/// Run the interactive TUI session.
///
/// If `initial_slug` is given, opens the episode browser directly for that
/// feed; otherwise starts at the feed browser.
///
/// Returns `Action::Quit` when the user exits normally, or `Action::Sync(slug)`
/// when the user requests a sync (so the caller can run it then re-call `run`).
pub async fn run(cfg: &config::Config, db: &Db, initial_slug: Option<String>) -> Result<Action> {
    let db = db.clone();
    let cfg_feeds: Vec<FeedConfig> = cfg.feeds.clone();
    let server_cfg: Option<ServerConfig> = cfg.server.clone();
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || -> Result<Action> {
        let _guard = TerminalGuard::enter()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

        match initial_slug {
            Some(slug) => {
                episode_browser::run(
                    &handle,
                    &db,
                    &cfg_feeds,
                    server_cfg.as_ref(),
                    &mut terminal,
                    &slug,
                )?;
                Ok(Action::Quit)
            }
            None => feed_browser::run(&handle, &db, &cfg_feeds, server_cfg.as_ref(), &mut terminal),
        }
    })
    .await?
}

// ---- Terminal RAII guard ----------------------------------------------------

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}

pub type Term = Terminal<CrosstermBackend<Stdout>>;

// ---- Shared helpers ---------------------------------------------------------

/// Format a UTC timestamp as "Xh ago", "Xm ago", etc.
pub fn time_ago(dt: Option<DateTime<Utc>>) -> String {
    let Some(dt) = dt else {
        return "never".to_string();
    };
    let secs = (Utc::now() - dt).num_seconds().max(0) as u64;
    if secs < 120 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Format bytes as "82 MB", "1.2 GB", etc.
pub fn format_bytes(bytes: Option<u64>) -> String {
    match bytes {
        None => "—".to_string(),
        Some(b) if b < 1_000_000 => format!("{} KB", b / 1_000),
        Some(b) if b < 1_000_000_000 => format!("{} MB", b / 1_000_000),
        Some(b) => format!("{:.1} GB", b as f64 / 1_000_000_000.0),
    }
}

/// Format a duration in seconds as "1h 23m" or "45m".
pub fn format_dur(secs: Option<u64>) -> String {
    let Some(s) = secs else {
        return "—".to_string();
    };
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else {
        format!("{}m", m)
    }
}

/// Truncate a string to `max` display characters, appending `…` if needed.
pub fn trunc(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let cut: String = chars[..max.saturating_sub(1)].iter().collect();
        format!("{cut}…")
    }
}

/// Build the restreamed feed URL for a feed, including auth token if configured.
pub fn feed_restream_url(feed_cfg: &FeedConfig, server_cfg: &ServerConfig) -> Option<String> {
    if !feed_cfg.restream {
        return None;
    }
    let base = server_cfg.base_url.as_str().trim_end_matches('/');
    let key = server_cfg
        .auth_token
        .as_deref()
        .map(|t| format!("?key={t}"))
        .unwrap_or_default();
    Some(format!("{base}/rss/{}{key}", feed_cfg.slug))
}

/// Return a centered `Rect` of fixed absolute size within `outer`.
pub fn centered_rect(width: u16, height: u16, outer: Rect) -> Rect {
    let x = outer.x + outer.width.saturating_sub(width) / 2;
    let y = outer.y + outer.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(outer.width),
        height: height.min(outer.height),
    }
}

/// Copy text to the system clipboard.
///
/// Cascade:
///   1. arboard  — works in local GUI environments
///   2. OSC 52   — terminal escape sequence; works in modern terminals over SSH
///                 (iTerm2, WezTerm, kitty, foot, xterm, …) — no confirmation
///                 is possible so we assume success if the write succeeds
///   3. stderr   — last resort when both above fail
pub fn copy_to_clipboard(text: &str) -> String {
    if arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .is_ok()
    {
        return "Copied to clipboard".to_string();
    }

    if try_osc52(text) {
        return "Copied via terminal (OSC 52 — requires compatible terminal)".to_string();
    }

    eprintln!("[dura] URL: {text}");
    "No clipboard available — URL printed to stderr".to_string()
}

/// Write an OSC 52 clipboard escape sequence to the terminal.
/// Writes to /dev/tty on Unix so the sequence bypasses any stdout buffering
/// from the TUI.  Returns false if the write itself fails.
fn try_osc52(text: &str) -> bool {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", b64(text.as_bytes()));

    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        return tty.write_all(seq.as_bytes()).is_ok();
    }

    // Non-Unix or /dev/tty unavailable: write directly to stdout.
    std::io::stdout()
        .write_all(seq.as_bytes())
        .and_then(|_| std::io::stdout().flush())
        .is_ok()
}

/// Minimal standard base64 encoder (RFC 4648) — avoids adding a crate dep.
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0xF) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}
