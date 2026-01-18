# The Aggregate Derive

The `#[derive(Aggregate)]` macro eliminates boilerplate by generating the event enum and trait implementations.

## Basic Usage

```rust,ignore
use sourcery::Aggregate;

#[derive(Debug, Default, Aggregate)]
#[aggregate(id = String, error = String, events(FundsDeposited, FundsWithdrawn))]
pub struct Account {
    balance: i64,
}
```

## Attribute Reference

| Attribute | Required | Description |
|-----------|----------|-------------|
| `id = Type` | Yes | Aggregate identifier type |
| `error = Type` | Yes | Error type for command handling |
| `events(E1, E2, ...)` | Yes | Event types this aggregate produces |
| `kind = "name"` | No | Aggregate type identifier (default: lowercase struct name) |
| `event_enum = "Name"` | No | Generated enum name (default: `{Struct}Event`) |
| `derives(Trait1, Trait2)` | No | Additional derives for the generated enum (default: `Clone` only) |

## What Gets Generated

For this input:

```rust,ignore
#[derive(Aggregate)]
#[aggregate(id = String, error = AccountError, events(FundsDeposited, FundsWithdrawn))]
pub struct Account { balance: i64 }
```

The macro generates:

```d2
Input: |md
  #[derive(Aggregate)]
  struct Account
|

Input -> "enum AccountEvent"
Input -> "impl Aggregate for Account"
Input -> "impl From<FundsDeposited>"
Input -> "impl From<FundsWithdrawn>"
Input -> "impl EventKind"
Input -> "impl serde::Serialize"
Input -> "impl ProjectionEvent"
```

### 1. Event Enum

```rust,ignore
#[derive(Clone)]
pub enum AccountEvent {
    FundsDeposited(FundsDeposited),
    FundsWithdrawn(FundsWithdrawn),
}
```

### 2. From Implementations

```rust,ignore
impl From<FundsDeposited> for AccountEvent {
    fn from(event: FundsDeposited) -> Self {
        AccountEvent::FundsDeposited(event)
    }
}

impl From<FundsWithdrawn> for AccountEvent {
    fn from(event: FundsWithdrawn) -> Self {
        AccountEvent::FundsWithdrawn(event)
    }
}
```

This enables the `.into()` call in command handlers:

```rust,ignore
Ok(vec![FundsDeposited { amount: 100 }.into()])
```

### 3. Aggregate Implementation

```rust,ignore
impl Aggregate for Account {
    const KIND: &'static str = "account";
    type Event = AccountEvent;
    type Error = AccountError;
    type Id = String;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::FundsDeposited(e) => Apply::apply(self, e),
            AccountEvent::FundsWithdrawn(e) => Apply::apply(self, e),
        }
    }
}
```

### 4. EventKind Implementation

Provides runtime access to the event type identifier:

```rust,ignore
impl EventKind for AccountEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::FundsDeposited(_) => FundsDeposited::KIND,
            Self::FundsWithdrawn(_) => FundsWithdrawn::KIND,
        }
    }
}
```

### 5. Serialize Implementation

Serializes events without enum wrapper (flat serialization):

```rust,ignore
impl serde::Serialize for AccountEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::FundsDeposited(inner) => inner.serialize(serializer),
            Self::FundsWithdrawn(inner) => inner.serialize(serializer),
        }
    }
}
```

This ensures events are stored as their inner type, not as tagged enums.

### 6. ProjectionEvent Implementation

Deserializes stored events by kind:

```rust,ignore
impl ProjectionEvent for AccountEvent {
    const EVENT_KINDS: &'static [&'static str] = &[
        FundsDeposited::KIND,
        FundsWithdrawn::KIND,
    ];

    fn from_stored<S: EventStore>(
        stored: &S::StoredEvent,
        store: &S,
    ) -> Result<Self, EventDecodeError<S::Error>> {
        use sourcery::store::StoredEventView;
        match stored.kind() {
            FundsDeposited::KIND => Ok(Self::FundsDeposited(
                store.decode_event(stored)?
            )),
            FundsWithdrawn::KIND => Ok(Self::FundsWithdrawn(
                store.decode_event(stored)?
            )),
            _ => Err(EventDecodeError::UnknownKind { ... }),
        }
    }
}
```

## Customizing the Kind

By default, `KIND` is the lowercase struct name. Override it:

```rust,ignore
#[derive(Aggregate)]
#[aggregate(
    kind = "bank-account",
    id = String,
    error = String,
    events(FundsDeposited)
)]
pub struct Account { /* ... */ }
```

Now `Account::KIND` is `"bank-account"`.

## Customizing the Event Enum Name

```rust,ignore
#[derive(Aggregate)]
#[aggregate(
    event_enum = "BankEvent",
    id = String,
    error = String,
    events(FundsDeposited)
)]
pub struct Account { /* ... */ }
```

Now the generated enum is `BankEvent` instead of `AccountEvent`.

## Adding Derives to the Event Enum

By default, the generated enum only derives `Clone`. Add more:

```rust,ignore
#[derive(Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(FundsDeposited),
    derives(Debug, PartialEq)
)]
pub struct Account { /* ... */ }
```

Now the generated enum has `#[derive(Clone, Debug, PartialEq)]`.

## Requirements

Your struct must also derive/implement:

- `Default` — Fresh aggregate state

Each event type must implement `DomainEvent`.

**Note:** Aggregates do not require `Serialize` or `Deserialize` by default. These are only needed if you enable snapshots via `Repository::with_snapshots()`.

## Next

[Manual Implementation](manual-implementation.md) — Implementing without the macro
