//! In-memory event store implementation for testing.
//!
//! This module provides [`Store`], a thread-safe in-memory implementation of
//! [`EventStore`](super::EventStore) suitable for unit tests and examples.
//!
//! # Example
//!
//! ```
//! use sourcery_core::store::inmemory;
//!
//! let store: inmemory::Store<String, ()> = inmemory::Store::new();
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use nonempty::NonEmpty;

use crate::{
    concurrency::ConcurrencyConflict,
    store::{
        CommitError, Committed, EventFilter, EventStore, GloballyOrderedStore, LoadEventsResult,
        OptimisticCommitError, StoredEvent, StreamKey,
    },
};

/// Event stream stored in memory with fixed position and data types.
type InMemoryStream<Id, M> = Vec<StoredEvent<Id, u64, serde_json::Value, M>>;

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

struct Inner<Id, M> {
    streams: HashMap<StreamKey<Id>, InMemoryStream<Id, M>>,
    next_position: u64,
}

impl<Id, M> Store<Id, M> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                streams: HashMap::new(),
                next_position: 0,
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
        serde_json::from_value(stored.data.clone())
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
            // Serialize all events first
            let mut staged = Vec::with_capacity(events.len());
            for (index, event) in events.iter().enumerate() {
                let data = serde_json::to_value(event).map_err(|e| CommitError::Serialization {
                    index,
                    source: InMemoryError::Serialization(Box::new(e)),
                })?;
                staged.push((event.kind().to_string(), data));
            }

            let mut inner = self.inner.write().expect("in-memory store lock poisoned");
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
            drop(inner);
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
            // Serialize all events first
            let mut staged = Vec::with_capacity(events.len());
            for (index, event) in events.iter().enumerate() {
                let data = serde_json::to_value(event).map_err(|e| {
                    OptimisticCommitError::Serialization {
                        index,
                        source: InMemoryError::Serialization(Box::new(e)),
                    }
                })?;
                staged.push((event.kind().to_string(), data));
            }

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
            drop(inner);
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
        use std::collections::HashSet;

        let inner = self.inner.read().expect("in-memory store lock poisoned");
        let mut result = Vec::new();
        let mut seen: HashSet<(StreamKey<Id>, String)> = HashSet::new(); // (stream key, event kind)

        // Group filters by aggregate ID, tracking each filter's individual position
        // constraint Maps event_kind -> after_position for that specific filter
        let mut all_kinds: HashMap<String, Option<u64>> = HashMap::new(); // Filters with no aggregate restriction
        let mut by_aggregate: HashMap<StreamKey<Id>, HashMap<String, Option<u64>>> = HashMap::new(); // Filters targeting a specific aggregate

        for filter in filters {
            if let (Some(kind), Some(id)) = (&filter.aggregate_kind, &filter.aggregate_id) {
                by_aggregate
                    .entry(StreamKey::new(kind.clone(), id.clone()))
                    .or_default()
                    .insert(filter.event_kind.clone(), filter.after_position);
            } else {
                all_kinds.insert(filter.event_kind.clone(), filter.after_position);
            }
        }

        // Helper to check position filter for a specific after_position constraint
        let passes_position_filter =
            |event: &StoredEvent<Id, u64, serde_json::Value, M>,
             after_position: Option<u64>|
             -> bool { after_position.is_none_or(|after| event.position > after) };

        // Load events for specific aggregates
        for (stream_key, kinds) in &by_aggregate {
            if let Some(stream) = inner.streams.get(stream_key) {
                for event in stream {
                    // Check if this event kind is requested AND passes its specific position filter
                    if let Some(&after_pos) = kinds.get(&event.kind)
                        && passes_position_filter(event, after_pos)
                    {
                        // Track that we've seen this (aggregate_kind, aggregate_id, kind) triple
                        seen.insert((
                            StreamKey::new(
                                event.aggregate_kind.clone(),
                                event.aggregate_id.clone(),
                            ),
                            event.kind.clone(),
                        ));
                        result.push(event.clone());
                    }
                }
            }
        }

        // Load events from all aggregates for unfiltered kinds
        // Skip events we've already loaded for specific aggregates
        if !all_kinds.is_empty() {
            for stream in inner.streams.values() {
                for event in stream {
                    // Check if this event kind is requested AND passes its specific position filter
                    if let Some(&after_pos) = all_kinds.get(&event.kind)
                        && passes_position_filter(event, after_pos)
                    {
                        let key = (
                            StreamKey::new(
                                event.aggregate_kind.clone(),
                                event.aggregate_id.clone(),
                            ),
                            event.kind.clone(),
                        );
                        if !seen.contains(&key) {
                            result.push(event.clone());
                        }
                    }
                }
            }
        }

        // Sort by position for chronological ordering across streams
        result.sort_by_key(|event| event.position);

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
