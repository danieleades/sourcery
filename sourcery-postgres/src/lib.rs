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

use nonempty::NonEmpty;
use serde::{Serialize, de::DeserializeOwned};
use sourcery_core::{
    concurrency::ConcurrencyConflict,
    event::DomainEvent,
    store::{
        CommitError, Committed, EventFilter, EventStore, GloballyOrderedStore, LoadEventsResult,
        OptimisticCommitError, StoredEvent,
    },
    subscription::{EventStream, SubscribableStore},
};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgListener};

/// Error type for `PostgreSQL` event store operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Query execution or transaction failure.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Invalid position value supplied by a caller.
    #[error("invalid position value from database: {0}")]
    InvalidPosition(i64),
    /// Insert operation returned no positions for written events.
    #[error("database did not return an inserted position")]
    MissingReturnedPosition,
    /// Event serialisation failed before writing.
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Event deserialisation failed while loading/replaying.
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
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

type PgLoadResult<M> = LoadEventsResult<uuid::Uuid, i64, serde_json::Value, M, Error>;

impl<M> Store<M> {
    /// Construct a `PostgreSQL` event store from a connection pool.
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

impl<M> Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    const EVENTS_NOTIFY_CHANNEL: &'static str = "sourcery_es_events";

    async fn load_all_events_since(&self, from_position: Option<i64>) -> PgLoadResult<M> {
        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT aggregate_kind, aggregate_id, event_kind, position, data, metadata FROM \
             es_events",
        );

        if let Some(after) = from_position {
            qb.push(" WHERE position > ").push_bind(after);
        }

        qb.push(" ORDER BY position ASC");
        let rows = qb.build().fetch_all(&self.pool).await?;

        Self::decode_rows(rows)
    }

    fn decode_rows(rows: Vec<sqlx::postgres::PgRow>) -> PgLoadResult<M> {
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

    async fn load_events_since_filtered(
        &self,
        filters: &[EventFilter<uuid::Uuid, i64>],
        from_position: Option<i64>,
    ) -> PgLoadResult<M> {
        let positioned = filters
            .iter()
            .cloned()
            .map(|mut filter| {
                if let Some(from) = from_position {
                    filter.after_position =
                        Some(filter.after_position.map_or(from, |after| after.max(from)));
                }
                filter
            })
            .collect::<Vec<_>>();
        self.load_events(&positioned).await
    }

    async fn ensure_stream_row(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        aggregate_kind: &str,
        aggregate_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        sqlx::query(
            r"
                INSERT INTO es_streams (aggregate_kind, aggregate_id, last_position)
                VALUES ($1, $2, NULL)
                ON CONFLICT (aggregate_kind, aggregate_id) DO NOTHING
                ",
        )
        .bind(aggregate_kind)
        .bind(aggregate_id)
        .execute(&mut **tx)
        .await
        .map(|_| ())
        .map_err(Error::from)
    }

    async fn append_prepared_events(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        aggregate_kind: &str,
        aggregate_id: &uuid::Uuid,
        prepared: Vec<(String, serde_json::Value)>,
        metadata: &M,
    ) -> Result<i64, Error> {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO es_events (aggregate_kind, aggregate_id, event_kind, data, metadata) ",
        );
        qb.push_values(prepared, |mut b, (kind, data)| {
            b.push_bind(aggregate_kind);
            b.push_bind(aggregate_id);
            b.push_bind(kind);
            b.push_bind(sqlx::types::Json(data));
            b.push_bind(sqlx::types::Json(metadata.clone()));
        });
        qb.push(" RETURNING position");

        let rows: Vec<i64> = qb.build_query_scalar().fetch_all(&mut **tx).await?;

        let last_position = *rows.last().ok_or(Error::MissingReturnedPosition)?;

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
        .execute(&mut **tx)
        .await?;

        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(Self::EVENTS_NOTIFY_CHANNEL)
            .bind(last_position.to_string())
            .execute(&mut **tx)
            .await?;

        Ok(last_position)
    }

    fn subscribe_with_listener(
        &self,
        from_position: Option<i64>,
        filters: Option<Vec<EventFilter<uuid::Uuid, i64>>>,
    ) -> EventStream<'_, Self> {
        let store = self.clone();

        Box::pin(async_stream::stream! {
            let mut last_position = from_position;
            let mut listener = match PgListener::connect_with(&store.pool).await {
                Ok(listener) => listener,
                Err(error) => {
                    yield Err(Error::Database(error));
                    return;
                }
            };

            if let Err(error) = listener.listen(Self::EVENTS_NOTIFY_CHANNEL).await {
                yield Err(Error::Database(error));
                return;
            }

            // Historical catch-up.
            let historical = if let Some(filters) = filters.as_deref() {
                store.load_events_since_filtered(filters, last_position).await
            } else {
                store.load_all_events_since(last_position).await
            };
            match historical {
                Ok(events) => {
                    for event in events {
                        if let Some(ref lp) = last_position
                            && event.position <= *lp
                        {
                            continue;
                        }
                        last_position = Some(event.position);
                        yield Ok(event);
                    }
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }

            // Live notifications.
            loop {
                if let Err(error) = listener.recv().await {
                    yield Err(Error::Database(error));
                    return;
                }

                let loaded = if let Some(filters) = filters.as_deref() {
                    store.load_events_since_filtered(filters, last_position).await
                } else {
                    store.load_all_events_since(last_position).await
                };
                match loaded {
                    Ok(events) => {
                        for event in events {
                            last_position = Some(event.position);
                            yield Ok(event);
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
        })
    }

    fn prepare_events<E>(
        events: &NonEmpty<E>,
    ) -> Result<Vec<(String, serde_json::Value)>, (usize, serde_json::Error)>
    where
        E: sourcery_core::event::EventKind + serde::Serialize,
    {
        let mut prepared: Vec<(String, serde_json::Value)> = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let data = serde_json::to_value(event).map_err(|error| (index, error))?;
            prepared.push((event.kind().to_string(), data));
        }
        Ok(prepared)
    }
}

impl<M> EventStore for Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Data = serde_json::Value;
    type Error = Error;
    type Id = uuid::Uuid;
    type Metadata = M;
    type Position = i64;

    fn decode_event<E>(
        &self,
        stored: &StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>,
    ) -> Result<E, Self::Error>
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

    #[tracing::instrument(
        skip(self, events, metadata),
        fields(
            aggregate_kind,
            aggregate_id = %aggregate_id,
            events_len = events.len()
        )
    )]
    async fn commit_events<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> Result<Committed<i64>, CommitError<Self::Error>>
    where
        E: sourcery_core::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        let prepared =
            Self::prepare_events(&events).map_err(|(index, error)| CommitError::Serialization {
                index,
                source: Error::Serialization(Box::new(error)),
            })?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CommitError::Store(Error::Database(e)))?;

        Self::ensure_stream_row(&mut tx, aggregate_kind, aggregate_id)
            .await
            .map_err(CommitError::Store)?;

        let last_position =
            Self::append_prepared_events(&mut tx, aggregate_kind, aggregate_id, prepared, metadata)
                .await
                .map_err(CommitError::Store)?;

        tx.commit()
            .await
            .map_err(|e| CommitError::Store(Error::Database(e)))?;

        Ok(Committed { last_position })
    }

    #[tracing::instrument(
        skip(self, events, metadata),
        fields(
            aggregate_kind,
            aggregate_id = %aggregate_id,
            expected_version,
            events_len = events.len()
        )
    )]
    async fn commit_events_optimistic<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> Result<Committed<i64>, OptimisticCommitError<i64, Self::Error>>
    where
        E: sourcery_core::event::EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        let prepared = Self::prepare_events(&events).map_err(|(index, error)| {
            OptimisticCommitError::Serialization {
                index,
                source: Error::Serialization(Box::new(error)),
            }
        })?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| OptimisticCommitError::Store(Error::Database(e)))?;

        Self::ensure_stream_row(&mut tx, aggregate_kind, aggregate_id)
            .await
            .map_err(OptimisticCommitError::Store)?;

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
        .map_err(|e| OptimisticCommitError::Store(Error::Database(e)))?;

        // Version check
        match expected_version {
            Some(expected) => {
                if current != Some(expected) {
                    return Err(OptimisticCommitError::Conflict(ConcurrencyConflict {
                        expected: Some(expected),
                        actual: current,
                    }));
                }
            }
            None => {
                // Expected new stream (no events)
                if let Some(actual) = current {
                    return Err(OptimisticCommitError::Conflict(ConcurrencyConflict {
                        expected: None,
                        actual: Some(actual),
                    }));
                }
            }
        }

        let last_position =
            Self::append_prepared_events(&mut tx, aggregate_kind, aggregate_id, prepared, metadata)
                .await
                .map_err(OptimisticCommitError::Store)?;

        tx.commit()
            .await
            .map_err(|e| OptimisticCommitError::Store(Error::Database(e)))?;

        Ok(Committed { last_position })
    }

    #[tracing::instrument(skip(self, filters), fields(filters_len = filters.len()))]
    async fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<Self::Id, Self::Position>],
    ) -> PgLoadResult<M> {
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
        Self::decode_rows(rows)
    }
}

impl<M> GloballyOrderedStore for Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    async fn latest_position(&self) -> Result<Option<Self::Position>, Self::Error> {
        let latest = sqlx::query_scalar("SELECT MAX(position) FROM es_events")
            .fetch_one(&self.pool)
            .await?;
        Ok(latest)
    }
}

impl<M> SubscribableStore for Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from_position: Option<Self::Position>,
    ) -> EventStream<'_, Self>
    where
        Self::Position: Ord,
    {
        self.subscribe_with_listener(from_position, Some(filters.to_vec()))
    }

    fn subscribe_all(&self, from_position: Option<Self::Position>) -> EventStream<'_, Self>
    where
        Self::Position: Ord,
    {
        self.subscribe_with_listener(from_position, None)
    }
}
