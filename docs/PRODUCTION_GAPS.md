# Production Readiness Gap Analysis

This document identifies the top 3 missing features that prevent Sourcery from being production-ready for most event-sourced systems.

## Executive Summary

Sourcery is a well-engineered, thoughtfully designed event-sourcing library with excellent documentation and a clean API. However, three critical features are missing that most production systems require:

| Priority | Feature | Impact |
|----------|---------|--------|
| 1 | Event Subscriptions/Streaming | Projections must be fully rebuilt on every read |
| 2 | Durable Snapshot Store Implementation | No production-ready implementation provided |
| 3 | Outbox Pattern | No reliable integration with external systems |

---

## 1. Event Subscriptions/Streaming (Critical)

### What's Missing

Currently, Sourcery only supports **pull-based** event loading via `load_events()`:

```rust
// Current API: Load all events matching filters
let events = store.load_events(&filters).await?;
```

There is **no push-based subscription mechanism** to receive events as they're written. This means:

- **Projections are fully rebuilt on every read** (`projection.rs:341-387`)
- **No incremental projection updates** - even if 1 event was added, you replay all events
- **No live streaming** for real-time dashboards or notifications
- **No catch-up subscriptions** to resume from a known position

### Why This Matters for Production

In production systems:

1. **Read models become stale immediately** - If you build a projection, query it, then a new event arrives, your next query must rebuild everything
2. **Performance degrades with event count** - Loading 10,000 events to apply 1 new one is wasteful
3. **No real-time capabilities** - Can't push updates to clients as events occur
4. **Horizontal scaling impossible** - Can't have multiple projection workers processing event streams

### What's Needed

A subscription API that supports:

```rust
// Catch-up subscription (start from position, catch up to head, then stream)
let subscription = store.subscribe()
    .from_position(last_known_position)
    .filter_events::<AccountEvent>()
    .start()
    .await?;

while let Some(event) = subscription.next().await {
    projection.apply(&event);
    checkpoint.save(event.position).await?;
}

// Live-only subscription (only new events)
let live = store.subscribe_live::<OrderPlaced>().await?;
```

### Current Workaround

None. Users must either:
- Accept full rebuilds on every projection load
- Build their own polling mechanism outside the library
- Couple directly to PostgreSQL LISTEN/NOTIFY

### Evidence

From `projection.rs:341-387`:
```rust
pub async fn load_for(
    self,
    instance_id: &P::InstanceId,
) -> Result<P, ProjectionError<...>> {
    // Loads ALL events every time
    let events = self.store.load_events(&self.filters).await...;
    for stored in events {
        // Applies every event from scratch
        (handler)(&mut projection, ...)?;
    }
}
```

---

## 2. No Durable Snapshot Store Implementation Provided (High)

### What's Missing

Sourcery has **full snapshot support** for both aggregates and projections via the `SnapshotStore` trait. However, **no durable implementation is provided**:

| Implementation | Durability | Production Ready |
|---------------|------------|------------------|
| `NoSnapshots` | N/A | N/A (opt-out) |
| `InMemorySnapshotStore` | Memory only | No (lost on restart) |
| `PostgresSnapshotStore` | **Not provided** | - |

The snapshot *mechanism* is complete - what's missing is a production-ready *implementation*.

### Why This Matters for Production

Without a durable snapshot store implementation:

1. **Users must implement their own** - Duplicating work for every production deployment
2. **Snapshots lost on restart** - In-memory snapshots vanish, negating their benefit
3. **Cold starts replay all events** - An aggregate with 10,000 events replays all 10,000 after restart

### What's Needed

A PostgreSQL snapshot store with schema:

```sql
CREATE TABLE es_snapshots (
    aggregate_kind TEXT NOT NULL,
    aggregate_id   UUID NOT NULL,
    position       BIGINT NOT NULL,
    data           BYTEA NOT NULL,
    created_at     TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (aggregate_kind, aggregate_id)
);
```

And implementation:

```rust
pub struct PostgresSnapshotStore {
    pool: PgPool,
}

impl SnapshotStore<Uuid> for PostgresSnapshotStore {
    async fn load(&self, kind: &str, id: &Uuid)
        -> Result<Option<Snapshot<i64>>, Error>;

    async fn offer_snapshot<CE, Create>(&self, ...)
        -> Result<SnapshotOffer, OfferSnapshotError<...>>;
}
```

### Current Workaround

Users must implement their own `SnapshotStore` for PostgreSQL, duplicating the logic in `InMemorySnapshotStore` but with SQL.

### Evidence

From `snapshot.rs:240-262`:
```rust
/// In-memory snapshot store with configurable policy.
///
/// This is a reference implementation suitable for testing and development.
/// Production systems should implement [`SnapshotStore`] with durable storage.
pub struct InMemorySnapshotStore<Pos> {
    snapshots: Arc<RwLock<HashMap<SnapshotKey, Snapshot<Pos>>>>,
    policy: SnapshotPolicy,
}
```

The documentation explicitly states this is for testing, but no production implementation exists.

---

## 3. Outbox Pattern (High)

### What's Missing

There is **no transactional outbox** for reliable event publishing to external systems. The design decisions document (`design-decisions.md:114-137`) explicitly states:

> **No outbox, no scheduler, no event streaming, no saga orchestrator.**

### Why This Matters for Production

Event sourcing systems rarely exist in isolation. They need to:

1. **Publish events to message brokers** (Kafka, RabbitMQ, NATS)
2. **Notify other microservices** of domain changes
3. **Trigger workflows** in external systems

Without an outbox pattern:

- **Dual-write problem**: Writing to event store and message broker aren't atomic
- **Lost events**: If publish fails after event store commit, event is lost
- **Inconsistent systems**: External systems may miss events or see duplicates

### What's Needed

An outbox that:
1. Stores "to publish" records in the same transaction as events
2. Has a background worker to publish and mark as delivered
3. Supports retry with backoff for failures

```rust
pub trait OutboxStore {
    async fn record_for_publishing(
        &self,
        event: &StoredEvent<...>,
        destination: &str,
    ) -> Result<(), Error>;

    async fn mark_published(&self, outbox_id: i64) -> Result<(), Error>;

    async fn get_unpublished(&self, limit: usize)
        -> Result<Vec<OutboxEntry>, Error>;
}

// Usage with repository
repository
    .execute_command_with_outbox::<Account, Deposit>(...)
    .await?;  // Event stored + outbox entry in same transaction
```

### Current Workaround

Users must:
1. Build their own outbox table
2. Wrap event appends in larger transactions
3. Implement their own polling/publishing worker
4. Handle idempotency on the consumer side

This is error-prone and duplicates significant infrastructure work.

### Evidence

From `design-decisions.md:114-137`:
```markdown
## Minimal Infrastructure

**Decision**: No outbox, no scheduler, no event streaming, no saga orchestrator.

**Why**:
- Event sourcing is the foundation; infrastructure varies by use case
...
- May be added later.
```

---

## Summary & Recommendations

### Immediate Actions (Blocking Production)

1. **Event Subscriptions** - Add `subscribe()` API with catch-up and live modes
2. **PostgresSnapshotStore** - Provide a durable `SnapshotStore` implementation (trait already exists)

### Near-Term (Enables Integration)

3. **Outbox Pattern** - Transactional outbox with background publisher

### Comparative Analysis

| Feature | Sourcery | EventStoreDB | Marten | Axon |
|---------|----------|--------------|--------|------|
| Event Subscriptions | No | Yes | Yes | Yes |
| Durable Snapshots | No | Yes | Yes | Yes |
| Outbox Pattern | No | N/A (built-in) | Yes | Yes |
| Production Maturity | Pre-1.0 | Mature | Mature | Mature |

### Path Forward

These three features represent the minimum viable set for most production event-sourced systems. They're also mentioned in the design decisions as things that "may be added later," suggesting they're recognized gaps.

The library's architecture is well-designed to accommodate these additions:
- `EventStore` trait can be extended with `subscribe()` method
- `SnapshotStore` trait already exists; PostgreSQL impl is straightforward
- Outbox can be a new optional module with its own trait

---

*Analysis conducted: 2026-01-04*
*Codebase version: 0.1.0 (commit 0ced803)*
