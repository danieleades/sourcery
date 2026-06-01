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
//! 3. Maintains the live projection, readable via
//!    [`current`](Subscription::current), with each subsequent update available
//!    through the opt-in stream from [`updates`](Subscription::updates)
//!
//! # Example
//!
//! ```ignore
//! let subscription = repository
//!     .subscribe::<Dashboard>(())
//!     .start()
//!     .await?;
//!
//! // Read your own write: await the write's token, then read the live model.
//! let token = repository
//!     .update_tracked::<Account, Deposit>(&id, &deposit, &metadata)
//!     .await?;
//! let dashboard = subscription.read_after(token).await?;
//! println!("{dashboard:?}");
//!
//! // Later, shut down gracefully
//! subscription.stop().await?;
//! ```
//!
//! [`Repository::load_projection`]: crate::repository::Repository::load_projection

use std::{collections::HashMap, future::Future, pin::Pin};

use futures_core::Stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::{
    event::EventDecodeError,
    projection::Projection,
    snapshot::{Snapshot, SnapshotStore},
    store::{EventFilter, EventStore, StoredEvent},
};

/// Capacity of the per-subscription broadcast channel backing
/// [`Subscription::updates`]. A consumer that falls this far behind skips the
/// intervening states (and can resync via [`Subscription::current`]) rather
/// than stalling the runtime.
const UPDATES_CHANNEL_CAPACITY: usize = 1024;

/// A stored event paired with the [`Checkpoint`](SubscribableStore::Checkpoint)
/// that marks its place in the commit-ordered delivery stream.
///
/// The `event` is the ordinary [`StoredEvent`]; the `checkpoint` is the opaque,
/// ordered cursor a subscriber persists to resume later.
pub struct Checkpointed<S: SubscribableStore + ?Sized> {
    /// Cursor marking this event's position in the delivery stream.
    pub checkpoint: S::Checkpoint,
    /// The delivered event.
    pub event: StoredEvent<
        <S as EventStore>::Id,
        <S as EventStore>::Position,
        <S as EventStore>::Data,
        <S as EventStore>::Metadata,
    >,
}

/// An item yielded by [`SubscribableStore::subscribe`]: either a matching event
/// or a global progress frontier.
///
/// Server-side filtering means non-matching events never reach the subscriber,
/// so their checkpoints cannot be observed from the event stream. [`Frontier`]
/// closes that gap: it reports the highest *stable* global checkpoint through
/// which the store guarantees no (further) matching event exists, letting a
/// subscriber advance a global progress cursor across filtered-out events. This
/// is what makes [`Subscription::wait_for`] resolve a global
/// [`ConsistencyToken`] instead of stalling when a write does not touch the
/// projection's filter.
///
/// A `Frontier` carries no event and must not apply a handler, emit an update,
/// or trigger a snapshot — it only advances global progress.
///
/// [`Frontier`]: Delivery::Frontier
pub enum Delivery<S: SubscribableStore + ?Sized> {
    /// A filter-matching event at its delivery checkpoint.
    Event(Checkpointed<S>),
    /// No (further) matching event exists through this stable global
    /// checkpoint; advance global progress to here.
    Frontier(S::Checkpoint),
}

/// An opaque, ordered marker for a point in a store's commit-ordered delivery
/// stream, used to bridge CQRS eventual consistency into read-your-writes.
///
/// A token is produced *after a write* by
/// [`Repository::consistency_token`](crate::repository::Repository::consistency_token)
/// and consumed by [`Subscription::wait_for`], which blocks until a read-side
/// subscription has processed at least up to that point. Awaiting a token
/// before querying a subscription-fed read model guarantees the model reflects
/// the write — turning an eventually-consistent projection into a monotonic,
/// read-your-writes one on demand.
///
/// The token wraps a store's [`Checkpoint`](SubscribableStore::Checkpoint), so
/// it inherits the checkpoint's total order. To await several writes at once,
/// keep the largest (`tokens.into_iter().max()`). It is [`Serialize`]/
/// [`DeserializeOwned`] so it can be returned to a client and presented on a
/// later read, extending the guarantee across a process boundary.
///
/// # On-demand projections do not need this
///
/// [`Repository::load_projection`](crate::repository::Repository::load_projection)
/// rebuilds from the event stream on each call and is already strongly
/// consistent with prior writes. Tokens exist for *subscription-fed* read
/// models
/// (see [`Repository::subscribe`](crate::repository::Repository::subscribe)),
/// which are the only place the eventual-consistency window is observable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConsistencyToken<C>(C);

impl<C> ConsistencyToken<C> {
    /// Wrap a checkpoint as a consistency token.
    pub const fn new(checkpoint: C) -> Self {
        Self(checkpoint)
    }

    /// Borrow the underlying checkpoint.
    pub const fn checkpoint(&self) -> &C {
        &self.0
    }

    /// Unwrap into the underlying checkpoint.
    pub fn into_checkpoint(self) -> C {
        self.0
    }
}

/// Error returned by [`Subscription::wait_for`] when the requested checkpoint
/// cannot be awaited.
#[derive(Debug, Error)]
pub enum AwaitError {
    /// The subscription stopped (gracefully or due to a failure) before
    /// processing up to the requested checkpoint.
    ///
    /// To recover the underlying failure, call
    /// [`Subscription::stop`](Subscription::stop) and inspect its error.
    #[error("subscription stopped before reaching the requested checkpoint")]
    SubscriptionStopped,
}

/// A store that supports push-based event subscriptions.
///
/// Extends [`EventStore`] with a `subscribe` method that returns a stream of
/// events. The stream replays historical events first, then yields live events
/// as they are committed.
///
/// This is a separate trait (not on [`EventStore`] directly) because not all
/// stores support push notifications. The in-memory store uses
/// `tokio::sync::broadcast`; a `PostgreSQL` implementation uses `LISTEN/NOTIFY`
/// plus transaction-id stability gating.
///
/// Subscription delivery order comes from [`Checkpoint`](Self::Checkpoint), not
/// from [`EventStore::Position`], so a store with per-stream (or `()`)
/// positions can still be subscribable.
///
/// # Checkpoints versus positions
///
/// A subscription cursor is **not** an [`EventStore::Position`]. `Position` is
/// the per-stream version assigned at write time; it is dense per stream but
/// not a safe global subscription cursor on stores where positions become
/// visible out of assignment order (e.g. `BIGSERIAL` under concurrent commits).
/// [`Checkpoint`](Self::Checkpoint) is a store-defined, opaque, ordered cursor
/// that advances in *delivery* order and is gap-free for resumption.
// ANCHOR: subscribable_store_trait
pub trait SubscribableStore: EventStore {
    /// Opaque, ordered cursor into the commit-ordered delivery stream.
    ///
    /// The store guarantees that checkpoints emitted by [`subscribe`] are
    /// strictly increasing in delivery order, so a subscriber can deduplicate
    /// and resume using a single `Checkpoint` value.
    ///
    /// [`subscribe`]: Self::subscribe
    type Checkpoint: Clone + Ord + Send + Sync + Serialize + DeserializeOwned + 'static;

    /// Subscribe to events matching the given filters.
    ///
    /// Returns a stream that:
    /// 1. Yields all historical events after `from` (catch-up phase)
    /// 2. Yields live events as they are committed (live phase)
    ///
    /// `from` is **exclusive**: the stream yields events strictly *after* the
    /// given checkpoint. Pass `None` to start from the beginning.
    ///
    /// **Delivery guarantee**: matching events ([`Delivery::Event`]) are
    /// delivered at-least-once in strictly increasing
    /// [`Checkpoint`](Self::Checkpoint) order. The stream may yield duplicate
    /// events during the catch-up-to-live transition; the subscription loop
    /// deduplicates by checkpoint.
    ///
    /// **Frontier guarantee**: whenever the stream has delivered every matching
    /// event through some stable global checkpoint `F` and is about to idle, it
    /// yields [`Delivery::Frontier(F)`](Delivery::Frontier). `F` must be chosen
    /// so that no matching event with checkpoint `<= F` can ever be delivered
    /// afterwards (i.e. `F` is below the store's stability boundary). This lets
    /// subscribers advance a *global* progress cursor across filtered-out
    /// events, which [`Subscription::wait_for`] relies on. Stores that
    /// cannot cheaply compute a frontier may omit it, at the cost of
    /// `wait_for` stalling for tokens beyond the projection's own events.
    fn subscribe(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
        from: Option<Self::Checkpoint>,
    ) -> Pin<Box<impl Stream<Item = Result<Delivery<Self>, Self::Error>> + Send + '_>>;

    /// The largest checkpoint currently reachable by catch-up for `filters`.
    ///
    /// Used to decide when a freshly started subscription has finished
    /// replaying history. Returns `None` when no matching events exist.
    ///
    /// # Errors
    ///
    /// Returns a store-specific error when the query fails.
    fn current_checkpoint(
        &self,
        filters: &[EventFilter<Self::Id, Self::Position>],
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send;

    /// The latest committed [`Checkpoint`](Self::Checkpoint) across *all*
    /// events, ignoring filters; `None` when the store is empty.
    ///
    /// This mints a global, projection-independent read-your-writes token: call
    /// it after a write to capture "the log now includes my commit", then await
    /// the token on any subscription via [`Subscription::wait_for`]. Unlike
    /// [`current_checkpoint`](Self::current_checkpoint), it is unfiltered, so
    /// the token is meaningful regardless of which projection later
    /// consumes it.
    ///
    /// The returned checkpoint must be one a subscriber's global progress
    /// cursor can eventually reach — i.e. the caller's just-committed
    /// events are included and become stable. It need not itself be below
    /// the stability boundary at call time; it only has to become so.
    ///
    /// # Errors
    ///
    /// Returns a store-specific error when the query fails.
    fn latest_checkpoint(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send;

    /// Resolve the exact [`Checkpoint`](Self::Checkpoint) of the event
    /// committed at `position` (the `last_position` returned by a commit).
    ///
    /// This mints an *exact* read-your-writes token bound to a specific write,
    /// tighter than [`latest_checkpoint`](Self::latest_checkpoint): it names
    /// precisely the caller's commit rather than the global head, so a
    /// subscriber awaiting it never blocks on unrelated concurrent writes.
    /// Returns `None` when no event exists at `position`.
    ///
    /// # Errors
    ///
    /// Returns a store-specific error when the query fails.
    fn checkpoint_for_position(
        &self,
        position: &Self::Position,
    ) -> impl Future<Output = Result<Option<Self::Checkpoint>, Self::Error>> + Send;
}
// ANCHOR_END: subscribable_store_trait

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

/// A running subscription: a live read model plus an opt-in stream of updates.
///
/// Read the materialised state at any time with [`current`](Self::current), or
/// await a write with [`wait_for`](Self::wait_for) /
/// [`read_after`](Self::read_after). For reactive consumers,
/// [`updates`](Self::updates) hands back the current snapshot together with a
/// [`Stream`] of every subsequent state, taken atomically so no update is lost
/// or duplicated across the boundary. Call [`stop()`] for graceful shutdown.
///
/// [`stop()`]: Self::stop
pub struct Subscription<P, Cp, StoreError>
where
    StoreError: std::error::Error + 'static,
{
    /// Broadcasts each applied projection state to live [`updates`] streams.
    /// The runtime only sends when there is at least one receiver, so a
    /// subscription used purely for [`current`]/[`wait_for`] never queues
    /// anything.
    ///
    /// [`updates`]: Self::updates
    /// [`current`]: Self::current
    /// [`wait_for`]: Self::wait_for
    updates_tx: broadcast::Sender<P>,
    /// Latest checkpoint the runtime has processed (`None` until the first
    /// event). Drives [`processed`](Self::processed) and
    /// [`wait_for`](Self::wait_for); closes when the runtime task ends.
    progress: tokio::sync::watch::Receiver<Option<Cp>>,
    /// Latest projection state the runtime has applied. Seeded with the resume
    /// baseline and republished after every event, so
    /// [`current`](Self::current) reads the live read model without
    /// subscribing to the update stream.
    ///
    /// A plain `Arc<Mutex<P>>` rather than a `watch` channel: the latter's
    /// `Receiver<P>` would require `P: Sync` for the subscription to stay
    /// `Sync`, tightening the bound on [`wait_for`](Self::wait_for). The
    /// mutex is only ever held to clone in/out, never across an `.await`.
    current: std::sync::Arc<std::sync::Mutex<P>>,
    handle: SubscriptionHandle<StoreError>,
}

impl<P, Cp, StoreError> Subscription<P, Cp, StoreError>
where
    StoreError: std::error::Error + 'static,
{
    /// Stop the subscription gracefully and wait for it to finish.
    ///
    /// # Errors
    ///
    /// Returns the subscription's error if it failed before being stopped.
    pub async fn stop(self) -> Result<(), SubscriptionError<StoreError>>
    where
        P: Send,
        Cp: Send + Sync,
        StoreError: Send,
    {
        self.handle.stop().await
    }

    /// Check if the subscription task is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.handle.is_running()
    }

    /// The latest checkpoint this subscription has processed, or `None` if it
    /// has not yet applied any event past its starting point.
    ///
    /// This is a non-blocking snapshot of read-side progress. To *wait* for a
    /// specific point, use [`wait_for`](Self::wait_for).
    #[must_use]
    pub fn processed(&self) -> Option<Cp>
    where
        Cp: Clone,
    {
        self.progress.borrow().clone()
    }

    /// The latest projection state the subscription has applied.
    ///
    /// A non-blocking read of the live read model that does **not** consume the
    /// update stream, so it composes with [`wait_for`](Self::wait_for): await a
    /// write's [`ConsistencyToken`], then call `current()` to observe a read
    /// model guaranteed to reflect that write.
    ///
    /// # Panics
    ///
    /// Panics only if the background task panicked while updating the
    /// projection (poisoning the internal mutex); a healthy subscription
    /// never panics here.
    #[must_use]
    pub fn current(&self) -> P
    where
        P: Clone,
    {
        self.current
            .lock()
            .expect("projection mutex poisoned")
            .clone()
    }

    /// Subscribe to live projection updates, returning the current state and a
    /// [`Stream`] of every state that follows.
    ///
    /// The snapshot and the stream are taken atomically: every event is on
    /// exactly one side of the boundary — folded into the returned snapshot, or
    /// yielded by the stream — so none is lost or duplicated. This avoids the
    /// snapshot-then-subscribe race of reading [`current`](Self::current) and
    /// subscribing separately. Events processed before this call (including the
    /// catch-up phase) are reflected in the snapshot, not replayed through the
    /// stream.
    ///
    /// Each stream item is the projection state after one event. A consumer
    /// that falls behind the channel's capacity skips the intervening states
    /// rather than blocking the runtime; call [`current`](Self::current) to
    /// resynchronise. Ignore the snapshot (`let (_, stream) = sub.updates();`)
    /// for a pure live-delta stream.
    ///
    /// # Panics
    ///
    /// Panics only if the background task panicked while updating the
    /// projection (poisoning the internal mutex); a healthy subscription never
    /// panics here.
    pub fn updates(&self) -> (P, impl Stream<Item = P> + Send + 'static)
    where
        P: Clone + Send + 'static,
    {
        // Subscribe and snapshot under the same lock the runtime holds when it
        // republishes state and broadcasts (see `handle_delivery`), so the
        // snapshot/stream boundary is exact: an event folded into `snapshot`
        // was broadcast before this `subscribe`, and any later event is
        // delivered by the returned stream.
        let guard = self.current.lock().expect("projection mutex poisoned");
        let stream = BroadcastStream::new(self.updates_tx.subscribe()).filter_map(Result::ok);
        (guard.clone(), stream)
    }

    /// Wait for `token`, then return the latest projection state.
    ///
    /// This is the convenience read-your-writes path for subscription-fed read
    /// models: pass the optional token returned by `create_tracked`,
    /// `update_tracked`, `upsert_tracked`, or `consistency_token`, and this
    /// method waits only when there is something to wait for. It then returns
    /// [`current`](Self::current) without consuming the update stream.
    ///
    /// Use [`wait_for`](Self::wait_for) directly when you need only the
    /// progress barrier, or when you want to await several tokens before
    /// performing one or more reads.
    ///
    /// # Errors
    ///
    /// Returns [`AwaitError::SubscriptionStopped`] if the runtime stops before
    /// reaching the requested token.
    ///
    /// # Panics
    ///
    /// Panics only if the background task panicked while updating the
    /// projection (poisoning the internal mutex); a healthy subscription
    /// never panics here.
    pub async fn read_after(&self, token: Option<ConsistencyToken<Cp>>) -> Result<P, AwaitError>
    where
        P: Clone + Send,
        Cp: PartialOrd + Send + Sync,
        StoreError: Send,
    {
        if let Some(token) = token {
            self.wait_for(token).await?;
        }

        Ok(self.current())
    }

    /// Wait until this subscription has processed at least up to `token`.
    ///
    /// This bridges CQRS eventual consistency into a read-your-writes
    /// guarantee: after a write, obtain a token from
    /// [`Repository::consistency_token`](crate::repository::Repository::consistency_token),
    /// await it here, then query the read model — it will reflect the write.
    ///
    /// Resolves immediately if the subscription has already advanced past
    /// `token` (including writes this projection does not subscribe to, whose
    /// tokens never exceed the latest relevant checkpoint). The returned future
    /// carries no timeout; wrap it in `tokio::time::timeout` or `select!` to
    /// bound the wait. Waking is event-driven (no polling): the future is
    /// notified only when the runtime advances.
    ///
    /// # Errors
    ///
    /// Returns [`AwaitError::SubscriptionStopped`] if the runtime stops before
    /// reaching `token`. Call [`stop`](Self::stop) afterwards to surface the
    /// underlying failure, if any.
    pub async fn wait_for(&self, token: ConsistencyToken<Cp>) -> Result<(), AwaitError>
    where
        P: Send,
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
    SS: SnapshotStore<P::InstanceId, Position = S::Checkpoint> + Send + Sync + 'static,
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
    /// 1. Loads the most recent snapshot and its checkpoint (if available)
    /// 2. Subscribes to the event stream from that checkpoint
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
    pub async fn start(
        self,
    ) -> Result<Subscription<P, S::Checkpoint, S::Error>, SubscriptionError<S::Error>>
    where
        P: Clone,
    {
        let Self {
            store,
            snapshots,
            instance_id,
        } = self;

        // Opt-in live update channel. The initial receiver is dropped: the
        // runtime only broadcasts when a consumer has subscribed via
        // `Subscription::updates`, so a subscription used purely for
        // `current()`/`wait_for()` never clones or queues a single state.
        let (updates_tx, _) = broadcast::channel(UPDATES_CHANNEL_CAPACITY);

        // Resume from the persisted checkpoint, if any.
        let (projection, from_checkpoint) = load_snapshot::<P, SS>(&snapshots, &instance_id).await;

        // Live read model: seeded with the resume baseline and republished after
        // each event so `current()` reads state without draining the update
        // stream. Shared with the background task via `Arc<Mutex<_>>`.
        let current = std::sync::Arc::new(std::sync::Mutex::new(projection.clone()));

        // Read-your-writes progress channel. Seeded with the resume checkpoint
        // so `processed()`/`wait_for()` account for the snapshot baseline. The
        // background task advances it as it processes events; when the task
        // ends the sender drops, waking any waiters with `SubscriptionStopped`.
        let (progress_tx, progress_rx) = tokio::sync::watch::channel(from_checkpoint.clone());

        // Filters are position-free: the subscription cursor is the checkpoint,
        // passed to `subscribe` directly rather than baked into the filters.
        let filters = P::filters::<S>(&instance_id);
        let (event_filters, handlers) = filters.into_event_filters(None);

        // Snapshot the catch-up target before subscribing.
        let catchup_target = store
            .current_checkpoint(&event_filters)
            .await
            .map_err(SubscriptionError::Store)?;

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        // `caught_up` is true from the start when there is nothing to replay; the
        // catch-up and live phases otherwise share a single loop in `run`.
        let caught_up = catchup_target.is_none() || from_checkpoint >= catchup_target;

        let subscription_loop = SubscriptionLoop {
            snapshots,
            instance_id,
            handlers,
            projection,
            updates_tx: updates_tx.clone(),
            progress_tx,
            current: std::sync::Arc::clone(&current),
            catchup_target,
            // `last_checkpoint` tracks matching events; `observed` is the global
            // progress cursor (max of matching checkpoints and frontiers). They
            // start equal at the resume point but diverge as filtered-out writes
            // advance `observed` for `wait_for` without moving `last_checkpoint`.
            last_checkpoint: from_checkpoint.clone(),
            observed: from_checkpoint.clone(),
            events_since_snapshot: 0,
            caught_up,
        };

        let task = tokio::spawn(subscription_loop.run(
            store,
            event_filters,
            from_checkpoint,
            stop_rx,
            ready_tx,
        ));

        match ready_rx.await {
            Ok(()) => Ok(Subscription {
                updates_tx,
                progress: progress_rx,
                current,
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

/// Owned state for the background subscription loop, factored out of
/// [`SubscriptionBuilder::start`] so that method stays focused on wiring up
/// channels while the catch-up/live loop lives here.
///
/// `store` and the stream derived from it are intentionally *not* held here:
/// the stream borrows the store for its whole lifetime, so keeping the store as
/// a separate `run` parameter lets the per-event handler take `&mut self`
/// without conflicting with that borrow.
struct SubscriptionLoop<S, P, SS>
where
    S: SubscribableStore,
    P: Projection<Id = S::Id>,
{
    snapshots: SS,
    instance_id: P::InstanceId,
    handlers: HashMap<&'static str, crate::projection::EventHandler<P, S>>,
    projection: P,
    updates_tx: broadcast::Sender<P>,
    progress_tx: tokio::sync::watch::Sender<Option<S::Checkpoint>>,
    current: std::sync::Arc<std::sync::Mutex<P>>,
    catchup_target: Option<S::Checkpoint>,
    last_checkpoint: Option<S::Checkpoint>,
    observed: Option<S::Checkpoint>,
    events_since_snapshot: u64,
    caught_up: bool,
}

impl<S, P, SS> SubscriptionLoop<S, P, SS>
where
    S: SubscribableStore + Clone + Send + Sync + 'static,
    S::Data: Send,
    S::Metadata: Send + Sync,
    P: Projection<Id = S::Id, Metadata = S::Metadata> + Clone + Serialize + Send + Sync + 'static,
    P::InstanceId: Send + Sync + 'static,
    P::Metadata: Send,
    SS: SnapshotStore<P::InstanceId, Position = S::Checkpoint> + Send + Sync + 'static,
{
    /// Drive the shared catch-up + live loop until stopped or the stream ends,
    /// taking a final snapshot on the way out.
    async fn run(
        mut self,
        store: S,
        event_filters: Vec<EventFilter<S::Id, S::Position>>,
        from_checkpoint: Option<S::Checkpoint>,
        mut stop_rx: tokio::sync::oneshot::Receiver<()>,
        ready_tx: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(), SubscriptionError<S::Error>> {
        let mut ready_tx = Some(ready_tx);
        if self.caught_up {
            signal(&mut ready_tx);
        }

        let mut stream = store.subscribe(&event_filters, from_checkpoint);

        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    tracing::debug!("subscription stopped");
                    break;
                }
                item = stream.next() => {
                    let Some(result) = item else {
                        tracing::debug!("subscription stream ended");
                        break;
                    };
                    self.handle_delivery(result, &store, &mut ready_tx).await?;
                }
            }
        }

        // Final snapshot on shutdown.
        maintain_snapshot(
            &self.snapshots,
            &self.instance_id,
            self.last_checkpoint.as_ref(),
            &mut self.events_since_snapshot,
            &self.projection,
        )
        .await;

        Ok(())
    }

    /// Apply one stream item: advance the global cursor on a frontier, or
    /// deduplicate, dispatch, advance progress, signal catch-up readiness, and
    /// offer a snapshot on a matching event.
    async fn handle_delivery(
        &mut self,
        result: Result<Delivery<S>, S::Error>,
        store: &S,
        ready_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(), SubscriptionError<S::Error>> {
        let checkpoint = match result.map_err(SubscriptionError::Store)? {
            Delivery::Frontier(frontier) => {
                // No event: advance global progress only.
                advance_observed(&mut self.observed, &self.progress_tx, frontier);
                return Ok(());
            }
            Delivery::Event(delivery) => {
                // Checkpoint-based deduplication (delivery order is strictly
                // increasing in checkpoint).
                if let Some(ref lc) = self.last_checkpoint
                    && delivery.checkpoint <= *lc
                {
                    return Ok(());
                }

                let checkpoint = delivery.checkpoint.clone();
                process_subscription_event(
                    &mut self.projection,
                    delivery,
                    &self.handlers,
                    store,
                    &mut self.last_checkpoint,
                    &mut self.events_since_snapshot,
                )?;
                // Publish the new state and broadcast it under one lock. Holding
                // the lock across both makes `updates()` (which subscribes and
                // snapshots under the same lock) see an exact boundary, and
                // doing it before advancing progress (below) ensures a
                // `wait_for` waiter that wakes on the checkpoint always observes
                // a `current()` that includes it. The broadcast is skipped when
                // no consumer is subscribed, so a `current()`-only subscription
                // never clones state for the stream.
                {
                    let mut current = self.current.lock().expect("projection mutex poisoned");
                    *current = self.projection.clone();
                    if self.updates_tx.receiver_count() > 0 {
                        // Clone through the guard so the broadcast provably
                        // happens under the same lock `updates()` holds.
                        let _ = self.updates_tx.send(current.clone());
                    }
                }
                checkpoint
            }
        };

        // Advance read-your-writes progress before the (possibly slow) snapshot
        // offer, so waiters wake as early as possible.
        advance_observed(&mut self.observed, &self.progress_tx, checkpoint);

        // Signal readiness once we've replayed up to the target.
        if !self.caught_up
            && (self.catchup_target.is_none() || self.last_checkpoint >= self.catchup_target)
        {
            self.caught_up = true;
            signal(ready_tx);
        }

        // Offer a snapshot after every event; the snapshot store's own policy
        // decides whether to persist.
        maintain_snapshot(
            &self.snapshots,
            &self.instance_id,
            self.last_checkpoint.as_ref(),
            &mut self.events_since_snapshot,
            &self.projection,
        )
        .await;

        Ok(())
    }
}

/// Signal one-time catch-up readiness, consuming the sender so later calls are
/// no-ops. A dropped receiver is ignored.
fn signal(ready_tx: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(());
    }
}

/// Advance `observed` to `candidate` when it is greater, publishing the new
/// global progress cursor to [`Subscription::wait_for`] waiters. `send_replace`
/// ignores a dropped receiver.
fn advance_observed<Cp>(
    observed: &mut Option<Cp>,
    progress_tx: &tokio::sync::watch::Sender<Option<Cp>>,
    candidate: Cp,
) where
    Cp: Ord + Clone,
{
    if observed.as_ref().is_none_or(|current| candidate > *current) {
        *observed = Some(candidate);
        progress_tx.send_replace(observed.clone());
    }
}

/// Load the latest projection snapshot, falling back to `P::init`.
///
/// Snapshot load failures are logged and treated as cache misses so the caller
/// can still start from event replay.
pub(crate) async fn load_snapshot<P, SS>(
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
    .map_err(|error| SubscriptionError::EventDecode(error.into_decode_error()))
}

/// Process one event through handler dispatch and checkpoint tracking.
///
/// Publishing the new state (to `current()` and the live update stream) is the
/// caller's responsibility, so it can do so atomically under the projection
/// lock.
fn process_subscription_event<P, S>(
    projection: &mut P,
    delivery: Checkpointed<S>,
    handlers: &HashMap<&'static str, crate::projection::EventHandler<P, S>>,
    store: &S,
    last_checkpoint: &mut Option<S::Checkpoint>,
    events_since_snapshot: &mut u64,
) -> Result<(), SubscriptionError<S::Error>>
where
    P: Projection<Id = S::Id>,
    S: SubscribableStore,
{
    let Checkpointed { checkpoint, event } = delivery;

    // Unknown kinds are ignored: filters can intentionally include wider sets
    // than this projection currently handles.
    if let Some(handler) = handlers.get(event.kind()) {
        apply_handler(handler, projection, &event, store)?;
    }

    *last_checkpoint = Some(checkpoint);
    *events_since_snapshot += 1;

    Ok(())
}

/// Offer a snapshot for the current projection state, resetting the pending
/// counter when the store accepts it. No-op when no events are pending.
async fn maintain_snapshot<P, SS>(
    snapshots: &SS,
    instance_id: &P::InstanceId,
    last_checkpoint: Option<&SS::Position>,
    events_since_snapshot: &mut u64,
    projection: &P,
) where
    P: Projection + Serialize + Sync,
    P::InstanceId: Sync,
    SS: SnapshotStore<P::InstanceId>,
    SS::Position: Clone,
{
    if *events_since_snapshot > 0
        && let Some(cp) = last_checkpoint
        && offer_projection_snapshot(
            snapshots,
            instance_id,
            *events_since_snapshot,
            cp,
            projection,
        )
        .await
    {
        *events_since_snapshot = 0;
    }
}

/// Offer a projection snapshot and report whether it was stored.
///
/// Errors are logged and treated as "not stored" so the caller remains live.
pub(crate) async fn offer_projection_snapshot<P, SS>(
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
            tracing::warn!(error = %e, "failed to store projection snapshot");
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
