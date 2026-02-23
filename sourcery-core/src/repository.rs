//! Command execution and aggregate lifecycle management.
//!
//! The [`Repository`] orchestrates the core event sourcing workflow:
//!
//! 1. Load aggregate state by replaying events
//! 2. Execute creation/existing commands via
//!    [`HandleCreate<C>`](crate::aggregate::HandleCreate) /
//!    [`Handle<C>`](crate::aggregate::Handle)
//! 3. Persist resulting events transactionally
//! 4. Load projections from event streams
//!
//! # Quick Example
//!
//! ```ignore
//! let repo = Repository::new(store);
//!
//! // Create stream
//! repo.create::<Account, OpenAccount>(&id, &open, &metadata)
//!     .await?;
//!
//! // Execute command on existing stream
//! repo.update::<Account, Deposit>(&id, &deposit, &metadata)
//!     .await?;
//!
//! // Load aggregate state
//! let account: Account = repo.load(&id).await?;
//!
//! // Load a projection
//! let report = repo.load_projection::<Report>(&()).await?;
//! ```
//!
//! See the [quickstart example](https://github.com/danieleades/sourcery/blob/main/examples/quickstart.rs)
//! for a complete working example.

use std::{collections::HashMap, convert::Infallible, marker::PhantomData};

use nonempty::NonEmpty;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    aggregate::{Aggregate, Handle, HandleCreate},
    concurrency::{ConcurrencyConflict, ConcurrencyStrategy, Optimistic, Unchecked},
    event::{EventKind, ProjectionEvent},
    projection::{HandlerError, Projection, ProjectionError},
    snapshot::{OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore},
    store::{
        CommitError, EventFilter, EventStore, GloballyOrderedStore, OptimisticCommitError,
        StoredEvents,
    },
    subscription::{SubscribableStore, SubscriptionBuilder},
};

type LoadError<S> = ProjectionError<<S as EventStore>::Error>;
type EventDecodeError<S> = crate::event::EventDecodeError<<S as EventStore>::Error>;
type RebuiltAggregate<A, S> = Option<(A, <S as EventStore>::Position)>;

/// Lifecycle mismatch when executing a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LifecycleError {
    #[error("aggregate not found")]
    AggregateNotFound,
    #[error("aggregate already exists")]
    AggregateAlreadyExists,
}

/// Error type for command execution across all repository modes.
#[derive(Debug, Error)]
pub enum CommandError<AggregateError, ConcurrencyError, StoreError, SnapshotError>
where
    ConcurrencyError: std::error::Error + 'static,
    StoreError: std::error::Error + 'static,
    SnapshotError: std::error::Error + 'static,
{
    #[error("aggregate rejected command: {0}")]
    Aggregate(AggregateError),
    #[error(transparent)]
    Lifecycle(LifecycleError),
    #[error(transparent)]
    Concurrency(ConcurrencyError),
    #[error("failed to rebuild aggregate state: {0}")]
    Projection(#[source] ProjectionError<StoreError>),
    #[error("failed to persist events: {0}")]
    Store(#[source] StoreError),
    #[error("snapshot operation failed: {0}")]
    Snapshot(#[source] SnapshotError),
}

/// Result type alias for unchecked command execution.
pub type UncheckedCommandResult<A, S> = Result<
    (),
    CommandError<<A as Aggregate>::Error, Infallible, <S as EventStore>::Error, Infallible>,
>;

/// Result type alias for snapshot-enabled unchecked command execution.
pub type UncheckedSnapshotCommandResult<A, S, SS> = Result<
    (),
    CommandError<
        <A as Aggregate>::Error,
        Infallible,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

/// Result type alias for optimistic command execution.
pub type OptimisticCommandResult<A, S> = Result<
    (),
    CommandError<
        <A as Aggregate>::Error,
        ConcurrencyConflict<<S as EventStore>::Position>,
        <S as EventStore>::Error,
        Infallible,
    >,
>;

/// Result type alias for snapshot-enabled optimistic command execution.
pub type OptimisticSnapshotCommandResult<A, S, SS> = Result<
    (),
    CommandError<
        <A as Aggregate>::Error,
        ConcurrencyConflict<<S as EventStore>::Position>,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

/// Result type alias for retry operations (optimistic, no snapshots).
pub type RetryResult<A, S> = Result<
    usize,
    CommandError<
        <A as Aggregate>::Error,
        ConcurrencyConflict<<S as EventStore>::Position>,
        <S as EventStore>::Error,
        Infallible,
    >,
>;

/// Result type alias for retry operations (optimistic, snapshots enabled).
pub type SnapshotRetryResult<A, S, SS> = Result<
    usize,
    CommandError<
        <A as Aggregate>::Error,
        ConcurrencyConflict<<S as EventStore>::Position>,
        <S as EventStore>::Error,
        <SS as SnapshotStore<<S as EventStore>::Id>>::Error,
    >,
>;

#[doc(hidden)]
pub enum CommitPolicyError<C, S> {
    Concurrency(C),
    Store(S),
}

#[doc(hidden)]
pub trait CommitPolicy<S: EventStore>: private::Sealed {
    type ConcurrencyError: std::error::Error + 'static;

    fn commit<E>(
        store: &S,
        kind: &str,
        id: &S::Id,
        expected: Option<S::Position>,
        events: NonEmpty<E>,
        metadata: &S::Metadata,
    ) -> impl std::future::Future<
        Output = Result<S::Position, CommitPolicyError<Self::ConcurrencyError, S::Error>>,
    > + Send
    where
        E: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone;
}

#[doc(hidden)]
pub trait SnapshotPolicy<S: EventStore, A: Aggregate<Id = S::Id>>: private::Sealed {
    type SnapshotError: std::error::Error + 'static;
    type Prepared;

    fn load_base(
        &self,
        kind: &str,
        id: &S::Id,
    ) -> impl std::future::Future<Output = Option<(A, S::Position)>> + Send;

    fn prepare_snapshot_from_existing(
        &self,
        aggregate: A,
        events: &NonEmpty<A::Event>,
    ) -> Self::Prepared;

    fn prepare_snapshot_from_new(&self, events: &NonEmpty<A::Event>) -> Self::Prepared;

    fn offer_snapshot(
        &self,
        kind: &str,
        id: &S::Id,
        events_since_snapshot: u64,
        new_position: S::Position,
        prepared: Self::Prepared,
    ) -> impl std::future::Future<Output = Result<(), Self::SnapshotError>> + Send;
}

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
    events: &StoredEvents<S::Id, S::Position, S::Data, S::Metadata>,
) -> Result<Option<S::Position>, crate::event::EventDecodeError<S::Error>>
where
    S: EventStore,
    S::Position: Clone,
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

/// Build an aggregate from scratch using the first event's `create` constructor
/// then applying subsequent events.
///
/// Returns `None` if the event list is empty (aggregate does not yet exist).
fn create_from_stored_events<A, S>(
    store: &S,
    events: &StoredEvents<S::Id, S::Position, S::Data, S::Metadata>,
) -> Result<RebuiltAggregate<A, S>, EventDecodeError<S>>
where
    A: Aggregate<Id = S::Id>,
    A::Event: ProjectionEvent,
    S: EventStore,
    S::Position: Clone,
{
    let mut aggregate: Option<A> = None;
    let mut last_position: Option<S::Position> = None;

    for stored in events {
        let event = A::Event::from_stored(stored, store)?;
        match aggregate.as_mut() {
            Some(agg) => agg.apply(&event),
            None => aggregate = Some(A::create(&event)),
        }
        last_position = Some(stored.position());
    }

    Ok(aggregate.zip(last_position))
}

fn create_from_new_events<A>(events: &NonEmpty<A::Event>) -> A
where
    A: Aggregate,
{
    let mut iter = events.iter();
    let first = iter
        .next()
        .expect("NonEmpty guarantees at least one event when creating aggregate");
    let mut aggregate = A::create(first);
    for event in iter {
        aggregate.apply(event);
    }
    aggregate
}

fn replay_projection_events<P, S>(
    projection: &mut P,
    events: &StoredEvents<S::Id, S::Position, S::Data, S::Metadata>,
    handlers: &HashMap<&'static str, crate::projection::EventHandler<P, S>>,
    store: &S,
) -> Result<Option<S::Position>, ProjectionError<S::Error>>
where
    S: EventStore,
    S::Position: Clone,
    P: Projection<Id = S::Id>,
{
    let mut last_position = None;

    for stored in events {
        let aggregate_id = stored.aggregate_id();
        let kind = stored.kind();
        let metadata = stored.metadata();

        if let Some(handler) = handlers.get(kind) {
            (handler)(projection, aggregate_id, stored, metadata, store).map_err(|error| {
                match error {
                    HandlerError::Store(error) => {
                        ProjectionError::EventDecode(crate::event::EventDecodeError::Store(error))
                    }
                    HandlerError::EventDecode(error) => ProjectionError::EventDecode(error),
                }
            })?;
        }
        last_position = Some(stored.position());
    }

    Ok(last_position)
}

/// Snapshot-enabled repository mode wrapper.
///
/// This is an implementation detail of [`Repository`]'s type-state pattern for
/// snapshot support. You should never construct this type directly.
///
/// # Usage
///
/// Enable snapshots via [`Repository::with_snapshots()`]:
///
/// ```ignore
/// let repo = Repository::new(store)
///     .with_snapshots(snapshot_store);
/// ```
pub struct Snapshots<SS>(
    /// The underlying snapshot store implementation.
    ///
    /// This field is public for trait implementation purposes only.
    /// Do not access it directly.
    #[doc(hidden)]
    pub SS,
);

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

mod private {
    pub trait Sealed {}

    impl Sealed for crate::concurrency::Unchecked {}
    impl Sealed for crate::concurrency::Optimistic {}
    impl<Pos> Sealed for crate::snapshot::NoSnapshots<Pos> {}
    impl<SS> Sealed for super::Snapshots<SS> {}
}

impl<S> CommitPolicy<S> for Unchecked
where
    S: EventStore,
{
    type ConcurrencyError = Infallible;

    async fn commit<E>(
        store: &S,
        kind: &str,
        id: &S::Id,
        _expected: Option<S::Position>,
        events: NonEmpty<E>,
        metadata: &S::Metadata,
    ) -> Result<S::Position, CommitPolicyError<Self::ConcurrencyError, S::Error>>
    where
        E: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone,
    {
        let committed = store
            .commit_events(kind, id, events, metadata)
            .await
            .map_err(|e| match e {
                CommitError::Store(err) | CommitError::Serialization { source: err, .. } => {
                    CommitPolicyError::Store(err)
                }
            })?;

        Ok(committed.last_position)
    }
}

impl<S> CommitPolicy<S> for Optimistic
where
    S: EventStore,
{
    type ConcurrencyError = ConcurrencyConflict<S::Position>;

    async fn commit<E>(
        store: &S,
        kind: &str,
        id: &S::Id,
        expected: Option<S::Position>,
        events: NonEmpty<E>,
        metadata: &S::Metadata,
    ) -> Result<S::Position, CommitPolicyError<Self::ConcurrencyError, S::Error>>
    where
        E: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone,
    {
        let committed = store
            .commit_events_optimistic(kind, id, expected, events, metadata)
            .await
            .map_err(|e| match e {
                OptimisticCommitError::Conflict(conflict) => {
                    CommitPolicyError::Concurrency(conflict)
                }
                OptimisticCommitError::Store(err)
                | OptimisticCommitError::Serialization { source: err, .. } => {
                    CommitPolicyError::Store(err)
                }
            })?;

        Ok(committed.last_position)
    }
}

impl<S, A> SnapshotPolicy<S, A> for crate::snapshot::NoSnapshots<S::Position>
where
    S: EventStore,
    A: Aggregate<Id = S::Id>,
{
    type Prepared = ();
    type SnapshotError = Infallible;

    async fn load_base(&self, _kind: &str, _id: &S::Id) -> Option<(A, S::Position)> {
        None
    }

    fn prepare_snapshot_from_existing(
        &self,
        _aggregate: A,
        _events: &NonEmpty<A::Event>,
    ) -> Self::Prepared {
    }

    fn prepare_snapshot_from_new(&self, _events: &NonEmpty<A::Event>) -> Self::Prepared {}

    async fn offer_snapshot(
        &self,
        _kind: &str,
        _id: &S::Id,
        _events_since_snapshot: u64,
        _new_position: S::Position,
        _prepared: Self::Prepared,
    ) -> Result<(), Self::SnapshotError> {
        Ok(())
    }
}

impl<S, A, SS> SnapshotPolicy<S, A> for Snapshots<SS>
where
    S: EventStore,
    A: Aggregate<Id = S::Id> + Serialize + DeserializeOwned + Send,
    SS: SnapshotStore<S::Id, Position = S::Position>,
{
    type Prepared = A;
    type SnapshotError = SS::Error;

    async fn load_base(&self, kind: &str, id: &S::Id) -> Option<(A, S::Position)> {
        self.0
            .load::<A>(kind, id)
            .await
            .inspect_err(|e| {
                tracing::error!(
                    error = %e,
                    "failed to load snapshot, falling back to full replay"
                );
            })
            .ok()
            .flatten()
            .map(|snapshot| (snapshot.data, snapshot.position))
    }

    fn prepare_snapshot_from_existing(
        &self,
        mut aggregate: A,
        events: &NonEmpty<A::Event>,
    ) -> Self::Prepared {
        for event in events {
            aggregate.apply(event);
        }
        aggregate
    }

    fn prepare_snapshot_from_new(&self, events: &NonEmpty<A::Event>) -> Self::Prepared {
        create_from_new_events::<A>(events)
    }

    async fn offer_snapshot(
        &self,
        kind: &str,
        id: &S::Id,
        events_since_snapshot: u64,
        new_position: S::Position,
        prepared: Self::Prepared,
    ) -> Result<(), Self::SnapshotError> {
        let offer_result = self.0.offer_snapshot(
            kind,
            id,
            events_since_snapshot,
            move || -> Result<Snapshot<S::Position, A>, Infallible> {
                Ok(Snapshot {
                    position: new_position,
                    data: prepared,
                })
            },
        );

        match offer_result.await {
            Ok(SnapshotOffer::Declined | SnapshotOffer::Stored)
            | Err(OfferSnapshotError::Create(_)) => Ok(()),
            Err(OfferSnapshotError::Snapshot(e)) => Err(e),
        }
    }
}

/// Repository type alias with optimistic concurrency and no snapshots.
///
/// This configuration provides version-checked writes without snapshot support.
/// It is equivalent to:
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
/// let repo: OptimisticRepository<_> = Repository::new(store);
/// ```
pub type OptimisticRepository<S> =
    Repository<S, Optimistic, crate::snapshot::NoSnapshots<<S as EventStore>::Position>>;

/// Repository type alias with unchecked concurrency and no snapshots.
///
/// This configuration skips version checking, allowing last-writer-wins
/// semantics. Use when concurrent writes are impossible or acceptable.
///
/// Equivalent to:
/// ```ignore
/// Repository<S, Unchecked, NoSnapshots<<S as EventStore>::Position>>
/// ```
pub type UncheckedRepository<S> =
    Repository<S, Unchecked, crate::snapshot::NoSnapshots<<S as EventStore>::Position>>;

/// Repository type alias with optimistic concurrency and snapshot support.
///
/// This configuration enables snapshot support for faster aggregate loading
/// with version-checked writes. Requires aggregate state to implement
/// `Serialize + DeserializeOwned`.
///
/// Equivalent to:
/// ```ignore
/// Repository<S, Optimistic, Snapshots<SS>>
/// ```
///
/// # Example
///
/// ```ignore
/// use sourcery::{Repository, store::inmemory, snapshot::inmemory};
///
/// let store = inmemory::Store::new();
/// let snapshot_store = inmemory::Store::every(100);
/// let repo: OptimisticSnapshotRepository<_, _> = Repository::new(store)
///     .with_snapshots(snapshot_store);
/// ```
pub type OptimisticSnapshotRepository<S, SS> = Repository<S, Optimistic, Snapshots<SS>>;

/// Command execution and aggregate lifecycle orchestrator.
///
/// Repository manages the complete event sourcing workflow: loading aggregates
/// by replaying events, executing commands through handlers, and persisting
/// resulting events transactionally.
///
/// # Usage
///
/// ```ignore
/// // Create repository
/// let repo = Repository::new(store);
///
/// // Create stream
/// repo.create::<Account, OpenAccount>(&id, &open, &metadata)
///     .await?;
///
/// // Execute command on existing stream
/// repo.update::<Account, Deposit>(&id, &deposit, &metadata)
///     .await?;
///
/// // Load aggregate state
/// let account: Account = repo.load(&id).await?;
///
/// // Load projections
/// let report = repo.load_projection::<InventoryReport>(&()).await?;
///
/// // Enable snapshots for faster loading
/// let repo_with_snaps = repo.with_snapshots(snapshot_store);
/// ```
///
/// # Type Aliases
///
/// Use these type aliases for common configurations:
///
/// - [`OptimisticRepository<S>`] - Version-checked writes, no snapshots
/// - [`UncheckedRepository<S>`] - Last-writer-wins, no snapshots
/// - [`OptimisticSnapshotRepository<S, SS>`] - Version-checked writes with
///   snapshots
///
/// # Concurrency Strategies
///
/// - **Optimistic** (default): Detects conflicts via version checking. Use
///   [`update_with_retry()`](Self::update_with_retry) to automatically retry on
///   conflicts.
/// - **Unchecked**: Last-writer-wins semantics. Use only when concurrent writes
///   are impossible or acceptable.
///
/// # See Also
///
/// - [quickstart example](https://github.com/danieleades/sourcery/blob/main/examples/quickstart.rs)
///   - Complete workflow
/// - [`create()`](Self::create) - Creation command execution
/// - [`update()`](Self::update) - Existing-stream command execution
/// - [`upsert()`](Self::upsert) - Create-or-update command execution
/// - [`load()`](Self::load) - Aggregate loading
/// - [`load_projection()`](Self::load_projection) - Projection loading
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

    /// Load a projection by replaying events (one-shot query, no snapshots).
    ///
    /// Filter configuration is defined centrally in the projection's
    /// [`Projection`] implementation. The `instance_id` parameterises
    /// which events to load.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialised.
    #[tracing::instrument(
        skip(self, instance_id),
        fields(
            projection_type = std::any::type_name::<P>(),
        )
    )]
    pub async fn load_projection<P>(
        &self,
        instance_id: &P::InstanceId,
    ) -> Result<P, ProjectionError<S::Error>>
    where
        P: Projection<Id = S::Id, Metadata = S::Metadata>,
        P::InstanceId: Send + Sync,
        M: Sync,
    {
        tracing::debug!("loading projection");

        let filters = P::filters::<S>(instance_id);
        let (event_filters, handlers) = filters.into_event_filters(None);

        let events = self
            .store
            .load_events(&event_filters)
            .await
            .map_err(ProjectionError::Store)?;

        let mut projection = P::init(instance_id);
        let event_count = events.len();
        tracing::debug!(
            events_to_replay = event_count,
            "replaying events into projection"
        );

        let _ = replay_projection_events(&mut projection, &events, &handlers, &self.store)?;

        tracing::info!(events_applied = event_count, "projection loaded");
        Ok(projection)
    }

    /// Load an aggregate, using snapshots when configured.
    ///
    /// Returns `None` if no events exist for the given ID (the aggregate has
    /// not yet been created).
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] if the store fails to load events or if an
    /// event cannot be decoded into the aggregate's event sum type.
    pub async fn load<A>(&self, id: &S::Id) -> Result<Option<A>, LoadError<S>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
        M: SnapshotPolicy<S, A> + Sync,
    {
        Ok(self.load_aggregate::<A>(id).await?.map(|la| la.aggregate))
    }

    async fn load_aggregate<A>(
        &self,
        id: &S::Id,
    ) -> Result<Option<LoadedAggregate<A, S::Position>>, LoadError<S>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: ProjectionEvent,
        M: SnapshotPolicy<S, A> + Sync,
    {
        let snapshot = self.snapshots.load_base(A::KIND, id).await;

        let filters = aggregate_event_filters::<S, A::Event>(
            A::KIND,
            id,
            snapshot.as_ref().map(|(_, pos)| pos),
        );

        let events = self
            .store
            .load_events(&filters)
            .await
            .map_err(ProjectionError::Store)?;

        if let Some((mut agg, snap_pos)) = snapshot {
            // Resume from snapshot; apply subsequent events.
            let last_event_pos = apply_stored_events::<A, S>(&mut agg, &self.store, &events)
                .map_err(ProjectionError::EventDecode)?;
            let version = last_event_pos.or(Some(snap_pos));
            return Ok(Some(LoadedAggregate {
                aggregate: agg,
                version,
                events_since_snapshot: events.len() as u64,
            }));
        }

        // No snapshot — build from events using create().
        match create_from_stored_events::<A, S>(&self.store, &events)
            .map_err(ProjectionError::EventDecode)?
        {
            None => Ok(None),
            Some((agg, pos)) => Ok(Some(LoadedAggregate {
                aggregate: agg,
                version: Some(pos),
                events_since_snapshot: events.len() as u64,
            })),
        }
    }

    async fn persist_events<A>(
        &self,
        id: &S::Id,
        expected_version: Option<S::Position>,
        events_since_snapshot: u64,
        events: NonEmpty<A::Event>,
        prepared_snapshot: M::Prepared,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
    {
        let new_position =
            C::commit::<A::Event>(&self.store, A::KIND, id, expected_version, events, metadata)
                .await
                .map_err(|error| match error {
                    CommitPolicyError::Concurrency(conflict) => CommandError::Concurrency(conflict),
                    CommitPolicyError::Store(store_error) => CommandError::Store(store_error),
                })?;

        self.snapshots
            .offer_snapshot(
                A::KIND,
                id,
                events_since_snapshot,
                new_position,
                prepared_snapshot,
            )
            .await
            .map_err(CommandError::Snapshot)
    }

    /// Persist new events produced from a creation command.
    ///
    /// This is the shared tail of [`create`](Self::create) and the creation
    /// branch of [`upsert`](Self::upsert).
    async fn finalize_create<A>(
        &self,
        id: &S::Id,
        events: NonEmpty<A::Event>,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
    {
        let total_events_since_snapshot = events.len() as u64;
        let prepared = self.snapshots.prepare_snapshot_from_new(&events);
        self.persist_events::<A>(
            id,
            None,
            total_events_since_snapshot,
            events,
            prepared,
            metadata,
        )
        .await
    }

    /// Persist events produced by handling a command against an existing
    /// aggregate.
    ///
    /// This is the shared tail of [`update`](Self::update) and the update
    /// branch of [`upsert`](Self::upsert).
    async fn finalize_update<A>(
        &self,
        id: &S::Id,
        aggregate: A,
        version: Option<S::Position>,
        events_since_snapshot: u64,
        events: NonEmpty<A::Event>,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id>,
        A::Event: EventKind + Serialize + Send + Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
    {
        let total_events_since_snapshot = events_since_snapshot + events.len() as u64;
        let prepared = self
            .snapshots
            .prepare_snapshot_from_existing(aggregate, &events);
        self.persist_events::<A>(
            id,
            version,
            total_events_since_snapshot,
            events,
            prepared,
            metadata,
        )
        .await
    }

    /// Execute a creation command, initialising a new aggregate stream.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] when the aggregate rejects the command, events
    /// cannot be encoded, the store fails to persist, or snapshot persistence
    /// fails. Returns [`LifecycleError::AggregateAlreadyExists`] if the stream
    /// already exists.
    pub async fn create<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id> + HandleCreate<Cmd>,
        A::Event: EventKind + Serialize + Send + Sync,
        Cmd: Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
        M::Prepared: Send,
    {
        if self
            .store
            .stream_version(A::KIND, id)
            .await
            .map_err(CommandError::Store)?
            .is_some()
        {
            return Err(CommandError::Lifecycle(
                LifecycleError::AggregateAlreadyExists,
            ));
        }

        let new_events = <A as HandleCreate<Cmd>>::handle_create(command)
            .map_err(|error| CommandError::Aggregate(error.into()))?;

        let Some(events) = NonEmpty::from_vec(new_events) else {
            return Ok(());
        };

        self.finalize_create::<A>(id, events, metadata).await
    }

    /// Execute a command against an existing aggregate stream.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] when the aggregate rejects the command, events
    /// cannot be encoded, the store fails to persist, snapshot persistence
    /// fails, or the aggregate cannot be rebuilt. Optimistic repositories
    /// return [`CommandError::Concurrency`] on conflicts. Returns
    /// [`LifecycleError::AggregateNotFound`] if the stream does not yet exist.
    pub async fn update<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Send,
        A::Event: ProjectionEvent + EventKind + Serialize + Send + Sync,
        Cmd: Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
        M::Prepared: Send,
    {
        let LoadedAggregate {
            aggregate,
            version,
            events_since_snapshot,
        } = self
            .load_aggregate::<A>(id)
            .await
            .map_err(CommandError::Projection)?
            .ok_or(CommandError::Lifecycle(LifecycleError::AggregateNotFound))?;

        let new_events = Handle::<Cmd>::handle(&aggregate, command)
            .map_err(|error| CommandError::Aggregate(error.into()))?;

        let Some(events) = NonEmpty::from_vec(new_events) else {
            return Ok(());
        };

        self.finalize_update::<A>(
            id,
            aggregate,
            version,
            events_since_snapshot,
            events,
            metadata,
        )
        .await
    }

    /// Execute a command that may create or update an aggregate stream.
    ///
    /// If the stream already exists, the command is dispatched via [`Handle`].
    /// If the stream does not yet exist, it is dispatched via [`HandleCreate`].
    ///
    /// Use this method when a command is valid for both new and existing
    /// aggregates and no lifecycle enforcement is required.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] when the aggregate rejects the command, events
    /// cannot be encoded, the store fails to persist, snapshot persistence
    /// fails, or the aggregate cannot be rebuilt. Optimistic repositories
    /// return [`CommandError::Concurrency`] on conflicts.
    pub async fn upsert<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
    ) -> Result<(), CommandError<A::Error, C::ConcurrencyError, S::Error, M::SnapshotError>>
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + HandleCreate<Cmd> + Send,
        A::Event: ProjectionEvent + EventKind + Serialize + Send + Sync,
        Cmd: Sync,
        S::Metadata: Clone,
        C: CommitPolicy<S>,
        M: SnapshotPolicy<S, A> + Sync,
        M::Prepared: Send,
    {
        if let Some(LoadedAggregate {
            aggregate,
            version,
            events_since_snapshot,
        }) = self
            .load_aggregate::<A>(id)
            .await
            .map_err(CommandError::Projection)?
        {
            let new_events = Handle::<Cmd>::handle(&aggregate, command)
                .map_err(|error| CommandError::Aggregate(error.into()))?;

            let Some(events) = NonEmpty::from_vec(new_events) else {
                return Ok(());
            };

            self.finalize_update::<A>(
                id,
                aggregate,
                version,
                events_since_snapshot,
                events,
                metadata,
            )
            .await
        } else {
            let new_events = <A as HandleCreate<Cmd>>::handle_create(command)
                .map_err(|error| CommandError::Aggregate(error.into()))?;

            let Some(events) = NonEmpty::from_vec(new_events) else {
                return Ok(());
            };

            self.finalize_create::<A>(id, events, metadata).await
        }
    }
}

impl<S, C, SS> Repository<S, C, Snapshots<SS>>
where
    S: EventStore + GloballyOrderedStore,
    S::Position: Ord,
    C: ConcurrencyStrategy,
{
    /// Load a projection with snapshot support.
    ///
    /// Loads the most recent snapshot (if available), replays events from that
    /// position, and offers a new snapshot after loading.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] when the store fails to load events or when
    /// an event cannot be deserialised.
    #[tracing::instrument(
        skip(self, instance_id),
        fields(
            projection_type = std::any::type_name::<P>(),
        )
    )]
    pub async fn load_projection_with_snapshot<P>(
        &self,
        instance_id: &P::InstanceId,
    ) -> Result<P, ProjectionError<S::Error>>
    where
        P: Projection<Id = S::Id, Metadata = S::Metadata> + Serialize + DeserializeOwned + Sync,
        P::InstanceId: Send + Sync,
        SS: SnapshotStore<P::InstanceId, Position = S::Position>,
    {
        tracing::debug!("loading projection with snapshot");

        let snapshot_result = self
            .snapshots
            .0
            .load::<P>(P::KIND, instance_id)
            .await
            .inspect_err(|e| {
                tracing::error!(error = %e, "failed to load projection snapshot");
            })
            .ok()
            .flatten();

        let (mut projection, snapshot_position) = if let Some(snapshot) = snapshot_result {
            (snapshot.data, Some(snapshot.position))
        } else {
            (P::init(instance_id), None)
        };

        let filters = P::filters::<S>(instance_id);
        let (event_filters, handlers) = filters.into_event_filters(snapshot_position.as_ref());

        let events = self
            .store
            .load_events(&event_filters)
            .await
            .map_err(ProjectionError::Store)?;

        let event_count = events.len();
        let last_position =
            replay_projection_events(&mut projection, &events, &handlers, &self.store)?;

        if event_count > 0
            && let Some(position) = last_position
        {
            let projection_ref = &projection;
            let offer = self.snapshots.0.offer_snapshot(
                P::KIND,
                instance_id,
                event_count as u64,
                move || -> Result<Snapshot<S::Position, &P>, std::convert::Infallible> {
                    Ok(Snapshot {
                        position,
                        data: projection_ref,
                    })
                },
            );

            if let Err(e) = offer.await {
                tracing::error!(error = %e, "failed to store projection snapshot");
            }
        }

        tracing::info!(events_applied = event_count, "projection loaded");
        Ok(projection)
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
}

impl<S, C, M> Repository<S, C, M>
where
    S: EventStore,
    C: ConcurrencyStrategy,
{
    /// Start a continuous subscription for a projection.
    ///
    /// Returns a [`SubscriptionBuilder`] that can be configured with callbacks
    /// before starting. The subscription replays historical events first
    /// (catch-up phase), then processes live events as they are committed.
    ///
    /// Subscription snapshots are disabled. Use [`subscribe_with_snapshots()`]
    /// to provide a snapshot store for the subscription.
    ///
    /// [`subscribe_with_snapshots()`]: Self::subscribe_with_snapshots
    ///
    /// # Example
    ///
    /// ```ignore
    /// let subscription = repo
    ///     .subscribe::<Dashboard>(())
    ///     .on_update(|d| println!("{d:?}"))
    ///     .start()
    ///     .await?;
    /// ```
    pub fn subscribe<P>(
        &self,
        instance_id: P::InstanceId,
    ) -> SubscriptionBuilder<S, P, crate::snapshot::NoSnapshots<S::Position>>
    where
        S: SubscribableStore + Clone + 'static,
        S::Position: Ord,
        P: Projection<Id = S::Id, Metadata = S::Metadata>
            + Serialize
            + DeserializeOwned
            + Send
            + Sync
            + 'static,
        P::InstanceId: Clone + Send + Sync + 'static,
        P::Metadata: Send,
    {
        SubscriptionBuilder::new(
            self.store.clone(),
            crate::snapshot::NoSnapshots::new(),
            instance_id,
        )
    }

    /// Start a continuous subscription with an explicit snapshot store.
    ///
    /// The snapshot store is keyed by `P::InstanceId` and tracks the
    /// subscription's position for faster restart.
    pub fn subscribe_with_snapshots<P, SS>(
        &self,
        instance_id: P::InstanceId,
        snapshots: SS,
    ) -> SubscriptionBuilder<S, P, SS>
    where
        S: SubscribableStore + Clone + 'static,
        S::Position: Ord,
        P: Projection<Id = S::Id, Metadata = S::Metadata>
            + Serialize
            + DeserializeOwned
            + Send
            + Sync
            + 'static,
        P::InstanceId: Clone + Send + Sync + 'static,
        P::Metadata: Send,
        SS: SnapshotStore<P::InstanceId, Position = S::Position> + Send + Sync + 'static,
    {
        SubscriptionBuilder::new(self.store.clone(), snapshots, instance_id)
    }
}

impl<S, M> Repository<S, Optimistic, M>
where
    S: EventStore,
{
    /// Execute a command against an existing aggregate stream, retrying
    /// automatically on concurrency conflicts.
    ///
    /// Returns the number of attempts made (1-based). Immediately surfaces any
    /// non-concurrency error.
    ///
    /// # Errors
    ///
    /// Returns the last error if all retries are exhausted, or a
    /// non-concurrency error on the first such failure.
    pub async fn update_with_retry<A, Cmd>(
        &self,
        id: &S::Id,
        command: &Cmd,
        metadata: &S::Metadata,
        max_retries: usize,
    ) -> Result<
        usize,
        CommandError<A::Error, ConcurrencyConflict<S::Position>, S::Error, M::SnapshotError>,
    >
    where
        A: Aggregate<Id = S::Id> + Handle<Cmd> + Send,
        A::Event: ProjectionEvent + EventKind + serde::Serialize + Send + Sync,
        Cmd: Sync,
        S::Metadata: Clone,
        M: SnapshotPolicy<S, A> + Sync,
        M::Prepared: Send,
    {
        for attempt in 1..=max_retries {
            match self.update::<A, Cmd>(id, command, metadata).await {
                Ok(()) => return Ok(attempt),
                Err(CommandError::Concurrency(_)) => {}
                Err(e) => return Err(e),
            }
        }

        self.update::<A, Cmd>(id, command, metadata)
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
        let error: CommandError<String, Infallible, io::Error, Infallible> =
            CommandError::Aggregate("invalid state".to_string());
        let msg = error.to_string();
        assert!(msg.contains("aggregate rejected command"));
        assert!(error.source().is_none());
    }

    #[test]
    fn command_error_store_has_source() {
        let error: CommandError<String, Infallible, io::Error, Infallible> =
            CommandError::Store(io::Error::other("store error"));
        assert!(error.source().is_some());
    }

    #[test]
    fn optimistic_command_error_concurrency_mentions_conflict() {
        let conflict = ConcurrencyConflict {
            expected: Some(1u64),
            actual: Some(2u64),
        };
        let error: CommandError<String, ConcurrencyConflict<u64>, io::Error, Infallible> =
            CommandError::Concurrency(conflict);
        let msg = error.to_string();
        assert!(msg.contains("concurrency conflict"));
        assert!(error.source().is_none());
    }
}
