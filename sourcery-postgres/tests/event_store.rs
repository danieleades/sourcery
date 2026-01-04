//! Integration tests for the `PostgreSQL` event store.
//!
//! These tests require Docker to be running and will spin up a `PostgreSQL`
//! container using testcontainers.

use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use sourcery_core::store::{EventFilter, EventStore, JsonCodec, PersistableEvent};
use sourcery_postgres::Store;
use sqlx::PgPool;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

/// Simple metadata type for tests.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct TestMetadata {
    user_id: String,
}

/// Test helper to set up a `PostgreSQL` container and connection pool.
struct TestDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let container = Postgres::default().start().await.unwrap();
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

fn test_event(kind: &str, data: &str, user_id: &str) -> PersistableEvent<TestMetadata> {
    PersistableEvent {
        kind: kind.to_string(),
        data: data.as_bytes().to_vec(),
        metadata: TestMetadata {
            user_id: user_id.to_string(),
        },
    }
}

#[tokio::test]
async fn migrate_creates_event_tables() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);

    store.migrate().await.unwrap();

    // Verify the tables exist
    let streams: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM es_streams")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM es_events")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    assert_eq!(streams.0, 0);
    assert_eq!(events.0, 0);
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);

    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}

#[tokio::test]
async fn stream_version_returns_none_for_new_stream() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let version = store.stream_version("test.aggregate", &id).await.unwrap();

    assert!(version.is_none());
}

#[tokio::test]
async fn append_expecting_new_creates_stream() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let events = NonEmpty::singleton(test_event("test.event", "data", "user1"));

    let result = store
        .append_expecting_new("test.aggregate", &id, events)
        .await
        .unwrap();

    assert!(result.last_position > 0);

    // Stream version should now be set
    let version = store.stream_version("test.aggregate", &id).await.unwrap();
    assert_eq!(version, Some(result.last_position));
}

#[tokio::test]
async fn append_expecting_new_fails_for_existing_stream() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let events = NonEmpty::singleton(test_event("test.event", "data", "user1"));

    // First append succeeds
    store
        .append_expecting_new("test.aggregate", &id, events.clone())
        .await
        .unwrap();

    // Second append should fail
    let result = store
        .append_expecting_new("test.aggregate", &id, events)
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn append_with_expected_version_succeeds() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Create stream
    let first = store
        .append_expecting_new(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("test.event", "first", "user1")),
        )
        .await
        .unwrap();

    // Append with correct expected version
    let second = store
        .append(
            "test.aggregate",
            &id,
            Some(first.last_position),
            NonEmpty::singleton(test_event("test.event", "second", "user1")),
        )
        .await
        .unwrap();

    assert!(second.last_position > first.last_position);
}

#[tokio::test]
async fn append_with_wrong_expected_version_fails() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Create stream
    store
        .append_expecting_new(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("test.event", "first", "user1")),
        )
        .await
        .unwrap();

    // Append with wrong expected version
    let result = store
        .append(
            "test.aggregate",
            &id,
            Some(999), // Wrong version
            NonEmpty::singleton(test_event("test.event", "second", "user1")),
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn load_events_returns_stored_events() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    store
        .append_expecting_new(
            "test.aggregate",
            &id,
            NonEmpty::new(test_event("test.created", "created data", "user1")),
        )
        .await
        .unwrap();

    let filters = vec![EventFilter::for_aggregate(
        "test.created",
        "test.aggregate",
        id,
    )];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "test.created");
    assert_eq!(events[0].aggregate_id, id);
    assert_eq!(events[0].data, b"created data");
}

#[tokio::test]
async fn load_events_with_after_position_filter() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Append first event
    let first = store
        .append_expecting_new(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("test.event", "first", "user1")),
        )
        .await
        .unwrap();

    // Append second event
    store
        .append(
            "test.aggregate",
            &id,
            Some(first.last_position),
            NonEmpty::singleton(test_event("test.event", "second", "user1")),
        )
        .await
        .unwrap();

    // Load only events after the first position
    let filters = vec![
        EventFilter::for_aggregate("test.event", "test.aggregate", id)
            .after(first.last_position),
    ];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, b"second");
}

#[tokio::test]
async fn append_multiple_events_atomically() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    let events = NonEmpty::from_vec(vec![
        test_event("test.event", "first", "user1"),
        test_event("test.event", "second", "user1"),
        test_event("test.event", "third", "user1"),
    ])
    .unwrap();

    let result = store
        .append_expecting_new("test.aggregate", &id, events)
        .await
        .unwrap();

    // Load all events
    let filters = vec![EventFilter::for_aggregate(
        "test.event",
        "test.aggregate",
        id,
    )];
    let loaded = store.load_events(&filters).await.unwrap();

    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[0].data, b"first");
    assert_eq!(loaded[1].data, b"second");
    assert_eq!(loaded[2].data, b"third");

    // Positions should be monotonically increasing
    assert!(loaded[0].position < loaded[1].position);
    assert!(loaded[1].position < loaded[2].position);
    assert_eq!(loaded[2].position, result.last_position);
}

#[tokio::test]
async fn events_are_ordered_by_position() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    // Create events in different streams
    store
        .append_expecting_new(
            "test.aggregate",
            &id1,
            NonEmpty::singleton(test_event("test.event", "stream1-first", "user1")),
        )
        .await
        .unwrap();

    store
        .append_expecting_new(
            "test.aggregate",
            &id2,
            NonEmpty::singleton(test_event("test.event", "stream2-first", "user2")),
        )
        .await
        .unwrap();

    // Load all events (across both streams)
    let filters = vec![EventFilter {
        event_kind: "test.event".to_string(),
        aggregate_kind: Some("test.aggregate".to_string()),
        aggregate_id: None,
        after_position: None,
    }];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 2);
    // Events should be ordered by position
    assert!(events[0].position < events[1].position);
}

#[tokio::test]
async fn metadata_is_preserved() {
    let db = TestDb::new().await;
    let store: Store<JsonCodec, TestMetadata> = Store::new(db.pool.clone(), JsonCodec);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    store
        .append_expecting_new(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("test.event", "data", "special-user-123")),
        )
        .await
        .unwrap();

    let filters = vec![EventFilter::for_aggregate(
        "test.event",
        "test.aggregate",
        id,
    )];
    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events[0].metadata.user_id, "special-user-123");
}
