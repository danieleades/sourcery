//! Persistence layer abstractions.
//!
//! This module describes the storage contract ([`EventStore`]), wire formats,
//! transactions, and a reference in-memory implementation. Filters and
//! positions live here to keep storage concerns together.
//!
//! # Event Lifecycle
//!
//! Events flow through two representations during persistence:
//!
//! 1. **[`StagedEvent`]** - An event that has been serialized and staged for
//!    persistence. Created by [`EventStore::stage_event`], it holds the
//!    serialized event data but has no position yet (that's assigned by the
//!    store during append).
//!
//! 2. **[`StoredEvent`]** - An event loaded from the store. Contains the
//!    serialized event data plus store-assigned metadata (position, aggregate
//!    info). Use [`EventStore::decode_event`] to deserialize back to a domain
//!    event.
//!
//! ```text
//! DomainEvent ──stage_event()──▶ StagedEvent ──append()──▶ Database
//!                                                              │
//! DomainEvent ◀──decode_event()── StoredEvent ◀──load_events()─┘
//! ```
use std::{future::Future, marker::PhantomData};

pub use nonempty::NonEmpty;
use thiserror::Error;

use crate::concurrency::{ConcurrencyConflict, ConcurrencyStrategy, Optimistic, Unchecked};

pub mod inmemory;

/// An event that has been serialized and staged for persistence.
///
/// Created by [`EventStore::stage_event`] and consumed by
/// [`EventStore::append`]. The staged event holds the serialized event data and
/// metadata, but does not yet have a position—that is assigned by the store
/// during append.
///
/// # Type Parameters
///
/// - `Data`: The serialized event payload type (e.g., `serde_json::Value` for
///   JSON stores, `Vec<u8>` for binary stores)
/// - `M`: The metadata type
#[derive(Clone, Debug)]
pub struct StagedEvent<Data, M> {
    /// The event type identifier (e.g., `"account.deposited"`).
    pub kind: String,
    /// The serialized event payload.
    pub data: Data,
    /// Infrastructure metadata (timestamps, causation IDs, etc.).
    pub metadata: M,
}

/// An event loaded from the store.
///
/// Contains the serialized event data plus store-assigned metadata. Returned by
/// [`EventStore::load_events`]. Use [`EventStore::decode_event`] to deserialize
/// the `data` field back into a domain event.
///
/// # Type Parameters
///
/// - `Id`: The aggregate identifier type
/// - `Pos`: The position type used for ordering
/// - `Data`: The serialized event payload type (e.g., `serde_json::Value`)
/// - `M`: The metadata type
#[derive(Clone, Debug)]
pub struct StoredEvent<Id, Pos, Data, M> {
    /// The aggregate type identifier (e.g., `"account"`).
    pub aggregate_kind: String,
    /// The aggregate instance identifier.
    pub aggregate_id: Id,
    /// The event type identifier (e.g., `"account.deposited"`).
    pub kind: String,
    /// The global position assigned by the store.
    pub position: Pos,
    /// The serialized event payload.
    pub data: Data,
    /// Infrastructure metadata (timestamps, causation IDs, etc.).
    pub metadata: M,
}

impl<Id, Pos, Data, M> StoredEvent<Id, Pos, Data, M> {
    /// Returns the aggregate type identifier.
    #[inline]
    pub fn aggregate_kind(&self) -> &str {
        &self.aggregate_kind
    }

    /// Returns a reference to the aggregate instance identifier.
    #[inline]
    pub const fn aggregate_id(&self) -> &Id {
        &self.aggregate_id
    }

    /// Returns the event type identifier.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns a reference to the metadata.
    #[inline]
    pub const fn metadata(&self) -> &M {
        &self.metadata
    }
}

impl<Id, Pos: Clone, Data, M> StoredEvent<Id, Pos, Data, M> {
    /// Returns a copy of the position.
    #[inline]
    pub fn position(&self) -> Pos {
        self.position.clone()
    }
}

/// Filter describing which events should be loaded from the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventFilter<Id, Pos = ()> {
    pub event_kind: String,
    pub aggregate_kind: Option<String>,
    pub aggregate_id: Option<Id>,
    /// Only load events with position strictly greater than this value.
    /// Used for snapshot-based loading to skip already-applied events.
    pub after_position: Option<Pos>,
}

impl<Id, Pos> EventFilter<Id, Pos> {
    /// Load all events of the specified kind across every aggregate.
    #[must_use]
    pub fn for_event(kind: impl Into<String>) -> Self {
        Self {
            event_kind: kind.into(),
            aggregate_kind: None,
            aggregate_id: None,
            after_position: None,
        }
    }

    /// Load events of the specified kind for a single aggregate instance.
    #[must_use]
    pub fn for_aggregate(
        event_kind: impl Into<String>,
        aggregate_kind: impl Into<String>,
        aggregate_id: impl Into<Id>,
    ) -> Self {
        Self {
            event_kind: event_kind.into(),
            aggregate_kind: Some(aggregate_kind.into()),
            aggregate_id: Some(aggregate_id.into()),
            after_position: None,
        }
    }

    /// Only load events with position strictly greater than the given value.
    ///
    /// This is used for snapshot-based loading: load a snapshot at position N,
    /// then load events with `after(N)` to get only the events that occurred
    /// after the snapshot was taken.
    #[must_use]
    pub fn after(mut self, position: Pos) -> Self {
        self.after_position = Some(position);
        self
    }
}

/// Error from append operations with version checking.
#[derive(Debug, Error)]
pub enum AppendError<Pos, StoreError>
where
    Pos: std::fmt::Debug,
    StoreError: std::error::Error,
{
    /// Attempted to commit or append an empty batch.
    #[error("cannot append an empty event batch")]
    EmptyAppend,
    /// Concurrency conflict - another writer modified the stream.
    #[error(transparent)]
    Conflict(#[from] ConcurrencyConflict<Pos>),
    /// Underlying store error.
    #[error("store error: {0}")]
    Store(#[source] StoreError),
}

impl<Pos: std::fmt::Debug, StoreError: std::error::Error> AppendError<Pos, StoreError> {
    /// Create a store error variant.
    pub const fn store(err: StoreError) -> Self {
        Self::Store(err)
    }
}

/// Result of a successful append operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppendResult<Pos> {
    /// Position of the last event written in the batch.
    pub last_position: Pos,
}

/// Convenience alias for append outcomes returned by event stores.
pub type AppendOutcome<Pos, Err> = Result<AppendResult<Pos>, AppendError<Pos, Err>>;

/// A vector of stored events loaded from an event store.
pub type StoredEvents<Id, Pos, Data, M> = Vec<StoredEvent<Id, Pos, Data, M>>;

/// Result type for event loading operations.
pub type LoadEventsResult<Id, Pos, Data, M, E> = Result<StoredEvents<Id, Pos, Data, M>, E>;

/// Transaction for appending events to an aggregate instance.
///
/// Events are accumulated in the transaction and persisted atomically when
/// `commit()` is called. If the transaction is dropped without calling
/// `commit()`, the events are silently discarded (rolled back). This allows
/// errors during event serialization to be handled gracefully.
///
/// The `C` type parameter determines the concurrency strategy:
/// - [`Optimistic`]: Version checked on commit (default, recommended)
/// - [`Unchecked`]: No version checking (last-writer-wins)
#[must_use = "transactions do nothing unless committed"]
pub struct Transaction<'a, S: EventStore, C: ConcurrencyStrategy = Optimistic> {
    store: &'a S,
    aggregate_kind: String,
    aggregate_id: S::Id,
    expected_version: Option<S::Position>,
    events: Vec<StagedEvent<S::Data, S::Metadata>>,
    committed: bool,
    _concurrency: PhantomData<C>,
}

impl<'a, S: EventStore, C: ConcurrencyStrategy> Transaction<'a, S, C> {
    pub const fn new(
        store: &'a S,
        aggregate_kind: String,
        aggregate_id: S::Id,
        expected_version: Option<S::Position>,
    ) -> Self {
        Self {
            store,
            aggregate_kind,
            aggregate_id,
            expected_version,
            events: Vec::new(),
            committed: false,
            _concurrency: PhantomData,
        }
    }

    /// Append an event to the transaction.
    ///
    /// For sum-type events (enums), this serializes each variant to its
    /// staged form.
    ///
    /// # Errors
    ///
    /// Returns a store error if serialization fails.
    pub fn append<E>(&mut self, event: &E, metadata: S::Metadata) -> Result<(), S::Error>
    where
        E: crate::event::EventKind + serde::Serialize,
    {
        let staged = self.store.stage_event(event, metadata)?;
        tracing::trace!(event_kind = %event.kind(), "event appended to transaction");
        self.events.push(staged);
        Ok(())
    }
}

impl<S: EventStore> Transaction<'_, S, Unchecked> {
    /// Commit the transaction without version checking.
    ///
    /// Events are persisted atomically. No conflict detection is performed.
    ///
    /// # Errors
    ///
    /// Returns a store error if persistence fails.
    pub async fn commit(
        mut self,
    ) -> Result<AppendResult<S::Position>, AppendError<S::Position, S::Error>> {
        let events = std::mem::take(&mut self.events);
        let Some(events) = NonEmpty::from_vec(events) else {
            return Err(AppendError::EmptyAppend);
        };
        let event_count = events.len();
        tracing::debug!(
            aggregate_kind = %self.aggregate_kind,
            event_count,
            "committing transaction (unchecked)"
        );
        self.committed = true;
        self.store
            .append(&self.aggregate_kind, &self.aggregate_id, None, events)
            .await
            .map_err(|e| match e {
                AppendError::Store(e) => AppendError::Store(e),
                AppendError::Conflict(_) => unreachable!("conflict impossible without version"),
                AppendError::EmptyAppend => unreachable!("empty append filtered above"),
            })
    }
}

impl<S: EventStore> Transaction<'_, S, Optimistic> {
    /// Commit the transaction with version checking.
    ///
    /// The commit will fail with a [`ConcurrencyConflict`] if the stream
    /// version has changed since the aggregate was loaded.
    ///
    /// When `expected_version` is `None`, this means we expect a new aggregate
    /// (empty stream). The commit will fail with a conflict if the stream
    /// already has events.
    ///
    /// # Errors
    ///
    /// Returns [`AppendError::Conflict`] if another writer modified the stream,
    /// or [`AppendError::Store`] if persistence fails.
    pub async fn commit(
        mut self,
    ) -> Result<AppendResult<S::Position>, AppendError<S::Position, S::Error>> {
        let events = std::mem::take(&mut self.events);
        let Some(events) = NonEmpty::from_vec(events) else {
            return Err(AppendError::EmptyAppend);
        };
        let event_count = events.len();
        tracing::debug!(
            aggregate_kind = %self.aggregate_kind,
            event_count,
            expected_version = ?self.expected_version,
            "committing transaction (optimistic)"
        );
        self.committed = true;

        match self.expected_version.clone() {
            Some(version) => {
                // Expected specific version - delegate to store's version checking
                self.store
                    .append(
                        &self.aggregate_kind,
                        &self.aggregate_id,
                        Some(version),
                        events,
                    )
                    .await
            }
            None => {
                // Expected new stream - verify stream is actually empty
                self.store
                    .append_expecting_new(&self.aggregate_kind, &self.aggregate_id, events)
                    .await
            }
        }
    }
}

impl<S: EventStore, C: ConcurrencyStrategy> Drop for Transaction<'_, S, C> {
    fn drop(&mut self) {
        if !self.committed && !self.events.is_empty() {
            tracing::trace!(
                aggregate_kind = %self.aggregate_kind,
                expected_version = ?self.expected_version,
                event_count = self.events.len(),
                "transaction dropped without commit; discarding buffered events"
            );
        }
    }
}

/// Abstraction over the persistence layer for event streams.
///
/// This trait supports both stream-based and type-partitioned storage
/// implementations. Stores own event serialization/deserialization.
///
/// Associated types allow stores to customize their behavior:
/// - `Id`: Aggregate identifier type
/// - `Position`: Ordering strategy (`()` for stream-based, `u64` for global
///   ordering)
/// - `Metadata`: Infrastructure metadata type (timestamps, causation tracking,
///   etc.)
/// - `Data`: Serialized event payload type (e.g., `serde_json::Value` for JSON)
// ANCHOR: event_store_trait
pub trait EventStore: Send + Sync {
    /// Aggregate identifier type.
    ///
    /// This type must be clonable so repositories can reuse IDs across calls.
    /// Common choices: `String`, `Uuid`, or custom ID types.
    type Id: Clone + Send + Sync + 'static;

    /// Position type used for ordering events and version checking.
    ///
    /// Must be `Clone + PartialEq` to support optimistic concurrency.
    /// Use `()` if ordering is not needed.
    type Position: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static;

    /// Store-specific error type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Metadata type for infrastructure concerns.
    type Metadata: Send + Sync + 'static;

    /// Serialized event payload type.
    ///
    /// This is the format used to store event data internally. Common choices:
    /// - `serde_json::Value` for JSON-based stores
    /// - `Vec<u8>` for binary stores
    type Data: Clone + Send + Sync + 'static;

    /// Stage an event for append (serialize it).
    ///
    /// Converts a domain event into a [`StagedEvent`] ready for persistence.
    /// The staged event holds the serialized payload but has no position yet.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    fn stage_event<E>(
        &self,
        event: &E,
        metadata: Self::Metadata,
    ) -> Result<StagedEvent<Self::Data, Self::Metadata>, Self::Error>
    where
        E: crate::event::EventKind + serde::Serialize;

    /// Decode a stored event into a concrete event type.
    ///
    /// Deserializes the `data` field of a [`StoredEvent`] back into a domain
    /// event.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    fn decode_event<E>(
        &self,
        stored: &StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>,
    ) -> Result<E, Self::Error>
    where
        E: crate::event::DomainEvent + serde::de::DeserializeOwned;

    /// Get the current version (latest position) for an aggregate stream.
    ///
    /// Returns `None` for streams with no events.
    ///
    /// # Errors
    ///
    /// Returns a store-specific error when the operation fails.
    fn stream_version<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
    ) -> impl Future<Output = Result<Option<Self::Position>, Self::Error>> + Send + 'a;

    /// Begin a transaction for appending events to an aggregate.
    ///
    /// The transaction type is determined by the concurrency strategy `C`.
    ///
    /// # Arguments
    /// * `aggregate_kind` - The aggregate type identifier (`Aggregate::KIND`)
    /// * `aggregate_id` - The aggregate instance identifier
    /// * `expected_version` - The version expected for optimistic concurrency
    fn begin<C: ConcurrencyStrategy>(
        &self,
        aggregate_kind: &str,
        aggregate_id: Self::Id,
        expected_version: Option<Self::Position>,
    ) -> Transaction<'_, Self, C>
    where
        Self: Sized;

    /// Append events with optional version checking.
    ///
    /// If `expected_version` is `Some`, the append fails with a concurrency
    /// conflict if the current stream version doesn't match.
    /// If `expected_version` is `None`, no version checking is performed.
    ///
    /// # Errors
    ///
    /// Returns [`AppendError::Conflict`] if the version doesn't match, or
    /// [`AppendError::Store`] if persistence fails.
    fn append<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<StagedEvent<Self::Data, Self::Metadata>>,
    ) -> impl Future<Output = AppendOutcome<Self::Position, Self::Error>> + Send + 'a;

    /// Load events matching the specified filters.
    ///
    /// Each filter describes an event kind and optional aggregate identity:
    /// - [`EventFilter::for_event`] loads every event of the given kind
    /// - [`EventFilter::for_aggregate`] narrows to a single aggregate instance
    ///
    /// The store optimizes based on its storage model and returns events
    /// merged by position (if positions are available).
    ///
    /// # Errors
    ///
    /// Returns a store-specific error when loading fails.
    #[allow(clippy::type_complexity)]
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
    + 'a;

    /// Append events expecting an empty stream.
    ///
    /// This method is used by optimistic concurrency when creating new
    /// aggregates. It fails with a [`ConcurrencyConflict`] if the stream
    /// already has events.
    ///
    /// # Errors
    ///
    /// Returns [`AppendError::Conflict`] if the stream is not empty,
    /// or [`AppendError::Store`] if persistence fails.
    fn append_expecting_new<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<StagedEvent<Self::Data, Self::Metadata>>,
    ) -> impl Future<Output = AppendOutcome<Self::Position, Self::Error>> + Send + 'a;
}

/// Marker trait for stores that provide globally ordered positions.
///
/// Projection snapshots require this guarantee.
pub trait GloballyOrderedStore: EventStore {}
// ANCHOR_END: event_store_trait

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StreamKey<Id> {
    aggregate_kind: String,
    aggregate_id: Id,
}

impl<Id> StreamKey<Id> {
    pub(crate) fn new(aggregate_kind: impl Into<String>, aggregate_id: Id) -> Self {
        Self {
            aggregate_kind: aggregate_kind.into(),
            aggregate_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_filter_for_event_is_unrestricted() {
        let filter: EventFilter<String> = EventFilter::for_event("my-event");
        assert_eq!(filter.event_kind, "my-event");
        assert_eq!(filter.aggregate_kind, None);
        assert_eq!(filter.aggregate_id, None);
        assert_eq!(filter.after_position, None);
    }

    #[test]
    fn event_filter_for_aggregate_is_restricted() {
        let filter: EventFilter<String> =
            EventFilter::for_aggregate("my-event", "my-aggregate", "123");
        assert_eq!(filter.event_kind, "my-event");
        assert_eq!(filter.aggregate_kind.as_deref(), Some("my-aggregate"));
        assert_eq!(filter.aggregate_id.as_deref(), Some("123"));
        assert_eq!(filter.after_position, None);
    }

    #[test]
    fn event_filter_after_sets_after_position() {
        let filter: EventFilter<String, u64> = EventFilter::for_event("e").after(10);
        assert_eq!(filter.after_position, Some(10));
    }
}
