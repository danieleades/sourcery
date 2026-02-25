use serde::{Serialize, de::DeserializeOwned};
use sourcery_core::store::{EventFilter, EventStore, StoredEvent};
use sqlx::{Postgres, QueryBuilder, Row, postgres::PgRow};

use super::{PgLoadResult, Store};

impl<M> Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    pub(in crate::store) async fn load_all_events_since_with_pool(
        pool: &sqlx::PgPool,
        from_position: Option<i64>,
    ) -> PgLoadResult<M> {
        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT aggregate_kind, aggregate_id, event_kind, position, data, metadata FROM \
             es_events",
        );

        if let Some(after) = from_position {
            qb.push(" WHERE position > ").push_bind(after);
        }

        qb.push(" ORDER BY position ASC");
        let rows = qb.build().fetch_all(pool).await?;
        Self::decode_rows(rows)
    }

    /// Load all events with positions strictly greater than `from_position`,
    /// ordered ascending. Passing `None` loads from the beginning.
    pub(in crate::store) async fn load_all_events_since(
        &self,
        from_position: Option<i64>,
    ) -> PgLoadResult<M> {
        Self::load_all_events_since_with_pool(&self.pool, from_position).await
    }

    /// Deserialise a batch of raw Postgres rows into [`StoredEvent`]s.
    pub(in crate::store) fn decode_rows(rows: Vec<PgRow>) -> PgLoadResult<M> {
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

    /// Load filtered events, merging `from_position` as a floor for every
    /// filter's `after_position`.
    ///
    /// When a filter already carries its own `after_position`, the *maximum*
    /// of the two values is used so that the caller's catch-up watermark never
    /// causes already-seen events to be re-delivered.
    pub(in crate::store) async fn load_events_since_filtered(
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
}
