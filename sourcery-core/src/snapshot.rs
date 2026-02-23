//! Snapshot support for optimised aggregate loading.
//!
//! Snapshots persist aggregate state at a point in time, reducing the number of
//! events that need to be replayed when loading an aggregate. This module
//! provides:
//!
//! - [`Snapshot`] - Point-in-time aggregate state
//! - [`SnapshotStore`] - Trait for snapshot persistence with policy
//! - [`NoSnapshots`] - No-op implementation; this is the default when
//!   [`Repository::with_snapshots`](crate::repository::Repository::with_snapshots)
//!   is not called
//! - [`inmemory`] - In-memory reference implementation with configurable policy

use std::convert::Infallible;

use serde::{Serialize, de::DeserializeOwned};

pub mod inmemory;
/// Point-in-time snapshot of aggregate state.
///
/// The `position` field indicates the event stream position when this snapshot
/// was taken. When loading an aggregate, only events after this position need
/// to be replayed.
///
/// Schema evolution is handled at the serialisation layer (e.g., via
/// `serde_evolve`), so no version field is needed here.
///
/// # Type Parameters
///
/// - `Pos`: The position type used by the event store (e.g., `u64`, `i64`,
///   etc.)
/// - `Data`: The snapshot payload type.
#[derive(Clone, Debug)]
pub struct Snapshot<Pos, Data> {
    /// Event position when this snapshot was taken.
    pub position: Pos,
    /// Snapshot payload.
    pub data: Data,
}

/// Trait for snapshot persistence with built-in policy.
///
/// Implementations decide both *how* to store snapshots and *when* to store
/// them. The repository calls [`offer_snapshot`](SnapshotStore::offer_snapshot)
/// after each successful command execution to decide whether to create and
/// persist a new snapshot.
///
/// # Example Implementations
///
/// - Always save: useful for aggregates with expensive replay
/// - Every N events: balance between storage and replay cost
/// - Never save: read-only replicas that only load snapshots created elsewhere
// ANCHOR: snapshot_store_trait
pub trait SnapshotStore<Id: Sync>: Send + Sync {
    /// Position type for tracking snapshot positions.
    ///
    /// Must match the `EventStore::Position` type used in the same repository.
    type Position: Send + Sync;

    /// Error type for snapshot operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Load the most recent snapshot for an aggregate.
    ///
    /// Returns `Ok(None)` if no snapshot exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying storage fails.
    fn load<T>(
        &self,
        kind: &str,
        id: &Id,
    ) -> impl std::future::Future<Output = Result<Option<Snapshot<Self::Position, T>>, Self::Error>> + Send
    where
        T: DeserializeOwned;

    /// Whether to store a snapshot, with lazy snapshot creation.
    ///
    /// The repository calls this after successfully appending new events,
    /// passing `events_since_last_snapshot` and a `create_snapshot`
    /// callback. Implementations may decline without invoking
    /// `create_snapshot`, avoiding unnecessary snapshot creation cost
    /// (serialisation, extra I/O, etc.).
    ///
    /// Returning [`SnapshotOffer::Stored`] indicates that the snapshot was
    /// persisted. Returning [`SnapshotOffer::Declined`] indicates that no
    /// snapshot was stored.
    ///
    /// # Errors
    ///
    /// Returns [`OfferSnapshotError::Create`] if `create_snapshot` fails.
    /// Returns [`OfferSnapshotError::Snapshot`] if persistence fails.
    fn offer_snapshot<CE, T, Create>(
        &self,
        kind: &str,
        id: &Id,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> impl std::future::Future<
        Output = Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>,
    > + Send
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE> + Send;
}
// ANCHOR_END: snapshot_store_trait

/// Result of offering a snapshot to a store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotOffer {
    /// The snapshot store declined to store a snapshot.
    Declined,
    /// The snapshot store stored the snapshot.
    Stored,
}

/// Error returned by [`SnapshotStore::offer_snapshot`].
#[derive(Debug, thiserror::Error)]
pub enum OfferSnapshotError<SnapshotError, CreateError>
where
    SnapshotError: std::error::Error + 'static,
    CreateError: std::error::Error + 'static,
{
    /// Snapshot creation failed (e.g., serialisation, extra I/O, etc.).
    #[error("failed to create snapshot: {0}")]
    Create(#[source] CreateError),
    /// Snapshot persistence failed.
    #[error("snapshot operation failed: {0}")]
    Snapshot(#[source] SnapshotError),
}

/// No-op snapshot store for backwards compatibility.
///
/// This implementation:
/// - Always returns `None` from `load()`
/// - Silently discards all offered snapshots
///
/// Use this as the default when snapshots are not needed.
///
/// Generic over `Pos` to match the `EventStore` position type.
#[derive(Clone, Debug, Default)]
pub struct NoSnapshots<Pos>(std::marker::PhantomData<Pos>);

impl<Pos> NoSnapshots<Pos> {
    /// Create a new no-op snapshot store.
    #[must_use]
    pub const fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<Id, Pos> SnapshotStore<Id> for NoSnapshots<Pos>
where
    Id: Send + Sync,
    Pos: Send + Sync,
{
    type Error = Infallible;
    type Position = Pos;

    async fn load<T>(&self, _kind: &str, _id: &Id) -> Result<Option<Snapshot<Pos, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        Ok(None)
    }

    async fn offer_snapshot<CE, T, Create>(
        &self,
        _kind: &str,
        _id: &Id,
        _events_since_last_snapshot: u64,
        _create_snapshot: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Pos, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::*;

    #[tokio::test]
    async fn no_snapshots_load_returns_none() {
        let store = NoSnapshots::<u64>::new();
        let result: Option<Snapshot<u64, String>> = store.load("test", &"id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn no_snapshots_offer_declines() {
        let store = NoSnapshots::<u64>::new();
        let result = store
            .offer_snapshot::<io::Error, _, _>("test", &"id", 100, || {
                Ok(Snapshot {
                    position: 1,
                    data: "data",
                })
            })
            .await
            .unwrap();

        assert_eq!(result, SnapshotOffer::Declined);
    }

    #[test]
    fn offer_error_create_displays_source() {
        let err: OfferSnapshotError<io::Error, io::Error> =
            OfferSnapshotError::Create(io::Error::other("create failed"));
        let msg = err.to_string();
        assert!(msg.contains("failed to create snapshot"));
        assert!(err.source().is_some());
    }

    #[test]
    fn offer_error_snapshot_displays_source() {
        let err: OfferSnapshotError<io::Error, io::Error> =
            OfferSnapshotError::Snapshot(io::Error::other("snapshot failed"));
        let msg = err.to_string();
        assert!(msg.contains("snapshot operation failed"));
        assert!(err.source().is_some());
    }
}
