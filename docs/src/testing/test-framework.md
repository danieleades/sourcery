# Test Framework

The crate provides two testing utilities:

- **`TestExecutor`**: Unit testing aggregates in isolation (no stores, no serialization)
- **`RepositoryTestExt`**: Integration testing with real repositories (seeding data, simulating concurrency)

## Enabling the Test Framework

Add the `test-util` feature to your dev dependencies:

```toml
[dev-dependencies]
sourcery = { version = "0.1", features = ["test-util"] }
```

## Basic Usage

```rust,ignore
use sourcery::test::TestExecutor;

#[test]
fn deposit_increases_balance() {
    TestExecutor::<Account>::given(&[FundsDeposited { amount: 100 }.into()])
        .when(&Deposit { amount: 50 })
        .then_expect_events(&[FundsDeposited { amount: 50 }.into()]);
}
```

## Given Methods

Set up initial state with `given(events)`:

```rust,ignore
// With existing events
TestExecutor::<Account>::given(&[
    FundsDeposited { amount: 100 }.into(),
    FundsWithdrawn { amount: 30 }.into(),
])  // Balance is now 70

// Fresh aggregate (pass empty slice)
TestExecutor::<Account>::given(&[])  // Balance is 0
```

## When Methods

### `when(command)`

Execute a command against the aggregate:

```rust,ignore
.when(&Withdraw { amount: 50 })
```

## Then Methods

Assert outcomes with these methods:

```rust,ignore
// Expect specific events (requires PartialEq on events)
.then_expect_events(&[FundsWithdrawn { amount: 50 }.into()])

// Expect no events (valid no-op)
.then_expect_no_events()

// Expect any error
.then_expect_error()

// Expect specific error
.then_expect_error_eq(&AccountError::InsufficientFunds)
```

Additional methods: `then_expect_error_message(substring)` for substring matching, `inspect_result(closure)` for custom assertions.

## Complete Test Suite Example

```rust,ignore
use sourcery::test::TestExecutor;

#[test]
fn deposits_positive_amount() {
    TestExecutor::<Account>::given(&[])
        .when(&Deposit { amount: 100 })
        .then_expect_events(&[FundsDeposited { amount: 100 }.into()]);
}

#[test]
fn rejects_overdraft() {
    TestExecutor::<Account>::given(&[FundsDeposited { amount: 100 }.into()])
        .when(&Withdraw { amount: 150 })
        .then_expect_error_eq(&AccountError::InsufficientFunds);
}

#[test]
fn rejects_invalid_deposit() {
    TestExecutor::<Account>::given(&[])
        .when(&Deposit { amount: -50 })
        .then_expect_error();
}
```

## Testing Projections

Projections don't use `TestExecutor`. Test them directly:

```rust,ignore
#[test]
fn projection_aggregates_deposits() {
    let mut proj = AccountSummary::default();

    proj.apply_projection("ACC-001", &FundsDeposited { amount: 100 }, &());
    proj.apply_projection("ACC-002", &FundsDeposited { amount: 50 }, &());
    proj.apply_projection("ACC-001", &FundsDeposited { amount: 25 }, &());

    assert_eq!(proj.accounts.get("ACC-001"), Some(&125));
    assert_eq!(proj.accounts.get("ACC-002"), Some(&50));
}
```

## Next

[Design Decisions](../reference/design-decisions.md) — Why the crate works this way
