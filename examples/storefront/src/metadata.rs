//! Custom event metadata: [`RequestContext`].
//!
//! Infrastructure concerns stay out of the pure domain events and travel
//! alongside them as the store's `Metadata` type. One `RequestContext` is
//! shared by every aggregate and projection in the store.
//!
//! `correlation_id` names the whole flow and `causation_id` names the single
//! message that directly caused this one. These are for tracing and audit —
//! they are **not** domain join keys. A projection computing lifetime spend
//! joins on the payload's `customer_id`, never on `correlation_id`.
//!
//! `RequestContext` derives `Clone` and is `Send + Sync` because command
//! methods require `S::Metadata: Clone` and subscriptions/reactors require
//! `S::Metadata: Send + Sync`. It also derives `Serialize`/`Deserialize`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Infrastructure envelope committed alongside every event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    /// Names the whole flow (a conversation), for tracing across commands.
    pub correlation_id: String,
    /// Names the single message that directly caused this one, if any.
    pub causation_id: Option<String>,
    /// Who or what issued the command (a user, service, or the coordinator).
    pub actor: String,
    /// Wall-clock time the context was created, in epoch milliseconds.
    pub at_millis: u64,
}

impl RequestContext {
    /// A fresh root context for an externally initiated request, with a new
    /// correlation id and no causation.
    #[must_use]
    pub fn new(actor: impl Into<String>) -> Self {
        Self {
            correlation_id: Uuid::new_v4().to_string(),
            causation_id: None,
            actor: actor.into(),
            at_millis: now_millis(),
        }
    }

    /// Derive the context for a command issued *in reaction* to an event.
    ///
    /// The correlation id is propagated (same flow); the causation id names the
    /// triggering event. This is how the coordinator stamps causal lineage when
    /// it turns an event into a command.
    #[must_use]
    pub fn caused_by(&self, actor: impl Into<String>, causation_id: impl Into<String>) -> Self {
        Self {
            correlation_id: self.correlation_id.clone(),
            causation_id: Some(causation_id.into()),
            actor: actor.into(),
            at_millis: now_millis(),
        }
    }

    /// The day this context was created (epoch day number), used by
    /// `RevenueDashboard` to bucket revenue per day.
    #[must_use]
    pub const fn epoch_day(&self) -> u64 {
        self.at_millis / 86_400_000
    }
}

/// Milliseconds since the Unix epoch (saturating to zero before 1970).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
