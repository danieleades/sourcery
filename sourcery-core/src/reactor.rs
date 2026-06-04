//! Write-side event reactors.
//!
//! A reactor consumes the same checkpointed event stream as a projection
//! subscription, but performs side effects instead of folding a read model.
//! With a checkpoint store configured, delivery is at-least-once with resume.

use std::{borrow::Cow, error::Error, future::Future, pin::Pin, time::Duration};

use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::{
    aggregate::Aggregate,
    event::{DomainEvent, EventDecodeError, ProjectionEvent},
    key::StorageKey,
    projection::{EventContext, event_spec, events_specs},
    snapshot::{Snapshot, SnapshotOffer, SnapshotStore},
    store::{EventFilter, EventStore, StoredEvent},
    subscription::{
        AwaitError, ConsistencyToken, EventRunner, RunEventOutcome, RunStrategy, RunnerError,
        SubscribableStore, SubscriptionHandle,
    },
};

/// A write-side event handler that declares what it consumes and how to resume.
///
/// A reactor is deliberately stateless from the runtime's perspective. Store
/// immutable handles or configuration in `self`; keep event-derived decision
/// state in aggregates or other durable systems so redelivery is idempotent.
pub trait Reactor: Send + Sync + Sized + 'static {
    /// Stable type identifier used as the checkpoint-store kind.
    const KIND: &'static str;

    /// Raw store stream-key type.
    type Id;
    /// Metadata type expected by this reactor's handlers.
    type Metadata;

    /// Stable checkpoint-store instance id for this logical reactor instance.
    fn checkpoint_id(&self) -> Cow<'static, str> {
        Cow::Borrowed("default")
    }

    /// Declare the event filters and handlers this reactor consumes.
    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>;
}

/// Handle one concrete event type.
pub trait React<E>: Reactor {
    /// React to one event.
    ///
    /// Reactions are at-least-once: a crash after the effect but before
    /// checkpoint persistence can redeliver the same event. Implementations
    /// must therefore be idempotent.
    fn react(
        &self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &E,
    ) -> impl Future<Output = Result<(), ReactError>> + Send;
}

/// Error returned by a reaction.
#[derive(Debug, Error)]
pub enum ReactError {
    /// Transient error; the runner retries according to its policy.
    #[error("retryable reaction error: {0}")]
    Retry(#[source] Box<dyn Error + Send + Sync>),
    /// Fatal error; the runner stops.
    #[error("fatal reaction error: {0}")]
    Fatal(#[source] Box<dyn Error + Send + Sync>),
}

impl ReactError {
    /// Wrap a retryable infrastructure failure.
    pub fn retry(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Retry(Box::new(error))
    }

    /// Wrap a fatal infrastructure failure.
    pub fn fatal(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Fatal(Box::new(error))
    }

    fn into_inner(self) -> Box<dyn Error + Send + Sync> {
        match self {
            Self::Retry(error) | Self::Fatal(error) => error,
        }
    }
}

/// Errors that can occur during reactor lifecycle.
#[derive(Debug, Error)]
pub enum ReactorError<StoreError>
where
    StoreError: Error + 'static,
{
    /// Shared runner failure.
    #[error(transparent)]
    Runner(#[from] RunnerError<StoreError>),
    /// Checkpoint load or persist failed.
    #[error("checkpoint operation failed: {0}")]
    Checkpoint(#[source] Box<dyn Error + Send + Sync>),
    /// A reaction returned a fatal error or exhausted retries.
    #[error("reaction failed: {0}")]
    Reaction(#[source] Box<dyn Error + Send + Sync>),
}

/// Retry/backoff policy for [`ReactError::Retry`].
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    initial_delay: Duration,
    max_delay: Duration,
    max_retries: Option<usize>,
}

impl RetryPolicy {
    /// Create an exponential retry policy.
    #[must_use]
    pub const fn exponential(
        initial_delay: Duration,
        max_delay: Duration,
        max_retries: Option<usize>,
    ) -> Self {
        Self {
            initial_delay,
            max_delay,
            max_retries,
        }
    }

    fn next_delay(&self, retry_index: usize) -> Duration {
        let shift = u32::try_from(retry_index).unwrap_or(u32::MAX);
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(multiplier)
            .min(self.max_delay)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(5),
            max_retries: None,
        }
    }
}

pub(crate) enum ReactDispatchError<StoreError>
where
    StoreError: Error + 'static,
{
    EventDecode(EventDecodeError<StoreError>),
    Reaction(ReactError),
}

type ReactHandler<R, S> = Box<
    dyn for<'a> Fn(
            &'a R,
            &'a StoredEvent<
                <S as EventStore>::Id,
                <S as EventStore>::Position,
                <S as EventStore>::Data,
                <S as EventStore>::Metadata,
            >,
            &'a S,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(), ReactDispatchError<<S as EventStore>::Error>>>
                    + Send
                    + 'a,
            >,
        > + Send
        + Sync,
>;

/// Positioned filters and async handler map, ready for reactor execution.
pub(crate) type PositionedReactFilters<S, R> = (
    Vec<EventFilter<<S as EventStore>::Id, <S as EventStore>::Position>>,
    std::collections::HashMap<&'static str, ReactHandler<R, S>>,
);

/// Combined filter configuration and async handler map for a reactor.
pub struct ReactFilters<S, R>
where
    S: EventStore,
{
    pub(crate) specs: Vec<EventFilter<S::Id, S::Position>>,
    pub(crate) handlers: std::collections::HashMap<&'static str, ReactHandler<R, S>>,
}

impl<S, R> Default for ReactFilters<S, R>
where
    S: EventStore,
    R: Reactor<Id = S::Id, Metadata = S::Metadata>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, R> ReactFilters<S, R>
where
    S: EventStore,
    R: Reactor<Id = S::Id, Metadata = S::Metadata>,
{
    /// Create an empty filter set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            specs: Vec::new(),
            handlers: std::collections::HashMap::new(),
        }
    }

    fn push_domain_event<E>(&mut self, spec: EventFilter<S::Id, S::Position>)
    where
        E: DomainEvent + DeserializeOwned + Send + Sync + 'static,
        R: React<E>,
    {
        self.specs.push(spec);
        self.handlers.insert(
            E::KIND,
            Box::new(|reactor, stored, store| {
                Box::pin(async move {
                    let event: E = store.decode_event(stored).map_err(|error| {
                        ReactDispatchError::EventDecode(EventDecodeError::Store(error))
                    })?;
                    reactor
                        .react(EventContext::from_stored(stored), &event)
                        .await
                        .map_err(ReactDispatchError::Reaction)
                })
            }),
        );
    }

    fn push_projection_event<E>(
        &mut self,
        spec: impl Fn(&'static str) -> EventFilter<S::Id, S::Position>,
    ) where
        E: ProjectionEvent + Send + Sync + 'static,
        R: React<E>,
    {
        for (kind, spec) in events_specs::<S, E>(spec) {
            self.specs.push(spec);
            self.handlers.insert(
                kind,
                Box::new(|reactor, stored, store| {
                    Box::pin(async move {
                        let event = E::from_stored(stored, store)
                            .map_err(ReactDispatchError::EventDecode)?;
                        reactor
                            .react(EventContext::from_stored(stored), &event)
                            .await
                            .map_err(ReactDispatchError::Reaction)
                    })
                }),
            );
        }
    }

    /// Subscribe to a specific event type globally.
    #[must_use]
    pub fn event<E>(mut self) -> Self
    where
        E: DomainEvent + DeserializeOwned + Send + Sync + 'static,
        R: React<E>,
    {
        self.push_domain_event::<E>(event_spec::<S, E>());
        self
    }

    /// Subscribe to all event kinds supported by a [`ProjectionEvent`] sum
    /// type.
    #[must_use]
    pub fn events<E>(mut self) -> Self
    where
        E: ProjectionEvent + Send + Sync + 'static,
        R: React<E>,
    {
        self.push_projection_event::<E>(EventFilter::for_event);
        self
    }

    /// Subscribe to a specific event type from a specific aggregate instance.
    ///
    /// The instance is named by the aggregate's own id type, projected onto the
    /// store's raw key via [`StorageKey`] — mirroring the command-side
    /// repository methods and the projection
    /// [`Filters`](crate::projection::Filters).
    #[must_use]
    pub fn event_for<A, E>(mut self, aggregate_id: &A::Id) -> Self
    where
        A: Aggregate,
        A::Id: StorageKey<S::Id>,
        E: DomainEvent + DeserializeOwned + Send + Sync + 'static,
        R: React<E>,
    {
        self.push_domain_event::<E>(EventFilter::for_aggregate(
            E::KIND,
            A::KIND,
            aggregate_id.to_key(),
        ));
        self
    }

    /// Subscribe to all events from a specific aggregate instance.
    ///
    /// The instance is named by the aggregate's own id type (see
    /// [`event_for`](Self::event_for)).
    #[must_use]
    pub fn events_for<A>(mut self, aggregate_id: &A::Id) -> Self
    where
        A: Aggregate,
        A::Id: StorageKey<S::Id>,
        A::Event: ProjectionEvent + Send + Sync + 'static,
        R: React<A::Event>,
    {
        let aggregate_id = aggregate_id.to_key();
        self.push_projection_event::<A::Event>(move |kind| {
            EventFilter::for_aggregate(kind, A::KIND, aggregate_id.clone())
        });
        self
    }

    pub(crate) fn into_event_filters(
        self,
        after: Option<&S::Position>,
    ) -> PositionedReactFilters<S, R> {
        let filters = self
            .specs
            .into_iter()
            .map(|mut spec| {
                spec.after_position = after.cloned();
                spec
            })
            .collect();
        (filters, self.handlers)
    }
}

/// Builder for configuring and starting a reactor.
pub struct ReactorBuilder<S, R, CS>
where
    S: EventStore,
    R: Reactor,
{
    pub(crate) store: S,
    pub(crate) reactor: R,
    pub(crate) checkpoints: CS,
    pub(crate) retry_policy: RetryPolicy,
}

impl<S, R, CS> ReactorBuilder<S, R, CS>
where
    S: EventStore,
    R: Reactor,
{
    /// Create a builder. Called internally by
    /// [`Repository::react`](crate::repository::Repository::react).
    pub(crate) fn new(store: S, reactor: R, checkpoints: CS) -> Self {
        Self {
            store,
            reactor,
            checkpoints,
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Persist reactor progress with an explicit checkpoint store.
    pub fn with_checkpoints<CS2>(self, store: CS2) -> ReactorBuilder<S, R, CS2>
    where
        CS2: SnapshotStore<String>,
    {
        ReactorBuilder {
            store: self.store,
            reactor: self.reactor,
            checkpoints: store,
            retry_policy: self.retry_policy,
        }
    }

    /// Configure retry/backoff for [`ReactError::Retry`].
    #[must_use]
    pub const fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }
}

impl<S, R, CS> ReactorBuilder<S, R, CS>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Data: Send,
    S::Metadata: Send + Sync,
    R: Reactor<Id = S::Id, Metadata = S::Metadata> + Send + Sync + 'static,
    CS: SnapshotStore<String, Position = S::Checkpoint> + Send + Sync + 'static,
{
    /// Start the reactor background task.
    ///
    /// This method returns only after catch-up completes.
    ///
    /// # Errors
    ///
    /// Returns a [`ReactorError`] if checkpoint loading, initial store access,
    /// catch-up, or the background task fails before startup is ready.
    pub async fn start(
        self,
    ) -> Result<ReactorHandle<S::Checkpoint, S::Error>, ReactorError<S::Error>> {
        let Self {
            store,
            reactor,
            checkpoints,
            retry_policy,
        } = self;

        let checkpoint_id = reactor.checkpoint_id().into_owned();
        let from_checkpoint =
            load_checkpoint::<CS, S::Checkpoint, S::Error>(&checkpoints, R::KIND, &checkpoint_id)
                .await?;
        let (progress_tx, progress_rx) = tokio::sync::watch::channel(from_checkpoint.clone());

        let filters = reactor.filters::<S>();
        let (event_filters, handlers) = filters.into_event_filters(None);
        let catchup_target = store
            .current_checkpoint(&event_filters)
            .await
            .map_err(RunnerError::Store)?;

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let caught_up = catchup_target.is_none() || from_checkpoint >= catchup_target;
        let strategy = ReactorStrategy {
            reactor,
            checkpoint_id,
            checkpoints,
            handlers,
            retry_policy,
            events_since_checkpoint: 0,
        };
        let runner = EventRunner {
            strategy,
            progress_tx,
            catchup_target,
            last_checkpoint: from_checkpoint.clone(),
            observed: from_checkpoint.clone(),
            caught_up,
        };

        let task =
            tokio::spawn(runner.run(store, event_filters, from_checkpoint, stop_rx, ready_tx));

        match ready_rx.await {
            Ok(()) => Ok(ReactorHandle {
                progress: progress_rx,
                handle: SubscriptionHandle::new(stop_tx, task),
            }),
            Err(_) => match task.await {
                Ok(Ok(())) => Err(RunnerError::CatchupInterrupted.into()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(RunnerError::TaskPanicked.into()),
            },
        }
    }
}

/// Handle to a running reactor.
pub struct ReactorHandle<Cp, StoreError>
where
    StoreError: Error + 'static,
{
    progress: tokio::sync::watch::Receiver<Option<Cp>>,
    handle: SubscriptionHandle<StoreError, ReactorError<StoreError>>,
}

impl<Cp, StoreError> ReactorHandle<Cp, StoreError>
where
    StoreError: Error + 'static,
{
    /// Stop the reactor gracefully and wait for it to finish.
    ///
    /// # Errors
    ///
    /// Returns the reactor's error if it failed before being stopped.
    pub async fn stop(self) -> Result<(), ReactorError<StoreError>>
    where
        Cp: Send + Sync,
        StoreError: Send,
    {
        self.handle.stop().await
    }

    /// Check if the reactor task is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.handle.is_running()
    }

    /// The latest checkpoint this reactor has processed.
    #[must_use]
    pub fn processed(&self) -> Option<Cp>
    where
        Cp: Clone,
    {
        self.progress.borrow().clone()
    }

    /// Wait until this reactor has processed at least up to `token`.
    ///
    /// # Errors
    ///
    /// Returns [`AwaitError::SubscriptionStopped`] if the runtime stops before
    /// reaching `token`.
    pub async fn wait_for(&self, token: ConsistencyToken<Cp>) -> Result<(), AwaitError>
    where
        Cp: PartialOrd + Send + Sync,
        StoreError: Send,
    {
        let target = token.checkpoint();
        let mut progress = self.progress.clone();
        progress
            .wait_for(|processed| processed.as_ref().is_some_and(|cp| cp >= target))
            .await
            .map(|_| ())
            .map_err(|_| AwaitError::SubscriptionStopped)
    }
}

struct ReactorStrategy<S, R, CS>
where
    S: EventStore,
    R: Reactor<Id = S::Id, Metadata = S::Metadata>,
{
    reactor: R,
    checkpoint_id: String,
    checkpoints: CS,
    handlers: std::collections::HashMap<&'static str, ReactHandler<R, S>>,
    retry_policy: RetryPolicy,
    events_since_checkpoint: u64,
}

// The strategy futures must be `Send` (the runner spawns them), which an
// `async fn` return cannot express; the explicit `-> impl Future + Send` is
// required, so `manual_async_fn` does not apply here.
#[allow(clippy::manual_async_fn)]
impl<S, R, CS> RunStrategy<S> for ReactorStrategy<S, R, CS>
where
    S: SubscribableStore + Send + Sync + 'static,
    S::Data: Send,
    S::Metadata: Send + Sync,
    R: Reactor<Id = S::Id, Metadata = S::Metadata> + Send + Sync + 'static,
    CS: SnapshotStore<String, Position = S::Checkpoint> + Send + Sync + 'static,
{
    type Error = ReactorError<S::Error>;

    fn on_event<'a>(
        &'a mut self,
        _checkpoint: &'a S::Checkpoint,
        event: &'a StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
        store: &'a S,
        stop_rx: &'a mut tokio::sync::oneshot::Receiver<()>,
    ) -> impl Future<Output = Result<RunEventOutcome, Self::Error>> + Send + 'a {
        async move {
            // The handler is invariant across retries, so resolve it once.
            // Matching events without a registered handler (filters may be wider
            // than the handler set) need no work, but still advance the cursor.
            let Some(handler) = self.handlers.get(event.kind()) else {
                return Ok(RunEventOutcome::Processed);
            };

            let mut retry_index = 0;
            loop {
                match handler(&self.reactor, event, store).await {
                    Ok(()) => return Ok(RunEventOutcome::Processed),
                    Err(ReactDispatchError::EventDecode(error)) => {
                        return Err(RunnerError::EventDecode(error).into());
                    }
                    Err(ReactDispatchError::Reaction(ReactError::Fatal(error))) => {
                        return Err(ReactorError::Reaction(error));
                    }
                    Err(ReactDispatchError::Reaction(error @ ReactError::Retry(_))) => {
                        if self
                            .retry_policy
                            .max_retries
                            .is_some_and(|max| retry_index >= max)
                        {
                            return Err(ReactorError::Reaction(error.into_inner()));
                        }

                        let delay = self.retry_policy.next_delay(retry_index);
                        retry_index += 1;
                        tokio::select! {
                            biased;
                            _ = &mut *stop_rx => return Ok(RunEventOutcome::Stop),
                            () = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }
        }
    }

    fn persist<'a>(
        &'a mut self,
        checkpoint: &'a S::Checkpoint,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'a {
        async move {
            // The runner calls `persist` exactly once per processed event, so
            // this is where the pending-event count is tracked.
            self.events_since_checkpoint += 1;
            let stored = offer_checkpoint(
                &self.checkpoints,
                R::KIND,
                &self.checkpoint_id,
                self.events_since_checkpoint,
                checkpoint,
            )
            .await?;
            // Reset only when the store durably persisted, so a batching policy
            // (e.g. `every(n)`) accumulates the count across declined offers.
            if stored {
                self.events_since_checkpoint = 0;
            }
            Ok(())
        }
    }

    fn on_shutdown(
        self,
        last_checkpoint: Option<S::Checkpoint>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move {
            if let Some(checkpoint) = last_checkpoint.as_ref() {
                offer_checkpoint(
                    &self.checkpoints,
                    R::KIND,
                    &self.checkpoint_id,
                    self.events_since_checkpoint,
                    checkpoint,
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn load_checkpoint<CS, Cp, SE>(
    store: &CS,
    kind: &str,
    id: &String,
) -> Result<Option<Cp>, ReactorError<SE>>
where
    CS: SnapshotStore<String, Position = Cp>,
    SE: Error + 'static,
{
    store
        .load::<()>(kind, id)
        .await
        .map_err(|error| ReactorError::Checkpoint(Box::new(error)))
        .map(|snapshot| snapshot.map(|snapshot| snapshot.position))
}

/// Offer the latest checkpoint to the store, returning whether it was durably
/// stored. The caller resets its pending-event counter only on a `true`, so the
/// store's batching policy (e.g. `every(n)`) sees an accumulating count rather
/// than a perpetual `1` — mirroring projection snapshot maintenance.
async fn offer_checkpoint<CS, SE>(
    store: &CS,
    kind: &str,
    id: &String,
    events_since_checkpoint: u64,
    position: &CS::Position,
) -> Result<bool, ReactorError<SE>>
where
    CS: SnapshotStore<String>,
    CS::Position: Clone,
    SE: Error + 'static,
{
    if events_since_checkpoint == 0 {
        return Ok(false);
    }

    let position = position.clone();
    let result = store
        .offer_snapshot(
            kind,
            id,
            events_since_checkpoint,
            move || -> Result<Snapshot<CS::Position, ()>, std::convert::Infallible> {
                Ok(Snapshot { position, data: () })
            },
        )
        .await;

    match result {
        Ok(SnapshotOffer::Stored) => Ok(true),
        Ok(SnapshotOffer::Declined) => Ok(false),
        Err(error) => Err(ReactorError::Checkpoint(Box::new(error))),
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, time::Duration};

    use super::*;
    use crate::{snapshot::NoSnapshots, subscription::RunnerError};

    // --- RetryPolicy ---

    #[test]
    fn retry_policy_delay_starts_at_initial() {
        let policy =
            RetryPolicy::exponential(Duration::from_millis(10), Duration::from_secs(1), None);
        assert_eq!(policy.next_delay(0), Duration::from_millis(10));
    }

    #[test]
    fn retry_policy_delay_doubles_each_step() {
        let policy =
            RetryPolicy::exponential(Duration::from_millis(10), Duration::from_secs(1), None);
        assert_eq!(policy.next_delay(1), Duration::from_millis(20));
        assert_eq!(policy.next_delay(2), Duration::from_millis(40));
        assert_eq!(policy.next_delay(3), Duration::from_millis(80));
    }

    #[test]
    fn retry_policy_delay_saturates_at_max() {
        let policy =
            RetryPolicy::exponential(Duration::from_millis(10), Duration::from_millis(50), None);
        // 10ms * 8 = 80ms > 50ms cap
        assert_eq!(policy.next_delay(3), Duration::from_millis(50));
        assert_eq!(policy.next_delay(100), Duration::from_millis(50));
    }

    #[test]
    fn retry_policy_large_index_no_overflow() {
        let policy =
            RetryPolicy::exponential(Duration::from_millis(10), Duration::from_secs(5), None);
        // Both very large indices should saturate silently rather than panicking.
        let _ = policy.next_delay(64);
        let _ = policy.next_delay(usize::MAX);
    }

    #[test]
    fn retry_policy_default_has_sane_bounds() {
        let policy = RetryPolicy::default();
        // First retry should be short; after many retries it must cap.
        assert!(policy.next_delay(0) < Duration::from_secs(1));
        assert_eq!(policy.next_delay(usize::MAX), policy.max_delay);
    }

    // --- ReactError ---

    #[test]
    fn react_error_retry_display_mentions_retryable() {
        let err = ReactError::retry(io::Error::other("transient failure"));
        assert!(err.to_string().contains("retryable"));
        assert!(err.source().is_some());
    }

    #[test]
    fn react_error_fatal_display_mentions_fatal() {
        let err = ReactError::fatal(io::Error::other("permanent failure"));
        assert!(err.to_string().contains("fatal"));
        assert!(err.source().is_some());
    }

    // --- ReactorError ---

    #[test]
    fn reactor_error_checkpoint_has_source() {
        let err: ReactorError<io::Error> =
            ReactorError::Checkpoint(Box::new(io::Error::other("db unavailable")));
        assert!(err.to_string().contains("checkpoint"));
        assert!(err.source().is_some());
    }

    #[test]
    fn reactor_error_reaction_has_source() {
        let err: ReactorError<io::Error> =
            ReactorError::Reaction(Box::new(io::Error::other("side-effect failed")));
        assert!(err.to_string().contains("reaction failed"));
        assert!(err.source().is_some());
    }

    #[test]
    fn reactor_error_runner_displays_transparently() {
        let inner: RunnerError<io::Error> = RunnerError::TaskPanicked;
        let err: ReactorError<io::Error> = ReactorError::Runner(inner);
        assert!(err.to_string().contains("panicked"));
    }

    // --- offer_checkpoint ---

    #[tokio::test]
    async fn offer_checkpoint_skips_store_when_no_events_processed() {
        // A reactor that has not yet processed any events should never write a
        // checkpoint — the position has not advanced.
        let store: NoSnapshots<i64> = NoSnapshots::new();
        let id = "reactor-instance".to_string();
        let result = offer_checkpoint::<_, io::Error>(&store, "MyReactor", &id, 0, &42_i64).await;
        assert_eq!(result.unwrap(), false);
    }

    // --- load_checkpoint ---

    #[tokio::test]
    async fn load_checkpoint_returns_none_for_fresh_reactor() {
        // A brand-new reactor with no prior checkpoint should start from the
        // beginning of the stream (None position).
        let store: NoSnapshots<i64> = NoSnapshots::new();
        let id = "reactor-instance".to_string();
        let result = load_checkpoint::<_, i64, io::Error>(&store, "MyReactor", &id).await;
        assert_eq!(result.unwrap(), None);
    }
}
