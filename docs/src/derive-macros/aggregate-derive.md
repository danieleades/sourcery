# The Aggregate Derive

`#[derive(Aggregate)]` removes aggregate boilerplate by generating the event enum and aggregate wiring.

## Basic Usage

```rust,ignore
use sourcery::Aggregate;

#[derive(Debug, Default, Aggregate)]
#[aggregate(id = String, error = String, events(FundsDeposited, FundsWithdrawn))]
pub struct Account {
    balance: i64,
}
```

Then add:

- `Apply<E>` impls for each event
- `Handle<C>` impls for each command

## Attribute Reference

| Attribute | Required | Description |
|-----------|----------|-------------|
| `id = Type` | Yes | Aggregate identifier type |
| `error = Type` | Yes | Error type for command handling |
| `events(E1, E2, ...)` | Yes | Event types this aggregate produces |
| `kind = "name"` | No | Aggregate type identifier (default: lowercase struct name) |
| `event_enum = "Name"` | No | Generated enum name (default: `{Struct}Event`) |
| `derives(T1, T2, ...)` | No | Additional derives for generated event enum (always includes `Clone`) |

## Typical Customisation

### Customising the Kind

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

### Customising Event Enum Name

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

### Adding Derives to Event Enum

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

## What Gets Generated (Reference)

Given:

```rust,ignore
#[derive(Aggregate)]
#[aggregate(id = String, error = AccountError, events(FundsDeposited, FundsWithdrawn))]
pub struct Account { balance: i64 }
```

The macro generates:

- `enum AccountEvent { ... }`
- `impl Aggregate for Account`
- `impl From<E> for AccountEvent` per event
- `impl EventKind for AccountEvent`
- `impl serde::Serialize for AccountEvent`
- `impl ProjectionEvent for AccountEvent`

### Event Enum

```rust,ignore
#[derive(Clone)]
pub enum AccountEvent {
    FundsDeposited(FundsDeposited),
    FundsWithdrawn(FundsWithdrawn),
}
```

### `From` Implementations

```rust,ignore
impl From<FundsDeposited> for AccountEvent {
    fn from(event: FundsDeposited) -> Self {
        AccountEvent::FundsDeposited(event)
    }
}
```

### Aggregate Implementation

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

### `ProjectionEvent` Implementation

The generated enum supports replay/deserialisation from stored events by kind.

## Requirements

- Aggregate struct must implement `Default`.
- Event types must implement `DomainEvent`.
- `Serialize + DeserializeOwned` on aggregate state is only required when using snapshots.

## Next

[Manual Implementation](manual-implementation.md) — Implementing without the macro
