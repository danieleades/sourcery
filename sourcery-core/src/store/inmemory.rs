use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, RwLock},
};

use crate::{
    codec::Codec,
    concurrency::{ConcurrencyConflict, ConcurrencyStrategy},
    store::{
        AppendError, AppendResult, EventFilter, EventStore, GloballyOrderedStore, NonEmpty,
        PersistableEvent, StoredEvent, StreamKey, Transaction,
    },
};

/// In-memory event store that keeps streams in a hash map.
///
/// Uses a global sequence counter (`Position = u64`) to maintain chronological
/// ordering across streams, enabling cross-aggregate projections that need to
/// interleave events by time rather than by stream name.
///
/// Generic over:
/// - `Id`: Aggregate identifier type (must be hashable/equatable for map keys)
/// - `C`: Serialization codec
/// - `M`: Metadata type (use `()` when not needed)
#[derive(Clone)]
pub struct Store<Id, C, M>
where
    C: Codec,
{
    codec: C,
    inner: Arc<RwLock<Inner<Id, M>>>,
}

struct Inner<Id, M> {
    streams: HashMap<StreamKey<Id>, Vec<StoredEvent<Id, u64, M>>>,
    next_position: u64,
}

impl<Id, C, M> Store<Id, C, M>
where
    C: Codec,
{
    #[must_use]
    pub fn new(codec: C) -> Self {
        Self {
            codec,
            inner: Arc::new(RwLock::new(Inner {
                streams: HashMap::new(),
                next_position: 0,
            })),
        }
    }
}

/// Infallible error type that implements `std::error::Error`.
///
/// Used by [`InMemoryEventStore`] which cannot fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("infallible")]
pub enum InMemoryError {}

impl From<Infallible> for InMemoryError {
    fn from(x: Infallible) -> Self {
        match x {}
    }
}

impl<Id, C, M> EventStore for Store<Id, C, M>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    C: Codec + Clone + Send + Sync + 'static,
    M: Clone + Send + Sync + 'static,
{
    type Codec = C;
    // Global sequence for chronological ordering
    type Error = InMemoryError;
    type Id = Id;
    type Metadata = M;
    type Position = u64;

    fn codec(&self) -> &Self::Codec {
        &self.codec
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
        events: NonEmpty<PersistableEvent<Self::Metadata>>,
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
            let stored: Vec<StoredEvent<Id, u64, M>> = events
                .into_iter()
                .map(|e| {
                    let position = inner.next_position;
                    inner.next_position += 1;
                    last_position = Some(position);
                    StoredEvent {
                        aggregate_kind: aggregate_kind.to_string(),
                        aggregate_id: aggregate_id.clone(),
                        kind: e.kind,
                        position,
                        data: e.data,
                        metadata: e.metadata,
                    }
                })
                .collect();

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
        events: NonEmpty<PersistableEvent<Self::Metadata>>,
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
            let stored: Vec<StoredEvent<Id, u64, M>> = events
                .into_iter()
                .map(|e| {
                    let position = inner.next_position;
                    inner.next_position += 1;
                    last_position = Some(position);
                    StoredEvent {
                        aggregate_kind: aggregate_kind.to_string(),
                        aggregate_id: aggregate_id.clone(),
                        kind: e.kind,
                        position,
                        data: e.data,
                        metadata: e.metadata,
                    }
                })
                .collect();

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
    ) -> impl Future<Output = Result<Vec<StoredEvent<Id, u64, M>>, Self::Error>> + Send + 'a {
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
            |event: &StoredEvent<Id, u64, M>, after_position: Option<u64>| -> bool {
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

impl<Id, C, M> GloballyOrderedStore for Store<Id, C, M>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    C: Codec + Clone + Send + Sync + 'static,
    M: Clone + Send + Sync + 'static,
{
}
