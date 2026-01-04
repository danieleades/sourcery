extern crate self as sourcery;

#[path = "../support.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use sourcery_macros::Aggregate;

pub struct AggregateError;

#[derive(Aggregate)]
#[aggregate(id = String, error = AggregateError)]
pub struct Account {}

fn main() {}
