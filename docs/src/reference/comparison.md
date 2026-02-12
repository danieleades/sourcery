# How Sourcery Compares

Sourcery borrows inspiration from projects like
[`eventually`](https://github.com/get-eventually/eventually-rs) and
[`cqrs`](https://github.com/serverlesstechnology/cqrs) but makes different trade-offs.

| Area | Typical ES/CQRS libraries | Sourcery |
|------|---------------------------|----------|
| Event modelling | Events are enum variants tied to one aggregate | Events are standalone structs, reusable across aggregates |
| Projections | Coupled to a specific aggregate enum | Decoupled — subscribe to event types from any aggregate |
| Metadata | Often embedded in event payloads | Carried alongside events in the envelope; domain types stay pure |
| Infrastructure | Bundled command bus, outbox, saga orchestrator | Minimal — repository + store only; you add what you need |
| Event versioning | Explicit upcaster pipelines | Serde attributes on event structs ([details](../advanced/event-versioning.md)) |
| Concurrency control | Runtime configuration | Type-level — `Optimistic` vs `Unchecked` ([details](../advanced/optimistic-concurrency.md)) |

See [Design Decisions](design-decisions.md) for the full rationale behind each choice.
