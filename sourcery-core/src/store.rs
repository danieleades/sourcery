//! Persistence layer abstractions.
//!
//! This module describes the storage contract ([`EventStore`]), wire formats,
//! and a reference in-memory implementation. Filters and positions live here
//! to keep storage concerns together.
//!
//! # Event Lifecycle
//!
//! Events flow through a simple lifecycle during persistence:
//!
//! ```text
//! DomainEvent ──commit_events()──▶ Database
//!                                      │
//! DomainEvent ◀──decode_event()── StoredEvent ◀──load_events()─┘
//! ```
//!
//! [`StoredEvent`] contains the serialized event data plus store-assigned
//! metadata (position, aggregate info). Use [`EventStore::decode_event`] to
//! deserialize back to a domain event.
use std::future::Future;

pub use nonempty::NonEmpty;
use thiserror::Error;

use crate::concurrency::ConcurrencyConflict;

pub mod inmemory;

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

/// Error from commit operations without version checking.
#[derive(Debug, Error)]
pub enum CommitError<StoreError>
where
    StoreError: std::error::Error,
{
    /// Failed to serialize an event.
    #[error("failed to serialize event at index {index}")]
    Serialization {
        index: usize,
        #[source]
        source: StoreError,
    },
    /// Underlying store error.
    #[error("store error: {0}")]
    Store(#[source] StoreError),
}

/// Error from commit operations with optimistic concurrency checking.
#[derive(Debug, Error)]
pub enum OptimisticCommitError<Pos, StoreError>
where
    Pos: std::fmt::Debug,
    StoreError: std::error::Error,
{
    /// Failed to serialize an event.
    #[error("failed to serialize event at index {index}")]
    Serialization {
        index: usize,
        #[source]
        source: StoreError,
    },
    /// Concurrency conflict - another writer modified the stream.
    #[error(transparent)]
    Conflict(#[from] ConcurrencyConflict<Pos>),
    /// Underlying store error.
    #[error("store error: {0}")]
    Store(#[source] StoreError),
}

/// Successful commit of events to a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Committed<Pos> {
    /// Position of the last event written in the batch.
    pub last_position: Pos,
}

/// A vector of stored events loaded from an event store.
pub type StoredEvents<Id, Pos, Data, M> = Vec<StoredEvent<Id, Pos, Data, M>>;

/// Result type for event loading operations.
pub type LoadEventsResult<Id, Pos, Data, M, E> = Result<StoredEvents<Id, Pos, Data, M>, E>;

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

    /// Commit events to an aggregate stream without version checking.
    ///
    /// Events are serialized and persisted atomically. No conflict detection
    /// is performed (last-writer-wins).
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::Serialization`] if an event fails to serialize,
    /// or [`CommitError::Store`] if persistence fails.
    fn commit_events<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<Output = Result<Committed<Self::Position>, CommitError<Self::Error>>> + Send + 'a
    where
        E: crate::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone;

    /// Commit events to an aggregate stream with optimistic concurrency
    /// control.
    ///
    /// Events are serialized and persisted atomically. The commit fails if:
    /// - `expected_version` is `Some(v)` and the current version differs from
    ///   `v`
    /// - `expected_version` is `None` and the stream already has events (new
    ///   aggregate expected)
    ///
    /// # Errors
    ///
    /// Returns [`OptimisticCommitError::Serialization`] if an event fails to
    /// serialize, [`OptimisticCommitError::Conflict`] if the version check
    /// fails, or [`OptimisticCommitError::Store`] if persistence fails.
    #[allow(clippy::type_complexity)]
    fn commit_events_optimistic<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<
        Output = Result<
            Committed<Self::Position>,
            OptimisticCommitError<Self::Position, Self::Error>,
        >,
    > + Send
    + 'a
    where
        E: crate::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone;

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
