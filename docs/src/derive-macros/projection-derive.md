# The Projection Derive

The `#[derive(Projection)]` macro implements the `Projection` trait for a
struct, generating the `KIND` constant. You implement `Subscribable` and
`ApplyProjection<E>` manually.

## What Gets Generated

The macro generates only:

```rust,ignore
impl Projection for MyProjection {
    const KIND: &'static str = "my-projection"; // kebab-case by default
}
```

## Attributes

All attributes are optional:

| Attribute | Default | Description |
|-----------|---------|-------------|
| `kind = "name"` | kebab-case struct name | Projection type identifier for snapshots |

## Minimal Example

```rust,ignore
#[derive(Default, sourcery::Projection)]
pub struct AccountLedger {
    total: i64,
}
```

This generates `impl Projection for AccountLedger` with `KIND = "account-ledger"`.

## Custom Kind

```rust,ignore
#[derive(Default, sourcery::Projection)]
#[projection(kind = "audit-log")]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}
```

## Complete Projection Setup

The derive only generates the `Projection` impl. You must also implement
`Subscribable` (for filter configuration) and `ApplyProjection<E>` (for event
handlers):

```rust,ignore
use sourcery::{ApplyProjection, Filters, Subscribable, store::EventStore};

#[derive(Debug, Default, sourcery::Projection)]
pub struct AccountSummary {
    totals: HashMap<String, i64>,
}

impl Subscribable for AccountSummary {
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

impl ApplyProjection<FundsDeposited> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, _: &Self::Metadata) {
        *self.totals.entry(id.clone()).or_default() += event.amount;
    }
}
```

## Loading

Since filters are defined centrally in `Subscribable::filters()`, loading is a
single call:

```rust,ignore
let projection = repository
    .load_projection::<AccountSummary>(&())
    .await?;
```
