# Commands

Commands request a state change. They can be rejected.

## Basic Usage

1. Define a command struct
2. Implement `Handle<C>` on your aggregate
3. Return events or a domain error

```rust,ignore
#[derive(Debug)]
pub struct Deposit {
    pub amount: i64,
}

impl Handle<Deposit> for Account {
    fn handle(&self, cmd: &Deposit) -> Result<Vec<Self::Event>, Self::Error> {
        if cmd.amount <= 0 {
            return Err(AccountError::InvalidAmount);
        }
        Ok(vec![FundsDeposited { amount: cmd.amount }.into()])
    }
}
```

## Executing Commands

```rust,ignore
repository
    .execute_command::<Account, Deposit>(&account_id, &Deposit { amount: 100 }, &metadata)
    .await?;
```

## Validation Patterns

- Reject invalid input (`Result::Err`)
- Emit one or more events when valid
- Return `vec![]` for valid no-op commands

```rust,ignore
if cmd.amount == 0 {
    return Ok(vec![]);
}
```

## Trait Reference

### The `Handle<C>` Trait

```rust,ignore
{{#include ../../../sourcery-core/src/aggregate.rs:handle_trait}}
```

Key points:

- Takes `&self` (validate against current state)
- Returns `Vec<Event>` (0..n events)
- Returns `Result` (commands can fail)

## Command Naming

Commands are imperative:

| Good | Bad |
|------|-----|
| `Deposit` | `FundsDeposited` |
| `PlaceOrder` | `OrderPlaced` |
| `RegisterUser` | `UserRegistered` |
| `ChangePassword` | `PasswordChanged` |

## Next

[Projections](projections.md) — Building read models from events
