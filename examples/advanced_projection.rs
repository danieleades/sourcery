//! Advanced projection example using the `Projection` trait.
//!
//! This example demonstrates how to compose a projection that mixes
//! global and scoped event subscriptions without relying on the
//! aggregate event enums. The `Projection` impl registers:
//!
//! - All `ProductRestocked` events (global)
//! - `InventoryAdjusted` events scoped to a specific product SKU
//! - `SaleCompleted` events scoped to sale aggregates for the same SKU
//! - `PromotionApplied` events coming from a different aggregate kind

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, Create, DomainEvent, Filters, Projection, Repository,
    store::{EventStore, inmemory},
    test::RepositoryTestExt,
};

// =============================================================================
// Aggregates and domain events
// =============================================================================

#[derive(Default, Serialize, Deserialize, Aggregate)]
#[aggregate(id = String, error = String, events(ProductRestocked, InventoryAdjusted), create(ProductRestocked))]
struct Product;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ProductRestocked {
    sku: String,
    quantity: i64,
}

impl DomainEvent for ProductRestocked {
    const KIND: &'static str = "inventory.product.restocked";
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct InventoryAdjusted {
    sku: String,
    delta: i64,
}

impl DomainEvent for InventoryAdjusted {
    const KIND: &'static str = "inventory.product.adjusted";
}

impl Apply<ProductRestocked> for Product {
    fn apply(&mut self, _event: &ProductRestocked) {}
}

impl Apply<InventoryAdjusted> for Product {
    fn apply(&mut self, _event: &InventoryAdjusted) {}
}

impl Create<ProductRestocked> for Product {
    fn create(_event: &ProductRestocked) -> Self {
        Self
    }
}

#[derive(Default, Serialize, Deserialize, Aggregate)]
#[aggregate(id = String, error = String, events(SaleCompleted), create(SaleCompleted))]
struct Sale;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SaleCompleted {
    sale_id: String,
    product_sku: String,
    quantity: i64,
}

impl DomainEvent for SaleCompleted {
    const KIND: &'static str = "sales.sale.completed";
}

impl Apply<SaleCompleted> for Sale {
    fn apply(&mut self, _event: &SaleCompleted) {}
}

impl Create<SaleCompleted> for Sale {
    fn create(_event: &SaleCompleted) -> Self {
        Self
    }
}

#[derive(Default, Serialize, Deserialize, Aggregate)]
#[aggregate(id = String, error = String, events(PromotionApplied), create(PromotionApplied))]
struct Promotion;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PromotionApplied {
    promotion_id: String,
    product_sku: String,
    amount_cents: i64,
}

impl DomainEvent for PromotionApplied {
    const KIND: &'static str = "marketing.promotion.applied";
}

impl Apply<PromotionApplied> for Promotion {
    fn apply(&mut self, _event: &PromotionApplied) {}
}

impl Create<PromotionApplied> for Promotion {
    fn create(_event: &PromotionApplied) -> Self {
        Self
    }
}

// =============================================================================
// Projection with mixed global/scoped filters
// =============================================================================

/// Parameters for the `ProductSummary` projection instance.
///
/// Captures the aggregate IDs needed for scoped event subscriptions.
struct ProductSummaryParams {
    product: String,
    sale: String,
    promotion: String,
}

#[derive(Debug, Default)]
struct ProductSummary {
    stock_levels: HashMap<String, i64>,
    sales: HashMap<String, i64>,
    promotion_totals: HashMap<String, i64>,
}

/// Manual `Projection` impl: mixes global and instance-scoped subscriptions.
///
/// When a projection's `filters()` method depends on instance-specific IDs
/// (like `ProductSummaryParams`), implement `Projection` directly rather than
/// using `#[derive(Projection)]`. The derive always generates a singleton
/// (`InstanceId = ()`) with global-only filters.
impl Projection for ProductSummary {
    type Id = String;
    type InstanceId = ProductSummaryParams;
    type Metadata = ();

    const KIND: &'static str = "product-summary";

    fn init(_params: &ProductSummaryParams) -> Self {
        Self::default()
    }

    fn filters<S>(params: &ProductSummaryParams) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new()
            .event::<ProductRestocked>() // global restocks
            .event_for::<Product, InventoryAdjusted>(&params.product)
            .event_for::<Sale, SaleCompleted>(&params.sale)
            .event_for::<Promotion, PromotionApplied>(&params.promotion)
    }
}

impl ApplyProjection<ProductRestocked> for ProductSummary {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &ProductRestocked,
        _metadata: &Self::Metadata,
    ) {
        *self.stock_levels.entry(event.sku.clone()).or_default() += event.quantity;
    }
}

impl ApplyProjection<InventoryAdjusted> for ProductSummary {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &InventoryAdjusted,
        _metadata: &Self::Metadata,
    ) {
        *self.stock_levels.entry(event.sku.clone()).or_default() += event.delta;
    }
}

impl ApplyProjection<SaleCompleted> for ProductSummary {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &SaleCompleted,
        _metadata: &Self::Metadata,
    ) {
        *self.sales.entry(event.product_sku.clone()).or_default() += event.quantity;
    }
}

impl ApplyProjection<PromotionApplied> for ProductSummary {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &PromotionApplied,
        _metadata: &Self::Metadata,
    ) {
        *self
            .promotion_totals
            .entry(event.product_sku.clone())
            .or_default() += event.amount_cents;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store: inmemory::Store<String, ()> = inmemory::Store::new();
    let mut repository = Repository::new(store);

    let product_id = String::from("SKU-007");
    let sale_id = String::from("sale-123");
    let promotion_id = String::from("promo-42");

    // Seed the store using the test utilities.
    repository
        .seed_events::<Product>(
            &product_id,
            vec![
                ProductRestocked {
                    sku: "SKU-007".into(),
                    quantity: 50,
                }
                .into(),
                InventoryAdjusted {
                    sku: "SKU-007".into(),
                    delta: -5,
                }
                .into(),
            ],
        )
        .await?;

    repository
        .seed_events::<Sale>(
            &sale_id,
            vec![
                SaleCompleted {
                    sale_id: "sale-123".into(),
                    product_sku: "SKU-007".into(),
                    quantity: 2,
                }
                .into(),
            ],
        )
        .await?;

    repository
        .seed_events::<Promotion>(
            &promotion_id,
            vec![
                PromotionApplied {
                    promotion_id: "promo-42".into(),
                    product_sku: "SKU-007".into(),
                    amount_cents: 300,
                }
                .into(),
            ],
        )
        .await?;

    // Load the projection using the centralised filter configuration.
    let params = ProductSummaryParams {
        product: product_id.clone(),
        sale: sale_id.clone(),
        promotion: promotion_id.clone(),
    };
    let summary = repository
        .load_projection::<ProductSummary>(&params)
        .await?;

    println!("Product summary: {summary:#?}");

    Ok(())
}
