//! Integration tests for reactor functionality.

#![cfg(feature = "test-util")]

use std::{
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, Create, DomainEvent, EventContext, Handle, HandleCreate, React, ReactError,
    ReactFilters, Reactor, ReactorError, Repository, RetryPolicy,
    repository::CommandError,
    snapshot::{OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore},
    store::inmemory,
};

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

#[derive(Clone)]
struct RecordingReactor {
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingReactor {
    const fn new(seen: Arc<Mutex<Vec<String>>>) -> Self {
        Self { seen }
    }
}

impl Reactor for RecordingReactor {
    type Id = String;
    type Metadata = ();

    const KIND: &'static str = "recording-reactor";

    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: sourcery::store::EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        ReactFilters::new().event::<ItemAdded>()
    }
}

impl React<ItemAdded> for RecordingReactor {
    fn react(
        &self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &ItemAdded,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        let seen = Arc::clone(&self.seen);
        let name = event.name.clone();
        async move {
            seen.lock().expect("seen lock poisoned").push(name);
            Ok(())
        }
    }
}

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

#[tokio::test]
async fn reactor_processes_historical_events_before_start_returns() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    add_item(&repo, "inv1", "apple").await;
    add_item(&repo, "inv1", "banana").await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let reactor = repo
        .react(RecordingReactor::new(Arc::clone(&seen)))
        .start()
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().expect("seen lock poisoned"),
        vec!["apple".to_string(), "banana".to_string()]
    );

    reactor.stop().await.unwrap();
}

#[tokio::test]
async fn reactor_wait_for_resolves_after_live_event() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reactor = repo
        .react(RecordingReactor::new(Arc::clone(&seen)))
        .start()
        .await
        .unwrap();

    let token = repo
        .create_tracked::<Inventory, AddItem>(
            &"inv1".to_string(),
            &AddItem {
                name: "cherry".to_string(),
            },
            &(),
        )
        .await
        .unwrap()
        .expect("token");

    tokio::time::timeout(std::time::Duration::from_secs(5), reactor.wait_for(token))
        .await
        .expect("reactor wait_for must not hang")
        .expect("reactor reached token");

    assert_eq!(
        *seen.lock().expect("seen lock poisoned"),
        vec!["cherry".to_string()]
    );

    reactor.stop().await.unwrap();
}

#[tokio::test]
async fn reactor_with_checkpoints_resumes_after_persisted_checkpoint() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    let checkpoints = sourcery::snapshot::inmemory::Store::<u64>::always();

    add_item(&repo, "inv1", "apple").await;
    add_item(&repo, "inv1", "banana").await;

    let first_seen = Arc::new(Mutex::new(Vec::new()));
    let first = repo
        .react(RecordingReactor::new(Arc::clone(&first_seen)))
        .with_checkpoints(checkpoints.clone())
        .start()
        .await
        .unwrap();
    first.stop().await.unwrap();

    add_item(&repo, "inv1", "cherry").await;

    let second_seen = Arc::new(Mutex::new(Vec::new()));
    let second = repo
        .react(RecordingReactor::new(Arc::clone(&second_seen)))
        .with_checkpoints(checkpoints)
        .start()
        .await
        .unwrap();

    assert_eq!(
        *first_seen.lock().expect("seen lock poisoned"),
        vec!["apple".to_string(), "banana".to_string()]
    );
    assert_eq!(
        *second_seen.lock().expect("seen lock poisoned"),
        vec!["cherry".to_string()]
    );

    second.stop().await.unwrap();
}

#[tokio::test]
async fn reactor_with_batched_checkpoints_resumes_from_last_persisted_batch() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    // `every(2)` persists a checkpoint only on every second processed event.
    let checkpoints = sourcery::snapshot::inmemory::Store::<u64>::every(2);

    // Three historical events. With `every(2)`, the checkpoint advances to the
    // second event ("banana"); the third ("cherry") is processed but left in an
    // un-checkpointed partial batch.
    add_item(&repo, "inv1", "apple").await;
    add_item(&repo, "inv1", "banana").await;
    add_item(&repo, "inv1", "cherry").await;

    let first_seen = Arc::new(Mutex::new(Vec::new()));
    let first = repo
        .react(RecordingReactor::new(Arc::clone(&first_seen)))
        .with_checkpoints(checkpoints.clone())
        .start()
        .await
        .unwrap();
    first.stop().await.unwrap();

    add_item(&repo, "inv1", "date").await;

    let second_seen = Arc::new(Mutex::new(Vec::new()));
    let second = repo
        .react(RecordingReactor::new(Arc::clone(&second_seen)))
        .with_checkpoints(checkpoints)
        .start()
        .await
        .unwrap();

    assert_eq!(
        *first_seen.lock().expect("seen lock poisoned"),
        vec![
            "apple".to_string(),
            "banana".to_string(),
            "cherry".to_string()
        ]
    );
    // Resume must restart after the last *persisted* checkpoint ("banana"), so
    // the second runner re-delivers the un-checkpointed tail ("cherry") and the
    // new live event ("date") — and nothing before "banana".
    assert_eq!(
        *second_seen.lock().expect("seen lock poisoned"),
        vec!["cherry".to_string(), "date".to_string()]
    );

    second.stop().await.unwrap();
}

/// Fails its first `remaining_failures` reactions with a retryable error, then
/// records the event name on success.
#[derive(Clone)]
struct FlakyReactor {
    remaining_failures: Arc<Mutex<usize>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Reactor for FlakyReactor {
    type Id = String;
    type Metadata = ();

    const KIND: &'static str = "flaky-reactor";

    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: sourcery::store::EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        ReactFilters::new().event::<ItemAdded>()
    }
}

impl React<ItemAdded> for FlakyReactor {
    fn react(
        &self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &ItemAdded,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        let remaining = Arc::clone(&self.remaining_failures);
        let seen = Arc::clone(&self.seen);
        let name = event.name.clone();
        async move {
            let should_fail = {
                let mut remaining = remaining.lock().expect("remaining lock poisoned");
                if *remaining > 0 {
                    *remaining -= 1;
                    true
                } else {
                    false
                }
            };
            if should_fail {
                return Err(ReactError::retry(io::Error::other("transient")));
            }
            seen.lock().expect("seen lock poisoned").push(name);
            Ok(())
        }
    }
}

#[tokio::test]
async fn reactor_retries_transient_failures_then_succeeds() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    add_item(&repo, "inv1", "apple").await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let reactor = repo
        .react(FlakyReactor {
            remaining_failures: Arc::new(Mutex::new(2)),
            seen: Arc::clone(&seen),
        })
        .with_retry(RetryPolicy::exponential(
            Duration::from_millis(1),
            Duration::from_millis(5),
            Some(10),
        ))
        .start()
        .await
        .unwrap();

    // `start()` returns only after catch-up, which includes the retries; the
    // event is recorded exactly once despite the two transient failures.
    assert_eq!(
        *seen.lock().expect("seen lock poisoned"),
        vec!["apple".to_string()]
    );

    reactor.stop().await.unwrap();
}

/// Reacts to every `ItemAdded` with a configurable [`ReactError`].
#[derive(Clone)]
struct AlwaysFailsReactor {
    fatal: bool,
}

impl Reactor for AlwaysFailsReactor {
    type Id = String;
    type Metadata = ();

    const KIND: &'static str = "always-fails-reactor";

    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: sourcery::store::EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        ReactFilters::new().event::<ItemAdded>()
    }
}

impl React<ItemAdded> for AlwaysFailsReactor {
    fn react(
        &self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        _event: &ItemAdded,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        let fatal = self.fatal;
        async move {
            let error = io::Error::other("boom");
            if fatal {
                Err(ReactError::fatal(error))
            } else {
                Err(ReactError::retry(error))
            }
        }
    }
}

#[tokio::test]
async fn reactor_fatal_error_surfaces_as_reaction_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    add_item(&repo, "inv1", "apple").await;

    // A fatal reaction on a historical event halts catch-up, so `start()`
    // surfaces the failure directly.
    let result = repo.react(AlwaysFailsReactor { fatal: true }).start().await;

    assert!(matches!(result, Err(ReactorError::Reaction(_))));
}

#[tokio::test]
async fn reactor_exhausting_max_retries_surfaces_as_reaction_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    add_item(&repo, "inv1", "apple").await;

    let result = repo
        .react(AlwaysFailsReactor { fatal: false })
        .with_retry(RetryPolicy::exponential(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Some(2),
        ))
        .start()
        .await;

    assert!(matches!(result, Err(ReactorError::Reaction(_))));
}

/// Issues a command in reaction to an event: mirrors each `ItemAdded` from
/// Issues a command in reaction to an event: mirrors each `ItemAdded` from
/// `source_id` into the "mirror" inventory. Scoped with `event_for` so its own
/// writes do not feed back into it.
#[derive(Clone)]
struct MirroringReactor {
    store: inmemory::Store<String, ()>,
    source_id: String,
}

impl Reactor for MirroringReactor {
    type Id = String;
    type Metadata = ();

    const KIND: &'static str = "mirroring-reactor";

    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: sourcery::store::EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        ReactFilters::new().event_for::<Inventory, ItemAdded>(&self.source_id)
    }
}

impl React<ItemAdded> for MirroringReactor {
    fn react(
        &self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &ItemAdded,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        let store = self.store.clone();
        let name = event.name.clone();
        async move {
            let repo = Repository::new(store);
            let id = "mirror".to_string();
            let command = AddItem { name };
            repo.upsert::<Inventory, AddItem>(&id, &command, &())
                .await
                .map_err(|error| ReactError::retry(io::Error::other(error.to_string())))?;
            Ok(())
        }
    }
}

#[tokio::test]
async fn reactor_issues_commands_in_reaction_to_events() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store.clone());

    let reactor = repo
        .react(MirroringReactor {
            store: store.clone(),
            source_id: "inv1".to_string(),
        })
        .start()
        .await
        .unwrap();

    repo.create_tracked::<Inventory, AddItem>(
        &"inv1".to_string(),
        &AddItem {
            name: "apple".to_string(),
        },
        &(),
    )
    .await
    .unwrap()
    .expect("token");

    let token = repo
        .update_tracked::<Inventory, AddItem>(
            &"inv1".to_string(),
            &AddItem {
                name: "banana".to_string(),
            },
            &(),
        )
        .await
        .unwrap()
        .expect("token");

    tokio::time::timeout(Duration::from_secs(5), reactor.wait_for(token))
        .await
        .expect("reactor wait_for must not hang")
        .expect("reactor reached token");

    // Each source `ItemAdded` drove one upsert into the mirror inventory.
    let mirror = repo
        .load::<Inventory>(&"mirror".to_string())
        .await
        .unwrap()
        .expect("mirror inventory exists");
    assert_eq!(mirror.count, 2);

    reactor.stop().await.unwrap();
}

/// Checkpoint store that always returns an error on `load`.
struct UnreachableCheckpointStore;

impl SnapshotStore<String> for UnreachableCheckpointStore {
    type Error = io::Error;
    type Position = u64;

    fn load<T>(
        &self,
        _kind: &str,
        _id: &String,
    ) -> impl std::future::Future<Output = Result<Option<Snapshot<Self::Position, T>>, Self::Error>> + Send
    where
        T: serde::de::DeserializeOwned,
    {
        async { Err(io::Error::other("checkpoint store unreachable")) }
    }

    fn offer_snapshot<CE, T, Create>(
        &self,
        _kind: &str,
        _id: &String,
        _events_since_last_snapshot: u64,
        _create_snapshot: Create,
    ) -> impl std::future::Future<
        Output = Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>,
    > + Send
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE> + Send,
    {
        async { Ok(SnapshotOffer::Declined) }
    }
}

#[tokio::test]
async fn reactor_checkpoint_load_failure_surfaces_as_checkpoint_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    let result = repo
        .react(RecordingReactor::new(Arc::new(Mutex::new(Vec::new()))))
        .with_checkpoints(UnreachableCheckpointStore)
        .start()
        .await;

    assert!(
        matches!(result, Err(ReactorError::Checkpoint(_))),
        "a failing checkpoint store should surface as ReactorError::Checkpoint"
    );
}

#[tokio::test]
async fn reactor_handle_reports_liveness_and_progress() {
    // `is_running()` and `processed()` are the observable state API on a live
    // reactor handle; verify their semantics before and after handling an event.
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    let seen = Arc::new(Mutex::new(Vec::new()));

    let reactor = repo
        .react(RecordingReactor::new(Arc::clone(&seen)))
        .start()
        .await
        .unwrap();

    // Reactor just started on an empty store — no events processed yet.
    assert!(reactor.is_running(), "reactor must be running after start");
    assert!(
        reactor.processed().is_none(),
        "processed must be None before the first event"
    );

    let token = repo
        .create_tracked::<Inventory, AddItem>(
            &"inv1".to_string(),
            &AddItem {
                name: "fig".to_string(),
            },
            &(),
        )
        .await
        .unwrap()
        .expect("token");

    tokio::time::timeout(Duration::from_secs(5), reactor.wait_for(token))
        .await
        .expect("reactor wait_for must not hang")
        .expect("reactor reached token");

    assert!(
        reactor.processed().is_some(),
        "processed must advance after an event is handled"
    );
    assert!(reactor.is_running(), "reactor must still be running");

    reactor.stop().await.unwrap();
}

#[tokio::test]
async fn reactor_stops_cleanly_during_retry_sleep() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    // Start on an empty store so there are no historical events to process.
    let reactor = repo
        .react(AlwaysFailsReactor { fatal: false })
        .with_retry(RetryPolicy::exponential(
            Duration::from_millis(200),
            Duration::from_millis(200),
            None,
        ))
        .start()
        .await
        .unwrap();

    // A live event triggers the retry loop; the reactor will sleep 200 ms
    // between retries. Stopping before the sleep expires should interrupt it
    // cleanly (via the biased select! on stop_rx) and return Ok.
    add_item(&repo, "inv1", "apple").await;
    tokio::task::yield_now().await;

    let result = tokio::time::timeout(Duration::from_secs(5), reactor.stop()).await;
    assert!(
        result
            .expect("reactor.stop() must complete within timeout")
            .is_ok(),
        "stopping a retrying reactor should succeed, not propagate the retry error"
    );
}
