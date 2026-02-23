extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{
    ApplyProjection, DomainEvent, Filters, Projection, codec, store,
};

use sourcery_macros::Projection;

#[derive(Clone, serde::Deserialize)]
pub struct FundsDeposited;

impl DomainEvent for FundsDeposited {
    const KIND: &'static str = "funds-deposited";
}

#[derive(Default, Projection)]
#[projection(
    id = String,
    instance_id = (),
    metadata = (),
    events(FundsDeposited)
)]
pub struct AccountLedger;

impl ApplyProjection<FundsDeposited> for AccountLedger {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        _event: &FundsDeposited,
        _metadata: &Self::Metadata,
    ) {
    }
}

fn main() {}
