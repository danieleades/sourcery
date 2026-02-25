use serde::{Serialize, de::DeserializeOwned};
use sourcery_core::{store::EventFilter, subscription::EventStream};
use tokio::sync::broadcast;

use super::{Store, live::LiveMessage};

impl<M> Store<M>
where
    M: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// Build a live [`EventStream`] backed by the shared internal live pump.
    ///
    /// The stream first replays any historical events since `from_position`,
    /// then switches to live delivery as new events are committed.
    ///
    /// When `filters` is `Some`, only matching event kinds are returned;
    /// `None` subscribes to all events.
    pub(in crate::store) fn subscribe_with_live_pump(
        &self,
        from_position: Option<i64>,
        filters: Option<Vec<EventFilter<uuid::Uuid, i64>>>,
    ) -> EventStream<'_, Self> {
        let store = self.clone();

        Box::pin(async_stream::stream! {
            let mut last_position = from_position;
            let mut live = store.live.subscribe(from_position).await;

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

            // Shared live stream.
            loop {
                let next = match live.recv().await {
                    Ok(next) => next,
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let recovered = if let Some(filters) = filters.as_deref() {
                            store.load_events_since_filtered(filters, last_position).await
                        } else {
                            store.load_all_events_since(last_position).await
                        };

                        match recovered {
                            Ok(events) => {
                                for event in events {
                                    if let Some(lp) = last_position
                                        && event.position <= lp
                                    {
                                        continue;
                                    }
                                    last_position = Some(event.position);
                                    yield Ok(event);
                                }
                                continue;
                            }
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        }
                    }
                };

                let LiveMessage::Event(event) = next;

                if let Some(filters) = filters.as_deref()
                    && !event_matches_filters(&event, filters, from_position)
                {
                    continue;
                }

                if let Some(lp) = last_position
                    && event.position <= lp
                {
                    continue;
                }
                last_position = Some(event.position);
                yield Ok((*event).clone());
            }
        })
    }
}

fn event_matches_filters<M>(
    event: &sourcery_core::store::StoredEvent<uuid::Uuid, i64, serde_json::Value, M>,
    filters: &[EventFilter<uuid::Uuid, i64>],
    from_position: Option<i64>,
) -> bool {
    filters.iter().any(|filter| {
        if filter.event_kind != event.kind {
            return false;
        }

        if filter
            .aggregate_kind
            .as_ref()
            .is_some_and(|kind| kind != &event.aggregate_kind)
        {
            return false;
        }

        if filter
            .aggregate_id
            .as_ref()
            .is_some_and(|id| id != &event.aggregate_id)
        {
            return false;
        }

        let effective_after = match (from_position, filter.after_position) {
            (Some(from), Some(filter_after)) => Some(from.max(filter_after)),
            (Some(from), None) => Some(from),
            (None, Some(filter_after)) => Some(filter_after),
            (None, None) => None,
        };

        effective_after.is_none_or(|after| event.position > after)
    })
}
