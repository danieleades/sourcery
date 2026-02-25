mod commit;
mod live;
mod load;
mod subscribe;

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
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::Error;

/// A PostgreSQL-backed [`EventStore`].
///
/// Defaults are intentionally conservative:
/// - Positions are global and monotonic (`i64`, backed by `BIGSERIAL`).
/// - Metadata is stored as `jsonb` (`M: Serialize + DeserializeOwned`).
/// - Event data is stored as `jsonb`.
#[derive(Clone)]
pub struct Store<M> {
    pub(crate) pool: PgPool,
    live: live::LivePump<M>,
}

pub(crate) type PgLoadResult<M> = LoadEventsResult<uuid::Uuid, i64, serde_json::Value, M, Error>;
pub(crate) type PgStoredEvent<M> = StoredEvent<uuid::Uuid, i64, serde_json::Value, M>;

impl<M> Store<M> {
    /// Construct a `PostgreSQL` event store from a connection pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            live: live::LivePump::new(pool.clone()),
            pool,
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
        self.subscribe_with_live_pump(from_position, Some(filters.to_vec()))
    }

    fn subscribe_all(&self, from_position: Option<Self::Position>) -> EventStream<'_, Self>
    where
        Self::Position: Ord,
    {
        self.subscribe_with_live_pump(from_position, None)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nonempty::nonempty;
    use serde::{Deserialize, Serialize};
    use sourcery_core::{
        event::DomainEvent,
        store::{CommitError, EventFilter, EventStore, OptimisticCommitError, StoredEvent},
        subscription::SubscribableStore,
    };
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestMetadata {
        source: String,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct AccountOpened {
        owner: String,
    }

    impl DomainEvent for AccountOpened {
        const KIND: &'static str = "account.opened";
    }

    #[derive(Clone, Debug)]
    struct MaybeUnserialisableEvent {
        should_fail: bool,
    }

    impl DomainEvent for MaybeUnserialisableEvent {
        const KIND: &'static str = "account.maybe_unserialisable";
    }

    impl Serialize for MaybeUnserialisableEvent {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            if self.should_fail {
                Err(serde::ser::Error::custom(
                    "intentional serialisation failure",
                ))
            } else {
                serializer.serialize_str("ok")
            }
        }
    }

    fn disconnected_store() -> Store<TestMetadata> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/sourcery")
            .expect("connection URL should be valid for lazy pool construction");
        Store::new(pool)
    }

    fn metadata() -> TestMetadata {
        TestMetadata {
            source: "tests".to_owned(),
        }
    }

    #[test]
    fn prepare_events_preserves_order_and_kind() {
        let events = nonempty![
            AccountOpened {
                owner: "Alice".to_owned()
            },
            AccountOpened {
                owner: "Bob".to_owned()
            }
        ];

        let prepared =
            Store::<TestMetadata>::prepare_events(&events).expect("events should serialise");

        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].0, AccountOpened::KIND);
        assert_eq!(
            prepared[0].1,
            serde_json::json!({
                "owner": "Alice",
            })
        );
        assert_eq!(prepared[1].0, AccountOpened::KIND);
        assert_eq!(
            prepared[1].1,
            serde_json::json!({
                "owner": "Bob",
            })
        );
    }

    #[test]
    fn prepare_events_reports_failing_index() {
        let events = nonempty![
            MaybeUnserialisableEvent { should_fail: false },
            MaybeUnserialisableEvent { should_fail: true }
        ];

        let error = Store::<TestMetadata>::prepare_events(&events)
            .expect_err("second event should fail serialisation");

        assert_eq!(error.0, 1);
    }

    #[tokio::test]
    async fn load_events_with_no_filters_returns_empty_without_database_roundtrip() {
        let store = disconnected_store();

        let loaded = store
            .load_events(&[])
            .await
            .expect("empty filter list should short-circuit");

        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn load_events_rejects_negative_positions_before_query_execution() {
        let store = disconnected_store();
        let filters = [EventFilter::for_event(AccountOpened::KIND).after(-1)];

        let error = store
            .load_events(&filters)
            .await
            .expect_err("negative positions should be rejected");

        assert!(matches!(error, Error::InvalidPosition(-1)));
    }

    #[tokio::test]
    async fn load_events_since_filtered_uses_max_position_with_catch_up_floor() {
        let store = disconnected_store();
        let filters = [EventFilter::for_event(AccountOpened::KIND).after(-10)];

        let error = store
            .load_events_since_filtered(&filters, Some(-4))
            .await
            .expect_err("merged negative position should still be rejected");

        assert!(matches!(error, Error::InvalidPosition(-4)));
    }

    #[tokio::test]
    async fn decode_event_deserialises_valid_payload() {
        let store = disconnected_store();
        let stored = StoredEvent {
            aggregate_kind: "account".to_owned(),
            aggregate_id: uuid::Uuid::new_v4(),
            kind: AccountOpened::KIND.to_owned(),
            position: 7,
            data: serde_json::json!({
                "owner": "Alice",
            }),
            metadata: metadata(),
        };

        let decoded: AccountOpened = store
            .decode_event(&stored)
            .expect("event payload should decode");

        assert_eq!(
            decoded,
            AccountOpened {
                owner: "Alice".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn decode_event_maps_deserialisation_errors() {
        let store = disconnected_store();
        let stored = StoredEvent {
            aggregate_kind: "account".to_owned(),
            aggregate_id: uuid::Uuid::new_v4(),
            kind: AccountOpened::KIND.to_owned(),
            position: 8,
            data: serde_json::json!({
                "owner": 42,
            }),
            metadata: metadata(),
        };

        let error = store
            .decode_event::<AccountOpened>(&stored)
            .expect_err("payload type mismatch should fail decoding");

        assert!(matches!(error, Error::Deserialization(_)));
    }

    #[tokio::test]
    async fn commit_events_surfaces_serialisation_index_without_opening_transaction() {
        let store = disconnected_store();
        let aggregate_id = uuid::Uuid::new_v4();
        let events = nonempty![
            MaybeUnserialisableEvent { should_fail: false },
            MaybeUnserialisableEvent { should_fail: true }
        ];

        let error = store
            .commit_events("account", &aggregate_id, events, &metadata())
            .await
            .expect_err("serialisation failure should be reported before store access");

        match error {
            CommitError::Serialization { index, source } => {
                assert_eq!(index, 1);
                assert!(matches!(source, Error::Serialization(_)));
            }
            CommitError::Store(_) => panic!("expected serialisation error"),
        }
    }

    #[tokio::test]
    async fn commit_events_optimistic_surfaces_serialisation_index_without_store_access() {
        let store = disconnected_store();
        let aggregate_id = uuid::Uuid::new_v4();
        let events = nonempty![
            MaybeUnserialisableEvent { should_fail: false },
            MaybeUnserialisableEvent { should_fail: true }
        ];

        let error = store
            .commit_events_optimistic("account", &aggregate_id, None, events, &metadata())
            .await
            .expect_err("serialisation failure should be reported before store access");

        match error {
            OptimisticCommitError::Serialization { index, source } => {
                assert_eq!(index, 1);
                assert!(matches!(source, Error::Serialization(_)));
            }
            OptimisticCommitError::Conflict(_) | OptimisticCommitError::Store(_) => {
                panic!("expected serialisation error")
            }
        }
    }

    #[tokio::test]
    async fn load_all_events_since_propagates_database_failures() {
        let store = disconnected_store();

        let from_start = store.load_all_events_since(None).await;
        let from_position = store.load_all_events_since(Some(3)).await;

        assert!(matches!(from_start, Err(Error::Database(_))));
        assert!(matches!(from_position, Err(Error::Database(_))));
    }

    #[tokio::test]
    async fn latest_position_propagates_database_failures() {
        let store = disconnected_store();

        let result = store.latest_position().await;

        assert!(matches!(result, Err(Error::Database(_))));
    }

    #[tokio::test]
    async fn subscribe_builders_create_streams_for_filtered_and_unfiltered_subscriptions() {
        let store = disconnected_store();
        let filters = [EventFilter::for_event(AccountOpened::KIND)];

        let _filtered = store.subscribe(&filters, Some(5));
        let _all = store.subscribe_all(Some(5));
    }
}
