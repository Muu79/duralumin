use std::time::Duration;

use bytesize::ByteSize;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, de};
use url::Url;

use duralumin_core::Action;

// ---- Serde helpers ---------------------------------------------------------

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    humantime::parse_duration(&String::deserialize(d)?).map_err(de::Error::custom)
}

fn de_bytesize<'de, D: Deserializer<'de>>(d: D) -> Result<ByteSize, D::Error> {
    String::deserialize(d)?
        .parse::<ByteSize>()
        .map_err(de::Error::custom)
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(3600) // 1h
}

fn default_enabled() -> bool {
    true
}

// ---- Config types ----------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub url: Url,
    pub slug: String,
    #[serde(default = "default_poll_interval", deserialize_with = "de_duration")]
    pub poll_interval: Duration,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Expose this feed through the RSS restream server when `dura start` is used.
    /// Requires a `[server]` block in config. Default: false.
    #[serde(default)]
    pub restream: bool,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    /// When set, acts as a catch-all at the end of this feed's rule list,
    /// preventing global rules from firing.  Equivalent to appending an
    /// `always` rule with the given action at priority `i32::MAX`.
    pub default_action: Option<Action>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleConfig {
    pub name: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(rename = "match")]
    pub match_: RuleKind,
    pub action: Action,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleKind {
    TitleRegex {
        pattern: String,
    },
    DescriptionRegex {
        pattern: String,
    },
    DurationMin {
        #[serde(deserialize_with = "de_duration")]
        value: Duration,
    },
    DurationMax {
        #[serde(deserialize_with = "de_duration")]
        value: Duration,
    },
    PublishedAfter {
        date: DateTime<Utc>,
    },
    PublishedBefore {
        date: DateTime<Utc>,
    },
    EpisodeSizeMax {
        #[serde(deserialize_with = "de_bytesize")]
        value: ByteSize,
    },
    Always,
}

// ---- Validation ------------------------------------------------------------

/// Validate a slice of rule configs. Returns human-readable error strings.
/// Called at config load time so bad regex / durations surface immediately.
pub fn validate_rules(rules: &[RuleConfig]) -> Vec<String> {
    let mut errors = Vec::new();
    for rule in rules {
        match &rule.match_ {
            RuleKind::TitleRegex { pattern } | RuleKind::DescriptionRegex { pattern } => {
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!(
                        "rule {:?}: invalid regex {:?}: {e}",
                        rule.name, pattern
                    ));
                }
            }
            // Duration fields are already parsed at deserialise time, so they
            // only reach here if valid. No secondary check needed.
            RuleKind::DurationMin { .. }
            | RuleKind::DurationMax { .. }
            | RuleKind::PublishedAfter { .. }
            | RuleKind::PublishedBefore { .. }
            | RuleKind::EpisodeSizeMax { .. }
            | RuleKind::Always => {}
        }
    }
    errors
}
