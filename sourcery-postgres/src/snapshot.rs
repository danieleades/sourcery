//! PostgreSQL-backed snapshot store implementation.
//!
//! This module provides [`SnapshotStore`], an implementation of
//! [`sourcery_core::snapshot::SnapshotStore`] for `PostgreSQL`.

use sourcery_core::snapshot::{OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore};
use sqlx::{PgPool, Row};

/// Error type for `PostgreSQL` snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Database error during snapshot operations.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Snapshot creation policy.
///
/// # Choosing a Policy
///
/// The right policy depends on your aggregate's characteristics:
///
/// | Policy | Best For | Trade-off |
/// |--------|----------|-----------|
/// | `Always` | Expensive replay, low write volume | Storage cost per command |
/// | `EveryNEvents(n)` | Most use cases | Balanced storage vs replay |
/// | `Never` | Read replicas, external snapshot management | Full replay every load |
///
/// ## `Always`
///
/// Creates a snapshot after every command. Best for aggregates where:
/// - Event replay is computationally expensive
/// - Aggregates have many events (100+)
/// - Read latency is more important than write overhead
/// - Write volume is relatively low
///
/// ## `EveryNEvents(n)`
///
/// Creates a snapshot every N events. Recommended for most use cases.
/// - Start with `n = 50-100` and tune based on profiling
/// - Balances storage cost against replay time
/// - Works well for aggregates with moderate event counts
///
/// ## `Never`
///
/// Never creates snapshots. Use when:
/// - Running a read replica that consumes snapshots created elsewhere
/// - Aggregates are short-lived (few events per instance)
/// - Managing snapshots through an external process
/// - Testing without snapshot overhead
#[derive(Clone, Debug)]
enum SnapshotPolicy {
    /// Create a snapshot after every command.
    Always,
    /// Create a snapshot every N events.
    EveryNEvents(u64),
    /// Never create snapshots (load-only mode).
    Never,
}

impl SnapshotPolicy {
    const fn should_snapshot(&self, events_since: u64) -> bool {
        match self {
            Self::Always => true,
            Self::EveryNEvents(threshold) => events_since >= *threshold,
            Self::Never => false,
        }
    }
}

/// A PostgreSQL-backed snapshot store with configurable policy.
///
/// This implementation stores snapshots in a dedicated `PostgreSQL` table
/// (`es_snapshots`), using the same database as the event store for
/// consistency.
///
/// # Schema
///
/// The store uses the following table schema (created by [`migrate()`](Self::migrate)):
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS es_snapshots (
///     aggregate_kind TEXT NOT NULL,
///     aggregate_id   UUID NOT NULL,
///     position       BIGINT NOT NULL,
///     data           BYTEA NOT NULL,
///     created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
///     PRIMARY KEY (aggregate_kind, aggregate_id)
/// )
/// ```
///
/// # Example
///
/// ```ignore
/// use sourcery_postgres::{Store, SnapshotStore};
/// use sourcery_core::Repository;
///
/// let pool = PgPool::connect("postgres://...").await?;
/// let event_store = Store::new(pool.clone(), JsonCodec);
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
                data           BYTEA NOT NULL,
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
    async fn load<'a>(
        &'a self,
        kind: &'a str,
        id: &'a uuid::Uuid,
    ) -> Result<Option<Snapshot<Self::Position>>, Self::Error> {
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

        let snapshot = result.map(|row| {
            let position: i64 = row.get("position");
            let data: Vec<u8> = row.get("data");
            Snapshot { position, data }
        });

        tracing::trace!(found = snapshot.is_some(), "snapshot lookup");
        Ok(snapshot)
    }

    #[tracing::instrument(skip(self, create_snapshot))]
    fn offer_snapshot<'a, CE, Create>(
        &'a self,
        kind: &'a str,
        id: &'a uuid::Uuid,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> impl std::future::Future<Output = Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>>
           + Send
           + 'a
    where
        CE: std::error::Error + Send + Sync + 'static,
        Create: FnOnce() -> Result<Snapshot<Self::Position>, CE> + 'a,
    {
        // Evaluate policy and create snapshot synchronously before entering async block
        // This avoids capturing the non-Send Create closure across await points
        let snapshot_result = if self.policy.should_snapshot(events_since_last_snapshot) {
            Some(create_snapshot())
        } else {
            None
        };

        async move {
            let snapshot = match snapshot_result {
                None => return Ok(SnapshotOffer::Declined),
                Some(Ok(snapshot)) => snapshot,
                Some(Err(e)) => return Err(OfferSnapshotError::Create(e)),
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
            .bind(snapshot.position)
            .bind(&snapshot.data)
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
