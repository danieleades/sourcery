//! Integration tests for subscription functionality.

#![cfg(feature = "test-util")]

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, Create, DomainEvent, Handle, HandleCreate, Projection,
    Repository, repository::CommandError, store::inmemory,
};
use tokio_stream::StreamExt as _;

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
    create(ItemAdded),
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

impl Create<ItemAdded> for Inventory {
    fn create(_event: &ItemAdded) -> Self {
        Self { count: 1 }
    }
}

struct AddItem {
    name: String,
}

impl Handle<AddItem> for Inventory {
    type HandleError = Self::Error;

    fn handle(&self, command: &AddItem) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            ItemAdded {
                name: command.name.clone(),
            }
            .into(),
        ])
    }
}

impl HandleCreate<AddItem> for Inventory {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &AddItem) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, Projection)]
#[projection(events(ItemAdded))]
struct ItemCount {
    count: u32,
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
    let command = AddItem {
        name: name.to_string(),
    };
    let id = id.to_string();

    match repo.create::<Inventory, AddItem>(&id, &command, &()).await {
        Ok(()) => {}
        Err(CommandError::Lifecycle(_)) => {
            repo.update::<Inventory, AddItem>(&id, &command, &())
                .await
                .unwrap();
        }
        Err(err) => panic!("failed to append command: {err}"),
    }
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

    let mut subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    let first = subscription.next().await.unwrap();
    let second = subscription.next().await.unwrap();

    assert_eq!(first.count, 1);
    assert_eq!(second.count, 2);

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_receives_live_events() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let mut subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    add_item(&repo, "inv1", "cherry").await;
    let first = subscription.next().await.unwrap();
    assert_eq!(first.count, 1);

    add_item(&repo, "inv1", "date").await;
    let second = subscription.next().await.unwrap();
    assert_eq!(second.count, 2);

    subscription.stop().await.unwrap();
}

#[tokio::test]
async fn subscription_catches_up_then_receives_live() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "historical").await;

    let mut subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    let historical = subscription.next().await.unwrap();
    assert_eq!(historical.count, 1);

    add_item(&repo, "inv1", "live").await;
    let live = subscription.next().await.unwrap();
    assert_eq!(live.count, 2);

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
async fn subscription_start_returns_when_no_events() {
    let store: inmemory::Store<String, ()> = inmemory::Store::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();
    assert!(subscription.is_running());

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
    let mut sub1 = repo
        .subscribe_with_snapshots::<ItemCount, _>((), snapshots.clone())
        .start()
        .await
        .unwrap();

    assert_eq!(sub1.next().await.unwrap().count, 1);
    assert_eq!(sub1.next().await.unwrap().count, 2);
    sub1.stop().await.unwrap();

    // Add more events
    add_item(&repo, "inv1", "c").await;

    // Second subscription: should resume from snapshot
    let mut sub2 = repo
        .subscribe_with_snapshots::<ItemCount, _>((), snapshots)
        .start()
        .await
        .unwrap();

    // Should resume at snapshot and emit only the new event.
    assert_eq!(sub2.next().await.unwrap().count, 3);

    sub2.stop().await.unwrap();
}
