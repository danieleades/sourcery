#![cfg(feature = "test-util")]

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sourcery::{
    Aggregate, Apply, ApplyProjection, DomainEvent, EventKind, Handle, Projection,
    Repository, projection::ProjectionError,
    repository::OptimisticCommandError,
    snapshot::{inmemory::Store as InMemorySnapshotStore, OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore},
    store::{EventStore, inmemory},
    test::RepositoryTestExt,
};
use sourcery_core::concurrency::ConcurrencyConflict;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ValueAdded {
    amount: i32,
}

impl DomainEvent for ValueAdded {
    const KIND: &'static str = "value-added";
}

struct InvalidValueAdded;

impl EventKind for InvalidValueAdded {
    fn kind(&self) -> &'static str {
        ValueAdded::KIND
    }
}

impl serde::Serialize for InvalidValueAdded {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("not-an-object")
    }
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(ValueAdded),
    derives(Debug, PartialEq, Eq)
)]
struct Counter {
    value: i32,
}

impl Apply<ValueAdded> for Counter {
    fn apply(&mut self, event: &ValueAdded) {
        self.value += event.amount;
    }
}

struct AddValue {
    amount: i32,
}

impl Handle<AddValue> for Counter {
    fn handle(&self, command: &AddValue) -> Result<Vec<Self::Event>, Self::Error> {
        if command.amount <= 0 {
            return Err("amount must be positive".to_string());
        }
        Ok(vec![ValueAdded {
            amount: command.amount,
        }.into()])
    }
}

struct NoOp;

impl Handle<NoOp> for Counter {
    fn handle(&self, _: &NoOp) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![])
    }
}

struct RequireAtLeast {
    min: i32,
}

impl Handle<RequireAtLeast> for Counter {
    fn handle(&self, command: &RequireAtLeast) -> Result<Vec<Self::Event>, Self::Error> {
        if self.value < command.min {
            return Err("insufficient value".to_string());
        }
        Ok(vec![])
    }
}

#[derive(Debug, Default)]
struct TotalsProjection {
    totals: HashMap<String, i32>,
}

impl Projection for TotalsProjection {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    const KIND: &'static str = "totals";
}

impl ApplyProjection<ValueAdded> for TotalsProjection {
    fn apply_projection(
        &mut self,
        aggregate_id: &Self::Id,
        event: &ValueAdded,
        &(): &Self::Metadata,
    ) {
        *self.totals.entry(aggregate_id.clone()).or_insert(0) += event.amount;
    }
}

#[test]
fn concurrency_conflict_formats_expected_new_stream_hint() {
    let conflict = ConcurrencyConflict::<u64> {
        expected: None,
        actual: Some(42),
    };
    let msg = conflict.to_string();
    assert!(msg.contains("expected new stream"));
}

#[test]
fn concurrency_conflict_formats_unexpected_empty_state() {
    let conflict = ConcurrencyConflict::<u64> {
        expected: None,
        actual: None,
    };
    let msg = conflict.to_string();
    assert!(msg.contains("unexpected empty state"));
}

#[tokio::test]
async fn in_memory_snapshot_store_policy_always_saves() {
    let snapshots = InMemorySnapshotStore::<String, u64>::always();
    let id = "c1".to_string();

    let result = snapshots
        .offer_snapshot(
            Counter::KIND,
            &id,
            0,
            || -> Result<Snapshot<u64, Vec<u8>>, Infallible> {
                Ok(Snapshot {
                    position: 1,
                    data: vec![1, 2, 3],
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(result, SnapshotOffer::Stored);

    let loaded = snapshots.load::<Vec<u8>>(Counter::KIND, &id).await.unwrap();
    assert!(loaded.is_some());
}

#[tokio::test]
async fn in_memory_snapshot_store_policy_every_n_events_saves_at_threshold() {
    let snapshots = InMemorySnapshotStore::<String, u64>::every(3);
    let id = "c1".to_string();

    let result = snapshots
        .offer_snapshot(
            Counter::KIND,
            &id,
            2,
            || -> Result<Snapshot<u64, Vec<u8>>, Infallible> {
                panic!("snapshot should be declined before threshold");
            },
        )
        .await
        .unwrap();
    assert_eq!(result, SnapshotOffer::Declined);

    assert!(snapshots
        .load::<Vec<u8>>(Counter::KIND, &id)
        .await
        .unwrap()
        .is_none());

    let result = snapshots
        .offer_snapshot(
            Counter::KIND,
            &id,
            3,
            || -> Result<Snapshot<u64, Vec<u8>>, Infallible> {
                Ok(Snapshot {
                    position: 2,
                    data: vec![2],
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(result, SnapshotOffer::Stored);
    assert!(snapshots
        .load::<Vec<u8>>(Counter::KIND, &id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn in_memory_snapshot_store_policy_never_does_not_save() {
    let snapshots = InMemorySnapshotStore::<String, u64>::never();
    let id = "c1".to_string();

    let result = snapshots
        .offer_snapshot(
            Counter::KIND,
            &id,
            100,
            || -> Result<Snapshot<u64, Vec<u8>>, Infallible> {
                panic!("snapshot should be declined when policy is never");
            },
        )
        .await
        .unwrap();
    assert_eq!(result, SnapshotOffer::Declined);

    assert!(snapshots
        .load::<Vec<u8>>(Counter::KIND, &id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn projection_event_for_filters_by_aggregate() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 10 }, &())
        .await
        .unwrap();
    repo.execute_command::<Counter, AddValue>(&"c2".to_string(), &AddValue { amount: 20 }, &())
        .await
        .unwrap();

    let projection: TotalsProjection = repo
        .build_projection()
        .event_for::<Counter, ValueAdded>(&"c1".to_string())
        .load()
        .await
        .unwrap();

    assert_eq!(projection.totals.get("c1"), Some(&10));
    assert!(!projection.totals.contains_key("c2"));
}

#[tokio::test]
async fn projection_load_surfaces_deserialization_error() {
    let store = inmemory::Store::new();
    let mut repo = Repository::new(store);

    // Inject malformed JSON data that won't deserialize to ValueAdded
    repo.inject_event(Counter::KIND, &"c1".to_string(), &InvalidValueAdded, ())
        .await
        .unwrap();

    let err = repo
        .build_projection::<TotalsProjection>()
        .event::<ValueAdded>()
        .load()
        .await
        .unwrap_err();

    // Should get EventDecode error with Store variant inside
    match err {
        ProjectionError::EventDecode(_) => {
            // Test passes - we got the expected error variant
        }
        other @ ProjectionError::Store(_) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn unchecked_repository_saves_snapshot_and_exposes_snapshot_store() {
    let store = inmemory::Store::new();
    let snapshots = InMemorySnapshotStore::<String, u64>::always();
    let repo = Repository::new(store)
        .with_snapshots(snapshots)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.execute_command::<Counter, AddValue>(&id, &AddValue { amount: 5 }, &())
        .await
        .unwrap();

    let loaded = repo
        .snapshot_store()
        .load::<Counter>(Counter::KIND, &id)
        .await
        .unwrap();
    assert!(loaded.is_some());
}

#[tokio::test]
async fn unchecked_execute_command_with_no_events_does_not_persist_or_snapshot() {
    let store = inmemory::Store::new();
    let snapshots = InMemorySnapshotStore::<String, u64>::always();
    let repo = Repository::new(store)
        .with_snapshots(snapshots)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.execute_command::<Counter, NoOp>(&id, &NoOp, &())
        .await
        .unwrap();

    assert!(
        repo.event_store()
            .stream_version(Counter::KIND, &id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.snapshot_store()
            .load::<Counter>(Counter::KIND, &id)
            .await
            .unwrap()
            .is_none()
    );
}

#[derive(Debug, Error)]
#[error("snapshot load failed")]
struct SnapshotLoadError;

#[derive(Debug)]
struct FailingLoadSnapshotStore;

impl SnapshotStore<String> for FailingLoadSnapshotStore {
    type Error = SnapshotLoadError;
    type Position = u64;

    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        Err(SnapshotLoadError)
    }

    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[tokio::test]
async fn snapshot_load_failure_falls_back_to_full_replay() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store)
        .with_snapshots(FailingLoadSnapshotStore)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.execute_command::<Counter, AddValue>(&id, &AddValue { amount: 10 }, &())
        .await
        .unwrap();

    repo.execute_command::<Counter, RequireAtLeast>(&id, &RequireAtLeast { min: 5 }, &())
        .await
        .unwrap();
}

#[derive(Debug, Default)]
struct CorruptSnapshotStore;

impl SnapshotStore<String> for CorruptSnapshotStore {
    type Error = SnapshotLoadError;
    type Position = u64;

    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        Err(SnapshotLoadError)
    }

    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[tokio::test]
async fn corrupt_snapshot_data_ignores_snapshot_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store)
        .with_snapshots(CorruptSnapshotStore)
        .without_concurrency_checking();

    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 1 }, &())
        .await
        .unwrap();
}

#[tokio::test]
async fn execute_with_retry_with_zero_retries_still_attempts_once() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let attempts = repo
        .execute_with_retry::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 1 }, &(), 0)
        .await
        .unwrap();

    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn optimistic_execute_with_retry_surfaces_non_concurrency_errors() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let err = repo
        .execute_with_retry::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 0 }, &(), 3)
        .await
        .unwrap_err();

    assert!(matches!(err, OptimisticCommandError::Aggregate(_)));
}

#[derive(Debug)]
struct TrackingSnapshotStore {
    load_called: AtomicBool,
}

impl TrackingSnapshotStore {
    const fn new() -> Self {
        Self {
            load_called: AtomicBool::new(false),
        }
    }

    fn load_called(&self) -> bool {
        self.load_called.load(Ordering::Relaxed)
    }
}

impl SnapshotStore<String> for TrackingSnapshotStore {
    type Error = Infallible;
    type Position = u64;

    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        self.load_called.store(true, Ordering::Relaxed);
        Ok(None)
    }

    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[tokio::test]
async fn aggregate_builder_load_consults_snapshot_store() {
    let store = inmemory::Store::<String, ()>::new();
    let snapshots = TrackingSnapshotStore::new();
    let repo = Repository::new(store).with_snapshots(snapshots);

    let counter: Counter = repo
        .load(&"c1".to_string())
        .await
        .unwrap();

    assert_eq!(counter.value, 0);
    assert!(repo.snapshot_store().load_called());
}
