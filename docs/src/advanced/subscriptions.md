# Subscriptions

While `load_projection` rebuilds a projection from scratch on each call, **subscriptions** maintain an in-memory projection that updates in real time as events are committed.

## How Subscriptions Work

A subscription:

1. **Catch-up phase** — Replays all historical events matching the projection's filters
2. **Live phase** — Transitions to processing events as they are committed
3. **Callbacks** — Fires `on_update` after each event

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
Sub -> App: "on_update(&projection)" {style.stroke-dash: 3}
```

## Starting a Subscription

Use `Repository::subscribe` to create a `SubscriptionBuilder`, configure callbacks, then call `start()`:

```rust,ignore
let subscription = repository
    .subscribe::<Dashboard>(())
    .on_update(|dashboard| println!("{dashboard:?}"))
    .start()
    .await?;
```

The instance ID argument matches the projection's `InstanceId` type. For singleton projections (`InstanceId = ()`), pass `()`.
`start()` returns only after catch-up completes.

## Callbacks

### `on_update`

Called after every event is applied to the projection. Receives an immutable reference to the current projection state:

```rust,ignore
.on_update(|projection| {
    // Update a shared cache, send to WebSocket clients, etc.
    cache.lock().unwrap().clone_from(projection);
})
```

Callbacks must complete quickly. Long-running work should be dispatched to a separate task via a channel.

## Stopping a Subscription

The `start()` method returns a `SubscriptionHandle`. Call `stop()` for graceful shutdown:

```rust,ignore
subscription.stop().await?;
```

The subscription processes any remaining events, offers a final snapshot, then terminates. Dropping the handle sends a best-effort stop signal, but use `stop()` when you need deterministic shutdown and error handling.

## Subscription Snapshots

By default, subscriptions don't persist snapshots. Use `subscribe_with_snapshots` to provide a snapshot store for faster restart:

```rust,ignore
let snapshot_store = inmemory::Store::every(100);

let subscription = repository
    .subscribe_with_snapshots::<Dashboard>((), snapshot_store)
    .on_update(|d| println!("{d:?}"))
    .start()
    .await?;
```

The subscription loads the most recent snapshot on startup and periodically offers new snapshots as events are processed.

## The `SubscribableStore` Trait

Not all stores support push notifications. The `SubscribableStore` trait extends `EventStore` with a `subscribe` method that returns a stream of events:

```rust,ignore
pub trait SubscribableStore: EventStore + GloballyOrderedStore {
    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from_position: Option<Self::Position>,
    ) -> EventStream<'_, Self>
    where
        Self::Position: Ord;
}
```

The in-memory store implements this via `tokio::sync::broadcast`. A PostgreSQL implementation would use `LISTEN/NOTIFY`.

## Shared State Pattern

A common pattern is to share projection state between the subscription and the command side via `Arc<Mutex<_>>`:

```rust,ignore
let live_state = Arc::new(Mutex::new(Dashboard::default()));
let state_for_callback = live_state.clone();

let subscription = repository
    .subscribe::<Dashboard>(())
    .on_update(move |projection| {
        *state_for_callback.lock().unwrap() = projection.clone();
    })
    .start()
    .await?;

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
| Batch reporting | `load_projection` |

## Example

See [`examples/subscription_billing.rs`](https://github.com/danieleades/sourcery/blob/main/examples/subscription_billing.rs) for a complete working example demonstrating:

- Live subscription with `on_update`
- Multi-aggregate projection (Subscription + Invoice)
- Guard conditions from live state
- Graceful shutdown

## Next

[Custom Stores](custom-stores.md) — Implementing your own persistence layer
