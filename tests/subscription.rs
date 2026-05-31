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

/// A command that intentionally produces no events.
struct AddNothing;

impl Handle<AddNothing> for Inventory {
    type HandleError = Self::Error;

    fn handle(&self, _command: &AddNothing) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(Vec::new())
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

// ============================================================================
// Read-your-writes: consistency tokens + Subscription::wait_for
// ============================================================================
//
// A separate aggregate whose event `ItemCount` does NOT subscribe to. Writes
// here advance the global log without producing any event the projection's
// filter matches — the case that would stall a naive `wait_for` and which the
// frontier mechanism must handle.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NoteRecorded {
    text: String,
}

impl DomainEvent for NoteRecorded {
    const KIND: &'static str = "note-recorded";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(NoteRecorded),
    create(NoteRecorded),
    derives(Debug, PartialEq, Eq)
)]
struct Notebook {
    notes: u32,
}

impl Apply<NoteRecorded> for Notebook {
    fn apply(&mut self, _event: &NoteRecorded) {
        self.notes += 1;
    }
}

impl Create<NoteRecorded> for Notebook {
    fn create(_event: &NoteRecorded) -> Self {
        Self { notes: 0 }
    }
}

#[derive(Debug)]
struct RecordNote {
    text: String,
}

impl HandleCreate<RecordNote> for Notebook {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &RecordNote) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            NoteRecorded {
                text: command.text.clone(),
            }
            .into(),
        ])
    }
}

async fn record_note(repo: &Repository<inmemory::Store<String, ()>>, id: &str, text: &str) {
    let id = id.to_string();
    let command = RecordNote {
        text: text.to_string(),
    };
    repo.create::<Notebook, RecordNote>(&id, &command, &())
        .await
        .expect("record note");
}

/// After a matching write, the token resolves once the subscription has applied
/// it — and the projection then reflects the write.
#[tokio::test]
async fn wait_for_resolves_after_matching_write() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    add_item(&repo, "inv1", "apple").await;
    let token = repo
        .consistency_token()
        .await
        .unwrap()
        .expect("token after write");

    // Bounded so a regression surfaces as a failure rather than a hang.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        subscription.wait_for(token),
    )
    .await
    .expect("wait_for must not hang")
    .expect("subscription reached token");

    assert_eq!(
        subscription.processed(),
        Some(0),
        "global progress reflects the single committed event"
    );

    subscription.stop().await.unwrap();
}

/// The crux: a token from a write the projection FILTERS OUT must still resolve
/// (via the global frontier), not stall. Without frontiers the subscription's
/// cursor would never reach the note's checkpoint and this would hang.
#[tokio::test]
async fn wait_for_resolves_for_filtered_out_write() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    // Write only an event `ItemCount` does not subscribe to.
    record_note(&repo, "note1", "hello").await;
    let token = repo
        .consistency_token()
        .await
        .unwrap()
        .expect("token after write");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        subscription.wait_for(token),
    )
    .await
    .expect("wait_for must resolve via frontier, not hang")
    .expect("subscription reached token");

    subscription.stop().await.unwrap();
}

/// A token captured before a subscription exists still resolves once a later
/// subscription catches up past it.
#[tokio::test]
async fn wait_for_resolves_against_historical_token() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "apple").await;
    add_item(&repo, "inv1", "banana").await;
    let token = repo
        .consistency_token()
        .await
        .unwrap()
        .expect("token after writes");

    // Subscription starts fresh and must catch up through the token.
    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        subscription.wait_for(token),
    )
    .await
    .expect("wait_for must not hang")
    .expect("subscription caught up to token");

    subscription.stop().await.unwrap();
}

/// An empty store has nothing to wait for.
#[tokio::test]
async fn consistency_token_is_none_when_empty() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);
    assert!(repo.consistency_token().await.unwrap().is_none());
}

/// The one-shot `create_tracked`/`update_tracked` return a token bound to the
/// write; awaiting it resolves and the projection reflects the write.
#[tokio::test]
async fn tracked_write_returns_resolvable_token() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    let token = repo
        .create_tracked::<Inventory, AddItem>(
            &"inv1".to_string(),
            &AddItem {
                name: "apple".to_string(),
            },
            &(),
        )
        .await
        .expect("create_tracked")
        .expect("token for committed write");

    // The token names exactly this write: position 0 (first, gap-free).
    assert_eq!(*token.checkpoint(), 0);

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        subscription.wait_for(token),
    )
    .await
    .expect("wait_for must not hang")
    .expect("subscription reached token");

    assert_eq!(subscription.processed(), Some(0));

    subscription.stop().await.unwrap();
}

/// `read_after()` awaits a write token and returns the current projection
/// without draining the update stream.
#[tokio::test]
async fn read_after_reflects_write_without_draining_stream() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    let subscription = repo.subscribe::<ItemCount>(()).start().await.unwrap();

    // Before any write, current() is the initial projection.
    assert_eq!(subscription.current().count, 0);

    for name in ["apple", "banana", "cherry"] {
        let token = repo
            .upsert_tracked::<Inventory, AddItem>(
                &"inv1".to_string(),
                &AddItem {
                    name: name.to_string(),
                },
                &(),
            )
            .await
            .expect("upsert_tracked")
            .expect("token");

        let projection = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            subscription.read_after(Some(token)),
        )
        .await
        .expect("read_after must not hang")
        .expect("subscription read after token");

        assert!(projection.count > 0);
    }

    // No `next()` draining anywhere — read_after/current sees all three writes.
    assert_eq!(subscription.current().count, 3);
    assert_eq!(subscription.read_after(None).await.unwrap().count, 3);

    subscription.stop().await.unwrap();
}

/// A command that produces no events yields no token.
#[tokio::test]
async fn tracked_write_without_events_returns_none() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store);

    add_item(&repo, "inv1", "apple").await;

    // `AddNothing` handles to an empty event vec on the existing stream.
    let token = repo
        .update_tracked::<Inventory, AddNothing>(&"inv1".to_string(), &AddNothing, &())
        .await
        .expect("update_tracked");

    assert!(token.is_none(), "no events committed, so no token");
}
