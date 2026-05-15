use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, poll, read};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};

use duralumin_core::Feed;
use duralumin_rules::config::FeedConfig;
use duralumin_server::ServerConfig;
use duralumin_storage::{Db, EpisodeFilter};

use super::{Action, Term, copy_to_clipboard, feed_restream_url, time_ago, trunc};

// ---- State ------------------------------------------------------------------

struct State {
    feeds: Vec<Feed>,
    /// Slug → config (for display_name, restream, etc.)
    cfg: HashMap<String, FeedConfig>,
    selected: usize,
    list_state: ListState,
    server_cfg: Option<ServerConfig>,
    status: String,
}

impl State {
    fn new(feeds: Vec<Feed>, cfg_feeds: &[FeedConfig], server_cfg: Option<ServerConfig>) -> Self {
        let cfg: HashMap<String, FeedConfig> = cfg_feeds
            .iter()
            .map(|f| (f.slug.clone(), f.clone()))
            .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            feeds,
            cfg,
            selected: 0,
            list_state,
            server_cfg,
            status: String::new(),
        }
    }

    fn select(&mut self, idx: usize) {
        self.selected = idx.min(self.feeds.len().saturating_sub(1));
        self.list_state.select(Some(self.selected));
    }

    fn selected_feed(&self) -> Option<&Feed> {
        self.feeds.get(self.selected)
    }

    fn display_name(&self, feed: &Feed) -> String {
        self.cfg
            .get(&feed.slug)
            .and_then(|c| c.display_name.as_deref())
            .or(feed.title.as_deref())
            .unwrap_or(&feed.slug)
            .to_string()
    }
}

// ---- Entry point ------------------------------------------------------------

pub fn run(
    handle: &tokio::runtime::Handle,
    db: &Db,
    cfg_feeds: &[FeedConfig],
    server_cfg: Option<&ServerConfig>,
    terminal: &mut Term,
) -> Result<Action> {
    let feeds = handle.block_on(db.list_feeds())?;
    let mut state = State::new(feeds, cfg_feeds, server_cfg.cloned());

    loop {
        terminal.draw(|f| render(f, &mut state))?;

        if !poll(Duration::from_millis(100))? {
            continue;
        }

        match read()? {
            Event::Key(key) => {
                match handle_key(handle, db, &mut state, key)? {
                    KeyAction::Continue => {}
                    KeyAction::Quit => return Ok(Action::Quit),
                    KeyAction::OpenEpisodes(slug) => {
                        super::episode_browser::run(
                            handle, db, cfg_feeds, server_cfg, terminal, &slug,
                        )?;
                        // Reload feeds in case states changed while in episode browser.
                        state.feeds = handle.block_on(db.list_feeds())?;
                        terminal.clear()?;
                    }
                    KeyAction::Sync(slug) => return Ok(Action::Sync(slug)),
                }
            }
            Event::Resize(_, _) => terminal.autoresize()?,
            _ => {}
        }
    }
}

// ---- Rendering --------------------------------------------------------------

fn render(f: &mut ratatui::Frame, state: &mut State) {
    let area = f.area();
    let [list_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    // Build list items.
    let items: Vec<ListItem> = state
        .feeds
        .iter()
        .map(|feed| {
            let name = state.display_name(feed);
            let last = time_ago(feed.last_fetched_at);
            let restream = state
                .cfg
                .get(&feed.slug)
                .map(|c| c.restream)
                .unwrap_or(false);
            let marker = if restream { "◉ " } else { "  " };
            let w = (list_area.width as usize).saturating_sub(marker.len() + last.len() + 4);
            let name_col = format!("{:<width$}", trunc(&name, w), width = w);
            Line::from(vec![
                Span::styled(marker, Style::new().fg(Color::Cyan)),
                Span::raw(name_col),
                Span::styled(last, Style::new().fg(Color::DarkGray)),
            ])
        })
        .map(ListItem::new)
        .collect();

    let title = format!(" Feeds ({}) ", state.feeds.len());
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(
            Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, list_area, &mut state.list_state);

    // Status / hint bar.
    let hint = if state.status.is_empty() {
        " ↑↓/jk navigate  enter open  c copy URL  r sync  q quit".to_string()
    } else {
        format!(" {}", state.status)
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        status_area,
    );
}

// ---- Key handling -----------------------------------------------------------

enum KeyAction {
    Continue,
    Quit,
    OpenEpisodes(String),
    Sync(String),
}

fn handle_key(
    handle: &tokio::runtime::Handle,
    db: &Db,
    state: &mut State,
    key: KeyEvent,
) -> Result<KeyAction> {
    // Ignore key-release events (crossterm sends both press and release on some platforms).
    if key.kind != crossterm::event::KeyEventKind::Press {
        return Ok(KeyAction::Continue);
    }

    // Ctrl+C always quits — check before the regular character match.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(KeyAction::Quit);
    }

    // Clear transient status on every keypress.
    state.status.clear();

    match key.code {
        // Navigation
        KeyCode::Up | KeyCode::Char('k') => {
            state.select(state.selected.saturating_sub(1));
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.select(state.selected + 1);
        }
        KeyCode::Home | KeyCode::Char('g') => {
            state.select(0);
        }
        KeyCode::End | KeyCode::Char('G') => {
            let last = state.feeds.len().saturating_sub(1);
            state.select(last);
        }

        // Open episode browser
        KeyCode::Enter => {
            if let Some(feed) = state.selected_feed() {
                return Ok(KeyAction::OpenEpisodes(feed.slug.clone()));
            }
        }

        // Copy restream URL to clipboard
        KeyCode::Char('c') => {
            if let Some(feed) = state.selected_feed() {
                let url = state
                    .cfg
                    .get(&feed.slug)
                    .zip(state.server_cfg.as_ref())
                    .and_then(|(cfg, srv)| feed_restream_url(cfg, srv));
                match url {
                    Some(u) => state.status = copy_to_clipboard(&u),
                    None => state.status = "Feed has no restream URL configured".to_string(),
                }
            }
        }

        // Trigger sync for selected feed
        KeyCode::Char('r') => {
            if let Some(feed) = state.selected_feed() {
                return Ok(KeyAction::Sync(feed.slug.clone()));
            }
        }

        // Show episode count in status bar
        KeyCode::Char('i') => {
            if let Some(feed) = state.selected_feed() {
                let filter = EpisodeFilter {
                    feed_id: Some(feed.id),
                    ..Default::default()
                };
                let count = handle.block_on(db.list_episodes(filter))?.len();
                state.status = format!("{}: {} episodes", feed.slug, count);
            }
        }

        // Quit
        KeyCode::Char('q') | KeyCode::Esc => return Ok(KeyAction::Quit),

        _ => {}
    }

    Ok(KeyAction::Continue)
}
