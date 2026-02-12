# Projections

Projections are read models built by replaying events. They are query-oriented and eventually consistent.

## Basic Usage

Most projections should use this shape:

1. `#[derive(Projection)]`
2. `#[projection(events(...))]`
3. `impl ApplyProjection<E>` for each event

```rust,ignore
use sourcery::ApplyProjection;

#[derive(Debug, Default, sourcery::Projection)]
#[projection(events(FundsDeposited, FundsWithdrawn))]
pub struct AccountSummary {
    pub accounts: HashMap<String, i64>,
}

impl ApplyProjection<FundsDeposited> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, _: &Self::Metadata) {
        *self.accounts.entry(id.clone()).or_default() += event.amount;
    }
}

impl ApplyProjection<FundsWithdrawn> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsWithdrawn, _: &Self::Metadata) {
        *self.accounts.entry(id.clone()).or_default() -= event.amount;
    }
}
```

`#[projection(events(...))]` auto-generates `ProjectionFilters` for the common singleton case.

## Loading Projections

```rust,ignore
// Singleton projection (InstanceId = ())
let summary = repository
    .load_projection::<AccountSummary>(&())
    .await?;

// Instance projection (InstanceId = String)
let report = repository
    .load_projection::<CustomerReport>(&customer_id)
    .await?;
```

## When You Need Manual Filters

Implement `ProjectionFilters` yourself when you need:

- Dynamic filter logic
- `event_for` / `events_for` scoped filtering
- Non-default projection initialisation
- Custom `Id`, `InstanceId`, or `Metadata` with explicit control

## Trait Reference

### The `Projection` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:projection_trait}}
```

`Projection` provides a stable `KIND` used for projection snapshots.

### The `ApplyProjection<E>` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:apply_projection_trait}}
```

`ApplyProjection<E>` is where event handling logic lives.

### The `ProjectionFilters` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:projection_filters_trait}}
```

`ProjectionFilters` controls initialisation and filter selection.

## Advanced Filtering with `Filters`

```rust,ignore
Filters::new()
    .event::<FundsDeposited>()                 // Global: all aggregates
    .event_for::<Account, FundsWithdrawn>(&id) // Event from one aggregate instance
    .events::<AccountEvent>()                  // All variants in an event enum
    .events_for::<Account>(&id)                // All events for one aggregate instance
```

| Method | Scope |
|--------|-------|
| `.event::<E>()` | All events of type `E` across all aggregates |
| `.event_for::<A, E>(&id)` | Events of type `E` from a specific aggregate instance |
| `.events::<Enum>()` | All event kinds in a `ProjectionEvent` enum |
| `.events_for::<A>(&id)` | All event kinds for a specific aggregate instance |

## Multi-Aggregate Projections

```rust,ignore
impl ProjectionFilters for InventoryReport {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self { Self::default() }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<()>,
    {
        Filters::new()
            .event::<ProductCreated>()
            .event::<SaleRecorded>()
    }
}
```

## Metadata in Projections

```rust,ignore
impl ProjectionFilters for AuditLog {
    type Id = String;
    type InstanceId = ();
    type Metadata = EventMetadata;

    fn init((): &()) -> Self { Self::default() }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<EventMetadata>,
    {
        Filters::new().event::<FundsDeposited>()
    }
}

impl ApplyProjection<FundsDeposited> for AuditLog {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, meta: &Self::Metadata) {
        self.entries.push(AuditEntry {
            timestamp: meta.timestamp,
            user: meta.user_id.clone(),
            action: format!("Deposited {} to {}", event.amount, id),
        });
    }
}
```

## Singleton vs Instance Projections

- **Singleton** (`InstanceId = ()`): one global projection.
- **Instance** (`InstanceId = String` or custom type): one projection per instance ID.

### Example: Instance Projection with Scoped Filters

A projection parameterised by two aggregate IDs, subscribing to all events from each:

```rust,ignore
impl ProjectionFilters for AccountComparison {
    type Id = String;
    type InstanceId = (String, String);
    type Metadata = ();

    fn init(ids: &Self::InstanceId) -> Self {
        Self { left_id: ids.0.clone(), right_id: ids.1.clone(), ..Self::default() }
    }

    fn filters<S>(ids: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<()>,
    {
        let (left, right) = ids;
        Filters::new()
            .events_for::<Account>(left)
            .events_for::<Account>(right)
    }
}
```

`apply_projection` receives the aggregate ID, not the instance ID — store the IDs in `init` so the handler can distinguish which aggregate each event belongs to.

## Snapshotting Projections

Projections support snapshots for faster loading. See [Snapshots — Projection Snapshots](../advanced/snapshots.md#projection-snapshots) for details.

## Projections vs Aggregates

| Aspect | Aggregate | Projection |
|--------|-----------|------------|
| Purpose | Enforce invariants | Serve queries |
| State source | Own events only | Any events |
| Receives IDs | No (in envelope) | Yes (as parameter) |
| Receives metadata | No | Yes |
| Consistency | Strong | Eventual |
| Mutability | Via commands only | Rebuilt on demand |

## Next

[Stores](stores.md) — Event persistence and serialisation
