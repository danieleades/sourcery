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
    repository::{DefaultRepository, Repository, SnapshotRepository, UncheckedRepository},
};
// Re-export proc macro derives so consumers only depend on `sourcery`.
pub use sourcery_macros::{Aggregate, Projection};

pub mod store {

    pub use sourcery_core::store::{
        AppendError, AppendResult, EventFilter, EventStore, GloballyOrderedStore,
        NonEmpty, StoredEventView, Transaction,
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
