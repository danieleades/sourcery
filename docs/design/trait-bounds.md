# Design: Relaxing Trait Bounds (Clean Break)

## Summary
This document proposes concrete, clean-break API changes that remove unnecessary trait bounds. The focus is on making the core API less restrictive for single-threaded runtimes, non-`Copy` positions, and custom ID types, while keeping semantics unchanged.

## Proposed API Changes (Concrete)

### 1) Remove redundant `Sized` bounds

Before:
```rust
pub trait Aggregate: Default + Sized { /* ... */ }
pub trait Projection: Default + Sized { /* ... */ }
```

After:
```rust
pub trait Aggregate: Default { /* ... */ }
pub trait Projection: Default { /* ... */ }
```

### 2) Relax `SnapshotStore` ID bounds and move them to implementations

Before:
```rust
pub trait SnapshotStore<Id>: Send + Sync
where
    Id: Send + Sync + 'static,
{
    type Position: Send + Sync + 'static;
    /* ... */
}
```

After:
```rust
pub trait SnapshotStore<Id>: Send + Sync {
    type Position: Send + Sync;
    type Error: std::error::Error + Send + Sync + 'static;

    fn load<'a>(
        &'a self,
        kind: &'a str,
        id: &'a Id,
    ) -> impl Future<Output = Result<Option<Snapshot<Self::Position>>, Self::Error>> + Send + 'a
    where
        Id: Sync;

    fn offer_snapshot<'a, CE, Create>(
        &'a self,
        kind: &'a str,
        id: &'a Id,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> impl Future<Output = Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>> + Send + 'a
    where
        Id: Sync,
        CE: std::error::Error + Send + Sync + 'static,
        Create: FnOnce() -> Result<Snapshot<Self::Position>, CE> + Send + 'a;
}
```

Implementation bounds move into concrete types:

```rust
impl<Id, Pos> SnapshotStore<Id> for InMemorySnapshotStore<Pos>
where
    Id: Clone + Eq + std::hash::Hash + Send + Sync + 'static,
    Pos: Clone + Ord + Send + Sync + 'static,
{ /* ... */ }
```

### 3) Remove `Projection::InstanceId: Send + Sync + 'static` from the trait

Before:
```rust
pub trait Projection: Default {
    type InstanceId: Send + Sync + 'static;
}
```

After:
```rust
pub trait Projection: Default {
    type InstanceId;
}
```

Snapshot-only APIs add the necessary bounds:

```rust
impl<S, P, SS, SC> ProjectionBuilder<'_, S, P, SS, WithSnapshot, SC>
where
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
    P::InstanceId: Sync,
    SC: SnapshotCodec,
{ /* ... */ }
```

### 4) Replace `EventStore::Position: Copy` with `Clone`

Before:
```rust
type Position: Copy + PartialEq + Debug + Send + Sync + 'static;
```

After:
```rust
type Position: Clone + PartialEq + Debug + Send + Sync + 'static;
```

`StoredEventView::position()` remains by value; implementations clone internally:

```rust
fn position(&self) -> Self::Pos {
    self.position.clone()
}
```

### 5) Relax `ConcurrencyStrategy` bounds

Before:
```rust
pub trait ConcurrencyStrategy: private::Sealed + Default + Send + Sync + 'static {
    const CHECK_VERSION: bool;
}
```

After:
```rust
pub trait ConcurrencyStrategy: private::Sealed + Default {
    const CHECK_VERSION: bool;
}
```

### 6) Drop `Cmd: Sync` from `execute_command`

Before:
```rust
pub async fn execute_command<A, Cmd>(...) -> Result<_, _>
where
    Cmd: Sync,
{ /* ... */ }
```

After:
```rust
pub async fn execute_command<A, Cmd>(...) -> Result<_, _> {
    /* ... */
}
```

This allows `Cmd` types that are not `Sync`, at the cost of the returned future potentially being `!Send` when used on multi-threaded runtimes.

### 7) Optional: Drop `S::Metadata: Clone` by accepting a factory
If `Clone` is undesirable for metadata, make the change explicit:

Before:
```rust
pub async fn execute_command<A, Cmd>(
    &self,
    id: &S::Id,
    command: &Cmd,
    metadata: &S::Metadata,
) -> Result<_, _>
where
    S::Metadata: Clone,
{ /* ... */ }
```

After:
```rust
pub async fn execute_command<A, Cmd, Meta>(
    &self,
    id: &S::Id,
    command: &Cmd,
    metadata: Meta,
) -> Result<_, _>
where
    Meta: Fn() -> S::Metadata,
{ /* ... */ }
```

This removes the `Clone` bound entirely and makes the cost explicit at the call site.

## Resulting Surface
- `Aggregate` and `Projection` are strictly more permissive.
- Snapshot APIs no longer force `InstanceId` or `Id` to be `Send + Sync + 'static` unless snapshots are actually used.
- Positions can be non-`Copy`.
- Repository command execution can accept non-`Sync` commands.
