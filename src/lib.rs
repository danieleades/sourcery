#![doc = include_str!("../README.md")]

#[cfg(feature = "test-util")]
pub use sourcery_core::test;
pub use sourcery_core::{
    aggregate,
    aggregate::{Aggregate, Apply, Handle},
    event,
    event::{DomainEvent, EventDecodeError, EventKind, ProjectionEvent},
    projection,
    projection::{ApplyProjection, Projection},
    repository,
    repository::{
        OptimisticRepository, OptimisticSnapshotRepository, Repository, UncheckedRepository,
    },
};
// Re-export proc macro derives so consumers only depend on `sourcery`.
pub use sourcery_macros::{Aggregate, Projection};

pub mod store {

    // Re-export low-level commit types for EventStore implementors only.
    // Most users should interact with the Repository API instead.
    #[doc(hidden)]
    pub use sourcery_core::store::{CommitError, CommitResult, OptimisticCommitError};
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
