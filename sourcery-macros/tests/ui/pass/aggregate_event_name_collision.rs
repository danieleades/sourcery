extern crate self as sourcery;
extern crate serde_json;

#[path = "../common.rs"]
mod support;

pub use support::{codec, event, store, Aggregate, Apply, Create, ProjectionEvent, Projection};

use event::DomainEvent;
use serde::{Deserialize, Serialize};
use sourcery_macros::Aggregate;

mod foo {
    use super::{DomainEvent, Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Event;

    impl DomainEvent for Event {
        const KIND: &'static str = "foo-event";
    }
}

mod bar {
    use super::{DomainEvent, Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Event;

    impl DomainEvent for Event {
        const KIND: &'static str = "bar-event";
    }
}

pub struct AggregateError;

#[derive(Default, Aggregate)]
#[aggregate(id = String, error = AggregateError, events(foo::Event, bar::Event), create(foo::Event))]
pub struct Account;

impl Apply<foo::Event> for Account {
    fn apply(&mut self, _event: &foo::Event) {}
}

impl Apply<bar::Event> for Account {
    fn apply(&mut self, _event: &bar::Event) {}
}

impl Create<foo::Event> for Account {
    fn create(_event: &foo::Event) -> Self {
        Self
    }
}

fn assert_variant_names() {
    let _ = AccountEvent::FooEvent(foo::Event);
    let _ = AccountEvent::BarEvent(bar::Event);
}

fn main() {}
