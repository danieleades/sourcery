pub trait Apply<E> {
    fn apply(&mut self, event: &E);
}

pub trait Aggregate {
    const KIND: &'static str;
    type Event;
    type Error;
    type Id;

    fn apply(&mut self, event: &Self::Event);
}

// Re-export at root for macro-generated code
#[allow(unused_imports)]
pub use event::{DomainEvent, EventDecodeError, EventKind, ProjectionEvent};

pub mod event {
    pub trait EventKind {
        fn kind(&self) -> &'static str;
    }

    pub trait DomainEvent {
        const KIND: &'static str;
    }

    impl<T: DomainEvent> EventKind for T {
        fn kind(&self) -> &'static str {
            T::KIND
        }
    }

    #[derive(Debug)]
    pub enum EventDecodeError<E> {
        Store(E),
        UnknownKind {
            kind: String,
            expected: &'static [&'static str],
        },
    }

    pub trait ProjectionEvent: Sized {
        const EVENT_KINDS: &'static [&'static str];

        fn from_stored<S: super::store::EventStore>(
            stored: &S::StoredEvent,
            store: &S,
        ) -> Result<Self, EventDecodeError<S::Error>>;
    }
}

pub mod store {
    pub trait StoredEventView {
        type Id;
        type Pos;
        type Metadata;
        fn aggregate_kind(&self) -> &str;
        fn aggregate_id(&self) -> &Self::Id;
        fn kind(&self) -> &str;
        fn position(&self) -> Self::Pos;
        fn metadata(&self) -> &Self::Metadata;
    }

    pub trait EventStore: Send + Sync {
        type Error;
        type Id;
        type Metadata;
        type Position;
        type StoredEvent: StoredEventView<Id = Self::Id, Pos = Self::Position, Metadata = Self::Metadata>;
        type StagedEvent: Send + 'static;

        fn stage_event<E>(&self, event: &E, metadata: Self::Metadata) -> Result<Self::StagedEvent, Self::Error>
        where
            E: super::event::EventKind + serde::Serialize;

        fn decode_event<E>(&self, stored: &Self::StoredEvent) -> Result<E, Self::Error>
        where
            E: super::event::DomainEvent + serde::de::DeserializeOwned;
    }
}

pub mod codec {
    // Re-export event module items for backward compatibility with UI tests
    pub use super::event::{EventDecodeError, ProjectionEvent};
}

pub trait Projection {
    const KIND: &'static str;
    type Id;
    type Metadata;
    type InstanceId;
}
