# Subscriptions

While `load_projection` rebuilds a projection from scratch on each call, **subscriptions** maintain an in-memory projection that updates in real time as events are committed.

## Basic Usage

Use `Repository::subscribe` to create a `SubscriptionBuilder`, call `start()`, then consume updates from the returned stream:

```rust,ignore
use tokio_stream::StreamExt as _;

let mut subscription = repository
    .subscribe::<Dashboard>(())
    .start()
    .await?;

while let Some(dashboard) = subscription.next().await {
    println!("{dashboard:?}");
}
```

The instance ID argument matches the projection's `InstanceId` type. For singleton projections (`InstanceId = ()`), pass `()`.
`start()` returns only after catch-up completes.

## Read-Your-Writes

Command methods return `Result<Option<Position>, ...>`:

- `Some(position)` means events were committed, ending at `position`
- `None` means the command was a valid no-op and emitted no events

Use `wait_for(position)` only when you get `Some(position)`:

```rust,ignore
let committed = repository
    .update::<Account, Deposit>(&account_id, &Deposit { amount: 100 }, &metadata)
    .await?;

let dashboard = if let Some(position) = committed {
    subscription.wait_for(position).await?
} else {
    subscription.current().await
};
```

`wait_for` handles events that are irrelevant to the projection:

1. Fast path: return immediately if this subscription already applied a relevant event at or past `position`
2. Confirm the store has globally committed at least `position`
3. Find the latest relevant event at or before `position`
4. If none exists, return current state immediately

```d2
shape: sequence_diagram

App: Application
Repo: Repository
Sub: Subscription
Store: Event Store

App -> Repo: "update/create/upsert(...)"
Repo -> App: "Ok(Some(position))"
App -> Sub: "wait_for(position)"
Sub -> Store: "latest_position()"
Sub -> Store: "subscribe_all(from?) [only if needed]" {style.stroke-dash: 3}
Sub -> App: "Projection state >= requested write"
```

## How Subscriptions Work

A subscription:

1. **Catch-up phase** — Replays all historical events matching the projection's filters
2. **Live phase** — Transitions to processing events as they are committed
3. **Stream updates** — Yields the projection after each event

```d2
shape: sequence_diagram

App: Application
Sub: Subscription
Store: EventStore

App -> Sub: "subscribe::<P>(instance_id)"
Sub -> Store: "load_events(filters)"
Store -> Sub: "Historical events" {style.stroke-dash: 3}
Sub -> Sub: "Replay events (catch-up)"
Sub -> App: "start() returns (caught up)" {style.stroke-dash: 3}
Store -> Sub: "Live event" {style.stroke-dash: 3}
Sub -> Sub: "apply_projection()"
Sub -> App: "stream.next() -> projection" {style.stroke-dash: 3}
```

## Consuming Updates

Each stream item is the projection state after one event is applied:

```rust,ignore
while let Some(projection) = subscription.next().await {
    // Update a shared cache, send to WebSocket clients, etc.
    cache.lock().unwrap().clone_from(&projection);
}
```

## Stopping a Subscription

The `start()` method returns a `Subscription` stream. Call `stop()` for graceful shutdown:

```rust,ignore
subscription.stop().await?;
```

The subscription processes any remaining events, offers a final snapshot, then terminates. Dropping the subscription sends a best-effort stop signal, but use `stop()` when you need deterministic shutdown and error handling.

## Subscription Snapshots

By default, subscriptions don't persist snapshots. Use `subscribe_with_snapshots` to provide a snapshot store for faster restart:

```rust,ignore
let snapshot_store = inmemory::Store::every(100);

let mut subscription = repository
    .subscribe_with_snapshots::<Dashboard>((), snapshot_store)
    .start()
    .await?;

while let Some(dashboard) = subscription.next().await {
    println!("{dashboard:?}");
}
```

The subscription loads the most recent snapshot on startup and periodically offers new snapshots as events are processed.

## The `SubscribableStore` Trait

Not all stores support push notifications. The `SubscribableStore` trait extends `EventStore` with filtered and global subscription streams:

```rust,ignore
pub trait SubscribableStore: EventStore + GloballyOrderedStore {
    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from_position: Option<Self::Position>,
    ) -> EventStream<'_, Self>
    where
        Self::Position: Ord;

    fn subscribe_all(&self, from_position: Option<Self::Position>) -> EventStream<'_, Self>
    where
        Self::Position: Ord;
}
```

`GloballyOrderedStore` provides `latest_position()`, which powers the lazy global confirmation step in `wait_for`.

The in-memory store implements subscriptions with `tokio::sync::broadcast`. The PostgreSQL backend uses `LISTEN/NOTIFY` for live delivery and only subscribes to all-events when a global wait is actually required.

## Shared State Pattern

A common pattern is to share projection state between the subscription and the command side via `Arc<Mutex<_>>`:

```rust,ignore
let live_state = Arc::new(Mutex::new(Dashboard::default()));
let state_for_consumer = live_state.clone();

let mut subscription = repository
    .subscribe::<Dashboard>(())
    .start()
    .await?;

while let Some(projection) = subscription.next().await {
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
| Read-your-writes in request/response | command result position + `wait_for` |
| Guard condition from live state | `subscribe` + `Arc<Mutex<_>>` |
| Batch reporting | `load_projection` |

## Example

See [`examples/subscription_billing.rs`](https://github.com/danieleades/sourcery/blob/main/examples/subscription_billing.rs) for a complete working example demonstrating:

- Live subscription consumed via `StreamExt::next()`
- Multi-aggregate projection (Subscription + Invoice)
- Guard conditions from live state
- Graceful shutdown

## Next

[Custom Stores](custom-stores.md) — Implementing your own persistence layer
