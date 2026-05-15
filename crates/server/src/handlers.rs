use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use duralumin_core::EpisodeState;

use crate::{AppState, auth::AuthGuard};

pub async fn rss_handler(
    _auth: AuthGuard,
    Path(slug): Path<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let path = state.rss_dir.join(format!("{slug}.xml"));
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "feed not found or RSS not yet generated").into_response();
    }
    // ServeFile handles ETag, Last-Modified, If-None-Match, and Range transparently.
    let svc = ServeFile::new(&path);
    match svc.oneshot(request).await {
        Ok(resp) => {
            // Override MIME — tower-http may use application/xml for .xml files.
            let (mut parts, body) = resp.into_parts();
            parts.headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/xml; charset=utf-8"),
            );
            Response::from_parts(parts, Body::new(body))
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn audio_handler(
    _auth: AuthGuard,
    Path((slug, episode_id_str)): Path<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Response {
    let eid = duralumin_core::EpisodeId::from(episode_id_str.clone());
    let episode = match state.db.get_episode(&eid).await {
        Ok(Some(ep)) => ep,
        Ok(None) => return (StatusCode::NOT_FOUND, "episode not found").into_response(),
        Err(e) => {
            tracing::error!(slug, episode_id = episode_id_str, error = %e, "db error");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Serve local file for Complete and Dynamic (both have a file on disk).
    let local_path = match &episode.state {
        EpisodeState::Complete { path, .. } => Some(path.clone()),
        EpisodeState::Dynamic { path, .. } => Some(path.clone()),
        _ => None,
    };
    if let Some(path) = local_path {
        if path.exists() {
            tracing::debug!(slug, episode_id = episode_id_str, path = %path.display(), "serving local file");
            let svc = ServeFile::new(&path);
            return match svc.oneshot(request).await {
                Ok(resp) => resp.map(Body::new),
                Err(e) => {
                    tracing::warn!(error = %e, "local file serve error, falling back to proxy");
                    proxy(&state, &headers, episode.enclosure_url.as_str()).await
                }
            };
        }
    }

    // Fall back to proxying the origin URL.
    tracing::debug!(slug, episode_id = episode_id_str, url = %episode.enclosure_url, "proxying to origin");
    proxy(&state, &headers, episode.enclosure_url.as_str()).await
}

pub async fn images_handler(
    _auth: AuthGuard,
    Path((slug, filename)): Path<(String, String)>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let path = state.images_dir.join(&slug).join(&filename);
    if !path.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let svc = ServeFile::new(&path);
    match svc.oneshot(request).await {
        Ok(resp) => resp.map(Body::new),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
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
