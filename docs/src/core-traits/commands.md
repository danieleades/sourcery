# Commands

Commands request a state change. They can be rejected.

## Basic Usage

1. Define a command struct
2. Implement `HandleCreate<C>` for create commands (new streams)
3. Implement `Handle<C>` for existing aggregate commands
4. Return events or a domain error

```rust,ignore
#[derive(Debug)]
pub struct Deposit {
    pub amount: i64,
}

impl Handle<Deposit> for Account {
    type HandleError = Self::Error;

    fn handle(&self, cmd: &Deposit) -> Result<Vec<Self::Event>, Self::HandleError> {
        if cmd.amount <= 0 {
            return Err(AccountError::InvalidAmount);
        }
        Ok(vec![FundsDeposited { amount: cmd.amount }.into()])
    }
}
```

## Executing Commands

```rust,ignore
// First command for a new stream
repository
    .create::<Account, OpenAccount>(&account_id, &OpenAccount { initial_balance: 0 }, &metadata)
    .await?;

// Subsequent commands for an existing stream
repository
    .update::<Account, Deposit>(&account_id, &Deposit { amount: 100 }, &metadata)
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
- Uses `HandleError` for command-specific errors, converted into `Aggregate::Error`
- Returns `Vec<Event>` (0..n events)
- Returns `Result` (commands can fail)

### The `HandleCreate<C>` Trait

```rust,ignore
pub trait HandleCreate<C>: Aggregate {
    type HandleCreateError: Into<<Self as Aggregate>::Error>;
    fn handle_create(command: &C) -> Result<Vec<Self::Event>, Self::HandleCreateError>;
}
```

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
