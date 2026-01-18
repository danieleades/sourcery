//! In-memory snapshot store implementation.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, RwLock},
};

use serde::{Serialize, de::DeserializeOwned};

use super::{OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore};

/// Snapshot store policy for when to accept snapshot offers.
///
/// This policy is used by snapshot store implementations to determine when to
/// persist snapshots:
///
/// - [`SnapshotPolicy::Always`]: Snapshot after every command (high storage
///   cost, minimal replay)
/// - [`SnapshotPolicy::EveryNEvents`]: Snapshot every N events (balanced
///   approach)
/// - [`SnapshotPolicy::Never`]: Don't persist snapshots (load-only mode)
///
/// # Guidelines
///
/// Choose based on your aggregate's characteristics:
///
/// ## Use `Always` when:
/// - Event replay is expensive (complex business logic per event)
/// - Aggregates accumulate many events (hundreds or thousands)
/// - Storage cost is less important than read performance
///
/// ## Use `EveryNEvents(n)` when:
/// - Balancing storage cost vs. replay cost
/// - Aggregates have moderate event counts
/// - Start with n=50-100 and tune based on profiling
///
/// ## Use `Never` when:
/// - Running a read replica that consumes snapshots created elsewhere
/// - Aggregates are short-lived (few events per instance)
/// - Managing snapshots through an external process
/// - Testing without snapshot overhead
///
/// # Example
///
/// ```ignore
/// use sourcery::snapshot::inmemory::SnapshotPolicy;
///
/// // Use in custom snapshot store implementations
/// impl MySnapshotStore {
///     pub fn new(policy: SnapshotPolicy) -> Self {
///         Self { policy }
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub enum SnapshotPolicy {
    /// Create a snapshot after every command.
    Always,
    /// Create a snapshot every N events.
    EveryNEvents(u64),
    /// Never create snapshots (load-only mode).
    Never,
}

impl SnapshotPolicy {
    /// Check if a snapshot should be created based on events since last
    /// snapshot.
    #[must_use]
    pub const fn should_snapshot(&self, events_since: u64) -> bool {
        match self {
            Self::Always => true,
            Self::EveryNEvents(threshold) => events_since >= *threshold,
            Self::Never => false,
        }
    }
}

/// Error type for in-memory snapshot store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl Error {
    fn serialization(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Serialization(Box::new(err))
    }

    fn deserialization(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Deserialization(Box::new(err))
    }
}

/// In-memory snapshot store with configurable policy.
///
/// This is a reference implementation suitable for testing and development.
/// Production systems should implement [`SnapshotStore`] with durable storage.
///
/// Keys are derived from `(kind, id)` directly, so `Id: Eq + Hash + Clone` is
/// required for this store.
///
/// Generic over `Pos` to match the `EventStore` position type.
///
/// # Example
///
/// ```ignore
/// use sourcery::{Repository, store::inmemory, snapshot::inmemory};
///
/// let repo = Repository::new(store::inmemory::Store::new())
///     .with_snapshots(snapshot::inmemory::Store::every(100));
/// ```
type SnapshotMap<Id, Pos> = HashMap<SnapshotKey<Id>, Snapshot<Pos, serde_json::Value>>;
type SharedSnapshots<Id, Pos> = Arc<RwLock<SnapshotMap<Id, Pos>>>;

#[derive(Clone, Debug)]
pub struct Store<Id, Pos> {
    snapshots: SharedSnapshots<Id, Pos>,
    policy: SnapshotPolicy,
}

impl<Id, Pos> Store<Id, Pos> {
    /// Create a snapshot store that saves after every command.
    ///
    /// Best for aggregates with expensive replay or many events.
    /// See the policy guidelines above for choosing an appropriate cadence.
    #[must_use]
    pub fn always() -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            policy: SnapshotPolicy::Always,
        }
    }

    /// Create a snapshot store that saves every N events.
    ///
    /// Recommended for most use cases. Start with `n = 50-100` and tune
    /// based on your aggregate's replay cost.
    /// See the policy guidelines above for choosing a policy.
    #[must_use]
    pub fn every(n: u64) -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            policy: SnapshotPolicy::EveryNEvents(n),
        }
    }

    /// Create a snapshot store that never saves (load-only).
    ///
    /// Use for read replicas, short-lived aggregates, or when managing
    /// snapshots externally. See the policy guidelines above for when this
    /// fits.
    #[must_use]
    pub fn never() -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            policy: SnapshotPolicy::Never,
        }
    }
}

impl<Id, Pos> Default for Store<Id, Pos> {
    fn default() -> Self {
        Self::always()
    }
}

impl<Id, Pos> SnapshotStore<Id> for Store<Id, Pos>
where
    Id: Clone + Eq + Hash + Send + Sync,
    Pos: Clone + Ord + Send + Sync,
{
    type Error = Error;
    type Position = Pos;

    #[tracing::instrument(skip(self, id))]
    async fn load<T>(&self, kind: &str, id: &Id) -> Result<Option<Snapshot<Pos, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        let key = SnapshotKey::new(kind, id.clone());
        let stored = {
            let snapshots = self.snapshots.read().expect("snapshot store lock poisoned");
            snapshots.get(&key).cloned()
        };
        let snapshot = match stored {
            Some(snapshot) => {
                let data = serde_json::from_value(snapshot.data.clone())
                    .map_err(Error::deserialization)?;
                Some(Snapshot {
                    position: snapshot.position,
                    data,
                })
            }
            None => None,
        };
        tracing::trace!(found = snapshot.is_some(), "snapshot lookup");
        Ok(snapshot)
    }

    #[tracing::instrument(skip(self, id, create_snapshot))]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        kind: &str,
        id: &Id,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Pos, T>, CE>,
    {
        if !self.policy.should_snapshot(events_since_last_snapshot) {
            return Ok(SnapshotOffer::Declined);
        }

        let snapshot = match create_snapshot() {
            Ok(snapshot) => snapshot,
            Err(e) => return Err(OfferSnapshotError::Create(e)),
        };
        let data = serde_json::to_value(&snapshot.data)
            .map_err(|e| OfferSnapshotError::Snapshot(Error::serialization(e)))?;
        let key = SnapshotKey::new(kind, id.clone());
        let stored = Snapshot {
            position: snapshot.position,
            data,
        };

        let offer = {
            let mut snapshots = self
                .snapshots
                .write()
                .expect("snapshot store lock poisoned");
            match snapshots.get(&key) {
                Some(existing) if existing.position >= stored.position => SnapshotOffer::Declined,
                _ => {
                    snapshots.insert(key, stored);
                    SnapshotOffer::Stored
                }
            }
        };

        tracing::debug!(
            events_since_last_snapshot,
            ?offer,
            "snapshot offer evaluated"
        );
        Ok(offer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct SnapshotKey<Id> {
    kind: String,
    id: Id,
}

impl<Id> SnapshotKey<Id> {
    fn new(kind: &str, id: Id) -> Self {
        Self {
            kind: kind.to_string(),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    #[test]
    fn always_should_snapshot() {
        let policy = SnapshotPolicy::Always;
        assert!(policy.should_snapshot(0));
        assert!(policy.should_snapshot(1));
        assert!(policy.should_snapshot(100));
    }

    #[test]
    fn every_n_at_threshold() {
        let policy = SnapshotPolicy::EveryNEvents(3);
        assert!(policy.should_snapshot(3));
        assert!(policy.should_snapshot(4));
        assert!(policy.should_snapshot(100));
    }

    #[test]
    fn every_n_below_threshold() {
        let policy = SnapshotPolicy::EveryNEvents(3);
        assert!(!policy.should_snapshot(0));
        assert!(!policy.should_snapshot(1));
        assert!(!policy.should_snapshot(2));
    }

    #[test]
    fn never_should_snapshot() {
        let policy = SnapshotPolicy::Never;
        assert!(!policy.should_snapshot(0));
        assert!(!policy.should_snapshot(1));
        assert!(!policy.should_snapshot(100));
    }

    #[tokio::test]
    async fn load_returns_none_for_missing() {
        let store = Store::<String, u64>::always();
        let result: Option<Snapshot<u64, String>> =
            store.load("test", &"id".to_string()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn load_returns_stored_snapshot() {
        let store = Store::<String, u64>::always();
        let id = "test-id".to_string();

        store
            .offer_snapshot::<Infallible, _, _>("test", &id, 1, || {
                Ok(Snapshot {
                    position: 5,
                    data: "snapshot-data".to_string(),
                })
            })
            .await
            .unwrap();

        let loaded: Snapshot<u64, String> = store.load("test", &id).await.unwrap().unwrap();
        assert_eq!(loaded.position, 5);
        assert_eq!(loaded.data, "snapshot-data");
    }

    #[tokio::test]
    async fn offer_declines_older_position() {
        let store = Store::<String, u64>::always();
        let id = "test-id".to_string();

        // Store initial snapshot at position 10
        store
            .offer_snapshot::<Infallible, _, _>("test", &id, 1, || {
                Ok(Snapshot {
                    position: 10,
                    data: "first",
                })
            })
            .await
            .unwrap();

        // Try to store older snapshot at position 5 - should be declined
        let result = store
            .offer_snapshot::<Infallible, _, _>("test", &id, 1, || {
                Ok(Snapshot {
                    position: 5,
                    data: "older",
                })
            })
            .await
            .unwrap();

        assert_eq!(result, SnapshotOffer::Declined);

        // Verify original snapshot is still there
        let loaded: Snapshot<u64, String> = store.load("test", &id).await.unwrap().unwrap();
        assert_eq!(loaded.position, 10);
        assert_eq!(loaded.data, "first");
    }

    #[test]
    fn default_is_always() {
        let store = Store::<String, u64>::default();
        assert!(matches!(store.policy, SnapshotPolicy::Always));
    }
}
