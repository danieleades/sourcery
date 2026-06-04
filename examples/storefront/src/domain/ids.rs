//! Strongly typed domain identifiers and their projection to storage keys.
//!
//! Every aggregate keeps its own id newtype (`ProductId`, `OrderId`,
//! `PaymentId`) while the backing store uses a single raw key type (`String`).
//! Each newtype implements [`StorageKey<String>`], projecting to a prefixed key
//! such as `product:<uuid>`, `order:<uuid>`, or `payment:<uuid>`. The
//! repository accepts the typed id directly and converts at the boundary,
//! exactly like the command-side methods.
//!
//! `CustomerId` is *not* an aggregate id — there is no `Customer` aggregate. It
//! is a foreign domain key carried in event payloads and used as the instance
//! id of `CustomerAccountView`. It is a `String` newtype so it serialises
//! deterministically when used as a snapshot key.

use std::fmt;

use serde::{Deserialize, Serialize};
use sourcery::StorageKey;
use uuid::Uuid;

/// Identifies a `Product` aggregate instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProductId(pub Uuid);

/// Identifies an `Order` aggregate instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub Uuid);

/// Identifies a `Payment` aggregate instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaymentId(pub Uuid);

/// A foreign domain key naming a customer. Carried in event payloads and used
/// as the `CustomerAccountView` instance id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CustomerId(pub String);

impl ProductId {
    /// Mint a fresh product id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProductId {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderId {
    /// Mint a fresh order id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Recover an [`OrderId`] from a raw order storage key (`order:<uuid>`).
    ///
    /// The coordinator uses this to turn `EventContext.aggregate_id` (the raw
    /// `String` stream key) back into the typed id it needs to issue commands.
    #[must_use]
    pub fn from_storage_key(key: &str) -> Option<Self> {
        let uuid = key.strip_prefix("order:")?;
        Uuid::parse_str(uuid).ok().map(Self)
    }
}

impl Default for OrderId {
    fn default() -> Self {
        Self::new()
    }
}

impl PaymentId {
    /// Derive the payment id for an order.
    ///
    /// A `Payment` belongs to exactly one `Order`, so minting `PaymentId` from
    /// the order's uuid encodes the 1:1 link in the stream key itself: the
    /// payment event's `aggregate_id` is already the order join key, with no
    /// payload field needed.
    #[must_use]
    pub const fn for_order(order: OrderId) -> Self {
        Self(order.0)
    }

    /// Recover the owning [`OrderId`] from a raw payment storage key.
    ///
    /// [`StorageKey`] only projects a domain id to a storage key, never the
    /// reverse, so a read-side consumer that wants the typed parent id back
    /// parses the raw key string itself.
    #[must_use]
    pub fn order_from_storage_key(key: &str) -> Option<OrderId> {
        let uuid = key.strip_prefix("payment:")?;
        Uuid::parse_str(uuid).ok().map(OrderId)
    }
}

impl CustomerId {
    /// Construct a customer id from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl StorageKey<String> for ProductId {
    fn to_key(&self) -> String {
        format!("product:{}", self.0)
    }
}

impl StorageKey<String> for OrderId {
    fn to_key(&self) -> String {
        format!("order:{}", self.0)
    }
}

impl StorageKey<String> for PaymentId {
    fn to_key(&self) -> String {
        format!("payment:{}", self.0)
    }
}

impl fmt::Display for ProductId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "product:{}", self.0)
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "order:{}", self.0)
    }
}

impl fmt::Display for PaymentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payment:{}", self.0)
    }
}

impl fmt::Display for CustomerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
