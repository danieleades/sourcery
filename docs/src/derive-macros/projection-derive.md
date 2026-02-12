# The Projection Derive

`#[derive(Projection)]` generates a stable projection `KIND`.

For common cases, `#[projection(events(...))]` also generates `ProjectionFilters`.

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
| `events(E1, E2, ...)` | none | Auto-generate `ProjectionFilters::filters()` |
| `id = Type` | `String` | Auto-generated `ProjectionFilters::Id` |
| `instance_id = Type` | `()` | Auto-generated `ProjectionFilters::InstanceId` |
| `metadata = Type` | `()` | Auto-generated `ProjectionFilters::Metadata` |

## What Is Generated

Always:

```rust,ignore
impl Projection for MyProjection {
    const KIND: &'static str = "my-projection";
}
```

When `events(...)` (or filter type overrides) are present, also generates:

- `impl ProjectionFilters for MyProjection`
- `init(...) -> Self::default()`
- `filters(...) -> Filters::new().event::<...>()...`

## When to Go Manual

Manually implement `ProjectionFilters` when you need:

- Dynamic filtering
- `event_for` / `events_for`
- Non-default initialisation
- Full control over filter construction

```rust,ignore
use sourcery::{ApplyProjection, Filters, ProjectionFilters, store::EventStore};

#[derive(Debug, Default, sourcery::Projection)]
pub struct AccountSummary {
    totals: HashMap<String, i64>,
}

impl ProjectionFilters for AccountSummary {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self {
        Self::default()
    }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String>,
        S::Metadata: Clone + Into<()>,
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
