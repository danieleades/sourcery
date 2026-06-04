# storefront

An end-to-end example application for the [Sourcery](../../README.md)
event-sourcing library: a small order-fulfillment system for an online store.

## Running

```bash
# Run the demo (in-memory, no database needed)
cargo run -p storefront

# Run the tests
cargo test -p storefront
```

The demo wires everything together and drives three order scenarios, asserting
the expected outcome at each step, so it doubles as a smoke test and exits with
an error if anything regresses.

## The domain

Three aggregates, each its own consistency boundary, coordinating only through
events:

| Aggregate | Id          | Responsibility                                      |
|-----------|-------------|-----------------------------------------------------|
| `Product` | `ProductId` | Stock on hand, reservations, replenishment          |
| `Order`   | `OrderId`   | cart → placed → confirmed/cancelled → shipped       |
| `Payment` | `PaymentId` | authorize → capture → refund/fail for one order     |

Each aggregate keeps its own strongly typed id newtype, while the store uses a
single raw `String` key. The newtypes implement `StorageKey<String>` and project
to prefixed keys such as `product:<uuid>`, `order:<uuid>`, and `payment:<uuid>`.

Events are namespaced `store.<aggregate>.<event>` and carry foreign domain keys
(`order_id`, `customer_id`) in their payload, which is what the cross-aggregate
read models join on.

## The two headline pieces

- **`coordinator::FulfillmentCoordinator`** — a checkpointed reactor (process
  manager) that reacts to committed events with commands, turning the three
  aggregates into one eventually consistent workflow:
  - `OrderPlaced` → reserve every line; if stock runs out, release what was
    reserved and cancel the order; otherwise authorize a payment.
  - `PaymentCaptured` → confirm the order and commit the reservations.
  - `PaymentFailed` → release the reservations and cancel the order.

  Reactions are idempotent (the coordinator reads current state before acting),
  so at-least-once redelivery and restart-from-checkpoint are safe.

- **Four read models** (`projections`), each a different shape:
  - `InventoryAvailability` — singleton, snapshot-backed: `on_hand` / `reserved`
    / `available` per product.
  - `CustomerAccountView` — one instance per customer; joins `Order` and
    `Payment` events by the `customer_id` in their payloads.
  - `OrderFulfillmentView` — one instance per order; assembled from three
    streams scoped by typed id (`events_for` / `event_for`).
  - `RevenueDashboard` — singleton analytics fed by a live subscription whose
    `updates()` stream the demo prints.

## Layout

```
src/
  lib.rs          # re-exports the modules below
  main.rs         # the runnable demo
  app.rs          # concrete store / repository types
  metadata.rs     # RequestContext (custom event metadata)
  domain/
    ids.rs        # id newtypes + StorageKey impls
    product.rs    # Product aggregate, events, commands
    order.rs      # Order aggregate, events, commands
    payment.rs    # Payment aggregate, events, commands (+ event versioning)
  projections.rs  # the four read models
  coordinator.rs  # FulfillmentCoordinator process manager
tests/
  product_rules.rs / order_rules.rs / payment_rules.rs  # given/when/then rules
  concurrency.rs                                        # optimistic conflict + retry
  fulfillment.rs                                        # end-to-end through the coordinator
```

## What it exercises

`#[derive(Aggregate)]` and `#[derive(Projection)]`; `Apply` / `Create` /
`Handle` / `HandleCreate`; per-aggregate typed ids via `StorageKey`; optimistic
concurrency with `update_with_retry`; an `Unchecked` repository view for
idempotent replenishment; aggregate and projection snapshots; on-demand
projection loading and snapshot-backed projection loading; live subscriptions
(`subscribe`, `updates`, `current`, `wait_for`); a checkpointed reactor
(`react`, `with_checkpoints`, `wait_for`) with compensation; read-your-writes
consistency tokens (`create_tracked` / `update_tracked`); custom event metadata;
additive event versioning via a serde default; and the `TestFramework` /
`RepositoryTestExt` testing utilities.
