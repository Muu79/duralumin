use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, poll, read};
use ratatui::{
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph},
};

use duralumin_core::{Action, EpisodeState, FeedId};
use duralumin_rules::config::FeedConfig;
use duralumin_server::ServerConfig;
use duralumin_storage::{Db, EpisodeFilter};

use super::{Term, centered_rect, copy_to_clipboard, format_bytes, format_dur, trunc};

// ---- Status symbol / colour per episode state -------------------------------

struct EpStyle {
    symbol: &'static str,
    color: Color,
}

fn ep_style(state: &EpisodeState) -> EpStyle {
    match state {
        EpisodeState::Complete { .. } | EpisodeState::Dynamic { .. } => EpStyle {
            symbol: "✓",
            color: Color::Green,
        },
        EpisodeState::Matched(Action::Download | Action::Dynamic) => EpStyle {
            symbol: "↓",
            color: Color::Cyan,
        },
        EpisodeState::Matched(Action::Skip | Action::Purge) => EpStyle {
            symbol: "–",
            color: Color::DarkGray,
        },
        EpisodeState::Discovered => EpStyle {
            symbol: "?",
            color: Color::Yellow,
        },
        EpisodeState::Quarantined { .. } => EpStyle {
            symbol: "!",
            color: Color::Red,
        },
        EpisodeState::Purged { .. } => EpStyle {
            symbol: "–",
            color: Color::DarkGray,
        },
        EpisodeState::Failed { .. } | EpisodeState::Downloading { .. } => EpStyle {
            symbol: "↓",
            color: Color::Blue,
        },
        _ => EpStyle {
            symbol: " ",
            color: Color::Reset,
        },
    }
}

// ---- Popup kinds ------------------------------------------------------------

enum Popup {
    Url { ep_idx: usize },
    ConfirmDelete { ep_idx: usize },
}

// ---- State ------------------------------------------------------------------

struct State {
    feed_id: FeedId,
    feed_slug: String,
    feed_title: String,
    feed_cfg: Option<FeedConfig>,
    /// All episodes for the feed (unfiltered, newest first).
    all: Vec<duralumin_core::Episode>,
    /// Indices into `all` that pass the current filter.
    visible: Vec<usize>,
    filter: String,
    filter_mode: bool,
    selected: usize,
    list_state: ListState,
    popup: Option<Popup>,
    server_cfg: Option<ServerConfig>,
    status: String,
}

impl State {
    fn select(&mut self, idx: usize) {
        self.selected = idx.min(self.visible.len().saturating_sub(1));
        self.list_state.select(Some(self.selected));
    }

    fn refilter(&mut self) {
        let lower = self.filter.to_lowercase();
        self.visible = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, ep)| lower.is_empty() || ep.title.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect();
        // Keep selection in bounds.
        self.select(self.selected);
    }

    fn selected_ep(&self) -> Option<&duralumin_core::Episode> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.all.get(i))
    }

    fn count_complete(&self) -> usize {
        self.all
            .iter()
            .filter(|ep| {
                matches!(
                    &ep.state,
                    EpisodeState::Complete { .. } | EpisodeState::Dynamic { .. }
                )
            })
            .count()
    }
}

// ---- Entry point ------------------------------------------------------------

pub fn run(
    handle: &tokio::runtime::Handle,
    db: &Db,
    cfg_feeds: &[FeedConfig],
    server_cfg: Option<&ServerConfig>,
    terminal: &mut Term,
    slug: &str,
) -> Result<()> {
    let feed = match handle.block_on(db.get_feed_by_slug(slug))? {
        Some(f) => f,
        None => {
            // Feed not in DB yet — just return silently.
            return Ok(());
        }
    };

    let all = load_episodes(handle, db, feed.id)?;
    let feed_cfg = cfg_feeds.iter().find(|c| c.slug == slug).cloned();
    let feed_title = feed.title.clone().unwrap_or_else(|| slug.to_string());

    let mut state = State {
        feed_id: feed.id,
        feed_slug: slug.to_string(),
        feed_title,
        feed_cfg,
        all,
        visible: Vec::new(),
        filter: String::new(),
        filter_mode: false,
        selected: 0,
        list_state: ListState::default(),
        popup: None,
        server_cfg: server_cfg.cloned(),
        status: String::new(),
    };
    state.refilter();
    state.list_state.select(Some(0));

    loop {
        terminal.draw(|f| render(f, &state))?;

        if !poll(Duration::from_millis(100))? {
            continue;
        }

        match read()? {
            Event::Key(key) => {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                state.status.clear();

                let done = if state.popup.is_some() {
                    handle_key_popup(handle, db, &mut state, key)?
                } else if state.filter_mode {
                    handle_key_filter(&mut state, key)
                } else {
                    handle_key_normal(handle, db, &mut state, key)?
                };

                if done {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => terminal.autoresize()?,
            _ => {}
        }
    }
}

fn load_episodes(
    handle: &tokio::runtime::Handle,
    db: &Db,
    feed_id: FeedId,
) -> Result<Vec<duralumin_core::Episode>> {
    Ok(handle.block_on(db.list_episodes(EpisodeFilter {
        feed_id: Some(feed_id),
        ..Default::default()
    }))?)
}

// ---- Rendering --------------------------------------------------------------

fn render(f: &mut ratatui::Frame, state: &State) {
    let area = f.area();

    // Layout: header | filter bar | list | status bar
    let [header_area, filter_area, list_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // Header
    let complete = state.count_complete();
    let total = state.all.len();
    let title = format!(
        " {} — {} episodes (✓ {}/{})",
        trunc(&state.feed_title, 40),
        if state.filter.is_empty() {
            total
        } else {
            state.visible.len()
        },
        complete,
        total,
    );
    f.render_widget(
        Paragraph::new(title).style(Style::new().add_modifier(Modifier::BOLD)),
        header_area,
    );

    // Filter bar
    let filter_content = if state.filter_mode {
        format!(" / Filter: {}█", state.filter)
    } else if !state.filter.is_empty() {
        format!(" / Filter: {} (esc to clear)", state.filter)
    } else {
        " / to filter".to_string()
    };
    let filter_style = if state.filter_mode {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    f.render_widget(
        Paragraph::new(filter_content).style(filter_style),
        filter_area,
    );

    // Episode list
    let inner_width = list_area.width.saturating_sub(2) as usize; // inside border
    // Fixed columns: st(3) + gap(2) + date(10) + gap(2) + size(8) + gap(2) + dur(8) = 35
    let fixed = 35usize;
    let title_w = inner_width.saturating_sub(fixed);

    let mut list_state = state.list_state;
    let items: Vec<ListItem> = state
        .visible
        .iter()
        .map(|&idx| {
            let ep = &state.all[idx];
            let sty = ep_style(&ep.state);
            let date = ep.pub_date.format("%Y-%m-%d").to_string();
            let size = format_bytes(ep.enclosure_size);
            let dur = format_dur(ep.duration_secs);
            let title_col = format!("{:<width$}", trunc(&ep.title, title_w), width = title_w);

            Line::from(vec![
                Span::styled(
                    format!("{:<3}", sty.symbol),
                    Style::new().fg(sty.color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(title_col),
                Span::styled(format!("  {:<10}", date), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("  {:>8}", size), Style::new()),
                Span::styled(format!("  {:>8}", dur), Style::new().fg(Color::DarkGray)),
            ])
        })
        .map(ListItem::new)
        .collect();

    let list = List::new(items)
        .block(Block::bordered())
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, list_area, &mut list_state);

    // Status / hint bar
    let hint = if !state.status.is_empty() {
        format!(" {}", state.status)
    } else if state.filter_mode {
        " Type to filter, esc to exit filter".to_string()
    } else {
        " ↑↓/jk nav  / filter  d download  s skip  x del+skip  u URLs  q back".to_string()
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        status_area,
    );

    // Overlays
    if let Some(popup) = &state.popup {
        match popup {
            Popup::Url { ep_idx } => {
                if let Some(ep) = state.all.get(*ep_idx) {
                    render_url_popup(f, state, ep, area);
                }
            }
            Popup::ConfirmDelete { ep_idx } => {
                if let Some(ep) = state.all.get(*ep_idx) {
                    render_confirm_popup(f, ep, area);
                }
            }
        }
    }
}

fn render_url_popup(
    f: &mut ratatui::Frame,
    state: &State,
    ep: &duralumin_core::Episode,
    area: ratatui::layout::Rect,
) {
    let popup_area = centered_rect(72, 8, area);
    f.render_widget(Clear, popup_area);

    let orig = ep.enclosure_url.as_str();
    let restream = state
        .feed_cfg
        .as_ref()
        .zip(state.server_cfg.as_ref())
        .and_then(|(cfg, srv)| {
            if cfg.restream {
                let base = srv.base_url.as_str().trim_end_matches('/');
                let key = srv
                    .auth_token
                    .as_deref()
                    .map(|t| format!("?key={t}"))
                    .unwrap_or_default();
                Some(format!("{base}/rss/{}/{}{key}", state.feed_slug, ep.id))
            } else {
                None
            }
        })
        .unwrap_or_else(|| "(restreaming not configured)".to_string());

    let title_line = trunc(&ep.title, 50);
    let content = vec![
        Line::from(vec![
            Span::styled("  Original:  ", Style::new().fg(Color::DarkGray)),
            Span::raw(trunc(orig, (popup_area.width as usize).saturating_sub(14))),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Restream:  ", Style::new().fg(Color::DarkGray)),
            Span::raw(trunc(
                &restream,
                (popup_area.width as usize).saturating_sub(14),
            )),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "  [c] copy original  [r] copy restream  [esc] close",
            Style::new().fg(Color::DarkGray),
        )]),
    ];

    f.render_widget(
        Paragraph::new(content).block(Block::bordered().title(format!(" URLs — {title_line} "))),
        popup_area,
    );
}

fn render_confirm_popup(
    f: &mut ratatui::Frame,
    ep: &duralumin_core::Episode,
    area: ratatui::layout::Rect,
) {
    let popup_area = centered_rect(50, 6, area);
    f.render_widget(Clear, popup_area);

    let content = vec![
        Line::raw(""),
        Line::from(Span::raw(format!("  {}", trunc(&ep.title, 44)))),
        Line::raw(""),
        Line::from(Span::styled(
            "  The file will be deleted from disk.",
            Style::new().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  [y] confirm delete     [n/esc] cancel",
            Style::new().fg(Color::DarkGray),
        )),
    ];

    f.render_widget(
        Paragraph::new(content).block(
            Block::bordered()
                .title(" Delete episode file? ")
                .title_alignment(Alignment::Center),
        ),
        popup_area,
    );
}

// ---- Key handlers -----------------------------------------------------------

/// Returns `true` when the episode browser should exit (go back to feed browser).
fn handle_key_normal(
    handle: &tokio::runtime::Handle,
    db: &Db,
    state: &mut State,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        // Navigation
        KeyCode::Up | KeyCode::Char('k') => state.select(state.selected.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => state.select(state.selected + 1),
        KeyCode::Home | KeyCode::Char('g') => state.select(0),
        KeyCode::End | KeyCode::Char('G') => {
            let last = state.visible.len().saturating_sub(1);
            state.select(last);
        }
        KeyCode::PageUp => state.select(state.selected.saturating_sub(10)),
        KeyCode::PageDown => state.select(state.selected + 10),

        // Enter filter mode
        KeyCode::Char('/') => {
            state.filter_mode = true;
        }

        // Queue for download
        KeyCode::Char('d') => {
            if let Some(ep) = state.selected_ep() {
                let id = ep.id.clone();
                let new_state = EpisodeState::Matched(Action::Download);
                handle.block_on(db.update_episode_state(&id, &new_state))?;
                handle.block_on(db.enqueue(&id, Action::Download))?;
                state.all = load_episodes(handle, db, state.feed_id)?;
                state.refilter();
                state.status = "Queued for download".to_string();
            }
        }

        // Skip
        KeyCode::Char('s') => {
            if let Some(ep) = state.selected_ep() {
                let id = ep.id.clone();
                let new_state = EpisodeState::Matched(Action::Skip);
                handle.block_on(db.update_episode_state(&id, &new_state))?;
                handle.block_on(db.dequeue(&id))?;
                state.all = load_episodes(handle, db, state.feed_id)?;
                state.refilter();
                state.status = "Marked as skipped".to_string();
            }
        }

        // Delete file + skip (shows confirm popup)
        KeyCode::Char('x') => {
            if let Some(&ep_idx) = state.visible.get(state.selected) {
                state.popup = Some(Popup::ConfirmDelete { ep_idx });
            }
        }

        // URL popup
        KeyCode::Char('u') | KeyCode::Enter => {
            if let Some(&ep_idx) = state.visible.get(state.selected) {
                state.popup = Some(Popup::Url { ep_idx });
            }
        }

        // Back
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(true);
        }

        _ => {}
    }
    Ok(false)
}

fn handle_key_filter(state: &mut State, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            state.filter_mode = false;
            state.filter.clear();
            state.refilter();
        }
        KeyCode::Enter => {
            state.filter_mode = false;
        }
        KeyCode::Backspace => {
            state.filter.pop();
            state.refilter();
        }
        KeyCode::Char(c) => {
            state.filter.push(c);
            state.refilter();
        }
        _ => {}
    }
    false
}

fn handle_key_popup(
    handle: &tokio::runtime::Handle,
    db: &Db,
    state: &mut State,
    key: KeyEvent,
) -> Result<bool> {
    match &state.popup {
        Some(Popup::Url { ep_idx }) => {
            let ep_idx = *ep_idx;
            match key.code {
                KeyCode::Char('c') => {
                    if let Some(ep) = state.all.get(ep_idx) {
                        state.status = copy_to_clipboard(ep.enclosure_url.as_str());
                    }
                    state.popup = None;
                }
                KeyCode::Char('r') => {
                    // Copy restream URL
                    let url = state.all.get(ep_idx).and_then(|ep| {
                        let cfg = state.feed_cfg.as_ref()?;
                        let srv = state.server_cfg.as_ref()?;
                        if !cfg.restream {
                            return None;
                        }
                        let base = srv.base_url.as_str().trim_end_matches('/');
                        let key_sfx = srv
                            .auth_token
                            .as_deref()
                            .map(|t| format!("?key={t}"))
                            .unwrap_or_default();
                        Some(format!("{base}/rss/{}/{}{key_sfx}", state.feed_slug, ep.id))
                    });
                    match url {
                        Some(u) => state.status = copy_to_clipboard(&u),
                        None => state.status = "Restreaming not configured".to_string(),
                    }
                    state.popup = None;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    state.popup = None;
                }
                _ => {}
            }
        }

        Some(Popup::ConfirmDelete { ep_idx }) => {
            let ep_idx = *ep_idx;
            match key.code {
                KeyCode::Char('y') => {
                    if let Some(ep) = state.all.get(ep_idx) {
                        // Delete file if present.
                        if let EpisodeState::Complete { ref path, .. }
                        | EpisodeState::Dynamic { ref path, .. } = ep.state
                        {
                            let _ = std::fs::remove_file(path);
                        }
                        let id = ep.id.clone();
                        let new_state = EpisodeState::Matched(Action::Skip);
                        handle.block_on(db.update_episode_state(&id, &new_state))?;
                        handle.block_on(db.dequeue(&id))?;
                    }
                    state.popup = None;
                    state.all = load_episodes(handle, db, state.feed_id)?;
                    state.refilter();
                    state.status = "File deleted, episode marked as skipped".to_string();
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    state.popup = None;
                }
                _ => {}
            }
        }

        None => {}
    }
    Ok(false)
}
