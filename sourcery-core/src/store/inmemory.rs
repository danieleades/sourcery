//! In-memory event store implementation for testing.
//!
//! This module provides [`Store`], a thread-safe in-memory implementation of
//! [`EventStore`] suitable for unit tests and examples.
//!
//! # Example
//!
//! ```
//! use sourcery_core::store::inmemory;
//!
//! let store: inmemory::Store<String, ()> = inmemory::Store::new();
//! ```

use std::{
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::{Arc, RwLock},
};

use nonempty::NonEmpty;
use tokio::sync::broadcast;

use crate::{
    concurrency::ConcurrencyConflict,
    store::{
        CommitError, Committed, EventFilter, EventStore, GloballyOrderedStore, LoadEventsResult,
        OptimisticCommitError, StoredEvent, StreamKey,
    },
    subscription::{Checkpointed, Delivery, SubscribableStore},
};

/// Event stream stored in memory with fixed position and data types.
type InMemoryStream<Id, M> = Vec<StoredEvent<Id, u64, serde_json::Value, M>>;

/// Capacity of the commit-notification broadcast channel. A subscriber that
/// lags this far behind misses notifications but recovers on the next one,
/// since each notification triggers a full re-drain past its cursor.
const NOTIFY_CHANNEL_CAPACITY: usize = 1024;

/// In-memory event store that keeps streams in a hash map.
///
/// Uses a global sequence counter (`Position = u64`) to maintain chronological
/// ordering across streams, enabling cross-aggregate projections that need to
/// interleave events by time rather than by stream name.
///
/// Generic over:
/// - `Id`: Aggregate identifier type (must be hashable/equatable for map keys)
/// - `M`: Metadata type (use `()` when not needed)
///
/// This store uses `serde_json::Value` as the internal data representation.
#[derive(Clone)]
pub struct Store<Id, M> {
    inner: Arc<RwLock<Inner<Id, M>>>,
}

/// Shared mutable state of the in-memory store, protected by an `RwLock`.
struct Inner<Id, M> {
    /// All event streams, keyed by (`aggregate_kind`, `aggregate_id`).
    streams: HashMap<StreamKey<Id>, InMemoryStream<Id, M>>,
    /// Monotonically increasing counter used to assign globally ordered
    /// positions to every committed event.
    next_position: u64,
    /// Broadcasts the position of newly committed events. Subscribers use this
    /// to detect live writes without polling.
    notify_tx: broadcast::Sender<u64>,
}

impl<Id, M> Store<Id, M> {
    /// Create an empty in-memory store.
    ///
    /// Event positions start at `0` and increase globally across all streams.
    #[must_use]
    pub fn new() -> Self {
        let (notify_tx, _) = broadcast::channel(NOTIFY_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(RwLock::new(Inner {
                streams: HashMap::new(),
                next_position: 0,
                notify_tx,
            })),
        }
    }
}

impl<Id, M> Default for Store<Id, M> {
    fn default() -> Self {
        Self::new()
    }
}

/// Error type for in-memory store.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryError {
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl<Id, M> EventStore for Store<Id, M>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    M: Clone + Send + Sync + 'static,
{
    type Data = serde_json::Value;
    type Error = InMemoryError;
    type Id = Id;
    type Metadata = M;
    type Position = u64;

    fn decode_event<E>(
        &self,
        stored: &StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>,
    ) -> Result<E, Self::Error>
    where
        E: crate::event::DomainEvent + serde::de::DeserializeOwned,
    {
        serde::Deserialize::deserialize(&stored.data)
            .map_err(|e| InMemoryError::Deserialization(Box::new(e)))
    }

    #[tracing::instrument(skip(self, aggregate_id))]
    fn stream_version<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
    ) -> impl Future<Output = Result<Option<u64>, Self::Error>> + Send + 'a {
        let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
        let version = {
            let inner = self.inner.read().expect("in-memory store lock poisoned");
            inner
                .streams
                .get(&stream_key)
                .and_then(|s| s.last().map(|e| e.position))
        };
        tracing::trace!(?version, "retrieved stream version");
        std::future::ready(Ok(version))
    }

    #[tracing::instrument(skip(self, aggregate_id, events, metadata), fields(event_count = events.len()))]
    fn commit_events<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<Output = Result<Committed<u64>, CommitError<Self::Error>>> + Send + 'a
    where
        E: crate::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        let result = (|| {
            let staged = stage_events(&events)
                .map_err(|(index, source)| CommitError::Serialization { index, source })?;

            let mut inner = self.inner.write().expect("in-memory store lock poisoned");
            let (last_position, notify_tx) =
                commit_staged(&mut inner, aggregate_kind, aggregate_id, staged, metadata);
            drop(inner);
            // Notify subscribers of the new position (ignore send errors -- no
            // receivers is fine)
            let _ = notify_tx.send(last_position);
            tracing::debug!(events_appended = events.len(), "events committed to stream");
            Ok(Committed { last_position })
        })();

        std::future::ready(result)
    }

    #[tracing::instrument(skip(self, aggregate_id, events, metadata), fields(event_count = events.len()))]
    fn commit_events_optimistic<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<Output = Result<Committed<u64>, OptimisticCommitError<u64, Self::Error>>> + Send + 'a
    where
        E: crate::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        let result = (|| {
            let staged = stage_events(&events).map_err(|(index, source)| {
                OptimisticCommitError::Serialization { index, source }
            })?;

            let mut inner = self.inner.write().expect("in-memory store lock poisoned");
            let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());

            // Check version
            let current = inner
                .streams
                .get(&stream_key)
                .and_then(|s| s.last().map(|e| e.position));

            match expected_version {
                Some(expected) => {
                    // Expected specific version
                    if current != Some(expected) {
                        tracing::debug!(?expected, ?current, "version mismatch, rejecting commit");
                        return Err(ConcurrencyConflict {
                            expected: Some(expected),
                            actual: current,
                        }
                        .into());
                    }
                }
                None => {
                    // Expected new stream (no events)
                    if let Some(actual) = current {
                        tracing::debug!(
                            ?actual,
                            "stream already exists, rejecting new aggregate commit"
                        );
                        return Err(ConcurrencyConflict {
                            expected: None,
                            actual: Some(actual),
                        }
                        .into());
                    }
                }
            }

            let (last_position, notify_tx) =
                commit_staged(&mut inner, aggregate_kind, aggregate_id, staged, metadata);
            drop(inner);
            let _ = notify_tx.send(last_position);
            tracing::debug!(
                events_appended = events.len(),
                "events committed to stream (optimistic)"
            );
            Ok(Committed { last_position })
        })();

        std::future::ready(result)
    }

    #[tracing::instrument(skip(self, filters), fields(filter_count = filters.len()))]
    fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<Self::Id, Self::Position>],
    ) -> impl Future<
        Output = LoadEventsResult<
            Self::Id,
            Self::Position,
            Self::Data,
            Self::Metadata,
            Self::Error,
        >,
    > + Send
    + 'a {
        let result = load_matching_events(
            &self.inner.read().expect("in-memory store lock poisoned"),
            filters,
        );

        tracing::debug!(events_loaded = result.len(), "loaded events from store");
        std::future::ready(Ok(result))
    }
}

impl<Id, M> GloballyOrderedStore for Store<Id, M>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    M: Clone + Send + Sync + 'static,
{
}

impl<Id, M> SubscribableStore for Store<Id, M>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    M: Clone + Send + Sync + 'static,
{
    // The in-memory store assigns positions under a write lock from a single
    // global counter, so positions are already gap-free in commit order. The
    // checkpoint is therefore just the position.
    type Checkpoint = u64;

    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from: Option<Self::Checkpoint>,
    ) -> Pin<Box<impl futures_core::Stream<Item = Result<Delivery<Self>, Self::Error>> + Send + '_>>
    {
        let filters = filters.to_vec();
        let inner = self.inner.clone();

        // Subscribe to broadcast FIRST to avoid missing events committed
        // between the historical load and live listening.
        let mut rx = {
            let guard = inner.read().expect("in-memory store lock poisoned");
            guard.notify_tx.subscribe()
        };

        Box::pin(async_stream::stream! {
            // Snapshot the matching events and the global frontier together
            // under one lock, so the frontier provably covers every matching
            // event in the batch (positions are gap-free under the write lock).
            let drain = || {
                let guard = inner.read().expect("in-memory store lock poisoned");
                let events = load_matching_events(&guard, &filters);
                // Highest committed position; `None` while the store is empty.
                let frontier = guard.next_position.checked_sub(1);
                drop(guard);
                (events, frontier)
            };

            let mut last_position = from;

            // First pass is catch-up (drains immediately); each later pass is
            // live, woken by a commit notification. Dedup by `last_position`
            // makes the two phases identical, so they share one loop body.
            loop {
                let (events, frontier) = drain();
                for event in events {
                    if let Some(ref lp) = last_position
                        && event.position <= *lp
                    {
                        continue;
                    }
                    last_position = Some(event.position);
                    let checkpoint = event.position;
                    yield Ok(Delivery::Event(Checkpointed { checkpoint, event }));
                }
                if let Some(frontier) = frontier {
                    yield Ok(Delivery::Frontier(frontier));
                }

                // Wait for the next commit, then loop to re-drain. A lagged
                // receiver still triggers a drain (dedup absorbs any overlap);
                // a closed channel ends the stream.
                if rx.recv().await == Err(broadcast::error::RecvError::Closed) {
                    break;
                }
            }
        })
    }

    fn current_checkpoint(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send {
        // `load_matching_events` returns events sorted by position, so the last
        // one carries the highest currently-committed position.
        let latest = {
            let guard = self.inner.read().expect("in-memory store lock poisoned");
            load_matching_events(&guard, filters)
                .last()
                .map(|event| event.position)
        };
        std::future::ready(Ok(latest))
    }

    fn latest_checkpoint(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send {
        // Positions are gap-free; the highest committed one is the global cursor.
        let latest = {
            let guard = self.inner.read().expect("in-memory store lock poisoned");
            guard.next_position.checked_sub(1)
        };
        std::future::ready(Ok(latest))
    }

    fn checkpoint_for_position(
        &self,
        position: &Self::Position,
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send {
        // Checkpoint == position for the in-memory store: positions are gap-free
        // and assigned under the write lock, so a committed position is itself a
        // valid, exact cursor. Report `None` for positions not yet assigned.
        let checkpoint = {
            let guard = self.inner.read().expect("in-memory store lock poisoned");
            (*position < guard.next_position).then_some(*position)
        };
        std::future::ready(Ok(checkpoint))
    }
}

/// Serialise events to `(kind, json)` pairs, reporting the offending index on
/// failure so callers can build their commit-specific `Serialization` variant.
fn stage_events<E>(
    events: &NonEmpty<E>,
) -> Result<Vec<(String, serde_json::Value)>, (usize, InMemoryError)>
where
    E: crate::event::EventKind + serde::Serialize,
{
    let mut staged = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let data = serde_json::to_value(event)
            .map_err(|e| (index, InMemoryError::Serialization(Box::new(e))))?;
        staged.push((event.kind().to_string(), data));
    }
    Ok(staged)
}

/// Append a batch of pre-serialised events to the store, assigning positions.
///
/// The caller must hold the write lock and pass `&mut *inner`. The function
/// clones `notify_tx` before returning so the caller can drop the lock and
/// then send the notification outside of the critical section.
fn commit_staged<Id, M>(
    inner: &mut Inner<Id, M>,
    aggregate_kind: &str,
    aggregate_id: &Id,
    staged: Vec<(String, serde_json::Value)>,
    metadata: &M,
) -> (u64, broadcast::Sender<u64>)
where
    Id: Clone + Eq + std::hash::Hash,
    M: Clone,
{
    let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
    let mut last_position = 0;
    let mut stored = Vec::with_capacity(staged.len());

    for (kind, data) in staged {
        let position = inner.next_position;
        inner.next_position += 1;
        last_position = position;
        stored.push(StoredEvent {
            aggregate_kind: aggregate_kind.to_string(),
            aggregate_id: aggregate_id.clone(),
            kind,
            position,
            data,
            metadata: metadata.clone(),
        });
    }

    inner.streams.entry(stream_key).or_default().extend(stored);
    let notify_tx = inner.notify_tx.clone();
    (last_position, notify_tx)
}

/// Load all events matching the given filters, sorted by position.
fn load_matching_events<Id, M>(
    inner: &Inner<Id, M>,
    filters: &[EventFilter<Id, u64>],
) -> Vec<StoredEvent<Id, u64, serde_json::Value, M>>
where
    Id: Clone + Eq + std::hash::Hash,
    M: Clone,
{
    // An event is included when it satisfies *any* filter — the set-union
    // semantics of the Postgres store's `UNION`. Keying by the globally unique
    // position deduplicates events matched by several overlapping filters (e.g.
    // a global `events()` filter and an aggregate-scoped `events_for()` filter
    // on the same kind), and yields the result already sorted by position.
    let mut matched: BTreeMap<u64, StoredEvent<Id, u64, serde_json::Value, M>> = BTreeMap::new();

    for stream in inner.streams.values() {
        for event in stream {
            if !matched.contains_key(&event.position)
                && filters.iter().any(|filter| filter_matches(filter, event))
            {
                matched.insert(event.position, event.clone());
            }
        }
    }

    matched.into_values().collect()
}

/// Whether `event` satisfies `filter`: the event kind must match, any present
/// aggregate scope must match, and the position must be past `after_position`.
///
/// Mirrors the per-filter `WHERE` clause built by the Postgres store, so both
/// stores agree on which events a filter set selects.
fn filter_matches<Id, M>(
    filter: &EventFilter<Id, u64>,
    event: &StoredEvent<Id, u64, serde_json::Value, M>,
) -> bool
where
    Id: Eq,
{
    filter.event_kind == event.kind
        && filter
            .aggregate_kind
            .as_ref()
            .is_none_or(|kind| *kind == event.aggregate_kind)
        && filter
            .aggregate_id
            .as_ref()
            .is_none_or(|id| *id == event.aggregate_id)
        && filter
            .after_position
            .is_none_or(|after| event.position > after)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::event::DomainEvent;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestEvent {
        value: i32,
    }

    impl DomainEvent for TestEvent {
        const KIND: &'static str = "test-event";
    }

    #[test]
    fn new_has_no_streams() {
        let store = Store::<String, ()>::new();
        let inner = store.inner.read().unwrap();
        assert!(inner.streams.is_empty());
        assert_eq!(inner.next_position, 0);
        drop(inner);
    }

    #[test]
    fn decode_event_deserializes() {
        let store = Store::<String, ()>::new();
        let event = TestEvent { value: 42 };
        let data = serde_json::to_value(&event).unwrap();

        // Create a stored event
        let stored = StoredEvent {
            aggregate_kind: "test-agg".to_string(),
            aggregate_id: "id".to_string(),
            kind: "test-event".to_string(),
            position: 0,
            data,
            metadata: (),
        };

        let decoded: TestEvent = store.decode_event(&stored).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn error_display_serialization() {
        let err = InMemoryError::Serialization(Box::new(std::io::Error::other("test")));
        assert!(err.to_string().contains("serialization error"));
    }

    #[test]
    fn error_display_deserialization() {
        let err = InMemoryError::Deserialization(Box::new(std::io::Error::other("test")));
        assert!(err.to_string().contains("deserialization error"));
    }

    #[tokio::test]
    async fn version_returns_none_for_new_stream() {
        let store = Store::<String, ()>::new();
        let version = store
            .stream_version("test-agg", &"id".to_string())
            .await
            .unwrap();
        assert!(version.is_none());
    }

    #[tokio::test]
    async fn version_returns_position_after_commit() {
        let store = Store::<String, ()>::new();
        let id = "id".to_string();
        let events = NonEmpty::singleton(TestEvent { value: 1 });

        store
            .commit_events("test-agg", &id, events, &())
            .await
            .unwrap();

        let version = store.stream_version("test-agg", &id).await.unwrap();
        assert_eq!(version, Some(0));
    }

    #[tokio::test]
    async fn commit_with_wrong_version_returns_conflict() {
        let store = Store::<String, ()>::new();
        let id = "id".to_string();
        let events1 = NonEmpty::singleton(TestEvent { value: 1 });

        // First, create the stream
        store
            .commit_events("test-agg", &id, events1, &())
            .await
            .unwrap();

        // Try to commit with wrong expected version
        let events2 = NonEmpty::singleton(TestEvent { value: 2 });
        let result = store
            .commit_events_optimistic("test-agg", &id, Some(99), events2, &())
            .await;

        assert!(matches!(result, Err(OptimisticCommitError::Conflict(_))));
    }

    #[tokio::test]
    async fn commit_new_stream_fails_if_stream_exists() {
        let store = Store::<String, ()>::new();
        let id = "id".to_string();
        let events = NonEmpty::singleton(TestEvent { value: 1 });

        // First, create the stream
        store
            .commit_events("test-agg", &id, events, &())
            .await
            .unwrap();

        // Try to commit expecting new stream
        let events2 = NonEmpty::singleton(TestEvent { value: 2 });
        let result = store
            .commit_events_optimistic("test-agg", &id, None, events2, &())
            .await;

        assert!(matches!(result, Err(OptimisticCommitError::Conflict(_))));
    }
}
