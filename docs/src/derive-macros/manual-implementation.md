# Manual Implementation

While `#[derive(Aggregate)]` handles most cases, you might implement the traits manually for:

- Custom serialisation logic
- Non-standard event routing
- Learning how the system works

## Side-by-Side Comparison

### With Derive Macro

```rust,ignore
use sourcery::{Aggregate, Apply, DomainEvent, Handle};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundsDeposited { pub amount: i64 }

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "account.deposited";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundsWithdrawn { pub amount: i64 }

impl DomainEvent for FundsWithdrawn {
    const KIND: &'static str = "account.withdrawn";
}

#[derive(Debug, Default, Serialize, Deserialize, Aggregate)]
#[aggregate(id = String, error = String, events(FundsDeposited, FundsWithdrawn))]
pub struct Account {
    balance: i64,
}

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, e: &FundsDeposited) { self.balance += e.amount; }
}

impl Apply<FundsWithdrawn> for Account {
    fn apply(&mut self, e: &FundsWithdrawn) { self.balance -= e.amount; }
}
```

### Without Derive Macro

```rust,ignore
use sourcery::{Aggregate, DomainEvent, EventKind, ProjectionEvent};
use sourcery::event::EventDecodeError;
use sourcery::store::{EventStore, StoredEvent};
use serde::{Deserialize, Serialize};

// Events (same as before)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundsDeposited { pub amount: i64 }

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "account.deposited";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundsWithdrawn { pub amount: i64 }

impl DomainEvent for FundsWithdrawn {
    const KIND: &'static str = "account.withdrawn";
}

// Manual event enum
#[derive(Clone, Debug)]
pub enum AccountEvent {
    Deposited(FundsDeposited),
    Withdrawn(FundsWithdrawn),
}

// From implementations for ergonomic .into()
impl From<FundsDeposited> for AccountEvent {
    fn from(e: FundsDeposited) -> Self { Self::Deposited(e) }
}

impl From<FundsWithdrawn> for AccountEvent {
    fn from(e: FundsWithdrawn) -> Self { Self::Withdrawn(e) }
}

// EventKind for runtime kind dispatch
impl EventKind for AccountEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::Deposited(_) => FundsDeposited::KIND,
            Self::Withdrawn(_) => FundsWithdrawn::KIND,
        }
    }
}

// Serialize — serialises only the inner event (no enum wrapper)
impl serde::Serialize for AccountEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Deposited(inner) => inner.serialize(serializer),
            Self::Withdrawn(inner) => inner.serialize(serializer),
        }
    }
}

// ProjectionEvent for loading/deserialisation
impl ProjectionEvent for AccountEvent {
    const EVENT_KINDS: &'static [&'static str] = &[
        FundsDeposited::KIND,
        FundsWithdrawn::KIND,
    ];

    fn from_stored<S: EventStore>(
        stored: &StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
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

// Aggregate struct
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Account {
    balance: i64,
}

// Manual Aggregate implementation
impl Aggregate for Account {
    const KIND: &'static str = "account";
    type Event = AccountEvent;
    type Error = String;
    type Id = String;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::Deposited(e) => self.balance += e.amount,
            AccountEvent::Withdrawn(e) => self.balance -= e.amount,
        }
    }
}
```

## Trade-offs

**You gain**: Custom enum structure, custom serialisation (compress/encrypt), fallback handling for unknown events, conditional replay logic.

**You lose**: More code to maintain, easy to introduce bugs in match arms, must keep `EVENT_KINDS` in sync with `from_stored`.

## When to Go Manual

| Scenario | Recommendation |
|----------|----------------|
| Standard CRUD aggregate | Use derive macro |
| Learning the crate | Start with derive, then explore manual |
| Custom event serialisation | Manual |
| Dynamic event types | Manual |
| Unusual enum structure | Manual |

## Next

[Snapshots](../advanced/snapshots.md) — Optimising aggregate loading
