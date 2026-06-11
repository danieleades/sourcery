//! The `Order` aggregate: cart → placed → confirmed/cancelled → shipped.
//!
//! An order references products (many-to-many) and a customer. Both foreign
//! keys live in the `OrderPlaced` payload: `customer_id` is the join key
//! `CustomerAccountView` reads, and each line's `product_id` is the join key
//! the coordinator uses to reserve stock.

use serde::{Deserialize, Serialize};
use sourcery::{Aggregate, Apply, Create, DomainEvent, Handle, HandleCreate};

use super::ids::{CustomerId, OrderId, ProductId};

// ---------------------------------------------------------------------------
// Value types & events (`store.order.*`)
// ---------------------------------------------------------------------------

/// A single line of an order: which product, how many.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderLine {
    pub product_id: ProductId,
    pub quantity: u32,
}

/// An order was placed with a non-empty set of lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub customer_id: CustomerId,
    pub lines: Vec<OrderLine>,
}

impl DomainEvent for OrderPlaced {
    const KIND: &'static str = "store.order.placed";
}

/// The order was confirmed (payment captured, stock reserved).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderConfirmed {}

impl DomainEvent for OrderConfirmed {
    const KIND: &'static str = "store.order.confirmed";
}

/// The order was cancelled before shipping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderCancelled {
    pub reason: String,
}

impl DomainEvent for OrderCancelled {
    const KIND: &'static str = "store.order.cancelled";
}

/// The order shipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderShipped {
    pub carrier: String,
    pub tracking: String,
}

impl DomainEvent for OrderShipped {
    const KIND: &'static str = "store.order.shipped";
}

// ---------------------------------------------------------------------------
// Error & status
// ---------------------------------------------------------------------------

/// Reasons an order command can be rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OrderError {
    /// `PlaceOrder` was issued with no lines.
    #[error("cannot place an order with an empty cart")]
    EmptyCart,
    /// A lifecycle command was issued from an incompatible state.
    #[error("invalid transition: cannot {action} an order that is {state}")]
    InvalidTransition {
        action: &'static str,
        state: &'static str,
    },
}

/// Lifecycle state of an order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    /// Placed, awaiting confirmation.
    Placed,
    Confirmed,
    Cancelled,
    Shipped,
}

impl OrderStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::Confirmed => "confirmed",
            Self::Cancelled => "cancelled",
            Self::Shipped => "shipped",
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The order aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = OrderId,
    error = OrderError,
    kind = "store.order",
    events(OrderPlaced, OrderConfirmed, OrderCancelled, OrderShipped),
    create(OrderPlaced),
    derives(Debug, PartialEq, Eq)
)]
pub struct Order {
    status: OrderStatus,
    customer_id: CustomerId,
    lines: Vec<OrderLine>,
}

impl Order {
    /// Current lifecycle status.
    #[must_use]
    pub const fn status(&self) -> OrderStatus {
        self.status
    }

    /// The customer that placed this order.
    #[must_use]
    pub const fn customer_id(&self) -> &CustomerId {
        &self.customer_id
    }

    /// The order's lines.
    #[must_use]
    pub fn lines(&self) -> &[OrderLine] {
        &self.lines
    }
}

impl Create<OrderPlaced> for Order {
    fn create(event: &OrderPlaced) -> Self {
        Self {
            status: OrderStatus::Placed,
            customer_id: event.customer_id.clone(),
            lines: event.lines.clone(),
        }
    }
}

impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, event: &OrderPlaced) {
        self.status = OrderStatus::Placed;
        self.customer_id.clone_from(&event.customer_id);
        self.lines.clone_from(&event.lines);
    }
}

impl Apply<OrderConfirmed> for Order {
    fn apply(&mut self, _event: &OrderConfirmed) {
        self.status = OrderStatus::Confirmed;
    }
}

impl Apply<OrderCancelled> for Order {
    fn apply(&mut self, _event: &OrderCancelled) {
        self.status = OrderStatus::Cancelled;
    }
}

impl Apply<OrderShipped> for Order {
    fn apply(&mut self, _event: &OrderShipped) {
        self.status = OrderStatus::Shipped;
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Place a new order (creation command).
#[derive(Clone, Debug)]
pub struct PlaceOrder {
    pub customer_id: CustomerId,
    pub lines: Vec<OrderLine>,
}

/// Confirm a placed order.
#[derive(Clone, Debug)]
pub struct ConfirmOrder;

/// Cancel an order before it ships.
#[derive(Clone, Debug)]
pub struct CancelOrder {
    pub reason: String,
}

/// Ship a confirmed order.
#[derive(Clone, Debug)]
pub struct ShipOrder {
    pub carrier: String,
    pub tracking: String,
}

impl HandleCreate<PlaceOrder> for Order {
    type HandleCreateError = OrderError;

    fn handle_create(command: &PlaceOrder) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        if command.lines.is_empty() {
            return Err(OrderError::EmptyCart);
        }
        Ok(vec![
            OrderPlaced {
                customer_id: command.customer_id.clone(),
                lines: command.lines.clone(),
            }
            .into(),
        ])
    }
}

impl Handle<ConfirmOrder> for Order {
    type HandleError = OrderError;

    fn handle(&self, _command: &ConfirmOrder) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            OrderStatus::Placed => Ok(vec![OrderConfirmed {}.into()]),
            other => Err(OrderError::InvalidTransition {
                action: "confirm",
                state: other.label(),
            }),
        }
    }
}

impl Handle<CancelOrder> for Order {
    type HandleError = OrderError;

    fn handle(&self, command: &CancelOrder) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            OrderStatus::Placed | OrderStatus::Confirmed => Ok(vec![
                OrderCancelled {
                    reason: command.reason.clone(),
                }
                .into(),
            ]),
            other => Err(OrderError::InvalidTransition {
                action: "cancel",
                state: other.label(),
            }),
        }
    }
}

impl Handle<ShipOrder> for Order {
    type HandleError = OrderError;

    fn handle(&self, command: &ShipOrder) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            OrderStatus::Confirmed => Ok(vec![
                OrderShipped {
                    carrier: command.carrier.clone(),
                    tracking: command.tracking.clone(),
                }
                .into(),
            ]),
            other => Err(OrderError::InvalidTransition {
                action: "ship",
                state: other.label(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use sourcery::test::TestFramework;

    use super::*;

    type OrderTest = TestFramework<Order>;

    fn placed() -> OrderPlaced {
        OrderPlaced {
            customer_id: CustomerId::new("cust-1"),
            lines: vec![OrderLine {
                product_id: ProductId::new(),
                quantity: 1,
            }],
        }
    }

    #[test]
    fn placing_an_order_emits_order_placed() {
        let event = placed();
        OrderTest::new()
            .when_create(&PlaceOrder {
                customer_id: event.customer_id.clone(),
                lines: event.lines.clone(),
            })
            .then_expect_events(&[event.into()]);
    }

    #[test]
    fn placing_an_empty_cart_is_rejected() {
        OrderTest::new()
            .when_create(&PlaceOrder {
                customer_id: CustomerId::new("cust-1"),
                lines: vec![],
            })
            .then_expect_error_eq(&OrderError::EmptyCart);
    }

    #[test]
    fn confirming_a_placed_order_emits_confirmed() {
        OrderTest::given(&[placed().into()])
            .when(&ConfirmOrder)
            .then_expect_events(&[OrderConfirmed {}.into()]);
    }

    #[test]
    fn confirming_a_confirmed_order_is_rejected() {
        OrderTest::given(&[placed().into(), OrderConfirmed {}.into()])
            .when(&ConfirmOrder)
            .then_expect_error_eq(&OrderError::InvalidTransition {
                action: "confirm",
                state: "confirmed",
            });
    }

    #[test]
    fn cancelling_a_placed_order_emits_cancelled() {
        OrderTest::given(&[placed().into()])
            .when(&CancelOrder {
                reason: "changed mind".to_string(),
            })
            .then_expect_events(&[OrderCancelled {
                reason: "changed mind".to_string(),
            }
            .into()]);
    }

    #[test]
    fn cancelling_a_shipped_order_is_rejected() {
        OrderTest::given(&[
            placed().into(),
            OrderConfirmed {}.into(),
            OrderShipped {
                carrier: "DHL".to_string(),
                tracking: "T1".to_string(),
            }
            .into(),
        ])
        .when(&CancelOrder {
            reason: "too late".to_string(),
        })
        .then_expect_error_eq(&OrderError::InvalidTransition {
            action: "cancel",
            state: "shipped",
        });
    }

    #[test]
    fn shipping_a_confirmed_order_emits_shipped() {
        OrderTest::given(&[placed().into(), OrderConfirmed {}.into()])
            .when(&ShipOrder {
                carrier: "DHL".to_string(),
                tracking: "T1".to_string(),
            })
            .then_expect_events(&[OrderShipped {
                carrier: "DHL".to_string(),
                tracking: "T1".to_string(),
            }
            .into()]);
    }

    #[test]
    fn shipping_a_placed_order_is_rejected() {
        OrderTest::given(&[placed().into()])
            .when(&ShipOrder {
                carrier: "DHL".to_string(),
                tracking: "T1".to_string(),
            })
            .then_expect_error_eq(&OrderError::InvalidTransition {
                action: "ship",
                state: "placed",
            });
    }
}
