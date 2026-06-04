//! The four read models, each a different projection shape.
//!
//! - [`InventoryAvailability`] — singleton, snapshot-backed, one row per
//!   product.
//! - [`CustomerAccountView`] — one instance per customer, joining order and
//!   payment events by the `customer_id` carried in their payloads.
//! - [`OrderFulfillmentView`] — one instance per order, assembled from three
//!   independent streams scoped by typed id.
//! - [`RevenueDashboard`] — singleton analytics fed by a live subscription.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sourcery::{ApplyProjection, EventContext, Filters, Projection, StorageKey, store::EventStore};

use crate::{
    domain::{
        ids::{CustomerId, OrderId, PaymentId, ProductId},
        order::{Order, OrderEvent, OrderPlaced},
        payment::{Payment, PaymentAuthorized, PaymentCaptured, PaymentFailed, PaymentRefunded},
        product::{Product, ProductEvent},
    },
    metadata::RequestContext,
};

// ===========================================================================
// InventoryAvailability — singleton, snapshot-backed
// ===========================================================================

/// One product's stock position in the inventory read model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockLevel {
    pub name: String,
    pub on_hand: u32,
    pub reserved: u32,
}

impl StockLevel {
    /// Free stock available to reserve.
    #[must_use]
    pub const fn available(&self) -> u32 {
        self.on_hand.saturating_sub(self.reserved)
    }
}

/// Live `on_hand` / `reserved` / `available` per product, keyed by the
/// product's raw stream key. A singleton projection (`InstanceId = ()`) rebuilt
/// from a snapshot plus the events committed since.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Projection)]
#[projection(
    kind = "store.inventory-availability",
    metadata = crate::metadata::RequestContext,
    events(
        crate::domain::product::ProductListed,
        crate::domain::product::StockReplenished,
        crate::domain::product::StockReserved,
        crate::domain::product::StockReservationReleased,
        crate::domain::product::StockCommitted
    )
)]
pub struct InventoryAvailability {
    products: BTreeMap<String, StockLevel>,
}

impl InventoryAvailability {
    /// Look up one product's stock level by its raw stream key.
    #[must_use]
    pub fn level(&self, product: ProductId) -> Option<&StockLevel> {
        // Fully qualified: the blanket `StorageKey<T> for T` impl also applies
        // to `ProductId`, so name the target raw type explicitly.
        let key = <ProductId as StorageKey<String>>::to_key(&product);
        self.products.get(&key)
    }

    /// All product stock levels, keyed by raw stream key.
    #[must_use]
    pub const fn levels(&self) -> &BTreeMap<String, StockLevel> {
        &self.products
    }
}

impl ApplyProjection<crate::domain::product::ProductListed> for InventoryAvailability {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &crate::domain::product::ProductListed,
    ) {
        self.products.insert(
            ctx.aggregate_id.clone(),
            StockLevel {
                name: event.name.clone(),
                on_hand: event.initial_stock,
                reserved: 0,
            },
        );
    }
}

impl ApplyProjection<crate::domain::product::StockReplenished> for InventoryAvailability {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &crate::domain::product::StockReplenished,
    ) {
        self.products
            .entry(ctx.aggregate_id.clone())
            .or_default()
            .on_hand += event.quantity;
    }
}

impl ApplyProjection<crate::domain::product::StockReserved> for InventoryAvailability {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &crate::domain::product::StockReserved,
    ) {
        self.products
            .entry(ctx.aggregate_id.clone())
            .or_default()
            .reserved += event.quantity;
    }
}

impl ApplyProjection<crate::domain::product::StockReservationReleased> for InventoryAvailability {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &crate::domain::product::StockReservationReleased,
    ) {
        let level = self.products.entry(ctx.aggregate_id.clone()).or_default();
        level.reserved = level.reserved.saturating_sub(event.quantity);
    }
}

impl ApplyProjection<crate::domain::product::StockCommitted> for InventoryAvailability {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &crate::domain::product::StockCommitted,
    ) {
        let level = self.products.entry(ctx.aggregate_id.clone()).or_default();
        level.reserved = level.reserved.saturating_sub(event.quantity);
        level.on_hand = level.on_hand.saturating_sub(event.quantity);
    }
}

// ===========================================================================
// CustomerAccountView — one instance per customer, cross-aggregate
// ===========================================================================

/// Order history and money position for a single customer.
///
/// This is the canonical cross-aggregate join: it correlates two different
/// aggregate types — `Order` and `Payment` — by the `customer_id` carried in
/// their event payloads, not by any stream key. The filters are global; each
/// handler keeps only the events whose payload names *this* customer.
#[derive(Clone, Debug)]
pub struct CustomerAccountView {
    customer_id: CustomerId,
    orders: Vec<OrderId>,
    authorized_cents: u64,
    captured_cents: u64,
    refunded_cents: u64,
}

impl CustomerAccountView {
    /// Orders this customer has placed, in the order they were seen.
    #[must_use]
    pub fn orders(&self) -> &[OrderId] {
        &self.orders
    }

    /// Net amount the customer has actually spent (captured minus refunded).
    #[must_use]
    pub const fn lifetime_spend_cents(&self) -> u64 {
        self.captured_cents.saturating_sub(self.refunded_cents)
    }

    /// Authorised funds not yet captured.
    #[must_use]
    pub const fn outstanding_cents(&self) -> u64 {
        self.authorized_cents.saturating_sub(self.captured_cents)
    }

    fn owns(&self, customer: &CustomerId) -> bool {
        &self.customer_id == customer
    }
}

impl Projection for CustomerAccountView {
    type Id = String;
    type InstanceId = CustomerId;
    type Metadata = RequestContext;

    const KIND: &'static str = "store.customer-account-view";

    fn init(instance_id: &Self::InstanceId) -> Self {
        Self {
            customer_id: instance_id.clone(),
            orders: Vec::new(),
            authorized_cents: 0,
            captured_cents: 0,
            refunded_cents: 0,
        }
    }

    fn filters<S>(_instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        Filters::new()
            .event::<OrderPlaced>()
            .event::<PaymentAuthorized>()
            .event::<PaymentCaptured>()
            .event::<PaymentRefunded>()
    }
}

impl ApplyProjection<OrderPlaced> for CustomerAccountView {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &OrderPlaced,
    ) {
        if self.owns(&event.customer_id)
            && let Some(order) = OrderId::from_storage_key(ctx.aggregate_id)
        {
            self.orders.push(order);
        }
    }
}

impl ApplyProjection<PaymentAuthorized> for CustomerAccountView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentAuthorized,
    ) {
        if self.owns(&event.customer_id) {
            self.authorized_cents += event.amount_cents;
        }
    }
}

impl ApplyProjection<PaymentCaptured> for CustomerAccountView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentCaptured,
    ) {
        if self.owns(&event.customer_id) {
            self.captured_cents += event.amount_cents;
        }
    }
}

impl ApplyProjection<PaymentRefunded> for CustomerAccountView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentRefunded,
    ) {
        if self.owns(&event.customer_id) {
            self.refunded_cents += event.amount_cents;
        }
    }
}

// ===========================================================================
// OrderFulfillmentView — one instance per order, three aggregates
// ===========================================================================

/// The typed ids that scope an [`OrderFulfillmentView`] to one order.
#[derive(Clone, Debug)]
pub struct OrderRefs {
    pub order: OrderId,
    pub payment: PaymentId,
    pub products: Vec<ProductId>,
}

/// Fulfillment status of one order, assembled from its order stream, its
/// payment stream, and each product stream it touches.
///
/// Every filter is scoped to a specific aggregate instance by its typed id, so
/// loading this view reads only those three kinds of stream rather than the
/// whole log. `Order` and `Product` use `events_for` (all events of one
/// instance); `Payment` uses `event_for` for the specific lifecycle events.
#[derive(Clone, Debug, Default)]
pub struct OrderFulfillmentView {
    order_status: Option<String>,
    payment_status: Option<String>,
    /// Raw stream keys of products currently reserved for this order.
    reserved_products: BTreeSet<String>,
    /// Owning order recovered from the payment stream key (rather than a
    /// payload field), demonstrating the derived-id correlation.
    payment_belongs_to: Option<OrderId>,
}

impl OrderFulfillmentView {
    /// Latest order lifecycle status, if any order event has been seen.
    #[must_use]
    pub fn order_status(&self) -> Option<&str> {
        self.order_status.as_deref()
    }

    /// Latest payment lifecycle status, if any payment event has been seen.
    #[must_use]
    pub fn payment_status(&self) -> Option<&str> {
        self.payment_status.as_deref()
    }

    /// Number of products still holding a reservation for this order.
    #[must_use]
    pub fn reserved_product_count(&self) -> usize {
        self.reserved_products.len()
    }

    /// Order id recovered by parsing the payment stream key.
    #[must_use]
    pub const fn payment_belongs_to(&self) -> Option<OrderId> {
        self.payment_belongs_to
    }
}

impl Projection for OrderFulfillmentView {
    type Id = String;
    type InstanceId = OrderRefs;
    type Metadata = RequestContext;

    const KIND: &'static str = "store.order-fulfillment-view";

    fn init(_instance_id: &Self::InstanceId) -> Self {
        Self::default()
    }

    fn filters<S>(instance_id: &Self::InstanceId) -> Filters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        let mut filters = Filters::new()
            .events_for::<Order>(&instance_id.order)
            .event_for::<Payment, PaymentAuthorized>(&instance_id.payment)
            .event_for::<Payment, PaymentCaptured>(&instance_id.payment)
            .event_for::<Payment, PaymentFailed>(&instance_id.payment);
        for product in &instance_id.products {
            filters = filters.events_for::<Product>(product);
        }
        filters
    }
}

impl ApplyProjection<OrderEvent> for OrderFulfillmentView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &OrderEvent,
    ) {
        let status = match event {
            OrderEvent::OrderPlaced(_) => "placed",
            OrderEvent::OrderConfirmed(_) => "confirmed",
            OrderEvent::OrderCancelled(_) => "cancelled",
            OrderEvent::OrderShipped(_) => "shipped",
        };
        self.order_status = Some(status.to_string());
    }
}

impl ApplyProjection<PaymentAuthorized> for OrderFulfillmentView {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        _event: &PaymentAuthorized,
    ) {
        self.payment_status = Some("authorized".to_string());
        self.payment_belongs_to = PaymentId::order_from_storage_key(ctx.aggregate_id);
    }
}

impl ApplyProjection<PaymentCaptured> for OrderFulfillmentView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        _event: &PaymentCaptured,
    ) {
        self.payment_status = Some("captured".to_string());
    }
}

impl ApplyProjection<PaymentFailed> for OrderFulfillmentView {
    fn apply_projection(
        &mut self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        _event: &PaymentFailed,
    ) {
        self.payment_status = Some("failed".to_string());
    }
}

impl ApplyProjection<ProductEvent> for OrderFulfillmentView {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &ProductEvent,
    ) {
        let product_key = ctx.aggregate_id.clone();
        match event {
            ProductEvent::StockReserved(_) => {
                self.reserved_products.insert(product_key);
            }
            ProductEvent::StockReservationReleased(_) | ProductEvent::StockCommitted(_) => {
                self.reserved_products.remove(&product_key);
            }
            ProductEvent::ProductListed(_) | ProductEvent::StockReplenished(_) => {}
        }
    }
}

// ===========================================================================
// RevenueDashboard — singleton analytics, subscription-fed
// ===========================================================================

/// Net revenue overall and per day, driven by captured and refunded payments.
/// A singleton projection whose live state feeds the demo's `updates()` stream.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Projection)]
#[projection(
    kind = "store.revenue-dashboard",
    metadata = crate::metadata::RequestContext,
    events(PaymentCaptured, PaymentRefunded)
)]
pub struct RevenueDashboard {
    gross_captured: u64,
    refunded: u64,
    net_per_day: BTreeMap<u64, i64>,
}

impl RevenueDashboard {
    /// Total captured, before refunds, in cents.
    #[must_use]
    pub const fn gross_captured_cents(&self) -> u64 {
        self.gross_captured
    }

    /// Net revenue in cents: captured minus refunded.
    #[must_use]
    pub fn net_cents(&self) -> i64 {
        let captured = i64::try_from(self.gross_captured).unwrap_or(i64::MAX);
        let refunded = i64::try_from(self.refunded).unwrap_or(i64::MAX);
        captured - refunded
    }

    /// Net revenue in cents per epoch day.
    #[must_use]
    pub const fn net_per_day_cents(&self) -> &BTreeMap<u64, i64> {
        &self.net_per_day
    }
}

impl ApplyProjection<PaymentCaptured> for RevenueDashboard {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentCaptured,
    ) {
        self.gross_captured += event.amount_cents;
        *self
            .net_per_day
            .entry(ctx.metadata.epoch_day())
            .or_default() += i64::try_from(event.amount_cents).unwrap_or(i64::MAX);
    }
}

impl ApplyProjection<PaymentRefunded> for RevenueDashboard {
    fn apply_projection(
        &mut self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentRefunded,
    ) {
        self.refunded += event.amount_cents;
        *self
            .net_per_day
            .entry(ctx.metadata.epoch_day())
            .or_default() -= i64::try_from(event.amount_cents).unwrap_or(i64::MAX);
    }
}
