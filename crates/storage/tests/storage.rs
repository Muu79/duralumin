use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use duralumin_core::Action;
use duralumin_core::{Episode, EpisodeId, EpisodeState, Feed, FeedId};
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
        episode_no: 0,
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
    db.upsert_feed(&make_feed("https://a.com/rss", "feed-a"))
        .await
        .unwrap();
    db.upsert_feed(&make_feed("https://b.com/rss", "feed-b"))
        .await
        .unwrap();
    let feeds = db.list_feeds().await.unwrap();
    assert_eq!(feeds.len(), 2);
}

#[tokio::test]
async fn get_feed_by_slug() {
    let db = open_mem_db().await;
    db.upsert_feed(&make_feed("https://example.com/rss", "slug-test"))
        .await
        .unwrap();
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

    let fetched = db
        .get_episode(&ep.id)
        .await
        .unwrap()
        .expect("episode should exist");
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
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "rt"))
        .await
        .unwrap();
    let ep = make_episode(feed_id, "rt-ep");
    db.upsert_episode(&ep).await.unwrap();

    let states: Vec<EpisodeState> = vec![
        EpisodeState::Matched(Action::Download),
        EpisodeState::Downloading {
            attempt: 1,
            started_at: Utc::now(),
        },
        EpisodeState::Failed {
            last_error: "timeout".into(),
            attempts: 2,
        },
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
async fn enqueue_is_idempotent() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "queue"))
        .await
        .unwrap();
    let ep = make_episode(feed_id, "q1");
    db.upsert_episode(&ep).await.unwrap();

    let first = db.enqueue(&ep.id, Action::Download).await.unwrap();
    assert!(first, "first enqueue should return true");

    let second = db.enqueue(&ep.id, Action::Download).await.unwrap();
    assert!(!second, "duplicate enqueue should return false");

    let queue = db.get_queue().await.unwrap();
    assert_eq!(queue.len(), 1);
}

#[tokio::test]
async fn dequeue_removes_entry() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "dequeue"))
        .await
        .unwrap();
    let ep = make_episode(feed_id, "dq1");
    db.upsert_episode(&ep).await.unwrap();
    db.enqueue(&ep.id, Action::Download).await.unwrap();

    db.dequeue(&ep.id).await.unwrap();
    let queue = db.get_queue().await.unwrap();
    assert!(queue.is_empty());
}

#[tokio::test]
async fn dequeue_is_noop_when_not_queued() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "noop"))
        .await
        .unwrap();
    let ep = make_episode(feed_id, "nq1");
    db.upsert_episode(&ep).await.unwrap();

    // Should not error even though ep was never enqueued.
    db.dequeue(&ep.id).await.unwrap();
}

#[tokio::test]
async fn get_queue_returns_enqueued_episodes_in_order() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "order"))
        .await
        .unwrap();

    let ep1 = make_episode(feed_id, "o1");
    let ep2 = make_episode(feed_id, "o2");
    let ep3 = make_episode(feed_id, "o3");

    db.upsert_episode(&ep1).await.unwrap();
    db.upsert_episode(&ep2).await.unwrap();
    db.upsert_episode(&ep3).await.unwrap();

    // Enqueue only ep1 and ep3
    db.enqueue(&ep1.id, Action::Download).await.unwrap();
    db.enqueue(&ep3.id, Action::Dynamic).await.unwrap();

    let queue = db.get_queue().await.unwrap();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0].id, ep1.id);
    assert_eq!(queue[1].id, ep3.id);
}

#[tokio::test]
async fn cascade_delete_clears_queue_entry() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "cascade"))
        .await
        .unwrap();
    let ep = make_episode(feed_id, "cas1");
    db.upsert_episode(&ep).await.unwrap();
    db.enqueue(&ep.id, Action::Download).await.unwrap();

    db.delete_episode(&ep.id).await.unwrap();
    let queue = db.get_queue().await.unwrap();
    assert!(
        queue.is_empty(),
        "queue entry should cascade-delete with episode"
    );
}

// ---- Episode list filter ---------------------------------------------------

#[tokio::test]
async fn list_episodes_with_state_filter() {
    let db = open_mem_db().await;
    let feed_id = db
        .upsert_feed(&make_feed("https://example.com/rss", "filter"))
        .await
        .unwrap();

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
