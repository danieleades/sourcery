# Design: serde_json Exposure Review (Clean Break)

## Summary
This document proposes a clean-break refactor that removes `serde_json` from the public API of `sourcery-core` while keeping JSON as an internal choice for concrete stores and optional codecs. The result is a core API that is JSON-agnostic and allows alternative encodings without additional wrapper layers.

## Goals
- Remove `serde_json` types from `sourcery-core` public signatures.
- Keep event and snapshot encoding pluggable without heavy frameworks.
- Minimize the number of new concepts and type parameters.

## Proposed API (Concrete Changes)

### 1) Event Serialization: Remove `SerializableEvent`
Replace `SerializableEvent` with standard `serde::Serialize` and require `EventKind` for kind lookups. This keeps type erasure in the store, not in the event trait.

New/updated traits and signatures:

```rust
pub trait DomainEvent {
    const KIND: &'static str;
}

pub trait EventKind {
    fn kind(&self) -> &'static str;
}

impl<T: DomainEvent> EventKind for T {
    fn kind(&self) -> &'static str {
        T::KIND
    }
}

pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];
    fn from_stored<S: crate::store::EventStore>(
        stored: &S::StoredEvent,
        store: &S,
    ) -> Result<Self, crate::event::EventDecodeError<S::Error>>;
}
```

Event store and transaction changes:

```rust
pub trait EventStore: Send + Sync {
    type StagedEvent: Send + 'static;
    type StoredEvent: StoredEventView<Id = Self::Id, Pos = Self::Position, Metadata = Self::Metadata>;

    fn stage_event<E>(
        &self,
        event: &E,
        metadata: Self::Metadata,
    ) -> Result<Self::StagedEvent, Self::Error>
    where
        E: EventKind + serde::Serialize;

    fn decode_event<E>(&self, stored: &Self::StoredEvent) -> Result<E, Self::Error>
    where
        E: DomainEvent + serde::de::DeserializeOwned;
}

impl<'a, S: EventStore, C: ConcurrencyStrategy> Transaction<'a, S, C> {
    pub fn append<E>(&mut self, event: &E, metadata: S::Metadata) -> Result<(), S::Error>
    where
        E: EventKind + serde::Serialize,
    { /* ... */ }
}
```

Repository bounds change accordingly:

```rust
pub async fn execute_command<A, Cmd>(...)
where
    A: Aggregate<Id = S::Id> + Handle<Cmd>,
    A::Event: ProjectionEvent + EventKind + serde::Serialize,
```

Macro updates:
- `#[derive(Aggregate)]` must implement `serde::Serialize` for the generated event enum with the same JSON shape as before (untagged or custom serializer).
- The generated event enum implements `EventKind` by matching the active variant.
- `SerializableEvent` is removed entirely.

### 2) Snapshot Serialization: Add `SnapshotCodec`
Move snapshot (de)serialization out of the repository and into a pluggable codec that returns raw bytes.

New trait:

```rust
pub trait SnapshotCodec {
    type Error: std::error::Error + Send + Sync + 'static;

    fn serialize<T: serde::Serialize>(&self, value: &T) -> Result<Vec<u8>, Self::Error>;
    fn deserialize<T: serde::de::DeserializeOwned>(&self, data: &[u8]) -> Result<T, Self::Error>;
}
```

`ProjectionError` becomes codec-aware:

```rust
pub enum ProjectionError<StoreError, CodecError>
where
    StoreError: std::error::Error + 'static,
    CodecError: std::error::Error + 'static,
{
    Store(#[source] StoreError),
    EventDecode(#[source] EventDecodeError<StoreError>),
    SnapshotCodec(#[source] CodecError),
}
```

Repository carries a codec explicitly (no defaults assumed):

```rust
pub struct Repository<S, C = Optimistic, M = NoSnapshots<<S as EventStore>::Position>, SC>
where
    S: EventStore,
    C: ConcurrencyStrategy,
    SC: SnapshotCodec,
{
    store: S,
    snapshots: M,
    snapshot_codec: SC,
    _concurrency: PhantomData<C>,
}

impl<S, SC> Repository<S, Optimistic, NoSnapshots<S::Position>, SC>
where
    S: EventStore,
    SC: SnapshotCodec,
{
    pub fn new(store: S, snapshot_codec: SC) -> Self { /* ... */ }
}
```

Snapshot usage in repository and projections now calls the codec:

```rust
let restored: A = self.snapshot_codec
    .deserialize(&snapshot.data)
    .map_err(ProjectionError::SnapshotCodec)?;

let bytes = self.snapshot_codec
    .serialize(&aggregate)
    .map_err(OfferSnapshotError::Create)?;
```

`ProjectionBuilder` carries a codec reference, and `ProjectionBuilder<WithSnapshot>` requires `SC: SnapshotCodec`.

### 3) Remove `serde_json` from `sourcery-core` Public Types
Keep JSON inside concrete stores, but avoid exposing JSON types in public structs:

`store::inmemory`:

```rust
pub struct StagedEvent<M> {
    pub kind: String,
    pub data: Vec<u8>,
    pub metadata: M,
}

pub struct StoredEvent<Id, M> {
    aggregate_kind: String,
    aggregate_id: Id,
    kind: String,
    position: u64,
    data: Vec<u8>,
    metadata: M,
}
```

Internal encoding remains JSON-based, but it is not part of the public API:

```rust
let data = serde_json::to_vec(event).map_err(InMemoryError::Serialization)?;
let event: E = serde_json::from_slice(&stored.data).map_err(InMemoryError::Deserialization)?;
```

`snapshot::InMemorySnapshotStore` no longer serializes IDs:

```rust
struct SnapshotKey<Id> {
    kind: String,
    id: Id,
}

pub struct InMemorySnapshotStore<Pos> {
    snapshots: Arc<RwLock<HashMap<SnapshotKey<Id>, Snapshot<Pos>>>>,
    policy: SnapshotPolicy,
}
```

This removes `serde_json::Error` from its public error type; it can be `Infallible` for persistence errors.

## Resulting Surface
- `sourcery-core` no longer exposes `serde_json` in public APIs.
- JSON remains a private implementation detail in concrete stores and optional codecs.
- Alternative encodings can be introduced by implementing `SnapshotCodec` (and by choosing store-specific encodings in `EventStore` implementations).

## Open Questions
- Should `sourcery-core` keep a JSON codec in-tree behind a feature flag, or should JSON codecs live in separate crates?
- Is it acceptable for the in-memory store to stay JSON-based internally, as long as its public types are JSON-free?
