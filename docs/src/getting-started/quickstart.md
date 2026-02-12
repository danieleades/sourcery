# Quick Start

Build a simple bank account aggregate in a compact, end-to-end example. This example demonstrates events, commands, an aggregate, a projection, and the repository.

## The Complete Example

```rust,ignore
{{#include ../../../examples/quickstart.rs:full_example}}
```

## What Just Happened?

1. **Defined types** — `FundsDeposited` event with `DomainEvent`, `Deposit` command (plain struct)
2. **Created an aggregate** — `Account` with the derive macro, `Apply` for state mutations, `Handle` for command validation
3. **Created a projection** — `TotalDeposits` builds a read model from events
4. **Wired the repository** — Connected everything with `inmemory::Store`

## Key Points

- **Events are past tense facts** — `FundsDeposited`, not `DepositFunds`
- **Commands are imperative** — `Deposit`, not `Deposited`
- **The derive macro generates** — The event enum, `From` impls, serialisation
- **Projections are decoupled** — They receive events, not aggregate types
- **IDs are infrastructure** — Passed to the repository, not embedded in events

## Next Steps

- [Aggregates](../core-traits/aggregate.md) — Deep dive into the aggregate trait
- [Projections](../core-traits/projections.md) — Multi-stream projections
- [The Aggregate Derive](../derive-macros/aggregate-derive.md) — Full attribute reference
