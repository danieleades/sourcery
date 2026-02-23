extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Filters, Projection};

use sourcery_macros::Projection;

#[derive(Default, Projection)]
pub struct AccountLedger {}

fn main() {}
