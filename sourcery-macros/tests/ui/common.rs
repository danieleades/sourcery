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
            stored: &super::store::StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
            store: &S,
        ) -> Result<Self, EventDecodeError<S::Error>>;
    }
}

pub mod store {
    /// Stored event with position and metadata.
    #[derive(Clone, Debug)]
    pub struct StoredEvent<Id, Pos, Data, M> {
        pub aggregate_kind: String,
        pub aggregate_id: Id,
        pub kind: String,
        pub position: Pos,
        pub data: Data,
        pub metadata: M,
    }

    impl<Id, Pos, Data, M> StoredEvent<Id, Pos, Data, M> {
        pub fn aggregate_kind(&self) -> &str {
            &self.aggregate_kind
        }

        pub fn aggregate_id(&self) -> &Id {
            &self.aggregate_id
        }

        pub fn kind(&self) -> &str {
            &self.kind
        }

        pub fn metadata(&self) -> &M {
            &self.metadata
        }
    }

    impl<Id, Pos: Clone, Data, M> StoredEvent<Id, Pos, Data, M> {
        pub fn position(&self) -> Pos {
            self.position.clone()
        }
    }

    /// Staged event awaiting persistence.
    #[derive(Clone, Debug)]
    pub struct StagedEvent<Data, M> {
        pub kind: String,
        pub data: Data,
        pub metadata: M,
    }

    pub trait EventStore: Send + Sync {
        type Error;
        type Id;
        type Metadata;
        type Position;
        type Data: Clone + Send + Sync + 'static;

        fn stage_event<E>(
            &self,
            event: &E,
            metadata: Self::Metadata,
        ) -> Result<StagedEvent<Self::Data, Self::Metadata>, Self::Error>
        where
            E: super::event::EventKind + serde::Serialize;

        fn decode_event<E>(
            &self,
            stored: &StoredEvent<Self::Id, Self::Position, Self::Data, Self::Metadata>,
        ) -> Result<E, Self::Error>
        where
            E: super::event::DomainEvent + serde::de::DeserializeOwned;
    }
}

pub mod codec {
    // Re-export event module items for backward compatibility with UI tests
    pub use super::event::{EventDecodeError, ProjectionEvent};
}

pub trait Subscribable: Sized {
    type Id;
    type InstanceId;
    type Metadata;

    fn init(instance_id: &Self::InstanceId) -> Self;

    fn filters<S>(instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: store::EventStore<Id = Self::Id>,
        S::Metadata: Clone + Into<Self::Metadata>;
}

pub trait Projection: Subscribable {
    const KIND: &'static str;
}

/// Stub Filters type for trybuild tests.
pub struct Filters<S, P> {
    _s: std::marker::PhantomData<S>,
    _p: std::marker::PhantomData<P>,
}

impl<S, P> Filters<S, P>
where
    S: store::EventStore,
    P: Subscribable<Id = S::Id>,
{
    pub fn new() -> Self {
        Self {
            _s: std::marker::PhantomData,
            _p: std::marker::PhantomData,
        }
    }
}
