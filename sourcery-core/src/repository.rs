//! Application service orchestration.
//!
//! `Repository` coordinates loading aggregates, invoking command handlers, and
//! appending resulting events to the store.
//!
//! Snapshot support is opt-in via `Repository<_, _, Snapshots<_>>`. This keeps
//! the default repository lightweight: no snapshot load/serialize work and no
//! serde bounds on aggregate state unless snapshots are enabled.

use std::marker::PhantomData;

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    aggregate::{Aggregate, Handle},
    event::{EventKind, ProjectionEvent},
    concurrency::{ConcurrencyConflict, ConcurrencyStrategy, Optimistic, Unchecked},
    projection::{Projection, ProjectionBuilder, ProjectionError},
    snapshot::{OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore},
    store::{AppendError, EventFilter, EventStore, StoredEventView},
};

type LoadError<S> = ProjectionError<<S as EventStore>::Error>;

/// Error type for unchecked command execution (no concurrency variant).
#[derive(Debug, Error)]
pub enum CommandError<AggregateError, StoreError>
where
    StoreError: std::error::Error + 'static,
{
    #[error("aggregate rejected command: {0}")]
    Aggregate(AggregateError),
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<StoreError>),
    #[error("failed to persist events: {0}")]
    Store(#[source] StoreError),
}

/// Error type for snapshot-enabled unchecked command execution.
#[derive(Debug, Error)]
pub enum SnapshotCommandError<AggregateError, StoreError, SnapshotError>
where
    StoreError: std::error::Error + 'static,
    SnapshotError: std::error::Error + 'static,
{
    #[error("aggregate rejected command: {0}")]
    Aggregate(AggregateError),
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<StoreError>),
    #[error("failed to persist events: {0}")]
    Store(#[source] StoreError),
    #[error("snapshot operation failed: {0}")]
    Snapshot(#[source] SnapshotError),
}

/// Error type for optimistic command execution (includes concurrency).
#[derive(Debug, Error)]
pub enum OptimisticCommandError<AggregateError, Position, StoreError>
where
    Position: std::fmt::Debug,
    StoreError: std::error::Error + 'static,
{
    #[error("aggregate rejected command: {0}")]
    Aggregate(AggregateError),
    #[error(transparent)]
    Concurrency(ConcurrencyConflict<Position>),
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<StoreError>),
    #[error("failed to persist events: {0}")]
    Store(#[source] StoreError),
}

/// Error type for snapshot-enabled optimistic command execution (includes
/// concurrency).
#[derive(Debug, Error)]
pub enum OptimisticSnapshotCommandError<AggregateError, Position, StoreError, SnapshotError>
where
    Position: std::fmt::Debug,
    StoreError: std::error::Error + 'static,
    SnapshotError: std::error::Error + 'static,
{
    #[error("aggregate rejected command: {0}")]
    Aggregate(AggregateError),
    #[error(transparent)]
    Concurrency(ConcurrencyConflict<Position>),
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<StoreError>),
    #[error("failed to persist events: {0}")]
    Store(#[source] StoreError),
    #[error("snapshot operation failed: {0}")]
    Snapshot(#[source] SnapshotError),
}

/// Result type alias for unchecked command execution.
pub type UncheckedCommandResult<A, S> =
    Result<(), CommandError<<A as Aggregate>::Error, <S as EventStore>::Error>>;

/// Result type alias for snapshot-enabled unchecked command execution.
pub type UncheckedSnapshotCommandResult<A, S, SS> = Result<
    (),
    SnapshotCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

/// Result type alias for optimistic command execution.
pub type OptimisticCommandResult<A, S> = Result<
    (),
    OptimisticCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Position,
        <S as EventStore>::Error,
    >,
>;

/// Result type alias for snapshot-enabled optimistic command execution.
pub type OptimisticSnapshotCommandResult<A, S, SS> = Result<
    (),
    OptimisticSnapshotCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Position,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

/// Result type alias for retry operations (optimistic, no snapshots).
pub type RetryResult<A, S> = Result<
    usize,
    OptimisticCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Position,
        <S as EventStore>::Error,
    >,
>;

/// Result type alias for retry operations (optimistic, snapshots enabled).
pub type SnapshotRetryResult<A, S, SS> = Result<
    usize,
    OptimisticSnapshotCommandError<
        <A as Aggregate>::Error,
        <S as EventStore>::Position,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

struct LoadedAggregate<A, Pos> {
    aggregate: A,
    version: Option<Pos>,
    events_since_snapshot: u64,
}

fn aggregate_event_filters<S, E>(
    aggregate_kind: &str,
    aggregate_id: &S::Id,
    after: Option<&S::Position>,
) -> Vec<EventFilter<S::Id, S::Position>>
where
    S: EventStore,
    E: ProjectionEvent,
{
    E::EVENT_KINDS
        .iter()
        .map(|kind| {
            let mut filter =
                EventFilter::for_aggregate(*kind, aggregate_kind, aggregate_id.clone());
            if let Some(position) = after {
                filter = filter.after(position.clone());
            }
            filter
        })
        .collect()
}

fn apply_stored_events<A, S>(
    aggregate: &mut A,
    store: &S,
    events: &[S::StoredEvent],
) -> Result<Option<S::Position>, crate::event::EventDecodeError<S::Error>>
where
    S: EventStore,
    A: Aggregate<Id = S::Id>,
    A::Event: ProjectionEvent,
{
    let mut last_event_position: Option<S::Position> = None;

    for stored in events {
        let event = A::Event::from_stored(stored, store)?;
        aggregate.apply(&event);
        last_event_position = Some(stored.position());
    }

    Ok(last_event_position)
}

/// Snapshot-enabled repository mode wrapper.
pub struct Snapshots<SS>(pub SS);

impl<Id, SS> SnapshotStore<Id> for Snapshots<SS>
where
    Id: Send + Sync + 'static,
    SS: SnapshotStore<Id>,
{
    type Error = SS::Error;
    type Position = SS::Position;

    async fn load<T>(
        &self,
        kind: &str,
        id: &Id,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        self.0.load(kind, id).await
    }

    async fn offer_snapshot<CE, T, Create>(
        &self,
        kind: &str,
        id: &Id,
        events_since_last_snapshot: u64,
        create_snapshot: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE> + Send,
    {
        self.0
            .offer_snapshot(kind, id, events_since_last_snapshot, create_snapshot)
            .await
    }
}

/// Repository type alias with default concurrency (optimistic) and no snapshots.
///
/// This is the most common repository configuration and is equivalent to:
/// ```ignore
/// Repository<S, Optimistic, NoSnapshots<<S as EventStore>::Position>>
/// ```
///
/// # Example
///
/// ```ignore
/// use sourcery::{Repository, store::inmemory};
///
/// let store = inmemory::Store::new();
/// let repo: DefaultRepository<_> = Repository::new(store);
/// ```
pub type DefaultRepository<S> =
    Repository<S, Optimistic, crate::snapshot::NoSnapshots<<S as EventStore>::Position>>;

/// Repository type alias with unchecked concurrency and no snapshots.
///
/// This configuration skips version checking, allowing last-writer-wins semantics.
/// Use when concurrent writes are impossible or acceptable.
///
/// Equivalent to:
/// ```ignore
/// Repository<S, Unchecked, NoSnapshots<<S as EventStore>::Position>>
/// ```
pub type UncheckedRepository<S> =
    Repository<S, Unchecked, crate::snapshot::NoSnapshots<<S as EventStore>::Position>>;

/// Repository type alias with snapshots enabled and default optimistic concurrency.
///
/// This configuration enables snapshot support for faster aggregate loading.
/// Requires aggregate state to implement `Serialize + DeserializeOwned`.
///
/// Equivalent to:
/// ```ignore
/// Repository<S, Optimistic, SS>
/// ```
///
/// # Example
///
/// ```ignore
/// use sourcery::{Repository, store::inmemory, snapshot::inmemory};
///
/// let store = inmemory::Store::new();
/// let snapshot_store = inmemory::Store::every(100);
/// let repo: SnapshotRepository<_, _> = Repository::new(store)
///     .with_snapshots(snapshot_store);
/// ```
pub type SnapshotRepository<S, SS> = Repository<S, Optimistic, SS>;

/// Repository.
///
/// Orchestrates aggregate loading, command execution, and event persistence.
///
/// # Type Parameters
///
/// - `S`: Event store implementation
/// - `C`: Concurrency strategy (`Optimistic` or `Unchecked`), defaults to `Optimistic`
/// - `M`: Snapshot mode (`NoSnapshots` or `SnapshotStore`), defaults to `NoSnapshots`
///
/// # Type Aliases
///
/// For common configurations, use these type aliases instead of the full generic form:
///
/// - [`DefaultRepository<S>`] - Optimistic concurrency, no snapshots (most common)
/// - [`UncheckedRepository<S>`] - No concurrency checking, no snapshots
/// - [`SnapshotRepository<S, SS>`] - Optimistic concurrency with snapshots
///
/// # Examples
///
/// ```ignore
/// // Using type alias (recommended)
/// let repo: DefaultRepository<_> = Repository::new(store);
///
/// // Or full generic form
/// let repo: Repository<_, Optimistic, NoSnapshots<_>> = Repository::new(store);
/// ```
pub struct Repository<
    S,
    C = Optimistic,
    M = crate::snapshot::NoSnapshots<<S as EventStore>::Position>,
> where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    pub(crate) store: S,
    snapshots: M,
    _concurrency: PhantomData<C>,
}

impl<S> Repository<S>
where
    S: EventStore,
{
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self {
            store,
            snapshots: crate::snapshot::NoSnapshots::new(),
            _concurrency: PhantomData,
        }
    }
}

impl<S, M> Repository<S, Optimistic, M>
where
    S: EventStore,
{
    /// Disable optimistic concurrency checking for this repository.
    #[must_use]
    pub fn without_concurrency_checking(self) -> Repository<S, Unchecked, M> {
        Repository {
            store: self.store,
            snapshots: self.snapshots,
            _concurrency: PhantomData,
        }
    }
}

impl<S, C, M> Repository<S, C, M>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    #[must_use]
    pub const fn event_store(&self) -> &S {
        &self.store
    }

    pub fn build_projection<P>(&self) -> ProjectionBuilder<'_, S, P, M>
    where
        P: Projection<Id = S::Id>,
        P::InstanceId: Sync,
        M: SnapshotStore<P::InstanceId, Position = S::Position>,
    {
        ProjectionBuilder::new(&self.store, &self.snapshots)
    }

    #[must_use]
    pub fn with_snapshots<SS>(self, snapshots: SS) -> Repository<S, C, Snapshots<SS>>
    where
        SS: SnapshotStore<S::Id, Position = S::Position>,
    {
        Repository {
            store: self.store,
            snapshots: Snapshots(snapshots),
            _concurrency: PhantomData,
        }
    }
}

impl<S, C> Repository<S, C, crate::snapshot::NoSnapshots<S::Position>>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// Load an aggregate by replaying all events (no snapshots).
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] if the store fails to load events or if an
    /// event cannot be decoded into the aggregate's event sum type.
    pub async fn load<A>(&self, id: &S::Id) -> Result<A, LoadError<S>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
    {
        Ok(self.load_aggregate::<A>(id).await?.aggregate)
    }

    async fn load_aggregate<A>(
        &self,
        id: &S::Id,
    ) -> Result<LoadedAggregate<A, S::Position>, LoadError<S>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
    {
        let filters = aggregate_event_filters::<S, A::Event>(A::KIND, id, None);

        let events = self
            .store
            .load_events(&filters)
            .await
            .map_err(ProjectionError::Store)?;

        let mut aggregate = A::default();
        let version = apply_stored_events::<A, S>(&mut aggregate, &self.store, &events)
            .map_err(ProjectionError::EventDecode)?;

        Ok(LoadedAggregate {
            aggregate,
            version,
            events_since_snapshot: events.len() as u64,
        })
    }
}

impl<S> Repository<S, Unchecked, crate::snapshot::NoSnapshots<S::Position>>
where
    S: EventStore,
{
    /// Execute a command with last-writer-wins semantics (no concurrency
    /// checking).
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] when the aggregate rejects the command, events
    /// cannot be encoded, the store fails to persist, or the aggregate
    /// cannot be rebuilt.
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> UncheckedCommandResult<A, S>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        let new_events = {
            let LoadedAggregate { aggregate, .. } = self
                .load_aggregate::<A>(id)
                .await
                .map_err(CommandError::Projection)?;
            Handle::<Cmd>::handle(&aggregate, command).map_err(CommandError::Aggregate)?
        };

        if new_events.is_empty() {
            return Ok(());
        }

        let mut tx = self.store.begin::<Unchecked>(A::KIND, id.clone(), None);
        for event in &new_events {
            tx.append(event, metadata.clone())
                .map_err(CommandError::Store)?;
        }
        drop(new_events);
        tx.commit().await.map_err(|e| match e {
            AppendError::Store(err) => CommandError::Store(err),
            AppendError::Conflict(_) => unreachable!("conflict impossible without version"),
            AppendError::EmptyAppend => unreachable!("empty append filtered above"),
        })?;
        Ok(())
    }
}

impl<S> Repository<S, Optimistic, crate::snapshot::NoSnapshots<S::Position>>
where
    S: EventStore,
{
    /// Execute a command using optimistic concurrency control.
    ///
    /// # Errors
    ///
    /// Returns [`OptimisticCommandError::Concurrency`] if the stream version
    /// changed between loading and committing. Other variants cover
    /// aggregate validation, encoding, persistence, and projection rebuild
    /// errors.
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> OptimisticCommandResult<A, S>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        let (version, new_events) = {
            let LoadedAggregate {
                aggregate, version, ..
            } = self
                .load_aggregate::<A>(id)
                .await
                .map_err(OptimisticCommandError::Projection)?;
            let new_events = Handle::<Cmd>::handle(&aggregate, command)
                .map_err(OptimisticCommandError::Aggregate)?;
            (version, new_events)
        };

        if new_events.is_empty() {
            return Ok(());
        }

        let mut tx = self.store.begin::<Optimistic>(A::KIND, id.clone(), version);
        for event in &new_events {
            tx.append(event, metadata.clone())
                .map_err(OptimisticCommandError::Store)?;
        }
        drop(new_events);

        match tx.commit().await {
            Ok(_) => {}
            Err(AppendError::Conflict(c)) => {
                return Err(OptimisticCommandError::Concurrency(c));
            }
            Err(AppendError::Store(s)) => return Err(OptimisticCommandError::Store(s)),
            Err(AppendError::EmptyAppend) => unreachable!("empty append filtered above"),
        }

        Ok(())
    }

    /// Execute a command with automatic retry on concurrency conflicts.
    ///
    /// # Errors
    ///
    /// Returns the last error if all retries are exhausted, or a
    /// non-concurrency error immediately.
    pub async fn execute_with_retry<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
        max_retries: usize,
    ) -> RetryResult<A, S>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd>,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        for attempt in 1..=max_retries {
            match self.execute_command::<A, Cmd>(id, command, metadata).await {
                Ok(()) => return Ok(attempt),
                Err(OptimisticCommandError::Concurrency(_)) => {}
                Err(e) => return Err(e),
            }
        }

        self.execute_command::<A, Cmd>(id, command, metadata)
            .await
            .map(|()| max_retries + 1)
    }
}

impl<S, SS, C> Repository<S, C, Snapshots<SS>>
where
    S: EventStore,
    SS: SnapshotStore<S::Id, Position = S::Position>,
    C: ConcurrencyStrategy,
{
    #[must_use]
    pub const fn snapshot_store(&self) -> &SS {
        &self.snapshots.0
    }

    /// Load an aggregate using snapshots when available.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] if the store fails to load events, if an
    /// event cannot be decoded, or if a stored snapshot cannot be
    /// deserialized (which indicates snapshot corruption).
    pub async fn load<A>(&self, id: &S::Id) -> Result<A, LoadError<S>>
    where
        A: Aggregate<Id = S::Id> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent,
    {
        Ok(self.load_aggregate::<A>(id).await?.aggregate)
    }

    async fn load_aggregate<A>(
        &self,
        id: &S::Id,
    ) -> Result<LoadedAggregate<A, S::Position>, LoadError<S>>
    where
        A: Aggregate<Id = S::Id> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent,
    {
        let snapshot_result = self
            .snapshots
            .0
            .load::<A>(A::KIND, id)
            .await
            .inspect_err(|e| {
                tracing::error!(
                    error = %e,
                    "failed to load snapshot, falling back to full replay"
                );
            })
            .ok()
            .flatten();

        let (mut aggregate, snapshot_position) = if let Some(snapshot) = snapshot_result {
            (snapshot.data, Some(snapshot.position))
        } else {
            (A::default(), None)
        };

        let filters =
            aggregate_event_filters::<S, A::Event>(A::KIND, id, snapshot_position.as_ref());

        let events = self
            .store
            .load_events(&filters)
            .await
            .map_err(ProjectionError::Store)?;

        let last_event_position = apply_stored_events::<A, S>(&mut aggregate, &self.store, &events)
            .map_err(ProjectionError::EventDecode)?;

        let version = last_event_position.or(snapshot_position);

        Ok(LoadedAggregate {
            aggregate,
            version,
            events_since_snapshot: events.len() as u64,
        })
    }
}

impl<S, SS> Repository<S, Unchecked, Snapshots<SS>>
where
    S: EventStore,
    SS: SnapshotStore<S::Id, Position = S::Position>,
{
    /// Execute a command with last-writer-wins semantics and optional
    /// snapshotting.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotCommandError`] when the aggregate rejects the command,
    /// events cannot be encoded, the store fails to persist, snapshot
    /// persistence fails, or the aggregate cannot be rebuilt.
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> UncheckedSnapshotCommandResult<A, S, SS>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        let LoadedAggregate {
            aggregate,
            events_since_snapshot,
            ..
        } = self
            .load_aggregate::<A>(id)
            .await
            .map_err(SnapshotCommandError::Projection)?;

        let new_events =
            Handle::<Cmd>::handle(&aggregate, command).map_err(SnapshotCommandError::Aggregate)?;

        if new_events.is_empty() {
            return Ok(());
        }

        let total_events_since_snapshot = events_since_snapshot + new_events.len() as u64;

        let mut aggregate = aggregate;
        for event in &new_events {
            aggregate.apply(event);
        }

        let mut tx = self.store.begin::<Unchecked>(A::KIND, id.clone(), None);
        for event in &new_events {
            tx.append(event, metadata.clone())
                .map_err(SnapshotCommandError::Store)?;
        }
        drop(new_events);
        let append_result = tx.commit().await.map_err(|e| match e {
            AppendError::Store(err) => SnapshotCommandError::Store(err),
            AppendError::Conflict(_) => unreachable!("conflict impossible without version"),
            AppendError::EmptyAppend => unreachable!("empty append filtered above"),
        })?;
        let new_position = append_result.last_position;

        let offer_result = self.snapshots.0.offer_snapshot(
            A::KIND,
            id,
            total_events_since_snapshot,
            move || -> Result<Snapshot<S::Position, A>, std::convert::Infallible> {
                Ok(Snapshot {
                    position: new_position,
                    data: aggregate,
                })
            },
        );

        match offer_result.await {
            Ok(SnapshotOffer::Declined | SnapshotOffer::Stored) => {}
            Err(OfferSnapshotError::Create(_e)) => {
                // Snapshot serialization failed - log and continue
                // This is not fatal, we successfully committed events
            }
            Err(OfferSnapshotError::Snapshot(e)) => return Err(SnapshotCommandError::Snapshot(e)),
        }

        Ok(())
    }
}

impl<S, SS> Repository<S, Optimistic, Snapshots<SS>>
where
    S: EventStore,
    SS: SnapshotStore<S::Id, Position = S::Position>,
{
    /// Execute a command using optimistic concurrency control and optional
    /// snapshotting.
    ///
    /// # Errors
    ///
    /// Returns [`OptimisticSnapshotCommandError::Concurrency`] if the stream
    /// version changed between loading and committing. Other variants cover
    /// aggregate validation, encoding, persistence, snapshot persistence,
    /// and projection rebuild errors.
    pub async fn execute_command<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> OptimisticSnapshotCommandResult<A, S, SS>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        let LoadedAggregate {
            aggregate,
            version,
            events_since_snapshot,
        } = self
            .load_aggregate::<A>(id)
            .await
            .map_err(OptimisticSnapshotCommandError::Projection)?;

        let new_events = Handle::<Cmd>::handle(&aggregate, command)
            .map_err(OptimisticSnapshotCommandError::Aggregate)?;

        if new_events.is_empty() {
            return Ok(());
        }

        let total_events_since_snapshot = events_since_snapshot + new_events.len() as u64;

        let mut aggregate = aggregate;
        for event in &new_events {
            aggregate.apply(event);
        }

        let mut tx = self.store.begin::<Optimistic>(A::KIND, id.clone(), version);
        for event in &new_events {
            tx.append(event, metadata.clone())
                .map_err(OptimisticSnapshotCommandError::Store)?;
        }
        drop(new_events);

        let append_result = match tx.commit().await {
            Ok(result) => result,
            Err(AppendError::Conflict(c)) => {
                return Err(OptimisticSnapshotCommandError::Concurrency(c));
            }
            Err(AppendError::Store(s)) => return Err(OptimisticSnapshotCommandError::Store(s)),
            Err(AppendError::EmptyAppend) => unreachable!("empty append filtered above"),
        };
        let new_position = append_result.last_position;

        let offer_result = self.snapshots.0.offer_snapshot(
            A::KIND,
            id,
            total_events_since_snapshot,
            move || -> Result<Snapshot<S::Position, A>, std::convert::Infallible> {
                Ok(Snapshot {
                    position: new_position,
                    data: aggregate,
                })
            },
        );

        match offer_result.await {
            Ok(SnapshotOffer::Declined | SnapshotOffer::Stored) => {}
            Err(OfferSnapshotError::Create(_e)) => {
                // Snapshot serialization failed - log and continue
                // This is not fatal, we successfully committed events
            }
            Err(OfferSnapshotError::Snapshot(e)) => {
                return Err(OptimisticSnapshotCommandError::Snapshot(e));
            }
        }

        Ok(())
    }

    /// Execute a command with automatic retry on concurrency conflicts.
    ///
    /// # Errors
    ///
    /// Returns the last error if all retries are exhausted, or a
    /// non-concurrency error immediately.
    pub async fn execute_with_retry<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
        max_retries: usize,
    ) -> SnapshotRetryResult<A, S, SS>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Serialize + DeserializeOwned + Send,
        A::Event: ProjectionEvent + EventKind + serde::Serialize,
        Cmd: Sync,
        S::Metadata: Clone,
    {
        for attempt in 1..=max_retries {
            match self.execute_command::<A, Cmd>(id, command, metadata).await {
                Ok(()) => return Ok(attempt),
                Err(OptimisticSnapshotCommandError::Concurrency(_)) => {}
                Err(e) => return Err(e),
            }
        }

        self.execute_command::<A, Cmd>(id, command, metadata)
            .await
            .map(|()| max_retries + 1)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::*;

    #[test]
    fn command_error_display_mentions_aggregate() {
        let error: CommandError<String, io::Error> =
            CommandError::Aggregate("invalid state".to_string());
        let msg = error.to_string();
        assert!(msg.contains("aggregate rejected command"));
        assert!(error.source().is_none());
    }

    #[test]
    fn command_error_store_has_source() {
        let error: CommandError<String, io::Error> =
            CommandError::Store(io::Error::other("store error"));
        assert!(error.source().is_some());
    }

    #[test]
    fn optimistic_command_error_concurrency_mentions_conflict() {
        let conflict = ConcurrencyConflict {
            expected: Some(1u64),
            actual: Some(2u64),
        };
        let error: OptimisticCommandError<String, u64, io::Error> =
            OptimisticCommandError::Concurrency(conflict);
        let msg = error.to_string();
        assert!(msg.contains("concurrency conflict"));
        assert!(error.source().is_none());
    }
}
