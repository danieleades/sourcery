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
    pin::Pin,
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
/// Each `Stream` item is the projection state after one event has been applied.
/// Consume updates with `StreamExt::next()`, and call [`stop()`] for graceful
/// shutdown.
///
/// [`stop()`]: Self::stop
pub struct Subscription<P, StoreError>
where
    StoreError: std::error::Error + 'static,
{
    updates: tokio::sync::mpsc::UnboundedReceiver<P>,
    handle: SubscriptionHandle<StoreError>,
}

impl<P, StoreError> Subscription<P, StoreError>
where
    StoreError: std::error::Error + 'static,
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
}

impl<P, StoreError> Stream for Subscription<P, StoreError>
where
    StoreError: std::error::Error + 'static,
{
    type Item = P;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.updates.poll_recv(cx)
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
    /// # Errors
    ///
    /// Returns an error if the initial snapshot load or stream setup fails.
    ///
    /// # Type Requirements
    ///
    /// `P` must implement [`Clone`] because each stream item yields an owned
    /// snapshot of the projection state.
    #[allow(clippy::too_many_lines)]
    pub async fn start(self) -> Result<Subscription<P, S::Error>, SubscriptionError<S::Error>>
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

        // Build filters and load historical events
        let filters = P::filters::<S>(&instance_id);
        let (event_filters, handlers) = filters.into_event_filters(snapshot_position.as_ref());

        let current_events = store
            .load_events(&event_filters)
            .await
            .map_err(SubscriptionError::Store)?;

        let catchup_target_position = current_events.last().map(|e| e.position.clone());

        // Apply all historical events
        let mut last_position = snapshot_position;
        let mut events_since_snapshot: u64 = 0;

        for stored in &current_events {
            process_subscription_event(
                &mut projection,
                stored,
                &handlers,
                &store,
                &update_tx,
                &mut last_position,
                &mut events_since_snapshot,
            )?;
        }

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

        let task = tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);

            let signal_ready = |ready_tx: &mut Option<tokio::sync::oneshot::Sender<()>>| {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(());
                }
            };

            // Build filters for live stream from our current position
            let filters = P::filters::<S>(&instance_id);
            let (live_filters, handlers) = filters.into_event_filters(last_position.as_ref());

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

                        process_subscription_event(
                            &mut projection,
                            &stored,
                            &handlers,
                            &store,
                            &update_tx,
                            &mut last_position,
                            &mut events_since_snapshot,
                        )?;

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
        HandlerError::Store(error) => {
            SubscriptionError::EventDecode(EventDecodeError::Store(error))
        }
    })
}

/// Process one event through handler dispatch, position tracking, and update
/// emission.
fn process_subscription_event<P, S>(
    projection: &mut P,
    stored: &StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
    handlers: &HashMap<&'static str, crate::projection::EventHandler<P, S>>,
    store: &S,
    updates_tx: &tokio::sync::mpsc::UnboundedSender<P>,
    last_position: &mut Option<S::Position>,
    events_since_snapshot: &mut u64,
) -> Result<(), SubscriptionError<S::Error>>
where
    P: Projection<Id = S::Id> + Clone,
    S: EventStore,
    S::Position: Clone,
{
    // Unknown kinds are ignored: filters can intentionally include wider sets
    // than this projection currently handles.
    if let Some(handler) = handlers.get(stored.kind()) {
        apply_handler(handler, projection, stored, store)?;
    }

    *last_position = Some(stored.position());
    *events_since_snapshot += 1;

    let _ = updates_tx.send(projection.clone());

    Ok(())
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
}
