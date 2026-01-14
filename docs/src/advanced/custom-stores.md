# Custom Stores

The `inmemory::Store` is useful for testing, but production systems need durable storage. This guide walks through implementing `EventStore` for your database.

## The EventStore Trait

```rust,ignore
{{#include ../../../sourcery-core/src/store.rs:event_store_trait}}
```

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
use sourcery::store::{
    AppendError, AppendResult, EventFilter, EventStore, NonEmpty,
    StoredEventView, Transaction,
};
use sourcery::concurrency::ConcurrencyStrategy;

pub struct PostgresEventStore {
    pool: sqlx::PgPool,
}

// Your stored event type
pub struct StoredEvent {
    aggregate_kind: String,
    aggregate_id: String,
    kind: String,
    position: i64,
    data: serde_json::Value,
    metadata: serde_json::Value,
}

impl StoredEventView for StoredEvent {
    type Id = String;
    type Pos = i64;
    type Metadata = serde_json::Value;

    fn aggregate_kind(&self) -> &str { &self.aggregate_kind }
    fn aggregate_id(&self) -> &Self::Id { &self.aggregate_id }
    fn kind(&self) -> &str { &self.kind }
    fn position(&self) -> Self::Pos { self.position }
    fn metadata(&self) -> &Self::Metadata { &self.metadata }
}

impl EventStore for PostgresEventStore {
    type Id = String;
    type Position = i64;
    type Error = sqlx::Error;
    type Metadata = serde_json::Value;
    type StoredEvent = StoredEvent;
    type StagedEvent = StagedEvent; // Your staged event type

    fn stage_event<E>(&self, event: &E, metadata: Self::Metadata)
        -> Result<Self::StagedEvent, Self::Error>
    where
        E: EventKind + serde::Serialize
    {
        todo!("Serialize event to StagedEvent")
    }

    fn decode_event<E>(&self, stored: &Self::StoredEvent)
        -> Result<E, Self::Error>
    where
        E: DomainEvent + serde::de::DeserializeOwned
    {
        todo!("Deserialize from stored.data")
    }

    fn stream_version<'a>(&'a self, aggregate_kind: &'a str, aggregate_id: &'a Self::Id)
        -> impl Future<Output = Result<Option<Self::Position>, Self::Error>> + Send + 'a
    {
        async move {
            todo!("SELECT MAX(position) WHERE aggregate_kind = $1 AND aggregate_id = $2")
        }
    }

    fn begin<C: ConcurrencyStrategy>(
        &self,
        aggregate_kind: &str,
        aggregate_id: Self::Id,
        expected_version: Option<Self::Position>
    ) -> Transaction<'_, Self, C> {
        Transaction::new(self, aggregate_kind.to_string(), aggregate_id, expected_version)
    }

    fn append<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        expected_version: Option<Self::Position>,
        events: NonEmpty<Self::StagedEvent>
    ) -> impl Future<Output = Result<AppendResult<Self::Position>, AppendError<Self::Position, Self::Error>>> + Send + 'a
    {
        async move { todo!("INSERT with version check") }
    }

    fn append_expecting_new<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a Self::Id,
        events: NonEmpty<Self::StagedEvent>
    ) -> impl Future<Output = Result<AppendResult<Self::Position>, AppendError<Self::Position, Self::Error>>> + Send + 'a
    {
        async move { todo!("INSERT only if stream empty") }
    }

    fn load_events<'a>(&'a self, filters: &'a [EventFilter<Self::Id, Self::Position>])
        -> impl Future<Output = Result<Vec<Self::StoredEvent>, Self::Error>> + Send + 'a
    {
        async move { todo!("SELECT with filters") }
    }
}
```

## Loading Events

Handle multiple filters efficiently:

```rust,ignore
fn load_events(&self, filters: &[EventFilter<String, i64>])
    -> Result<Vec<StoredEvent>, sqlx::Error>
{
    // Deduplicate overlapping filters
    // Build WHERE clause:
    //   (event_kind = 'x' AND position > N)
    //   OR (event_kind = 'y' AND aggregate_kind = 'a' AND aggregate_id = 'b')
    // ORDER BY position ASC
    // Map rows to StoredEvent
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
// In append:
// 1. Get current max version for stream
// 2. Insert with version + 1
// 3. Handle unique constraint violation as concurrency conflict
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
async fn test_append_and_load() {
    let store = PostgresEventStore::new(test_pool()).await;

    // Append events
    let mut tx = store.begin::<Unchecked>("account", "ACC-001".to_string(), None);
    tx.append(&event, metadata)?;
    tx.commit().await?;

    // Load and verify
    let events = store
        .load_events(&[
        EventFilter::for_aggregate("account.deposited", "account", "ACC-001".to_string())
    ])
        .await?;

    assert_eq!(events.len(), 1);
}
```

## Next

[Test Framework](../testing/test-framework.md) — Testing aggregates in isolation
