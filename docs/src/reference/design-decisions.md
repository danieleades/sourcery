# Design Decisions

This page explains the reasoning behind key architectural choices in the crate.

## Events as First-Class Structs

**Decision**: Events are standalone structs implementing `DomainEvent`, not just enum variants.

**Why**:
- Events can be shared across multiple aggregates
- Projections subscribe to event types, not aggregate enums
- Easier to evolve events independently
- Better compile-time isolation

**Trade-off**: More boilerplate (mitigated by derive macro).

```rust,ignore
// This crate: events stand alone
struct OrderPlaced { /* ... */ }
struct PaymentReceived { /* ... */ }

// Both Order and Payment aggregates can use PaymentReceived
```

## IDs as Infrastructure

**Decision**: Aggregate IDs are passed to the repository, not embedded in events or handlers.

**Why**:
- Domain events stay pure (no infrastructure metadata)
- Same event type works with different ID schemes
- Command handlers focus on behaviour, not identity
- IDs travel in the envelope alongside events

**Trade-off**: You can't access the ID inside `Handle<C>`. If needed, include relevant IDs in the command.

```rust,ignore
// ID is infrastructure
repository
    .create::<Account, OpenAccount>(&account_id, &open, &metadata)
    .await?;
repository
    .update::<Account, Deposit>(&account_id, &deposit, &metadata)
    .await?;

// Event doesn't contain ID
struct FundsDeposited { amount: i64 }  // No account_id field
```

## No Built-In Command Bus

**Decision**: The crate provides `Repository::create()` / `Repository::update()` but no command routing infrastructure.

**Why**:
- Command buses vary widely (sync, async, distributed, in-process)
- Many applications don't need one (web handlers call repository directly)
- Easy to add on top: trait object dispatch, actor messages, etc.
- Keeps the crate focused on event sourcing, not messaging

**What you might build**:
```rust,ignore
// Your command bus (not provided)
trait CommandBus {
    fn dispatch<C: Command>(&self, id: &str, command: C) -> Result<(), Error>;
}
```

## Event Serialisation in Stores

**Decision**: EventStore implementations handle serialisation, not a separate Codec trait. The generated event enum implements `serde::Serialize` with custom logic that serialises only the inner event struct.

**Why**:
- Eliminates unnecessary abstraction layer (Codec trait)
- Store can choose optimal format (JSON, JSONB, MessagePack, etc.)
- Simpler architecture with fewer generic parameters
- Event enum variants are never serialised as tagged enums - only the inner event is serialised

**How it works**:
```rust,ignore
// The Aggregate macro generates an event enum
#[derive(Aggregate)]
#[aggregate(id = String, error = String, events(FundsDeposited, FundsWithdrawn))]
struct Account { /* ... */ }

// Generated code (simplified):
enum AccountEvent {
    FundsDeposited(FundsDeposited),
    FundsWithdrawn(FundsWithdrawn),
}

// Generated Serialize impl — serialises only the inner event
impl serde::Serialize for AccountEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        match self {
            Self::FundsDeposited(inner) => inner.serialize(serializer),
            Self::FundsWithdrawn(inner) => inner.serialize(serializer),
        }
    }
}

// EventStore uses kind() for routing and serde::Serialize for data
// - kind() returns the event type identifier ("account.deposited")
// - Serialize serialises just the inner event data
```

**Event versioning**: Use serde attributes on the event structs themselves:
```rust,ignore
#[derive(serde::Serialize, serde::Deserialize)]
struct FundsDeposited {
    amount: i64,
    #[serde(default)]  // Field added in v2
    currency: String,
}
```

## Projections Decoupled from Aggregates

**Decision**: Projections implement `ApplyProjection<E>` for event types, not aggregate types.

**Why**:
- A projection can consume events from many aggregates
- Projections don't need to know about aggregate enums
- Enables cross-aggregate views naturally
- Better separation of read and write concerns

```rust,ignore
// Projection consumes from multiple aggregates
#[derive(Default, sourcery::Projection)]
#[projection(kind = "dashboard")]
struct Dashboard { /* ... */ }

impl ProjectionFilters for Dashboard {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();
    fn init((): &()) -> Self { Self::default() }
    fn filters<S>((): &()) -> Filters<S, Self>
    where S: EventStore<Id = String, Metadata = ()> {
        Filters::new()
            .event::<OrderPlaced>()
            .event::<PaymentReceived>()
            .event::<ShipmentDispatched>()
    }
}

impl ApplyProjection<OrderPlaced> for Dashboard { /* ... */ }
impl ApplyProjection<PaymentReceived> for Dashboard { /* ... */ }
impl ApplyProjection<ShipmentDispatched> for Dashboard { /* ... */ }
```

## Minimal Infrastructure

**Decision**: No outbox, no scheduler, no event streaming, no saga orchestrator.

**Why**:
- Event sourcing is the foundation; infrastructure varies by use case
- Message brokers differ (Kafka, RabbitMQ, NATS, none)
- Database choices affect outbox patterns
- Keeps dependencies minimal
- May be added later.

**What we do provide**:
- Core traits for aggregates and projections
- Repository for command execution
- In-memory store for testing
- Test framework for aggregate testing

**What you add**:
- Production event store
- Outbox pattern (if needed)
- Process managers / sagas
- Event publishing to message broker
