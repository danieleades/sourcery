/// Error type for `PostgreSQL` event store operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Query execution or transaction failure.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Invalid position value supplied by a caller.
    #[error("invalid position value from database: {0}")]
    InvalidPosition(i64),
    /// Insert operation returned no positions for written events.
    #[error("database did not return an inserted position")]
    MissingReturnedPosition,
    /// Event serialisation failed before writing.
    #[error("serialization error: {0}")]
    Serialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Event deserialisation failed while loading/replaying.
    #[error("deserialization error: {0}")]
    Deserialization(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}
