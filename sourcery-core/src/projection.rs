//! Read-side primitives.
//!
//! Projections rebuild query models from streams of stored events. This module
//! provides the projection trait, event application hooks via
//! [`ApplyProjection`], and the [`Filters`] builder that wires everything
//! together.
use std::collections::HashMap;

use thiserror::Error;

use crate::{
    aggregate::Aggregate,
    event::{DomainEvent, EventDecodeError, ProjectionEvent},
    store::{EventFilter, EventStore, StoredEvent},
};

/// Base trait for types that subscribe to events.
///
/// The `filters()` method returns both the event filters AND the handlers
/// needed to process those events. It is generic over the store type `S`,
/// allowing the same projection to work with any store that shares the
/// same `Id` type.
///
/// `filters()` must be **pure and deterministic**: given the same
/// `instance_id`, it must always return the same filter set.
///
/// `init()` constructs a fresh instance from the instance identifier.
/// This replaces the `Default` constraint, allowing instance-aware
/// projections to capture their identity at construction time.
// ANCHOR: subscribable_trait
pub trait Subscribable: Sized {
    /// Aggregate identifier type this subscriber is compatible with.
    type Id;
    /// Instance identifier for this subscriber.
    ///
    /// For singleton subscribers use `()`.
    type InstanceId;
    /// Metadata type expected by this subscriber's handlers.
    type Metadata;

    /// Construct a fresh instance from the instance identifier.
    ///
    /// For singleton projections (`InstanceId = ()`), this typically
    /// delegates to `Self::default()`. For instance projections, this
    /// captures the instance identifier at construction time.
    fn init(instance_id: &Self::InstanceId) -> Self;

    /// Build the filter set and handler map for this subscriber.
    fn filters<S>(instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = Self::Id>,
        S::Metadata: Clone + Into<Self::Metadata>;
}
// ANCHOR_END: subscribable_trait

/// Trait implemented by read models that can be constructed from an event
/// stream.
///
/// Extends [`Subscribable`] with a stable `KIND` identifier for snapshot
/// storage. Derivable via `#[derive(Projection)]`.
// ANCHOR: projection_trait
pub trait Projection: Subscribable {
    /// Stable identifier for this projection type.
    const KIND: &'static str;
}
// ANCHOR_END: projection_trait

/// Apply an event to a projection with access to envelope context.
///
/// Implementations receive the aggregate identifier, the pure domain event,
/// and metadata supplied by the backing store.
///
/// ```ignore
/// impl ApplyProjection<InventoryAdjusted> for InventoryReport {
///     fn apply_projection(&mut self, aggregate_id: &Self::Id, event: &InventoryAdjusted, _metadata: &Self::Metadata) {
///         let stats = self.products.entry(aggregate_id.clone()).or_default();
///         stats.quantity += event.delta;
///     }
/// }
/// ```
// ANCHOR: apply_projection_trait
pub trait ApplyProjection<E>: Subscribable {
    fn apply_projection(&mut self, aggregate_id: &Self::Id, event: &E, metadata: &Self::Metadata);
}
// ANCHOR_END: apply_projection_trait

/// Errors that can occur when rebuilding a projection.
#[derive(Debug, Error)]
pub enum ProjectionError<StoreError>
where
    StoreError: std::error::Error + 'static,
{
    #[error("failed to load events: {0}")]
    Store(#[source] StoreError),
    #[error("failed to decode event: {0}")]
    EventDecode(#[source] EventDecodeError<StoreError>),
}

/// Internal error type for event handler closures.
#[derive(Debug)]
pub(crate) enum HandlerError<StoreError> {
    EventDecode(EventDecodeError<StoreError>),
    Store(StoreError),
}

impl<StoreError> From<StoreError> for HandlerError<StoreError> {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Type alias for event handler closures used by [`Filters`].
pub(crate) type EventHandler<P, S> = Box<
    dyn Fn(
            &mut P,
            &<S as EventStore>::Id,
            &StoredEvent<
                <S as EventStore>::Id,
                <S as EventStore>::Position,
                <S as EventStore>::Data,
                <S as EventStore>::Metadata,
            >,
            &<S as EventStore>::Metadata,
            &S,
        ) -> Result<(), HandlerError<<S as EventStore>::Error>>
        + Send
        + Sync,
>;

/// Positioned filters and handler map, ready for event loading.
pub(crate) type PositionedFilters<S, P> = (
    Vec<EventFilter<<S as EventStore>::Id, <S as EventStore>::Position>>,
    HashMap<&'static str, EventHandler<P, S>>,
);

/// Combined filter configuration and handler map for a subscriber.
///
/// `Filters` captures both the event filters (which events to load) and the
/// handler closures (how to apply them). It is parameterized by the store
/// type `S` to enable store-mediated decoding.
///
/// Filters are constructed without positions. Positions are applied at
/// load/subscribe time.
pub struct Filters<S, P>
where
    S: EventStore,
{
    pub(crate) specs: Vec<EventFilter<S::Id, ()>>,
    pub(crate) handlers: HashMap<&'static str, EventHandler<P, S>>,
}

impl<S, P> Default for Filters<S, P>
where
    S: EventStore,
    P: Subscribable<Id = S::Id>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, P> Filters<S, P>
where
    S: EventStore,
    P: Subscribable<Id = S::Id>,
{
    /// Create an empty filter set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            specs: Vec::new(),
            handlers: HashMap::new(),
        }
    }

    /// Subscribe to a specific event type globally (all aggregates).
    #[must_use]
    pub fn event<E>(mut self) -> Self
    where
        E: DomainEvent + serde::de::DeserializeOwned,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.specs.push(EventFilter::for_event(E::KIND));
        self.handlers.insert(
            E::KIND,
            Box::new(|proj, agg_id, stored, metadata, store| {
                let event: E = store.decode_event(stored)?;
                let metadata_converted: P::Metadata = metadata.clone().into();
                ApplyProjection::apply_projection(proj, agg_id, &event, &metadata_converted);
                Ok(())
            }),
        );
        self
    }

    /// Subscribe to all event kinds supported by a [`ProjectionEvent`] sum
    /// type across all aggregates.
    #[must_use]
    pub fn events<E>(mut self) -> Self
    where
        E: ProjectionEvent,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        for &kind in E::EVENT_KINDS {
            self.specs.push(EventFilter::for_event(kind));
            self.handlers.insert(
                kind,
                Box::new(move |proj, agg_id, stored, metadata, store| {
                    let event = E::from_stored(stored, store).map_err(HandlerError::EventDecode)?;
                    let metadata_converted: P::Metadata = metadata.clone().into();
                    ApplyProjection::apply_projection(proj, agg_id, &event, &metadata_converted);
                    Ok(())
                }),
            );
        }
        self
    }

    /// Subscribe to a specific event type from a specific aggregate instance.
    #[must_use]
    pub fn event_for<A, E>(mut self, aggregate_id: &S::Id) -> Self
    where
        A: Aggregate<Id = S::Id>,
        E: DomainEvent + serde::de::DeserializeOwned,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.specs.push(EventFilter::for_aggregate(
            E::KIND,
            A::KIND,
            aggregate_id.clone(),
        ));
        self.handlers.insert(
            E::KIND,
            Box::new(|proj, agg_id, stored, metadata, store| {
                let event: E = store.decode_event(stored)?;
                let metadata_converted: P::Metadata = metadata.clone().into();
                ApplyProjection::apply_projection(proj, agg_id, &event, &metadata_converted);
                Ok(())
            }),
        );
        self
    }

    /// Subscribe to all events from a specific aggregate instance.
    #[must_use]
    pub fn events_for<A>(mut self, aggregate_id: &S::Id) -> Self
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
        P: ApplyProjection<A::Event>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        for &kind in <A::Event as ProjectionEvent>::EVENT_KINDS {
            self.specs.push(EventFilter::for_aggregate(
                kind,
                A::KIND,
                aggregate_id.clone(),
            ));
            self.handlers.insert(
                kind,
                Box::new(move |proj, agg_id, stored, metadata, store| {
                    let event = <A::Event as ProjectionEvent>::from_stored(stored, store)
                        .map_err(HandlerError::EventDecode)?;
                    let metadata_converted: P::Metadata = metadata.clone().into();
                    ApplyProjection::apply_projection(proj, agg_id, &event, &metadata_converted);
                    Ok(())
                }),
            );
        }
        self
    }

    /// Convert positionless filter specs into positioned [`EventFilter`]s.
    pub(crate) fn into_event_filters(self, after: Option<&S::Position>) -> PositionedFilters<S, P> {
        let filters = self
            .specs
            .into_iter()
            .map(|spec| {
                let mut filter = EventFilter {
                    event_kind: spec.event_kind,
                    aggregate_kind: spec.aggregate_kind,
                    aggregate_id: spec.aggregate_id,
                    after_position: None,
                };
                if let Some(pos) = after {
                    filter = filter.after(pos.clone());
                }
                filter
            })
            .collect();
        (filters, self.handlers)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::*;

    #[test]
    fn error_display_store_mentions_loading() {
        let error: ProjectionError<io::Error> =
            ProjectionError::Store(io::Error::new(io::ErrorKind::NotFound, "not found"));
        let msg = error.to_string();
        assert!(msg.contains("failed to load events"));
        assert!(error.source().is_some());
    }
}
