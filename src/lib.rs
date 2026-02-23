#![doc = include_str!("../README.md")]

#[cfg(feature = "test-util")]
pub use sourcery_core::test;
pub use sourcery_core::{
    aggregate,
    aggregate::{Aggregate, Apply, Create, Handle, HandleCreate},
    event,
    event::{DomainEvent, EventDecodeError, ProjectionEvent},
    projection,
    projection::{ApplyProjection, Filters, Projection},
    repository,
    repository::{
        OptimisticRepository, OptimisticSnapshotRepository, Repository, UncheckedRepository,
    },
    subscription,
    subscription::{
        SubscribableStore, Subscription, SubscriptionBuilder, SubscriptionError, SubscriptionHandle,
    },
};
// Re-export proc macro derives so consumers only depend on `sourcery`.
pub use sourcery_macros::{Aggregate, Projection};

/// Event-store interfaces and built-in store implementations.
pub mod store {

    // Re-export low-level commit types for EventStore implementors only.
    // Most users should interact with the Repository API instead.
    #[doc(hidden)]
    pub use sourcery_core::store::{CommitError, Committed, OptimisticCommitError};
    pub use sourcery_core::store::{
        EventFilter, EventStore, GloballyOrderedStore, NonEmpty, StoredEvent,
    };

    #[cfg(feature = "postgres")]
    #[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
    pub mod postgres {
        pub use sourcery_postgres::{Error, Store};
    }

    pub use sourcery_core::store::inmemory;
}

/// Snapshot interfaces and built-in snapshot store implementations.
pub mod snapshot {

    pub use sourcery_core::snapshot::{
        NoSnapshots, OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore, inmemory,
    };

    #[cfg(feature = "postgres")]
    #[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
    pub mod postgres {
        pub use sourcery_postgres::snapshot::{Error, Store};
    }
}
