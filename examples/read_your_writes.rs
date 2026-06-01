//! Read-your-writes example for subscription-fed read models.
//!
//! `load_projection` rebuilds from the event stream and already sees prior
//! writes. Consistency tokens are for live projections maintained by
//! `subscribe()`, where a command can commit just before the read side has
//! observed the event.
//!
//! Run with: `cargo run --example read_your_writes`

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, Create, DomainEvent, Handle, HandleCreate, Projection,
    Repository, store::inmemory,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountOpened {
    pub owner: String,
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

#[derive(Default, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(AccountOpened, FundsDeposited),
    create(AccountOpened)
)]
pub struct Account {
    balance: i64,
}

impl Create<AccountOpened> for Account {
    fn create(_event: &AccountOpened) -> Self {
        Self { balance: 0 }
    }
}

impl Apply<AccountOpened> for Account {
    fn apply(&mut self, _event: &AccountOpened) {
        self.balance = 0;
    }
}

impl Apply<FundsDeposited> for Account {
    fn apply(&mut self, event: &FundsDeposited) {
        self.balance += event.amount;
    }
}

#[derive(Debug)]
pub struct OpenAccount {
    pub owner: String,
}

impl HandleCreate<OpenAccount> for Account {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &OpenAccount) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            AccountOpened {
                owner: command.owner.clone(),
            }
            .into(),
        ])
    }
}

#[derive(Debug)]
pub struct Deposit {
    pub amount: i64,
}

impl Handle<Deposit> for Account {
    type HandleError = Self::Error;

    fn handle(&self, command: &Deposit) -> Result<Vec<Self::Event>, Self::HandleError> {
        if command.amount <= 0 {
            return Err("deposit amount must be positive".to_string());
        }

        Ok(vec![
            FundsDeposited {
                amount: command.amount,
            }
            .into(),
        ])
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Projection)]
#[projection(events(AccountOpened, FundsDeposited))]
pub struct AccountBalances {
    balances: HashMap<String, i64>,
}

impl ApplyProjection<AccountOpened> for AccountBalances {
    fn apply_projection(
        &mut self,
        account_id: &Self::Id,
        _event: &AccountOpened,
        _metadata: &Self::Metadata,
    ) {
        self.balances.insert(account_id.clone(), 0);
    }
}

impl ApplyProjection<FundsDeposited> for AccountBalances {
    fn apply_projection(
        &mut self,
        account_id: &Self::Id,
        event: &FundsDeposited,
        _metadata: &Self::Metadata,
    ) {
        *self.balances.entry(account_id.clone()).or_default() += event.amount;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = inmemory::Store::<String, ()>::new();
    let repository = Repository::new(store);
    let account_id = "ACC-001".to_string();

    let subscription = repository.subscribe::<AccountBalances>(()).start().await?;

    let token = repository
        .create_tracked::<Account, OpenAccount>(
            &account_id,
            &OpenAccount {
                owner: "Ada".to_string(),
            },
            &(),
        )
        .await?;
    let balances = subscription.read_after(token).await?;
    println!(
        "Opened account; balance is {}",
        balances.balances[&account_id]
    );
    assert_eq!(balances.balances[&account_id], 0);

    let token = repository
        .update_tracked::<Account, Deposit>(&account_id, &Deposit { amount: 100 }, &())
        .await?;
    let balances = subscription.read_after(token).await?;
    println!(
        "Deposited funds; balance is {}",
        balances.balances[&account_id]
    );
    assert_eq!(balances.balances[&account_id], 100);

    subscription.stop().await?;

    Ok(())
}
