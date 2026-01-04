# The Projection Derive

The `#[derive(Projection)]` macro implements the `Projection` trait for a
struct with sensible defaults, so you can focus on `ApplyProjection<E>`
handlers and builder configuration.

## Attributes

All configuration happens via the `#[projection(...)]` attribute:

- `id = Type` (**required**) — Aggregate ID type the projection reads.
- `metadata = Type` (optional, default `()`) — Metadata type passed to
  `ApplyProjection<E>` handlers.
- `instance_id = Type` (optional, default `()`) — Projection instance identifier
  type used for snapshots.
- `kind = "name"` (optional, default kebab-case struct name) — Projection kind
  identifier.

## Example

```rust,ignore
#[derive(Default, sourcery::Projection)]
#[projection(id = String)]
pub struct AccountSummary {
    totals: HashMap<String, i64>,
}

impl ApplyProjection<FundsDeposited> for AccountSummary {
    fn apply_projection(&mut self, id: &Self::Id, event: &FundsDeposited, _: &Self::Metadata) {
        *self.totals.entry(id.clone()).or_default() += event.amount;
    }
}
```

## Metadata + Custom Kind

```rust,ignore
#[derive(Default, sourcery::Projection)]
#[projection(id = String, metadata = EventMetadata, kind = "audit-log")]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}
```

## Event Registration

The derive does **not** register events. Use the builder API to pick which
events are applied:

```rust,ignore
let projection = repository
    .build_projection::<AccountSummary>()
    .event::<FundsDeposited>()
    .event::<FundsWithdrawn>()
    .load()
    .await?;
```
