# Aggregates

An **aggregate** rebuilds state from events and validates commands to produce new events.

## Basic Usage

For most aggregates:

1. `#[derive(Aggregate)]`
2. Implement `Apply<E>` per event
3. Implement `HandleCreate<C>` for create commands
4. Implement `Handle<C>` for existing aggregate commands

```rust,ignore
#[derive(Default, sourcery::Aggregate)]
#[aggregate(id = String, error = AccountError, events(FundsDeposited, FundsWithdrawn))]
pub struct Account {
    balance: i64,
}

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, event: &FundsDeposited) {
        self.balance += event.amount;
    }
}

impl Apply<FundsWithdrawn> for Account {
    fn apply(&mut self, event: &FundsWithdrawn) {
        self.balance -= event.amount;
    }
}
```

## Loading Aggregates

```rust,ignore
let account: Account = repository
    .load(&account_id)
    .await?;
```

This replays events for that aggregate ID. If snapshots are configured, replay starts from the latest snapshot.

## Trait Reference

### The `Aggregate` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/aggregate.rs:aggregate_trait}}
```

`#[derive(Aggregate)]` generates most of this.

### The `Apply<E>` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/aggregate.rs:apply_trait}}
```

`apply` should be:

- Infallible (events are facts)
- Deterministic (same events -> same state)

## Event Replay Model

```d2
shape: sequence_diagram

Store: Event Store
Agg: Account (created from first event)

Store -> Agg: "create(FundsDeposited { amount: 100 })"
Agg: "balance = 100" {shape: text}
Store -> Agg: "apply(FundsWithdrawn { amount: 30 })"
Agg: "balance = 70" {shape: text}
Store -> Agg: "apply(FundsDeposited { amount: 50 })"
Agg: "balance = 120" {shape: text}
```

## Snapshots and `serde`

Snapshots are opt-in via `Repository::with_snapshots()`.

If enabled, aggregate state must implement `Serialize + DeserializeOwned`.

## Next

[Domain Events](domain-event.md) — Defining events as first-class types
