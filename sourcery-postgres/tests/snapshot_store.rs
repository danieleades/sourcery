//! Integration tests for the `PostgreSQL` snapshot store.
//!
//! These tests require Docker to be running and will spin up a `PostgreSQL`
//! container using testcontainers.

use sourcery_core::snapshot::{Snapshot, SnapshotOffer, SnapshotStore};
use sourcery_postgres::snapshot::Store;
use sqlx::PgPool;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

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

#[tokio::test]
async fn migrate_creates_snapshot_table() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());

    store.migrate().await.unwrap();

    // Verify the table exists by querying it
    let result: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM es_snapshots")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    assert_eq!(result.0, 0);
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());

    // Running migrate multiple times should not error
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}

#[tokio::test]
async fn load_returns_none_for_nonexistent_snapshot() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let result = store.load("test.aggregate", &id).await.unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn offer_and_load_snapshot_roundtrip() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    let snapshot_data = b"test snapshot data".to_vec();

    // Offer a snapshot
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 10, || {
            Ok(Snapshot {
                position: 42,
                data: snapshot_data.clone(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Stored);

    // Load the snapshot back
    let loaded = store.load("test.aggregate", &id).await.unwrap().unwrap();

    assert_eq!(loaded.position, 42);
    assert_eq!(loaded.data, snapshot_data);
}

#[tokio::test]
async fn offer_updates_existing_snapshot_with_higher_position() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Store initial snapshot at position 10
    store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 10,
                data: b"first".to_vec(),
            })
        })
        .await
        .unwrap();

    // Update with higher position
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 20,
                data: b"second".to_vec(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Stored);

    // Verify the updated snapshot
    let loaded = store.load("test.aggregate", &id).await.unwrap().unwrap();
    assert_eq!(loaded.position, 20);
    assert_eq!(loaded.data, b"second".to_vec());
}

#[tokio::test]
async fn offer_declines_stale_snapshot_with_lower_position() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Store initial snapshot at position 20
    store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 20,
                data: b"newer".to_vec(),
            })
        })
        .await
        .unwrap();

    // Try to store older snapshot at position 10
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 10,
                data: b"older".to_vec(),
            })
        })
        .await
        .unwrap();

    // Should be declined due to staleness
    assert_eq!(result, SnapshotOffer::Declined);

    // Original snapshot should remain
    let loaded = store.load("test.aggregate", &id).await.unwrap().unwrap();
    assert_eq!(loaded.position, 20);
    assert_eq!(loaded.data, b"newer".to_vec());
}

#[tokio::test]
async fn offer_declines_equal_position_snapshot() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Store initial snapshot at position 15
    store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 15,
                data: b"first".to_vec(),
            })
        })
        .await
        .unwrap();

    // Try to store snapshot with same position
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 5, || {
            Ok(Snapshot {
                position: 15,
                data: b"duplicate".to_vec(),
            })
        })
        .await
        .unwrap();

    // Should be declined (not strictly greater)
    assert_eq!(result, SnapshotOffer::Declined);
}

#[tokio::test]
async fn policy_always_stores_snapshot() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Even with 0 events since last snapshot, should store
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 0, || {
            Ok(Snapshot {
                position: 1,
                data: b"data".to_vec(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Stored);
}

#[tokio::test]
async fn policy_every_n_declines_below_threshold() {
    let db = TestDb::new().await;
    let store = Store::every(db.pool.clone(), 50);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // With only 49 events, should decline
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 49, || {
            Ok(Snapshot {
                position: 1,
                data: b"data".to_vec(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Declined);
}

#[tokio::test]
async fn policy_every_n_stores_at_threshold() {
    let db = TestDb::new().await;
    let store = Store::every(db.pool.clone(), 50);
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // At exactly 50 events, should store
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 50, || {
            Ok(Snapshot {
                position: 1,
                data: b"data".to_vec(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Stored);
}

#[tokio::test]
async fn policy_never_always_declines() {
    let db = TestDb::new().await;
    let store = Store::never(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Even with many events, should decline
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 1000, || {
            Ok(Snapshot {
                position: 1,
                data: b"data".to_vec(),
            })
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Declined);
}

#[tokio::test]
async fn policy_never_can_still_load_snapshots() {
    let db = TestDb::new().await;

    // First, store a snapshot using an "always" store
    let always_store = Store::always(db.pool.clone());
    always_store.migrate().await.unwrap();

    let id = Uuid::new_v4();
    always_store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 0, || {
            Ok(Snapshot {
                position: 100,
                data: b"stored by always".to_vec(),
            })
        })
        .await
        .unwrap();

    // Now create a "never" store and verify it can load the snapshot
    let never_store = Store::never(db.pool.clone());

    let loaded = never_store
        .load("test.aggregate", &id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.position, 100);
    assert_eq!(loaded.data, b"stored by always".to_vec());
}

#[tokio::test]
async fn different_aggregate_kinds_are_isolated() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // Store snapshot for "kind.a"
    store
        .offer_snapshot::<std::convert::Infallible, _>("kind.a", &id, 0, || {
            Ok(Snapshot {
                position: 10,
                data: b"kind a data".to_vec(),
            })
        })
        .await
        .unwrap();

    // Store snapshot for "kind.b" with same id
    store
        .offer_snapshot::<std::convert::Infallible, _>("kind.b", &id, 0, || {
            Ok(Snapshot {
                position: 20,
                data: b"kind b data".to_vec(),
            })
        })
        .await
        .unwrap();

    // Verify they are separate
    let loaded_a = store.load("kind.a", &id).await.unwrap().unwrap();
    let loaded_b = store.load("kind.b", &id).await.unwrap().unwrap();

    assert_eq!(loaded_a.position, 10);
    assert_eq!(loaded_a.data, b"kind a data".to_vec());
    assert_eq!(loaded_b.position, 20);
    assert_eq!(loaded_b.data, b"kind b data".to_vec());
}

#[tokio::test]
async fn different_aggregate_ids_are_isolated() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    // Store snapshots for different IDs
    store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id1, 0, || {
            Ok(Snapshot {
                position: 10,
                data: b"id1 data".to_vec(),
            })
        })
        .await
        .unwrap();

    store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id2, 0, || {
            Ok(Snapshot {
                position: 20,
                data: b"id2 data".to_vec(),
            })
        })
        .await
        .unwrap();

    // Verify they are separate
    let loaded1 = store.load("test.aggregate", &id1).await.unwrap().unwrap();
    let loaded2 = store.load("test.aggregate", &id2).await.unwrap().unwrap();

    assert_eq!(loaded1.position, 10);
    assert_eq!(loaded1.data, b"id1 data".to_vec());
    assert_eq!(loaded2.position, 20);
    assert_eq!(loaded2.data, b"id2 data".to_vec());
}

#[tokio::test]
async fn create_snapshot_callback_not_called_when_policy_declines() {
    let db = TestDb::new().await;
    let store = Store::never(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    // The callback should never be invoked for "never" policy
    let result = store
        .offer_snapshot::<std::convert::Infallible, _>("test.aggregate", &id, 1000, || {
            panic!("create_snapshot should not be called when policy declines");
        })
        .await
        .unwrap();

    assert_eq!(result, SnapshotOffer::Declined);
}

#[derive(Debug)]
struct CreateError;
impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "create error")
    }
}
impl std::error::Error for CreateError {}

#[tokio::test]
async fn create_snapshot_error_is_propagated() {
    let db = TestDb::new().await;
    let store = Store::always(db.pool.clone());
    store.migrate().await.unwrap();

    let id = Uuid::new_v4();

    let result = store
        .offer_snapshot::<CreateError, _>("test.aggregate", &id, 0, || Err(CreateError))
        .await;

    assert!(matches!(
        result,
        Err(sourcery_core::snapshot::OfferSnapshotError::Create(
            CreateError
        ))
    ));
}
