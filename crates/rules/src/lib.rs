pub mod config;

pub use config::{FeedConfig, RuleConfig, RuleKind, validate_rules};

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use duralumin_core::{Action, Episode, Feed, FeedId};
use regex::Regex;

// ---- Rule trait ------------------------------------------------------------

pub trait Rule: Send + Sync {
    /// `None` = this rule does not apply; keep evaluating.
    /// `Some(action)` = match; stop and use this action.
    fn evaluate(&self, episode: &Episode, feed: &Feed) -> Option<Action>;
    fn name(&self) -> &str;
}

// ---- Error type ------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("rule {name:?}: invalid regex {pattern:?}: {source}")]
    Regex {
        name: String,
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

// ---- Built-in rule structs (private) ---------------------------------------

struct TitleRegexRule {
    regex: Regex,
    action: Action,
    name: String,
}

impl Rule for TitleRegexRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        self.regex.is_match(&episode.title).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct DescriptionRegexRule {
    regex: Regex,
    action: Action,
    name: String,
}

impl Rule for DescriptionRegexRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        episode
            .description
            .as_deref()
            .filter(|d| self.regex.is_match(d))
            .map(|_| self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct DurationMinRule {
    min: Duration,
    action: Action,
    name: String,
}

impl Rule for DurationMinRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        // None duration → no match (spec §4.3)
        (episode.duration()? >= self.min).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct DurationMaxRule {
    max: Duration,
    action: Action,
    name: String,
}

impl Rule for DurationMaxRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        (episode.duration()? <= self.max).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct PublishedAfterRule {
    after: DateTime<Utc>,
    action: Action,
    name: String,
}

impl Rule for PublishedAfterRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        (episode.pub_date > self.after).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct PublishedBeforeRule {
    before: DateTime<Utc>,
    action: Action,
    name: String,
}

impl Rule for PublishedBeforeRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        (episode.pub_date < self.before).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct EpisodeSizeMaxRule {
    max_bytes: u64,
    action: Action,
    name: String,
}

impl Rule for EpisodeSizeMaxRule {
    fn evaluate(&self, episode: &Episode, _feed: &Feed) -> Option<Action> {
        // None enclosure_size → no match (spec §4.3)
        (episode.enclosure_size? <= self.max_bytes).then_some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

struct AlwaysRule {
    action: Action,
    name: String,
}

impl Rule for AlwaysRule {
    fn evaluate(&self, _episode: &Episode, _feed: &Feed) -> Option<Action> {
        Some(self.action)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

// ---- Factory ---------------------------------------------------------------

fn build_rule(cfg: &RuleConfig) -> Result<Box<dyn Rule>, RuleError> {
    let name = cfg.name.clone();
    let action = cfg.action;

    let rule: Box<dyn Rule> = match &cfg.match_ {
        RuleKind::TitleRegex { pattern } => {
            let regex = Regex::new(pattern).map_err(|e| RuleError::Regex {
                name: name.clone(),
                pattern: pattern.clone(),
                source: e,
            })?;
            Box::new(TitleRegexRule {
                regex,
                action,
                name,
            })
        }
        RuleKind::DescriptionRegex { pattern } => {
            let regex = Regex::new(pattern).map_err(|e| RuleError::Regex {
                name: name.clone(),
                pattern: pattern.clone(),
                source: e,
            })?;
            Box::new(DescriptionRegexRule {
                regex,
                action,
                name,
            })
        }
        // Duration/date/size are already parsed at deserialise time — just copy.
        RuleKind::DurationMin { value } => Box::new(DurationMinRule {
            min: *value,
            action,
            name,
        }),
        RuleKind::DurationMax { value } => Box::new(DurationMaxRule {
            max: *value,
            action,
            name,
        }),
        RuleKind::PublishedAfter { date } => Box::new(PublishedAfterRule {
            after: *date,
            action,
            name,
        }),
        RuleKind::PublishedBefore { date } => Box::new(PublishedBeforeRule {
            before: *date,
            action,
            name,
        }),
        RuleKind::EpisodeSizeMax { value } => Box::new(EpisodeSizeMaxRule {
            max_bytes: value.0,
            action,
            name,
        }),
        RuleKind::Always => Box::new(AlwaysRule { action, name }),
    };

    Ok(rule)
}

// ---- RuleEngine ------------------------------------------------------------

pub struct RuleEngine {
    /// Per-feed rules, sorted ascending by config priority (lower = first).
    per_feed: HashMap<FeedId, Vec<Box<dyn Rule>>>,
    /// Global catch-all rules, applied after per-feed rules.
    global: Vec<Box<dyn Rule>>,
    /// Fallback when no rule matches.
    default: Action,
}

impl RuleEngine {
    /// Construct the engine from already-loaded config data.
    ///
    /// `per_feed` pairs a `FeedId` (from storage, after upsert) with the rules
    /// for that feed. Rules are sorted by priority before building so that
    /// the compiled `Vec<Box<dyn Rule>>` is already in evaluation order.
    pub fn build(
        per_feed: &[(FeedId, &[RuleConfig])],
        global: &[RuleConfig],
        default: Action,
    ) -> Result<Self, RuleError> {
        let mut per_feed_map = HashMap::new();

        for (feed_id, rules) in per_feed {
            let mut sorted: Vec<&RuleConfig> = rules.iter().collect();
            sorted.sort_by_key(|r| r.priority);
            let built = sorted
                .iter()
                .map(|r| build_rule(r))
                .collect::<Result<_, _>>()?;
            per_feed_map.insert(*feed_id, built);
        }

        let mut global_sorted: Vec<&RuleConfig> = global.iter().collect();
        global_sorted.sort_by_key(|r| r.priority);
        let global_built = global_sorted
            .iter()
            .map(|r| build_rule(r))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            per_feed: per_feed_map,
            global: global_built,
            default,
        })
    }

    /// Evaluate rules for one episode and return the winning action.
    ///
    /// Order: per-feed rules → global rules → default.
    pub fn evaluate(&self, episode: &Episode, feed: &Feed) -> Action {
        let per_feed_rules = self
            .per_feed
            .get(&feed.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        for rule in per_feed_rules.iter().chain(self.global.iter()) {
            if let Some(action) = rule.evaluate(episode, feed) {
                tracing::debug!(
                    episode_id = %episode.id,
                    rule = rule.name(),
                    action = %action,
                    "rule matched"
                );
                return action;
            }
        }

        tracing::debug!(
            episode_id = %episode.id,
            action = %self.default,
            "no rule matched, using default"
        );
        self.default
    }
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use duralumin_core::{EpisodeId, EpisodeState, FeedId};
    use url::Url;

    fn make_feed(id: i64) -> Feed {
        Feed {
            id: FeedId(id),
            url: Url::parse("https://example.com/rss").unwrap(),
            slug: "test".into(),
            title: None,
            last_fetched_at: None,
            etag: None,
            last_modified: None,
            enabled: true,
            image_url: None,
        }
    }

    fn make_episode(title: &str, duration_secs: Option<u64>, size: Option<u64>) -> Episode {
        Episode {
            id: EpisodeId::from_parts("https://example.com/rss", title),
            feed_id: FeedId(1),
            guid: title.into(),
            title: title.into(),
            description: None,
            pub_date: Utc.with_ymd_and_hms(2024, 3, 15, 0, 0, 0).unwrap(),
            duration_secs,
            enclosure_url: Url::parse("https://example.com/ep.mp3").unwrap(),
            enclosure_size: size,
            enclosure_mime: Some("audio/mpeg".into()),
            state: EpisodeState::Discovered,
            image_url: None,
        }
    }

    #[test]
    fn title_regex_matches() {
        let rule = TitleRegexRule {
            regex: Regex::new("^Episode \\d+").unwrap(),
            action: Action::Download,
            name: "test".into(),
        };
        let feed = make_feed(1);
        let ep = make_episode("Episode 42: Rome", None, None);
        assert_eq!(rule.evaluate(&ep, &feed), Some(Action::Download));
    }

    #[test]
    fn title_regex_no_match() {
        let rule = TitleRegexRule {
            regex: Regex::new("^Episode \\d+").unwrap(),
            action: Action::Download,
            name: "test".into(),
        };
        let feed = make_feed(1);
        let ep = make_episode("Bonus: Q&A", None, None);
        assert_eq!(rule.evaluate(&ep, &feed), None);
    }

    #[test]
    fn duration_max_skips_none() {
        let rule = DurationMaxRule {
            max: Duration::from_secs(300),
            action: Action::Skip,
            name: "test".into(),
        };
        let feed = make_feed(1);
        let ep = make_episode("No Duration", None, None);
        assert_eq!(rule.evaluate(&ep, &feed), None); // None → no match
    }

    #[test]
    fn duration_max_matches_short() {
        let rule = DurationMaxRule {
            max: Duration::from_secs(300),
            action: Action::Skip,
            name: "test".into(),
        };
        let feed = make_feed(1);
        let ep = make_episode("Short", Some(120), None);
        assert_eq!(rule.evaluate(&ep, &feed), Some(Action::Skip));
    }

    #[test]
    fn always_rule_matches() {
        let rule = AlwaysRule {
            action: Action::Download,
            name: "catch-all".into(),
        };
        let feed = make_feed(1);
        let ep = make_episode("Anything", None, None);
        assert_eq!(rule.evaluate(&ep, &feed), Some(Action::Download));
    }

    #[test]
    fn engine_per_feed_before_global() {
        use config::{RuleConfig, RuleKind};

        let per_feed_rules = vec![RuleConfig {
            name: "feed-skip".into(),
            priority: 0,
            match_: RuleKind::Always,
            action: Action::Skip,
        }];
        let global_rules = vec![RuleConfig {
            name: "global-download".into(),
            priority: 0,
            match_: RuleKind::Always,
            action: Action::Download,
        }];

        let engine =
            RuleEngine::build(&[(FeedId(1), &per_feed_rules)], &global_rules, Action::Skip)
                .unwrap();

        let feed = make_feed(1);
        let ep = make_episode("Anything", None, None);
        // Per-feed rule fires first and returns Skip
        assert_eq!(engine.evaluate(&ep, &feed), Action::Skip);
    }

    #[test]
    fn engine_falls_back_to_default() {
        let engine = RuleEngine::build(&[], &[], Action::Download).unwrap();
        let feed = make_feed(99);
        let ep = make_episode("Anything", None, None);
        assert_eq!(engine.evaluate(&ep, &feed), Action::Download);
    }
}
