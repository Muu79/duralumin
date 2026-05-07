use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ids::FeedId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feed {
    /// `0` means not yet persisted; set by storage after first upsert.
    pub id: FeedId,
    pub url: Url,
    pub slug: String,
    pub title: Option<String>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub enabled: bool,
}
