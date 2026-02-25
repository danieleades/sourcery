use std::collections::BTreeMap;

use serde::{Serialize, de::DeserializeOwned};
use sqlx::postgres::PgListener;
use tokio::{
    sync::{Mutex, broadcast},
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};

use super::{PgStoredEvent, Store};
use crate::Error;

const LIVE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LIVE_BUFFER_CAPACITY: usize = 8192;

pub(in crate::store) enum LiveMessage<M> {
    Event(std::sync::Arc<PgStoredEvent<M>>),
}

impl<M> Clone for LiveMessage<M> {
    fn clone(&self) -> Self {
        match self {
            Self::Event(event) => Self::Event(std::sync::Arc::clone(event)),
        }
    }
}

#[derive(Clone)]
pub(in crate::store) struct LivePump<M> {
    inner: std::sync::Arc<LivePumpInner<M>>,
}

struct LivePumpInner<M> {
    pool: sqlx::PgPool,
    sender: broadcast::Sender<LiveMessage<M>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl<M> LivePump<M> {
    #[must_use]
    pub(in crate::store) fn new(pool: sqlx::PgPool) -> Self {
        let (sender, _) = broadcast::channel(LIVE_BUFFER_CAPACITY);
        Self {
            inner: std::sync::Arc::new(LivePumpInner {
                pool,
                sender,
                task: Mutex::new(None),
            }),
        }
    }
}

impl<M> LivePump<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    pub(in crate::store) async fn subscribe(
        &self,
        from_position: Option<i64>,
    ) -> broadcast::Receiver<LiveMessage<M>> {
        self.ensure_running(from_position).await;
        self.inner.sender.subscribe()
    }

    async fn ensure_running(&self, from_position: Option<i64>) {
        let mut task_guard = self.inner.task.lock().await;
        if task_guard.is_some() {
            return;
        }

        let pool = self.inner.pool.clone();
        let sender = self.inner.sender.clone();
        let start_from = from_position.unwrap_or(0).max(0);

        *task_guard = Some(tokio::spawn(async move {
            run_live_pump::<M>(pool, sender, start_from).await;
        }));
    }
}

async fn run_live_pump<M>(
    pool: sqlx::PgPool,
    sender: broadcast::Sender<LiveMessage<M>>,
    mut watermark: i64,
) where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    // Contract: events are published strictly in ascending contiguous position
    // order from `watermark + 1` onwards. If a position never materialises,
    // publication remains paused at that gap.
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("live pump failed to connect listener: {error}");
            return;
        }
    };

    if let Err(error) = listener.listen(Store::<M>::EVENTS_NOTIFY_CHANNEL).await {
        tracing::error!("live pump failed to listen on channel: {error}");
        return;
    }

    let mut pending: BTreeMap<i64, PgStoredEvent<M>> = BTreeMap::new();
    let mut poll_tick = tokio::time::interval(LIVE_POLL_INTERVAL);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll_tick.tick() => {
                if let Err(error) = poll_and_publish::<M>(&pool, &sender, &mut watermark, &mut pending, None).await {
                    tracing::error!("live pump polling failed: {error}");
                    return;
                }
            }
            recv = listener.recv() => {
                let notification = match recv {
                    Ok(notification) => notification,
                    Err(error) => {
                        tracing::error!("live pump listener receive failed: {error}");
                        return;
                    }
                };

                let notif_first_pos = notification.payload().parse::<i64>().ok();
                if let Err(error) = poll_and_publish::<M>(
                    &pool,
                    &sender,
                    &mut watermark,
                    &mut pending,
                    notif_first_pos,
                )
                .await
                {
                    tracing::error!("live pump notification handling failed: {error}");
                    return;
                }
            }
        }
    }
}

async fn poll_and_publish<M>(
    pool: &sqlx::PgPool,
    sender: &broadcast::Sender<LiveMessage<M>>,
    watermark: &mut i64,
    pending: &mut BTreeMap<i64, PgStoredEvent<M>>,
    notif_first_pos: Option<i64>,
) -> Result<(), Error>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    let rewind_from = notif_first_pos
        .map(|first| first.saturating_sub(1))
        .map_or(*watermark, |from| from.min(*watermark));
    let loaded = Store::<M>::load_all_events_since_with_pool(pool, Some(rewind_from)).await?;

    for event in loaded {
        if event.position > *watermark {
            pending.entry(event.position).or_insert(event);
        }
    }

    // Publish only the contiguous prefix after the watermark. This centralises
    // ordering logic and prevents a later lower-position commit from being
    // dropped or emitted out of order.
    loop {
        let expected = watermark.saturating_add(1);
        let Some(event) = pending.remove(&expected) else {
            break;
        };
        *watermark = event.position;
        let _ = sender.send(LiveMessage::Event(std::sync::Arc::new(event)));
    }

    Ok(())
}
