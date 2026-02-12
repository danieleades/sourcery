extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Filters, Projection, ProjectionFilters};

use sourcery_macros::Projection;

#[derive(Projection)]
pub enum AccountLedger {
    Daily,
    Monthly,
}

fn main() {}
