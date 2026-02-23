extern crate self as sourcery;

#[path = "../common.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Filters, Projection};

pub struct AccountLedger<'a, T: 'static> {
    marker: std::marker::PhantomData<&'a T>,
}

impl<'a, T: 'static> Projection for AccountLedger<'a, T> {
    const KIND: &'static str = "account-ledger";
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init(_instance_id: &Self::InstanceId) -> Self {
        Self {
            marker: std::marker::PhantomData,
        }
    }

    fn filters<S>(_instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: store::EventStore<Id = String, Metadata = Self::Metadata>,
    {
        Filters::new()
    }
}

fn main() {}
