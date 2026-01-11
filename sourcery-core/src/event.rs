//! Domain event marker.
//!
//! `DomainEvent` is the lightweight trait every concrete event struct
//! implements. It intentionally avoids persistence concerns; serialization is
//! handled by the event stores.

use thiserror::Error;

/// Error returned when deserializing a stored event fails.
#[derive(Debug, Error)]
pub enum EventDecodeError<StoreError> {
    /// The event kind was not recognized by this event enum.
    #[error("unknown event kind `{kind}`, expected one of {expected:?}")]
    UnknownKind {
        /// The unrecognized event kind string.
        kind: String,
        /// The list of event kinds this enum can handle.
        expected: &'static [&'static str],
    },
    /// The store failed to deserialize the event data.
    #[error("store error: {0}")]
    Store(#[source] StoreError),
}

/// Marker trait for events that can be persisted by the event store.
///
/// Each event carries a unique [`Self::KIND`] identifier so the repository can
/// route stored bytes back to the correct type when rebuilding aggregates or
/// projections.
///
/// Most projects implement this trait by hand, but the `#[derive(Aggregate)]`
/// macro generates it automatically for the aggregate event enums it creates.
pub trait DomainEvent {
    const KIND: &'static str;
}

/// Extension trait for getting the event kind from an event instance.
///
/// This trait has a blanket implementation for all types that implement
/// [`DomainEvent`], ensuring that the `kind()` method always returns the
/// same value as the `KIND` constant.
///
/// # Relationship to `DomainEvent`
///
/// You should implement [`DomainEvent`] on your event types. This trait
/// (`EventKind`) is automatically derived from `DomainEvent` and provides
/// an instance method `kind()` that returns the same value as the `KIND`
/// constant.
///
/// **You never need to implement this trait yourself** - it's automatically
/// available on any type that implements `DomainEvent`.
pub trait EventKind {
    fn kind(&self) -> &'static str;
}

impl<T: DomainEvent> EventKind for T {
    fn kind(&self) -> &'static str {
        T::KIND
    }
}

/// Trait for event sum types that can deserialize themselves from stored
/// events.
///
/// Implemented by event enums to deserialize stored events.
///
/// Deriving [`Aggregate`](crate::aggregate::Aggregate) includes a `ProjectionEvent`
/// implementation for the generated sum type. Custom enums can opt in manually
/// using the pattern illustrated below.
pub trait ProjectionEvent: Sized {
    /// The list of event kinds this sum type can deserialize.
    ///
    /// This data is generated automatically when using the derive macros.
    const EVENT_KINDS: &'static [&'static str];

    /// Deserialize an event from stored representation.
    ///
    /// # Errors
    ///
    /// Returns [`EventDecodeError::UnknownKind`] if the event kind is not
    /// recognized, or [`EventDecodeError::Store`] if deserialization fails.
    fn from_stored<S: crate::store::EventStore>(
        stored: &S::StoredEvent,
        store: &S,
    ) -> Result<Self, EventDecodeError<S::Error>>;
}
