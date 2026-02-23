# Projections

Projections are read models built by replaying events. They are query-oriented and eventually consistent.

## Recommended Default Path

For most projections, keep to this shape:

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

`#[projection(events(...))]` auto-generates `Projection::filters()` for the common case.

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

## When to Implement `Projection` Manually

Use manual filters when you need:

- Dynamic filtering
- Scoped filtering (`event_for` / `events_for`)
- Non-default initialisation
- Full control over `Id`, `InstanceId`, or `Metadata`

### Example: Scoped Instance Filter

```rust,ignore
impl Projection for AccountComparison {
    type Id = String;
    type InstanceId = (String, String);
    type Metadata = ();

    fn init(ids: &Self::InstanceId) -> Self {
        Self {
            left_id: ids.0.clone(),
            right_id: ids.1.clone(),
            ..Self::default()
        }
    }

    fn filters<S>(ids: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        let (left, right) = ids;
        Filters::new()
            .events_for::<Account>(left)
            .events_for::<Account>(right)
    }
}
```

## `Filters` Cheat Sheet

```rust,ignore
Filters::new()
    .event::<FundsDeposited>()                 // Global: all aggregates
    .event_for::<Account, FundsWithdrawn>(&id) // One event type, one aggregate instance
    .events::<AccountEvent>()                  // All kinds in an event enum
    .events_for::<Account>(&id)                // All event kinds for one aggregate instance
```

| Method | Scope |
|--------|-------|
| `.event::<E>()` | All events of type `E` across all aggregates |
| `.event_for::<A, E>(&id)` | Events of type `E` from one aggregate instance |
| `.events::<Enum>()` | All event kinds in a `ProjectionEvent` enum |
| `.events_for::<A>(&id)` | All event kinds for one aggregate instance |

## Metadata in Projections

Use `metadata = ...` in the derive attribute for the common case:

```rust,ignore
#[derive(Debug, Default, sourcery::Projection)]
#[projection(metadata = EventMetadata, events(FundsDeposited))]
struct AuditLog {
    entries: Vec<AuditEntry>,
}

impl ApplyProjection<FundsDeposited> for AuditLog {
    fn apply_projection(
        &mut self,
        id: &Self::Id,
        event: &FundsDeposited,
        meta: &Self::Metadata,
    ) {
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

`apply_projection` receives the aggregate ID, not the instance ID. If handlers need instance context, store it during `init`.

## Trait Reference

For full signatures, see the API docs:

- `Projection`
- `ApplyProjection<E>`

You can also inspect the trait definitions directly:

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:projection_trait}}
{{#include ../../../sourcery-core/src/projection.rs:apply_projection_trait}}
```

## Snapshotting Projections

Projections support snapshots for faster loading. See [Snapshots — Projection Snapshots](../advanced/snapshots.md#projection-snapshots).

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
