//! PostgreSQL-backed snapshot store implementation.
//!
//! This module provides [`Store`], an implementation of
//! [`sourcery_core::snapshot::SnapshotStore`] for `PostgreSQL`.

use sourcery_core::snapshot::{
    inmemory::SnapshotPolicy, OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore,
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{PgPool, Row};

/// Error type for `PostgreSQL` snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Database error during snapshot operations.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Deserialization error.
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// A PostgreSQL-backed snapshot store with configurable policy.
///
/// This implementation stores snapshots in a dedicated `PostgreSQL` table
/// (`es_snapshots`), using the same database as the event store for
/// consistency.
///
/// # Schema
///
/// The store uses the following table schema (created by
/// [`migrate()`](Self::migrate)):
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS es_snapshots (
///     aggregate_kind TEXT NOT NULL,
///     aggregate_id   UUID NOT NULL,
///     position       BIGINT NOT NULL,
///     data           JSONB NOT NULL,
///     created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
///     PRIMARY KEY (aggregate_kind, aggregate_id)
/// )
/// ```
///
/// # Example
///
/// ```ignore
/// use sourcery_postgres::{Store as EventStore};
/// use sourcery_postgres::snapshot::Store as SnapshotStore;
/// use sourcery_core::Repository;
///
/// let pool = PgPool::connect("postgres://...").await?;
/// let event_store = EventStore::new(pool.clone());
/// let snapshot_store = SnapshotStore::every(pool, 100);
///
/// // Run migrations
/// event_store.migrate().await?;
/// snapshot_store.migrate().await?;
///
/// let repo = Repository::new(event_store).with_snapshots(snapshot_store);
/// ```
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    policy: SnapshotPolicy,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotStore")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Create a snapshot store that saves after every command.
    ///
    /// Best for aggregates with expensive replay or many events.
    /// See the policy guidelines in [`SnapshotPolicy`] for choosing an
    /// appropriate cadence.
    #[must_use]
    pub const fn always(pool: PgPool) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::Always,
        }
    }

    /// Create a snapshot store that saves every N events.
    ///
    /// Recommended for most use cases. Start with `n = 50-100` and tune
    /// based on your aggregate's replay cost.
    #[must_use]
    pub const fn every(pool: PgPool, n: u64) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::EveryNEvents(n),
        }
    }

    /// Create a snapshot store that never saves (load-only).
    ///
    /// Use for read replicas, short-lived aggregates, or when managing
    /// snapshots externally.
    #[must_use]
    pub const fn never(pool: PgPool) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::Never,
        }
    }

    /// Apply the snapshot schema (idempotent).
    ///
    /// This uses `CREATE TABLE IF NOT EXISTS` style DDL so it can be run on
    /// startup alongside the event store migrations.
    ///
    /// # Errors
    ///
    /// Returns a `sqlx::Error` if the schema creation query fails.
    #[tracing::instrument(skip(self))]
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS es_snapshots (
                aggregate_kind TEXT NOT NULL,
                aggregate_id   UUID NOT NULL,
                position       BIGINT NOT NULL,
                data           JSONB NOT NULL,
                created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
                PRIMARY KEY (aggregate_kind, aggregate_id)
            )
            ",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

impl SnapshotStore<uuid::Uuid> for Store {
    type Error = Error;
    type Position = i64;

    #[tracing::instrument(skip(self))]
    async fn load<T>(
        &self,
        kind: &str,
        id: &uuid::Uuid,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        let result = sqlx::query(
            r"
            SELECT position, data
            FROM es_snapshots
            WHERE aggregate_kind = $1 AND aggregate_id = $2
            ",
        )
        .bind(kind)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let snapshot = result
            .map(|row| {
                let position: i64 = row.get("position");
                let data: sqlx::types::Json<serde_json::Value> = row.get("data");
                serde_json::from_value::<T>(data.0)
                    .map(|decoded| Snapshot { position, data: decoded })
                    .map_err(|e| Error::Deserialization(Box::new(e)))
            })
            .transpose()?;

        tracing::trace!(found = snapshot.is_some(), "snapshot lookup");
        Ok(snapshot)
    }

    #[tracing::instrument(skip(self, create_snapshot))]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        kind: &str,
        id: &uuid::Uuid,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        let prepared = if self.policy.should_snapshot(events_since_last_snapshot) {
            match create_snapshot() {
                Ok(snapshot) => serde_json::to_value(&snapshot.data)
                    .map(|data| Some((snapshot.position, data)))
                    .map_err(|e| OfferSnapshotError::Snapshot(Error::Serialization(Box::new(e)))),
                Err(e) => Err(OfferSnapshotError::Create(e)),
            }
        } else {
            Ok(None)
        }?;

        let Some((position, data)) = prepared else {
            return Ok(SnapshotOffer::Declined);
        };

        // Use ON CONFLICT to upsert, but only if the new position is greater
        // than the existing one. This prevents race conditions where an older
        // snapshot could overwrite a newer one.
        let result = sqlx::query(
            r"
            INSERT INTO es_snapshots (aggregate_kind, aggregate_id, position, data)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (aggregate_kind, aggregate_id)
            DO UPDATE SET position = EXCLUDED.position, data = EXCLUDED.data, created_at = now()
            WHERE es_snapshots.position < EXCLUDED.position
            ",
        )
        .bind(kind)
        .bind(id)
        .bind(position)
        .bind(sqlx::types::Json(data))
        .execute(&self.pool)
        .await
        .map_err(|e| OfferSnapshotError::Snapshot(Error::Database(e)))?;

        // rows_affected() will be 1 if inserted or updated, 0 if the existing
        // snapshot has a >= position (declined due to staleness)
        let offer = if result.rows_affected() > 0 {
            SnapshotOffer::Stored
        } else {
            SnapshotOffer::Declined
        };

        tracing::debug!(
            events_since_last_snapshot,
            ?offer,
            "snapshot offer evaluated"
        );
        Ok(offer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_always_should_snapshot() {
        let policy = SnapshotPolicy::Always;
        assert!(policy.should_snapshot(0));
        assert!(policy.should_snapshot(1));
        assert!(policy.should_snapshot(100));
    }

    #[test]
    fn policy_every_n_events_should_snapshot() {
        let policy = SnapshotPolicy::EveryNEvents(50);
        assert!(!policy.should_snapshot(0));
        assert!(!policy.should_snapshot(49));
        assert!(policy.should_snapshot(50));
        assert!(policy.should_snapshot(100));
    }

    #[test]
    fn policy_never_should_snapshot() {
        let policy = SnapshotPolicy::Never;
        assert!(!policy.should_snapshot(0));
        assert!(!policy.should_snapshot(1));
        assert!(!policy.should_snapshot(1000));
    }
}
