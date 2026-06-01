# Subscriptions

While `load_projection` rebuilds a projection from scratch on each call, **subscriptions** maintain an in-memory projection that updates in real time as events are committed.

## Basic Usage

Use `Repository::subscribe` to create a `SubscriptionBuilder`, call `start()`, then read the current state and stream subsequent updates via `updates()`:

```rust,ignore
use tokio_stream::StreamExt;

let subscription = repository
    .subscribe::<Dashboard>(())
    .start()
    .await?;

// Current state plus a stream of every subsequent update, captured atomically
// so no update is missed across the boundary.
let (dashboard, mut updates) = subscription.updates();
println!("{dashboard:?}");
while let Some(dashboard) = updates.next().await {
    println!("{dashboard:?}");
}
```

The instance ID argument matches the projection's `InstanceId` type. For singleton projections (`InstanceId = ()`), pass `()`.
`start()` returns only after catch-up completes.

## How Subscriptions Work

A subscription:

1. **Catch-up phase** — Replays all historical events matching the projection's filters
2. **Live phase** — Transitions to processing events as they are committed
3. **Publish updates** — Republishes the projection after each event to `current()` and any live `updates()` stream

```d2
shape: sequence_diagram

App: Application
Sub: Subscription
Store: EventStore

App -> Sub: "subscribe::<P>(instance_id)"
Sub -> Store: "subscribe(filters, from_checkpoint)"
Store -> Sub: "Historical events" {style.stroke-dash: 3}
Sub -> Sub: "Replay events (catch-up)"
Sub -> App: "start() returns (caught up)" {style.stroke-dash: 3}
Store -> Sub: "Live event" {style.stroke-dash: 3}
Sub -> Sub: "apply_projection()"
Sub -> App: "updates() stream -> projection" {style.stroke-dash: 3}
```

## Consuming Updates

`updates()` returns the current projection state together with a stream of every state that follows, captured atomically so no update is lost or duplicated across the boundary. Events applied before the call (including the whole catch-up phase) are folded into the returned snapshot; the stream carries only what comes after. Use it when you want to *react* to every update (push to WebSocket clients, invalidate caches, etc.):

```rust,ignore
let (initial, mut updates) = subscription.updates();
render(&initial);
while let Some(projection) = updates.next().await {
    broadcast_to_clients(&projection);
}
```

If you only need the *latest* state — the common case for guard reads and read-your-writes — call `current()` instead. It returns the live projection without subscribing to the stream, so a subscription used purely this way never queues updates. It composes with `wait_for` (see [Read-Your-Writes](#read-your-writes-consistency)):

```rust,ignore
let dashboard = subscription.current();
```

## Stopping a Subscription

The `start()` method returns a `Subscription` handle. Call `stop()` for graceful shutdown:

```rust,ignore
subscription.stop().await?;
```

The subscription processes any remaining events, offers a final snapshot, then terminates. Dropping the subscription sends a best-effort stop signal, but use `stop()` when you need deterministic shutdown and error handling.

## Subscription Snapshots

By default, subscriptions don't persist snapshots. Use `subscribe_with_snapshots` to provide a snapshot store for faster restart:

```rust,ignore
let snapshot_store = inmemory::Store::every(100);

let subscription = repository
    .subscribe_with_snapshots::<Dashboard>((), snapshot_store)
    .start()
    .await?;

let (dashboard, mut updates) = subscription.updates();
println!("{dashboard:?}");
while let Some(dashboard) = updates.next().await {
    println!("{dashboard:?}");
}
```

The subscription loads the most recent snapshot on startup and periodically offers new snapshots as events are processed. The snapshot's stored checkpoint becomes the resume point. For PostgreSQL, use `store::postgres::CheckpointStore` to persist the cursor and projection state across restarts.

## Read-Your-Writes Consistency

Subscription-fed read models are **eventually consistent**: after a command commits, the background subscription needs a moment to observe and apply the new event, so a read immediately after a write may not yet reflect it. Consistency tokens bridge that gap into an on-demand **read-your-writes** guarantee.

The one-shot path is the `*_tracked` command methods (`update_tracked`, `upsert_tracked`, `create_tracked`). They perform the write and hand back a token naming *exactly* that commit. Pass it to `read_after`, which awaits the token when present and returns the live read model.

```rust,ignore
// 1. Write and get back an exact token for this commit.
let token = repository.update_tracked::<Account, Deposit>(&id, &deposit, &metadata).await?;

// 2. Await the token, then read the live read model.
let dashboard = subscription.read_after(token).await?;
```

`read_after` is convenience sugar for `wait_for(token).await?` followed by `current()`. `current()` is a non-blocking read of the live projection that does **not** consume the update stream, so the lower-level pair is still useful when you need only a progress barrier, want to await several tokens before one read, or want multiple reads after a single wait.

The `*_tracked` methods return `None` only when the command produced no events (nothing to wait for). Because the token is bound to your commit — not the global head — awaiting it never blocks on unrelated concurrent writes. `wait_for` is event-driven — it wakes when the runtime advances, without polling — and carries no timeout, so wrap it in `tokio::time::timeout` or `select!` to bound the wait. It returns `AwaitError::SubscriptionStopped` if the runtime stops before reaching the token, rather than hanging.

For a non-blocking snapshot of progress, use `processed()`, which returns the latest checkpoint the subscription has applied (or `None`).

### Minting a token without a write: `consistency_token`

When there is no single write to hang a token on, `repository.consistency_token()` mints one from the log's current global head:

- **Read-freshness / monotonic reads** — capture "as current as *now*" without writing, then await it before a read so the read never regresses.
- **Batching writes** — perform many cheap `update`s, then mint *one* token covering all of them instead of collecting and `max()`-ing per-write tokens.
- **Out-of-band writes** — the write happened through a path that didn't return a token (another service, a migration, a raw retry loop).

`consistency_token()` returns `None` only when the store is empty. Since it captures the global head, awaiting it may block on unrelated concurrent writes — prefer `update_tracked` for the plain "wait for the write I just made" case.

### Why filtered-out writes still resolve

A `ConsistencyToken` is **global** — a point in the commit order, independent of which events a given projection consumes. So a token can name a write the subscription filters out entirely. The subscription still resolves it: alongside matching events, the store delivers `Delivery::Frontier` markers that advance a global progress cursor across filtered-out commits. Without frontiers, `wait_for` would stall forever on such a token because the projection's own cursor would never reach it.

### Across process boundaries

`ConsistencyToken` is `Serialize`/`DeserializeOwned`. Return it to a client from the write response, then have the client present it on a subsequent read request; the read side awaits it before querying. This extends the read-your-writes guarantee across a network boundary. To combine several writes, keep the largest token (`tokens.into_iter().max()`).

### You usually don't need this for your own entity

`load_projection` rebuilds from the event stream on each call and is already strongly consistent with prior writes — it never observes the eventual-consistency window. Tokens exist specifically for *subscription-fed* read models. For "is **my entity** updated?", the on-demand `load_projection` path is simpler and immune to global-checkpoint latency.

## The `SubscribableStore` Trait

Not all stores support push notifications. The `SubscribableStore` trait extends `EventStore` with a `subscribe` method that returns a stream of `Delivery` items:

```rust,ignore
{{#include ../../../sourcery-core/src/subscription.rs:subscribable_store_trait}}
```

The cursor is a store-defined `Checkpoint`, not an `EventStore::Position`. `Position` is the per-stream version used for optimistic concurrency; `Checkpoint` orders events for *delivery* and is gap-free for resumption. Delivery is at-least-once and strictly increasing in checkpoint — the subscription loop deduplicates by checkpoint, so handlers must be idempotent.

Each stream item is a `Delivery`:

- `Delivery::Event` — a filter-matching event at its delivery checkpoint.
- `Delivery::Frontier` — no event, just a stable global checkpoint through which the store guarantees no further matching event will arrive. Frontiers let a subscription advance a *global* progress cursor across events it filters out; this is what makes [read-your-writes](#read-your-writes-consistency) resolve for writes that don't touch the projection. Stores that can't cheaply compute a frontier may omit it.

`latest_checkpoint` returns the newest committed checkpoint across *all* events, ignoring filters; it backs [`consistency_token`](#minting-a-token-without-a-write-consistency_token). `checkpoint_for_position` maps a just-committed position to its *exact* checkpoint, which is how the `*_tracked` command methods mint a token bound to a single write rather than the global head.

Both built-in stores implement the trait:

- The in-memory store uses `tokio::sync::broadcast`, with the global position as its checkpoint.
- The PostgreSQL store uses `LISTEN/NOTIFY` for low-latency wake-ups plus a polling fallback, and a transaction-id high-water-mark for its checkpoint, so events are never skipped when positions become visible out of order under concurrent commits.

## Shared State Pattern

A common pattern is to share projection state between the subscription and the command side via `Arc<Mutex<_>>`:

```rust,ignore
let live_state = Arc::new(Mutex::new(Dashboard::default()));
let state_for_consumer = live_state.clone();

let subscription = repository
    .subscribe::<Dashboard>(())
    .start()
    .await?;

let (_, mut updates) = subscription.updates();
while let Some(projection) = updates.next().await {
    *state_for_consumer.lock().unwrap() = projection;
}

// Read the live projection from another task — no event replay needed
let current = live_state.lock().unwrap().clone();
```

## When to Use Subscriptions

| Use Case | Approach |
|----------|----------|
| One-off query | `load_projection` |
| Real-time dashboard | `subscribe` |
| Pre-computed read model | `subscribe` with snapshots |
| Guard condition from live state | `subscribe` + `Arc<Mutex<_>>` |
| Read-your-writes on a live read model | `subscribe` + `update_tracked` / `read_after` |
| Batch reporting | `load_projection` |

## Example

See [`examples/read_your_writes.rs`](https://github.com/danieleades/sourcery/blob/main/examples/read_your_writes.rs) for a focused read-your-writes example, and [`examples/subscription_billing.rs`](https://github.com/danieleades/sourcery/blob/main/examples/subscription_billing.rs) for a complete working subscription scenario demonstrating:

- Live subscription read via `current()` / `read_after`
- Multi-aggregate projection (Subscription + Invoice)
- Guard conditions from live state
- Read-your-writes before querying the live model
- Graceful shutdown

## Next

[Custom Stores](custom-stores.md) — Implementing your own persistence layer
