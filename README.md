# sourcery

[![Continuous integration](https://github.com/danieleades/sourcery/actions/workflows/CI.yml/badge.svg)](https://github.com/danieleades/sourcery/actions/workflows/CI.yml)
[![codecov](https://codecov.io/gh/danieleades/sourcery/graph/badge.svg?token=CteU2EuZf8)](https://codecov.io/gh/danieleades/sourcery)

Building blocks for pragmatic event-sourced systems in Rust.

## Highlights

- **Domain-first API** – events are plain structs that implement `DomainEvent`; IDs and
  metadata live in an envelope rather than in your payloads.
- **Aggregate derive** – `#[derive(Aggregate)]` generates the event enum plus
  serialisation/deserialisation glue so command handlers stay focused on behaviour.
- **Repository orchestration** – `Repository` loads aggregates, executes commands via
  `Handle<C>` / `HandleCreate<C>`, and persists the resulting events in a single
  transaction.
- **Metadata-aware projections** – projections receive aggregate IDs, events, and metadata
  via `ApplyProjection`, enabling cross-aggregate correlation using causation, timestamps,
  or custom metadata.
- **Store agnostic** – ships with an in-memory store for demos/tests; implement the
  `EventStore` trait to plug in your own persistence.

## Quick Look

```rust,ignore
use sourcery::{Apply, DomainEvent, Handle};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundsDeposited { pub amount_cents: i64 }

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "bank.account.deposited";
}

#[derive(Debug, Default, sourcery::Aggregate)]
#[aggregate(id = String, error = String, events(FundsDeposited))]
pub struct Account { balance_cents: i64 }

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, event: &FundsDeposited) {
        self.balance_cents += event.amount_cents;
    }
}

pub struct DepositFunds { pub amount_cents: i64 }

impl Handle<DepositFunds> for Account {
    type HandleError = Self::Error;

    fn handle(&self, cmd: &DepositFunds) -> Result<Vec<Self::Event>, Self::HandleError> {
        Ok(vec![FundsDeposited { amount_cents: cmd.amount_cents }.into()])
    }
}
```

## Key Concepts

| Concept | Description |
|---------|-------------|
| **[Aggregates](https://danieleades.github.io/sourcery/core-traits/aggregate.html)** | Rebuild state from events and validate commands via `Apply<E>`, `Handle<C>`, and `HandleCreate<C>`. |
| **[Projections](https://danieleades.github.io/sourcery/core-traits/projections.html)** | Read models that replay events via `ApplyProjection<E>`, potentially across multiple aggregates. |
| **[Repository](https://danieleades.github.io/sourcery/concepts/architecture.html)** | Orchestrates loading, command execution, and persistence in a single transaction. |
| **[EventStore](https://danieleades.github.io/sourcery/core-traits/stores.html)** | Trait defining the persistence boundary—implement for Postgres, `DynamoDB`, S3, etc. |

## Documentation

- **[Full documentation](https://danieleades.github.io/sourcery/)** — Conceptual guides and tutorials
- **[API docs](https://docs.rs/sourcery/latest/sourcery/)** — Type and trait reference
- **[How sourcery compares](https://danieleades.github.io/sourcery/reference/comparison.html)** — Design trade-offs vs other Rust event-sourcing crates

## Examples

| Example | Description |
|---------|-------------|
| [`quickstart`](examples/quickstart.rs) | Minimal aggregate + projection |
| [`inventory_report`](examples/inventory_report.rs) | Cross-aggregate projection |
| [`subscription_billing`](examples/subscription_billing.rs) | Real-world billing domain |
| [`versioned_events`](examples/versioned_events.rs) | Schema migration via serde |
| [`optimistic_concurrency`](examples/optimistic_concurrency.rs) | Conflict detection and retry |
| [`snapshotting`](examples/snapshotting.rs) | Aggregate snapshots for performance |

```shell
cargo run --example quickstart
```

## Status

Pre-1.0. APIs will evolve as real-world usage grows. Feedback and contributions welcome!

## Licensing

This project is publicly available under the **GNU General Public License v3.0**. It may optionally be distributed under the **MIT licence** by commercial arrangement.

---

*Was this useful? [Buy me a coffee](https://github.com/sponsors/danieleades/sponsorships?sponsor=danieleades&preview=true&frequency=recurring&amount=5)*
