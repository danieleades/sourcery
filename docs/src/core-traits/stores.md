# Stores

The crate separates storage concerns into two traits:

- `EventStore` for event persistence
- `SnapshotStore` for aggregate/projection snapshots

Stores own serialisation and deserialisation.

## The `EventStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/store.rs:event_store_trait}}
```

## Built-in: `inmemory::Store`

For testing and prototyping:

```rust,ignore
use sourcery::store::inmemory;

// Unit metadata
let store: inmemory::Store<String, ()> = inmemory::Store::new();

// Custom metadata
let store: inmemory::Store<String, MyMetadata> = inmemory::Store::new();
```

The in-memory store uses `serde_json` and supports globally ordered positions.

## Committing Events

```rust,ignore
use nonempty::NonEmpty;

let events = NonEmpty::singleton(my_event);

// Unchecked (last-writer-wins)
store.commit_events("account", &account_id, events.clone(), &metadata).await?;

// Optimistic concurrency
store
    .commit_events_optimistic("account", &account_id, Some(expected_version), events, &metadata)
    .await?;
```

`commit_events_optimistic` fails with `ConcurrencyConflict` when the expected and actual stream versions differ.

## Loading Events with Filters

```rust,ignore
// All events of one kind
EventFilter::for_event("account.deposited")

// One aggregate instance
EventFilter::for_aggregate("account.deposited", "account", "ACC-001")

// Incremental loading
EventFilter::for_event("account.deposited").after(100)
```

## `StoredEvent` in Practice

`load_events` returns `StoredEvent<Id, Pos, Data, Metadata>`, containing:

- envelope fields (`aggregate_kind`, `aggregate_id`, `kind`, `position`)
- serialised payload (`data`)
- metadata (`metadata`)

Use `EventStore::decode_event()` to deserialise payloads into domain events.

For the exact field layout, see API docs for `StoredEvent`.

## The `SnapshotStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/snapshot.rs:snapshot_store_trait}}
```

See [Snapshots](../advanced/snapshots.md) for policy and usage guidance.

## Implementing a Custom Store

See [Custom Stores](../advanced/custom-stores.md) for a practical guide.

## Next

[The Aggregate Derive](../derive-macros/aggregate-derive.md) — Reducing boilerplate with macros
