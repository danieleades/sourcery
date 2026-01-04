extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use sourcery_macros::Projection;

#[derive(Projection)]
pub struct AccountLedger {}

fn main() {}
