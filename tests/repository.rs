//! Integration tests for repository functionality.

#![cfg(feature = "test-util")]

use std::{
    convert::Infallible,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sourcery::{
    Aggregate, Apply, DomainEvent, Handle, Repository,
    repository::CommandError,
    snapshot::{
        OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore,
        inmemory::Store as InMemorySnapshotStore,
    },
    store::{EventStore, inmemory},
};
use thiserror::Error;

// ============================================================================
// Test Domain: Counter
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ValueAdded {
    amount: i32,
}

impl sourcery::DomainEvent for ValueAdded {
    const KIND: &'static str = "value-added";
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
        Ok(vec![
            ValueAdded {
                amount: command.amount,
            }
            .into(),
        ])
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

// ============================================================================
// Custom Snapshot Stores for Testing
// ============================================================================

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

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn saves_snapshot_and_exposes_snapshot_store() {
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
async fn no_events_does_not_persist_or_snapshot() {
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

#[tokio::test]
async fn corrupt_snapshot_ignores_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store)
        .with_snapshots(CorruptSnapshotStore)
        .without_concurrency_checking();

    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 1 }, &())
        .await
        .unwrap();
}

#[tokio::test]
async fn retry_with_zero_retries_still_attempts_once() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let attempts = repo
        .execute_with_retry::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 1 }, &(), 0)
        .await
        .unwrap();

    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn retry_surfaces_non_concurrency_errors() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let err = repo
        .execute_with_retry::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 0 }, &(), 3)
        .await
        .unwrap_err();

    assert!(matches!(err, CommandError::Aggregate(_)));
}

#[tokio::test]
async fn load_consults_snapshot_store() {
    let store = inmemory::Store::<String, ()>::new();
    let snapshots = TrackingSnapshotStore::new();
    let repo = Repository::new(store).with_snapshots(snapshots);

    let counter: Counter = repo.load(&"c1".to_string()).await.unwrap();

    assert_eq!(counter.value, 0);
    assert!(repo.snapshot_store().load_called());
}
