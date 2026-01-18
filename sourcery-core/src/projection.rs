//! Read-side primitives.
//!
//! Projections rebuild query models from streams of stored events. This module
//! provides the projection trait, event application hooks via
//! [`ApplyProjection`], and the [`ProjectionBuilder`] that wires everything
//! together.
use std::{collections::HashMap, marker::PhantomData};

use thiserror::Error;

use crate::{
    aggregate::Aggregate,
    event::{DomainEvent, EventDecodeError, ProjectionEvent},
    snapshot::{Snapshot, SnapshotStore},
    store::{EventFilter, EventStore, GloballyOrderedStore, StoredEvent},
};

/// Trait implemented by read models that can be constructed from an event
/// stream.
///
/// Implementors specify the identifier and metadata types their
/// [`ApplyProjection`] handlers expect. Projections are typically rebuilt by
/// calling [`Repository::build_projection`] and configuring the desired event
/// streams before invoking [`ProjectionBuilder::load`].
// ANCHOR: projection_trait
pub trait Projection: Default {
    /// Stable identifier for this projection type.
    const KIND: &'static str;
    /// Aggregate identifier type this projection is compatible with.
    type Id;
    /// Metadata type expected by this projection
    type Metadata;
    /// Projection instance identifier
    ///
    /// For 'singleton' projections (those for which there is only one global
    /// projection with it's 'KIND') use `()`.
    type InstanceId;
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
pub trait ApplyProjection<E>: Projection {
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

/// Type alias for event handler closures.
#[derive(Debug)]
enum HandlerError<StoreError> {
    EventDecode(EventDecodeError<StoreError>),
    Store(StoreError),
}

impl<StoreError> From<StoreError> for HandlerError<StoreError> {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

type EventHandler<P, S> = Box<
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

/// Builder used to configure which events should be loaded for a projection.
pub struct ProjectionBuilder<'a, S, P, SS, Snap = NoSnapshot>
where
    S: EventStore,
    P: Projection<Id = S::Id>,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
{
    store: &'a S,
    snapshots: &'a SS,
    /// Event kind -> handler mapping for O(1) dispatch
    handlers: HashMap<&'a str, EventHandler<P, S>>,
    /// Filters for loading events from the store
    filters: Vec<EventFilter<S::Id, S::Position>>,
    _phantom: PhantomData<fn() -> (P, Snap)>,
}

/// Type-level marker indicating no snapshot support.
///
/// This is an implementation detail of [`ProjectionBuilder`]'s type-state
/// pattern. You should never need to name this type directly - use the builder
/// methods instead.
#[doc(hidden)]
pub struct NoSnapshot;

/// Type-level marker indicating snapshot support is enabled.
///
/// This is an implementation detail of [`ProjectionBuilder`]'s type-state
/// pattern. You should never need to name this type directly - use the builder
/// methods instead.
#[doc(hidden)]
pub struct WithSnapshot;

impl<'a, S, P, SS, Snap> ProjectionBuilder<'a, S, P, SS, Snap>
where
    S: EventStore,
    P: Projection<Id = S::Id>,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
{
    pub(super) fn new(store: &'a S, snapshots: &'a SS) -> Self {
        Self {
            store,
            snapshots,
            handlers: HashMap::new(),
            filters: Vec::new(),
            _phantom: PhantomData,
        }
    }

    /// Register a specific event type to load from all aggregates.
    ///
    /// # Type Constraints
    ///
    /// The store's metadata type must be convertible to the projection's
    /// metadata type. `Clone` is required because event handlers receive
    /// metadata by reference, but `Into::into()` requires ownership. The
    /// metadata is cloned once per event.
    ///
    /// # Example
    /// ```ignore
    /// builder.event::<ProductRestocked>()  // All products
    /// ```
    #[must_use]
    pub fn event<E>(mut self) -> Self
    where
        E: DomainEvent + serde::de::DeserializeOwned,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.filters.push(EventFilter::for_event(E::KIND));
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

    /// Register all event kinds supported by a `ProjectionEvent` sum type
    /// across all aggregates.
    ///
    /// This is primarily intended for subscribing to an aggregate's generated
    /// event enum (`A::Event` from `#[derive(Aggregate)]`) as a single
    /// "unit", rather than registering each `DomainEvent` type
    /// individually.
    ///
    /// # Example
    /// ```ignore
    /// builder.events::<AccountEvent>() // All accounts, all account event variants
    /// ```
    #[must_use]
    pub fn events<E>(mut self) -> Self
    where
        E: ProjectionEvent,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        for &kind in E::EVENT_KINDS {
            self.filters.push(EventFilter::for_event(kind));
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

    /// Register a specific event type to load from a specific aggregate
    /// instance.
    ///
    /// Use this when you only care about a single event kind. If you want to
    /// subscribe to an aggregate's full event enum (`A::Event`), prefer
    /// [`ProjectionBuilder::events_for`].
    ///
    /// # Example
    /// ```ignore
    /// builder.event_for::<Account, FundsDeposited>(&account_id); // One account stream
    /// ```
    #[must_use]
    pub fn event_for<A, E>(mut self, aggregate_id: &S::Id) -> Self
    where
        A: Aggregate<Id = S::Id>,
        E: DomainEvent + serde::de::DeserializeOwned,
        P: ApplyProjection<E>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.filters.push(EventFilter::for_aggregate(
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

    /// Register all event kinds for a specific aggregate instance.
    ///
    /// This subscribes the projection to the aggregate's event sum type
    /// (`A::Event`) and loads all events in that stream that correspond to
    /// `A::Event::EVENT_KINDS`.
    ///
    /// # Example
    /// ```ignore
    /// let history = repository
    ///     .build_projection::<AccountHistory>()
    ///     .events_for::<Account>(&account_id)
    ///     .load()?;
    /// ```
    #[must_use]
    pub fn events_for<A>(mut self, aggregate_id: &S::Id) -> Self
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
        P: ApplyProjection<A::Event>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        for &kind in <A::Event as ProjectionEvent>::EVENT_KINDS {
            self.filters.push(EventFilter::for_aggregate(
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
}

impl<'a, S, P, SS> ProjectionBuilder<'a, S, P, SS, NoSnapshot>
where
    S: EventStore,
    P: Projection<Id = S::Id>,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
{
    /// Enable snapshot loading/saving for this projection.
    #[must_use]
    pub fn with_snapshot(self) -> ProjectionBuilder<'a, S, P, SS, WithSnapshot> {
        ProjectionBuilder {
            store: self.store,
            snapshots: self.snapshots,
            handlers: self.handlers,
            filters: self.filters,
            _phantom: PhantomData,
        }
    }

    /// Replays the configured events and materializes the projection.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialized.
    #[tracing::instrument(
        skip(self),
        fields(
            projection_type = std::any::type_name::<P>(),
            filter_count = self.filters.len(),
            handler_count = self.handlers.len()
        )
    )]
    pub async fn load(self) -> Result<P, ProjectionError<S::Error>>
    where
        P: Projection<InstanceId = ()>,
    {
        self.load_for(&()).await
    }

    /// Replays the configured events for a specific projection instance.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialized.
    #[tracing::instrument(
        skip(self, instance_id),
        fields(
            projection_type = std::any::type_name::<P>(),
            filter_count = self.filters.len(),
            handler_count = self.handlers.len()
        )
    )]
    pub async fn load_for(
        self,
        instance_id: &P::InstanceId,
    ) -> Result<P, ProjectionError<S::Error>> {
        tracing::debug!("loading projection");

        let events = self
            .store
            .load_events(&self.filters)
            .await
            .map_err(ProjectionError::Store)?;
        let mut projection = P::default();

        let event_count = events.len();
        tracing::debug!(
            events_to_replay = event_count,
            "replaying events into projection"
        );

        for stored in &events {
            let aggregate_id = stored.aggregate_id();
            let kind = stored.kind();
            let metadata = stored.metadata();

            // O(1) handler lookup instead of O(n) linear scan
            if let Some(handler) = self.handlers.get(kind) {
                (handler)(&mut projection, aggregate_id, stored, metadata, self.store).map_err(
                    |error| match error {
                        HandlerError::Store(error) => {
                            ProjectionError::EventDecode(EventDecodeError::Store(error))
                        }
                        HandlerError::EventDecode(error) => ProjectionError::EventDecode(error),
                    },
                )?;
            }
        }

        tracing::info!(events_applied = event_count, "projection loaded");
        let _ = instance_id;
        Ok(projection)
    }
}

impl<S, P, SS> ProjectionBuilder<'_, S, P, SS, WithSnapshot>
where
    S: EventStore + GloballyOrderedStore,
    P: Projection<Id = S::Id> + serde::Serialize + serde::de::DeserializeOwned + Sync,
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
    S::Position: Ord,
    P::InstanceId: Sync,
{
    /// Replays the configured events and materializes the projection.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialized.
    #[tracing::instrument(
        skip(self),
        fields(
            projection_type = std::any::type_name::<P>(),
            filter_count = self.filters.len(),
            handler_count = self.handlers.len()
        )
    )]
    pub async fn load(self) -> Result<P, ProjectionError<S::Error>>
    where
        P: Projection<InstanceId = ()>,
    {
        self.load_for(&()).await
    }

    /// Replays the configured events for a specific projection instance,
    /// using snapshots when available.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialized.
    #[tracing::instrument(
        skip(self, instance_id),
        fields(
            projection_type = std::any::type_name::<P>(),
            filter_count = self.filters.len(),
            handler_count = self.handlers.len()
        )
    )]
    pub async fn load_for(
        self,
        instance_id: &P::InstanceId,
    ) -> Result<P, ProjectionError<S::Error>> {
        tracing::debug!("loading projection");

        let snapshot_result = self
            .snapshots
            .load::<P>(P::KIND, instance_id)
            .await
            .inspect_err(|e| {
                tracing::error!(error = %e, "failed to load projection snapshot");
            })
            .ok()
            .flatten();

        let (mut projection, snapshot_position) = if let Some(snapshot) = snapshot_result {
            (snapshot.data, Some(snapshot.position))
        } else {
            (P::default(), None)
        };

        let filters = if let Some(position) = snapshot_position {
            self.filters
                .into_iter()
                .map(|filter| filter.after(position.clone()))
                .collect::<Vec<_>>()
        } else {
            self.filters
        };

        let events = self
            .store
            .load_events(&filters)
            .await
            .map_err(ProjectionError::Store)?;

        let event_count = events.len();
        let mut last_position = None;

        for stored in &events {
            let aggregate_id = stored.aggregate_id();
            let kind = stored.kind();
            let metadata = stored.metadata();
            let position = stored.position();

            if let Some(handler) = self.handlers.get(kind) {
                (handler)(&mut projection, aggregate_id, stored, metadata, self.store).map_err(
                    |error| match error {
                        HandlerError::Store(error) => {
                            ProjectionError::EventDecode(EventDecodeError::Store(error))
                        }
                        HandlerError::EventDecode(error) => ProjectionError::EventDecode(error),
                    },
                )?;
            }
            last_position = Some(position);
        }

        if event_count > 0
            && let Some(position) = last_position
        {
            let projection_ref = &projection;
            let offer = self.snapshots.offer_snapshot(
                P::KIND,
                instance_id,
                event_count as u64,
                move || -> Result<Snapshot<S::Position, &P>, std::convert::Infallible> {
                    Ok(Snapshot {
                        position,
                        data: projection_ref,
                    })
                },
            );

            if let Err(e) = offer.await {
                tracing::error!(error = %e, "failed to store projection snapshot");
            }
        }

        tracing::info!(events_applied = event_count, "projection loaded");
        Ok(projection)
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
