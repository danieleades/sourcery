# Projections

Projections are read models built by replaying events. They're optimized for queries rather than consistency. A single event stream can feed many projections, each structuring data differently.

## The `Subscribable` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:subscribable_trait}}
```

`Subscribable` is the base trait for types that consume events. It defines:

- **`Id`** — the aggregate identifier type
- **`InstanceId`** — the projection instance identifier (use `()` for singletons)
- **`Metadata`** — the metadata type passed to event handlers
- **`init()`** — constructs a fresh instance from the instance identifier
- **`filters()`** — builds the filter set and handler map centrally

## The `Projection` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:projection_trait}}
```

Extends `Subscribable` with a stable `KIND` identifier used for snapshot storage. Derivable via `#[derive(Projection)]`.

## The `ApplyProjection<E>` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:apply_projection_trait}}
```

Projections receive the aggregate ID and metadata alongside each event, enabling cross-aggregate views and audit trails.

## Basic Example

```rust,ignore
use sourcery::{ApplyProjection, Filters, Subscribable, store::EventStore};

#[derive(Debug, Default, sourcery::Projection)]
pub struct AccountSummary {
    pub accounts: HashMap<String, i64>,
}

impl Subscribable for AccountSummary {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self {
        Self::default()
    }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<()>,
    {
        Filters::new()
            .event::<FundsDeposited>()
            .event::<FundsWithdrawn>()
    }
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

## Loading Projections

Filter configuration is defined centrally in `Subscribable::filters()`, not at the call site. Pass the instance ID to load:

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

## The `Filters` Builder

`Filters` captures both which events to load and how to apply them:

```rust,ignore
Filters::new()
    .event::<FundsDeposited>()           // Global: all aggregates
    .event_for::<Account, FundsWithdrawn>(&id)  // Scoped: specific aggregate instance
    .events::<AccountEvent>()            // All variants of an event enum
    .events_for::<Account>(&id)          // All variants for a specific aggregate
```

| Method | Scope |
|--------|-------|
| `.event::<E>()` | All events of type `E` across all aggregates |
| `.event_for::<A, E>(&id)` | Events of type `E` from a specific aggregate instance |
| `.events::<Enum>()` | All event kinds in a `ProjectionEvent` enum |
| `.events_for::<A>(&id)` | All event kinds for a specific aggregate instance |

## Multi-Aggregate Projections

Projections can consume events from multiple aggregate types. Register each event type in `filters()`:

```rust,ignore
impl Subscribable for InventoryReport {
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
            .event::<ProductCreated>()   // From Product aggregate
            .event::<SaleRecorded>()     // From Sale aggregate
    }
}
```

Implement `ApplyProjection<E>` for each event type the projection handles.

## Filtering by Aggregate

Use `events_for` to load all events for a specific aggregate instance:

```rust,ignore
fn filters<S>(account_id: &String) -> Filters<S, Self>
where
    S: EventStore<Id = String>,
    S::Metadata: Clone + Into<()>,
{
    Filters::new()
        .events_for::<Account>(account_id)
}
```

## Using Metadata

Projections can access event metadata for cross-cutting concerns. Set the `Metadata` associated type on `Subscribable`:

```rust,ignore
impl Subscribable for AuditLog {
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

- **Singleton** (`InstanceId = ()`): One global projection. Use `init((): &())` and `load_projection::<P>(&())`.
- **Instance** (`InstanceId = String` or custom type): One projection per instance. Use `init(id: &String)` and `load_projection::<P>(&id)`.

Instance projections can capture their identity at construction time and use it to scope their filters.

## Snapshotting Projections

On repositories with snapshot support, use `load_projection_with_snapshot`:

```rust,ignore
let projection = repo
    .load_projection_with_snapshot::<LoyaltySummary>(&instance_id)
    .await?;
```

Snapshotting requires a globally ordered store (`S: GloballyOrderedStore`),
`S::Position: Ord`, and `P: Serialize + DeserializeOwned`.

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

[Stores](stores.md) — Event persistence and serialization
