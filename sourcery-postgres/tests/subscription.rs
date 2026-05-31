//! Integration tests for the `PostgreSQL` push-based subscription.
//!
//! These tests require Docker; they spin up a `PostgreSQL` container via
//! testcontainers.

use std::time::Duration;

use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use sourcery_core::{
    event::DomainEvent,
    snapshot::{Snapshot, SnapshotOffer, SnapshotStore},
    store::{EventFilter, EventStore},
    subscription::{Checkpointed, Delivery, SubscribableStore},
};
use sourcery_postgres::{
    Store,
    subscription::{CheckpointStore, Watermark},
};
use sqlx::PgPool;
use testcontainers::{ContainerAsync, ImageExt as _, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::time::timeout;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct TestMetadata {
    user_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct TestEvent {
    data: String,
}

impl DomainEvent for TestEvent {
    const KIND: &'static str = "test-event";
}

struct TestDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        // PostgreSQL 13+ is required for `pg_current_xact_id()` / `xid8`.
        let container = Postgres::default().with_tag("17").start().await.unwrap();
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let connection_string = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let pool = PgPool::connect(&connection_string).await.unwrap();
        Self {
            _container: container,
            pool,
        }
    }
}

fn metadata() -> TestMetadata {
    TestMetadata {
        user_id: "u".to_string(),
    }
}

fn event_filters() -> [EventFilter<Uuid, i64>; 1] {
    [EventFilter::for_event(TestEvent::KIND)]
}

/// Await the next matching event within 5s, skipping [`Delivery::Frontier`]
/// global-progress markers (which carry no event).
async fn next_event<S>(stream: &mut S) -> Checkpointed<Store<TestMetadata>>
where
    S: tokio_stream::Stream<Item = Result<Delivery<Store<TestMetadata>>, sourcery_postgres::Error>>
        + Unpin,
{
    loop {
        match timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("event within timeout")
            .expect("stream yielded an item")
            .expect("item is ok")
        {
            Delivery::Event(event) => return event,
            Delivery::Frontier(_) => {}
        }
    }
}

/// Poll for up to `within` for a matching event, skipping frontier markers.
/// Returns `true` if no event arrived in the window (frontiers are allowed).
async fn no_event_within<S>(stream: &mut S, within: Duration) -> bool
where
    S: tokio_stream::Stream<Item = Result<Delivery<Store<TestMetadata>>, sourcery_postgres::Error>>
        + Unpin,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => return true,
            Ok(item) => match item.expect("stream item").expect("ok item") {
                Delivery::Event(_) => return false,
                Delivery::Frontier(_) => {}
            },
        }
    }
}

/// Insert one event directly, returning `(position, xid)`. Used to construct
/// precise concurrency scenarios that `commit_events` (which commits
/// immediately) cannot express.
async fn raw_insert<'c>(
    executor: impl sqlx::PgExecutor<'c>,
    aggregate_id: Uuid,
    data: &str,
) -> (i64, i64) {
    sqlx::query_as(
        "INSERT INTO es_events (aggregate_kind, aggregate_id, event_kind, data, metadata) VALUES \
         ($1, $2, $3, $4, $5) RETURNING position, xid",
    )
    .bind("test.agg")
    .bind(aggregate_id)
    .bind(TestEvent::KIND)
    .bind(sqlx::types::Json(serde_json::json!({ "data": data })))
    .bind(sqlx::types::Json(metadata()))
    .fetch_one(executor)
    .await
    .unwrap()
}

#[tokio::test]
async fn subscription_catches_up_then_streams_live() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Two events committed before subscribing (the catch-up set).
    for data in ["a", "b"] {
        store
            .commit_events(
                "test.agg",
                &id,
                NonEmpty::singleton(TestEvent {
                    data: data.to_string(),
                }),
                &metadata(),
            )
            .await
            .unwrap();
    }

    let filters = event_filters();
    let mut stream = store.subscribe(&filters, None);

    let first = next_event(&mut stream).await;
    let second = next_event(&mut stream).await;
    assert!(first.checkpoint < second.checkpoint);

    // A live commit is delivered (NOTIFY wakes the subscriber).
    store
        .commit_events(
            "test.agg",
            &id,
            NonEmpty::singleton(TestEvent {
                data: "c".to_string(),
            }),
            &metadata(),
        )
        .await
        .unwrap();

    let third = next_event(&mut stream).await;
    assert!(second.checkpoint < third.checkpoint);
}

/// The core regression test for the sequence-gap problem.
///
/// Transaction A reserves a low `(xid, position)` and stays open while
/// transaction B commits a higher one. A naive position cursor would deliver B
/// and then skip A forever. The high-water-mark must withhold *both* until A
/// settles, then deliver them in `(xid, position)` order.
#[tokio::test]
async fn in_flight_transaction_gap_is_not_skipped() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // A acquires its xid and a low position first, then stays open.
    let mut tx_a = db.pool.begin().await.unwrap();
    let (pos_a, xid_a) = raw_insert(&mut *tx_a, id, "A").await;

    // B starts later (higher xid), writes a higher position, and commits.
    let mut tx_b = db.pool.begin().await.unwrap();
    let (pos_b, xid_b) = raw_insert(&mut *tx_b, id, "B").await;
    tx_b.commit().await.unwrap();

    assert!(xid_a < xid_b, "A must have the older transaction id");
    assert!(pos_a < pos_b, "A must have the lower position");

    let filters = event_filters();
    let mut stream = store.subscribe(&filters, None);

    // While A is in-flight, B is withheld (its xid is at/above xmin) even though
    // it is committed — no event is delivered. Frontiers may still arrive, but
    // only below A's xid (xmin is pinned by A), so they never cover B.
    assert!(
        no_event_within(&mut stream, Duration::from_millis(900)).await,
        "B must not be delivered while A is in-flight"
    );

    // Once A settles, both become deliverable, in (xid, position) order.
    tx_a.commit().await.unwrap();

    let first = next_event(&mut stream).await;
    let second = next_event(&mut stream).await;

    assert_eq!(
        first.event.position, pos_a,
        "A delivered first, not skipped"
    );
    assert_eq!(second.event.position, pos_b, "B delivered after A");
    assert!(first.checkpoint < second.checkpoint);
}

#[tokio::test]
async fn subscription_resumes_from_checkpoint() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    for data in ["a", "b", "c"] {
        store
            .commit_events(
                "test.agg",
                &id,
                NonEmpty::singleton(TestEvent {
                    data: data.to_string(),
                }),
                &metadata(),
            )
            .await
            .unwrap();
    }

    let filters = event_filters();

    // Consume the first event, remember its checkpoint, drop the stream.
    let checkpoint = {
        let mut stream = store.subscribe(&filters, None);
        next_event(&mut stream).await.checkpoint
    };

    // Resuming from that checkpoint must skip the already-seen event.
    let mut resumed = store.subscribe(&filters, Some(checkpoint));
    let next = next_event(&mut resumed).await;
    assert!(next.checkpoint > checkpoint);
}

#[tokio::test]
async fn current_checkpoint_reports_latest() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let filters = event_filters();
    assert!(store.current_checkpoint(&filters).await.unwrap().is_none());

    let id = Uuid::new_v4();
    store
        .commit_events(
            "test.agg",
            &id,
            NonEmpty::singleton(TestEvent {
                data: "a".to_string(),
            }),
            &metadata(),
        )
        .await
        .unwrap();

    assert!(store.current_checkpoint(&filters).await.unwrap().is_some());
}

#[tokio::test]
async fn checkpoint_for_position_resolves_exact_watermark() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    // No event at this position yet.
    assert!(store.checkpoint_for_position(&1).await.unwrap().is_none());

    let id = Uuid::new_v4();
    let committed = store
        .commit_events(
            "test.agg",
            &id,
            NonEmpty::singleton(TestEvent {
                data: "a".to_string(),
            }),
            &metadata(),
        )
        .await
        .unwrap();
    let position = committed.last_position;

    // The exact checkpoint carries the writing transaction's id and this
    // position, matching what a subscription delivers for the same row.
    let checkpoint = store
        .checkpoint_for_position(&position)
        .await
        .unwrap()
        .expect("checkpoint for committed position");
    assert_eq!(checkpoint.position, position);

    let delivered = {
        let mut stream = store.subscribe(&event_filters(), None);
        next_event(&mut stream).await.checkpoint
    };
    assert_eq!(checkpoint, delivered);
}

#[tokio::test]
async fn checkpoint_store_round_trip_and_staleness() {
    let db = TestDb::new().await;
    let store = CheckpointStore::always(db.pool.clone());
    store.migrate().await.unwrap();

    let instance = "dashboard".to_string();

    // Store a checkpoint at (xid 5, position 10) with projection state 42.
    let stored = store
        .offer_snapshot::<std::io::Error, i32, _>("report", &instance, 1, || {
            Ok(Snapshot {
                position: Watermark {
                    xid: 5,
                    position: 10,
                },
                data: 42,
            })
        })
        .await
        .unwrap();
    assert_eq!(stored, SnapshotOffer::Stored);

    let loaded: Option<Snapshot<Watermark, i32>> = store.load("report", &instance).await.unwrap();
    let loaded = loaded.unwrap();
    assert_eq!(
        loaded.position,
        Watermark {
            xid: 5,
            position: 10
        }
    );
    assert_eq!(loaded.data, 42);

    // A stale (lower) checkpoint must be declined, leaving the stored one intact.
    let stale = store
        .offer_snapshot::<std::io::Error, i32, _>("report", &instance, 1, || {
            Ok(Snapshot {
                position: Watermark {
                    xid: 5,
                    position: 9,
                },
                data: 7,
            })
        })
        .await
        .unwrap();
    assert_eq!(stale, SnapshotOffer::Declined);

    let after: Option<Snapshot<Watermark, i32>> = store.load("report", &instance).await.unwrap();
    assert_eq!(after.unwrap().data, 42, "stale offer must not overwrite");
}
