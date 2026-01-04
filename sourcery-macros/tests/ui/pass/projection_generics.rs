extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use sourcery_macros::Projection;

#[derive(Projection)]
#[projection(id = String)]
pub struct AccountLedger<'a, T> {
    marker: std::marker::PhantomData<&'a T>,
}

fn main() {}
