//! Unit tests for the `Product` aggregate's command rules, using the
//! given/when/then `TestFramework`.

use sourcery::test::TestFramework;
use storefront::domain::{
    ids::OrderId,
    product::{
        CommitReservation, ListProduct, Product, ProductError, ProductListed, ReleaseReservation,
        Replenish, Reserve, StockCommitted, StockReplenished, StockReservationReleased,
        StockReserved,
    },
};

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
