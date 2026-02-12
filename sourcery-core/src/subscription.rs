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
//! 3. Fires callbacks on each update
//!
//! # Example
//!
//! ```ignore
//! let subscription = repository
//!     .subscribe::<Dashboard>(())
//!     .on_update(|dashboard| println!("{dashboard:?}"))
//!     .start()
//!     .await?;
//!
//! // Later, shut down gracefully
//! subscription.stop().await?;
//! ```
//!
//! [`Repository::load_projection`]: crate::repository::Repository::load_projection

use std::pin::Pin;

use futures_core::Stream;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt as _;

use crate::{
    event::EventDecodeError,
    projection::{HandlerError, Projection, ProjectionFilters},
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
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.task
            .take()
            .expect("subscription task missing")
            .await
            .map_err(|_| SubscriptionError::TaskPanicked)?
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
        if self
            .task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return;
        }
        tracing::warn!(
            "subscription handle dropped without stop(); signaling background task to stop"
        );
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Type alias for the update callback.
type UpdateCallback<P> = Box<dyn Fn(&P) + Send + Sync + 'static>;

/// Builder for configuring and starting a subscription.
///
/// Created via [`Repository::subscribe()`]. Use [`on_update()`] to register
/// callbacks, then call [`start()`] to begin processing events.
///
/// [`Repository::subscribe()`]: crate::repository::Repository::subscribe
/// [`on_update()`]: SubscriptionBuilder::on_update
/// [`start()`]: SubscriptionBuilder::start
pub struct SubscriptionBuilder<S, P, SS>
where
    S: EventStore,
    P: ProjectionFilters,
{
    store: S,
    snapshots: SS,
    instance_id: P::InstanceId,
    on_update: Option<UpdateCallback<P>>,
}

impl<S, P, SS> SubscriptionBuilder<S, P, SS>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Position: Ord + Send + Sync,
    S::Data: Send,
    S::Metadata: Clone + Send + Sync + Into<P::Metadata>,
    P: Projection
        + ProjectionFilters<Id = S::Id>
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static,
    P::InstanceId: Clone + Send + Sync + 'static,
    P::Metadata: Send,
    SS: SnapshotStore<P::InstanceId, Position = S::Position> + Send + Sync + 'static,
{
    pub(crate) fn new(store: S, snapshots: SS, instance_id: P::InstanceId) -> Self {
        Self {
            store,
            snapshots,
            instance_id,
            on_update: None,
        }
    }

    /// Register a callback invoked after each event is applied.
    ///
    /// Callbacks must complete quickly. Long-running work should be dispatched
    /// to a separate task via a channel. Blocking the callback stalls the
    /// subscription loop and delays event processing.
    #[must_use]
    pub fn on_update<F>(mut self, callback: F) -> Self
    where
        F: Fn(&P) + Send + Sync + 'static,
    {
        self.on_update = Some(Box::new(callback));
        self
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
    /// 5. Continues processing live events and firing `on_update`
    ///
    /// # Errors
    ///
    /// Returns an error if the initial snapshot load or stream setup fails.
    #[allow(clippy::too_many_lines)]
    pub async fn start(self) -> Result<SubscriptionHandle<S::Error>, SubscriptionError<S::Error>> {
        let Self {
            store,
            snapshots,
            instance_id,
            on_update,
        } = self;

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
            let kind = stored.kind();
            if let Some(handler) = handlers.get(kind) {
                apply_handler(handler, &mut projection, stored, &store)?;
            }
            last_position = Some(stored.position());
            events_since_snapshot += 1;

            if let Some(ref callback) = on_update {
                callback(&projection);
            }
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

                        let kind = stored.kind();
                        if let Some(handler) = handlers.get(kind) {
                            apply_handler(handler, &mut projection, &stored, &store)?;
                        }

                        last_position = Some(stored.position());
                        events_since_snapshot += 1;

                        if let Some(ref callback) = on_update {
                            callback(&projection);
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
            Ok(()) => Ok(SubscriptionHandle {
                stop_tx: Some(stop_tx),
                task: Some(task),
            }),
            Err(_) => match task.await {
                Ok(Ok(())) => Err(SubscriptionError::CatchupInterrupted),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(SubscriptionError::TaskPanicked),
            },
        }
    }
}

async fn load_snapshot<P, SS>(
    snapshots: &SS,
    instance_id: &P::InstanceId,
) -> (P, Option<SS::Position>)
where
    P: Projection + ProjectionFilters + DeserializeOwned,
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

fn apply_handler<P, S>(
    handler: &crate::projection::EventHandler<P, S>,
    projection: &mut P,
    stored: &StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
    store: &S,
) -> Result<(), SubscriptionError<S::Error>>
where
    P: ProjectionFilters<Id = S::Id>,
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

async fn offer_projection_snapshot<P, SS>(
    snapshots: &SS,
    instance_id: &P::InstanceId,
    events_since_snapshot: u64,
    position: &SS::Position,
    projection: &P,
) -> bool
where
    P: Projection + ProjectionFilters + Serialize + Sync,
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
}
