//! Push-based subscriptions for the `PostgreSQL` event store.
//!
//! # The problem
//!
//! A `BIGSERIAL` position is assigned when a row is *inserted* but only becomes
//! visible when its transaction *commits*. Under concurrent writers those two
//! orders diverge: a transaction can reserve a low position, run for a while,
//! and commit *after* a higher-positioned transaction. A naive subscriber that
//! tracks "highest position seen" would step over the lower position forever —
//! and the gap is invisible, because uncommitted rows don't appear in queries.
//!
//! # The fix: transaction-id high-water-mark
//!
//! Every event is stamped with the writing transaction's id
//! (`xid BIGINT DEFAULT pg_current_xact_id()::text::bigint`). A subscriber
//! reads `pg_snapshot_xmin(pg_current_snapshot())` — the oldest transaction
//! still running — and delivers only events whose `xid` is **below** it.
//! Everything below `xmin` is settled forever, so it can never sprout a
//! lower-positioned sibling. Events at or above `xmin` are withheld until the
//! snapshot advances.
//!
//! Delivery is ordered by `(xid, position)` and the cursor is the
//! [`Watermark`] of the last delivered event. Because we never deliver a
//! transaction until its `xid < xmin`, and `xmin` only advances, every
//! subsequently delivered event has a strictly greater `xid` than anything
//! delivered before — so delivery is gap-free and strictly increasing in
//! `(xid, position)`, exactly what [`SubscribableStore`] requires.
//!
//! `LISTEN/NOTIFY` provides low-latency wake-ups; a periodic poll is the
//! correctness safety-net, because a rolled-back oldest transaction advances
//! `xmin` without emitting a notification.

use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sourcery_core::{
    snapshot::{
        OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore, inmemory::SnapshotPolicy,
    },
    store::{EventFilter, StoredEvent},
    subscription::{CheckpointStream, Checkpointed, SubscribableStore},
};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgListener};

use crate::{Error, Store};

/// `LISTEN/NOTIFY` channel used to wake live subscribers on commit.
pub const NOTIFY_CHANNEL: &str = "sourcery_events";

/// How often a subscriber re-polls when no notification arrives. This is the
/// safety-net that releases events gated behind a transaction that rolled back
/// (a rollback advances `xmin` without firing a notification).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum rows drained per query while catching up.
const BATCH_LIMIT: usize = 1024;

/// A result row: the writing transaction id paired with the decoded event.
type EventRow<M> = (i64, StoredEvent<uuid::Uuid, i64, serde_json::Value, M>);

/// Commit-ordered subscription cursor: `(xid, position)`, lexicographically
/// ordered (the `xid` field first, so derived `Ord` gives the right order).
///
/// `xid` is the writing transaction's id; `position` breaks ties between
/// events written by the same transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Watermark {
    /// Writing transaction id (`pg_current_xact_id()`), stored as `BIGINT`.
    pub xid: i64,
    /// Global event position (`BIGSERIAL`).
    pub position: i64,
}

/// Append the filter match clause: an `OR` of per-filter predicates, wrapped in
/// parentheses. An empty filter set matches nothing (`false`).
fn push_filter_predicate<'a>(
    qb: &mut QueryBuilder<'a, Postgres>,
    filters: &'a [EventFilter<uuid::Uuid, i64>],
) {
    qb.push("(");
    if filters.is_empty() {
        qb.push("false");
    }
    for (i, filter) in filters.iter().enumerate() {
        if i > 0 {
            qb.push(" OR ");
        }
        qb.push("(event_kind = ")
            .push_bind(filter.event_kind.as_str());
        if let Some(kind) = &filter.aggregate_kind {
            qb.push(" AND aggregate_kind = ").push_bind(kind.as_str());
        }
        if let Some(id) = &filter.aggregate_id {
            qb.push(" AND aggregate_id = ").push_bind(*id);
        }
        qb.push(")");
    }
    qb.push(")");
}

/// Build a [`StoredEvent`] and its `xid` from a result row.
fn row_to_event<M>(row: &sqlx::postgres::PgRow) -> Result<EventRow<M>, Error>
where
    M: serde::de::DeserializeOwned,
{
    let xid: i64 = row.try_get("xid")?;
    Ok((xid, crate::row_to_stored_event::<M>(row)?))
}

/// Load the next stable batch after `cursor`, ordered by `(xid, position)`.
///
/// Only events whose transaction is settled (`xid < xmin`) are returned, so the
/// result can never be undercut later by a lower-positioned in-flight write.
async fn load_stable_batch<M>(
    pool: &PgPool,
    filters: &[EventFilter<uuid::Uuid, i64>],
    cursor: Watermark,
) -> Result<Vec<EventRow<M>>, Error>
where
    M: serde::de::DeserializeOwned,
{
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT aggregate_kind, aggregate_id, event_kind, position, xid, data, metadata FROM \
         es_events WHERE xid < pg_snapshot_xmin(pg_current_snapshot())::text::bigint AND (xid, \
         position) > (",
    );
    qb.push_bind(cursor.xid)
        .push(", ")
        .push_bind(cursor.position)
        .push(") AND ");
    push_filter_predicate(&mut qb, filters);
    qb.push(" ORDER BY xid, position LIMIT ")
        .push_bind(i64::try_from(BATCH_LIMIT).expect("BATCH_LIMIT fits in i64"));

    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(row_to_event::<M>).collect()
}

impl<M> SubscribableStore for Store<M>
where
    M: Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Checkpoint = Watermark;

    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from: Option<Self::Checkpoint>,
    ) -> CheckpointStream<'_, Self> {
        let pool = self.pool.clone();
        let filters = filters.to_vec();
        let mut cursor = from.unwrap_or(Watermark {
            xid: 0,
            position: 0,
        });

        Box::pin(async_stream::stream! {
            // Best-effort live notifications; fall back to poll-only on failure.
            let mut listener = match PgListener::connect_with(&pool).await {
                Ok(mut l) => l.listen(NOTIFY_CHANNEL).await.ok().map(|()| l),
                Err(_) => None,
            };

            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                // Drain every stable event past the cursor.
                loop {
                    let batch = match load_stable_batch::<M>(&pool, &filters, cursor).await {
                        Ok(batch) => batch,
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    };

                    let drained = batch.len();
                    for (xid, event) in batch {
                        cursor = Watermark { xid, position: event.position };
                        yield Ok(Checkpointed { checkpoint: cursor, event });
                    }

                    if drained < BATCH_LIMIT {
                        break;
                    }
                }

                // Wait for a commit notification or the poll tick.
                match listener.as_mut() {
                    Some(l) => {
                        tokio::select! {
                            _ = l.recv() => {}
                            _ = ticker.tick() => {}
                        }
                    }
                    None => {
                        ticker.tick().await;
                    }
                }
            }
        })
    }

    async fn current_checkpoint(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
    ) -> Result<Option<Self::Checkpoint>, Self::Error> {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT xid, position FROM es_events WHERE ");
        push_filter_predicate(&mut qb, filters);
        qb.push(" ORDER BY xid DESC, position DESC LIMIT 1");

        let row = qb.build().fetch_optional(&self.pool).await?;
        Ok(row.map(|row| Watermark {
            xid: row.get("xid"),
            position: row.get("position"),
        }))
    }
}

/// Durable storage for subscription checkpoints (and the projection state they
/// resume).
///
/// This is a [`SnapshotStore`] whose position is a [`Watermark`], backed by a
/// dedicated `es_subscriptions` table. It is keyed by `(projection_kind,
/// instance_id)`, where `instance_id` is the JSON serialisation of the
/// projection's `InstanceId` — so any `Serialize` instance type works,
/// including `()` for singletons.
///
/// Pass it to
/// [`Repository::subscribe_with_snapshots`](sourcery_core::repository::Repository::subscribe_with_snapshots)
/// to make a subscription resumable across restarts.
#[derive(Clone)]
pub struct CheckpointStore {
    pool: PgPool,
    policy: SnapshotPolicy,
}

impl std::fmt::Debug for CheckpointStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointStore")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl CheckpointStore {
    /// Persist the checkpoint after every processed event.
    #[must_use]
    pub const fn always(pool: PgPool) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::Always,
        }
    }

    /// Persist the checkpoint every `n` events.
    #[must_use]
    pub const fn every(pool: PgPool, n: u64) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::EveryNEvents(n),
        }
    }

    /// Never persist (load-only); the subscription replays from scratch on
    /// restart.
    #[must_use]
    pub const fn never(pool: PgPool) -> Self {
        Self {
            pool,
            policy: SnapshotPolicy::Never,
        }
    }

    /// Apply the subscription-checkpoint schema (idempotent).
    ///
    /// # Errors
    ///
    /// Returns a `sqlx::Error` if the schema creation query fails.
    #[tracing::instrument(skip(self))]
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS es_subscriptions (
                projection_kind TEXT NOT NULL,
                instance_id     TEXT NOT NULL,
                xid             BIGINT NOT NULL,
                position        BIGINT NOT NULL,
                data            JSONB NOT NULL,
                created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                PRIMARY KEY (projection_kind, instance_id)
            )
            ",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Serialise an instance identifier into the `TEXT` storage key.
fn instance_key<K: Serialize>(id: &K) -> Result<String, Error> {
    serde_json::to_string(id).map_err(|e| Error::Serialization(Box::new(e)))
}

impl<K> SnapshotStore<K> for CheckpointStore
where
    K: Serialize + Send + Sync,
{
    type Error = Error;
    type Position = Watermark;

    #[tracing::instrument(skip(self, id))]
    async fn load<T>(&self, kind: &str, id: &K) -> Result<Option<Snapshot<Watermark, T>>, Error>
    where
        T: DeserializeOwned,
    {
        let instance = instance_key(id)?;
        let row = sqlx::query(
            r"
            SELECT xid, position, data
            FROM es_subscriptions
            WHERE projection_kind = $1 AND instance_id = $2
            ",
        )
        .bind(kind)
        .bind(&instance)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let xid: i64 = row.get("xid");
            let position: i64 = row.get("position");
            let data: sqlx::types::Json<serde_json::Value> = row.get("data");
            serde_json::from_value::<T>(data.0)
                .map(|data| Snapshot {
                    position: Watermark { xid, position },
                    data,
                })
                .map_err(|e| Error::Deserialization(Box::new(e)))
        })
        .transpose()
    }

    #[tracing::instrument(skip(self, id, create_snapshot))]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        kind: &str,
        id: &K,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE> + Send,
    {
        let instance = instance_key(id).map_err(OfferSnapshotError::Snapshot)?;

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

        let Some((watermark, data)) = prepared else {
            return Ok(SnapshotOffer::Declined);
        };

        // Upsert, but only advance when the new checkpoint is strictly ahead, so
        // a slower concurrent writer can never roll the cursor backwards.
        let result = sqlx::query(
            r"
            INSERT INTO es_subscriptions (projection_kind, instance_id, xid, position, data)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (projection_kind, instance_id)
            DO UPDATE SET xid = EXCLUDED.xid, position = EXCLUDED.position, data = EXCLUDED.data
            WHERE (es_subscriptions.xid, es_subscriptions.position)
                < (EXCLUDED.xid, EXCLUDED.position)
            ",
        )
        .bind(kind)
        .bind(&instance)
        .bind(watermark.xid)
        .bind(watermark.position)
        .bind(sqlx::types::Json(data))
        .execute(&self.pool)
        .await
        .map_err(|e| OfferSnapshotError::Snapshot(Error::Database(e)))?;

        Ok(if result.rows_affected() > 0 {
            SnapshotOffer::Stored
        } else {
            SnapshotOffer::Declined
        })
    }
}
