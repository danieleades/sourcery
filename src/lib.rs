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
    repository::{OptimisticRepository, OptimisticSnapshotRepository, Repository, UncheckedRepository},
};
// Re-export proc macro derives so consumers only depend on `sourcery`.
pub use sourcery_macros::{Aggregate, Projection};

pub mod store {

    pub use sourcery_core::store::{
        EventFilter, EventStore, GloballyOrderedStore, NonEmpty, StoredEventView,
        Transaction,
    };

    // Re-export low-level append types for EventStore implementors only.
    // Most users should interact with the Repository API instead.
    #[doc(hidden)]
    pub use sourcery_core::store::{AppendError, AppendResult};

    #[cfg(feature = "postgres")]
    #[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
    pub mod postgres {
        pub use sourcery_postgres::{Error, Store};
    }

    pub use sourcery_core::store::inmemory;
}

pub mod snapshot {

    pub use sourcery_core::snapshot::{
        NoSnapshots, OfferSnapshotError, Snapshot, SnapshotOffer,
        SnapshotStore,
    };

    pub use sourcery_core::snapshot::inmemory;

    #[cfg(feature = "postgres")]
    #[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
    pub mod postgres {
        pub use sourcery_postgres::snapshot::{Error, Store};
    }
}
