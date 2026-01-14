# Stores

The crate separates storage concerns into two traits: `EventStore` for event persistence and `SnapshotStore` for aggregate snapshots. Serialization is handled internally by stores using `serde`.

## The `EventStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/store.rs:event_store_trait}}
```

## Built-in: `inmemory::Store`

For testing and prototyping:

```rust,ignore
use sourcery::store::inmemory;

// With unit metadata
let store: inmemory::Store<String, ()> = inmemory::Store::new();

// With custom metadata
let store: inmemory::Store<String, MyMetadata> = inmemory::Store::new();
```

Features:
- Uses `u64` positions (global sequence number)
- Events stored in memory (lost on drop)
- JSON serialization via `serde_json`
- Deduplicates overlapping filters when loading

## Transactions

Events are appended within a transaction for atomicity:

```rust,ignore
use sourcery::concurrency::Unchecked;

let mut tx = store.begin::<Unchecked>("account", "ACC-001".to_string(), None);
tx.append(&event, metadata.clone())?;
tx.append(&event2, metadata.clone())?;
tx.commit().await?; // Events visible only after commit
```

If the transaction is dropped without committing, no events are persisted.

## Event Filters

Control which events to load:

```rust,ignore
// All events of a specific kind (across all aggregates)
EventFilter::for_event("account.deposited")

// All events for a specific aggregate instance
EventFilter::for_aggregate("account.deposited", "account", "ACC-001")

// Events after a position (for incremental loading)
EventFilter::for_event("account.deposited").after(100)
```

## The `StoredEventView` Trait

Events loaded from the store implement this trait:

```rust,ignore
pub trait StoredEventView {
    type Id;
    type Pos;
    type Metadata;

    fn aggregate_kind(&self) -> &str;
    fn aggregate_id(&self) -> &Self::Id;
    fn kind(&self) -> &str;
    fn position(&self) -> Self::Pos;
    fn metadata(&self) -> &Self::Metadata;
}
```

Stores provide their own `StoredEvent` type that implements this trait, allowing different internal representations (e.g., JSONB, binary) while exposing a uniform interface.

## The `SnapshotStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/snapshot.rs:snapshot_store_trait}}
```

See [Snapshots](../advanced/snapshots.md) for details.

## Implementing a Custom Store

See [Custom Stores](../advanced/custom-stores.md) for a guide on implementing `EventStore` for your database.

## Next

[The Aggregate Derive](../derive-macros/aggregate-derive.md) — Reducing boilerplate with macros
