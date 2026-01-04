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

pub mod codec {
    use crate::store::PersistableEvent;

    pub trait Codec {
        type Error;
        fn serialize<T: ?Sized>(&self, value: &T) -> Result<Vec<u8>, Self::Error>;
        fn deserialize<T>(&self, data: &[u8]) -> Result<T, Self::Error>;
    }

    #[derive(Debug)]
    pub enum EventDecodeError<E> {
        Codec(E),
        UnknownKind {
            kind: String,
            expected: &'static [&'static str],
        },
    }

    pub trait ProjectionEvent: Sized {
        const EVENT_KINDS: &'static [&'static str];

        fn from_stored<C: Codec>(
            kind: &str,
            data: &[u8],
            codec: &C,
        ) -> Result<Self, EventDecodeError<C::Error>>;
    }

    pub trait SerializableEvent {
        fn to_persistable<C: Codec, M>(
            self,
            codec: &C,
            metadata: M,
        ) -> Result<PersistableEvent<M>, C::Error>;
    }
}

pub mod store {
    #[derive(Debug)]
    pub struct PersistableEvent<M> {
        pub kind: String,
        pub data: Vec<u8>,
        pub metadata: M,
    }
}

pub trait Projection {
    const KIND: &'static str;
    type Id;
    type Metadata;
    type InstanceId;
}
