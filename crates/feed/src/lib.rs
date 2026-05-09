use std::io::Cursor;

use duralumin_core::{Episode, EpisodeId, EpisodeState, Feed};
use url::Url;

// ---- Public types ----------------------------------------------------------

/// Metadata returned from a successful feed fetch (may differ from stored values).
pub struct FeedMeta {
    pub title: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub image_url: Option<Url>,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("feed parse error: {0}")]
    Parse(#[from] feed_rs::parser::ParseFeedError),
}

pub type Result<T> = std::result::Result<T, FetchError>;

// ---- FeedFetcher -----------------------------------------------------------

pub struct FeedFetcher {
    client: reqwest::Client,
}

impl FeedFetcher {
    pub fn new(user_agent: &str, accept_invalid_certs: bool) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .danger_accept_invalid_certs(accept_invalid_certs)
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    /// Fetch and parse the feed.
    ///
    /// Returns `(meta, episodes)` where `episodes` is empty on 304 Not Modified.
    /// In the 304 case, `meta` contains the feed's existing etag/last_modified.
    pub async fn fetch(&self, feed: &Feed) -> Result<(FeedMeta, Vec<Episode>)> {
        tracing::info!(
            slug = %feed.slug,
            url  = %feed.url,
            cached = feed.etag.is_some() || feed.last_modified.is_some(),
            "fetching feed"
        );

        let mut rb = self.client.get(feed.url.as_str());
        if let Some(ref etag) = feed.etag {
            rb = rb.header("If-None-Match", etag.as_str());
        }
        if let Some(ref lm) = feed.last_modified {
            rb = rb.header("If-Modified-Since", lm.as_str());
        }

        let resp = rb.send().await?;

        // 304 Not Modified — nothing changed
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            tracing::debug!(slug = %feed.slug, "feed not modified (304), skipping parse");
            return Ok((
                FeedMeta {
                    title: None,
                    etag: feed.etag.clone(),
                    last_modified: feed.last_modified.clone(),
                    image_url: feed.image_url.clone(),
                },
                vec![],
            ));
        }

        if !resp.status().is_success() {
            return Err(FetchError::HttpStatus(resp.status()));
        }

        let etag = resp
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let last_modified = resp
            .headers()
            .get("Last-Modified")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let bytes = resp.bytes().await?;
        let parsed = feed_rs::parser::Builder::new()
            .base_uri(Some(feed.url.as_str()))
            .build()
            .parse(Cursor::new(bytes.as_ref()))?;

        let image_url = parsed
            .logo
            .as_ref()
            .and_then(|img| Url::parse(&img.uri).ok())
            .or_else(|| {
                parsed
                    .icon
                    .as_ref()
                    .and_then(|img| Url::parse(&img.uri).ok())
            });

        let meta = FeedMeta {
            title: parsed.title.map(|t| t.content),
            etag,
            last_modified,
            image_url,
        };

        let episodes: Vec<Episode> = parsed
            .entries
            .iter()
            .filter_map(|entry| entry_to_episode(entry, feed))
            .collect();

        tracing::info!(
            slug = %feed.slug,
            episode_count = episodes.len(),
            "parsed feed"
        );

        Ok((meta, episodes))
    }
}

// ---- Public helper for testing ---------------------------------------------

/// Parse raw RSS/Atom bytes as if they came from `feed`.
/// Skips malformed entries (no enclosure, no date) with a tracing warn.
pub fn parse_bytes(bytes: &[u8], feed: &Feed) -> Result<Vec<Episode>> {
    let parsed = feed_rs::parser::Builder::new()
        .base_uri(Some(feed.url.as_str()))
        .build()
        .parse(std::io::Cursor::new(bytes))?;

    let episodes = parsed
        .entries
        .iter()
        .filter_map(|entry| entry_to_episode(entry, feed))
        .collect();

    Ok(episodes)
}

// ---- Entry → Episode mapping -----------------------------------------------

fn entry_to_episode(entry: &feed_rs::model::Entry, feed: &Feed) -> Option<Episode> {
    // Enclosure is required — skip without one
    let (enc_url, enc_size, enc_mime) = match extract_enclosure(entry) {
        Some(e) => e,
        None => {
            tracing::warn!(
                feed_slug = %feed.slug,
                entry_id  = %entry.id,
                "skipping entry: no audio enclosure"
            );
            return None;
        }
    };

    // GUID: prefer entry.id, fall back to enclosure URL
    let guid = if !entry.id.is_empty() {
        entry.id.clone()
    } else {
        enc_url.to_string()
    };

    // pub_date is required for filename construction
    let pub_date = match entry.published.or(entry.updated) {
        Some(d) => d,
        None => {
            tracing::warn!(
                feed_slug = %feed.slug,
                entry_id  = %entry.id,
                "skipping entry: no publish date"
            );
            return None;
        }
    };

    let id = EpisodeId::from_parts(feed.url.as_str(), &guid);

    let title = entry
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .unwrap_or_else(|| guid.clone());

    let description = entry
        .summary
        .as_ref()
        .map(|t| t.content.clone())
        .or_else(|| entry.content.as_ref().and_then(|c| c.body.clone()));

    let duration_secs = entry
        .media
        .first()
        .and_then(|m| m.duration)
        .map(|d| d.as_secs());

    let image_url = entry
        .media
        .first()
        .and_then(|m| m.thumbnails.first())
        .and_then(|t| Url::parse(&t.image.uri).ok());

    Some(Episode {
        id,
        feed_id: feed.id,
        guid,
        title,
        description,
        pub_date,
        duration_secs,
        enclosure_url: enc_url,
        enclosure_size: enc_size,
        enclosure_mime: enc_mime,
        state: EpisodeState::Discovered,
        image_url,
    })
}

/// Extract the first usable audio enclosure from a feed entry.
///
/// Checks `entry.media[*].content[*]` first (RSS `<enclosure>` and
/// `<media:content>` both map here via feed-rs), then falls back to
/// `entry.links` with `rel = "enclosure"`.
fn extract_enclosure(entry: &feed_rs::model::Entry) -> Option<(Url, Option<u64>, Option<String>)> {
    for media in &entry.media {
        for content in &media.content {
            if let Some(ref url) = content.url {
                let mime = content.content_type.as_ref().map(|m| m.to_string());
                let size = content.size;
                return Some((url.clone(), size, mime));
            }
        }
    }
    for link in &entry.links {
        if link.rel.as_deref() == Some("enclosure")
            && let Ok(url) = Url::parse(&link.href)
        {
            return Some((url, None, link.media_type.clone()));
        }
    }
    None
}
