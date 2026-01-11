# Design Document: StagedEvent Type Inconsistency

**Status**: Active prototyping phase - breaking changes acceptable, prioritize good design over backwards compatibility.

## Context

There are two distinct `StagedEvent` types in the codebase:

1. **In-memory store** (`sourcery-core/src/store/inmemory.rs:14-20`):
```rust
#[derive(Clone)]
pub struct StagedEvent<M> {
    pub kind: String,
    pub data: serde_json::Value,
    pub metadata: M,
}
```

2. **Postgres store** (`sourcery-postgres/src/lib.rs:43-49`):
```rust
#[derive(Clone)]
pub struct StagedEvent<M> {
    kind: String,
    data: serde_json::Value,
    metadata: M,
}
```

Both are re-exported through the main crate at `src/lib.rs`:
- Line 30: `pub use store::inmemory::StagedEvent;` (when in-memory is active)
- Line 44: `pub use store::postgres::StagedEvent;` (when postgres feature enabled)

## Key Difference

**Field visibility**:
- In-memory: fields are `pub` (public)
- Postgres: fields are private (no visibility modifier)

## Impact Analysis

### User Code Differences

With in-memory store:
```rust
use sourcery::store::inmemory::StagedEvent;

let event = StagedEvent {
    kind: "user.created".to_string(),
    data: json!({"name": "Alice"}),
    metadata: (),
};

// Direct field access allowed
println!("Event kind: {}", event.kind);
```

With postgres store:
```rust
use sourcery::store::postgres::StagedEvent;

// This won't compile - fields are private
let event = StagedEvent {
    kind: "user.created".to_string(),  // ERROR: private field
    data: json!({"name": "Alice"}),
    metadata: (),
};
```

### Current Usage Pattern

Looking at how `StagedEvent` is actually used in the codebase:

**Created by stores** (via `EventStore::stage_event()`):
- Postgres: `sourcery-postgres/src/lib.rs:186-190`
- In-memory: `sourcery-core/src/store/inmemory.rs:122-126`

**Consumed by stores** (via `EventStore::append()` and related methods):
- Postgres: `sourcery-postgres/src/lib.rs:292-297` (destructures in closure)
- In-memory: `sourcery-core/src/store/inmemory.rs:200-215` (destructures in closure)

**Never constructed directly by users** - always created through `stage_event()`.

### Visibility in Trait

The `EventStore` trait defines:

```rust
type StagedEvent;

fn stage_event<E>(&self, event: &E, metadata: Self::Metadata)
    -> Result<Self::StagedEvent, Self::Error>
where
    E: SerializableEvent;
```

The trait doesn't specify field visibility or construction methods - that's left to implementations.

## Why Are They Different?

### Historical Analysis

Based on the code structure:

1. **In-memory store is older**: It's part of `sourcery-core`, the fundamental crate
2. **Postgres store is newer**: It's a separate crate with its own error types and patterns
3. **Different design maturity**: Postgres was likely written with more encapsulation awareness

### Likely Causes

1. **Initial prototype vs production code**: In-memory might have started with public fields for testing convenience
2. **No abstraction requirement**: Since each store defines its own `StagedEvent`, there was no forcing function to make them consistent
3. **Feature-gated re-exports hide the issue**: Users typically only see one version at a time

## Why Two Separate Types?

### The Design Choice

Each store backend defines its own `StagedEvent` type because:

1. **Type parameter differences**:
   - In-memory: `StagedEvent<M>` where `M: Clone + Send + Sync`
   - Postgres: `StagedEvent<M>` where `M: Serialize + DeserializeOwned`

   The postgres version has stricter bounds because it must persist to database.

2. **Associated type in EventStore trait**:
   ```rust
   trait EventStore {
       type StagedEvent;
       // ...
   }
   ```

   Each implementation provides its own concrete type.

3. **Future flexibility**: Different stores might need different representations:
   - A protobuf store might use `Vec<u8>` instead of `serde_json::Value`
   - A memory-mapped store might use fixed-size buffers
   - An encrypted store might add encryption metadata fields

### Why Not Share a Common Type?

**Reasons for separation**:
- Stores are pluggable - don't want to couple them together
- Each store owns its serialization format (postgres uses JSONB, in-memory uses in-process JSON)
- Type parameters might differ (see bounds above)
- Future stores might need different fields

**However**: The current implementations are structurally identical, just with different visibility.

## Evaluation of Current Design

### Is This a Problem?

**Arguments it's a problem**:
1. **Inconsistent encapsulation**: Users get different APIs depending on which store they use
2. **Leaky abstraction**: In-memory exposes implementation details that postgres hides
3. **Potential for misuse**: Public fields in in-memory invite direct construction, bypassing `stage_event()`
4. **Documentation confusion**: Docs will show different field visibility based on feature flags

**Arguments it's acceptable**:
1. **Not user-facing**: `StagedEvent` is typically only touched by store implementations
2. **Feature-gated**: Users rarely switch stores, so inconsistency isn't noticed
3. **No shared trait bounds**: There's no `StagedEvent` trait requiring specific methods
4. **Internal to transaction**: Events staged in a transaction aren't meant to be inspected

### Actual User Impact

Checking example code and tests:

**Examples don't construct StagedEvent directly**:
- `examples/quickstart.rs`: Uses repository API, never touches `StagedEvent`
- `examples/optimistic_concurrency.rs`: Same
- All examples use high-level `Repository` API

**Tests use aggregates, not staged events**:
- `tests/aggregate_tests.rs`: Tests aggregate behavior
- `sourcery-postgres/tests/event_store.rs`: Tests event store but through trait methods

**Verdict**: Current user impact is minimal because the types are primarily internal.

## Proposed Solutions

Since this is a prototyping phase where breaking changes are acceptable, we should focus on the best design rather than migration concerns.

### Option 1: Make In-Memory Fields Private (Quick Fix)

**Approach**: Match postgres by making in-memory fields private.

```rust
// sourcery-core/src/store/inmemory.rs
#[derive(Clone)]
pub struct StagedEvent<M> {
    kind: String,        // Remove `pub`
    data: serde_json::Value,
    metadata: M,
}
```

**Changes needed**:
- Remove `pub` from three fields in `inmemory.rs:17-19`
- Verify no internal code breaks (store should still work via construction in `stage_event()`)

**Pros**:
- Simple one-line change
- Aligns with better encapsulation (postgres approach)
- Prevents users from bypassing `stage_event()` API
- Consistent behavior across stores

**Cons**:
- Doesn't provide accessor methods (fields just become inaccessible)
- Quick fix rather than principled solution

**Recommendation**: Simple immediate fix, but Option 3 is better long-term.

### Option 2: Make Postgres Fields Public (Consistency via Exposure)

**Approach**: Match in-memory by making postgres fields public.

```rust
// sourcery-postgres/src/lib.rs
#[derive(Clone)]
pub struct StagedEvent<M> {
    pub kind: String,
    pub data: serde_json::Value,
    pub metadata: M,
}
```

**Pros**:
- Technically not a breaking change (adds functionality)
- Provides access parity between stores

**Cons**:
- **Wrong direction**: Opens up encapsulation instead of tightening it
- Encourages bypassing `stage_event()` API
- Exposes `serde_json::Value` more prominently
- Users might construct invalid staged events

**Recommendation**: **Reject.** Wrong design direction.

### Option 3: Create Shared `StagedEvent` Type with Proper API (Best Design)

**Approach**: Define a single `StagedEvent` type in `sourcery-core` that both stores use.

```rust
// sourcery-core/src/store.rs
#[derive(Clone)]
pub struct StagedEvent<M> {
    kind: String,
    data: serde_json::Value,
    metadata: M,
}

impl<M> StagedEvent<M> {
    pub(crate) fn new(kind: String, data: serde_json::Value, metadata: M) -> Self {
        Self { kind, data, metadata }
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn data(&self) -> &serde_json::Value {
        &self.data
    }

    pub(crate) fn into_parts(self) -> (String, serde_json::Value, M) {
        (self.kind, self.data, self.metadata)
    }
}
```

Then both stores use this type:

```rust
impl EventStore for inmemory::Store<Id, M> {
    type StagedEvent = crate::store::StagedEvent<M>;
    // ...
}

impl EventStore for postgres::Store<M> {
    type StagedEvent = sourcery_core::store::StagedEvent<M>;
    // ...
}
```

**Changes needed**:
- Define shared type in `sourcery-core/src/store.rs`
- Add accessor methods (fields private, methods `pub(crate)`)
- Update both store implementations to use shared type
- Update re-exports

**Pros**:
- Single source of truth
- Guaranteed consistency
- Clear encapsulation boundary
- Easier to document
- Enables shared helper functions if needed

**Analysis**: The postgres crate is separate from core, so it can't access `pub(crate)` members. We need to make methods `pub`, which is actually fine:

**Refined approach**: Public accessors with documented construction:

```rust
impl<M> StagedEvent<M> {
    /// Create a new staged event.
    ///
    /// This is called by `EventStore::stage_event()` implementations.
    /// Users should not construct this directly - use the store's API instead.
    #[doc(hidden)]
    pub fn new(kind: String, data: serde_json::Value, metadata: M) -> Self {
        Self { kind, data, metadata }
    }

    /// Get the event kind identifier.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Get the serialized event data.
    pub fn data(&self) -> &serde_json::Value {
        &self.data
    }

    /// Get the event metadata.
    pub fn metadata(&self) -> &M {
        &self.metadata
    }

    /// Decompose into constituent parts (for store implementations).
    #[doc(hidden)]
    pub fn into_parts(self) -> (String, serde_json::Value, M) {
        (self.kind, self.data, self.metadata)
    }
}
```

**Pros over Option 1**:
- Single source of truth - no duplication
- Guaranteed API consistency
- Better encapsulation - controlled access through methods
- Documented interface for when/how to use each method
- `#[doc(hidden)]` on constructor signals "internal use only"
- Enables future enhancements (validation, debugging helpers, etc.)
- Simpler for documentation - one type instead of two

**Cons**:
- Couples stores to common representation (but they're already identical!)
- Slightly more code changes required
- Methods are technically public API (but better than public fields)

**Recommendation**: **Choose this.** It's the right long-term design. Since we're in prototyping phase with breaking changes acceptable, there's no reason to half-fix with Option 1.

### Option 4: Add Builder/Accessor Methods (Keep Separate Types)

**Approach**: Keep separate types but give them consistent APIs via methods.

```rust
// Both stores add these methods:
impl<M> StagedEvent<M> {
    pub(crate) fn new(kind: String, data: serde_json::Value, metadata: M) -> Self {
        Self { kind, data, metadata }
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn data(&self) -> &serde_json::Value {
        &self.data
    }

    pub fn metadata(&self) -> &M {
        &self.metadata
    }
}
```

Both types remain separate but provide the same interface.

**Pros**:
- Minimal breaking changes
- Provides consistent API surface
- Keeps stores decoupled
- Each store can add specific methods if needed

**Cons**:
- Code duplication
- Still two separate types in docs
- Relies on discipline to keep them in sync

**Recommendation**: **Worse than Option 3.** Code duplication without benefit. Reject.

## Similar Issue: StoredEvent

The same inconsistency exists for `StoredEvent`:

**In-memory** (`sourcery-core/src/store/inmemory.rs:22-31`):
```rust
#[derive(Clone)]
pub struct StoredEvent<Id, M> {
    aggregate_kind: String,    // Private
    aggregate_id: Id,
    kind: String,
    position: u64,
    data: serde_json::Value,
    metadata: M,
}
```

**Postgres** (`sourcery-postgres/src/lib.rs:51-60`):
```rust
#[derive(Clone)]
pub struct StoredEvent<M> {
    aggregate_kind: String,    // Private
    aggregate_id: uuid::Uuid,
    kind: String,
    position: i64,
    data: serde_json::Value,
    metadata: M,
}
```

**Good news**: `StoredEvent` fields are already private in both! The accessors come from the `StoredEventView` trait:

```rust
impl<Id, M> StoredEventView for StoredEvent<Id, M> {
    fn aggregate_kind(&self) -> &str { &self.aggregate_kind }
    fn aggregate_id(&self) -> &Self::Id { &self.aggregate_id }
    fn kind(&self) -> &str { &self.kind }
    fn position(&self) -> Self::Pos { self.position }
    fn metadata(&self) -> &Self::Metadata { &self.metadata }
}
```

This is the **correct pattern** - the trait provides a consistent interface while allowing different implementations.

## Recommendation

**Implement Option 3: Shared `StagedEvent` Type with Public Accessor Methods**

Since the library is in prototyping phase with breaking changes acceptable, we should implement the best design rather than a quick fix.

### Implementation Plan

1. **Define shared type in `sourcery-core/src/store.rs`**:
   ```rust
   /// Staged event awaiting persistence (type-erased, serialized form).
   ///
   /// Created by `EventStore::stage_event()`. Store implementations use this
   /// to batch events before persisting via `append()`.
   #[derive(Clone)]
   pub struct StagedEvent<M> {
       kind: String,
       data: serde_json::Value,
       metadata: M,
   }
   ```

2. **Add accessor methods**:
   - `new()` - marked `#[doc(hidden)]` for store implementations
   - `kind()`, `data()`, `metadata()` - public accessors
   - `into_parts()` - marked `#[doc(hidden)]` for stores

3. **Remove duplicate types**:
   - Delete `StagedEvent` from `sourcery-core/src/store/inmemory.rs`
   - Delete `StagedEvent` from `sourcery-postgres/src/lib.rs`

4. **Update store implementations**:
   - Change `type StagedEvent = StagedEvent<M>` to use shared type
   - Update construction to use `StagedEvent::new()`
   - Update field access to use accessor methods

5. **Update re-exports**:
   - Main crate can re-export the single shared type
   - Simplifies API - one `StagedEvent` regardless of features

### Why This Over Option 1

**Option 1** (just make fields private) is a half-measure:
- Fields become inaccessible with no replacement API
- Stores would still have duplicate type definitions
- Documentation still shows two separate types
- Doesn't fix the underlying design issue

**Option 3** (shared type):
- Eliminates duplication at the source
- Provides principled API through accessor methods
- Better documentation story (single type)
- Room to add functionality later (validation, debugging, etc.)
- Follows the pattern already established by `StoredEvent`

### Breaking Change Assessment

**Impact**: Low to none
- `StagedEvent` is primarily an internal type
- Users interact through `Repository` API, not directly with staged events
- No examples or tests construct `StagedEvent` manually

**Justification**: Since we're in prototyping phase, this is exactly the time to fix structural issues like duplication and inconsistent encapsulation.

## Comparison with StoredEvent (Good Pattern)

`StoredEvent` does this correctly:

1. **Fields are private** in both implementations ✓
2. **Trait provides interface** via `StoredEventView` ✓
3. **Different generic parameters** (in-memory has `Id` param, postgres uses `uuid::Uuid`) ✓
4. **Consistent user experience** via trait methods ✓

`StagedEvent` should follow this pattern:

1. **Make fields private** (fix in-memory) ✓ Option 1
2. **Consider adding a trait** like `StagedEventView` if access methods needed
3. **Keep separate types** (they may diverge in future)
4. **Provide consistent interface** via trait or documented methods

## Conclusion

The inconsistency exists due to less careful encapsulation in the earlier in-memory store implementation. Since the library is in prototyping phase where breaking changes are acceptable, we should implement **Option 3** (shared type with accessor methods) rather than a quick fix.

This provides:
- Elimination of code duplication
- Consistent API across all store implementations
- Better encapsulation through accessor methods
- Single source of truth for documentation
- Room for future enhancements

The implementation follows the pattern already established by `StoredEvent` + `StoredEventView`, but simplified since `StagedEvent` doesn't need different generic parameters between stores.

### Next Steps

1. Implement shared `StagedEvent` type in `sourcery-core/src/store.rs`
2. Add accessor methods with appropriate `#[doc(hidden)]` markers
3. Remove duplicate definitions from inmemory and postgres stores
4. Update store implementations to use shared type and accessors
5. Update re-exports in main crate
6. Run tests to verify no regressions
