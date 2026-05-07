use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

use crate::ids::{EpisodeId, FeedId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: EpisodeId,
    pub feed_id: FeedId,
    pub guid: String,
    pub title: String,
    pub description: Option<String>,
    pub pub_date: DateTime<Utc>,
    /// Duration in seconds, matching the `duration_secs` DB column.
    pub duration_secs: Option<u64>,
    pub enclosure_url: Url,
    pub enclosure_size: Option<u64>,
    pub enclosure_mime: Option<String>,
    pub state: EpisodeState,
    /// Per-episode cover art URL from `<itunes:image>`, if present.
    pub image_url: Option<Url>,
}

impl Episode {
    pub fn duration(&self) -> Option<std::time::Duration> {
        self.duration_secs.map(std::time::Duration::from_secs)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EpisodeState {
    Discovered,
    Matched(Action),
    Downloading {
        attempt: u8,
        started_at: DateTime<Utc>,
    },
    Complete {
        path: PathBuf,
        downloaded_at: DateTime<Utc>,
        sha256: String,
    },
    Failed {
        last_error: String,
        attempts: u8,
    },
    Quarantined {
        reason: String,
        last_error: String,
    },
}

impl EpisodeState {
    /// Lowercase discriminant name — used for filtering in storage and CLI output.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Matched(_) => "matched",
            Self::Downloading { .. } => "downloading",
            Self::Complete { .. } => "complete",
            Self::Failed { .. } => "failed",
            Self::Quarantined { .. } => "quarantined",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete { .. } | Self::Quarantined { .. } | Self::Matched(Action::Skip)
        )
    }
}

impl std::fmt::Display for EpisodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discovered => write!(f, "discovered"),
            Self::Matched(a) => write!(f, "matched ({a})"),
            Self::Downloading { attempt, .. } => write!(f, "downloading (attempt {attempt})"),
            Self::Complete { path, .. } => write!(f, "complete → {}", path.display()),
            Self::Failed { attempts, .. } => write!(f, "failed ({attempts} attempt(s))"),
            Self::Quarantined { reason, .. } => write!(f, "quarantined: {reason}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Download,
    Skip,
    Archive,
    Quarantine,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Download => "download",
            Self::Skip => "skip",
            Self::Archive => "archive",
            Self::Quarantine => "quarantine",
        })
    }
}
