use std::time::Duration;

use chrono::{TimeZone, Utc};
use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, Feed, FeedId};
use duralumin_rules::RuleEngine;
use duralumin_rules::config::{RuleConfig, RuleKind};
use url::Url;

fn feed(id: i64) -> Feed {
    Feed {
        id: FeedId(id),
        url: Url::parse("https://example.com/rss").unwrap(),
        slug: "test".into(),
        title: Some("Test Podcast".into()),
        last_fetched_at: None,
        etag: None,
        last_modified: None,
        enabled: true,
        image_url: None,
    }
}

fn episode(title: &str, duration_secs: Option<u64>, size: Option<u64>) -> Episode {
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

fn rule(name: &str, kind: RuleKind, action: Action, priority: i32) -> RuleConfig {
    RuleConfig {
        name: name.into(),
        priority,
        match_: kind,
        action,
    }
}

// ---- title_regex -----------------------------------------------------------

#[test]
fn title_regex_download_match() {
    let rules = vec![rule(
        "numbered",
        RuleKind::TitleRegex {
            pattern: r"^Episode \d+".into(),
        },
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[(FeedId(1), &rules)], &[], Action::Skip).unwrap();
    let ep = episode("Episode 42: Deep Dive", None, None);
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Download);
}

#[test]
fn title_regex_no_match_falls_through_to_default() {
    let rules = vec![rule(
        "numbered",
        RuleKind::TitleRegex {
            pattern: r"^Episode \d+".into(),
        },
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[(FeedId(1), &rules)], &[], Action::Skip).unwrap();
    let ep = episode("Bonus: Q&A", None, None);
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Skip);
}

// ---- duration rules --------------------------------------------------------

#[test]
fn duration_min_skips_short_episodes() {
    let rules = vec![rule(
        "long-only",
        RuleKind::DurationMin {
            value: Duration::from_secs(3600),
        },
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[], &rules, Action::Skip).unwrap();
    let short = episode("Short", Some(600), None);
    let long = episode("Long", Some(7200), None);
    assert_eq!(engine.evaluate(&short, &feed(1)), Action::Skip);
    assert_eq!(engine.evaluate(&long, &feed(1)), Action::Download);
}

#[test]
fn duration_rule_skips_episode_with_no_duration() {
    let rules = vec![rule(
        "min",
        RuleKind::DurationMin {
            value: Duration::from_secs(60),
        },
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[], &rules, Action::Skip).unwrap();
    let ep = episode("No Duration", None, None);
    // None duration → rule does not match → falls to default Skip
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Skip);
}

#[test]
fn duration_max_quarantines_long_episodes() {
    let rules = vec![rule(
        "short-only",
        RuleKind::DurationMax {
            value: Duration::from_secs(300),
        },
        Action::Quarantine,
        0,
    )];
    let engine = RuleEngine::build(&[], &rules, Action::Download).unwrap();
    let long = episode("Long", Some(3600), None);
    assert_eq!(engine.evaluate(&long, &feed(1)), Action::Download);
    let short = episode("Short", Some(120), None);
    assert_eq!(engine.evaluate(&short, &feed(1)), Action::Quarantine);
}

// ---- priority ordering -----------------------------------------------------

#[test]
fn lower_priority_wins() {
    let rules = vec![
        rule("catch-all", RuleKind::Always, Action::Download, 100),
        rule("block", RuleKind::Always, Action::Skip, 0),
    ];
    let engine = RuleEngine::build(&[(FeedId(1), &rules)], &[], Action::Download).unwrap();
    let ep = episode("Anything", None, None);
    // priority 0 fires first → Skip
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Skip);
}

// ---- per-feed before global ------------------------------------------------

#[test]
fn per_feed_rule_takes_precedence_over_global() {
    let per_feed = vec![rule("feed-skip", RuleKind::Always, Action::Skip, 0)];
    let global = vec![rule(
        "global-download",
        RuleKind::Always,
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[(FeedId(1), &per_feed)], &global, Action::Skip).unwrap();
    let ep = episode("Anything", None, None);
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Skip);
}

#[test]
fn global_rule_applies_to_unmatched_feed() {
    let global = vec![rule(
        "global-download",
        RuleKind::Always,
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[], &global, Action::Skip).unwrap();
    // feed(2) has no per-feed rules → global fires
    let ep = episode("Anything", None, None);
    assert_eq!(engine.evaluate(&ep, &feed(2)), Action::Download);
}

// ---- published_after / before ----------------------------------------------

#[test]
fn published_after_filters_old_episodes() {
    let cutoff = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let rules = vec![rule(
        "recent",
        RuleKind::PublishedAfter { date: cutoff },
        Action::Download,
        0,
    )];
    let engine = RuleEngine::build(&[], &rules, Action::Skip).unwrap();

    let old = {
        let mut e = episode("Old", None, None);
        e.pub_date = Utc.with_ymd_and_hms(2023, 6, 1, 0, 0, 0).unwrap();
        e
    };
    let new = episode("New", None, None); // 2024-03-15 > cutoff
    assert_eq!(engine.evaluate(&old, &feed(1)), Action::Skip);
    assert_eq!(engine.evaluate(&new, &feed(1)), Action::Download);
}

// ---- always rule -----------------------------------------------------------

#[test]
fn always_rule_as_catch_all() {
    let global = vec![rule("catch-all", RuleKind::Always, Action::Download, 999)];
    let engine = RuleEngine::build(&[], &global, Action::Skip).unwrap();
    let ep = episode("Whatever", Some(100), Some(1000));
    assert_eq!(engine.evaluate(&ep, &feed(99)), Action::Download);
}

// ---- invalid regex error ---------------------------------------------------

#[test]
fn bad_regex_returns_error() {
    let rules = vec![rule(
        "bad",
        RuleKind::TitleRegex {
            pattern: "[invalid".into(),
        },
        Action::Download,
        0,
    )];
    assert!(RuleEngine::build(&[(FeedId(1), &rules)], &[], Action::Skip).is_err());
}

// ---- empty engine ----------------------------------------------------------

#[test]
fn empty_engine_uses_default() {
    let engine = RuleEngine::build(&[], &[], Action::Archive).unwrap();
    let ep = episode("Anything", None, None);
    assert_eq!(engine.evaluate(&ep, &feed(1)), Action::Archive);
}
