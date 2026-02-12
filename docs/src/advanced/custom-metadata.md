# Custom Metadata

Metadata carries infrastructure concerns alongside events without polluting domain types. Common uses include correlation IDs, user context, timestamps, and causation tracking.

## Defining Metadata

Create a struct for your metadata:

```rust,ignore
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventMetadata {
    pub correlation_id: String,
    pub causation_id: Option<String>,
    pub user_id: String,
    pub timestamp: DateTime<Utc>,
}
```

## Using Metadata with the Store

Configure your store with the metadata type:

```rust,ignore
use sourcery::store::inmemory;

let store: inmemory::Store<String, EventMetadata> = inmemory::Store::new();
```

## Passing Metadata to Commands

Provide metadata when executing commands:

```rust,ignore
let metadata = EventMetadata {
    correlation_id: Uuid::new_v4().to_string(),
    causation_id: None,
    user_id: current_user.id.clone(),
    timestamp: Utc::now(),
};

repository.execute_command::<Account, Deposit>(
    &account_id,
    &Deposit { amount: 100 },
    &metadata,
)
.await?;
```

Each event produced by the command receives this metadata.

## Accessing Metadata in Projections

Set metadata in the derive attribute and receive it in `ApplyProjection`:

```rust,ignore
use sourcery::ApplyProjection;

#[derive(Debug, Default, sourcery::Projection)]
#[projection(metadata = EventMetadata, events(FundsDeposited))]
pub struct AuditLog {
    pub entries: Vec<AuditEntry>,
}

impl ApplyProjection<FundsDeposited> for AuditLog {
    fn apply_projection(
        &mut self,
        aggregate_id: &Self::Id,
        event: &FundsDeposited,
        meta: &Self::Metadata,
    ) {
        self.entries.push(AuditEntry {
            timestamp: meta.timestamp,
            user: meta.user_id.clone(),
            correlation_id: meta.correlation_id.clone(),
            action: format!("Deposited {} to account {}", event.amount, aggregate_id),
        });
    }
}
```

Projection handlers use the same metadata type as the store for that repository.

## Correlation and Causation

Track event relationships for debugging and workflows:

- **Correlation ID**: Groups all events from a single user request
- **Causation ID**: Points to the event that triggered this one

```rust,ignore
// When handling a saga or process manager
let follow_up_metadata = EventMetadata {
    correlation_id: original_meta.correlation_id.clone(),
    causation_id: Some(original_event_id.to_string()),
    user_id: "system".to_string(),
    timestamp: Utc::now(),
};
```

## Unit Metadata

If you don't need metadata, use `()`:

```rust,ignore
let store: inmemory::Store<String, ()> = inmemory::Store::new();

repository
    .execute_command::<Account, Deposit>(&id, &cmd, &())
    .await?;
```

Projections with `type Metadata = ()` ignore the metadata parameter.

## Metadata vs Event Data

| Put in Metadata | Put in Event |
|-----------------|--------------|
| Who did it (user ID) | What happened (domain facts) |
| When (timestamp) | Domain-relevant times (due date) |
| Request tracing (correlation) | Business identifiers |
| Infrastructure context | Domain context |

Events should be understandable without metadata. Metadata enhances observability.

## Example: Multi-Tenant Filtering

Metadata enables tenant-scoped projections. With `type Metadata = TenantMetadata` on `ProjectionFilters`, the handler can filter by tenant:

```rust,ignore
impl ApplyProjection<OrderPlaced> for TenantDashboard {
    fn apply_projection(
        &mut self,
        _id: &Self::Id,
        event: &OrderPlaced,
        meta: &Self::Metadata,
    ) {
        if meta.tenant_id == self.tenant_id {
            self.order_count += 1;
            self.total_revenue += event.total;
        }
    }
}
```

The projection follows the same pattern shown in [Accessing Metadata in Projections](#accessing-metadata-in-projections), with `instance_id = String` if each tenant needs its own projection instance.

## Next

[Custom Stores](custom-stores.md) — Implementing your own persistence layer
