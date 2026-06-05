//! Concrete store and repository types for the storefront.
//!
//! All three aggregates share one store keyed by `String`; each aggregate keeps
//! its own id newtype and projects to the raw key via `StorageKey<String>`. The
//! repository uses optimistic concurrency and snapshots aggregate state, so
//! reloading a long-lived `Product` after many stock movements stays cheap.

use sourcery::{
    Repository,
    repository::OptimisticSnapshotRepository,
    store::{EventStore, inmemory},
};

use crate::metadata::RequestContext;

/// The shared in-memory event store: raw key `String`, metadata
/// [`RequestContext`].
pub type AppStore = inmemory::Store<String, RequestContext>;

/// The store's error type, surfaced for naming reactor and subscription
/// handles.
pub type AppError = <AppStore as EventStore>::Error;

/// Snapshot store for aggregate state (positions are `u64` in memory).
pub type AppSnapshots = sourcery::snapshot::inmemory::Store<u64>;

/// The application repository: optimistic concurrency, snapshot-backed.
pub type AppRepo = OptimisticSnapshotRepository<AppStore, AppSnapshots>;

/// Build a fresh repository over an empty store.
#[must_use]
pub fn build_repo() -> AppRepo {
    Repository::new(AppStore::new()).with_snapshots(AppSnapshots::every(50))
}
