//! Integration tests for subscription functionality.

#![cfg(feature = "test-util")]

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, DomainEvent, Filters, Handle, Projection, Repository,
    Subscribable,
    store::{EventStore, inmemory},
};

// ============================================================================
// Test Domain
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ItemAdded {
    name: String,
}

impl DomainEvent for ItemAdded {
    const KIND: &'static str = "item-added";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(ItemAdded),
    derives(Debug, PartialEq, Eq)
)]
struct Inventory {
    count: u32,
}

impl Apply<ItemAdded> for Inventory {
    fn apply(&mut self, _event: &ItemAdded) {
        self.count += 1;
    }
}

struct AddItem {
    name: String,
}

impl Handle<AddItem> for Inventory {
    fn handle(&self, command: &AddItem) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            ItemAdded {
                name: command.name.clone(),
            }
            .into(),
        ])
    }
}

// ============================================================================
// Test Projection
// ============================================================================

#[derive(Debug, Default, Serialize, Deserialize, Projection)]
struct ItemCount {
    count: u32,
}

impl Subscribable for ItemCount {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self {
        Self::default()
    }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<()>,
    {
        Filters::new().event::<ItemAdded>()
    }
}

impl ApplyProjection<ItemAdded> for ItemCount {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        _event: &ItemAdded,
        &(): &Self::Metadata,
    ) {
        self.count += 1;
    }
}

// ============================================================================
// Helper
// ============================================================================

async fn add_item(repo: &Repository<inmemory::Store<String, ()>>, id: &str, name: &str) {
    repo.execute_command::<Inventory, AddItem>(
        &id.to_string(),
        &AddItem {
            name: name.to_string(),
        },
        &(),
    )
    .await
    .unwrap();
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn subscription_replays_historical_events() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "apple").await;
    add_item(&repo, "inv1", "banana").await;

    let update_count = Arc::new(AtomicU32::new(0));
    let update_count_clone = update_count.clone();

    let catchup_complete = Arc::new(AtomicBool::new(false));
    let catchup_clone = catchup_complete.clone();

    let subscription = repo
        .subscribe::<ItemCount>(())
        .on_catchup_complete(move || {
            catchup_clone.store(true, Ordering::SeqCst);
        })
        .on_update(move |projection| {
            update_count_clone.store(projection.count, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    // Give the subscription a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(catchup_complete.load(Ordering::SeqCst));
    assert_eq!(update_count.load(Ordering::SeqCst), 2);

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_receives_live_events() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let update_count = Arc::new(AtomicU32::new(0));
    let update_count_clone = update_count.clone();

    let subscription = repo
        .subscribe::<ItemCount>(())
        .on_update(move |projection| {
            update_count_clone.store(projection.count, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    add_item(&repo, "inv1", "cherry").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(update_count.load(Ordering::SeqCst), 1);

    add_item(&repo, "inv1", "date").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(update_count.load(Ordering::SeqCst), 2);

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_catches_up_then_receives_live() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "historical").await;

    let update_count = Arc::new(AtomicU32::new(0));
    let update_count_clone = update_count.clone();

    let catchup_complete = Arc::new(AtomicBool::new(false));
    let catchup_clone = catchup_complete.clone();

    let subscription = repo
        .subscribe::<ItemCount>(())
        .on_catchup_complete(move || {
            catchup_clone.store(true, Ordering::SeqCst);
        })
        .on_update(move |projection| {
            update_count_clone.store(projection.count, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(catchup_complete.load(Ordering::SeqCst));
    assert_eq!(update_count.load(Ordering::SeqCst), 1);

    add_item(&repo, "inv1", "live").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(update_count.load(Ordering::SeqCst), 2);

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_stop_shuts_down_cleanly() {
    let store: inmemory::Store<String, ()> = inmemory::Store::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    assert!(subscription.is_running());
    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn on_catchup_complete_fires_when_no_events() {
    let store: inmemory::Store<String, ()> = inmemory::Store::new();
    let repo = Repository::new(store);

    let catchup_complete = Arc::new(AtomicBool::new(false));
    let catchup_clone = catchup_complete.clone();

    let subscription = repo
        .subscribe::<ItemCount>(())
        .on_catchup_complete(move || {
            catchup_clone.store(true, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(catchup_complete.load(Ordering::SeqCst));

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_with_snapshot_resumes() {
    use sourcery::snapshot::inmemory::Store as SnapshotStore;

    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "a").await;
    add_item(&repo, "inv1", "b").await;

    // SnapshotStore keyed by () (InstanceId of ItemCount)
    let snapshots = SnapshotStore::<(), u64>::always();

    // First subscription: catches up and creates a snapshot on stop
    let update_count = Arc::new(AtomicU32::new(0));
    let update_count_clone = update_count.clone();

    let sub1 = repo
        .subscribe_with_snapshots::<ItemCount, _>((), snapshots.clone())
        .on_update(move |p| {
            update_count_clone.store(p.count, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(update_count.load(Ordering::SeqCst), 2);
    sub1.stop().await.unwrap();

    // Add more events
    add_item(&repo, "inv1", "c").await;

    // Second subscription: should resume from snapshot
    let resumed_count = Arc::new(AtomicU32::new(0));
    let resumed_count_cb = resumed_count.clone();

    let sub2 = repo
        .subscribe_with_snapshots::<ItemCount, _>((), snapshots)
        .on_update(move |p| {
            resumed_count_cb.store(p.count, Ordering::SeqCst);
        })
        .start()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Should have all 3 events total (2 from snapshot + 1 new)
    assert_eq!(resumed_count.load(Ordering::SeqCst), 3);

    sub2.stop().await.unwrap();
}
