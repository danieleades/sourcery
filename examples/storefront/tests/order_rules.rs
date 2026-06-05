//! Unit tests for the `Order` aggregate's command rules.

use sourcery::test::TestFramework;
use storefront::domain::{
    ids::{CustomerId, ProductId},
    order::{
        CancelOrder, ConfirmOrder, Order, OrderCancelled, OrderConfirmed, OrderError, OrderLine,
        OrderPlaced, OrderShipped, PlaceOrder, ShipOrder,
    },
};

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
