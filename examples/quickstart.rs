//! A minimal example demonstrating the core concepts of event sourcing.
//!
//! Run with: `cargo run --example quickstart`

// NB: the 'ANCHOR's support embedding in mdbook in docs/ directory.

// ANCHOR: full_example
use serde::{Deserialize, Serialize};
use sourcery::{
    Apply, ApplyProjection, Create, DomainEvent, Handle, HandleCreate, Repository, store::inmemory,
};

// ANCHOR: events
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountOpened {
    pub initial_balance: i64,
}

impl DomainEvent for AccountOpened {
    const KIND: &'static str = "account.opened";
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundsDeposited {
    pub amount: i64,
}

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "account.deposited";
}
// ANCHOR_END: events

// ANCHOR: commands
#[derive(Debug)]
pub struct OpenAccount {
    pub initial_balance: i64,
}

#[derive(Debug)]
pub struct Deposit {
    pub amount: i64,
}
// ANCHOR_END: commands

// ANCHOR: aggregate
#[derive(Default, Serialize, Deserialize, sourcery::Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(AccountOpened, FundsDeposited),
    create(AccountOpened),
    derives(Debug, PartialEq, Eq)
)]
pub struct Account {
    balance: i64,
}

impl Create<AccountOpened> for Account {
    fn create(event: &AccountOpened) -> Self {
        Self {
            balance: event.initial_balance,
        }
    }
}

impl Apply<AccountOpened> for Account {
    fn apply(&mut self, event: &AccountOpened) {
        self.balance = event.initial_balance;
    }
}

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, event: &FundsDeposited) {
        self.balance += event.amount;
    }
}

impl Handle<OpenAccount> for Account {
    type HandleError = Self::Error;

    fn handle(&self, cmd: &OpenAccount) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            AccountOpened {
                initial_balance: cmd.initial_balance,
            }
            .into(),
        ])
    }
}

impl HandleCreate<OpenAccount> for Account {
    type HandleCreateError = Self::Error;

    fn handle_create(cmd: &OpenAccount) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            AccountOpened {
                initial_balance: cmd.initial_balance,
            }
            .into(),
        ])
    }
}

impl Handle<Deposit> for Account {
    type HandleError = Self::Error;

    fn handle(&self, cmd: &Deposit) -> Result<Vec<Self::Event>, Self::Error> {
        if cmd.amount <= 0 {
            return Err("amount must be positive".into());
        }
        Ok(vec![FundsDeposited { amount: cmd.amount }.into()])
    }
}
// ANCHOR_END: aggregate

// ANCHOR: projection
#[derive(Debug, Default, sourcery::Projection)]
#[projection(events(FundsDeposited))]
pub struct TotalDeposits {
    pub total: i64,
}

impl ApplyProjection<FundsDeposited> for TotalDeposits {
    fn apply_projection(&mut self, _id: &Self::Id, event: &FundsDeposited, _meta: &Self::Metadata) {
        self.total += event.amount;
    }
}
// ANCHOR_END: projection

// ANCHOR: main
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create an in-memory store
    let store = inmemory::Store::new();
    let repository = Repository::new(store);

    // Open a new account — this is the creation event, handled by
    // Create<AccountOpened>
    repository
        .create::<Account, OpenAccount>(
            &"ACC-001".to_string(),
            &OpenAccount { initial_balance: 0 },
            &(),
        )
        .await?;

    // Execute a deposit command
    repository
        .update::<Account, Deposit>(&"ACC-001".to_string(), &Deposit { amount: 100 }, &())
        .await?;

    // Load a projection
    let totals = repository.load_projection::<TotalDeposits>(&()).await?;

    println!("Total deposits: {}", totals.total);
    assert_eq!(totals.total, 100);

    Ok(())
}
// ANCHOR_END: main
// ANCHOR_END: full_example
