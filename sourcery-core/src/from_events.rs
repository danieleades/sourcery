//! Base trait for objects which can be instantiated from a stream of events
//! (aggregates and projections).

use crate::store::EventFilter;

pub trait FromEvents {
    /// The IDs needed to identify events associated with this object.
    ///
    /// For an aggregate or projection associated with a single aggregate
    /// instance, this could be a single ID.
    ///
    /// For cross-aggregate projections this can be a tuple of aggregate IDs.
    ///
    /// For "singleton" projections (which consume events globally across all
    /// aggregates of a particular kind) this should be `()`.
    type InstanceId;

    /// Aggregate identifier type stored on events (the event stream key).
    type Id;

    /// Store position type used for snapshot-based loading.
    type Position;

    /// Return filters describing the event streams this object consumes.
    fn filters(instance_id: Self::InstanceId) -> Filter<Self::Id, Self::Position>;
}

/// Event filter set which matches on any event that this object consumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter<Id, Pos = ()> {
    pub filters: Vec<EventFilter<Id, Pos>>,
}
