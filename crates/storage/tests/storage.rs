use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use duralumin_core::{Action, Episode, EpisodeId, EpisodeState, Feed, FeedId};
use duralumin_storage::{Db, EpisodeFilter};
use url::Url;

async fn open_mem_db() -> Db {
    Db::open_in_memory().await.expect("open in-memory DB")
}

fn make_feed(url: &str, slug: &str) -> Feed {
    Feed {
        id: FeedId(0),
        url: Url::parse(url).unwrap(),
        slug: slug.into(),
        title: Some("Test Feed".into()),
        last_fetched_at: None,
        etag: None,
        last_modified: None,
        enabled: true,
        image_url: None,
    }
}

fn make_episode(feed_id: FeedId, guid: &str) -> Episode {
    Episode {
        id: EpisodeId::from_parts("https://example.com/rss", guid),
        feed_id,
        guid: guid.into(),
        title: format!("Episode {guid}"),
        description: Some("A test episode.".into()),
        pub_date: Utc.with_ymd_and_hms(2024, 3, 15, 0, 0, 0).unwrap(),
        duration_secs: Some(3600),
        enclosure_url: Url::parse("https://example.com/ep.mp3").unwrap(),
        enclosure_size: Some(50_000_000),
        enclosure_mime: Some("audio/mpeg".into()),
        state: EpisodeState::Discovered,
        image_url: None,
    }
}

// ---- Feed CRUD -------------------------------------------------------------

#[tokio::test]
async fn upsert_and_get_feed() {
    let db = open_mem_db().await;
    let feed = make_feed("https://example.com/rss", "my-podcast");
    let id = db.upsert_feed(&feed).await.unwrap();
    assert!(id.0 > 0);

    let fetched = db.get_feed(id).await.unwrap().expect("feed should exist");
    assert_eq!(fetched.slug, "my-podcast");
    assert_eq!(fetched.title.as_deref(), Some("Test Feed"));
}

#[tokio::test]
async fn upsert_feed_is_idempotent() {
    let db = open_mem_db().await;
    let feed = make_feed("https://example.com/rss", "my-podcast");
    let id1 = db.upsert_feed(&feed).await.unwrap();
    let id2 = db.upsert_feed(&feed).await.unwrap();
    assert_eq!(id1, id2, "same URL should return same ID");
}

#[tokio::test]
async fn list_feeds_returns_all() {
    let db = open_mem_db().await;
    db.upsert_feed(&make_feed("https://a.com/rss", "feed-a")).await.unwrap();
    db.upsert_feed(&make_feed("https://b.com/rss", "feed-b")).await.unwrap();
    let feeds = db.list_feeds().await.unwrap();
    assert_eq!(feeds.len(), 2);
}

#[tokio::test]
async fn get_feed_by_slug() {
    let db = open_mem_db().await;
    db.upsert_feed(&make_feed("https://example.com/rss", "slug-test")).await.unwrap();
    let f = db.get_feed_by_slug("slug-test").await.unwrap();
    assert!(f.is_some());
    assert!(db.get_feed_by_slug("nonexistent").await.unwrap().is_none());
}

// ---- Episode CRUD ----------------------------------------------------------

#[tokio::test]
async fn upsert_and_get_episode() {
    let db = open_mem_db().await;
    let feed = make_feed("https://example.com/rss", "ep-test");
    let feed_id = db.upsert_feed(&feed).await.unwrap();

    let mut ep = make_episode(feed_id, "ep-001");
    ep.feed_id = feed_id;
    db.upsert_episode(&ep).await.unwrap();

    let fetched = db.get_episode(&ep.id).await.unwrap().expect("episode should exist");
    assert_eq!(fetched.title, ep.title);
    assert_eq!(fetched.duration_secs, Some(3600));
    assert!(matches!(fetched.state, EpisodeState::Discovered));
}

#[tokio::test]
async fn upsert_episode_preserves_state() {
    let db = open_mem_db().await;
    let feed = make_feed("https://example.com/rss", "preserve-test");
    let feed_id = db.upsert_feed(&feed).await.unwrap();
    let mut ep = make_episode(feed_id, "ep-state");

    // Insert as Discovered
    db.upsert_episode(&ep).await.unwrap();

    // Update to Complete
    let complete = EpisodeState::Complete {
        path: PathBuf::from("/podcasts/ep.mp3"),
        downloaded_at: Utc::now(),
        sha256: "abc123".into(),
    };
    db.update_episode_state(&ep.id, &complete).await.unwrap();

    // Re-upsert (simulating feed refresh) — state should NOT regress
    ep.title = "Updated Title".into();
    db.upsert_episode(&ep).await.unwrap();

    let fetched = db.get_episode(&ep.id).await.unwrap().unwrap();
    assert!(
        matches!(fetched.state, EpisodeState::Complete { .. }),
        "state should not regress to Discovered after re-upsert"
    );
}

// ---- State round-trips -----------------------------------------------------

#[tokio::test]
async fn episode_state_json_round_trips() {
    let db = open_mem_db().await;
    let feed_id = db.upsert_feed(&make_feed("https://example.com/rss", "rt")).await.unwrap();
    let ep = make_episode(feed_id, "rt-ep");
    db.upsert_episode(&ep).await.unwrap();

    let states: Vec<EpisodeState> = vec![
        EpisodeState::Matched(Action::Download),
        EpisodeState::Downloading { attempt: 1, started_at: Utc::now() },
        EpisodeState::Failed { last_error: "timeout".into(), attempts: 2 },
        EpisodeState::Quarantined {
            reason: "http_404".into(),
            last_error: "404 Not Found".into(),
        },
        EpisodeState::Complete {
            path: PathBuf::from("/podcasts/ep.mp3"),
            downloaded_at: Utc::now(),
            sha256: "deadbeef".into(),
        },
    ];

    for state in states {
        db.update_episode_state(&ep.id, &state).await.unwrap();
        let fetched = db.get_episode(&ep.id).await.unwrap().unwrap();
        assert_eq!(fetched.state.kind_name(), state.kind_name());
    }
}

// ---- Download queue --------------------------------------------------------

#[tokio::test]
async fn download_queue_returns_matched_episodes() {
    let db = open_mem_db().await;
    let feed_id = db.upsert_feed(&make_feed("https://example.com/rss", "queue")).await.unwrap();

    let mut ep1 = make_episode(feed_id, "q1");
    let mut ep2 = make_episode(feed_id, "q2");
    let mut ep3 = make_episode(feed_id, "q3");

    db.upsert_episode(&ep1).await.unwrap();
    db.upsert_episode(&ep2).await.unwrap();
    db.upsert_episode(&ep3).await.unwrap();

    // Only ep1 is Matched(Download)
    db.update_episode_state(&ep1.id, &EpisodeState::Matched(Action::Download))
        .await
        .unwrap();
    // ep2 is Matched(Skip) — not in queue
    db.update_episode_state(&ep2.id, &EpisodeState::Matched(Action::Skip))
        .await
        .unwrap();
    // ep3 stays Discovered — not in queue

    let queue = db.download_queue(3).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, ep1.id);
}

#[tokio::test]
async fn download_queue_includes_retryable_failed() {
    let db = open_mem_db().await;
    let feed_id =
        db.upsert_feed(&make_feed("https://example.com/rss", "retry")).await.unwrap();
    let ep = make_episode(feed_id, "retry-ep");
    db.upsert_episode(&ep).await.unwrap();

    // 1 attempt failed — max_retries = 3 → still retryable
    db.update_episode_state(
        &ep.id,
        &EpisodeState::Failed { last_error: "timeout".into(), attempts: 1 },
    )
    .await
    .unwrap();

    let queue = db.download_queue(3).await.unwrap();
    assert_eq!(queue.len(), 1);

    // Bump to 3 attempts — max_retries = 3 → no longer retryable
    db.update_episode_state(
        &ep.id,
        &EpisodeState::Failed { last_error: "timeout".into(), attempts: 3 },
    )
    .await
    .unwrap();
    let queue = db.download_queue(3).await.unwrap();
    assert_eq!(queue.len(), 0);
}

// ---- Episode list filter ---------------------------------------------------

#[tokio::test]
async fn list_episodes_with_state_filter() {
    let db = open_mem_db().await;
    let feed_id =
        db.upsert_feed(&make_feed("https://example.com/rss", "filter")).await.unwrap();

    for guid in ["f1", "f2", "f3"] {
        let ep = make_episode(feed_id, guid);
        db.upsert_episode(&ep).await.unwrap();
    }

    let all = db.list_episodes(EpisodeFilter::default()).await.unwrap();
    assert_eq!(all.len(), 3);

    let filter = EpisodeFilter {
        state_kind: Some("discovered".into()),
        ..Default::default()
    };
    let discovered = db.list_episodes(filter).await.unwrap();
    assert_eq!(discovered.len(), 3);

    // Update one to Complete
    let ep_id = &all[0].id;
    db.update_episode_state(
        ep_id,
        &EpisodeState::Complete {
            path: PathBuf::from("/podcasts/f1.mp3"),
            downloaded_at: Utc::now(),
            sha256: "abc".into(),
        },
    )
    .await
    .unwrap();

    let filter = EpisodeFilter {
        state_kind: Some("complete".into()),
        ..Default::default()
    };
    let complete = db.list_episodes(filter).await.unwrap();
    assert_eq!(complete.len(), 1);
}
