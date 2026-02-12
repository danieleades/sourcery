//! Integration tests for projection functionality.

#![cfg(feature = "test-util")]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, DomainEvent, EventKind, Filters, Handle, Projection,
    ProjectionFilters, Repository,
    projection::ProjectionError,
    store::{EventStore, inmemory},
    test::RepositoryTestExt,
};

// ============================================================================
// Test Domain: Counter with Projections
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ValueAdded {
    amount: i32,
}

impl DomainEvent for ValueAdded {
    const KIND: &'static str = "value-added";
}

/// Test helper that serialises to invalid JSON for testing deserialisation
/// error handling. Uses the same KIND as `ValueAdded` but serialises to a
/// string instead of an object.
struct InvalidValueAdded;

impl EventKind for InvalidValueAdded {
    fn kind(&self) -> &'static str {
        ValueAdded::KIND
    }
}

impl serde::Serialize for InvalidValueAdded {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("not-an-object")
    }
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(ValueAdded),
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

struct AddValue {
    amount: i32,
}

impl Handle<AddValue> for Counter {
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

// ============================================================================
// Test Projection
// ============================================================================

#[derive(Debug, Default, Projection)]
#[projection(events(ValueAdded))]
struct TotalsProjection {
    totals: HashMap<String, i32>,
}

impl ApplyProjection<ValueAdded> for TotalsProjection {
    fn apply_projection(
        &mut self,
        aggregate_id: &Self::Id,
        event: &ValueAdded,
        &(): &Self::Metadata,
    ) {
        *self.totals.entry(aggregate_id.clone()).or_insert(0) += event.amount;
    }
}

// ============================================================================
// Filtered projection for event_for tests
// ============================================================================

#[derive(Debug, Default, Projection)]
struct FilteredTotalsProjection {
    totals: HashMap<String, i32>,
}

impl ProjectionFilters for FilteredTotalsProjection {
    type Id = String;
    type InstanceId = String;
    type Metadata = ();

    fn init(_id: &String) -> Self {
        Self::default()
    }

    fn filters<S>(aggregate_id: &String) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new().event_for::<Counter, ValueAdded>(aggregate_id)
    }
}

impl ApplyProjection<ValueAdded> for FilteredTotalsProjection {
    fn apply_projection(
        &mut self,
        aggregate_id: &Self::Id,
        event: &ValueAdded,
        &(): &Self::Metadata,
    ) {
        *self.totals.entry(aggregate_id.clone()).or_insert(0) += event.amount;
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn event_for_filters_by_aggregate() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 10 }, &())
        .await
        .unwrap();
    repo.execute_command::<Counter, AddValue>(&"c2".to_string(), &AddValue { amount: 20 }, &())
        .await
        .unwrap();

    let projection: FilteredTotalsProjection =
        repo.load_projection(&"c1".to_string()).await.unwrap();

    assert_eq!(projection.totals.get("c1"), Some(&10));
    assert!(!projection.totals.contains_key("c2"));
}

#[tokio::test]
async fn load_surfaces_deserialization_error() {
    let store = inmemory::Store::new();
    let mut repo = Repository::new(store);

    // Inject malformed JSON data that won't deserialize to ValueAdded
    repo.inject_event(Counter::KIND, &"c1".to_string(), InvalidValueAdded, ())
        .await
        .unwrap();

    let err = repo
        .load_projection::<TotalsProjection>(&())
        .await
        .unwrap_err();

    // Should get EventDecode error with Store variant inside
    match err {
        ProjectionError::EventDecode(_) => {
            // Test passes - we got the expected error variant
        }
        other @ ProjectionError::Store(_) => panic!("unexpected error: {other:?}"),
    }
}

// ============================================================================
// Tests for events() method with aggregate event enums
//
// The `events()` and `events_for()` builder methods load all events from an
// aggregate's event enum in a single call. This requires implementing
// `ApplyProjection` for the enum type, which dispatches to individual handlers.
//
// This API is useful when:
// - An aggregate replays its own history
// - A projection is tightly coupled to one aggregate and wants all its events
//
// For most projections, prefer chaining `.event::<E>()` calls instead - this
// targets specific event types without needing the enum dispatch impl.
// ============================================================================

/// Second event type to test multi-event aggregate enums
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ValueReset {
    new_value: i32,
}

impl DomainEvent for ValueReset {
    const KIND: &'static str = "value-reset";
}

#[derive(Default, Clone, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = String,
    error = String,
    events(ValueAdded, ValueReset),
    derives(Debug, PartialEq, Eq)
)]
struct MultiEventCounter {
    value: i32,
}

impl Apply<ValueAdded> for MultiEventCounter {
    fn apply(&mut self, event: &ValueAdded) {
        self.value += event.amount;
    }
}

impl Apply<ValueReset> for MultiEventCounter {
    fn apply(&mut self, event: &ValueReset) {
        self.value = event.new_value;
    }
}

struct ResetValue {
    new_value: i32,
}

impl Handle<AddValue> for MultiEventCounter {
    fn handle(&self, command: &AddValue) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            ValueAdded {
                amount: command.amount,
            }
            .into(),
        ])
    }
}

impl Handle<ResetValue> for MultiEventCounter {
    fn handle(&self, command: &ResetValue) -> Result<Vec<Self::Event>, Self::Error> {
        Ok(vec![
            ValueReset {
                new_value: command.new_value,
            }
            .into(),
        ])
    }
}

/// Projection using `events()` to load via the aggregate's event enum
#[derive(Debug, Default, Projection)]
struct EnumProjection {
    additions: i32,
    resets: i32,
    last_value: Option<i32>,
}

impl ProjectionFilters for EnumProjection {
    type Id = String;
    type InstanceId = ();
    type Metadata = ();

    fn init((): &()) -> Self {
        Self::default()
    }

    fn filters<S>((): &()) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new().events::<MultiEventCounterEvent>()
    }
}

impl ApplyProjection<MultiEventCounterEvent> for EnumProjection {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &MultiEventCounterEvent,
        &(): &Self::Metadata,
    ) {
        match event {
            MultiEventCounterEvent::ValueAdded(e) => {
                self.additions += e.amount;
            }
            MultiEventCounterEvent::ValueReset(e) => {
                self.resets += 1;
                self.last_value = Some(e.new_value);
            }
        }
    }
}

/// Projection using `events_for()` to load from a specific aggregate instance
#[derive(Debug, Default, Projection)]
struct EventsForProjection {
    additions: i32,
    resets: i32,
    last_value: Option<i32>,
}

impl ProjectionFilters for EventsForProjection {
    type Id = String;
    type InstanceId = String;
    type Metadata = ();

    fn init(_id: &String) -> Self {
        Self::default()
    }

    fn filters<S>(aggregate_id: &String) -> Filters<S, Self>
    where
        S: EventStore<Id = String, Metadata = ()>,
    {
        Filters::new().events_for::<MultiEventCounter>(aggregate_id)
    }
}

impl ApplyProjection<MultiEventCounterEvent> for EventsForProjection {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &MultiEventCounterEvent,
        &(): &Self::Metadata,
    ) {
        match event {
            MultiEventCounterEvent::ValueAdded(e) => {
                self.additions += e.amount;
            }
            MultiEventCounterEvent::ValueReset(e) => {
                self.resets += 1;
                self.last_value = Some(e.new_value);
            }
        }
    }
}

#[tokio::test]
async fn events_loads_all_events_from_aggregate_enum() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    // Execute commands that produce different event types
    repo.execute_command::<MultiEventCounter, AddValue>(
        &"c1".to_string(),
        &AddValue { amount: 10 },
        &(),
    )
    .await
    .unwrap();

    repo.execute_command::<MultiEventCounter, ResetValue>(
        &"c1".to_string(),
        &ResetValue { new_value: 5 },
        &(),
    )
    .await
    .unwrap();

    repo.execute_command::<MultiEventCounter, AddValue>(
        &"c1".to_string(),
        &AddValue { amount: 3 },
        &(),
    )
    .await
    .unwrap();

    // Use events() via the EnumProjection's ProjectionFilters impl
    let projection: EnumProjection = repo.load_projection(&()).await.unwrap();

    assert_eq!(projection.additions, 13); // 10 + 3
    assert_eq!(projection.resets, 1);
    assert_eq!(projection.last_value, Some(5));
}

#[tokio::test]
async fn events_for_loads_events_for_specific_aggregate_instance() {
    let store = inmemory::Store::new();
    let repo = Repository::new(store);

    // Create events for two different aggregate instances
    repo.execute_command::<MultiEventCounter, AddValue>(
        &"c1".to_string(),
        &AddValue { amount: 10 },
        &(),
    )
    .await
    .unwrap();

    repo.execute_command::<MultiEventCounter, AddValue>(
        &"c2".to_string(),
        &AddValue { amount: 20 },
        &(),
    )
    .await
    .unwrap();

    repo.execute_command::<MultiEventCounter, ResetValue>(
        &"c1".to_string(),
        &ResetValue { new_value: 5 },
        &(),
    )
    .await
    .unwrap();

    // Use events_for() via the EventsForProjection's ProjectionFilters impl
    let projection: EventsForProjection = repo.load_projection(&"c1".to_string()).await.unwrap();

    assert_eq!(projection.additions, 10); // only c1's addition
    assert_eq!(projection.resets, 1);
    assert_eq!(projection.last_value, Some(5));
}

// ============================================================================
// Tests for projection snapshots (exercises Snapshots wrapper delegation)
// ============================================================================

/// Projection with Serialize/Deserialize for snapshot support.
/// Uses String as both aggregate ID and instance ID for simplicity.
#[derive(Debug, Default, Serialize, Deserialize, Projection)]
#[projection(instance_id = String, events(ValueAdded))]
struct SnapshotProjection {
    total: i32,
    event_count: u32,
}

impl ApplyProjection<ValueAdded> for SnapshotProjection {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &ValueAdded,
        &(): &Self::Metadata,
    ) {
        self.total += event.amount;
        self.event_count += 1;
    }
}

#[tokio::test]
async fn projection_with_snapshot_exercises_snapshots_wrapper_load() {
    use sourcery::snapshot::inmemory::Store as SnapshotStore;

    let store = inmemory::Store::new();
    // Snapshot store's ID type must match the projection's instance_id type
    // (String)
    let snapshots = SnapshotStore::<String, u64>::always();
    let repo = Repository::new(store).with_snapshots(snapshots);

    // Add some events
    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 10 }, &())
        .await
        .unwrap();
    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 20 }, &())
        .await
        .unwrap();

    // Build projection with snapshot support
    let projection: SnapshotProjection = repo
        .load_projection_with_snapshot(&"proj-instance".to_string())
        .await
        .unwrap();

    assert_eq!(projection.total, 30);
    assert_eq!(projection.event_count, 2);
}

#[tokio::test]
async fn projection_with_snapshot_offers_snapshot_after_load() {
    use sourcery::snapshot::inmemory::Store as SnapshotStore;

    let store = inmemory::Store::new();
    let snapshots = SnapshotStore::<String, u64>::always();
    let repo = Repository::new(store).with_snapshots(snapshots);

    // Add events
    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 5 }, &())
        .await
        .unwrap();

    // Load projection with snapshot - this should offer a snapshot after load
    let instance_id = "proj-instance".to_string();
    let projection: SnapshotProjection = repo
        .load_projection_with_snapshot(&instance_id)
        .await
        .unwrap();

    assert_eq!(projection.total, 5);

    // Add more events
    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 7 }, &())
        .await
        .unwrap();

    // Load again - should now use the snapshot from before
    let projection: SnapshotProjection = repo
        .load_projection_with_snapshot(&instance_id)
        .await
        .unwrap();

    // Should have both events (5 + 7)
    assert_eq!(projection.total, 12);
    assert_eq!(projection.event_count, 2);
}
