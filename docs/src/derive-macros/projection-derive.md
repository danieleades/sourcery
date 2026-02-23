# The Projection Derive

`#[derive(Projection)]` generates a complete `Projection` implementation.

For common cases, `#[projection(events(...))]` also generates
`Projection::filters()` automatically.

## Basic Usage

```rust,ignore
use sourcery::ApplyProjection;

#[derive(Debug, Default, sourcery::Projection)]
#[projection(events(FundsDeposited, FundsWithdrawn))]
pub struct AccountSummary {
    totals: HashMap<String, i64>,
}

impl ApplyProjection<FundsDeposited> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, _: &Self::Metadata) {
        *self.totals.entry(id.clone()).or_default() += event.amount;
    }
}

impl ApplyProjection<FundsWithdrawn> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsWithdrawn, _: &Self::Metadata) {
        *self.totals.entry(id.clone()).or_default() -= event.amount;
    }
}
```

That is the recommended default path.

## Attributes

| Attribute | Default | Description |
|-----------|---------|-------------|
| `kind = "name"` | kebab-case struct name | Projection identifier for snapshots |
| `events(E1, E2, ...)` | none | Auto-generate `Projection::filters()` with `.event::<E>()` calls |
| `id = Type` | `String` | Sets `Projection::Id` |
| `instance_id = Type` | `()` | Sets `Projection::InstanceId` |
| `metadata = Type` | `()` | Sets `Projection::Metadata` |

## What Is Generated

Always:

```rust,ignore
impl Projection for MyProjection {
    const KIND: &'static str = "my-projection";
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init(_instance_id: &Self::InstanceId) -> Self {
        Self::default()
    }

    fn filters<S>(_instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        Filters::new()
    }
}
```

When `events(...)` is present, the generated `filters(...)` body becomes:

- `Filters::new().event::<Event1>()...`
- one `.event::<...>()` call per listed event type

## When to Go Manual

Manually implement `Projection` when you need:

- Dynamic filtering
- `event_for` / `events_for`
- Non-default initialisation
- Full control over filter construction

```rust,ignore
use sourcery::{ApplyProjection, Filters, Projection, store::EventStore};

#[derive(Debug, Default)]
pub struct AccountSummary {
    totals: HashMap<String, i64>,
}

impl Projection for AccountSummary {
    const KIND: &'static str = "account-summary";
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self {
        Self::default()
    }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new()
            .event::<FundsDeposited>()
            .event::<FundsWithdrawn>()
    }
}
```

## Loading

```rust,ignore
let projection = repository
    .load_projection::<AccountSummary>(&())
    .await?;
```
