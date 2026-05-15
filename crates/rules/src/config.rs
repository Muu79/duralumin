use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use bytesize::ByteSize;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, de};
use url::Url;

use duralumin_core::Action;

// ---- Serde helpers ---------------------------------------------------------

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    humantime::parse_duration(&String::deserialize(d)?).map_err(de::Error::custom)
}

fn de_time_delta<'de, D: Deserializer<'de>>(d: D) -> Result<TimeDelta, D::Error> {
    match humantime::parse_duration(&String::deserialize(d)?) {
        Ok(duration) => TimeDelta::from_std(duration).map_err(de::Error::custom),
        Err(e) => Err(de::Error::custom(e)),
    }
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

fn default_restream_only_matched() -> bool {
    true
}

// ---- Config types ----------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub url: Url,
    pub slug: String,
    /// Optional human-readable name shown in CLI output.
    /// Falls back to the RSS feed title, then the slug.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Alternative identifiers accepted by slug-typed CLI arguments.
    /// Must be globally unique across all slugs and aliases.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "default_poll_interval", deserialize_with = "de_duration")]
    pub poll_interval: Duration,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Expose this feed through the RSS restream server when `dura start` is used.
    /// Requires a `[server]` block in config. Default: false.
    #[serde(default)]
    pub restream: bool,
    /// When `restream = true`, controls which episodes appear in the restreamed RSS feed.
    /// `true` (default): only episodes that are Complete, Dynamic, or queued for Download.
    /// `false`: all episodes, with audio proxied from origin for those not yet downloaded.
    #[serde(default = "default_restream_only_matched")]
    pub restream_only_matched: bool,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub dynamic: Vec<DynamicRuleConfig>,
    /// When set, acts as a catch-all at the end of this feed's rule list,
    /// preventing global rules from firing.  Equivalent to appending an
    /// `always` rule with the given action at priority `i32::MAX`.
    pub default_action: Option<Action>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RuleConfig {
    pub name: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(rename = "match")]
    pub match_: RuleKind,
    pub action: Action,
}

impl Ord for RuleConfig {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl PartialOrd for RuleConfig {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DynamicRuleConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub match_: DynamicRuleKind,
    #[serde(default)]
    pub action: Action,
}

#[derive(Debug, Clone, Deserialize, PartialOrd, PartialEq, Eq)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DynamicRuleKind {
    LastNEpisodes {
        last_n_episodes: usize,
    },
    DurationAgo {
        #[serde(deserialize_with = "de_time_delta")]
        duration: TimeDelta,
    },
}

impl Display for DynamicRuleKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicRuleKind::LastNEpisodes { last_n_episodes: n } => {
                write!(f, "Download the last {} episodes", n)
            }
            DynamicRuleKind::DurationAgo { duration } => {
                let days = duration.num_days();
                write!(f, "Download episodes released over the last {} days", days)
            }
        }
    }
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

pub fn validate_dynamic_rules(dyn_rules: &[DynamicRuleConfig]) -> Vec<String> {
    let mut errors = Vec::new();
    for dyn_rule in dyn_rules {
        match dyn_rule.match_ {
            DynamicRuleKind::LastNEpisodes { last_n_episodes: n } => {
                if n == 0 {
                    errors.push("Set to find the last 0 episodes in dynamic rule, this is probably a mistake.\
                    \nRemove the rule or increase the number of episodes threshold".to_string());
                }
            }
            DynamicRuleKind::DurationAgo { duration } => {
                if duration.num_hours() < 12 {
                    errors.push("Dynamic rule set to keep releases within the last 12 hours, this is probably a mistake\n\
                    Remove the rule or add a duration greater than 12 hours.\n\
                    \tNote that episodes older than what is specified will be purged if not matched by a separate download rule".to_string());
                }
            }
        }
    }
    errors
}
