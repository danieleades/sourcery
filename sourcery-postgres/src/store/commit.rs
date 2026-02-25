use nonempty::NonEmpty;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Postgres, QueryBuilder};

use super::Store;
use crate::Error;

impl<M> Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// Upsert an `es_streams` row for the given aggregate, doing nothing if
    /// one already exists.
    ///
    /// Must be called inside an open transaction before inserting events.
    pub(in crate::store) async fn ensure_stream_row(
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

    /// Bulk-insert `prepared` events, update the stream's `last_position`, and
    /// emit a `pg_notify` so that live subscribers can poll for the new batch.
    ///
    /// Returns the last (highest) position assigned by the database.
    ///
    /// # Notification payload
    ///
    /// The payload carries the *first* (minimum) position of the batch rather
    /// than the last. Subscribers use this to detect out-of-order commits
    /// (possible because `PostgreSQL` sequences are allocated before commit)
    /// and re-query from just before the earliest new event.
    pub(in crate::store) async fn append_prepared_events(
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

        let first_position = *rows.first().ok_or(Error::MissingReturnedPosition)?;
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
            .bind(first_position.to_string())
            .execute(&mut **tx)
            .await?;

        Ok(last_position)
    }

    /// Serialise each event in `events` to a `(kind, JSON value)` pair,
    /// returning the index of the first event that fails serialisation.
    pub(in crate::store) fn prepare_events<E>(
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
