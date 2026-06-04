//! Integration tests for repository functionality.

#![cfg(feature = "test-util")]

use std::{
    convert::Infallible,
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value as JsonValue;
use sourcery::{
    Aggregate, Apply, ApplyProjection, ConcurrencyConflict, Create, DomainEvent, EventContext,
    Filters, Handle, HandleCreate, Projection, Repository, StorageKey,
    event::EventKind,
    repository::CommandError,
    snapshot::{
        OfferSnapshotError, Snapshot, SnapshotOffer, SnapshotStore,
        inmemory::Store as InMemorySnapshotStore,
    },
    store::{
        CommitError, Committed, EventFilter, EventStore, NonEmpty, OptimisticCommitError,
        StoredEvent, inmemory,
    },
};
use thiserror::Error;

// ============================================================================
// Test Domain: Counter
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ValueAdded {
    amount: i32,
}

impl sourcery::DomainEvent for ValueAdded {
    const KIND: &'static str = "value-added";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(ValueAdded),
    create(ValueAdded),
    derives(Debug, PartialEq, Eq)
)]
struct Counter {
    value: i32,
}

impl Apply<ValueAdded> for Counter {
    fn apply(&mut self, event: &ValueAdded) {
        self.value += event.amount;
    }
}

impl Create<ValueAdded> for Counter {
    fn create(event: &ValueAdded) -> Self {
        let mut this = Self::default();
        <Self as Apply<ValueAdded>>::apply(&mut this, event);
        this
    }
}

struct AddValue {
    amount: i32,
}

impl Handle<AddValue> for Counter {
    type HandleError = Self::Error;

    fn handle(&self, command: &AddValue) -> Result<Vec<Self::Event>, Self::Error> {
        if command.amount <= 0 {
            return Err("amount must be positive".to_string());
        }
        Ok(vec![
            ValueAdded {
                amount: command.amount,
            }
            .into(),
        ])
    }
}

impl HandleCreate<AddValue> for Counter {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &AddValue) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        if command.amount <= 0 {
            return Err("amount must be positive".to_string());
        }
        Ok(vec![
            ValueAdded {
                amount: command.amount,
            }
            .into(),
        ])
    }
}

struct NoOp;

impl Handle<NoOp> for Counter {
    type HandleError = Self::Error;

    fn handle(&self, _: &NoOp) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![])
    }
}

impl HandleCreate<NoOp> for Counter {
    type HandleCreateError = Self::Error;

    fn handle_create(_: &NoOp) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![])
    }
}

struct RequireAtLeast {
    min: i32,
}

impl Handle<RequireAtLeast> for Counter {
    type HandleError = Self::Error;

    fn handle(&self, command: &RequireAtLeast) -> Result<Vec<Self::Event>, Self::Error> {
        if self.value < command.min {
            return Err("insufficient value".to_string());
        }
        Ok(vec![])
    }
}

// ============================================================================
// Test Domain: Per-aggregate ID types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductId(String);

impl StorageKey<String> for ProductId {
    fn to_key(&self) -> String {
        format!("product:{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderId(u64);

impl StorageKey<String> for OrderId {
    fn to_key(&self) -> String {
        format!("order:{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProductCreated {
    product_id: String,
    name: String,
}

impl DomainEvent for ProductCreated {
    const KIND: &'static str = "product-created";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProductRenamed {
    name: String,
}

impl DomainEvent for ProductRenamed {
    const KIND: &'static str = "product-renamed";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = ProductId,
    error = String,
    events(ProductCreated, ProductRenamed),
    create(ProductCreated),
    derives(Debug, PartialEq, Eq)
)]
struct Product {
    id: String,
    name: String,
    rename_count: u32,
}

impl Apply<ProductCreated> for Product {
    fn apply(&mut self, event: &ProductCreated) {
        self.id.clone_from(&event.product_id);
        self.name.clone_from(&event.name);
    }
}

impl Apply<ProductRenamed> for Product {
    fn apply(&mut self, event: &ProductRenamed) {
        self.name.clone_from(&event.name);
        self.rename_count += 1;
    }
}

impl Create<ProductCreated> for Product {
    fn create(event: &ProductCreated) -> Self {
        let mut product = Self::default();
        <Self as Apply<ProductCreated>>::apply(&mut product, event);
        product
    }
}

struct CreateProduct {
    id: ProductId,
    name: String,
}

impl HandleCreate<CreateProduct> for Product {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &CreateProduct) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            ProductCreated {
                product_id: command.id.0.clone(),
                name: command.name.clone(),
            }
            .into(),
        ])
    }
}

struct RenameProduct {
    name: String,
}

impl Handle<RenameProduct> for Product {
    type HandleError = Self::Error;

    fn handle(&self, command: &RenameProduct) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            ProductRenamed {
                name: command.name.clone(),
            }
            .into(),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: u64,
}

impl DomainEvent for OrderPlaced {
    const KIND: &'static str = "order-placed";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OrderLineAdded {
    sku: String,
    quantity: u32,
}

impl DomainEvent for OrderLineAdded {
    const KIND: &'static str = "order-line-added";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = OrderId,
    error = String,
    events(OrderPlaced, OrderLineAdded),
    create(OrderPlaced),
    derives(Debug, PartialEq, Eq)
)]
struct Order {
    id: u64,
    line_count: u32,
    total_quantity: u32,
}

impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, event: &OrderPlaced) {
        self.id = event.order_id;
    }
}

impl Apply<OrderLineAdded> for Order {
    fn apply(&mut self, event: &OrderLineAdded) {
        self.line_count += 1;
        self.total_quantity += event.quantity;
    }
}

impl Create<OrderPlaced> for Order {
    fn create(event: &OrderPlaced) -> Self {
        let mut order = Self::default();
        <Self as Apply<OrderPlaced>>::apply(&mut order, event);
        order
    }
}

struct PlaceOrder {
    id: OrderId,
}

impl HandleCreate<PlaceOrder> for Order {
    type HandleCreateError = Self::Error;

    fn handle_create(command: &PlaceOrder) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        Ok(vec![
            OrderPlaced {
                order_id: command.id.0,
            }
            .into(),
        ])
    }
}

struct AddOrderLine {
    sku: String,
    quantity: u32,
}

impl Handle<AddOrderLine> for Order {
    type HandleError = Self::Error;

    fn handle(&self, command: &AddOrderLine) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            OrderLineAdded {
                sku: command.sku.clone(),
                quantity: command.quantity,
            }
            .into(),
        ])
    }
}

// ============================================================================
// Custom Snapshot Stores for Testing
// ============================================================================

#[derive(Debug, Error)]
#[error("snapshot load failed")]
struct SnapshotLoadError;

#[derive(Debug)]
struct FailingLoadSnapshotStore;

impl SnapshotStore<String> for FailingLoadSnapshotStore {
    type Error = SnapshotLoadError;
    type Position = u64;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the explicit Future form would make the Send bound depend on T"
    )]
    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        Err(SnapshotLoadError)
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "async keeps this test double equivalent to the trait future contract"
    )]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[derive(Debug, Default)]
struct CorruptSnapshotStore;

impl SnapshotStore<String> for CorruptSnapshotStore {
    type Error = SnapshotLoadError;
    type Position = u64;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the explicit Future form would make the Send bound depend on T"
    )]
    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        Err(SnapshotLoadError)
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "async keeps this test double equivalent to the trait future contract"
    )]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

#[derive(Debug)]
struct TrackingSnapshotStore {
    load_called: AtomicBool,
}

impl TrackingSnapshotStore {
    const fn new() -> Self {
        Self {
            load_called: AtomicBool::new(false),
        }
    }

    fn load_called(&self) -> bool {
        self.load_called.load(Ordering::Relaxed)
    }
}

impl SnapshotStore<String> for TrackingSnapshotStore {
    type Error = Infallible;
    type Position = u64;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the explicit Future form would make the Send bound depend on T"
    )]
    async fn load<T>(
        &self,
        _: &str,
        _: &String,
    ) -> Result<Option<Snapshot<Self::Position, T>>, Self::Error>
    where
        T: DeserializeOwned,
    {
        self.load_called.store(true, Ordering::Relaxed);
        Ok(None)
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "async keeps this test double equivalent to the trait future contract"
    )]
    async fn offer_snapshot<CE, T, Create>(
        &self,
        _: &str,
        _: &String,
        _: u64,
        _: Create,
    ) -> Result<SnapshotOffer, OfferSnapshotError<Self::Error, CE>>
    where
        CE: std::error::Error + Send + Sync + 'static,
        T: Serialize,
        Create: FnOnce() -> Result<Snapshot<Self::Position, T>, CE>,
    {
        Ok(SnapshotOffer::Declined)
    }
}

// ============================================================================
// Test Domain: CounterTotal projection
// ============================================================================

/// A per-counter read model with snapshot support, used to exercise
/// `load_projection_with_snapshot`.
#[derive(Default, Serialize, Deserialize)]
struct CounterTotal {
    total: i32,
}

impl Projection for CounterTotal {
    type Id = String;
    type InstanceId = String;
    type Metadata = ();

    const KIND: &'static str = "counter-total";

    fn init(_instance_id: &String) -> Self {
        Self::default()
    }

    fn filters<S>(instance_id: &String) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new().events_for::<Counter>(instance_id)
    }
}

impl ApplyProjection<CounterEvent> for CounterTotal {
    fn apply_projection(&mut self, _ctx: EventContext<'_, String, ()>, event: &CounterEvent) {
        let CounterEvent::ValueAdded(e) = event;
        self.total += e.amount;
    }
}

// ============================================================================
// Custom store for concurrency retry testing
// ============================================================================

/// Wraps an in-memory store and injects a single artificial
/// `ConcurrencyConflict` on the first optimistic commit that has an
/// `expected_version` (i.e., the first update, not the initial create).
///
/// This lets us deterministically exercise the retry path of
/// `update_with_retry` without relying on real concurrent writes.
struct SabotageStore {
    inner: inmemory::Store<String, ()>,
    conflicted: AtomicBool,
}

impl SabotageStore {
    fn new() -> Self {
        Self {
            inner: inmemory::Store::new(),
            conflicted: AtomicBool::new(false),
        }
    }
}

impl EventStore for SabotageStore {
    type Data = JsonValue;
    type Error = inmemory::InMemoryError;
    type Id = String;
    type Metadata = ();
    type Position = u64;

    fn decode_event<E>(
        &self,
        stored: &StoredEvent<String, u64, JsonValue, ()>,
    ) -> Result<E, inmemory::InMemoryError>
    where
        E: DomainEvent + DeserializeOwned,
    {
        self.inner.decode_event(stored)
    }

    fn stream_version<'a>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a String,
    ) -> impl Future<Output = Result<Option<u64>, inmemory::InMemoryError>> + Send + 'a {
        self.inner.stream_version(aggregate_kind, aggregate_id)
    }

    fn commit_events<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a String,
        events: NonEmpty<E>,
        _metadata: &'a (),
    ) -> impl Future<Output = Result<Committed<u64>, CommitError<inmemory::InMemoryError>>> + Send + 'a
    where
        E: EventKind + Serialize + Send + Sync + 'a,
        (): Clone,
    {
        self.inner
            .commit_events(aggregate_kind, aggregate_id, events, &())
    }

    fn commit_events_optimistic<'a, E>(
        &'a self,
        aggregate_kind: &'a str,
        aggregate_id: &'a String,
        expected_version: Option<u64>,
        events: NonEmpty<E>,
        _metadata: &'a (),
    ) -> impl Future<
        Output = Result<Committed<u64>, OptimisticCommitError<u64, inmemory::InMemoryError>>,
    > + Send
    + 'a
    where
        E: EventKind + Serialize + Send + Sync + 'a,
        (): Clone,
    {
        let kind = aggregate_kind.to_string();
        let id = aggregate_id.clone();
        let inner = self.inner.clone();
        // Only sabotage the first update (expected_version is Some for updates,
        // None for creates). Compare-exchange atomically claims the sabotage slot.
        let inject = expected_version.is_some()
            && self
                .conflicted
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        async move {
            if inject {
                Err(ConcurrencyConflict {
                    expected: expected_version,
                    actual: expected_version.map(|v| v + 1),
                }
                .into())
            } else {
                inner
                    .commit_events_optimistic(&kind, &id, expected_version, events, &())
                    .await
            }
        }
    }

    fn load_events<'a>(
        &'a self,
        filters: &'a [EventFilter<String, u64>],
    ) -> impl Future<
        Output = Result<Vec<StoredEvent<String, u64, JsonValue, ()>>, inmemory::InMemoryError>,
    > + Send
    + 'a {
        self.inner.load_events(filters)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn per_aggregate_ids_share_one_string_store() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store).without_concurrency_checking();

    let product_id = ProductId("42".to_string());
    let order_id = OrderId(42);

    repo.create::<Product, CreateProduct>(
        &product_id,
        &CreateProduct {
            id: product_id.clone(),
            name: "Widget".to_string(),
        },
        &(),
    )
    .await
    .unwrap();
    repo.create::<Order, PlaceOrder>(&order_id, &PlaceOrder { id: order_id }, &())
        .await
        .unwrap();

    repo.update::<Product, RenameProduct>(
        &product_id,
        &RenameProduct {
            name: "Widget Pro".to_string(),
        },
        &(),
    )
    .await
    .unwrap();
    repo.update::<Order, AddOrderLine>(
        &order_id,
        &AddOrderLine {
            sku: "42".to_string(),
            quantity: 3,
        },
        &(),
    )
    .await
    .unwrap();

    let product = repo
        .load::<Product>(&product_id)
        .await
        .unwrap()
        .expect("product exists");
    let order = repo
        .load::<Order>(&order_id)
        .await
        .unwrap()
        .expect("order exists");

    assert_eq!(product.name, "Widget Pro");
    assert_eq!(product.rename_count, 1);
    assert_eq!(order.total_quantity, 3);
    let product_key: String = product_id.to_key();
    let order_key: String = order_id.to_key();
    assert_ne!(product_key, order_key);
    assert!(
        repo.event_store()
            .stream_version(Product::KIND, &product_key)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.event_store()
            .stream_version(Order::KIND, &order_key)
            .await
            .unwrap()
            .is_some()
    );
}

/// A per-order read model scoped to one `Order` instance. The store is keyed on
/// `String`, but `Order` keys on the `OrderId` newtype — exercising
/// `Filters::events_for::<Order>` with an `&OrderId`, the case the old
/// `A: Aggregate<Id = S::Id>` bound made impossible.
#[derive(Default)]
struct OrderSummary {
    lines: u32,
    quantity: u32,
}

impl Projection for OrderSummary {
    type Id = String;
    type InstanceId = OrderId;
    type Metadata = ();

    const KIND: &'static str = "order-summary";

    fn init(_instance_id: &OrderId) -> Self {
        Self::default()
    }

    fn filters<S>(instance_id: &OrderId) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new().events_for::<Order>(instance_id)
    }
}

impl ApplyProjection<OrderEvent> for OrderSummary {
    fn apply_projection(&mut self, _ctx: EventContext<'_, String, ()>, event: &OrderEvent) {
        match event {
            OrderEvent::OrderPlaced(_) => {}
            OrderEvent::OrderLineAdded(line) => {
                self.lines += 1;
                self.quantity += line.quantity;
            }
        }
    }
}

#[tokio::test]
async fn scoped_projection_over_newtype_keyed_aggregate() {
    let store = inmemory::Store::<String, ()>::new();
    let repo = Repository::new(store).without_concurrency_checking();

    let target = OrderId(1);
    let other = OrderId(2);

    for id in [target, other] {
        repo.create::<Order, PlaceOrder>(&id, &PlaceOrder { id }, &())
            .await
            .unwrap();
    }
    repo.update::<Order, AddOrderLine>(
        &target,
        &AddOrderLine {
            sku: "a".to_string(),
            quantity: 3,
        },
        &(),
    )
    .await
    .unwrap();
    // A line on a *different* order instance that the scoped filter must exclude.
    repo.update::<Order, AddOrderLine>(
        &other,
        &AddOrderLine {
            sku: "b".to_string(),
            quantity: 99,
        },
        &(),
    )
    .await
    .unwrap();

    let summary = repo.load_projection::<OrderSummary>(&target).await.unwrap();

    assert_eq!(summary.lines, 1, "only the target order's line is counted");
    assert_eq!(
        summary.quantity, 3,
        "the other order's quantity is excluded"
    );
}

#[tokio::test]
async fn per_aggregate_ids_share_snapshot_store() {
    let store = inmemory::Store::<String, ()>::new();
    let snapshots = InMemorySnapshotStore::<u64>::always();
    let repo = Repository::new(store).with_snapshots(snapshots);

    let product_id = ProductId("42".to_string());
    let order_id = OrderId(42);

    repo.create::<Product, CreateProduct>(
        &product_id,
        &CreateProduct {
            id: product_id.clone(),
            name: "Widget".to_string(),
        },
        &(),
    )
    .await
    .unwrap();
    repo.create::<Order, PlaceOrder>(&order_id, &PlaceOrder { id: order_id }, &())
        .await
        .unwrap();
    repo.update::<Product, RenameProduct>(
        &product_id,
        &RenameProduct {
            name: "Widget Pro".to_string(),
        },
        &(),
    )
    .await
    .unwrap();
    repo.update::<Order, AddOrderLine>(
        &order_id,
        &AddOrderLine {
            sku: "42".to_string(),
            quantity: 3,
        },
        &(),
    )
    .await
    .unwrap();

    let product_key: String = product_id.to_key();
    let order_key: String = order_id.to_key();

    let product_snapshot = repo
        .snapshot_store()
        .load::<Product>(Product::KIND, &product_key)
        .await
        .unwrap()
        .expect("product snapshot exists");
    let order_snapshot = repo
        .snapshot_store()
        .load::<Order>(Order::KIND, &order_key)
        .await
        .unwrap()
        .expect("order snapshot exists");

    assert_eq!(product_snapshot.data.name, "Widget Pro");
    assert_eq!(order_snapshot.data.total_quantity, 3);

    let product = repo
        .load::<Product>(&product_id)
        .await
        .unwrap()
        .expect("product resumes from snapshot");
    let order = repo
        .load::<Order>(&order_id)
        .await
        .unwrap()
        .expect("order resumes from snapshot");

    assert_eq!(product.name, product_snapshot.data.name);
    assert_eq!(order.total_quantity, order_snapshot.data.total_quantity);
}

#[tokio::test]
async fn saves_snapshot_and_exposes_snapshot_store() {
    let store = inmemory::Store::new();
    let snapshots = InMemorySnapshotStore::<u64>::always();
    let repo = Repository::new(store)
        .with_snapshots(snapshots)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 5 }, &())
        .await
        .unwrap();

    let loaded = repo
        .snapshot_store()
        .load::<Counter>(Counter::KIND, &id)
        .await
        .unwrap();
    assert!(loaded.is_some());
}

#[tokio::test]
async fn no_events_does_not_persist_or_snapshot() {
    let store = inmemory::Store::new();
    let snapshots = InMemorySnapshotStore::<u64>::always();
    let repo = Repository::new(store)
        .with_snapshots(snapshots)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.create::<Counter, NoOp>(&id, &NoOp, &()).await.unwrap();

    assert!(
        repo.event_store()
            .stream_version(Counter::KIND, &id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.snapshot_store()
            .load::<Counter>(Counter::KIND, &id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn snapshot_load_failure_falls_back_to_full_replay() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store)
        .with_snapshots(FailingLoadSnapshotStore)
        .without_concurrency_checking();

    let id = "c1".to_string();
    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 10 }, &())
        .await
        .unwrap();

    repo.update::<Counter, RequireAtLeast>(&id, &RequireAtLeast { min: 5 }, &())
        .await
        .unwrap();
}

#[tokio::test]
async fn corrupt_snapshot_ignores_error() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store)
        .with_snapshots(CorruptSnapshotStore)
        .without_concurrency_checking();

    repo.create::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 1 }, &())
        .await
        .unwrap();
}

#[tokio::test]
async fn retry_with_zero_retries_still_attempts_once() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    let id = "c1".to_string();

    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 1 }, &())
        .await
        .unwrap();

    let attempts = repo
        .update_with_retry::<Counter, AddValue>(&id, &AddValue { amount: 1 }, &(), 0)
        .await
        .unwrap();

    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn retry_surfaces_non_concurrency_errors() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);
    let id = "c1".to_string();

    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 1 }, &())
        .await
        .unwrap();

    let err = repo
        .update_with_retry::<Counter, AddValue>(&id, &AddValue { amount: 0 }, &(), 3)
        .await
        .unwrap_err();

    assert!(matches!(err, CommandError::Aggregate(_)));
}

#[tokio::test]
async fn upsert_creates_when_stream_does_not_exist() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store).without_concurrency_checking();
    let id = "c1".to_string();

    repo.upsert::<Counter, AddValue>(&id, &AddValue { amount: 5 }, &())
        .await
        .unwrap();

    let counter: Counter = repo.load(&id).await.unwrap().expect("counter should exist");
    assert_eq!(counter.value, 5);
}

#[tokio::test]
async fn upsert_updates_when_stream_already_exists() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store).without_concurrency_checking();
    let id = "c1".to_string();

    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 3 }, &())
        .await
        .unwrap();

    repo.upsert::<Counter, AddValue>(&id, &AddValue { amount: 7 }, &())
        .await
        .unwrap();

    let counter: Counter = repo.load(&id).await.unwrap().expect("counter should exist");
    assert_eq!(counter.value, 10);
}

#[tokio::test]
async fn load_consults_snapshot_store() {
    let store = inmemory::Store::<String, ()>::new();
    let snapshots = TrackingSnapshotStore::new();
    let repo = Repository::new(store).with_snapshots(snapshots);

    // No events have been committed, so load returns None.
    let counter: Option<Counter> = repo.load(&"c1".to_string()).await.unwrap();

    assert!(counter.is_none());
    assert!(repo.snapshot_store().load_called());
}

#[tokio::test]
async fn load_projection_with_snapshot_stores_and_resumes_from_snapshot() {
    let store = inmemory::Store::<String, ()>::new();
    let snapshots = InMemorySnapshotStore::<u64>::always();
    let repo = Repository::new(store).without_concurrency_checking();

    let id = "c1".to_string();
    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 3 }, &())
        .await
        .unwrap();
    repo.update::<Counter, AddValue>(&id, &AddValue { amount: 7 }, &())
        .await
        .unwrap();

    // First load: no snapshot yet — replays from events and offers a snapshot.
    let total: CounterTotal = repo
        .load_projection_with_snapshot(&id, &snapshots)
        .await
        .unwrap();
    assert_eq!(total.total, 10);

    assert!(
        snapshots
            .load::<CounterTotal>(CounterTotal::KIND, &id)
            .await
            .unwrap()
            .is_some(),
        "snapshot must be stored after first projection load"
    );

    // Add a new event after the snapshot was taken.
    repo.update::<Counter, AddValue>(&id, &AddValue { amount: 5 }, &())
        .await
        .unwrap();

    // Second load: resumes from the snapshot and applies only the new event.
    let total2: CounterTotal = repo
        .load_projection_with_snapshot(&id, &snapshots)
        .await
        .unwrap();
    assert_eq!(
        total2.total, 15,
        "snapshot resume must apply only post-snapshot events"
    );
}

#[tokio::test]
async fn update_with_retry_retries_on_concurrency_conflict() {
    let repo = Repository::new(SabotageStore::new());
    let id = "c1".to_string();

    repo.create::<Counter, AddValue>(&id, &AddValue { amount: 1 }, &())
        .await
        .unwrap();

    // The SabotageStore injects a conflict on the first update commit.
    // With max_retries=1 the second attempt sees the real store and succeeds.
    let attempts = repo
        .update_with_retry::<Counter, AddValue>(&id, &AddValue { amount: 1 }, &(), 1)
        .await
        .expect("update must succeed after one retry");

    assert_eq!(
        attempts, 2,
        "exactly two attempts: one conflict, one success"
    );

    let counter: Counter = repo.load(&id).await.unwrap().expect("counter must exist");
    assert_eq!(
        counter.value, 2,
        "both the create and the retried update are applied"
    );
}
