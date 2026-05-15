mod auth;
mod handlers;
pub mod rss;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{Router, routing::get};
use serde::Deserialize;
use url::Url;

use duralumin_storage::Db;

// ---- Config ----------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub base_url: Url,
    pub auth_token: Option<String>,
}

// ---- App state -------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct AppState {
    pub db: Db,
    pub config: Arc<ServerConfig>,
    pub http: reqwest::Client,
    /// Directory containing pre-generated `{slug}.xml` files.
    pub rss_dir: PathBuf,
    /// Directory containing cached cover images at `{slug}/cover.{ext}`.
    pub images_dir: PathBuf,
}

// ---- Public API ------------------------------------------------------------

pub async fn serve(
    db: Db,
    config: ServerConfig,
    rss_dir: PathBuf,
    images_dir: PathBuf,
) -> anyhow::Result<()> {
    let bind = config.bind;
    let state = AppState {
        db,
        config: Arc::new(config),
        http: reqwest::Client::new(),
        rss_dir,
        images_dir,
    };

    let app = Router::new()
        .route("/rss/{slug}", get(handlers::rss_handler))
        .route("/rss/{slug}/{episode_id}", get(handlers::audio_handler))
        .route("/images/{slug}/{filename}", get(handlers::images_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "RSS server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
