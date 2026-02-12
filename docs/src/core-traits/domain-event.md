# Domain Events

Events are facts: things that already happened.

## Basic Usage

Define a plain struct and implement `DomainEvent` with a stable `KIND`.

```rust,ignore
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub product_id: String,
    pub quantity: u32,
    pub unit_price: i64,
}

impl DomainEvent for OrderPlaced {
    const KIND: &'static str = "order.placed";
}
```

## Rules of Thumb

- Use past tense names (`OrderPlaced`, not `PlaceOrder`)
- Keep `KIND` stable forever once persisted
- Keep payload focused on a single fact

## Why First-Class Structs?

This crate keeps events as standalone structs rather than forcing enum variants:

- Reuse across aggregates
- Decoupled projections by event type
- Smaller, explicit imports

The aggregate derive still generates an internal event enum for replay and storage routing.

## `KIND` Naming Guidance

```rust,ignore
const KIND: &'static str = "order.placed"; // Good
const KIND: &'static str = "OrderPlaced";  // Acceptable
const KIND: &'static str = "placed";       // Too generic
```

## Event Design Guidelines

1. Include values needed to replay decisions later.
2. Keep aggregate IDs in the envelope, not event payload.
3. Prefer multiple specific events over one broad event.

## Trait Reference

```rust,ignore
pub trait DomainEvent {
    const KIND: &'static str;
}
```

## Next

[Commands](commands.md) — Handling requests that produce events
