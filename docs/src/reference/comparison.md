# How Sourcery Compares

Sourcery borrows inspiration from projects like
[`eventually`](https://github.com/get-eventually/eventually-rs) and
[`cqrs`](https://github.com/serverlesstechnology/cqrs) but makes different trade-offs.

## Events as First-Class Structs

Instead of immediately wrapping events in aggregate-specific enums, each `DomainEvent` stands on its own. Multiple aggregates (or even unrelated subsystems) can reuse the same event type. Projections receive aggregate identifiers and metadata alongside events rather than relying on the payload to embed IDs.

## Projections Decoupled from Aggregates

Read models don't depend on a particular aggregate enum or repository type. You declare the events you care about—potentially from several aggregate kinds—and compose them via the `ProjectionBuilder`.

## Metadata Outside Domain Objects

Infrastructure concerns (aggregate kind, ID, causation/correlation IDs, user info) travel alongside the event as separate parameters to projection handlers. The domain event itself remains pure, making it easier to share across bounded contexts.

## Minimal Infrastructure

No built-in command bus, outbox, snapshot scheduler, or event streaming layer. You wire the repository into whichever pipeline you prefer. This keeps the crate lightweight compared to libraries that bundle an entire CQRS stack.

## Versioning at the Serialization Layer

No explicit "upcaster" concept. Instead, migrate historical events transparently via serde attributes on event structs. See the [versioned_events example](https://github.com/danieleades/sourcery/blob/main/examples/versioned_events.rs) and [Event Versioning](../advanced/event-versioning.md).

## Type-Safe Optimistic Concurrency

Repositories use version-checked mutations by default. Conflicts are detected at the type level—the `Optimistic` strategy returns `OptimisticCommandError::Concurrency` when the stream version changes between load and commit.

```rust,ignore
let attempts = repo
    .execute_with_retry::<Account, Deposit>(&id, &cmd, &(), 3)
    .await?;
```

See the [optimistic_concurrency example](https://github.com/danieleades/sourcery/blob/main/examples/optimistic_concurrency.rs) and [Optimistic Concurrency](../advanced/optimistic-concurrency.md) for more.
