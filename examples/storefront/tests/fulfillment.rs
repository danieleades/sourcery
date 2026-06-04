//! End-to-end tests driving orders through the `FulfillmentCoordinator`.
//!
//! These exercise the whole stack: commands, the reactor catch-up + live
//! runner, read-your-writes tokens, optimistic retry, and compensation.

use std::time::Duration;

use sourcery::{ConsistencyToken, ReactorHandle, snapshot::inmemory as snapshot};
use storefront::{
    app::{AppError, AppRepo, build_repo},
    coordinator::FulfillmentCoordinator,
    domain::{
        ids::{CustomerId, OrderId, PaymentId, ProductId},
        order::{Order, OrderLine, OrderStatus, PlaceOrder},
        payment::{Capture, Payment, PaymentStatus},
        product::{ListProduct, Product},
    },
    metadata::RequestContext,
    projections::{CustomerAccountView, InventoryAvailability},
};

/// Start a coordinator over the repository with an in-memory checkpoint store.
async fn start_coordinator(repo: &AppRepo) -> ReactorHandle<u64, AppError> {
    repo.react(FulfillmentCoordinator::new(repo.clone()))
        .with_checkpoints(snapshot::Store::<u64>::always())
        .start()
        .await
        .expect("coordinator starts")
}

/// Wait for the coordinator to process up to `token`.
async fn settle(reactor: &ReactorHandle<u64, AppError>, token: Option<ConsistencyToken<u64>>) {
    if let Some(token) = token {
        tokio::time::timeout(Duration::from_secs(5), reactor.wait_for(token))
            .await
            .expect("coordinator settles in time")
            .expect("coordinator reaches token");
    }
}

async fn list_product(repo: &AppRepo, id: ProductId, stock: u32, price: u64) {
    repo.create::<Product, ListProduct>(
        &id,
        &ListProduct {
            sku: format!("SKU-{id}"),
            name: "Item".to_string(),
            initial_stock: stock,
            unit_price_cents: price,
        },
        &RequestContext::new("test"),
    )
    .await
    .expect("product listed");
}

async fn place_order(
    repo: &AppRepo,
    order: OrderId,
    customer: &CustomerId,
    lines: Vec<OrderLine>,
) -> Option<ConsistencyToken<u64>> {
    repo.create_tracked::<Order, PlaceOrder>(
        &order,
        &PlaceOrder {
            customer_id: customer.clone(),
            lines,
        },
        &RequestContext::new("test"),
    )
    .await
    .expect("order placed")
}

#[tokio::test]
async fn happy_path_reserves_authorizes_captures_and_confirms() {
    let repo = build_repo();
    let widget = ProductId::new();
    list_product(&repo, widget, 100, 1_000).await;

    let reactor = start_coordinator(&repo).await;

    let customer = CustomerId::new("cust-alice");
    let order = OrderId::new();
    let placed = place_order(
        &repo,
        order,
        &customer,
        vec![OrderLine {
            product_id: widget,
            quantity: 3,
        }],
    )
    .await;
    settle(&reactor, placed).await;

    // The coordinator reserved stock and authorized a payment.
    let payment_id = PaymentId::for_order(order);
    let payment = repo
        .load::<Payment>(&payment_id)
        .await
        .unwrap()
        .expect("payment authorized");
    assert_eq!(payment.status(), PaymentStatus::Authorized);
    assert_eq!(payment.amount_cents(), 3_000);

    // Capture the payment; the coordinator confirms and commits the reservation.
    let captured = repo
        .update_tracked::<Payment, Capture>(&payment_id, &Capture, &RequestContext::new("gateway"))
        .await
        .unwrap();
    settle(&reactor, captured).await;

    let order_state = repo.load::<Order>(&order).await.unwrap().expect("order");
    assert_eq!(order_state.status(), OrderStatus::Confirmed);

    // Inventory reflects the committed stock: 100 - 3 sold, nothing reserved.
    let snapshots = snapshot::Store::<u64>::always();
    let inventory = repo
        .load_projection_with_snapshot::<InventoryAvailability, _>(&(), &snapshots)
        .await
        .unwrap();
    let level = inventory.level(widget).expect("widget tracked");
    assert_eq!(level.on_hand, 97);
    assert_eq!(level.reserved, 0);

    // The customer account view joins the order and its captured payment.
    let account = repo
        .load_projection::<CustomerAccountView>(&customer)
        .await
        .unwrap();
    assert_eq!(account.orders(), &[order]);
    assert_eq!(account.lifetime_spend_cents(), 3_000);

    reactor.stop().await.unwrap();
}

#[tokio::test]
async fn out_of_stock_order_is_compensated() {
    let repo = build_repo();
    let scarce = ProductId::new();
    list_product(&repo, scarce, 5, 4_500).await;

    let reactor = start_coordinator(&repo).await;

    let customer = CustomerId::new("cust-bob");
    let order = OrderId::new();
    let placed = place_order(
        &repo,
        order,
        &customer,
        vec![OrderLine {
            product_id: scarce,
            quantity: 10,
        }],
    )
    .await;
    settle(&reactor, placed).await;

    // The coordinator could not reserve, so it cancelled the order and took no
    // payment.
    let order_state = repo.load::<Order>(&order).await.unwrap().expect("order");
    assert_eq!(order_state.status(), OrderStatus::Cancelled);
    assert!(
        repo.load::<Payment>(&PaymentId::for_order(order))
            .await
            .unwrap()
            .is_none(),
        "no payment for an out-of-stock order"
    );

    // Stock is untouched: nothing was reserved.
    let product = repo
        .load::<Product>(&scarce)
        .await
        .unwrap()
        .expect("product");
    assert_eq!(product.available(), 5);
    assert!(!product.has_reservation_for(order));

    reactor.stop().await.unwrap();
}
