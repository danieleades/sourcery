# API Improvements Design Document

This document details proposed improvements to the Sourcery API, with comprehensive analysis of each change including pros, cons, alternatives, and impact on compile-time guarantees.

---

## Table of Contents

1. [Unified Command Error Type](#1-unified-command-error-type)
2. [Deduplicated execute_command Implementation](#2-deduplicated-execute_command-implementation)
3. [ProjectionEvent Deserialization API](#3-projectionevent-deserialization-api)
4. [Silent Snapshot Failure Handling](#4-silent-snapshot-failure-handling)
5. [Hand-Written Aggregate Support Macro](#5-hand-written-aggregate-support-macro)
6. [Command Execution Ergonomics](#6-command-execution-ergonomics)
7. [Optional Metadata with Defaults](#7-optional-metadata-with-defaults)
8. [Projection Builder Shortcuts](#8-projection-builder-shortcuts)

---

## 1. Unified Command Error Type

### Problem Statement

The current implementation has four nearly-identical error types for command execution:

```rust
// Current: repository.rs lines 47-114

pub enum CommandError<AggregateError, StoreError> {
    Aggregate(AggregateError),
    Projection(ProjectionError<StoreError>),
    Store(StoreError),
}

pub enum SnapshotCommandError<AggregateError, StoreError, SnapshotError> {
    Aggregate(AggregateError),
    Projection(ProjectionError<StoreError>),
    Store(StoreError),
    Snapshot(SnapshotError),
}

pub enum OptimisticCommandError<AggregateError, Position, StoreError> {
    Aggregate(AggregateError),
    Concurrency(ConcurrencyConflict<Position>),
    Projection(ProjectionError<StoreError>),
    Store(StoreError),
}

pub enum OptimisticSnapshotCommandError<AggregateError, Position, StoreError, SnapshotError> {
    Aggregate(AggregateError),
    Concurrency(ConcurrencyConflict<Position>),
    Projection(ProjectionError<StoreError>),
    Store(StoreError),
    Snapshot(SnapshotError),
}
```

Plus six result type aliases that span multiple lines each:

```rust
pub type OptimisticSnapshotCommandResult<A, S, SS> = Result<
    (),
    OptimisticSnapshotCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Position,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;
```

### Impact

1. **Cognitive overhead**: Users must remember which error type corresponds to their repository configuration
2. **Documentation burden**: Four error types to document with overlapping variants
3. **Pattern matching**: Different match arms depending on repository configuration
4. **Type signature verbosity**: Result types become unwieldy

### Recommended Design: Single Parameterized Error Type

```rust
/// Unified error type for all command execution scenarios.
///
/// The `C` and `SS` type parameters control which variants are possible:
/// - `C = Optimistic`: `Concurrency` variant is possible
/// - `C = Unchecked`: `Concurrency` variant is impossible (never type)
/// - `SS = Snapshots<_>`: `Snapshot` variant is possible
/// - `SS = NoSnapshots<_>`: `Snapshot` variant is impossible (never type)
#[derive(Debug, Error)]
pub enum CommandError<A, S, C = Optimistic, SS = NoSnapshots<<S as EventStore>::Position>>
where
    A: Aggregate,
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// The aggregate rejected the command due to business rule violation.
    #[error("aggregate rejected command: {0}")]
    Aggregate(A::Error),

    /// Another writer modified the stream since we loaded the aggregate.
    ///
    /// This variant is only possible when using `Optimistic` concurrency.
    #[error(transparent)]
    Concurrency(ConcurrencyError<S::Position, C>),

    /// Failed to rebuild aggregate state from events.
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<S::Error>),

    /// The event store failed to persist events.
    #[error("failed to persist events: {0}")]
    Store(#[source] S::Error),

    /// Snapshot operation failed.
    ///
    /// This variant is only possible when snapshots are enabled.
    #[error("snapshot operation failed: {0}")]
    Snapshot(SnapshotError<SS>),
}

/// Type-level wrapper that makes `ConcurrencyConflict` impossible for `Unchecked`.
#[derive(Debug, Error)]
pub enum ConcurrencyError<Pos, C: ConcurrencyStrategy> {
    #[error(transparent)]
    Conflict(ConcurrencyConflict<Pos>),

    /// Marker variant that can never be constructed.
    /// Used when C = Unchecked to make this variant impossible.
    #[error("impossible")]
    #[doc(hidden)]
    _Impossible(PhantomData<C>, Infallible),
}

/// Type-level wrapper that makes snapshot errors impossible for `NoSnapshots`.
#[derive(Debug, Error)]
pub enum SnapshotError<SS> {
    #[error(transparent)]
    Store(SS::Error),

    #[doc(hidden)]
    #[error("impossible")]
    _Impossible(Infallible),
}

// Specialized implementations to make impossible variants truly impossible

impl<Pos> ConcurrencyError<Pos, Unchecked> {
    /// This method doesn't exist - you can never construct this type with Unchecked
}

impl<Pos> ConcurrencyError<Pos, Optimistic> {
    pub fn conflict(c: ConcurrencyConflict<Pos>) -> Self {
        Self::Conflict(c)
    }
}

// Clean type alias
pub type CommandResult<A, S, C = Optimistic, SS = NoSnapshots<<S as EventStore>::Position>> =
    Result<(), CommandError<A, S, C, SS>>;
```

### Usage Comparison

**Before:**
```rust
// User must know which error type to use
fn handle_result(
    result: OptimisticSnapshotCommandResult<Account, PostgresStore, InMemorySnapshotStore>
) {
    match result {
        Err(OptimisticSnapshotCommandError::Aggregate(e)) => { /* ... */ }
        Err(OptimisticSnapshotCommandError::Concurrency(c)) => { /* ... */ }
        Err(OptimisticSnapshotCommandError::Projection(p)) => { /* ... */ }
        Err(OptimisticSnapshotCommandError::Store(s)) => { /* ... */ }
        Err(OptimisticSnapshotCommandError::Snapshot(s)) => { /* ... */ }
        Ok(()) => { /* ... */ }
    }
}

// Different function for unchecked repository
fn handle_unchecked_result(
    result: UncheckedCommandResult<Account, PostgresStore>
) {
    match result {
        Err(CommandError::Aggregate(e)) => { /* ... */ }
        // No Concurrency variant exists here
        Err(CommandError::Projection(p)) => { /* ... */ }
        Err(CommandError::Store(s)) => { /* ... */ }
        Ok(()) => { /* ... */ }
    }
}
```

**After:**
```rust
// Single unified type, variants constrained by type parameters
fn handle_result<C: ConcurrencyStrategy, SS>(
    result: CommandResult<Account, PostgresStore, C, SS>
) {
    match result {
        Err(CommandError::Aggregate(e)) => { /* ... */ }
        Err(CommandError::Concurrency(c)) => { /* ... */ } // Compiler knows if possible
        Err(CommandError::Projection(p)) => { /* ... */ }
        Err(CommandError::Store(s)) => { /* ... */ }
        Err(CommandError::Snapshot(s)) => { /* ... */ } // Compiler knows if possible
        Ok(()) => { /* ... */ }
    }
}
```

### Alternative Design A: Non-Exhaustive with Optional Fields

```rust
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum CommandError<A: Aggregate, S: EventStore> {
    #[error("aggregate rejected command: {0}")]
    Aggregate(A::Error),

    #[error("concurrency conflict: {0}")]
    Concurrency(ConcurrencyConflict<S::Position>),

    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<S::Error>),

    #[error("failed to persist events: {0}")]
    Store(#[source] S::Error),

    #[error("snapshot operation failed")]
    Snapshot(Box<dyn std::error::Error + Send + Sync>),
}
```

**Pros:**
- Simpler type signature (no extra type parameters)
- Single error type for all scenarios
- `#[non_exhaustive]` allows future variants

**Cons:**
- **Weakens compile-time guarantees**: User can match on `Concurrency` even with `Unchecked` repository
- Type-erased snapshot error loses specific error type information
- Runtime possibility of "impossible" variants

### Alternative Design B: Trait-Based Error Abstraction

```rust
pub trait CommandErrorVariants {
    type AggregateError;
    type ConcurrencyError;
    type ProjectionError;
    type StoreError;
    type SnapshotError;
}

// Implement for each configuration...
```

**Pros:**
- Maximum flexibility
- Each configuration has exactly the variants it needs

**Cons:**
- Complex trait hierarchy
- Harder to understand
- More boilerplate

### Compile-Time Guarantee Analysis

| Approach | Concurrency Variant | Snapshot Variant | Type Safety |
|----------|--------------------|--------------------|-------------|
| Current (4 types) | ✅ Impossible when not applicable | ✅ Impossible when not applicable | ✅ Full |
| Recommended | ✅ Impossible via `Infallible` | ✅ Impossible via `Infallible` | ✅ Full |
| Alternative A | ❌ Always matchable | ❌ Always matchable | ⚠️ Weakened |
| Alternative B | ✅ Configurable | ✅ Configurable | ✅ Full |

### Recommendation

**Use the recommended design** with type-level impossibility via `Infallible`. This maintains full compile-time guarantees while dramatically simplifying the API surface.

The key insight is using `Infallible` in hidden enum variants makes them unmatchable and unconstructable, so the compiler will:
1. Warn about unreachable patterns when matching
2. Allow exhaustive matching without those variants
3. Optimize away the impossible branches

---

## 2. Deduplicated execute_command Implementation

### Problem Statement

The `execute_command` method is implemented four times with near-identical logic:

```rust
// Current: repository.rs

// Implementation 1: Unchecked, NoSnapshots (lines 516-552)
impl<S> Repository<S, Unchecked, NoSnapshots<S::Position>> {
    pub async fn execute_command<A, Cmd>(...) -> UncheckedCommandResult<A, S> {
        let new_events = {
            let LoadedAggregate { aggregate, .. } = self.load_aggregate::<A>(id).await?;
            Handle::<Cmd>::handle(&aggregate, command)?
        };
        if new_events.is_empty() { return Ok(()); }
        let mut tx = self.store.begin::<Unchecked>(...);
        for event in &new_events { tx.append(...)?; }
        tx.commit().await?;
        Ok(())
    }
}

// Implementation 2: Optimistic, NoSnapshots (lines 567-612)
// Implementation 3: Unchecked, Snapshots (lines 736-807)
// Implementation 4: Optimistic, Snapshots (lines 823-899)
```

Each differs only in:
1. Whether version is passed to `begin()` (Optimistic vs Unchecked)
2. Whether `offer_snapshot()` is called after commit
3. Whether aggregate needs `Serialize + DeserializeOwned` bounds

### Impact

1. **Maintenance burden**: Bug fixes must be applied in 4 places
2. **Code bloat**: ~300 lines of duplicated logic
3. **Inconsistency risk**: Implementations could drift apart

### Recommended Design: Internal Helper with Type-Driven Behavior

```rust
impl<S, C, M> Repository<S, C, M>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// Internal command execution with configuration-driven behavior.
    ///
    /// This is not public API - the public methods provide type-safe wrappers.
    async fn execute_command_internal<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
        version: Option<S::Position>,
        snapshot_handler: impl FnOnce(A, S::Position, u64) -> BoxFuture<'_, Result<(), SnapshotError>>,
    ) -> Result<S::Position, InternalCommandError<A::Error, S::Position, S::Error>>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        // Load aggregate
        let LoadedAggregate {
            aggregate,
            version: loaded_version,
            events_since_snapshot,
        } = self.load_aggregate_internal::<A>(id).await
            .map_err(InternalCommandError::Projection)?;

        // Handle command
        let new_events = Handle::<Cmd>::handle(&aggregate, command)
            .map_err(InternalCommandError::Aggregate)?;

        if new_events.is_empty() {
            return Ok(loaded_version.unwrap_or_default()); // No-op
        }

        // Apply events to get final state (for potential snapshot)
        let mut final_aggregate = aggregate;
        for event in &new_events {
            final_aggregate.apply(event);
        }
        let total_events_since_snapshot = events_since_snapshot + new_events.len() as u64;

        // Build transaction
        let expected_version = if C::CHECK_VERSION { version.or(loaded_version) } else { None };
        let mut tx = self.store.begin::<C>(A::KIND, id.clone(), expected_version);

        for event in &new_events {
            tx.append(event, metadata.clone())
                .map_err(InternalCommandError::Store)?;
        }

        // Commit
        let result = tx.commit().await.map_err(|e| match e {
            AppendError::Conflict(c) => InternalCommandError::Concurrency(c),
            AppendError::Store(s) => InternalCommandError::Store(s),
            AppendError::EmptyAppend => unreachable!("filtered above"),
        })?;

        // Offer snapshot (handler decides whether to actually do it)
        snapshot_handler(final_aggregate, result.last_position, total_events_since_snapshot).await?;

        Ok(result.last_position)
    }
}

// Public API methods become thin wrappers:

impl<S> Repository<S, Optimistic, NoSnapshots<S::Position>>
where
    S: EventStore,
{
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> CommandResult<A, S, Optimistic, NoSnapshots<S::Position>>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        self.execute_command_internal::<A, Cmd>(
            id,
            command,
            metadata,
            None, // Version comes from load
            |_, _, _| Box::pin(async { Ok(()) }), // No-op snapshot handler
        )
        .await
        .map(|_| ())
        .map_err(InternalCommandError::into_command_error)
    }
}

impl<S, SS> Repository<S, Optimistic, Snapshots<SS>>
where
    S: EventStore,
    SS: SnapshotStore<S::Id, Position = S::Position>,
{
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> CommandResult<A, S, Optimistic, Snapshots<SS>>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        let snapshots = &self.snapshots;

        self.execute_command_internal::<A, Cmd>(
            id,
            command,
            metadata,
            None,
            |aggregate, position, events_since| Box::pin(async move {
                snapshots.offer_snapshot(
                    A::KIND,
                    id,
                    events_since,
                    || Ok(Snapshot { position, data: aggregate }),
                ).await.map(|_| ()).map_err(SnapshotError::from)
            }),
        )
        .await
        .map(|_| ())
        .map_err(InternalCommandError::into_command_error)
    }
}
```

### Alternative Design: Macro-Based Generation

```rust
macro_rules! impl_execute_command {
    (
        concurrency: $C:ty,
        snapshots: $has_snapshots:tt,
        extra_bounds: [$($bounds:tt)*]
    ) => {
        impl<S $(, SS)?> Repository<S, $C, $snapshot_type>
        where
            S: EventStore,
            $($bounds)*
        {
            pub async fn execute_command<A, Cmd>(
                &self,
                id: &S::Id,
                command: &Cmd,
                metadata: &S::Metadata,
            ) -> /* return type */
            where
                A: Aggregate<Id = S::Id> + Handle<Cmd>,
                // ... bounds
            {
                // Shared implementation
                impl_execute_command!(@body self, id, command, metadata, $C, $has_snapshots)
            }
        }
    };

    (@body $self:ident, $id:ident, $cmd:ident, $meta:ident, Optimistic, true) => {
        // Full implementation with version checking and snapshots
    };
    // ... other combinations
}

impl_execute_command!(concurrency: Optimistic, snapshots: false, extra_bounds: []);
impl_execute_command!(concurrency: Optimistic, snapshots: true, extra_bounds: [SS: SnapshotStore<S::Id>]);
impl_execute_command!(concurrency: Unchecked, snapshots: false, extra_bounds: []);
impl_execute_command!(concurrency: Unchecked, snapshots: true, extra_bounds: [SS: SnapshotStore<S::Id>]);
```

**Pros:**
- Guarantees identical implementation across variants
- Explicit about what differs between implementations
- No runtime overhead

**Cons:**
- Macro complexity
- Harder to debug
- IDE support may be limited

### Alternative Design: Strategy Pattern

```rust
pub trait ExecutionStrategy<S: EventStore> {
    type SnapshotMode;

    fn expected_version(&self, loaded: Option<S::Position>) -> Option<S::Position>;

    async fn handle_snapshot<A>(
        &self,
        aggregate: A,
        position: S::Position,
        events_since: u64,
    ) -> Result<(), SnapshotError>;
}

struct OptimisticNoSnapshot;
struct OptimisticWithSnapshot<SS>(SS);
struct UncheckedNoSnapshot;
struct UncheckedWithSnapshot<SS>(SS);

impl<S: EventStore> ExecutionStrategy<S> for OptimisticNoSnapshot {
    type SnapshotMode = NoSnapshots<S::Position>;

    fn expected_version(&self, loaded: Option<S::Position>) -> Option<S::Position> {
        loaded // Use loaded version for optimistic checking
    }

    async fn handle_snapshot<A>(&self, _: A, _: S::Position, _: u64) -> Result<(), SnapshotError> {
        Ok(()) // No-op
    }
}
```

**Pros:**
- Clean separation of concerns
- Easily extensible to new strategies
- Testable in isolation

**Cons:**
- Adds runtime dispatch (trait objects) unless carefully designed
- More types to understand
- May complicate type inference

### Compile-Time Guarantee Analysis

| Approach | Version Check | Snapshot Handling | Bounds Enforcement |
|----------|--------------|-------------------|-------------------|
| Current (4 impls) | ✅ Compile-time | ✅ Compile-time | ✅ Per-impl |
| Recommended (helper) | ✅ Via `C::CHECK_VERSION` | ✅ Via closure | ✅ Per-wrapper |
| Macro-based | ✅ Compile-time | ✅ Compile-time | ✅ Per-expansion |
| Strategy pattern | ⚠️ Could be runtime | ⚠️ Could be runtime | ✅ Via trait bounds |

### Recommendation

**Use the recommended helper approach**. It:
- Maintains compile-time guarantees through wrapper methods
- Consolidates logic in one place
- Keeps public API identical to current
- Allows per-configuration bounds (e.g., `Serialize` only when snapshots enabled)

---

## 3. ProjectionEvent Deserialization API

### Problem Statement

The current `ProjectionEvent::from_stored` API couples projection events to `EventStore`:

```rust
// Current: event.rs lines 70-86

pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];

    fn from_stored<S: EventStore>(
        stored: &S::StoredEvent,
        store: &S,
    ) -> Result<Self, EventDecodeError<S::Error>>;
}
```

The `store` parameter is only used for:
```rust
store.decode_event::<ConcreteEvent>(stored)
```

This creates awkward coupling:
1. `ProjectionEvent` must know about `EventStore` trait
2. Generated macro code references `EventStore` methods
3. Custom implementations need store access just for deserialization

### Recommended Design: Decoder Closure

```rust
pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];

    /// Deserialize from stored representation using a provided decoder.
    ///
    /// The decoder handles the store-specific deserialization, allowing
    /// `ProjectionEvent` to remain agnostic of storage details.
    fn from_stored<E>(
        kind: &str,
        decode: impl FnOnce() -> Result<E, EventDecodeError<E>>,
    ) -> Result<Self, EventDecodeError<E>>
    where
        E: std::error::Error;
}
```

**Usage in generated code:**
```rust
// Generated by #[derive(Aggregate)]
impl ProjectionEvent for AccountEvent {
    const EVENT_KINDS: &'static [&'static str] = &[
        FundsDeposited::KIND,
        FundsWithdrawn::KIND,
    ];

    fn from_stored<E>(
        kind: &str,
        decode: impl FnOnce() -> Result<Self, EventDecodeError<E>>,
    ) -> Result<Self, EventDecodeError<E>>
    where
        E: std::error::Error,
    {
        match kind {
            FundsDeposited::KIND => decode().map(Self::Deposited),
            FundsWithdrawn::KIND => decode().map(Self::Withdrawn),
            _ => Err(EventDecodeError::UnknownKind {
                kind: kind.to_string(),
                expected: Self::EVENT_KINDS,
            }),
        }
    }
}
```

**Caller site (in repository/projection):**
```rust
// Before
let event = A::Event::from_stored(stored, store)?;

// After
let event = A::Event::from_stored(stored.kind(), || {
    store.decode_event(stored)
})?;
```

### Alternative Design A: Raw Bytes API

```rust
pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];

    /// Deserialize from raw JSON bytes.
    fn from_json(kind: &str, data: &[u8]) -> Result<Self, EventDecodeError<serde_json::Error>>;
}
```

**Pros:**
- Simplest possible interface
- No generics or closures
- Easy to understand and implement

**Cons:**
- **Hardcodes JSON**: Can't support binary formats (MessagePack, CBOR, etc.)
- Store abstraction leaks (assumes JSON)
- Less flexible for custom serialization

### Alternative Design B: Visitor Pattern

```rust
pub trait EventDecoder {
    type Error: std::error::Error;

    fn decode<E: DomainEvent + DeserializeOwned>(&self) -> Result<E, Self::Error>;
}

pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];

    fn from_stored<D: EventDecoder>(
        kind: &str,
        decoder: &D,
    ) -> Result<Self, EventDecodeError<D::Error>>;
}

// Store provides decoder
impl<M> EventDecoder for StoredEvent<M> {
    type Error = InMemoryError;

    fn decode<E: DomainEvent + DeserializeOwned>(&self) -> Result<E, Self::Error> {
        serde_json::from_value(self.data.clone())
            .map_err(|e| InMemoryError::Deserialization(Box::new(e)))
    }
}
```

**Pros:**
- Clean abstraction
- Each store provides its own decoder
- Type-safe error handling

**Cons:**
- New trait to understand
- More indirection
- Decoder must be created/passed around

### Alternative Design C: Associated Deserializer

```rust
pub trait ProjectionEvent: Sized {
    const EVENT_KINDS: &'static [&'static str];

    /// The deserializer type this event expects.
    type Deserializer: EventDeserializer;

    fn from_stored(
        kind: &str,
        deserializer: &Self::Deserializer,
    ) -> Result<Self, EventDecodeError<<Self::Deserializer as EventDeserializer>::Error>>;
}

pub trait EventDeserializer {
    type Error: std::error::Error;
    type Input;

    fn deserialize<E: DeserializeOwned>(&self, input: &Self::Input) -> Result<E, Self::Error>;
}
```

**Pros:**
- Most flexible
- Events can specify their deserializer requirements
- Supports heterogeneous serialization formats

**Cons:**
- Complex type signatures
- May overcomplicate simple use cases
- Associated type adds cognitive load

### Compile-Time Guarantee Analysis

| Approach | Store Independence | Format Flexibility | Type Safety |
|----------|-------------------|-------------------|-------------|
| Current | ❌ Requires `EventStore` | ✅ Via store | ✅ Full |
| Recommended (closure) | ✅ Only needs closure | ✅ Via closure | ✅ Full |
| Alternative A (JSON) | ✅ Standalone | ❌ JSON only | ✅ Full |
| Alternative B (visitor) | ✅ Via trait | ✅ Via impl | ✅ Full |
| Alternative C (assoc) | ✅ Via assoc type | ✅ Full control | ✅ Full |

### Recommendation

**Use the decoder closure approach**. It:
- Removes `EventStore` dependency from `ProjectionEvent`
- Maintains full flexibility (any serialization format)
- Minimal API change (just wraps decode call in closure)
- No new traits or types to understand

The closure captures the store-specific decode logic without the trait needing to know about stores.

---

## 4. Silent Snapshot Failure Handling

### Problem Statement

Snapshot creation errors are silently swallowed:

```rust
// Current: repository.rs lines 798-803, 889-891

match offer_result.await {
    Ok(SnapshotOffer::Declined | SnapshotOffer::Stored) => {}
    Err(OfferSnapshotError::Create(_e)) => {
        // Snapshot serialization failed - log and continue
        // This is not fatal, we successfully committed events
    }
    Err(OfferSnapshotError::Snapshot(e)) => {
        return Err(OptimisticSnapshotCommandError::Snapshot(e));
    }
}
```

**Issues:**
1. No logging when `Create` fails - silent data loss
2. `Snapshot` errors propagate but `Create` errors don't - inconsistent
3. Users can't detect persistent serialization failures
4. Debugging snapshot issues becomes very difficult

### Recommended Design: Configurable Error Handling with Logging

```rust
/// Policy for handling snapshot creation failures.
#[derive(Clone, Copy, Debug, Default)]
pub enum SnapshotErrorPolicy {
    /// Log the error and continue (default).
    /// The command succeeded, so snapshot failure is non-fatal.
    #[default]
    LogAndContinue,

    /// Propagate the error, failing the command.
    /// Use when snapshot consistency is critical.
    Propagate,

    /// Silently ignore snapshot errors.
    /// Use only in tests or when snapshots are purely optional.
    Ignore,
}

impl<S, SS> Repository<S, Optimistic, Snapshots<SS>>
where
    S: EventStore,
    SS: SnapshotStore<S::Id, Position = S::Position>,
{
    /// Set the error handling policy for snapshot failures.
    pub fn with_snapshot_error_policy(mut self, policy: SnapshotErrorPolicy) -> Self {
        self.snapshot_error_policy = policy;
        self
    }
}

// In execute_command:
match offer_result.await {
    Ok(SnapshotOffer::Declined) => {
        tracing::trace!("snapshot offer declined by policy");
    }
    Ok(SnapshotOffer::Stored) => {
        tracing::debug!(
            aggregate_kind = A::KIND,
            "snapshot stored successfully"
        );
    }
    Err(OfferSnapshotError::Create(e)) => {
        match self.snapshot_error_policy {
            SnapshotErrorPolicy::LogAndContinue => {
                tracing::warn!(
                    error = %e,
                    aggregate_kind = A::KIND,
                    "snapshot creation failed, continuing without snapshot"
                );
            }
            SnapshotErrorPolicy::Propagate => {
                return Err(CommandError::Snapshot(SnapshotError::Create(e)));
            }
            SnapshotErrorPolicy::Ignore => {}
        }
    }
    Err(OfferSnapshotError::Snapshot(e)) => {
        match self.snapshot_error_policy {
            SnapshotErrorPolicy::LogAndContinue => {
                tracing::warn!(
                    error = %e,
                    aggregate_kind = A::KIND,
                    "snapshot persistence failed, continuing without snapshot"
                );
            }
            SnapshotErrorPolicy::Propagate => {
                return Err(CommandError::Snapshot(SnapshotError::Store(e)));
            }
            SnapshotErrorPolicy::Ignore => {}
        }
    }
}
```

### Alternative Design A: Always Log, Never Fail

```rust
// Simplest approach: just add logging
match offer_result.await {
    Ok(SnapshotOffer::Declined | SnapshotOffer::Stored) => {}
    Err(OfferSnapshotError::Create(e)) => {
        tracing::warn!(
            error = %e,
            aggregate_kind = A::KIND,
            "snapshot serialization failed"
        );
    }
    Err(OfferSnapshotError::Snapshot(e)) => {
        tracing::error!(
            error = %e,
            aggregate_kind = A::KIND,
            "snapshot persistence failed"
        );
    }
}
```

**Pros:**
- Minimal change
- No new configuration
- Observability immediately improved

**Cons:**
- Users who want strict snapshot consistency can't get it
- No way to detect failures programmatically

### Alternative Design B: Callback-Based Handling

```rust
pub trait SnapshotErrorHandler: Send + Sync {
    fn on_create_error(&self, error: &dyn std::error::Error, aggregate_kind: &str);
    fn on_store_error(&self, error: &dyn std::error::Error, aggregate_kind: &str);
}

#[derive(Default)]
pub struct LoggingSnapshotErrorHandler;

impl SnapshotErrorHandler for LoggingSnapshotErrorHandler {
    fn on_create_error(&self, error: &dyn std::error::Error, aggregate_kind: &str) {
        tracing::warn!(%error, aggregate_kind, "snapshot creation failed");
    }

    fn on_store_error(&self, error: &dyn std::error::Error, aggregate_kind: &str) {
        tracing::error!(%error, aggregate_kind, "snapshot persistence failed");
    }
}

pub struct MetricsSnapshotErrorHandler {
    counter: prometheus::Counter,
}

impl SnapshotErrorHandler for MetricsSnapshotErrorHandler {
    fn on_create_error(&self, _: &dyn std::error::Error, _: &str) {
        self.counter.inc();
    }
    // ...
}
```

**Pros:**
- Maximum flexibility
- Supports metrics, alerting, custom logic
- Testable error handling

**Cons:**
- More complex API
- Another trait for users to understand
- Overkill for most use cases

### Alternative Design C: Result Type Change

```rust
/// Result of command execution with snapshot status.
pub struct CommandOutcome<Pos> {
    /// Position of last committed event.
    pub position: Pos,
    /// Whether a snapshot was successfully stored.
    pub snapshot_result: SnapshotOutcome,
}

pub enum SnapshotOutcome {
    /// No snapshot was attempted (snapshots disabled or policy declined).
    NotAttempted,
    /// Snapshot was successfully stored.
    Stored,
    /// Snapshot creation failed but command succeeded.
    CreateFailed(Box<dyn std::error::Error + Send + Sync>),
    /// Snapshot persistence failed but command succeeded.
    StoreFailed(Box<dyn std::error::Error + Send + Sync>),
}

// Usage
let outcome = repo.execute_command::<Account, Deposit>(&id, &cmd, &metadata).await?;
if let SnapshotOutcome::CreateFailed(e) = outcome.snapshot_result {
    eprintln!("Warning: snapshot failed: {e}");
}
```

**Pros:**
- No silent failures - caller always knows
- Explicit about what happened
- No configuration needed

**Cons:**
- **Breaking API change**: Return type changes from `Result<(), E>` to `Result<CommandOutcome, E>`
- Every call site must handle outcome
- Verbose for users who don't care about snapshots

### Compile-Time Guarantee Analysis

| Approach | Failure Visibility | User Control | API Simplicity |
|----------|-------------------|--------------|----------------|
| Current | ❌ Silent | ❌ None | ✅ Simple |
| Recommended (policy) | ✅ Configurable | ✅ Full | ⚠️ Moderate |
| Alt A (just log) | ✅ Always logged | ❌ None | ✅ Simple |
| Alt B (callback) | ✅ Custom | ✅ Full | ❌ Complex |
| Alt C (outcome type) | ✅ Explicit | ✅ Full | ⚠️ Verbose |

### Recommendation

**Use Alternative A (always log) as minimum viable change**, then consider the recommended policy approach for a more complete solution.

At minimum, every error path should log at `warn` or `error` level. The full policy-based approach gives users control but adds complexity.

---

## 5. Hand-Written Aggregate Support Macro

### Problem Statement

When `#[derive(Aggregate)]` doesn't fit (custom event enum behavior, conditional variants, etc.), users must manually implement:

1. `Aggregate` trait
2. `EventKind` for the event enum
3. `serde::Serialize` with custom variant-unwrapping logic
4. `ProjectionEvent` for deserialization
5. `From<E>` for each event type

This is substantial boilerplate:

```rust
// Current: ~60 lines of boilerplate for a 2-event aggregate

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountEvent {
    Deposited(FundsDeposited),
    Withdrawn(FundsWithdrawn),
}

impl EventKind for AccountEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::Deposited(_) => FundsDeposited::KIND,
            Self::Withdrawn(_) => FundsWithdrawn::KIND,
        }
    }
}

impl serde::Serialize for AccountEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Deposited(e) => e.serialize(serializer),
            Self::Withdrawn(e) => e.serialize(serializer),
        }
    }
}

impl ProjectionEvent for AccountEvent {
    const EVENT_KINDS: &'static [&'static str] = &[
        FundsDeposited::KIND,
        FundsWithdrawn::KIND,
    ];

    fn from_stored<S: EventStore>(
        stored: &S::StoredEvent,
        store: &S,
    ) -> Result<Self, EventDecodeError<S::Error>> {
        match stored.kind() {
            FundsDeposited::KIND => Ok(Self::Deposited(
                store.decode_event(stored).map_err(EventDecodeError::Store)?
            )),
            FundsWithdrawn::KIND => Ok(Self::Withdrawn(
                store.decode_event(stored).map_err(EventDecodeError::Store)?
            )),
            _ => Err(EventDecodeError::UnknownKind {
                kind: stored.kind().to_string(),
                expected: Self::EVENT_KINDS,
            }),
        }
    }
}

impl From<FundsDeposited> for AccountEvent {
    fn from(e: FundsDeposited) -> Self { Self::Deposited(e) }
}

impl From<FundsWithdrawn> for AccountEvent {
    fn from(e: FundsWithdrawn) -> Self { Self::Withdrawn(e) }
}

impl Aggregate for Account {
    const KIND: &'static str = "account";
    type Event = AccountEvent;
    type Error = String;
    type Id = String;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::Deposited(e) => Apply::apply(self, e),
            AccountEvent::Withdrawn(e) => Apply::apply(self, e),
        }
    }
}
```

### Recommended Design: Declarative Macro for Event Enums

```rust
/// Implements boilerplate traits for a hand-written event enum.
///
/// This generates:
/// - `EventKind` implementation
/// - `serde::Serialize` (unwraps to inner event)
/// - `ProjectionEvent` for deserialization
/// - `From<E>` for each variant
///
/// # Example
///
/// ```rust
/// use sourcery::impl_event_enum;
///
/// impl_event_enum! {
///     /// Events for the Account aggregate.
///     #[derive(Clone, Debug, PartialEq, Eq)]
///     pub enum AccountEvent {
///         Deposited(FundsDeposited),
///         Withdrawn(FundsWithdrawn),
///     }
/// }
/// ```
#[macro_export]
macro_rules! impl_event_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $variant:ident($event:ty)
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $($variant($event)),*
        }

        impl $crate::event::EventKind for $name {
            fn kind(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => <$event as $crate::event::DomainEvent>::KIND),*
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                match self {
                    $(Self::$variant(e) => e.serialize(serializer)),*
                }
            }
        }

        impl $crate::event::ProjectionEvent for $name {
            const EVENT_KINDS: &'static [&'static str] = &[
                $(<$event as $crate::event::DomainEvent>::KIND),*
            ];

            fn from_stored<Store: $crate::store::EventStore>(
                stored: &Store::StoredEvent,
                store: &Store,
            ) -> Result<Self, $crate::event::EventDecodeError<Store::Error>> {
                use $crate::store::StoredEventView;
                match stored.kind() {
                    $(
                        <$event as $crate::event::DomainEvent>::KIND => {
                            Ok(Self::$variant(
                                store.decode_event(stored)
                                    .map_err($crate::event::EventDecodeError::Store)?
                            ))
                        }
                    )*
                    _ => Err($crate::event::EventDecodeError::UnknownKind {
                        kind: stored.kind().to_string(),
                        expected: Self::EVENT_KINDS,
                    }),
                }
            }
        }

        $(
            impl From<$event> for $name {
                fn from(e: $event) -> Self {
                    Self::$variant(e)
                }
            }
        )*
    };
}
```

**Usage:**

```rust
// Before: ~40 lines of boilerplate
// After: 7 lines

impl_event_enum! {
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum AccountEvent {
        Deposited(FundsDeposited),
        Withdrawn(FundsWithdrawn),
    }
}

// Aggregate impl still needed but much shorter
impl Aggregate for Account {
    const KIND: &'static str = "account";
    type Event = AccountEvent;
    type Error = String;
    type Id = String;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::Deposited(e) => Apply::apply(self, e),
            AccountEvent::Withdrawn(e) => Apply::apply(self, e),
        }
    }
}
```

### Extended Design: Full Aggregate Macro

```rust
/// Complete aggregate implementation for hand-written types.
#[macro_export]
macro_rules! impl_aggregate {
    (
        aggregate: $agg:ty,
        kind: $kind:literal,
        id: $id:ty,
        error: $error:ty,
        events: {
            $(#[$enum_meta:meta])*
            $vis:vis enum $event_enum:ident {
                $($variant:ident($event:ty)),* $(,)?
            }
        }
    ) => {
        // Generate event enum with all traits
        $crate::impl_event_enum! {
            $(#[$enum_meta])*
            $vis enum $event_enum {
                $($variant($event)),*
            }
        }

        // Generate Aggregate impl
        impl $crate::aggregate::Aggregate for $agg {
            const KIND: &'static str = $kind;
            type Event = $event_enum;
            type Error = $error;
            type Id = $id;

            fn apply(&mut self, event: &Self::Event) {
                match event {
                    $($event_enum::$variant(e) => $crate::aggregate::Apply::apply(self, e)),*
                }
            }
        }
    };
}

// Usage
impl_aggregate! {
    aggregate: Account,
    kind: "account",
    id: String,
    error: String,
    events: {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum AccountEvent {
            Deposited(FundsDeposited),
            Withdrawn(FundsWithdrawn),
        }
    }
}
```

### Alternative Design: Derive Macro with More Options

Extend `#[derive(Aggregate)]` to support pre-defined event enums:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountEvent {
    Deposited(FundsDeposited),
    Withdrawn(FundsWithdrawn),
    // Custom variant that derive couldn't generate
    Legacy(LegacyEvent),
}

#[derive(Aggregate)]
#[aggregate(
    id = String,
    error = String,
    event_enum = AccountEvent,  // Use existing enum instead of generating
)]
pub struct Account {
    balance: i64,
}
```

**Pros:**
- Works within existing derive macro system
- Familiar syntax
- Could validate enum structure

**Cons:**
- Proc macro can't easily validate hand-written enum
- Error messages may be confusing
- Mixing paradigms (derive + hand-written)

### Compile-Time Guarantee Analysis

All approaches maintain full compile-time guarantees:
- Event kind constants verified at compile time
- Type-safe variant matching
- `From` implementations checked by compiler

### Recommendation

**Provide both macros:**
1. `impl_event_enum!` for users who just need the enum boilerplate
2. `impl_aggregate!` for users who want the full package

The declarative macro approach is:
- Easy to understand (no proc macro magic)
- Inspectable (users can see what it expands to)
- Flexible (users can customize the aggregate impl)

---

## 6. Command Execution Ergonomics

### Problem Statement

Current command execution requires double turbofish:

```rust
repo.execute_command::<Account, Deposit>(&id, &cmd, &metadata).await?;
```

Issues:
1. Verbose - must specify both Aggregate and Command types
2. Order matters but isn't obvious (`Aggregate, Command` not `Command, Aggregate`)
3. IDE autocomplete is limited with turbofish
4. Repetitive when executing multiple commands on same aggregate

### Recommended Design: Builder Pattern

```rust
impl<S, C, M> Repository<S, C, M>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// Start a command execution for a specific aggregate instance.
    pub fn for_aggregate<A>(&self, id: &S::Id) -> AggregateCommander<'_, S, C, M, A>
    where
        A: Aggregate<Id = S::Id>,
    {
        AggregateCommander {
            repo: self,
            id: id.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Builder for executing commands on a specific aggregate instance.
pub struct AggregateCommander<'r, S, C, M, A>
where
    S: EventStore,
    C: ConcurrencyStrategy,
    A: Aggregate<Id = S::Id>,
{
    repo: &'r Repository<S, C, M>,
    id: S::Id,
    _phantom: PhantomData<A>,
}

impl<'r, S, C, M, A> AggregateCommander<'r, S, C, M, A>
where
    S: EventStore,
    C: ConcurrencyStrategy,
    A: Aggregate<Id = S::Id>,
{
    /// Execute a command on this aggregate.
    pub async fn execute<Cmd>(
        &self,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> CommandResult<A, S, C, M>
    where
        A: Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        self.repo.execute_command::<A, Cmd>(&self.id, command, metadata).await
    }

    /// Execute a command with automatic retry on concurrency conflicts.
    pub async fn execute_with_retry<Cmd>(
        &self,
        command: &Cmd,
        metadata: &S::Metadata,
        max_retries: usize,
    ) -> Result<usize, CommandError<A, S, C, M>>
    where
        A: Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
        C: OptimisticConcurrency, // Only available for Optimistic
    {
        self.repo.execute_with_retry::<A, Cmd>(&self.id, command, metadata, max_retries).await
    }

    /// Load the current aggregate state.
    pub async fn load(&self) -> Result<A, ProjectionError<S::Error>>
    where
        A::Event: ProjectionEvent,
    {
        self.repo.load::<A>(&self.id).await
    }
}
```

**Usage:**

```rust
// Before
repo.execute_command::<Account, Deposit>(&id, &deposit, &()).await?;
repo.execute_command::<Account, Withdraw>(&id, &withdraw, &()).await?;
let account: Account = repo.load(&id).await?;

// After
let account_cmd = repo.for_aggregate::<Account>(&id);
account_cmd.execute(&deposit, &()).await?;
account_cmd.execute(&withdraw, &()).await?;
let account = account_cmd.load().await?;

// Or chained for single operations
repo.for_aggregate::<Account>(&id)
    .execute(&deposit, &())
    .await?;
```

### Alternative Design A: Command-First API

```rust
impl<S, C, M> Repository<S, C, M> {
    /// Execute a command, inferring the aggregate from the command type.
    pub async fn execute<Cmd>(
        &self,
        id: &<Cmd::Aggregate as Aggregate>::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> CommandResult<Cmd::Aggregate, S, C, M>
    where
        Cmd: Command, // New trait
        Cmd::Aggregate: Aggregate<Id = S::Id> + Handle<Cmd>,
    {
        // ...
    }
}

/// Marker trait linking commands to their aggregate.
pub trait Command {
    type Aggregate: Aggregate;
}

// User implements:
impl Command for Deposit {
    type Aggregate = Account;
}
```

**Usage:**
```rust
// Before
repo.execute_command::<Account, Deposit>(&id, &cmd, &()).await?;

// After
repo.execute(&id, &cmd, &()).await?;  // Aggregate inferred from command
```

**Pros:**
- Cleanest call site
- Type inference works naturally
- Command explicitly tied to aggregate

**Cons:**
- New trait for users to implement on every command
- Can't share commands between aggregates
- More boilerplate per command

### Alternative Design B: Method on Aggregate Types

```rust
/// Extension trait for aggregate types that enables fluent command execution.
pub trait AggregateExt: Aggregate + Sized {
    fn execute<'r, S, C, M, Cmd>(
        repo: &'r Repository<S, C, M>,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> impl Future<Output = CommandResult<Self, S, C, M>>
    where
        S: EventStore<Id = Self::Id>,
        C: ConcurrencyStrategy,
        Self: Handle<Cmd>,
        Cmd: Sync,
    {
        repo.execute_command::<Self, Cmd>(id, command, metadata)
    }
}

impl<A: Aggregate> AggregateExt for A {}
```

**Usage:**
```rust
// Before
repo.execute_command::<Account, Deposit>(&id, &cmd, &()).await?;

// After
Account::execute(&repo, &id, &cmd, &()).await?;
```

**Pros:**
- Reads as "Account executes command"
- Single turbofish (`Deposit` inferred from command)
- Works with existing types

**Cons:**
- Unusual pattern (static method on domain type)
- Repository passed as argument (not method receiver)
- May confuse users expecting OOP style

### Alternative Design C: Keep Current + Add Type Alias

```rust
// Define command executors per aggregate in user code
type AccountRepo<'a, S, C, M> = AggregateCommander<'a, S, C, M, Account>;

// Usage
let account: AccountRepo<_> = repo.for_aggregate(&id);
account.execute(&deposit, &()).await?;
```

This is essentially the recommended design but with user-defined type aliases.

### Compile-Time Guarantee Analysis

| Approach | Command-Aggregate Match | Inference | Breaking Change |
|----------|------------------------|-----------|-----------------|
| Current | ✅ Via turbofish | ❌ Must specify | N/A |
| Recommended (builder) | ✅ Via bounds | ✅ Command inferred | ❌ Additive |
| Alt A (Command trait) | ✅ Via assoc type | ✅ Full inference | ⚠️ New trait required |
| Alt B (static method) | ✅ Via bounds | ✅ Command inferred | ❌ Additive |

### Recommendation

**Use the recommended builder pattern**. It:
- Is purely additive (doesn't break existing code)
- Provides natural grouping for multiple operations on same aggregate
- Enables IDE autocomplete for commands (after specifying aggregate)
- Follows Rust conventions (builder pattern is idiomatic)

---

## 7. Optional Metadata with Defaults

### Problem Statement

Most examples use `&()` for metadata, which is noisy:

```rust
repo.execute_command::<Account, Deposit>(&id, &cmd, &()).await?;
//                                              ^^ noise
```

Metadata is an infrastructure concern that many simple applications don't need.

### Recommended Design: Default Metadata Parameter

```rust
impl<S, C, M> Repository<S, C, M>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// Execute a command with default metadata.
    ///
    /// Equivalent to `execute_command` with `&S::Metadata::default()`.
    pub async fn execute<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
    ) -> CommandResult<A, S, C, M>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + Serialize,
        Cmd: Sync,
        S::Metadata: Default + Clone,
    {
        self.execute_command::<A, Cmd>(id, command, &S::Metadata::default()).await
    }

    /// Execute a command with explicit metadata.
    pub async fn execute_with_metadata<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> CommandResult<A, S, C, M>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        // Current implementation
    }
}
```

**Usage:**

```rust
// Before
repo.execute_command::<Account, Deposit>(&id, &cmd, &()).await?;

// After (simple case)
repo.execute::<Account, Deposit>(&id, &cmd).await?;

// After (with metadata)
repo.execute_with_metadata::<Account, Deposit>(&id, &cmd, &metadata).await?;
```

### Alternative Design A: Rename Existing Methods

```rust
// Rename current method
pub async fn execute_command_with_metadata<A, Cmd>(..., metadata: &S::Metadata) -> ...

// Add shorter name for common case
pub async fn execute_command<A, Cmd>(...) -> ...
where
    S::Metadata: Default,
```

**Pros:**
- Cleaner common case
- Explicit naming for metadata version

**Cons:**
- **Breaking change**: Existing code using `execute_command` would need updates
- Longer name for less common case

### Alternative Design B: Optional Metadata Parameter via Trait

```rust
pub trait IntoMetadata<M> {
    fn into_metadata(self) -> M;
}

impl<M: Default> IntoMetadata<M> for () {
    fn into_metadata(self) -> M {
        M::default()
    }
}

impl<M> IntoMetadata<M> for M {
    fn into_metadata(self) -> M {
        self
    }
}

// Method accepts anything that converts to metadata
pub async fn execute_command<A, Cmd, Meta>(
    &self,
    id: &S::Id,
    command: &Cmd,
    metadata: Meta,
) -> CommandResult<A, S, C, M>
where
    Meta: IntoMetadata<S::Metadata>,
{
    let metadata = metadata.into_metadata();
    // ...
}
```

**Usage:**
```rust
// With unit (uses default)
repo.execute_command::<Account, Deposit>(&id, &cmd, ()).await?;

// With explicit metadata
repo.execute_command::<Account, Deposit>(&id, &cmd, my_metadata).await?;
```

**Pros:**
- Single method handles both cases
- Type-safe conversion
- No breaking change to signature

**Cons:**
- Subtle behavior (`()` becomes `Default::default()`)
- Extra trait for users to understand
- May cause confusing type inference issues

### Alternative Design C: Metadata Builder

```rust
pub async fn execute_command<A, Cmd>(
    &self,
    id: &S::Id,
    command: &Cmd,
) -> CommandBuilder<'_, S, C, M, A, Cmd>
{
    CommandBuilder {
        repo: self,
        id: id.clone(),
        command,
        metadata: None,
        _phantom: PhantomData,
    }
}

pub struct CommandBuilder<'r, S, C, M, A, Cmd> { ... }

impl<...> CommandBuilder<'r, S, C, M, A, Cmd> {
    pub fn with_metadata(mut self, metadata: S::Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub async fn run(self) -> CommandResult<A, S, C, M>
    where
        S::Metadata: Default,
    {
        let metadata = self.metadata.unwrap_or_default();
        // Execute...
    }
}
```

**Usage:**
```rust
// Without metadata
repo.execute_command::<Account, Deposit>(&id, &cmd).run().await?;

// With metadata
repo.execute_command::<Account, Deposit>(&id, &cmd)
    .with_metadata(my_metadata)
    .run()
    .await?;
```

**Pros:**
- Very flexible
- Extensible (could add more options)
- Clear what's optional

**Cons:**
- Always requires `.run()` at end
- More complex for simple case
- Two awaits in the chain look odd

### Compile-Time Guarantee Analysis

| Approach | Type Safety | Default Bound | Complexity |
|----------|-------------|---------------|------------|
| Current | ✅ Full | ❌ Always required | Low |
| Recommended (new method) | ✅ Full | ✅ Only when used | Low |
| Alt A (rename) | ✅ Full | ✅ Only when used | Low (breaking) |
| Alt B (trait) | ✅ Full | ✅ Via trait | Medium |
| Alt C (builder) | ✅ Full | ✅ Via unwrap_or | High |

### Recommendation

**Use the recommended approach** (new method with shorter name). It:
- Is non-breaking (additive)
- Provides cleaner API for common case
- Makes metadata explicit when needed
- Follows principle of making common things easy

The method name `execute` is shorter than `execute_command` which already suggests "command execution", making it slightly redundant.

---

## 8. Projection Builder Shortcuts

### Problem Statement

Building projections requires chaining multiple `.event::<E>()` calls:

```rust
let report = repo
    .build_projection::<InventoryReport>()
    .event::<ProductRestocked>()
    .event::<InventoryAdjusted>()
    .event::<SaleCompleted>()
    .event::<SaleRefunded>()
    .load()
    .await?;
```

When you want all events from an aggregate, you use `.events::<A::Event>()`:

```rust
.events::<AccountEvent>()  // All account events
```

But this requires knowing the generated event enum name.

### Recommended Design: Aggregate-Based Shortcut

```rust
impl<'a, S, P, SS, Snap> ProjectionBuilder<'a, S, P, SS, Snap>
where
    S: EventStore,
    P: Projection<Id = S::Id>,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId, Position = S::Position>,
{
    /// Subscribe to all events from a specific aggregate type.
    ///
    /// Equivalent to `.events::<A::Event>()` but clearer about intent.
    ///
    /// # Example
    ///
    /// ```ignore
    /// repo.build_projection::<AccountHistory>()
    ///     .from_aggregate::<Account>()
    ///     .load()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn from_aggregate<A>(self) -> Self
    where
        A: Aggregate,
        A::Event: ProjectionEvent,
        P: ApplyProjection<A::Event>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.events::<A::Event>()
    }

    /// Subscribe to all events from a specific aggregate instance.
    ///
    /// Equivalent to `.events_for::<A>(&id)`.
    #[must_use]
    pub fn from_aggregate_instance<A>(self, id: &S::Id) -> Self
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
        P: ApplyProjection<A::Event>,
        S::Metadata: Clone + Into<P::Metadata>,
    {
        self.events_for::<A>(id)
    }
}
```

**Usage:**

```rust
// Before
let report = repo.build_projection::<Report>()
    .events::<AccountEvent>()  // Must know generated name
    .events::<ProductEvent>()
    .load()
    .await?;

// After
let report = repo.build_projection::<Report>()
    .from_aggregate::<Account>()  // Uses aggregate type
    .from_aggregate::<Product>()
    .load()
    .await?;
```

### Extended Design: Multiple Aggregates at Once

```rust
impl<'a, S, P, SS, Snap> ProjectionBuilder<'a, S, P, SS, Snap> {
    /// Subscribe to events from multiple aggregate types.
    ///
    /// This is a convenience for calling `.from_aggregate()` multiple times.
    #[must_use]
    pub fn from_aggregates<AggList>(self) -> Self
    where
        AggList: AggregateList<P, S>,
    {
        AggList::register(self)
    }
}

/// Marker trait for lists of aggregates.
pub trait AggregateList<P, S> {
    fn register<SS, Snap>(builder: ProjectionBuilder<'_, S, P, SS, Snap>)
        -> ProjectionBuilder<'_, S, P, SS, Snap>;
}

// Implement for tuples
impl<P, S, A1, A2> AggregateList<P, S> for (A1, A2)
where
    A1: Aggregate,
    A2: Aggregate,
    // ... bounds
{
    fn register<SS, Snap>(builder: ProjectionBuilder<'_, S, P, SS, Snap>)
        -> ProjectionBuilder<'_, S, P, SS, Snap>
    {
        builder
            .from_aggregate::<A1>()
            .from_aggregate::<A2>()
    }
}
```

**Usage:**
```rust
let report = repo.build_projection::<Report>()
    .from_aggregates::<(Account, Product, Order)>()
    .load()
    .await?;
```

**Pros:**
- Very concise for multi-aggregate projections
- Type-safe aggregate list

**Cons:**
- Tuple syntax is unusual
- Complex trait implementation
- Bounds can get unwieldy

### Alternative Design A: Macro for Event Registration

```rust
/// Register multiple events in one call.
macro_rules! with_events {
    ($builder:expr, $($event:ty),+ $(,)?) => {
        $builder
            $(.event::<$event>())+
    };
}

// Usage
let report = with_events!(
    repo.build_projection::<Report>(),
    ProductRestocked,
    InventoryAdjusted,
    SaleCompleted,
    SaleRefunded,
)
.load()
.await?;
```

**Pros:**
- Compact event list
- No new traits
- Works with any events

**Cons:**
- Macro syntax less discoverable
- Doesn't compose with builder pattern well
- IDE support may be limited

### Alternative Design B: Vec-Based Registration

```rust
impl<'a, S, P, SS, Snap> ProjectionBuilder<'a, S, P, SS, Snap> {
    /// Register event types dynamically.
    ///
    /// Uses type-erased handlers internally.
    pub fn events_dynamic(mut self, registrations: Vec<EventRegistration<P, S>>) -> Self {
        for reg in registrations {
            self = reg.register(self);
        }
        self
    }
}

// Provide registration constructors
pub fn event_registration<E, P, S>() -> EventRegistration<P, S>
where
    E: DomainEvent + DeserializeOwned,
    P: ApplyProjection<E>,
{
    EventRegistration::new::<E>()
}
```

**Pros:**
- Dynamic registration possible
- Could load from configuration

**Cons:**
- Type erasure adds runtime cost
- Less type safety
- Complex implementation

### Compile-Time Guarantee Analysis

All approaches maintain full compile-time guarantees:
- Event-projection compatibility checked at compile time
- Missing `ApplyProjection` implementations caught by compiler
- Type-safe throughout

### Recommendation

**Use the recommended `from_aggregate` shortcut**. It:
- Aligns with user mental model (projections build from aggregates)
- Uses familiar aggregate type instead of generated event enum
- Is purely additive
- Simple implementation (just delegates to existing methods)

The tuple-based multi-aggregate approach is nice-to-have but can be added later if there's demand.

---

## Implementation Priority

Based on impact and effort:

### High Priority (Significant Impact, Moderate Effort)

1. **Unified Command Error Type** - Major ergonomics win, reduces API surface
2. **Deduplicated execute_command** - Maintenance improvement, reduces bugs
3. **Silent Snapshot Failure Handling** - Observability fix, easy to implement

### Medium Priority (Good Ergonomics, Low Effort)

4. **Optional Metadata with Defaults** - Clean up common case
5. **Projection Builder Shortcuts** - Better discoverability
6. **Command Execution Builder** - Improved fluency

### Lower Priority (Nice to Have)

7. **Hand-Written Aggregate Macro** - Helps advanced users
8. **ProjectionEvent Deserialization API** - Cleaner abstraction

---

## Migration Notes

Since breaking changes are acceptable, the recommended migration approach is:

1. **Implement all changes in a single major version bump**
2. **Provide a comprehensive migration guide** with before/after examples
3. **Update all examples and documentation** to use new APIs
4. **Consider a `compat` feature flag** that re-exports old type aliases for gradual migration (optional)

The goal is a cleaner, more ergonomic API that's easier to learn and use correctly.
