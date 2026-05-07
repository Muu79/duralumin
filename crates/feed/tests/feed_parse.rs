use duralumin_core::{Episode, FeedId};
use duralumin_feed::parse_bytes;
use url::Url;

fn make_feed(url: &str) -> duralumin_core::Feed {
    duralumin_core::Feed {
        id: FeedId(1),
        url: Url::parse(url).unwrap(),
        slug: "test".into(),
        title: None,
        last_fetched_at: None,
        etag: None,
        last_modified: None,
        enabled: true,
        image_url: None,
    }
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../../tests/fixtures").join(name)
}

fn parse_fixture(name: &str) -> Vec<Episode> {
    let path = fixture_path(name);
    let bytes = std::fs::read(&path).expect("fixture not found");
    let feed = make_feed("https://example.com/rss");
    parse_bytes(&bytes, &feed).expect("parse failed")
}

// ---- valid_feed.xml --------------------------------------------------------

#[test]
fn valid_feed_has_three_episodes() {
    let episodes = parse_fixture("valid_feed.xml");
    assert_eq!(episodes.len(), 3, "expected 3 episodes, got {}", episodes.len());
}

#[test]
fn valid_feed_titles_correct() {
    let episodes = parse_fixture("valid_feed.xml");
    assert!(episodes.iter().any(|e| e.title.contains("Episode 1")));
    assert!(episodes.iter().any(|e| e.title.contains("Episode 2")));
    assert!(episodes.iter().any(|e| e.title.contains("Episode 3")));
}

#[test]
fn valid_feed_episode_has_duration() {
    let episodes = parse_fixture("valid_feed.xml");
    let ep1 = episodes.iter().find(|e| e.title.contains("Episode 1")).unwrap();
    assert_eq!(ep1.duration_secs, Some(3600));
}

#[test]
fn valid_feed_episode_has_enclosure_size() {
    let episodes = parse_fixture("valid_feed.xml");
    let ep1 = episodes.iter().find(|e| e.title.contains("Episode 1")).unwrap();
    assert_eq!(ep1.enclosure_size, Some(50_000_000));
}

#[test]
fn valid_feed_episode_mime_type() {
    let episodes = parse_fixture("valid_feed.xml");
    for ep in &episodes {
        assert_eq!(ep.enclosure_mime.as_deref(), Some("audio/mpeg"));
    }
}

// ---- no_guid.xml -----------------------------------------------------------

#[test]
fn no_guid_episode_is_still_parsed() {
    // The entry has no <guid>; feed-rs assigns one internally.
    // The important thing is the entry parses successfully.
    let episodes = parse_fixture("no_guid.xml");
    assert_eq!(episodes.len(), 1, "should parse 1 episode even without a guid");
}

#[test]
fn no_guid_episode_has_correct_title() {
    let episodes = parse_fixture("no_guid.xml");
    assert!(episodes[0].title.contains("Without GUID"));
}

// ---- no_enclosure.xml ------------------------------------------------------

#[test]
fn no_enclosure_entry_is_skipped() {
    let episodes = parse_fixture("no_enclosure.xml");
    assert_eq!(episodes.len(), 1, "only the entry with an enclosure should survive");
    assert!(episodes[0].title.contains("Good Episode"));
}

// ---- atom_feed.xml ---------------------------------------------------------

#[test]
fn atom_feed_parses_two_episodes() {
    let episodes = parse_fixture("atom_feed.xml");
    assert_eq!(episodes.len(), 2, "expected 2 atom episodes, got {}", episodes.len());
}

#[test]
fn atom_episode_titles() {
    let episodes = parse_fixture("atom_feed.xml");
    assert!(episodes.iter().any(|e| e.title.contains("Atom Episode 1")));
    assert!(episodes.iter().any(|e| e.title.contains("Atom Episode 2")));
}

#[test]
fn atom_episode_has_pub_date() {
    let episodes = parse_fixture("atom_feed.xml");
    for ep in &episodes {
        assert!(ep.pub_date.timestamp() > 0, "pub_date should be set");
    }
}

// ---- Cross-fixture properties ----------------------------------------------

#[test]
fn episode_ids_are_deterministic() {
    let eps1 = parse_fixture("valid_feed.xml");
    let eps2 = parse_fixture("valid_feed.xml");
    for (a, b) in eps1.iter().zip(eps2.iter()) {
        assert_eq!(a.id, b.id);
    }
}

#[test]
fn episode_ids_are_unique_within_feed() {
    let episodes = parse_fixture("valid_feed.xml");
    let ids: std::collections::HashSet<_> = episodes.iter().map(|e| &e.id).collect();
    assert_eq!(ids.len(), episodes.len(), "episode IDs should be unique");
}
