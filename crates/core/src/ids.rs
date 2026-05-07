use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EpisodeId(String);

impl EpisodeId {
    pub fn from_parts(feed_url: &str, guid: &str) -> Self {
        let mut h = Sha256::new();
        h.update(feed_url.as_bytes());
        h.update(b"\0");
        h.update(guid.as_bytes());
        Self(hex::encode(h.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First 8 hex chars — used for collision suffixes in filenames.
    pub fn short(&self) -> &str {
        &self.0[..8]
    }
}

impl fmt::Display for EpisodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for EpisodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<EpisodeId> for String {
    fn from(id: EpisodeId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FeedId(pub i64);

impl fmt::Display for FeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_is_deterministic() {
        let a = EpisodeId::from_parts("https://example.com/feed", "episode-1");
        let b = EpisodeId::from_parts("https://example.com/feed", "episode-1");
        assert_eq!(a, b);
    }

    #[test]
    fn from_parts_distinguishes_feed_and_guid() {
        let a = EpisodeId::from_parts("https://feed-a.com/rss", "ep-1");
        let b = EpisodeId::from_parts("https://feed-b.com/rss", "ep-1");
        let c = EpisodeId::from_parts("https://feed-a.com/rss", "ep-2");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn short_is_8_chars() {
        let id = EpisodeId::from_parts("url", "guid");
        assert_eq!(id.short().len(), 8);
    }
}
