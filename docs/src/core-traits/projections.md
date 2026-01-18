# Projections

Projections are read models built by replaying events. They're optimized for queries rather than consistency. A single event stream can feed many projections, each structuring data differently.

## The `Projection` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:projection_trait}}
```

Projections must be `Default` because they start empty and build up through event replay.

## The `ApplyProjection<E>` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/projection.rs:apply_projection_trait}}
```

Projections receive the aggregate ID and metadata alongside each event, enabling cross-aggregate views and audit trails.

## Basic Example

```rust,ignore
#[derive(Debug, Default, sourcery::Projection)]
#[projection(id = String)]
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

## Building Projections

Use `ProjectionBuilder` to specify which events to load:

```rust,ignore
let summary = repository
    .build_projection::<AccountSummary>()
    .event::<FundsDeposited>()     // All deposits across all accounts
    .event::<FundsWithdrawn>()      // All withdrawals across all accounts
    .load()
    .await?;
```

## Multi-Aggregate Projections

Projections can consume events from multiple aggregate types. Register each event type with `.event::<E>()`:

```rust,ignore
let report = repository
    .build_projection::<InventoryReport>()
    .event::<ProductCreated>()   // From Product aggregate
    .event::<SaleRecorded>()     // From Sale aggregate
    .load()
    .await?;
```

Implement `ApplyProjection<E>` for each event type the projection handles.

## Filtering by Aggregate

Use `.events_for()` to load all events for a specific aggregate instance:

```rust,ignore
let account_history = repository
    .build_projection::<TransactionHistory>()
    .events_for::<Account>(&account_id)
    .load()
    .await?;
```

## Using Metadata

Projections can access event metadata for cross-cutting concerns:

```rust,ignore
#[derive(Debug)]
pub struct EventMetadata {
    pub timestamp: DateTime<Utc>,
    pub user_id: String,
}

#[derive(Debug)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub user: String,
    pub action: String,
}

#[derive(Debug, Default, sourcery::Projection)]
#[projection(id = String, metadata = EventMetadata)]
pub struct AuditLog {
    pub entries: Vec<AuditEntry>,
}

impl ApplyProjection<FundsDeposited> for AuditLog {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, meta: &Self::Metadata) {
        self.entries.push(AuditEntry {
            timestamp: meta.timestamp.clone(),
            user: meta.user_id.clone(),
            action: format!("Deposited {} to {}", event.amount, id),
        });
    }
}
```

## Snapshotting Projections

Projection snapshots reuse the repository snapshot store. Enable snapshotting
by calling `.with_snapshot()` on the builder:

```rust,ignore
let projection = repo
    .build_projection::<AuditLog>()
    .event::<FundsDeposited>()
    .with_snapshot()
    .load()
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

[Stores & Codecs](stores.md) — Event persistence and serialization
