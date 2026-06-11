//! The `Product` aggregate: stock on hand, reservations, replenishment.
//!
//! A product is a long-lived aggregate that accumulates many stock movements,
//! which is why the demo snapshots it (`with_snapshots`).
//!
//! Reservations are keyed by the *order* they belong to. A `Product` event
//! never carries `product_id` — the stream key *is* the product. It does carry
//! `order_id`, because that names a *different* aggregate the event refers to,
//! so that foreign key belongs in the payload.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sourcery::{Aggregate, Apply, Create, DomainEvent, Handle, HandleCreate};

use super::ids::{OrderId, ProductId};

// ---------------------------------------------------------------------------
// Events (`store.product.*`)
// ---------------------------------------------------------------------------

/// A new product was added to the catalogue with its opening stock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductListed {
    pub sku: String,
    pub name: String,
    pub initial_stock: u32,
    pub unit_price_cents: u64,
}

impl DomainEvent for ProductListed {
    const KIND: &'static str = "store.product.listed";
}

/// Stock was added to the shelf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockReplenished {
    pub quantity: u32,
}

impl DomainEvent for StockReplenished {
    const KIND: &'static str = "store.product.replenished";
}

/// Stock was reserved against an order (the foreign `order_id` lives in the
/// payload — it names another aggregate).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockReserved {
    pub order_id: OrderId,
    pub quantity: u32,
}

impl DomainEvent for StockReserved {
    const KIND: &'static str = "store.product.reserved";
}

/// A reservation was released without shipping (compensation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockReservationReleased {
    pub order_id: OrderId,
    pub quantity: u32,
}

impl DomainEvent for StockReservationReleased {
    const KIND: &'static str = "store.product.reservation-released";
}

/// A reservation was committed: stock leaves the shelf as the order ships.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockCommitted {
    pub order_id: OrderId,
    pub quantity: u32,
}

impl DomainEvent for StockCommitted {
    const KIND: &'static str = "store.product.committed";
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Reasons a product command can be rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProductError {
    /// Not enough free stock to satisfy a reservation.
    #[error("insufficient stock: {available} available, {requested} requested")]
    InsufficientStock { available: u32, requested: u32 },
    /// A quantity of zero is never a valid movement.
    #[error("quantity must be positive")]
    NonPositiveQuantity,
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// Stock-keeping aggregate for a single product.
///
/// `available = on_hand - reserved`. Reservations are tracked per order so the
/// reservation commands are idempotent under at-least-once reactor redelivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = ProductId,
    error = ProductError,
    kind = "store.product",
    events(
        ProductListed,
        StockReplenished,
        StockReserved,
        StockReservationReleased,
        StockCommitted
    ),
    create(ProductListed),
    derives(Debug, PartialEq, Eq)
)]
pub struct Product {
    sku: String,
    unit_price_cents: u64,
    on_hand: u32,
    /// Reservation quantity per owning order, keyed by the order's raw storage
    /// key. A `BTreeMap` keeps snapshot serialisation deterministic.
    reservations: BTreeMap<String, u32>,
}

impl Product {
    /// Total quantity currently reserved across all orders.
    #[must_use]
    pub fn reserved(&self) -> u32 {
        self.reservations.values().copied().sum()
    }

    /// Free stock that can still be reserved.
    #[must_use]
    pub fn available(&self) -> u32 {
        self.on_hand.saturating_sub(self.reserved())
    }

    /// On-hand stock (reserved + free).
    #[must_use]
    pub const fn on_hand(&self) -> u32 {
        self.on_hand
    }

    /// Catalogue unit price in cents.
    #[must_use]
    pub const fn unit_price_cents(&self) -> u64 {
        self.unit_price_cents
    }

    /// Whether this product holds a reservation for the given order.
    #[must_use]
    pub fn has_reservation_for(&self, order: OrderId) -> bool {
        self.reservations.contains_key(&order_key(order))
    }
}

/// Raw map key for an order's reservation.
fn order_key(order: OrderId) -> String {
    use sourcery::StorageKey;
    // Fully qualified: the blanket `StorageKey<T> for T` impl also applies to
    // `OrderId`, so the target raw type must be named explicitly.
    <OrderId as StorageKey<String>>::to_key(&order)
}

impl Create<ProductListed> for Product {
    fn create(event: &ProductListed) -> Self {
        Self {
            sku: event.sku.clone(),
            unit_price_cents: event.unit_price_cents,
            on_hand: event.initial_stock,
            reservations: BTreeMap::new(),
        }
    }
}

impl Apply<ProductListed> for Product {
    fn apply(&mut self, event: &ProductListed) {
        self.sku.clone_from(&event.sku);
        self.unit_price_cents = event.unit_price_cents;
        self.on_hand = event.initial_stock;
    }
}

impl Apply<StockReplenished> for Product {
    fn apply(&mut self, event: &StockReplenished) {
        self.on_hand += event.quantity;
    }
}

impl Apply<StockReserved> for Product {
    fn apply(&mut self, event: &StockReserved) {
        self.reservations
            .insert(order_key(event.order_id), event.quantity);
    }
}

impl Apply<StockReservationReleased> for Product {
    fn apply(&mut self, event: &StockReservationReleased) {
        self.reservations.remove(&order_key(event.order_id));
    }
}

impl Apply<StockCommitted> for Product {
    fn apply(&mut self, event: &StockCommitted) {
        if self
            .reservations
            .remove(&order_key(event.order_id))
            .is_some()
        {
            self.on_hand = self.on_hand.saturating_sub(event.quantity);
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Add a product to the catalogue (creation command).
#[derive(Clone, Debug)]
pub struct ListProduct {
    pub sku: String,
    pub name: String,
    pub initial_stock: u32,
    pub unit_price_cents: u64,
}

/// Add stock to the shelf.
#[derive(Clone, Debug)]
pub struct Replenish {
    pub quantity: u32,
}

/// Reserve stock against an order.
#[derive(Clone, Debug)]
pub struct Reserve {
    pub order_id: OrderId,
    pub quantity: u32,
}

/// Release a previously held reservation (compensation).
#[derive(Clone, Debug)]
pub struct ReleaseReservation {
    pub order_id: OrderId,
    pub quantity: u32,
}

/// Commit a reservation as the order ships.
#[derive(Clone, Debug)]
pub struct CommitReservation {
    pub order_id: OrderId,
    pub quantity: u32,
}

impl HandleCreate<ListProduct> for Product {
    type HandleCreateError = ProductError;

    fn handle_create(command: &ListProduct) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            ProductListed {
                sku: command.sku.clone(),
                name: command.name.clone(),
                initial_stock: command.initial_stock,
                unit_price_cents: command.unit_price_cents,
            }
            .into(),
        ])
    }
}

impl Handle<Replenish> for Product {
    type HandleError = ProductError;

    fn handle(&self, command: &Replenish) -> Result<Vec<Self::Event>, Self::HandleError> {
        if command.quantity == 0 {
            return Err(ProductError::NonPositiveQuantity);
        }
        Ok(vec![
            StockReplenished {
                quantity: command.quantity,
            }
            .into(),
        ])
    }
}

impl Handle<Reserve> for Product {
    type HandleError = ProductError;

    fn handle(&self, command: &Reserve) -> Result<Vec<Self::Event>, Self::HandleError> {
        if command.quantity == 0 {
            return Err(ProductError::NonPositiveQuantity);
        }
        // Idempotent under reactor redelivery: a reservation already exists for
        // this order, so re-reserving is a no-op rather than a double count.
        if self.has_reservation_for(command.order_id) {
            return Ok(vec![]);
        }
        if self.available() < command.quantity {
            return Err(ProductError::InsufficientStock {
                available: self.available(),
                requested: command.quantity,
            });
        }
        Ok(vec![
            StockReserved {
                order_id: command.order_id,
                quantity: command.quantity,
            }
            .into(),
        ])
    }
}

impl Handle<ReleaseReservation> for Product {
    type HandleError = ProductError;

    fn handle(&self, command: &ReleaseReservation) -> Result<Vec<Self::Event>, Self::HandleError> {
        // No reservation to release → no-op (idempotent compensation).
        if !self.has_reservation_for(command.order_id) {
            return Ok(vec![]);
        }
        Ok(vec![
            StockReservationReleased {
                order_id: command.order_id,
                quantity: command.quantity,
            }
            .into(),
        ])
    }
}

impl Handle<CommitReservation> for Product {
    type HandleError = ProductError;

    fn handle(&self, command: &CommitReservation) -> Result<Vec<Self::Event>, Self::HandleError> {
        // Nothing reserved for this order → no-op (idempotent commit).
        if !self.has_reservation_for(command.order_id) {
            return Ok(vec![]);
        }
        Ok(vec![
            StockCommitted {
                order_id: command.order_id,
                quantity: command.quantity,
            }
            .into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use sourcery::test::TestFramework;

    use super::*;

    type ProductTest = TestFramework<Product>;

    fn listed(stock: u32) -> ProductListed {
        ProductListed {
            sku: "SKU-1".to_string(),
            name: "Thing".to_string(),
            initial_stock: stock,
            unit_price_cents: 1_000,
        }
    }

    #[test]
    fn listing_a_product_emits_product_listed() {
        let event = listed(10);
        ProductTest::new()
            .when_create(&ListProduct {
                sku: event.sku.clone(),
                name: event.name.clone(),
                initial_stock: event.initial_stock,
                unit_price_cents: event.unit_price_cents,
            })
            .then_expect_events(&[event.into()]);
    }

    #[test]
    fn replenish_emits_stock_replenished() {
        ProductTest::given(&[listed(10).into()])
            .when(&Replenish { quantity: 5 })
            .then_expect_events(&[StockReplenished { quantity: 5 }.into()]);
    }

    #[test]
    fn replenish_zero_is_rejected() {
        ProductTest::given(&[listed(10).into()])
            .when(&Replenish { quantity: 0 })
            .then_expect_error_eq(&ProductError::NonPositiveQuantity);
    }

    #[test]
    fn reserve_within_stock_emits_stock_reserved() {
        let order = OrderId::new();
        ProductTest::given(&[listed(10).into()])
            .when(&Reserve {
                order_id: order,
                quantity: 3,
            })
            .then_expect_events(&[StockReserved {
                order_id: order,
                quantity: 3,
            }
            .into()]);
    }

    #[test]
    fn reserve_beyond_available_is_rejected() {
        let order = OrderId::new();
        ProductTest::given(&[listed(2).into()])
            .when(&Reserve {
                order_id: order,
                quantity: 5,
            })
            .then_expect_error_eq(&ProductError::InsufficientStock {
                available: 2,
                requested: 5,
            });
    }

    #[test]
    fn reserving_twice_for_the_same_order_is_idempotent() {
        let order = OrderId::new();
        ProductTest::given(&[
            listed(10).into(),
            StockReserved {
                order_id: order,
                quantity: 3,
            }
            .into(),
        ])
        .when(&Reserve {
            order_id: order,
            quantity: 3,
        })
        .then_expect_no_events();
    }

    #[test]
    fn releasing_a_held_reservation_emits_release() {
        let order = OrderId::new();
        ProductTest::given(&[
            listed(10).into(),
            StockReserved {
                order_id: order,
                quantity: 3,
            }
            .into(),
        ])
        .when(&ReleaseReservation {
            order_id: order,
            quantity: 3,
        })
        .then_expect_events(&[StockReservationReleased {
            order_id: order,
            quantity: 3,
        }
        .into()]);
    }

    #[test]
    fn releasing_an_unknown_reservation_is_a_no_op() {
        let order = OrderId::new();
        ProductTest::given(&[listed(10).into()])
            .when(&ReleaseReservation {
                order_id: order,
                quantity: 3,
            })
            .then_expect_no_events();
    }

    #[test]
    fn committing_a_held_reservation_emits_commit() {
        let order = OrderId::new();
        ProductTest::given(&[
            listed(10).into(),
            StockReserved {
                order_id: order,
                quantity: 3,
            }
            .into(),
        ])
        .when(&CommitReservation {
            order_id: order,
            quantity: 3,
        })
        .then_expect_events(&[StockCommitted {
            order_id: order,
            quantity: 3,
        }
        .into()]);
    }
}
