use anyhow::{Context, Result};
use comfy_table::{Cell, Color, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};
use duralumin_core::{Action, Episode, EpisodeState, Feed, FeedId};
use duralumin_downloader::DownloaderConfig as DlConfig;
use duralumin_rules::{
    RuleEngine,
    config::{DynamicRuleConfig, FeedConfig, RuleConfig, RuleKind},
};
use duralumin_storage::{Db, EpisodeFilter};

use crate::config;

// ---- Config conversion -----------------------------------------------------

impl From<&config::DownloaderConfig> for DlConfig {
    fn from(c: &config::DownloaderConfig) -> Self {
        Self {
            concurrent_downloads: c.concurrent_downloads,
            attempt_timeout: c.attempt_timeout,
            max_retries: c.max_retries,
            backoff_base: c.backoff_base,
            user_agent: c.user_agent.clone(),
            accept_invalid_certs: c.accept_invalid_certs,
            max_bytes_per_sec: c.max_bytes_per_sec,
        }
    }
}

// ---- Slug resolution -------------------------------------------------------

/// Resolve a slug or alias to the canonical feed slug.
/// If the input matches a configured alias, returns the owning feed's slug.
/// Falls back to the input unchanged so a DB lookup will fail naturally.
pub fn resolve_slug(cfg: &config::Config, input: &str) -> String {
    for feed in &cfg.feeds {
        if feed.slug == input {
            return feed.slug.clone();
        }
        if feed.aliases.iter().any(|a| a == input) {
            return feed.slug.clone();
        }
    }
    input.to_owned()
}

// ---- Feed construction -----------------------------------------------------

/// Construct a placeholder `Feed` (id = 0, no metadata) from config for initial upsert.
pub fn feed_from_config(fc: &FeedConfig) -> Feed {
    Feed {
        id: FeedId(0),
        url: fc.url.clone(),
        slug: fc.slug.clone(),
        title: None,
        last_fetched_at: None,
        etag: None,
        last_modified: None,
        enabled: fc.enabled,
        image_url: None,
    }
}

// ---- Rule engine -----------------------------------------------------------

/// Build a `RuleEngine` from the loaded config, upserting feeds first so they
/// have real IDs.
pub async fn build_engine(cfg: &config::Config, db: &Db) -> Result<RuleEngine> {
    let mut per_feed: Vec<(FeedId, Vec<RuleConfig>)> = Vec::new();
    let mut dyn_per_feed: Vec<(FeedId, Vec<DynamicRuleConfig>)> = Vec::new();

    for feed_cfg in &cfg.feeds {
        let feed = feed_from_config(feed_cfg);
        let id = db
            .upsert_feed(&feed)
            .await
            .with_context(|| format!("upsert feed {}", feed_cfg.slug))?;

        let mut rules = feed_cfg.rules.clone();
        let dyn_rules = feed_cfg.dynamic.clone();
        if let Some(action) = feed_cfg.default_action {
            rules.push(RuleConfig {
                name: format!("__feed_default_{}", feed_cfg.slug),
                priority: i32::MAX,
                match_: RuleKind::Always,
                action,
            });
        }
        per_feed.push((id, rules));
        dyn_per_feed.push((id, dyn_rules));
    }

    let pairs: Vec<(FeedId, &[RuleConfig])> = per_feed
        .iter()
        .map(|(id, rules)| (*id, rules.as_slice()))
        .collect();
    let dyn_pairs: Vec<(FeedId, &[DynamicRuleConfig])> = dyn_per_feed
        .iter()
        .map(|(id, rules)| (*id, rules.as_slice()))
        .collect();

    RuleEngine::build(
        &pairs,
        &dyn_pairs,
        &cfg.global_rules,
        &cfg.global_dynamics,
        cfg.defaults.action_on_no_match,
    )
    .context("building rule engine")
}

// ---- Table output helpers --------------------------------------------------

pub fn make_table() -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    t
}

pub fn state_cell(label: &str) -> Cell {
    match label {
        "complete" => Cell::new(label).fg(Color::Green),
        "quarantined" => Cell::new(label).fg(Color::Red),
        "failed" => Cell::new(label).fg(Color::Red),
        "matched" => Cell::new(label).fg(Color::Yellow),
        "downloading" => Cell::new(label).fg(Color::Cyan),
        _ => Cell::new(label),
    }
}

// ---- Episode counts --------------------------------------------------------

#[derive(Default)]
pub struct EpisodeCounts {
    pub dl: usize,
    pub dynamic: usize,
    pub queued: usize,
    pub skipped: usize,
    pub quarantined: usize,
    pub missing: usize,
}

impl EpisodeCounts {
    pub fn tally(episodes: &[Episode]) -> Self {
        let mut c = Self::default();
        for ep in episodes {
            match &ep.state {
                EpisodeState::Complete { path, .. } => {
                    if path.exists() {
                        c.dl += 1;
                    } else {
                        c.missing += 1;
                    }
                }
                EpisodeState::Dynamic { path, .. } => {
                    if path.exists() {
                        c.dl += 1;
                        c.dynamic += 1;
                    } else {
                        c.missing += 1;
                    }
                }
                EpisodeState::Matched(Action::Download | Action::Dynamic)
                | EpisodeState::Failed { .. } => c.queued += 1,
                EpisodeState::Matched(Action::Skip | Action::Purge)
                | EpisodeState::Purged { .. } => c.skipped += 1,
                EpisodeState::Quarantined { .. } => c.quarantined += 1,
                _ => {}
            }
        }
        c
    }
}

// ---- Startup file check ----------------------------------------------------

pub async fn warn_missing_files(db: &Db) {
    let all = match db.list_episodes(EpisodeFilter::default()).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::warn!(error = %e, "could not load episodes for file check");
            return;
        }
    };
    let mut missing = 0usize;
    for ep in &all {
        if let EpisodeState::Complete { path, .. } = &ep.state
            && !path.exists()
        {
            tracing::warn!(
                episode_id = %ep.id,
                path = %path.display(),
                "file missing for Complete episode"
            );
            missing += 1;
        }
    }
    if missing > 0 {
        tracing::warn!(
            count = missing,
            "run `dura check --fix` to requeue missing episodes for re-download"
        );
    }
}

// ---- Cover art -------------------------------------------------------------

pub async fn fetch_cover(episode: &Episode, feed: &Feed) -> Option<Vec<u8>> {
    let url = episode.image_url.as_ref().or(feed.image_url.as_ref())?;
    let resp = match reqwest::get(url.as_str()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "cover art request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(url = %url, status = %resp.status(), "cover art fetch failed");
        return None;
    }
    match resp.bytes().await {
        Ok(b) => Some(b.to_vec()),
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "failed to read cover art bytes");
            None
        }
    }
}
