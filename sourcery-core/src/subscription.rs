//! Push-based projection subscriptions.
//!
//! This module provides continuous event subscriptions that keep projections
//! up-to-date as events are committed. Unlike [`Repository::load_projection`],
//! which rebuilds a projection from scratch on each call, subscriptions
//! maintain an in-memory projection that updates in real time.
//!
//! # Overview
//!
//! A subscription:
//! 1. Replays historical events (catch-up phase)
//! 2. Transitions to processing live events as they are committed
//! 3. Yields projection updates via a stream
//!
//! # Example
//!
//! ```ignore
//! use tokio_stream::StreamExt as _;
//!
//! let mut subscription = repository
//!     .subscribe::<Dashboard>(())
//!     .start()
//!     .await?;
//!
//! if let Some(dashboard) = subscription.next().await {
//!     println!("{dashboard:?}");
//! }
//!
//! // Later, shut down gracefully
//! subscription.stop().await?;
//! ```
//!
//! [`Repository::load_projection`]: crate::repository::Repository::load_projection

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt as _;

use crate::{
    event::EventDecodeError,
    projection::{HandlerError, Projection},
    snapshot::{Snapshot, SnapshotStore},
    store::{EventFilter, EventStore, GloballyOrderedStore, StoredEvent},
};

/// Type alias for the boxed event stream returned by
/// [`SubscribableStore::subscribe`].
pub type EventStream<'a, S> = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    StoredEvent<
                        <S as EventStore>::Id,
                        <S as EventStore>::Position,
                        <S as EventStore>::Data,
                        <S as EventStore>::Metadata,
                    >,
                    <S as EventStore>::Error,
                >,
            > + Send
            + 'a,
    >,
>;

/// A store that supports push-based event subscriptions.
///
/// Extends [`EventStore`] with a `subscribe` method that returns a stream of
/// events. The stream replays historical events first, then yields live events
/// as they are committed.
///
/// This is a separate trait (not on [`EventStore`] directly) because not all
/// stores support push notifications. The in-memory store uses
/// `tokio::sync::broadcast`; a `PostgreSQL` implementation would use
/// `LISTEN/NOTIFY`.
pub trait SubscribableStore: EventStore + GloballyOrderedStore {
    /// Subscribe to events matching the given filters.
    ///
    /// Returns a stream that:
    /// 1. Yields all historical events after `from_position` (catch-up phase)
    /// 2. Yields live events as they are committed (live phase)
    ///
    /// `from_position` is **exclusive**: the stream yields events strictly
    /// *after* the given position.
    ///
    /// **Delivery guarantee**: at-least-once. The stream may yield duplicate
    /// events during the catch-up-to-live transition. The subscription loop
    /// deduplicates by position.
    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from_position: Option<Self::Position>,
    ) -> EventStream<'_, Self>
    where
        Self::Position: Ord;

    /// Subscribe to all events after a global position (exclusive).
    ///
    /// This is primarily used to observe the global store watermark, for
    /// example when implementing read-your-writes waiting for projection state.
    /// Unlike [`Self::subscribe`], this stream is not projection-filtered.
    fn subscribe_all(&self, from_position: Option<Self::Position>) -> EventStream<'_, Self>
    where
        Self::Position: Ord;
}

/// Errors that can occur during subscription lifecycle.
#[derive(Debug, Error)]
pub enum SubscriptionError<StoreError>
where
    StoreError: std::error::Error + 'static,
{
    /// The event store returned an error.
    #[error("store error: {0}")]
    Store(#[source] StoreError),
    /// An event could not be decoded.
    #[error("failed to decode event: {0}")]
    EventDecode(#[source] EventDecodeError<StoreError>),
    /// The subscription ended before completing catch-up.
    #[error("subscription ended before catch-up completed")]
    CatchupInterrupted,
    /// The subscription task panicked.
    #[error("subscription task panicked")]
    TaskPanicked,
}

/// Handle to a running subscription.
///
/// Dropping the handle sends a best-effort stop signal. Call [`stop()`] for
/// graceful shutdown and to observe task errors.
///
/// [`stop()`]: SubscriptionHandle::stop
pub struct SubscriptionHandle<StoreError>
where
    StoreError: std::error::Error + 'static,
{
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), SubscriptionError<StoreError>>>>,
}

impl<StoreError> SubscriptionHandle<StoreError>
where
    StoreError: std::error::Error + 'static,
{
    /// Stop the subscription gracefully and wait for it to finish.
    ///
    /// # Errors
    ///
    /// Returns the subscription's error if it failed before being stopped.
    #[allow(clippy::missing_panics_doc)]
    pub async fn stop(mut self) -> Result<(), SubscriptionError<StoreError>> {
        // By taking the tx, we ensure Drop won't try to send it again.
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        // Taking the task ensures that is_running() returns false from here on.
        if let Some(task) = self.task.take() {
            return task.await.map_err(|_| SubscriptionError::TaskPanicked)?;
        }

        Ok(())
    }

    /// Check if the subscription task is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
}

impl<StoreError> Drop for SubscriptionHandle<StoreError>
where
    StoreError: std::error::Error + 'static,
{
    fn drop(&mut self) {
        if self.is_running() {
            tracing::warn!(
                "subscription handle dropped without stop(); signaling background task to stop"
            );
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
        }
    }
}

/// Stream-first subscription handle that yields projection snapshots.
///
/// Each `Stream` item is the projection state after one relevant event has
/// been applied.
/// Historical updates produced during catch-up are buffered before
/// [`SubscriptionBuilder::start`] returns, so initial `next()` calls can yield
/// those buffered catch-up states before new live updates.
/// Consume updates with `StreamExt::next()`, and call [`stop()`] for graceful
/// shutdown.
///
/// [`stop()`]: Self::stop
pub struct Subscription<P, Pos, StoreError>
where
    Pos: Clone + Send + Sync + 'static,
    StoreError: std::error::Error + Send + Sync + 'static,
{
    updates: tokio::sync::mpsc::UnboundedReceiver<P>,
    handle: SubscriptionHandle<StoreError>,
    projection: Arc<tokio::sync::RwLock<P>>,
    last_applied_relevant_position_rx: tokio::sync::watch::Receiver<Option<Pos>>,
    wait_backend: Arc<dyn WaitBackend<Pos, StoreError>>,
    last_confirmed_global_position: Option<Pos>,
}

impl<P, Pos, StoreError> Subscription<P, Pos, StoreError>
where
    P: Send + Sync,
    Pos: Clone + Ord + Send + Sync + 'static,
    StoreError: std::error::Error + Send + Sync + 'static,
{
    /// Stop the subscription gracefully and wait for it to finish.
    ///
    /// # Errors
    ///
    /// Returns the subscription's error if it failed before being stopped.
    pub async fn stop(self) -> Result<(), SubscriptionError<StoreError>> {
        self.handle.stop().await
    }

    /// Check if the subscription task is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.handle.is_running()
    }

    /// Get the current projection state snapshot.
    pub async fn current(&self) -> P
    where
        P: Clone,
    {
        self.projection.read().await.clone()
    }

    /// Get the most recent relevant event position applied to this projection.
    #[must_use]
    pub fn last_position(&self) -> Option<Pos> {
        self.last_applied_relevant_position_rx.borrow().clone()
    }

    /// Wait until this subscription is up-to-date with at least `position`.
    ///
    /// `position` must be a global store position (for example, returned by
    /// [`crate::repository::Repository::create`],
    /// [`crate::repository::Repository::update`], or
    /// [`crate::repository::Repository::upsert`]).
    ///
    /// This method takes `&mut self` because it mutates an internal cache of
    /// the last confirmed global position. As a result, `wait_for` cannot be
    /// polled concurrently with `Stream::next()` on the same `Subscription`.
    ///
    /// If `position` belongs to an event this projection does not handle,
    /// `wait_for` confirms the global watermark, determines there is no
    /// required relevant event at or before `position`, and returns the current
    /// projection state immediately.
    ///
    /// Returns the current projection state once the relevant watermark reaches
    /// the target.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError::CatchupInterrupted`] if the subscription
    /// task exits before reaching the target.
    pub async fn wait_for(&mut self, position: Pos) -> Result<P, SubscriptionError<StoreError>>
    where
        P: Clone,
    {
        if self
            .last_applied_relevant_position_rx
            .borrow()
            .as_ref()
            .is_some_and(|current| current >= &position)
        {
            return Ok(self.current().await);
        }

        if self
            .last_confirmed_global_position
            .as_ref()
            .is_none_or(|known| known < &position)
        {
            self.last_confirmed_global_position = self
                .wait_backend
                .wait_until_global_at_least(
                    self.last_confirmed_global_position.clone(),
                    position.clone(),
                )
                .await
                .map_err(SubscriptionError::Store)?;

            if self
                .last_confirmed_global_position
                .as_ref()
                .is_none_or(|known| known < &position)
            {
                return Err(SubscriptionError::CatchupInterrupted);
            }
        }

        let after_position = self.last_applied_relevant_position_rx.borrow().clone();
        let required_relevant_position = self
            .wait_backend
            .max_relevant_position_at_or_before(after_position, position)
            .await
            .map_err(SubscriptionError::Store)?;

        let Some(required) = required_relevant_position else {
            return Ok(self.current().await);
        };

        loop {
            if self
                .last_applied_relevant_position_rx
                .borrow()
                .as_ref()
                .is_some_and(|current| current >= &required)
            {
                return Ok(self.current().await);
            }

            self.last_applied_relevant_position_rx
                .changed()
                .await
                .map_err(|_| SubscriptionError::CatchupInterrupted)?;
        }
    }
}

impl<P, Pos, StoreError> Stream for Subscription<P, Pos, StoreError>
where
    P: Send + Sync,
    Pos: Clone + Send + Sync + 'static,
    StoreError: std::error::Error + Send + Sync + 'static,
{
    type Item = P;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().get_mut().updates.poll_recv(cx)
    }
}

// `Subscription` does not contain self-referential pinned fields. `P` lives
// behind `Arc<RwLock<P>>`, so moving `Subscription` does not invalidate any
// pinned projection state.
impl<P, Pos, StoreError> Unpin for Subscription<P, Pos, StoreError>
where
    P: Send + Sync,
    Pos: Clone + Send + Sync + 'static,
    StoreError: std::error::Error + Send + Sync + 'static,
{
}

/// Backend operations used by [`Subscription::wait_for`].
trait WaitBackend<Pos, StoreError>: Send + Sync
where
    Pos: Clone + Ord + Send + Sync + 'static,
    StoreError: std::error::Error + 'static,
{
    fn wait_until_global_at_least<'a>(
        &'a self,
        from_position: Option<Pos>,
        target: Pos,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Pos>, StoreError>> + Send + 'a>>;

    fn max_relevant_position_at_or_before<'a>(
        &'a self,
        after_position: Option<Pos>,
        target: Pos,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Pos>, StoreError>> + Send + 'a>>;
}

/// Store-backed implementation of [`WaitBackend`].
///
/// `event_filters` must come from the same
/// `P::filters::<S>(&instance_id).into_event_filters(None)` call that produced
/// the subscription handler map. This keeps relevant-position lookups aligned
/// with handler dispatch.
struct StoreWaitBackend<S>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Position: Ord + Send + Sync,
{
    store: S,
    event_filters: Vec<EventFilter<S::Id, S::Position>>,
}

impl<S> StoreWaitBackend<S>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Position: Ord + Send + Sync,
{
    const fn new(store: S, event_filters: Vec<EventFilter<S::Id, S::Position>>) -> Self {
        Self {
            store,
            event_filters,
        }
    }
}

impl<S> WaitBackend<S::Position, S::Error> for StoreWaitBackend<S>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Position: Clone + Ord + Send + Sync + 'static,
{
    fn wait_until_global_at_least<'a>(
        &'a self,
        from_position: Option<S::Position>,
        target: S::Position,
    ) -> Pin<Box<dyn Future<Output = Result<Option<S::Position>, S::Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut current = from_position;
            if current.as_ref().is_some_and(|known| known >= &target) {
                return Ok(current);
            }

            let latest_position = self.store.latest_position().await?;
            current = match (current, latest_position) {
                (Some(current), Some(latest)) => Some(current.max(latest)),
                (Some(current), None) => Some(current),
                (None, latest) => latest,
            };

            if current.as_ref().is_some_and(|known| known >= &target) {
                return Ok(current);
            }

            let mut stream = self.store.subscribe_all(current.clone());
            while let Some(result) = stream.next().await {
                let event = result?;
                current = Some(event.position());
                if current.as_ref().is_some_and(|known| known >= &target) {
                    break;
                }
            }

            Ok(current)
        })
    }

    fn max_relevant_position_at_or_before<'a>(
        &'a self,
        after_position: Option<S::Position>,
        target: S::Position,
    ) -> Pin<Box<dyn Future<Output = Result<Option<S::Position>, S::Error>> + Send + 'a>> {
        Box::pin(async move {
            let filters = with_after_position(&self.event_filters, after_position.as_ref());
            let events = self.store.load_events(&filters).await?;
            let max = events
                .into_iter()
                .map(|stored| stored.position())
                .filter(|position| position <= &target)
                .max();
            Ok(max)
        })
    }
}

/// Builder for configuring and starting a subscription.
///
/// Created via [`Repository::subscribe()`]. Call [`start()`] to begin
/// processing events and obtain a stream of projection updates.
///
/// [`Repository::subscribe()`]: crate::repository::Repository::subscribe
/// [`start()`]: SubscriptionBuilder::start
pub struct SubscriptionBuilder<S, P, SS>
where
    S: EventStore,
    P: Projection,
{
    store: S,
    snapshots: SS,
    instance_id: P::InstanceId,
}

/// Merge a base set of filters with an additional `after` watermark.
///
/// For each filter, the effective `after_position` is the maximum of the
/// filter's own `after_position` and `after` (when both are present).
fn with_after_position<Id, Pos>(
    filters: &[EventFilter<Id, Pos>],
    after: Option<&Pos>,
) -> Vec<EventFilter<Id, Pos>>
where
    Id: Clone,
    Pos: Clone + Ord,
{
    filters
        .iter()
        .cloned()
        .map(|mut filter| {
            if let Some(base_after) = after {
                filter.after_position = Some(filter.after_position.map_or_else(
                    || base_after.clone(),
                    |own_after| own_after.max(base_after.clone()),
                ));
            }
            filter
        })
        .collect()
}

impl<S, P, SS> SubscriptionBuilder<S, P, SS>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Position: Ord + Send + Sync,
    S::Data: Send,
    S::Metadata: Send + Sync,
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
    /// Create a builder. Called internally by [`Repository::subscribe`] and
    /// [`Repository::subscribe_with_snapshots`].
    pub(crate) const fn new(store: S, snapshots: SS, instance_id: P::InstanceId) -> Self {
        Self {
            store,
            snapshots,
            instance_id,
        }
    }

    /// Start the subscription.
    ///
    /// This method returns only after catch-up completes.
    ///
    /// Spawns a background task that:
    /// 1. Loads the most recent snapshot (if available)
    /// 2. Subscribes to the event stream from the snapshot position
    /// 3. Replays historical events (catch-up phase)
    /// 4. Waits until catch-up is complete
    /// 5. Continues processing live events and yielding projection updates
    ///
    /// Historical projection updates emitted during catch-up are buffered in
    /// the returned stream before `start()` resolves.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial snapshot load or stream setup fails.
    ///
    /// # Type Requirements
    ///
    /// `P` must implement [`Clone`] because each stream item yields an owned
    /// snapshot of the projection state.
    // Keep this as one flow so catch-up state, readiness signalling, and live
    // hand-off remain coordinated without introducing extra lifetime plumbing.
    #[allow(clippy::too_many_lines)]
    pub async fn start(
        self,
    ) -> Result<Subscription<P, S::Position, S::Error>, SubscriptionError<S::Error>>
    where
        P: Clone,
    {
        let Self {
            store,
            snapshots,
            instance_id,
        } = self;

        let (update_tx, update_rx) = tokio::sync::mpsc::unbounded_channel();

        let (mut projection, snapshot_position) =
            load_snapshot::<P, SS>(&snapshots, &instance_id).await;

        // Build handlers and load historical events.
        let filters = P::filters::<S>(&instance_id);
        let (base_filters, handlers) = filters.into_event_filters(None);
        let historical_filters = with_after_position(&base_filters, snapshot_position.as_ref());

        let current_events = store
            .load_events(&historical_filters)
            .await
            .map_err(SubscriptionError::Store)?;

        let catchup_target_position = current_events.last().map(|e| e.position.clone());

        // Apply all historical events
        let mut last_position = snapshot_position;
        // Count only events that changed projection state, so snapshot cadence
        // reflects projection churn rather than unrelated traffic.
        let mut events_since_snapshot: u64 = 0;

        for stored in &current_events {
            let _ = process_subscription_event(
                &mut projection,
                stored,
                &handlers,
                &store,
                &update_tx,
                &mut last_position,
                &mut events_since_snapshot,
            )?;
        }

        let projection_state = Arc::new(tokio::sync::RwLock::new(projection.clone()));
        let (last_applied_relevant_position_tx, last_applied_relevant_position_rx) =
            tokio::sync::watch::channel(last_position.clone());
        // Keep wait-filter semantics aligned with handler dispatch by reusing
        // the same base filter set used to build `handlers`.
        let wait_backend: Arc<dyn WaitBackend<S::Position, S::Error>> =
            Arc::new(StoreWaitBackend::new(store.clone(), base_filters.clone()));

        // Offer snapshot after catch-up (preserve counter if declined)
        if events_since_snapshot > 0
            && let Some(ref pos) = last_position
            && offer_projection_snapshot(
                &snapshots,
                &instance_id,
                events_since_snapshot,
                pos,
                &projection,
            )
            .await
        {
            events_since_snapshot = 0;
        }

        // Spawn live subscription task
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let projection_state_task = Arc::clone(&projection_state);

        let task = tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);

            let signal_ready = |ready_tx: &mut Option<tokio::sync::oneshot::Sender<()>>| {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(());
                }
            };

            let live_filters = with_after_position(&base_filters, last_position.as_ref());
            let mut stream = store.subscribe(&live_filters, last_position.clone());

            // Determine the effective catch-up target by querying the store
            // after the live stream is attached. This captures any events
            // committed during the gap between the initial load_events and
            // subscribe — those events are buffered in the stream and must
            // be applied before the projection is truly current.
            let catchup_target = store
                .load_events(&live_filters)
                .await
                .map_err(SubscriptionError::Store)?
                .last()
                .map(|e| e.position.clone())
                .or(catchup_target_position);

            // If already caught up (no pending gap events), signal immediately.
            if catchup_target.is_none() || last_position >= catchup_target {
                signal_ready(&mut ready_tx);
            }

            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => {
                        tracing::debug!("subscription stopped");
                        break;
                    }
                    event = stream.next() => {
                        let Some(result) = event else {
                            tracing::debug!("subscription stream ended");
                            break;
                        };

                        let stored = result.map_err(SubscriptionError::Store)?;

                        // Position-based deduplication
                        if let Some(ref lp) = last_position
                            && stored.position <= *lp
                        {
                            continue;
                        }

                        let projection_changed = process_subscription_event(
                            &mut projection,
                            &stored,
                            &handlers,
                            &store,
                            &update_tx,
                            &mut last_position,
                            &mut events_since_snapshot,
                        )?;

                        if projection_changed {
                            let _ =
                                last_applied_relevant_position_tx.send(last_position.clone());
                            *projection_state_task.write().await = projection.clone();
                        }

                        // Signal catch-up completion once we've processed
                        // past the effective target (includes gap events).
                        if catchup_target.is_none() || last_position >= catchup_target {
                            signal_ready(&mut ready_tx);
                        }

                        // Periodically offer snapshots
                        if events_since_snapshot.is_multiple_of(100)
                            && let Some(ref pos) = last_position
                            && offer_projection_snapshot(
                                &snapshots,
                                &instance_id,
                                events_since_snapshot,
                                pos,
                                &projection,
                            )
                            .await
                        {
                            events_since_snapshot = 0;
                        }
                    }
                }
            }

            // Final snapshot on shutdown
            if events_since_snapshot > 0
                && let Some(ref pos) = last_position
            {
                let _ = offer_projection_snapshot(
                    &snapshots,
                    &instance_id,
                    events_since_snapshot,
                    pos,
                    &projection,
                )
                .await;
            }

            Ok(())
        });

        match ready_rx.await {
            Ok(()) => Ok(Subscription {
                updates: update_rx,
                handle: SubscriptionHandle {
                    stop_tx: Some(stop_tx),
                    task: Some(task),
                },
                projection: projection_state,
                last_applied_relevant_position_rx,
                wait_backend,
                last_confirmed_global_position: None,
            }),
            Err(_) => match task.await {
                Ok(Ok(())) => Err(SubscriptionError::CatchupInterrupted),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(SubscriptionError::TaskPanicked),
            },
        }
    }
}

/// Load the latest projection snapshot, falling back to `P::init`.
///
/// Snapshot load failures are logged and treated as cache misses so the
/// subscription can still start from event replay.
async fn load_snapshot<P, SS>(
    snapshots: &SS,
    instance_id: &P::InstanceId,
) -> (P, Option<SS::Position>)
where
    P: Projection + DeserializeOwned,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId>,
{
    let snapshot_result = snapshots
        .load::<P>(P::KIND, instance_id)
        .await
        .inspect_err(|e| {
            tracing::error!(error = %e, "failed to load subscription snapshot");
        })
        .ok()
        .flatten();

    if let Some(snapshot) = snapshot_result {
        (snapshot.data, Some(snapshot.position))
    } else {
        (P::init(instance_id), None)
    }
}

/// Apply one decoded handler closure and normalise its error into
/// [`SubscriptionError`].
fn apply_handler<P, S>(
    handler: &crate::projection::EventHandler<P, S>,
    projection: &mut P,
    stored: &StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
    store: &S,
) -> Result<(), SubscriptionError<S::Error>>
where
    P: Projection<Id = S::Id>,
    S: EventStore,
{
    (handler)(
        projection,
        stored.aggregate_id(),
        stored,
        stored.metadata(),
        store,
    )
    .map_err(|error| match error {
        HandlerError::EventDecode(error) => SubscriptionError::EventDecode(error),
        // Store failures raised during decode are surfaced through
        // `EventDecodeError` to preserve the decode-vs-store distinction.
        HandlerError::Store(error) => {
            SubscriptionError::EventDecode(EventDecodeError::Store(error))
        }
    })
}

/// Process one event through handler dispatch, position tracking, and update
/// emission.
///
/// `events_since_snapshot` counts only projection-changing events.
fn process_subscription_event<P, S>(
    projection: &mut P,
    stored: &StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
    handlers: &HashMap<&'static str, crate::projection::EventHandler<P, S>>,
    store: &S,
    updates_tx: &tokio::sync::mpsc::UnboundedSender<P>,
    last_position: &mut Option<S::Position>,
    events_since_snapshot: &mut u64,
) -> Result<bool, SubscriptionError<S::Error>>
where
    P: Projection<Id = S::Id> + Clone,
    S: EventStore,
    S::Position: Clone,
{
    // Unknown kinds are ignored: filters can intentionally include wider sets
    // than this projection currently handles.
    let projection_updated = if let Some(handler) = handlers.get(stored.kind()) {
        apply_handler(handler, projection, stored, store)?;
        true
    } else {
        false
    };

    *last_position = Some(stored.position());

    if projection_updated {
        *events_since_snapshot += 1;
        let _ = updates_tx.send(projection.clone());
    }

    Ok(projection_updated)
}

/// Offer a projection snapshot and report whether it was stored.
///
/// Errors are logged and treated as "not stored" so subscriptions remain live.
async fn offer_projection_snapshot<P, SS>(
    snapshots: &SS,
    instance_id: &P::InstanceId,
    events_since_snapshot: u64,
    position: &SS::Position,
    projection: &P,
) -> bool
where
    P: Projection + Serialize + Sync,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId>,
    SS::Position: Clone,
{
    let pos = position.clone();
    let result = snapshots
        .offer_snapshot(
            P::KIND,
            instance_id,
            events_since_snapshot,
            move || -> Result<Snapshot<SS::Position, &P>, std::convert::Infallible> {
                Ok(Snapshot {
                    position: pos,
                    data: projection,
                })
            },
        )
        .await;

    match result {
        Ok(crate::snapshot::SnapshotOffer::Stored) => true,
        Ok(crate::snapshot::SnapshotOffer::Declined) => false,
        Err(e) => {
            tracing::warn!(error = %e, "failed to store subscription snapshot");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::*;

    #[test]
    fn subscription_error_store_displays() {
        let err: SubscriptionError<io::Error> = SubscriptionError::Store(io::Error::other("test"));
        assert!(err.to_string().contains("store error"));
        assert!(err.source().is_some());
    }

    #[test]
    fn subscription_error_task_panicked_displays() {
        let err: SubscriptionError<io::Error> = SubscriptionError::TaskPanicked;
        assert!(err.to_string().contains("panicked"));
    }

    #[test]
    fn subscription_not_alive_after_stop_consumes_task_handle() {
        let handle: SubscriptionHandle<io::Error> = SubscriptionHandle {
            stop_tx: None,
            task: None,
        };
        assert!(!handle.is_running());
    }

    struct TestWaitBackend;

    impl WaitBackend<u64, io::Error> for TestWaitBackend {
        fn wait_until_global_at_least<'a>(
            &'a self,
            _from_position: Option<u64>,
            target: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Option<u64>, io::Error>> + Send + 'a>> {
            Box::pin(async move { Ok(Some(target)) })
        }

        fn max_relevant_position_at_or_before<'a>(
            &'a self,
            _after_position: Option<u64>,
            target: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Option<u64>, io::Error>> + Send + 'a>> {
            Box::pin(async move { Ok(Some(target)) })
        }
    }

    #[tokio::test]
    async fn wait_for_returns_catchup_interrupted_when_watch_channel_closes() {
        let (_update_tx, update_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let (position_tx, position_rx) = tokio::sync::watch::channel(None::<u64>);
        drop(position_tx);

        let mut subscription = Subscription {
            updates: update_rx,
            handle: SubscriptionHandle {
                stop_tx: None,
                task: None,
            },
            projection: Arc::new(tokio::sync::RwLock::new(0_u64)),
            last_applied_relevant_position_rx: position_rx,
            wait_backend: Arc::new(TestWaitBackend),
            last_confirmed_global_position: None,
        };

        let result = subscription.wait_for(42).await;
        assert!(matches!(result, Err(SubscriptionError::CatchupInterrupted)));
    }
}
