use std::path::{Path, PathBuf};
use std::sync::Arc;

use duralumin_core::{Action, EpisodeState};
use duralumin_rules::config::FeedConfig;
use duralumin_server::ServerConfig;
use duralumin_storage::{Db, EpisodeFilter};

/// Everything the RSS generator needs beyond the DB handle.
#[derive(Clone)]
pub struct RssContext {
    pub server_cfg: Arc<ServerConfig>,
    /// Root for pre-generated `{slug}.xml` feed files.
    pub rss_dir: PathBuf,
    /// Root for cached cover art at `{slug}/cover.{ext}`.
    pub images_dir: PathBuf,
    pub http: reqwest::Client,
}

/// Regenerate the static `{rss_dir}/{slug}.xml` for a restream-enabled feed.
///
/// Fetches episodes from the DB, applies the `restream_only_matched` filter,
/// downloads the cover image if not already cached, then writes the XML file.
/// No-ops quietly if the feed isn't in the DB yet.
pub async fn generate_rss_for_feed(
    feed_cfg: &FeedConfig,
    db: &Db,
    ctx: &RssContext,
) -> anyhow::Result<()> {
    let feed = match db.get_feed_by_slug(&feed_cfg.slug).await? {
        Some(f) => f,
        None => return Ok(()),
    };

    let all_episodes = db
        .list_episodes(EpisodeFilter {
            feed_id: Some(feed.id),
            ..Default::default()
        })
        .await?;

    let episodes: Vec<_> = if feed_cfg.restream_only_matched {
        all_episodes
            .into_iter()
            .filter(|ep| {
                matches!(
                    &ep.state,
                    EpisodeState::Complete { .. }
                        | EpisodeState::Dynamic { .. }
                        | EpisodeState::Matched(Action::Download)
                        | EpisodeState::Matched(Action::Dynamic)
                )
            })
            .collect()
    } else {
        all_episodes
    };

    let cover_url = cache_cover_image(feed_cfg, &feed, ctx).await;

    let xml = duralumin_server::rss::build_rss(
        &feed,
        &episodes,
        &ctx.server_cfg,
        &feed_cfg.slug,
        cover_url.as_deref(),
    );

    tokio::fs::create_dir_all(&ctx.rss_dir).await?;
    let rss_path = ctx.rss_dir.join(format!("{}.xml", feed_cfg.slug));
    tokio::fs::write(&rss_path, xml.as_bytes()).await?;
    tracing::info!(slug = %feed_cfg.slug, path = %rss_path.display(), episodes = episodes.len(), "RSS file written");
    Ok(())
}

/// Download and cache the feed's cover image to `{images_dir}/{slug}/cover.{ext}`.
///
/// Reuses a cached file if it was written within the last 24 hours.
/// Returns the public URL pointing at the locally-served image, or `None` if
/// no image URL is set on the feed or the download fails.
async fn cache_cover_image(
    feed_cfg: &FeedConfig,
    feed: &duralumin_core::Feed,
    ctx: &RssContext,
) -> Option<String> {
    let image_url = feed.image_url.as_ref()?;

    // Derive extension from the URL path; fall back to "jpg".
    let ext = image_url
        .path()
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("jpg");

    let slug_images_dir = ctx.images_dir.join(&feed_cfg.slug);
    let cover_path = slug_images_dir.join(format!("cover.{ext}"));

    // Reuse cached file if it was written within the last 24 hours.
    if is_fresh(&cover_path, 86400) {
        return Some(build_cover_url(ctx, &feed_cfg.slug, ext));
    }

    if let Err(e) = tokio::fs::create_dir_all(&slug_images_dir).await {
        tracing::warn!(slug = %feed_cfg.slug, error = %e, "failed to create images dir");
        return None;
    }

    match ctx.http.get(image_url.as_str()).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.bytes().await {
                Ok(bytes) => {
                    if let Err(e) = tokio::fs::write(&cover_path, &bytes).await {
                        tracing::warn!(slug = %feed_cfg.slug, error = %e, "failed to write cover image");
                        return None;
                    }
                    Some(build_cover_url(ctx, &feed_cfg.slug, ext))
                }
                Err(e) => {
                    tracing::warn!(slug = %feed_cfg.slug, error = %e, "failed to read cover image response");
                    None
                }
            }
        }
        Ok(resp) => {
            tracing::warn!(slug = %feed_cfg.slug, status = %resp.status(), "cover image fetch returned non-2xx");
            None
        }
        Err(e) => {
            tracing::warn!(slug = %feed_cfg.slug, error = %e, "failed to fetch cover image");
            // Return the cached path URL if the file exists at all (stale is better than nothing).
            if cover_path.exists() {
                Some(build_cover_url(ctx, &feed_cfg.slug, ext))
            } else {
                None
            }
        }
    }
}

fn build_cover_url(ctx: &RssContext, slug: &str, ext: &str) -> String {
    let base = ctx.server_cfg.base_url.as_str().trim_end_matches('/');
    let key_suffix = ctx
        .server_cfg
        .auth_token
        .as_deref()
        .map(|t| format!("?key={}", t))
        .unwrap_or_default();
    format!("{}/images/{}/cover.{}{}", base, slug, ext, key_suffix)
}

/// Return `true` if `path` exists and its mtime is within `max_age_secs`.
fn is_fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    age.as_secs() < max_age_secs
}
