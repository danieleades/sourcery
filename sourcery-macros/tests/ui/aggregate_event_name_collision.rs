extern crate self as sourcery;

#[path = "support.rs"]
mod support;

pub use support::{codec, store, Aggregate, Apply, Projection};

use serde::{Deserialize, Serialize};
use sourcery_macros::Aggregate;

mod foo {
    use super::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize)]
    pub struct Event;

    impl Event {
        pub const KIND: &'static str = "foo-event";
    }
}

mod bar {
    use super::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize)]
    pub struct Event;

    impl Event {
        pub const KIND: &'static str = "bar-event";
    }
}

pub struct AggregateError;

#[derive(Aggregate)]
#[aggregate(id = String, error = AggregateError, events(foo::Event, bar::Event))]
pub struct Account;

impl Apply<foo::Event> for Account {
    fn apply(&mut self, _event: &foo::Event) {}
}

impl Apply<bar::Event> for Account {
    fn apply(&mut self, _event: &bar::Event) {}
}

fn assert_variant_names() {
    let _ = AccountEvent::FooEvent(foo::Event);
    let _ = AccountEvent::BarEvent(bar::Event);
}

fn main() {}
