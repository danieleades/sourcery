# Design Document: serde_json Exposure in Public API

## Context

The `SerializableEvent` trait currently exposes `serde_json::Value` and `serde_json::Error` directly in the public API:

```rust
pub trait SerializableEvent: EventKind {
    fn serialize_event(&self) -> Result<serde_json::Value, serde_json::Error>;
}
```

This trait is:
- Used by the `#[derive(Aggregate)]` macro to generate implementations (line 213-219 in `sourcery-macros/src/lib.rs`)
- Called by both event store implementations (`stage_event()` method)
- Part of the core public API, re-exported from the main crate

## Current Impact

### Public Dependency Lock-in

Making `serde_json` types part of the public API creates several constraints:

1. **Breaking change to remove**: Changing the return type is a semver-breaking change
2. **Version constraints**: Library must coordinate `serde_json` version with downstream consumers
3. **Type leakage**: Users writing custom event stores must use `serde_json::Value` in their code

### Where It's Used

**In the macro** (`sourcery-macros/src/lib.rs:213-219`):
```rust
impl ::sourcery::event::SerializableEvent for #event_enum_name {
    fn serialize_event(&self) -> Result<::serde_json::Value, ::serde_json::Error> {
        match self {
            #(Self::#variant_names(inner) => ::serde_json::to_value(inner)),*
        }
    }
}
```

**In postgres store** (`sourcery-postgres/src/lib.rs:181-191`):
```rust
fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
where
    E: sourcery_core::event::SerializableEvent,
{
    let data = event.serialize_event().map_err(Error::Serialization)?;
    Ok(StagedEvent {
        kind: event.kind().to_string(),
        data,
        metadata,
    })
}
```

**In inmemory store** (`sourcery-core/src/store/inmemory.rs:117-127`):
```rust
fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
where
    E: crate::event::SerializableEvent,
{
    let data = event.serialize_event().map_err(InMemoryError::Serialization)?;
    Ok(StagedEvent {
        kind: event.kind().to_string(),
        data,
        metadata,
    })
}
```

## Architectural Analysis

### Why This Design Was Chosen

The current design follows a pragmatic approach:

1. **Simplicity**: JSON is universally understood and supported
2. **EventStoreDB compatibility**: The library targets EventStoreDB, which uses JSON
3. **Type erasure point**: `SerializableEvent` is where concrete event types become serialized data
4. **Macro generation**: Makes codegen straightforward - just call `serde_json::to_value()`

### Design Constraint: Stores Own Serialization

According to the documentation (CLAUDE.md):

> "Events are never serialized as tagged enums. The generated event enum has a helper method `serialize_event()` (via `SerializableEvent` trait) that serializes only the inner event struct. Store implementations use:
> - `event.kind()` - Returns the event type identifier string
> - `event.serialize_event()` - Returns serialized inner event data"

This suggests **stores control the serialization format**, but currently `SerializableEvent` forces JSON.

## Proposed Solutions

### Option 1: Accept JSON as Canonical Format (Minimal Change)

**Approach**: Document and commit to JSON as the canonical serialization format for Sourcery.

**Changes needed**:
- Add module-level documentation to `event.rs` explaining JSON is the standard
- Add trait-level documentation to `SerializableEvent` explaining the design decision
- Document in the library guide that custom stores should use JSON

**Pros**:
- No breaking changes
- Simple, clear design
- Aligns with EventStoreDB usage
- Realistic for the library's target use case

**Cons**:
- Locks library into `serde_json` forever
- Prevents alternative serialization formats (protobuf, msgpack, etc.)
- `serde_json` becomes a permanent public dependency

**Recommendation**: **Choose this if** the library is specifically targeting EventStoreDB and JSON event storage, and alternative formats aren't a goal.

### Option 2: Generic Serialization Format (Type Parameter)

**Approach**: Make `SerializableEvent` generic over the serialized representation.

```rust
pub trait SerializableEvent<S = serde_json::Value>: EventKind {
    type Error;
    fn serialize_event(&self) -> Result<S, Self::Error>;
}
```

**Changes needed**:
- Add type parameter to trait with default
- Update macro to generate implementations with the default
- Update event stores to specify their serialization type
- Add associated type for error (can't hardcode `serde_json::Error`)

**Pros**:
- Removes `serde_json` from public API surface
- Allows custom stores to use different formats
- Default parameter maintains backward compatibility (potentially)

**Cons**:
- **Complexity explosion**: Type parameter propagates through entire codebase
- Every trait/type using `SerializableEvent` needs the generic parameter
- Breaking change anyway (associated type error)
- Unlikely to be used in practice - most stores will just use JSON
- Violates YAGNI principle

**Recommendation**: **Avoid this approach** - the complexity doesn't justify the flexibility gain.

### Option 3: Opaque Serialized Type (Wrapper)

**Approach**: Hide `serde_json::Value` behind an opaque `SerializedEvent` type.

```rust
pub struct SerializedEvent(serde_json::Value);

impl SerializedEvent {
    // Internal constructor
    pub(crate) fn from_json(value: serde_json::Value) -> Self {
        Self(value)
    }

    // For store implementations to access the value
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    pub fn into_json(self) -> serde_json::Value {
        self.0
    }
}

pub trait SerializableEvent: EventKind {
    fn serialize_event(&self) -> Result<SerializedEvent, SerializationError>;
}

#[non_exhaustive]
pub enum SerializationError {
    Json(serde_json::Error),
}
```

**Changes needed**:
- Create `SerializedEvent` wrapper type
- Create `SerializationError` enum
- Update macro to wrap `serde_json::to_value()` result
- Update stores to unwrap via `as_json()` or `into_json()`

**Pros**:
- Removes direct `serde_json` dependency from trait signature
- Still simple for macro generation
- Leaves door open for future format additions (add more enum variants)
- Breaking change now, but future changes are non-breaking

**Cons**:
- Somewhat artificial wrapper (just hiding JSON)
- Still requires `serde_json` as public dependency (for `as_json()` return type)
- Doesn't fully solve the dependency exposure issue
- More indirection

**Recommendation**: **Only if** you want the appearance of format abstraction while still using JSON internally. Provides minimal benefit over Option 1.

### Option 4: Store-Defined Serialization (No Trait Method)

**Approach**: Remove `serialize_event()` from `SerializableEvent`, let stores handle serialization entirely.

```rust
pub trait DomainEvent {
    const KIND: &'static str;
}

// SerializableEvent removed entirely
// EventKind keeps its blanket impl

// Stores handle serialization directly:
impl<M> EventStore for Store<M> {
    fn stage_event<E>(&self, event: &E, metadata: Self::Metadata)
        -> Result<Self::StagedEvent, Self::Error>
    where
        E: DomainEvent + serde::Serialize,  // Store decides serialization bound
    {
        let data = serde_json::to_value(event).map_err(Error::Serialization)?;
        Ok(StagedEvent {
            kind: E::KIND.to_string(),
            data,
            metadata,
        })
    }
}
```

**Changes needed**:
- Remove `SerializableEvent` trait entirely
- Remove macro-generated `SerializableEvent` implementation
- Update `EventStore::stage_event` bounds from `E: SerializableEvent` to `E: DomainEvent + serde::Serialize`
- Update each store implementation to handle serialization

**Pros**:
- Completely removes serialization from core domain model
- Each store truly owns its serialization format
- No `serde_json` in public API at all
- More flexible - stores can use any format

**Cons**:
- **Breaking change**: Removes a public trait
- Duplicates serialization logic across store implementations
- Macro no longer generates serialization code - each store must handle event enum matching
- Store implementations become more complex
- Loses type-erased serialization helper that macro provided

**Recommendation**: **Consider this if** you want true format flexibility and don't mind store implementations handling the enum matching. This is the most "correct" design architecturally but requires significant refactoring.

## Detailed Analysis: Option 4 Impact

Let's trace through what Option 4 would require:

### Macro Changes

The macro currently generates this for the event enum:

```rust
impl ::sourcery::event::SerializableEvent for AccountEvent {
    fn serialize_event(&self) -> Result<::serde_json::Value, ::serde_json::Error> {
        match self {
            Self::Deposited(inner) => ::serde_json::to_value(inner),
            Self::Withdrawn(inner) => ::serde_json::to_value(inner),
        }
    }
}
```

**Without this**, stores would need to implement the matching themselves, or the event enum would need to implement `serde::Serialize` with custom logic. The problem: serde's default enum serialization creates tagged enums:

```json
{"Deposited": {"amount": 100}}  // Default serde
{"amount": 100}                  // What we want
```

**Solution**: The event enum could implement `Serialize` with custom serialization logic:

```rust
impl ::serde::Serialize for AccountEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ::serde::Serializer,
    {
        match self {
            Self::Deposited(inner) => inner.serialize(serializer),
            Self::Withdrawn(inner) => inner.serialize(serializer),
        }
    }
}
```

This would make `serde_json::to_value(&event)` produce the unwrapped JSON we want!

### Store Changes

Stores would change from:

```rust
fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
where
    E: SerializableEvent,
{
    let data = event.serialize_event().map_err(Error::Serialization)?;
    // ...
}
```

To:

```rust
fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
where
    E: DomainEvent + serde::Serialize,
{
    let data = serde_json::to_value(event).map_err(Error::Serialization)?;
    // ...
}
```

This is actually simpler! The custom `Serialize` impl on the event enum handles the unwrapping.

### Trade-offs

**What we gain**:
- No `SerializableEvent` trait in public API
- No `serde_json` in core event types
- Stores have full control over serialization
- Custom event enums just need `impl Serialize` (which macro can generate)

**What we lose**:
- The `SerializableEvent` abstraction (but it wasn't buying us much)
- A dedicated "serialization error" type (but we can use store's error type)

## Recommendation

### Short term (next minor version): Option 1
Accept JSON as the canonical format and document it clearly. This is pragmatic, aligns with the library's EventStoreDB focus, and avoids complexity.

**Action items**:
1. Add module documentation to `event.rs` explaining JSON serialization design
2. Document in `SerializableEvent` trait that it uses JSON and why
3. Add a design decision doc explaining this choice
4. Note in contributing guide that `serde_json` is a stable public dependency

### Long term (next major version): Option 4
If true format flexibility becomes important, refactor to remove `SerializableEvent` and have the macro generate custom `Serialize` implementations instead. This properly separates domain events from serialization while maintaining the "unwrapped" serialization behavior.

**Why this progression**:
- Option 1 gets the library to a documented, stable state quickly
- Users understand what they're getting (JSON-based event sourcing)
- Option 4 can be implemented in a future major version if needed
- Maintains flexibility to evolve the design based on real-world usage

## Migration Path (Option 1 → Option 4)

If pursuing Option 4 in a future version:

1. **v1.x**: Current state + documentation (Option 1)
2. **v1.y**: Add custom `Serialize` implementations via macro (keep both methods working)
3. **v1.z**: Deprecate `SerializableEvent` trait
4. **v2.0**: Remove `SerializableEvent`, stores use `E: serde::Serialize` directly

This provides a gradual migration path for users.
