//! A minimal example demonstrating the core concepts of event sourcing.
//!
//! Run with: `cargo run --example quickstart`

// NB: the 'ANCHOR's support embedding in mdbook in docs/ directory.

// ANCHOR: full_example
use serde::{Deserialize, Serialize};
use sourcery::{
    Apply, ApplyProjection, DomainEvent, Filters, Handle, Repository, Subscribable,
    store::{EventStore, inmemory},
};

// ANCHOR: events
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
pub struct Deposit {
    pub amount: i64,
}
// ANCHOR_END: commands

// ANCHOR: aggregate
#[derive(Default, Serialize, Deserialize, sourcery::Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(FundsDeposited),
    derives(Debug, PartialEq, Eq)
)]
pub struct Account {
    balance: i64,
}

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, event: &FundsDeposited) {
        self.balance += event.amount;
    }
}

impl Handle<Deposit> for Account {
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
pub struct TotalDeposits {
    pub total: i64,
}

impl Subscribable for TotalDeposits {
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
        Filters::new().event::<FundsDeposited>()
    }
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

    // Execute a command
    repository
        .execute_command::<Account, Deposit>(&"ACC-001".to_string(), &Deposit { amount: 100 }, &())
        .await?;

    // Load a projection
    let totals = repository.load_projection::<TotalDeposits>(&()).await?;

    println!("Total deposits: {}", totals.total);
    assert_eq!(totals.total, 100);

    Ok(())
}
// ANCHOR_END: main
// ANCHOR_END: full_example
