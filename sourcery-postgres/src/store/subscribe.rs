use serde::{Serialize, de::DeserializeOwned};
use sourcery_core::{store::EventFilter, subscription::EventStream};
use sqlx::postgres::PgListener;

use super::Store;
use crate::Error;

impl<M> Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// Build a live [`EventStream`] backed by a `LISTEN`/`NOTIFY` channel.
    ///
    /// The stream first replays any historical events since `from_position`,
    /// then switches to live delivery as new events are committed.
    ///
    /// When `filters` is `Some`, only matching event kinds are returned;
    /// `None` subscribes to all events.
    pub(in crate::store) fn subscribe_with_listener(
        &self,
        from_position: Option<i64>,
        filters: Option<Vec<EventFilter<uuid::Uuid, i64>>>,
    ) -> EventStream<'_, Self> {
        let store = self.clone();

        Box::pin(async_stream::stream! {
            let mut last_position = from_position;
            let mut listener = match PgListener::connect_with(&store.pool).await {
                Ok(listener) => listener,
                Err(error) => {
                    yield Err(Error::Database(error));
                    return;
                }
            };

            if let Err(error) = listener.listen(Self::EVENTS_NOTIFY_CHANNEL).await {
                yield Err(Error::Database(error));
                return;
            }

            // Historical catch-up.
            let historical = if let Some(filters) = filters.as_deref() {
                store.load_events_since_filtered(filters, last_position).await
            } else {
                store.load_all_events_since(last_position).await
            };
            match historical {
                Ok(events) => {
                    for event in events {
                        if let Some(ref lp) = last_position
                            && event.position <= *lp
                        {
                            continue;
                        }
                        last_position = Some(event.position);
                        yield Ok(event);
                    }
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }

            // Live notifications.
            loop {
                let notif = match listener.recv().await {
                    Ok(notif) => notif,
                    Err(error) => {
                        yield Err(Error::Database(error));
                        return;
                    }
                };

                // The notification payload is the first (minimum) position of
                // the just-committed batch. If that position is below our
                // current watermark, a lower-positioned transaction committed
                // after a higher-positioned one (possible because PostgreSQL
                // sequences are allocated before commit). Re-querying from
                // just before it ensures we don't permanently miss the event.
                let notif_first_pos: i64 = notif.payload().parse().unwrap_or(i64::MAX);
                let query_from =
                    last_position.map(|lp| lp.min(notif_first_pos.saturating_sub(1)));

                let loaded = if let Some(filters) = filters.as_deref() {
                    store.load_events_since_filtered(filters, query_from).await
                } else {
                    store.load_all_events_since(query_from).await
                };
                match loaded {
                    Ok(events) => {
                        for event in events {
                            // Skip events already yielded in a previous batch
                            // (possible when re-querying from a lower position).
                            if let Some(lp) = last_position
                                && event.position <= lp
                            {
                                continue;
                            }
                            last_position = Some(event.position);
                            yield Ok(event);
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
        })
    }
}
