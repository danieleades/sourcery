//! Integration test for optimistic concurrency on `Product` reservations.
//!
//! A writer loads a stream version, a concurrent writer advances it, and the
//! first writer's commit is rejected. `update_with_retry` then reloads fresh
//! state and succeeds.

use sourcery::{
    Aggregate, Repository, StorageKey,
    store::{EventStore, NonEmpty, OptimisticCommitError},
    test::RepositoryTestExt,
};
use storefront::{
    app::AppStore,
    domain::{
        ids::{OrderId, ProductId},
        product::{ListProduct, Product, Reserve, StockReserved},
    },
    metadata::RequestContext,
};

#[tokio::test]
async fn optimistic_conflict_is_detected_then_resolved_by_retry() {
    let mut repo = Repository::new(AppStore::new());
    let product = ProductId::new();
    let key = <ProductId as StorageKey<String>>::to_key(&product);
    let md = RequestContext::new("test");

    repo.create::<Product, ListProduct>(
        &product,
        &ListProduct {
            sku: "SKU-1".to_string(),
            name: "Widget".to_string(),
            initial_stock: 10,
            unit_price_cents: 100,
        },
        &md,
    )
    .await
    .unwrap();

    // Capture the version a writer would have loaded.
    let stale_version = repo
        .event_store()
        .stream_version(Product::KIND, &key)
        .await
        .unwrap();

    // A concurrent writer reserves stock, advancing the stream version. The
    // `inject_event` helper takes the raw stream kind and key, so it works with
    // an aggregate that keys on a domain newtype (unlike the typed
    // `inject_concurrent_event`, which requires `A::Id == Store::Id`).
    let order_a = OrderId::new();
    repo.inject_event(
        Product::KIND,
        &key,
        StockReserved {
            order_id: order_a,
            quantity: 1,
        },
        md.clone(),
    )
    .await
    .unwrap();

    // The first writer, still holding the stale version, now conflicts.
    let order_b = OrderId::new();
    let conflict = repo
        .event_store()
        .commit_events_optimistic(
            Product::KIND,
            &key,
            stale_version,
            NonEmpty::singleton(StockReserved {
                order_id: order_b,
                quantity: 1,
            }),
            &md,
        )
        .await;
    assert!(
        matches!(conflict, Err(OptimisticCommitError::Conflict(_))),
        "stale commit must be rejected"
    );

    // The retry helper reloads fresh state and succeeds in a single attempt.
    let attempts = repo
        .update_with_retry::<Product, Reserve>(
            &product,
            &Reserve {
                order_id: order_b,
                quantity: 1,
            },
            &md,
            3,
        )
        .await
        .unwrap();
    assert_eq!(attempts, 1);

    let product = repo
        .load::<Product>(&product)
        .await
        .unwrap()
        .expect("product exists");
    assert_eq!(product.reserved(), 2, "both reservations applied");
    assert_eq!(product.available(), 8);
}
