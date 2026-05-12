use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use duralumin_core::{Action, EpisodeId, EpisodeState};
use duralumin_storage::EpisodeFilter;

use crate::{AppState, auth::AuthGuard, rss};

pub async fn rss_handler(
    _auth: AuthGuard,
    Path(slug): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let feed = match state.db.get_feed_by_slug(&slug).await {
        Ok(Some(f)) => f,
        Ok(None) => return (StatusCode::NOT_FOUND, "feed not found").into_response(),
        Err(e) => {
            tracing::error!(slug, error = %e, "db error fetching feed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let filter = EpisodeFilter {
        feed_id: Some(feed.id),
        ..Default::default()
    };
    let episodes = match state.db.list_episodes(filter).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(slug, error = %e, "db error listing episodes");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Only expose downloaded or queued episodes. Skipped, Discovered, and
    // Quarantined episodes are not meaningful to a podcast app.
    let episodes: Vec<_> = episodes
        .into_iter()
        .filter(|ep| {
            matches!(
                &ep.state,
                EpisodeState::Complete { .. } | EpisodeState::Matched(Action::Download)
            )
        })
        .collect();

    let xml = rss::build_rss(&feed, &episodes, &state.config, &slug);
    tracing::debug!(slug, episodes = episodes.len(), "served RSS feed");

    (
        [(axum::http::header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

pub async fn audio_handler(
    _auth: AuthGuard,
    Path((slug, episode_id_str)): Path<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Response {
    let eid = EpisodeId::from(episode_id_str.clone());
    let episode = match state.db.get_episode(&eid).await {
        Ok(Some(ep)) => ep,
        Ok(None) => return (StatusCode::NOT_FOUND, "episode not found").into_response(),
        Err(e) => {
            tracing::error!(slug, episode_id = episode_id_str, error = %e, "db error");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Serve local file if downloaded
    if let EpisodeState::Complete { path, .. } = &episode.state
        && path.exists()
    {
        tracing::debug!(slug, episode_id = episode_id_str, path = %path.display(), "serving local file");
        let svc = ServeFile::new(path);
        return match svc.oneshot(request).await {
            Ok(resp) => resp.map(Body::new),
            Err(e) => {
                tracing::warn!(error = %e, "local file serve error, falling back to proxy");
                proxy(&state, &headers, episode.enclosure_url.as_str()).await
            }
        };
    }

    // Fall back to proxying the origin URL
    tracing::debug!(slug, episode_id = episode_id_str, url = %episode.enclosure_url, "proxying to origin");
    proxy(&state, &headers, episode.enclosure_url.as_str()).await
}

async fn proxy(state: &AppState, headers: &HeaderMap, url: &str) -> Response {
    let mut req = state.http.get(url);
    if let Some(range) = headers.get("Range") {
        req = req.header("Range", range);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(url, error = %e, "proxy request failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let stream = resp.bytes_stream();

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    // Forward useful headers from origin (Content-Type, Content-Length, Accept-Ranges, Content-Range)
    for key in &[
        "Content-Type",
        "Content-Length",
        "Accept-Ranges",
        "Content-Range",
    ] {
        if let Some(v) = resp_headers.get(*key) {
            response.headers_mut().insert(
                axum::http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                v.clone(),
            );
        }
    }
    response
}
