extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Filters, Projection};

use sourcery_macros::Projection;

#[derive(Projection)]
#[projection(kind = AccountLedger)]
pub struct AccountLedger {}

fn main() {}
