# Snapshots

Replaying events gets expensive as aggregates accumulate history. Snapshots checkpoint aggregate state, allowing you to skip replaying old events.

## How Snapshots Work

```d2
shape: sequence_diagram

App: Application
Snap: SnapshotStore
Store: EventStore
Agg: Aggregate

App -> Snap: 'load("account", "ACC-001")'
Snap -> App: "Snapshot at position 1000" {style.stroke-dash: 3}
App -> Agg: "Deserialize snapshot"
App -> Store: "load_events(after: 1000)"
Store -> App: "Events 1001-1050" {style.stroke-dash: 3}
App -> Agg: "apply(event) for each"
```

Instead of replaying all 1050 events, load the snapshot and replay only the 50 new ones.

## Enabling Snapshots

Use `with_snapshots()` when creating the repository:

```rust,ignore
use sourcery::{Repository, snapshot::inmemory, store::inmemory as event_store};

let event_store = event_store::Store::new();
let snapshot_store = inmemory::Store::every(100);

let repository = Repository::new(event_store)
    .with_snapshots(snapshot_store);
```

## Snapshot Policies

`snapshot::inmemory::Store` provides three policies:

### Always

Save a snapshot after every command:

```rust,ignore
let snapshots = inmemory::Store::always();
```

Use for: Aggregates with expensive replay, testing.

### Every N Events

Save after accumulating N events since the last snapshot:

```rust,ignore
let snapshots = inmemory::Store::every(100);
```

Use for: Production workloads balancing storage vs. replay cost.

### Never

Never save (load-only mode):

```rust,ignore
let snapshots = inmemory::Store::never();
```

Use for: Read-only replicas, debugging.

## The `SnapshotStore` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/snapshot.rs:snapshot_store_trait}}
```

The repository calls `offer_snapshot` after successfully appending new events. Implementations may decline without invoking `create_snapshot`, avoiding unnecessary snapshot serialization.

## The `Snapshot` Type

```rust,ignore
pub struct Snapshot<Pos, Data> {
    pub position: Pos,
    pub data: Data,
}
```

The `position` indicates which event this snapshot was taken after. When loading, only events after this position need to be replayed.

## Projection Snapshots

Projection snapshots use the same store and are keyed by `(P::KIND, instance_id)`.
Use `load_projection_with_snapshot` on a repository configured with snapshots:

```rust,ignore
#[derive(Default, Serialize, Deserialize, sourcery::Projection)]
#[projection(kind = "loyalty.summary")]
struct LoyaltySummary {
    total_earned: u64,
}

// (Subscribable impl defines filters and associated types)

let repo = Repository::new(store).with_snapshots(inmemory::Store::every(100));

let summary: LoyaltySummary = repo
    .load_projection_with_snapshot::<LoyaltySummary>(&instance_id)
    .await?;
```

Singleton projections use `InstanceId = ()` and pass `&()`. Instance projections
pass their instance identifier.

Projection snapshots require a globally ordered store (`S: GloballyOrderedStore`)
and `P: Serialize + DeserializeOwned`.

## When to Snapshot

| Aggregate Type | Recommendation |
|---------------|----------------|
| Short-lived (< 100 events) | Skip snapshots |
| Medium (100-1000 events) | Every 100-500 events |
| Long-lived (1000+ events) | Every 100 events |
| High-throughput | Every N events, tuned to your SLA |

## Implementing a Custom Store

For production, implement `SnapshotStore` with your database. See [Custom Stores](custom-stores.md) for a complete guide.

## Snapshot Invalidation

Snapshots are tied to your aggregate's serialized form. When you change the struct:

1. **Add fields** — Use `#[serde(default)]` for backwards compatibility
2. **Remove fields** — Old snapshots still deserialize (extra fields ignored)
3. **Rename fields** — Use `#[serde(alias = "old_name")]`
4. **Change types** — Old snapshots become invalid; delete them

For major changes, delete old snapshots and let them rebuild from events.

## Next

[Event Versioning](event-versioning.md) — Evolving event schemas over time
