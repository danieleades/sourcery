# Event Sourcing Primer

Event sourcing stores state as a sequence of events rather than as a single mutable record. Instead of updating a row in a database, you append an event describing what happened. Current state is derived by replaying events from the beginning.

## Traditional State vs Event Log

```d2
direction: right

Traditional: "Traditional (Mutable State)" {
  t1: "UPDATE balance = 150"
  t2: "Current: 150"
  t3: "UPDATE balance = 50"
  t4: "Current: 50"

  t1 -> t2 -> t3 -> t4
}

EventSourced: "Event Sourced" {
  e1: "FundsDeposited(100)"
  e2: "FundsDeposited(50)"
  e3: "FundsWithdrawn(100)"
  e4: "Replay: 100 + 50 - 100 = 50"

  e1 -> e2 -> e3 -> e4
}
```

With traditional storage, you only know the current balance. With event sourcing, you know *how* you got there.

## The Core Principle

> **Events are the source of truth.** To "undo" something, append a compensating event.

| Property | Description |
|----------|-------------|
| **Immutable** | Events cannot be changed after creation |
| **Past tense** | Named as facts: `OrderPlaced`, `FundsDeposited` |
| **Complete** | Contains all data needed to understand what happened |
| **Ordered** | Sequence matters for correct state reconstruction |

## Reconstructing State

When you need the current state of an entity, the repository replays its events in order into a fresh aggregate instance. This "replay" process runs every time you load an aggregate. For long-lived entities, [snapshots](../advanced/snapshots.md) optimize this by checkpointing state periodically.

## Benefits

- **Audit trail** — Every change is recorded with full context
- **Debugging** — Replay events to reproduce bugs exactly
- **Temporal queries** — Answer "what was the state at time T?"
- **Event-driven architecture** — Events naturally feed other systems

## Trade-offs

- **Complexity** — More concepts to understand than CRUD
- **Storage growth** — Event logs grow indefinitely (though events compress well)
- **Eventual consistency** — Read models may lag behind writes
- **Schema evolution** — Old events must remain readable forever

## Next

[CQRS Overview](cqrs-overview.md) — Separating reads from writes
