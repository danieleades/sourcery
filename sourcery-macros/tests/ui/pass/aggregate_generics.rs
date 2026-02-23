extern crate self as sourcery;
extern crate serde_json;

#[path = "../common.rs"]
mod support;

pub use support::{codec, event, store, Aggregate, Apply, Create, ProjectionEvent, Projection};

use event::DomainEvent;
use serde::{Deserialize, Serialize};
use sourcery_macros::Aggregate;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FundsDeposited {
    pub amount: i64,
}

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "funds-deposited";
}

pub struct AggregateError;

#[derive(Aggregate)]
#[aggregate(id = String, error = AggregateError, events(FundsDeposited), create(FundsDeposited))]
pub struct Account<'a, T> {
    marker: std::marker::PhantomData<&'a T>,
}

impl<'a, T> Default for Account<'a, T> {
    fn default() -> Self {
        Self {
            marker: std::marker::PhantomData,
        }
    }
}

impl<'a, T> Apply<FundsDeposited> for Account<'a, T> {
    fn apply(&mut self, _event: &FundsDeposited) {}
}

impl<'a, T> Create<FundsDeposited> for Account<'a, T> {
    fn create(_event: &FundsDeposited) -> Self {
        Self::default()
    }
}

fn main() {}
