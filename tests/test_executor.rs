//! Integration tests for aggregate behavior using the test utilities.
//! Run with `cargo test --features test-util`.

#[cfg(feature = "test-util")]
mod with_test_util {
    use serde::{Deserialize, Serialize};
    use sourcery::{
        Aggregate, Apply, DomainEvent, Handle,
        test::TestExecutor,
    };

    // ============================================================================
    // Test Domain: Bank Account
    // ============================================================================

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct MoneyDeposited {
        amount: u64,
    }

    impl DomainEvent for MoneyDeposited {
        const KIND: &'static str = "money-deposited";
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct MoneyWithdrawn {
        amount: u64,
    }

    impl DomainEvent for MoneyWithdrawn {
        const KIND: &'static str = "money-withdrawn";
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct AccountOpened {
        initial_balance: u64,
    }

    impl DomainEvent for AccountOpened {
        const KIND: &'static str = "account-opened";
    }

    #[derive(Default, Serialize, Deserialize, Aggregate)]
    #[aggregate(
        id = String,
        error = String,
        events(AccountOpened, MoneyDeposited, MoneyWithdrawn),
        derives(Debug, PartialEq)
    )]
    struct BankAccount {
        balance: u64,
        is_open: bool,
    }

    impl Apply<AccountOpened> for BankAccount {
        fn apply(&mut self, event: &AccountOpened) {
            self.is_open = true;
            self.balance = event.initial_balance;
        }
    }

    impl Apply<MoneyDeposited> for BankAccount {
        fn apply(&mut self, event: &MoneyDeposited) {
            self.balance += event.amount;
        }
    }

    impl Apply<MoneyWithdrawn> for BankAccount {
        fn apply(&mut self, event: &MoneyWithdrawn) {
            self.balance -= event.amount;
        }
    }

    // Commands
    struct OpenAccount {
        initial_balance: u64,
    }

    struct Deposit {
        amount: u64,
    }

    struct Withdraw {
        amount: u64,
    }

    impl Handle<OpenAccount> for BankAccount {
        fn handle(&self, command: &OpenAccount) -> Result<Vec<Self::Event>, Self::Error> {
            if self.is_open {
                return Err("account already open".to_string());
            }
            Ok(vec![AccountOpened {
                initial_balance: command.initial_balance,
            }.into()])
        }
    }

    impl Handle<Deposit> for BankAccount {
        fn handle(&self, command: &Deposit) -> Result<Vec<Self::Event>, Self::Error> {
            if !self.is_open {
                return Err("account not open".to_string());
            }
            if command.amount == 0 {
                return Err("amount must be positive".to_string());
            }
            Ok(vec![MoneyDeposited {
                amount: command.amount,
            }.into()])
        }
    }

    impl Handle<Withdraw> for BankAccount {
        fn handle(&self, command: &Withdraw) -> Result<Vec<Self::Event>, Self::Error> {
            if !self.is_open {
                return Err("account not open".to_string());
            }
            if command.amount == 0 {
                return Err("amount must be positive".to_string());
            }
            if command.amount > self.balance {
                return Err("insufficient funds".to_string());
            }
            Ok(vec![MoneyWithdrawn {
                amount: command.amount,
            }.into()])
        }
    }

    // ============================================================================
    // Tests
    // ============================================================================

    #[test]
    fn open_account_produces_event() {
        TestExecutor::<BankAccount>::given(&[])
            .when(&OpenAccount {
                initial_balance: 100,
            })
            .then_expect_events(&[AccountOpened {
                initial_balance: 100,
            }.into()]);
    }

    #[test]
    fn cannot_open_already_open_account() {
        TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 0,
        }.into()])
        .when(&OpenAccount {
            initial_balance: 100,
        })
        .then_expect_error_message("already open");
    }

    #[test]
    fn deposit_increases_balance() {
        TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 100,
        }.into()])
        .when(&Deposit { amount: 50 })
        .then_expect_events(&[MoneyDeposited { amount: 50 }.into()]);
    }

    #[test]
    fn cannot_deposit_to_closed_account() {
        TestExecutor::<BankAccount>::given(&[])
            .when(&Deposit { amount: 50 })
            .then_expect_error_message("not open");
    }

    #[test]
    fn withdraw_decreases_balance() {
        TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 100,
        }.into()])
        .when(&Withdraw { amount: 30 })
        .then_expect_events(&[MoneyWithdrawn { amount: 30 }.into()]);
    }

    #[test]
    fn cannot_withdraw_more_than_balance() {
        TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 100,
        }.into()])
        .when(&Withdraw { amount: 150 })
        .then_expect_error_message("insufficient funds");
    }

    #[test]
    fn cannot_withdraw_from_closed_account() {
        TestExecutor::<BankAccount>::given(&[])
            .when(&Withdraw { amount: 50 })
            .then_expect_error_message("not open");
    }

    #[test]
    fn state_is_rebuilt_from_event_history() {
        // Verify that given() properly rebuilds state from events
        TestExecutor::<BankAccount>::given(&[
            AccountOpened {
                initial_balance: 100,
            }.into(),
            MoneyDeposited { amount: 50 }.into(),
            MoneyWithdrawn { amount: 30 }.into(),
        ])
        // Balance should be 100 + 50 - 30 = 120
        // Withdrawing 120 should succeed
        .when(&Withdraw { amount: 120 })
        .then_expect_events(&[MoneyWithdrawn { amount: 120 }.into()]);
    }

    #[test]
    fn and_allows_building_complex_state() {
        TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 100,
        }.into()])
        .and(vec![MoneyDeposited {
            amount: 200,
        }.into()])
        .and(vec![MoneyWithdrawn {
            amount: 50,
        }.into()])
        // Balance: 100 + 200 - 50 = 250
        .when(&Withdraw { amount: 250 })
        .then_expect_events(&[MoneyWithdrawn { amount: 250 }.into()]);
    }

    #[test]
    fn inspect_result_allows_custom_assertions() {
        let result = TestExecutor::<BankAccount>::given(&[AccountOpened {
            initial_balance: 100,
        }.into()])
        .when(&Deposit { amount: 50 })
        .inspect_result();

        assert!(result.is_ok());
        let events = result.unwrap();
        assert_eq!(events.len(), 1);
        // The generated event enum uses PascalCase variant names derived from the event type
        // For example: MoneyDeposited becomes the MoneyDeposited variant
        match &events[0] {
            BankAccountEvent::MoneyDeposited(e) => assert_eq!(e.amount, 50),
            _ => panic!("Expected MoneyDeposited event"),
        }
    }
}

#[cfg(not(feature = "test-util"))]
#[test]
fn test_util_feature_is_required() {
    panic!(
        "Integration tests require the `test-util` feature. Run `cargo test --features test-util` \
         to execute them."
    );
}
