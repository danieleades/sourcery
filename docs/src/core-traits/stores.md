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

Uses JSON serialization via `serde_json` and deduplicates overlapping filters when loading.

## Committing Events

Events are committed atomically using one of two methods:

```rust,ignore
use nonempty::NonEmpty;

// Without version checking (last-writer-wins)
let events = NonEmpty::singleton(my_event);
let result = store
    .commit_events("account", &account_id, events, &metadata)
    .await?;

// With optimistic concurrency control
let events = NonEmpty::singleton(my_event);
let result = store
    .commit_events_optimistic("account", &account_id, Some(expected_version), events, &metadata)
    .await?;
```

The `commit_events_optimistic` method fails with a `ConcurrencyConflict` if:
- `expected_version` is `Some(v)` and the current stream version differs
- `expected_version` is `None` and the stream already has events

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

## Event Types

### `StoredEvent<Id, Pos, Data, M>`

An event loaded from the store. Contains the serialized data plus store-assigned metadata. Use `EventStore::decode_event()` to deserialize back to a domain event.

```rust,ignore
pub struct StoredEvent<Id, Pos, Data, M> {
    pub aggregate_kind: String,  // Aggregate type identifier
    pub aggregate_id: Id,        // Aggregate instance identifier
    pub kind: String,            // Event type identifier
    pub position: Pos,           // Global position assigned by store
    pub data: Data,              // Serialized event payload
    pub metadata: M,             // Infrastructure metadata
}
```

The `Data` type is generic over the serialization format (e.g., `serde_json::Value` for JSON stores). Store implementations specify their concrete type through the `EventStore::Data` associated type.

## The `SnapshotStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/snapshot.rs:snapshot_store_trait}}
```

See [Snapshots](../advanced/snapshots.md) for details.

## Implementing a Custom Store

See [Custom Stores](../advanced/custom-stores.md) for a guide on implementing `EventStore` for your database.

## Next

[The Aggregate Derive](../derive-macros/aggregate-derive.md) — Reducing boilerplate with macros
