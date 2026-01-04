extern crate self as sourcery;

#[path = "../support.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use serde::{Deserialize, Serialize};
use sourcery_macros::Aggregate;

#[derive(Clone, Serialize, Deserialize)]
pub struct FundsDeposited {
    pub amount: i64,
}

impl FundsDeposited {
    pub const KIND: &'static str = "funds-deposited";
}

pub struct AggregateError;

#[derive(Aggregate)]
#[aggregate(id = String, error = AggregateError, events(FundsDeposited))]
pub enum Account {
    Active,
    Closed,
}

fn main() {}
