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

use crate::{
    concurrency::{ConcurrencyConflict, ConcurrencyStrategy},
    store::{
        AppendError, AppendResult, EventFilter, EventStore, GloballyOrderedStore, NonEmpty,
        StoredEventView, StreamKey, Transaction,
    },
};

/// Stored event with position and metadata.
///
/// This type represents an event that has been persisted to the in-memory store
/// and can be loaded back. It contains all the information needed to deserialize
/// the event data and metadata, along with the aggregate identifiers and position.
///
/// # Type Parameters
///
/// - `Id`: The aggregate ID type (must be `Clone + Eq + Hash + Send + Sync`)
/// - `M`: The metadata type (must be `Clone + Send + Sync`)
///
/// # Fields
///
/// All fields are accessible through the [`StoredEventView`](super::StoredEventView)
/// trait implementation. Use the trait methods rather than accessing fields
/// directly to maintain compatibility across store implementations.
#[derive(Clone)]
pub struct StoredEvent<Id, M> {
    aggregate_kind: String,
    aggregate_id: Id,
    kind: String,
    position: u64,
    data: serde_json::Value,
    metadata: M,
}

impl<Id, M> StoredEventView for StoredEvent<Id, M> {
    type Id = Id;
    type Pos = u64;
    type Metadata = M;

    fn aggregate_kind(&self) -> &str {
        &self.aggregate_kind
    }

    fn aggregate_id(&self) -> &Self::Id {
        &self.aggregate_id
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    fn position(&self) -> Self::Pos {
        self.position
    }

    fn metadata(&self) -> &Self::Metadata {
        &self.metadata
    }
}

/// Staged event awaiting persistence.
///
/// This type represents an event that has been serialized and prepared for
/// persistence but not yet written to the in-memory store. The repository creates
/// these from domain events, then batches them together for atomic appending.
///
/// # Type Parameters
///
/// - `M`: The metadata type (must be `Clone + Send + Sync`)
///
/// # Internal Use
///
/// This type is primarily used by the [`Store`] implementation. Users typically
/// interact with domain events directly and don't need to create `StagedEvent`
/// instances manually.
#[derive(Clone)]
pub struct StagedEvent<M> {
    kind: String,
    data: serde_json::Value,
    metadata: M,
}

/// In-memory event store that keeps streams in a hash map.
///
/// Uses a global sequence counter (`Position = u64`) to maintain chronological
/// ordering across streams, enabling cross-aggregate projections that need to
/// interleave events by time rather than by stream name.
///
/// Generic over:
/// - `Id`: Aggregate identifier type (must be hashable/equatable for map keys)
/// - `M`: Metadata type (use `()` when not needed)
#[derive(Clone)]
pub struct Store<Id, M> {
    inner: Arc<RwLock<Inner<Id, M>>>,
}

struct Inner<Id, M> {
    streams: HashMap<StreamKey<Id>, Vec<StoredEvent<Id, M>>>,
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
    type Error = InMemoryError;
    type Id = Id;
    type Metadata = M;
    type Position = u64;
    type StoredEvent = StoredEvent<Id, M>;
    type StagedEvent = StagedEvent<M>;

    fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
    where
        E: crate::event::EventKind + serde::Serialize,
    {
        let data = serde_json::to_value(event)
            .map_err(|e| InMemoryError::Serialization(Box::new(e)))?;
        Ok(StagedEvent {
            kind: event.kind().to_string(),
            data,
            metadata,
        })
    }

    fn decode_event<E>(&self, stored: &Self::StoredEvent) -> Result<E, Self::Error>
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

    fn begin<Conc: ConcurrencyStrategy>(
        &self,
        aggregate_kind: &str,
        aggregate_id: Self::Id,
        expected_version: Option<Self::Position>,
    ) -> Transaction<'_, Self, Conc> {
        Transaction::new(
            self,
            aggregate_kind.to_string(),
            aggregate_id,
            expected_version,
        )
    }

    #[tracing::instrument(skip(self, aggregate_id, events), fields(event_count = events.len()))]
    fn append<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<u64>,
        events: NonEmpty<Self::StagedEvent>,
    ) -> impl Future<Output = Result<AppendResult<u64>, AppendError<u64, Self::Error>>> + Send + 'a
    {
        let event_count = events.len();

        let result = (|| {
            let mut inner = self.inner.write().expect("in-memory store lock poisoned");
            // Check version if provided
            if let Some(expected) = expected_version {
                let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
                let current = inner
                    .streams
                    .get(&stream_key)
                    .and_then(|s| s.last().map(|e| e.position));
                if current != Some(expected) {
                    tracing::debug!(?expected, ?current, "version mismatch, rejecting append");
                    return Err(ConcurrencyConflict {
                        expected: Some(expected),
                        actual: current,
                    }
                    .into());
                }
            }

            let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
            let mut last_position = None;
            let mut stored = Vec::with_capacity(events.len());
            for e in events {
                let position = inner.next_position;
                inner.next_position += 1;
                last_position = Some(position);
                let StagedEvent {
                    kind,
                    data,
                    metadata,
                } = e;
                stored.push(StoredEvent {
                    aggregate_kind: aggregate_kind.to_string(),
                    aggregate_id: aggregate_id.clone(),
                    kind,
                    position,
                    data,
                    metadata,
                });
            }

            inner.streams.entry(stream_key).or_default().extend(stored);
            drop(inner);
            tracing::debug!(events_appended = event_count, "events appended to stream");
            Ok(AppendResult {
                last_position: last_position.expect("nonempty append"),
            })
        })();

        std::future::ready(result)
    }

    #[tracing::instrument(skip(self, aggregate_id, events), fields(event_count = events.len()))]
    fn append_expecting_new<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<Self::StagedEvent>,
    ) -> impl Future<Output = Result<AppendResult<u64>, AppendError<u64, Self::Error>>> + Send + 'a
    {
        let event_count = events.len();

        let result = (|| {
            let mut inner = self.inner.write().expect("in-memory store lock poisoned");
            // Check that stream is empty (new aggregate)
            let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
            let current = inner
                .streams
                .get(&stream_key)
                .and_then(|s| s.last().map(|e| e.position));

            if let Some(actual) = current {
                // Stream already has events - conflict!
                tracing::debug!(
                    ?actual,
                    "stream already exists, rejecting new aggregate append"
                );
                return Err(ConcurrencyConflict {
                    expected: None, // "expected new stream"
                    actual: Some(actual),
                }
                .into());
            }

            // Stream is empty, proceed with append (no further version check needed)
            let stream_key = StreamKey::new(aggregate_kind, aggregate_id.clone());
            let mut last_position = None;
            let mut stored = Vec::with_capacity(events.len());
            for e in events {
                let position = inner.next_position;
                inner.next_position += 1;
                last_position = Some(position);
                let StagedEvent {
                    kind,
                    data,
                    metadata,
                } = e;
                stored.push(StoredEvent {
                    aggregate_kind: aggregate_kind.to_string(),
                    aggregate_id: aggregate_id.clone(),
                    kind,
                    position,
                    data,
                    metadata,
                });
            }

            inner.streams.entry(stream_key).or_default().extend(stored);
            drop(inner);
            tracing::debug!(
                events_appended = event_count,
                "new stream created with events"
            );
            Ok(AppendResult {
                last_position: last_position.expect("nonempty append"),
            })
        })();

        std::future::ready(result)
    }

    #[tracing::instrument(skip(self, filters), fields(filter_count = filters.len()))]
    fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<Self::Id, Self::Position>],
    ) -> impl Future<Output = Result<Vec<Self::StoredEvent>, Self::Error>> + Send + 'a {
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
            |event: &StoredEvent<Id, M>, after_position: Option<u64>| -> bool {
                after_position.is_none_or(|after| event.position > after)
            };

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
    fn stage_event_captures_kind_and_data() {
        let store = Store::<String, ()>::new();
        let event = TestEvent { value: 42 };
        let staged = store.stage_event(&event, ()).unwrap();

        assert_eq!(staged.kind, "test-event");
        assert_eq!(staged.data, serde_json::json!({"value": 42}));
    }

    #[test]
    fn decode_event_deserializes() {
        let store = Store::<String, ()>::new();
        let event = TestEvent { value: 42 };
        let staged = store.stage_event(&event, ()).unwrap();

        // Create a stored event from the staged event
        let stored = StoredEvent {
            aggregate_kind: "test-agg".to_string(),
            aggregate_id: "id".to_string(),
            kind: staged.kind,
            position: 0,
            data: staged.data,
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
        let version = store.stream_version("test-agg", &"id".to_string()).await.unwrap();
        assert!(version.is_none());
    }

    #[tokio::test]
    async fn version_returns_position_after_append() {
        let store = Store::<String, ()>::new();
        let id = "id".to_string();
        let event = TestEvent { value: 1 };
        let staged = store.stage_event(&event, ()).unwrap();

        store
            .append_expecting_new("test-agg", &id, NonEmpty::from_vec(vec![staged]).unwrap())
            .await
            .unwrap();

        let version = store.stream_version("test-agg", &id).await.unwrap();
        assert_eq!(version, Some(0));
    }

    #[tokio::test]
    async fn append_with_wrong_version_returns_conflict() {
        let store = Store::<String, ()>::new();
        let id = "id".to_string();
        let event = TestEvent { value: 1 };

        // First, create the stream
        let staged = store.stage_event(&event, ()).unwrap();
        store
            .append_expecting_new("test-agg", &id, NonEmpty::from_vec(vec![staged]).unwrap())
            .await
            .unwrap();

        // Try to append with wrong expected version
        let staged2 = store.stage_event(&TestEvent { value: 2 }, ()).unwrap();
        let result = store
            .append(
                "test-agg",
                &id,
                Some(99), // wrong version
                NonEmpty::from_vec(vec![staged2]).unwrap(),
            )
            .await;

        assert!(matches!(result, Err(AppendError::Conflict(_))));
    }
}
