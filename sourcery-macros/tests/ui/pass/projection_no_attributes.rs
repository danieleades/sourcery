extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Filters, Projection, ProjectionFilters};

use sourcery_macros::Projection;

#[derive(Default, Projection)]
pub struct AccountLedger {}

impl ProjectionFilters for AccountLedger {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init(_instance_id: &Self::InstanceId) -> Self {
        Self::default()
    }

    fn filters<S>(_instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: store::EventStore<Id = String, Metadata = Self::Metadata>,
    {
        Filters::new()
    }
}

fn main() {}
