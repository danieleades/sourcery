# Test Framework

The crate provides two testing utilities:

- **`TestFramework`**: Unit testing aggregates in isolation (no stores, no serialization)
- **`RepositoryTestExt`**: Integration testing with real repositories (seeding data, simulating concurrency)

## Enabling the Test Framework

Add the `test-util` feature to your dev dependencies:

```toml
[dev-dependencies]
sourcery = { version = "0.1", features = ["test-util"] }
```

## Basic Usage

```rust,ignore
use sourcery::test::TestFramework;

#[test]
fn deposit_increases_balance() {
    TestFramework::<Account>::given(&[FundsDeposited { amount: 100 }.into()])
        .when(&Deposit { amount: 50 })
        .then_expect_events(&[FundsDeposited { amount: 50 }.into()]);
}
```

## Given Methods

Set up initial state with `given(events)`:

```rust,ignore
// With existing events
TestFramework::<Account>::given(&[
    AccountOpened { initial_balance: 100 }.into(),
    FundsDeposited { amount: 100 }.into(),
    FundsWithdrawn { amount: 30 }.into(),
])  // Balance is now 70

// No stream yet
TestFramework::<Account>::new()
```

## When Methods

### `when_create(command)`

Execute a creation command against a stream that does not yet exist:

```rust,ignore
TestFramework::<Account>::new()
    .when_create(&OpenAccount { initial_balance: 100 })
```

### `when(command)`

Execute an update command against an existing aggregate:

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

Additional methods: `then_expect_error_message(substring)` for substring matching, `inspect_result()` to get the raw `Result` for custom assertions.

## Complete Test Suite Example

```rust,ignore
use sourcery::test::TestFramework;

#[test]
fn opens_account() {
    TestFramework::<Account>::new()
        .when_create(&OpenAccount { initial_balance: 100 })
        .then_expect_events(&[AccountOpened { initial_balance: 100 }.into()]);
}

#[test]
fn rejects_overdraft() {
    TestFramework::<Account>::given(&[
        AccountOpened { initial_balance: 100 }.into(),
        FundsDeposited { amount: 100 }.into(),
    ])
        .when(&Withdraw { amount: 150 })
        .then_expect_error_eq(&AccountError::InsufficientFunds);
}

#[test]
fn rejects_invalid_opening_balance() {
    TestFramework::<Account>::new()
        .when_create(&OpenAccount { initial_balance: 0 })
        .then_expect_error();
}
```

## Testing Projections

Projections don't use `TestFramework`. Test them directly by calling `init()` and `apply_projection()`:

```rust,ignore
#[test]
fn projection_aggregates_deposits() {
    let mut proj = AccountSummary::init(&());

    proj.apply_projection(&"ACC-001".to_string(), &FundsDeposited { amount: 100 }, &());
    proj.apply_projection(&"ACC-002".to_string(), &FundsDeposited { amount: 50 }, &());
    proj.apply_projection(&"ACC-001".to_string(), &FundsDeposited { amount: 25 }, &());

    assert_eq!(proj.accounts.get("ACC-001"), Some(&125));
    assert_eq!(proj.accounts.get("ACC-002"), Some(&50));
}
```

## Next

[Design Decisions](../reference/design-decisions.md) — Why the crate works this way
