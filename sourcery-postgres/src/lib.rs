//! Postgres-backed event sourcing implementations.
//!
//! This crate provides `PostgreSQL` implementations of the core Sourcery
//! traits:
//!
//! - [`Store`] - An implementation of [`sourcery_core::store::EventStore`]
//! - [`snapshot::Store`] - An implementation of
//!   [`sourcery_core::snapshot::SnapshotStore`]
//!
//! Both use the same database and can share a connection pool.

pub mod snapshot;

use std::marker::PhantomData;

use serde::{Serialize, de::DeserializeOwned};
use sourcery_core::{
    concurrency::{ConcurrencyConflict, ConcurrencyStrategy},
    event::DomainEvent,
    store::{
        AppendError, AppendResult, EventFilter, EventStore, GloballyOrderedStore, NonEmpty,
        StoredEventView, Transaction,
    },
};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

type AppendOutcome = Result<AppendResult<i64>, AppendError<i64, Error>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid position value from database: {0}")]
    InvalidPosition(i64),
    #[error("database did not return an inserted position")]
    MissingReturnedPosition,
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// Stored event with position and metadata.
///
/// This type represents an event that has been persisted to `PostgreSQL` and
/// loaded back. It contains all the information needed to deserialize the event
/// data and metadata, along with the aggregate identifiers and global position.
///
/// # Type Parameters
///
/// - `M`: The metadata type (must be `Serialize + DeserializeOwned`)
///
/// # Fields
///
/// All fields are accessible through the [`StoredEventView`] trait
/// implementation. Use the trait methods rather than accessing fields directly
/// to maintain compatibility across store implementations.
#[derive(Clone)]
pub struct StoredEvent<M> {
    aggregate_kind: String,
    aggregate_id: uuid::Uuid,
    kind: String,
    position: i64,
    data: serde_json::Value,
    metadata: M,
}

/// Staged event awaiting persistence.
///
/// This type represents an event that has been serialized and prepared for
/// persistence but not yet written to `PostgreSQL`. The repository creates
/// these from domain events, then batches them together for atomic appending.
///
/// # Type Parameters
///
/// - `M`: The metadata type (must be `Serialize + DeserializeOwned`)
///
/// # Internal Use
///
/// This type is primarily used by the [`Store`] implementation. Users typically
/// interact with domain events directly and don't need to create `StagedEvent`
/// instances manually.
#[derive(Clone)]
pub struct StagedEvent<M> {
    kind: String,
    data: serde_json::Value,
    metadata: M,
}

impl<M> StoredEventView for StoredEvent<M> {
    type Id = uuid::Uuid;
    type Metadata = M;
    type Pos = i64;

    fn aggregate_kind(&self) -> &str {
        &self.aggregate_kind
    }

    fn aggregate_id(&self) -> &Self::Id {
        &self.aggregate_id
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    fn position(&self) -> Self::Pos {
        self.position
    }

    fn metadata(&self) -> &Self::Metadata {
        &self.metadata
    }
}

/// A PostgreSQL-backed [`EventStore`].
///
/// Defaults are intentionally conservative:
/// - Positions are global and monotonic (`i64`, backed by `BIGSERIAL`).
/// - Metadata is stored as `jsonb` (`M: Serialize + DeserializeOwned`).
/// - Event data is stored as `jsonb`.
#[derive(Clone)]
pub struct Store<M> {
    pool: PgPool,
    _phantom: PhantomData<M>,
}

impl<M> Store<M> {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            _phantom: PhantomData,
        }
    }
}

impl<M> Store<M>
where
    M: Sync,
{
    /// Apply the initial schema (idempotent).
    ///
    /// This uses `CREATE TABLE IF NOT EXISTS` style DDL so it can be run on
    /// startup.
    ///
    /// # Errors
    ///
    /// Returns a `sqlx::Error` if any of the schema creation queries fail.
    #[tracing::instrument(skip(self))]
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        // Streams track per-aggregate last position for optimistic concurrency.
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS es_streams (
                aggregate_kind TEXT NOT NULL,
                aggregate_id   UUID NOT NULL,
                last_position  BIGINT NULL,
                PRIMARY KEY (aggregate_kind, aggregate_id)
            )
            ",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS es_events (
                position       BIGSERIAL PRIMARY KEY,
                aggregate_kind TEXT NOT NULL,
                aggregate_id   UUID NOT NULL,
                event_kind     TEXT NOT NULL,
                data           JSONB NOT NULL,
                metadata       JSONB NOT NULL,
                created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
            )
            ",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r"CREATE INDEX IF NOT EXISTS es_events_by_kind_and_position ON es_events(event_kind, position)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r"CREATE INDEX IF NOT EXISTS es_events_by_stream_and_position ON es_events(aggregate_kind, aggregate_id, position)",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

impl<M> EventStore for Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Error = Error;
    type Id = uuid::Uuid;
    type Metadata = M;
    type Position = i64;
    type StagedEvent = StagedEvent<M>;
    type StoredEvent = StoredEvent<M>;

    fn stage_event<E>(
        &self,
        event: &E,
        metadata: Self::Metadata,
    ) -> Result<Self::StagedEvent, Self::Error>
    where
        E: sourcery_core::event::EventKind + serde::Serialize,
    {
        let data = serde_json::to_value(event).map_err(|e| Error::Serialization(Box::new(e)))?;
        Ok(StagedEvent {
            kind: event.kind().to_string(),
            data,
            metadata,
        })
    }

    fn decode_event<E>(&self, stored: &Self::StoredEvent) -> Result<E, Self::Error>
    where
        E: DomainEvent + serde::de::DeserializeOwned,
    {
        serde_json::from_value(stored.data.clone()).map_err(|e| Error::Deserialization(Box::new(e)))
    }

    async fn stream_version<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
    ) -> Result<Option<Self::Position>, Self::Error> {
        let result: Option<i64> = sqlx::query_scalar(
            r"SELECT last_position FROM es_streams WHERE aggregate_kind = $1 AND aggregate_id = $2",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();

        Ok(result)
    }

    fn begin<Conc: ConcurrencyStrategy>(
        &self,
        aggregate_kind: &str,
        aggregate_id: Self::Id,
        expected_version: Option<Self::Position>,
    ) -> Transaction<'_, Self, Conc> {
        Transaction::new(
            self,
            aggregate_kind.to_string(),
            aggregate_id,
            expected_version,
        )
    }

    #[tracing::instrument(
        skip(self, events),
        fields(
            aggregate_kind,
            aggregate_id = %aggregate_id,
            expected_version,
            events_len = events.len()
        )
    )]
    async fn append<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<Self::StagedEvent>,
    ) -> AppendOutcome {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        sqlx::query(
            r"
                INSERT INTO es_streams (aggregate_kind, aggregate_id, last_position)
                VALUES ($1, $2, NULL)
                ON CONFLICT (aggregate_kind, aggregate_id) DO NOTHING
                ",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        let current: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
            r"
                SELECT last_position
                FROM es_streams
                WHERE aggregate_kind = $1 AND aggregate_id = $2
                FOR UPDATE
                ",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        if let Some(expected) = expected_version
            && current != Some(expected)
        {
            return Err(AppendError::Conflict(ConcurrencyConflict {
                expected: Some(expected),
                actual: current,
            }));
        }

        let mut prepared = Vec::with_capacity(events.len());
        for event in events {
            let StagedEvent {
                kind,
                data,
                metadata,
            } = event;
            prepared.push((kind, data, metadata));
        }

        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO es_events (aggregate_kind, aggregate_id, event_kind, data, metadata) ",
        );
        qb.push_values(prepared.into_iter(), |mut b, (kind, data, metadata)| {
            b.push_bind(aggregate_kind);
            b.push_bind(aggregate_id);
            b.push_bind(kind);
            b.push_bind(sqlx::types::Json(data));
            b.push_bind(sqlx::types::Json(metadata));
        });
        qb.push(" RETURNING position");

        let rows: Vec<i64> = qb
            .build_query_scalar()
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        let last_position = rows
            .last()
            .ok_or_else(|| AppendError::store(Error::MissingReturnedPosition))?;

        sqlx::query(
            r"
                UPDATE es_streams
                SET last_position = $1
                WHERE aggregate_kind = $2 AND aggregate_id = $3
                ",
        )
        .bind(last_position)
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        tx.commit()
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        Ok(AppendResult {
            last_position: *last_position,
        })
    }

    #[tracing::instrument(skip(self, filters), fields(filters_len = filters.len()))]
    async fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<Self::Id, Self::Position>],
    ) -> Result<Vec<Self::StoredEvent>, Self::Error> {
        if filters.is_empty() {
            return Ok(Vec::new());
        }

        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT aggregate_kind, aggregate_id, event_kind, position, data, metadata FROM (",
        );

        for (i, filter) in filters.iter().enumerate() {
            if i > 0 {
                qb.push(" UNION ALL ");
            }

            qb.push(
                "SELECT aggregate_kind, aggregate_id, event_kind, position, data, metadata FROM \
                 es_events WHERE event_kind = ",
            )
            .push_bind(&filter.event_kind);

            if let Some(kind) = &filter.aggregate_kind {
                qb.push(" AND aggregate_kind = ").push_bind(kind);
            }

            if let Some(id) = &filter.aggregate_id {
                qb.push(" AND aggregate_id = ").push_bind(id);
            }

            if let Some(after) = filter.after_position {
                if after < 0 {
                    return Err(Error::InvalidPosition(after));
                }
                qb.push(" AND position > ").push_bind(after);
            }
        }

        qb.push(") t ORDER BY position ASC");

        let rows = qb.build().fetch_all(&self.pool).await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let aggregate_kind: String = row.try_get("aggregate_kind")?;
            let aggregate_id: uuid::Uuid = row.try_get("aggregate_id")?;
            let event_kind: String = row.try_get("event_kind")?;
            let position: i64 = row.try_get("position")?;
            let data: sqlx::types::Json<serde_json::Value> = row.try_get("data")?;
            let metadata: sqlx::types::Json<M> = row.try_get("metadata")?;

            out.push(StoredEvent {
                aggregate_kind,
                aggregate_id,
                kind: event_kind,
                position,
                data: data.0,
                metadata: metadata.0,
            });
        }

        Ok(out)
    }

    #[tracing::instrument(
        skip(self, events),
        fields(aggregate_kind, aggregate_id = %aggregate_id, events_len = events.len())
    )]
    async fn append_expecting_new<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<Self::StagedEvent>,
    ) -> AppendOutcome {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        sqlx::query(
            r"
                INSERT INTO es_streams (aggregate_kind, aggregate_id, last_position)
                VALUES ($1, $2, NULL)
                ON CONFLICT (aggregate_kind, aggregate_id) DO NOTHING
                ",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        let current: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
            r"
                SELECT last_position
                FROM es_streams
                WHERE aggregate_kind = $1 AND aggregate_id = $2
                FOR UPDATE
                ",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        if let Some(actual) = current {
            return Err(AppendError::Conflict(ConcurrencyConflict {
                expected: None,
                actual: Some(actual),
            }));
        }

        let mut prepared = Vec::with_capacity(events.len());
        for event in events {
            let StagedEvent {
                kind,
                data,
                metadata,
            } = event;
            prepared.push((kind, data, metadata));
        }

        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO es_events (aggregate_kind, aggregate_id, event_kind, data, metadata) ",
        );
        qb.push_values(prepared.into_iter(), |mut b, (kind, data, metadata)| {
            b.push_bind(aggregate_kind);
            b.push_bind(aggregate_id);
            b.push_bind(kind);
            b.push_bind(sqlx::types::Json(data));
            b.push_bind(sqlx::types::Json(metadata));
        });
        qb.push(" RETURNING position");

        let rows: Vec<i64> = qb
            .build_query_scalar()
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        let last_position = rows
            .last()
            .ok_or_else(|| AppendError::store(Error::MissingReturnedPosition))?;

        sqlx::query(
            r"
                UPDATE es_streams
                SET last_position = $1
                WHERE aggregate_kind = $2 AND aggregate_id = $3
                ",
        )
        .bind(last_position)
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppendError::store(Error::Database(e)))?;

        tx.commit()
            .await
            .map_err(|e| AppendError::store(Error::Database(e)))?;

        Ok(AppendResult {
            last_position: *last_position,
        })
    }
}

impl<M> GloballyOrderedStore for Store<M> where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}
