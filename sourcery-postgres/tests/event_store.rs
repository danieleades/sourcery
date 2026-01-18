//! Integration tests for the `PostgreSQL` event store.
//!
//! These tests require Docker to be running and will spin up a `PostgreSQL`
//! container using testcontainers.

use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use sourcery_core::{
    event::DomainEvent,
    store::{EventFilter, EventStore},
};
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

/// Test event type
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct TestEvent {
    data: String,
}

impl DomainEvent for TestEvent {
    const KIND: &'static str = "test-event";
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

/// Helper function to create a test event
fn test_event(data: &str) -> TestEvent {
    TestEvent {
        data: data.to_string(),
    }
}

/// Helper function to create test metadata
fn test_metadata(user_id: &str) -> TestMetadata {
    TestMetadata {
        user_id: user_id.to_string(),
    }
}

#[tokio::test]
async fn migrate_creates_event_tables() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());

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
    let store: Store<TestMetadata> = Store::new(db.pool.clone());

    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}

#[tokio::test]
async fn stream_version_returns_none_for_new_stream() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let version = store.stream_version("test.aggregate", &id).await.unwrap();

    assert!(version.is_none());
}

#[tokio::test]
async fn commit_events_optimistic_new_creates_stream() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let events = NonEmpty::singleton(test_event("data"));
    let metadata = test_metadata("user1");

    let result = store
        .commit_events_optimistic("test.aggregate", &id, None, events, &metadata)
        .await
        .unwrap();

    assert!(result.last_position > 0);

    // Stream version should now be set
    let version = store.stream_version("test.aggregate", &id).await.unwrap();
    assert_eq!(version, Some(result.last_position));
}

#[tokio::test]
async fn commit_events_optimistic_new_fails_for_existing_stream() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // First commit succeeds
    store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("data")),
            &metadata,
        )
        .await
        .unwrap();

    // Second commit expecting new stream should fail
    let result = store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("data")),
            &metadata,
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn commit_events_optimistic_with_expected_version_succeeds() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // Create stream
    let first = store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("first")),
            &metadata,
        )
        .await
        .unwrap();

    // Commit with correct expected version
    let second = store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            Some(first.last_position),
            NonEmpty::singleton(test_event("second")),
            &metadata,
        )
        .await
        .unwrap();

    assert!(second.last_position > first.last_position);
}

#[tokio::test]
async fn commit_events_optimistic_with_wrong_expected_version_fails() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // Create stream
    store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("first")),
            &metadata,
        )
        .await
        .unwrap();

    // Commit with wrong expected version
    let result = store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            Some(999), // Wrong version
            NonEmpty::singleton(test_event("second")),
            &metadata,
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn load_events_returns_stored_events() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("created data")),
            &metadata,
        )
        .await
        .unwrap();

    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), "test-event");
    assert_eq!(events[0].aggregate_id(), &id);
}

#[tokio::test]
async fn load_events_with_after_position_filter() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // Commit first event
    let first = store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("first")),
            &metadata,
        )
        .await
        .unwrap();

    // Commit second event
    store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            Some(first.last_position),
            NonEmpty::singleton(test_event("second")),
            &metadata,
        )
        .await
        .unwrap();

    // Load only events after the first position
    let filters = vec![
        EventFilter::for_aggregate("test-event", "test.aggregate", id).after(first.last_position),
    ];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn commit_multiple_events_atomically() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    let events = NonEmpty::from_vec(vec![
        test_event("first"),
        test_event("second"),
        test_event("third"),
    ])
    .unwrap();

    let result = store
        .commit_events_optimistic("test.aggregate", &id, None, events, &metadata)
        .await
        .unwrap();

    // Load all events
    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];
    let loaded = store.load_events(&filters).await.unwrap();

    assert_eq!(loaded.len(), 3);

    // Positions should be monotonically increasing
    assert!(loaded[0].position() < loaded[1].position());
    assert!(loaded[1].position() < loaded[2].position());
    assert_eq!(loaded[2].position(), result.last_position);
}

#[tokio::test]
async fn events_are_ordered_by_position() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // Create events in different streams
    store
        .commit_events_optimistic(
            "test.aggregate",
            &id1,
            None,
            NonEmpty::singleton(test_event("stream1-first")),
            &metadata,
        )
        .await
        .unwrap();

    store
        .commit_events_optimistic(
            "test.aggregate",
            &id2,
            None,
            NonEmpty::singleton(test_event("stream2-first")),
            &test_metadata("user2"),
        )
        .await
        .unwrap();

    // Load all events (across both streams)
    let filters = vec![EventFilter {
        event_kind: "test-event".to_string(),
        aggregate_kind: Some("test.aggregate".to_string()),
        aggregate_id: None,
        after_position: None,
    }];

    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events.len(), 2);
    // Events should be ordered by position
    assert!(events[0].position() < events[1].position());
}

#[tokio::test]
async fn metadata_is_preserved() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("special-user-123");

    store
        .commit_events_optimistic(
            "test.aggregate",
            &id,
            None,
            NonEmpty::singleton(test_event("data")),
            &metadata,
        )
        .await
        .unwrap();

    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];
    let events = store.load_events(&filters).await.unwrap();

    assert_eq!(events[0].metadata().user_id, "special-user-123");
}

// =============================================================================
// Tests for commit_events (unchecked / last-writer-wins)
// =============================================================================

#[tokio::test]
async fn commit_events_creates_stream() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let events = NonEmpty::singleton(test_event("data"));
    let metadata = test_metadata("user1");

    let result = store
        .commit_events("test.aggregate", &id, events, &metadata)
        .await
        .unwrap();

    assert!(result.last_position > 0);

    // Stream version should now be set
    let version = store.stream_version("test.aggregate", &id).await.unwrap();
    assert_eq!(version, Some(result.last_position));
}

#[tokio::test]
async fn commit_events_appends_to_existing_stream() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    // First commit
    let first = store
        .commit_events(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("first")),
            &metadata,
        )
        .await
        .unwrap();

    // Second commit (no version check - should succeed)
    let second = store
        .commit_events(
            "test.aggregate",
            &id,
            NonEmpty::singleton(test_event("second")),
            &metadata,
        )
        .await
        .unwrap();

    assert!(second.last_position > first.last_position);

    // Both events should be in the stream
    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];
    let events = store.load_events(&filters).await.unwrap();
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn commit_events_multiple_events_atomically() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let metadata = test_metadata("user1");

    let events = NonEmpty::from_vec(vec![
        test_event("first"),
        test_event("second"),
        test_event("third"),
    ])
    .unwrap();

    let result = store
        .commit_events("test.aggregate", &id, events, &metadata)
        .await
        .unwrap();

    // Load all events
    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];
    let loaded = store.load_events(&filters).await.unwrap();

    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[2].position(), result.last_position);
}

// =============================================================================
// Tests for decode_event
// =============================================================================

#[tokio::test]
async fn decode_event_deserializes_stored_event() {
    let db = TestDb::new().await;
    let store: Store<TestMetadata> = Store::new(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let original_event = test_event("test data for decode");
    let metadata = test_metadata("user1");

    store
        .commit_events(
            "test.aggregate",
            &id,
            NonEmpty::singleton(original_event.clone()),
            &metadata,
        )
        .await
        .unwrap();

    // Load and decode the event
    let filters = vec![EventFilter::for_aggregate(
        "test-event",
        "test.aggregate",
        id,
    )];
    let stored = store.load_events(&filters).await.unwrap();

    let decoded: TestEvent = store.decode_event(&stored[0]).unwrap();
    assert_eq!(decoded, original_event);
}
