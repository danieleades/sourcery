//! Runnable storefront demo.
//!
//! Wires up the in-memory store, starts the live revenue subscription and the
//! fulfillment coordinator, then drives three order scenarios — happy path,
//! out-of-stock compensation, and payment failure — before printing the read
//! models. It exits `Ok` only if every scenario reaches its expected state, so
//! it doubles as a smoke test.
//!
//! Run with: `cargo run -p storefront`

use std::{error::Error, time::Duration};

use sourcery::{ConsistencyToken, ReactorHandle, snapshot::inmemory as snapshot};
use storefront::{
    app::{AppError, AppRepo, build_repo},
    coordinator::FulfillmentCoordinator,
    domain::{
        ids::{CustomerId, OrderId, PaymentId, ProductId},
        order::{Order, OrderLine, OrderStatus, PlaceOrder, ShipOrder},
        payment::{Capture, FailPayment, Payment, PaymentAuthorized, PaymentStatus},
        product::{ListProduct, Product, Replenish},
    },
    metadata::RequestContext,
    projections::{
        CustomerAccountView, InventoryAvailability, OrderFulfillmentView, OrderRefs,
        RevenueDashboard,
    },
};
use tokio_stream::StreamExt;

type DynError = Box<dyn Error + Send + Sync>;

/// The catalogue's product ids, minted up front so every scenario can refer to
/// them.
struct Catalogue {
    widget: ProductId,
    gadget: ProductId,
    gizmo: ProductId,
}

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let repo = build_repo();

    // Live ops dashboard: subscribe to the revenue read model and print every
    // update on a background task.
    let subscription = repo.subscribe::<RevenueDashboard>(()).start().await?;
    let (initial, mut updates) = subscription.updates();
    let printer = tokio::spawn(async move {
        println!("[revenue] initial net = {} cents", initial.net_cents());
        while let Some(dashboard) = updates.next().await {
            println!("[revenue] net = {} cents", dashboard.net_cents());
        }
    });

    let catalogue = seed_catalogue(&repo).await?;

    // Place one order *before* the coordinator starts, to show catch-up: when
    // the reactor starts it replays this historical event and fulfils it.
    let historical_customer = CustomerId::new("cust-historical");
    let historical_order = OrderId::new();
    place_order(
        &repo,
        historical_order,
        &historical_customer,
        vec![line(catalogue.widget, 3)],
    )
    .await?;

    // Start the coordinator with a persistent checkpoint store so a restart
    // resumes instead of replaying the whole workflow.
    let coordinator = FulfillmentCoordinator::new(repo.clone());
    let checkpoints = snapshot::Store::<u64>::always();
    let reactor = repo
        .react(coordinator)
        .with_checkpoints(checkpoints)
        .start()
        .await?;

    // Catch-up is complete once `start()` returns: the historical order has been
    // reserved and a payment authorized for it.
    let historical_payment = PaymentId::for_order(historical_order);
    let payment = repo.load::<Payment>(&historical_payment).await?;
    assert!(
        payment.is_some_and(|p| p.status() == PaymentStatus::Authorized),
        "catch-up should have authorized the historical order's payment"
    );
    println!("[catch-up] historical order authorized after coordinator start");

    happy_path(&repo, &reactor, &catalogue).await?;
    out_of_stock(&repo, &reactor, &catalogue).await?;
    payment_failure(&repo, &reactor, &catalogue).await?;
    unchecked_replenishment(&repo, &catalogue).await?;
    demonstrate_event_versioning()?;

    print_reports(&repo, &subscription, &catalogue).await?;

    // Shut down cleanly. Stopping the subscription ends the update stream, which
    // lets the printer task finish.
    reactor.stop().await?;
    subscription.stop().await?;
    printer.await?;

    println!("storefront demo completed successfully");
    Ok(())
}

/// List the catalogue and return the product ids.
async fn seed_catalogue(repo: &AppRepo) -> Result<Catalogue, DynError> {
    let catalogue = Catalogue {
        widget: ProductId::new(),
        gadget: ProductId::new(),
        gizmo: ProductId::new(),
    };
    let actor = RequestContext::new("catalogue-admin");

    list_product(
        repo,
        catalogue.widget,
        "WIDGET-1",
        "Widget",
        100,
        1_000,
        &actor,
    )
    .await?;
    // The gadget is deliberately scarce, to drive the out-of-stock scenario.
    list_product(
        repo,
        catalogue.gadget,
        "GADGET-1",
        "Gadget",
        5,
        4_500,
        &actor,
    )
    .await?;
    list_product(repo, catalogue.gizmo, "GIZMO-1", "Gizmo", 50, 2_500, &actor).await?;

    Ok(catalogue)
}

/// The happy path: place an order, capture its payment, and ship it.
async fn happy_path(
    repo: &AppRepo,
    reactor: &ReactorHandle<u64, AppError>,
    catalogue: &Catalogue,
) -> Result<(), DynError> {
    println!("\n== happy path ==");
    let customer = CustomerId::new("cust-alice");
    let order_id = OrderId::new();

    // Place the order and wait for the coordinator to reserve stock + authorize.
    let token = place_order(
        repo,
        order_id,
        &customer,
        vec![line(catalogue.widget, 2), line(catalogue.gizmo, 1)],
    )
    .await?;
    settle(reactor, token).await?;

    // Capture the payment (the "gateway"); the coordinator confirms the order
    // and commits the reservations in reaction.
    let payment_id = PaymentId::for_order(order_id);
    let captured = repo
        .update_tracked::<Payment, Capture>(&payment_id, &Capture, &RequestContext::new("gateway"))
        .await?;
    settle(reactor, captured).await?;

    let order = repo.load::<Order>(&order_id).await?.expect("order exists");
    assert_eq!(order.status(), OrderStatus::Confirmed, "order confirmed");

    // Ship the confirmed order.
    repo.update::<Order, ShipOrder>(
        &order_id,
        &ShipOrder {
            carrier: "Royal Mail".to_string(),
            tracking: "RM123456789GB".to_string(),
        },
        &RequestContext::new("warehouse"),
    )
    .await?;
    println!("[happy] order confirmed, captured, and shipped");
    Ok(())
}

/// The out-of-stock path: the coordinator cannot reserve, so it compensates.
async fn out_of_stock(
    repo: &AppRepo,
    reactor: &ReactorHandle<u64, AppError>,
    catalogue: &Catalogue,
) -> Result<(), DynError> {
    println!("\n== out of stock ==");
    let customer = CustomerId::new("cust-bob");
    let order_id = OrderId::new();

    // 10 gadgets, but only 5 are in stock.
    let token = place_order(repo, order_id, &customer, vec![line(catalogue.gadget, 10)]).await?;
    settle(reactor, token).await?;

    let order = repo.load::<Order>(&order_id).await?.expect("order exists");
    assert_eq!(order.status(), OrderStatus::Cancelled, "order cancelled");
    assert!(
        repo.load::<Payment>(&PaymentId::for_order(order_id))
            .await?
            .is_none(),
        "no payment should be authorized for an out-of-stock order"
    );
    println!("[out-of-stock] order auto-cancelled, no payment taken");
    Ok(())
}

/// The payment-failure path: stock is reserved, then the payment fails, so the
/// coordinator releases the reservations and cancels.
async fn payment_failure(
    repo: &AppRepo,
    reactor: &ReactorHandle<u64, AppError>,
    catalogue: &Catalogue,
) -> Result<(), DynError> {
    println!("\n== payment failure ==");
    let customer = CustomerId::new("cust-carol");
    let order_id = OrderId::new();

    let token = place_order(repo, order_id, &customer, vec![line(catalogue.widget, 4)]).await?;
    settle(reactor, token).await?;

    // The gateway declines the authorized payment.
    let payment_id = PaymentId::for_order(order_id);
    let failed = repo
        .update_tracked::<Payment, FailPayment>(
            &payment_id,
            &FailPayment {
                reason: "card declined".to_string(),
            },
            &RequestContext::new("gateway"),
        )
        .await?;
    settle(reactor, failed).await?;

    let order = repo.load::<Order>(&order_id).await?.expect("order exists");
    assert_eq!(order.status(), OrderStatus::Cancelled, "order cancelled");
    let widget = repo
        .load::<Product>(&catalogue.widget)
        .await?
        .expect("widget exists");
    assert!(
        !widget.has_reservation_for(order_id),
        "reservation should have been released"
    );
    println!("[payment-failure] reservations released and order cancelled");
    Ok(())
}

/// Replenish stock through an unchecked (last-writer-wins) repository view.
///
/// Restocking is naturally idempotent and never conflicts with itself, so it
/// has no need for version checking. Deriving an `Unchecked` view from the same
/// store keeps the configured snapshots while skipping the optimistic guard.
async fn unchecked_replenishment(repo: &AppRepo, catalogue: &Catalogue) -> Result<(), DynError> {
    println!("\n== replenishment (unchecked) ==");
    let unchecked = repo.clone().without_concurrency_checking();
    unchecked
        .update::<Product, Replenish>(
            &catalogue.gadget,
            &Replenish { quantity: 20 },
            &RequestContext::new("warehouse"),
        )
        .await?;
    println!("[replenish] added 20 gadgets without a version check");
    Ok(())
}

/// Show that an older `PaymentAuthorized` (no `currency` field) still decodes,
/// defaulting `currency` to `USD`.
fn demonstrate_event_versioning() -> Result<(), DynError> {
    println!("\n== event versioning ==");
    let order = OrderId::new();
    let v1_json = format!(
        r#"{{"order_id":"{}","customer_id":"cust-legacy","amount_cents":1999}}"#,
        order.0
    );
    let decoded: PaymentAuthorized = serde_json::from_str(&v1_json)?;
    assert_eq!(decoded.currency, "USD", "missing currency defaults to USD");
    println!(
        "[versioning] legacy event decoded with currency = {}",
        decoded.currency
    );
    Ok(())
}

/// Print all four read models.
async fn print_reports(
    repo: &AppRepo,
    subscription: &sourcery::Subscription<RevenueDashboard, u64, AppError>,
    catalogue: &Catalogue,
) -> Result<(), DynError> {
    println!("\n== read models ==");

    // Inventory: a snapshot-backed singleton rebuilt on demand.
    let inventory_snapshots = snapshot::Store::<u64>::always();
    let inventory = repo
        .load_projection_with_snapshot::<InventoryAvailability, _>(&(), &inventory_snapshots)
        .await?;
    for (key, level) in inventory.levels() {
        println!(
            "[inventory] {key}: on_hand={} reserved={} available={}",
            level.on_hand,
            level.reserved,
            level.available()
        );
    }

    // Customer account: a cross-aggregate join, rebuilt on demand.
    let alice = CustomerId::new("cust-alice");
    let account = repo.load_projection::<CustomerAccountView>(&alice).await?;
    println!(
        "[customer] {alice}: {} order(s), spent {} cents, {} cents outstanding",
        account.orders().len(),
        account.lifetime_spend_cents(),
        account.outstanding_cents()
    );

    // Order fulfillment: a per-order view scoped to three streams.
    if let Some(order) = account.orders().first().copied() {
        let refs = OrderRefs {
            order,
            payment: PaymentId::for_order(order),
            products: vec![catalogue.widget, catalogue.gizmo],
        };
        let view = repo.load_projection::<OrderFulfillmentView>(&refs).await?;
        println!(
            "[fulfillment] {order}: order={:?} payment={:?} reserved_products={} \
             payment_belongs_to={:?}",
            view.order_status(),
            view.payment_status(),
            view.reserved_product_count(),
            view.payment_belongs_to(),
        );
    }

    // Revenue: the live subscription. Wait for the dashboard to catch up to the
    // latest commit, then read its current state.
    if let Some(token) = repo.consistency_token().await? {
        subscription.wait_for(token).await?;
    }
    let revenue = subscription.current();
    println!(
        "[revenue] gross captured = {} cents, net = {} cents",
        revenue.gross_captured_cents(),
        revenue.net_cents()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Small command helpers
// ---------------------------------------------------------------------------

/// Build an order line.
const fn line(product_id: ProductId, quantity: u32) -> OrderLine {
    OrderLine {
        product_id,
        quantity,
    }
}

/// List a single product.
async fn list_product(
    repo: &AppRepo,
    id: ProductId,
    sku: &str,
    name: &str,
    initial_stock: u32,
    unit_price_cents: u64,
    actor: &RequestContext,
) -> Result<(), DynError> {
    repo.create::<Product, ListProduct>(
        &id,
        &ListProduct {
            sku: sku.to_string(),
            name: name.to_string(),
            initial_stock,
            unit_price_cents,
        },
        actor,
    )
    .await?;
    Ok(())
}

/// Place an order, returning the read-your-writes token for the commit.
async fn place_order(
    repo: &AppRepo,
    order_id: OrderId,
    customer_id: &CustomerId,
    lines: Vec<OrderLine>,
) -> Result<Option<ConsistencyToken<u64>>, DynError> {
    let token = repo
        .create_tracked::<Order, PlaceOrder>(
            &order_id,
            &PlaceOrder {
                customer_id: customer_id.clone(),
                lines,
            },
            &RequestContext::new("storefront"),
        )
        .await?;
    Ok(token)
}

/// Wait (with a timeout) for the coordinator to process up to `token`.
async fn settle<E>(
    reactor: &ReactorHandle<u64, E>,
    token: Option<ConsistencyToken<u64>>,
) -> Result<(), DynError>
where
    E: Error + Send + Sync + 'static,
{
    if let Some(token) = token {
        tokio::time::timeout(Duration::from_secs(5), reactor.wait_for(token)).await??;
    }
    Ok(())
}
