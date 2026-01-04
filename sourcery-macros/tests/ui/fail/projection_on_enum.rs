extern crate self as sourcery;

#[path = "../support.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use sourcery_macros::Projection;

#[derive(Projection)]
#[projection(id = String)]
pub enum AccountLedger {
    Daily,
    Monthly,
}

fn main() {}
