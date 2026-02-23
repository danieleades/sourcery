# Custom Stores

The `inmemory::Store` is useful for testing, but production systems need durable storage. This guide walks through implementing `EventStore` for your database.

## The EventStore Trait

See [Stores](../core-traits/stores.md) for the full trait definition.

## Design Decisions

### Position Type

Choose based on your ordering needs:

| Position Type | Use Case |
|---------------|----------|
| `()` | Unordered, append-only log |
| `u64` / `i64` | Global sequence number |
| `(i64, i32)` | Timestamp + sequence for distributed systems |

### Storage Schema

A typical SQL schema:

```sql
CREATE TABLE events (
    position BIGSERIAL PRIMARY KEY,
    aggregate_kind TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    data JSONB NOT NULL,
    metadata JSONB NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX idx_events_aggregate
    ON events (aggregate_kind, aggregate_id, position);

CREATE INDEX idx_events_kind
    ON events (event_kind, position);
```

## Implementation Skeleton

```rust,ignore
use std::future::Future;
use nonempty::NonEmpty;
use sourcery::event::{DomainEvent, EventKind};
use sourcery::store::{
    CommitError, Committed, EventFilter, EventStore,
    OptimisticCommitError, StoredEvent,
};
use sourcery::concurrency::ConcurrencyConflict;

pub struct PostgresEventStore {
    pool: sqlx::PgPool,
}

impl EventStore for PostgresEventStore {
    type Id = String;
    type Position = i64;
    type Error = sqlx::Error;
    type Metadata = serde_json::Value;
    type Data = serde_json::Value;

    fn decode_event<E>(
        &self,
        stored: &StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>,
    ) -> Result<E, Self::Error>
    where
        E: DomainEvent + serde::de::DeserializeOwned
    {
        serde_json::from_value(stored.data.clone())
            .map_err(Into::into)
    }

    fn stream_version<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
    ) -> impl Future<Output = Result<Option<Self::Position>, Self::Error>> + Send + 'a
    {
        async move {
            todo!("SELECT MAX(position) WHERE aggregate_kind = $1 AND aggregate_id = $2")
        }
    }

    fn commit_events<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<Output = Result<Committed<Self::Position>, CommitError<Self::Error>>> + Send + 'a
    where
        E: EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        async move {
            // Serialize each event with e.kind() and serde_json::to_value(e)
            // INSERT atomically, return Committed { last_position }
            todo!()
        }
    }

    fn commit_events_optimistic<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<E>,
        metadata: &'a Self::Metadata,
    ) -> impl Future<Output = Result<Committed<Self::Position>, OptimisticCommitError<Self::Position, Self::Error>>> + Send + 'a
    where
        E: EventKind + serde::Serialize + Send + Sync + 'a,
        Self::Metadata: Clone,
    {
        async move {
            // Check version, return Conflict on mismatch
            // Serialize and INSERT atomically
            todo!()
        }
    }

    fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<Self::Id, Self::Position>],
    ) -> impl Future<Output = Result<Vec<StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>>, Self::Error>> + Send + 'a
    {
        async move { todo!("SELECT with filters") }
    }
}
```

## Loading Events

Build a `WHERE` clause from filters with `OR`, ordered by position:

```rust,ignore
fn load_events(&self, filters: &[EventFilter<String, i64>])
    -> Result<Vec<StoredEvent>, sqlx::Error>
{
    // WHERE (event_kind = 'x' AND position > N)
    //    OR (aggregate_kind = 'a' AND aggregate_id = 'b')
    // ORDER BY position ASC
    todo!()
}
```

## Optimistic Concurrency

For systems requiring strict ordering, add version checking:

```sql
ALTER TABLE events ADD COLUMN stream_version INT NOT NULL;

CREATE UNIQUE INDEX idx_events_stream_version
    ON events (aggregate_kind, aggregate_id, stream_version);
```

```rust,ignore
// In commit_events_optimistic:
// 1. Get current max version for stream
// 2. Compare against expected_version
// 3. If mismatch, return OptimisticCommitError::Conflict(ConcurrencyConflict { expected, actual })
// 4. Insert with version + 1
// 5. Handle unique constraint violation as concurrency conflict
```

## Event Stores for Different Databases

### DynamoDB

```text
Table: events
  PK: {aggregate_kind}#{aggregate_id}
  SK: {position:012d}  (zero-padded for sorting)
  GSI: event_kind-position-index
```

### MongoDB

```javascript
{
  _id: ObjectId,
  aggregateKind: "account",
  aggregateId: "ACC-001",
  eventKind: "account.deposited",
  position: NumberLong(1234),
  data: { ... },
  metadata: { ... }
}
```

### S3 (Append-Only Log)

```text
s3://bucket/events/{aggregate_kind}/{aggregate_id}/{position}.json
s3://bucket/events-by-kind/{event_kind}/{position}.json  (symlinks/copies)
```

## Testing Your Store

Use the same test patterns as `inmemory::Store`:

```rust,ignore
#[tokio::test]
async fn test_commit_and_load() {
    let store = PostgresEventStore::new(test_pool()).await;

    // Commit events
    let events = NonEmpty::singleton(MyEvent { data: "test".into() });
    store.commit_events("account", &"ACC-001".into(), events, &metadata).await?;

    // Load and verify
    let loaded = store
        .load_events(&[
            EventFilter::for_aggregate("my-event", "account", "ACC-001".to_string())
        ])
        .await?;

    assert_eq!(loaded.len(), 1);
}

#[tokio::test]
async fn test_optimistic_concurrency() {
    let store = PostgresEventStore::new(test_pool()).await;

    // Create initial event
    let events = NonEmpty::singleton(MyEvent { data: "first".into() });
    store.commit_events_optimistic("account", &id, None, events, &metadata).await?;

    // Concurrent write with wrong version should fail
    let events = NonEmpty::singleton(MyEvent { data: "second".into() });
    let result = store
        .commit_events_optimistic("account", &id, Some(999), events, &metadata)
        .await;

    assert!(matches!(result, Err(OptimisticCommitError::Conflict(_))));
}
```

## Next

[Test Framework](../testing/test-framework.md) — Testing aggregates in isolation
