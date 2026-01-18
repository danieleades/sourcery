//! Integration tests for projection functionality.

#![cfg(feature = "test-util")]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, DomainEvent, EventKind, Handle, Projection, Repository,
    projection::ProjectionError, store::inmemory, test::RepositoryTestExt,
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

/// Test helper that serializes to invalid JSON for testing deserialization
/// error handling. Uses the same KIND as `ValueAdded` but serializes to a
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
#[projection(id = String)]
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

    let projection: TotalsProjection = repo
        .build_projection()
        .event_for::<Counter, ValueAdded>(&"c1".to_string())
        .load()
        .await
        .unwrap();

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
        .build_projection::<TotalsProjection>()
        .event::<ValueAdded>()
        .load()
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

/// Projection that handles multiple event types via aggregate event enum
#[derive(Debug, Default, Projection)]
#[projection(id = String)]
struct MultiEventProjection {
    additions: i32,
    resets: i32,
    last_value: Option<i32>,
}

impl ApplyProjection<ValueAdded> for MultiEventProjection {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &ValueAdded,
        &(): &Self::Metadata,
    ) {
        self.additions += event.amount;
    }
}

impl ApplyProjection<ValueReset> for MultiEventProjection {
    fn apply_projection(
        &mut self,
        _aggregate_id: &Self::Id,
        event: &ValueReset,
        &(): &Self::Metadata,
    ) {
        self.resets += 1;
        self.last_value = Some(event.new_value);
    }
}

/// Implementation for the aggregate event enum - dispatches to individual
/// handlers
impl ApplyProjection<MultiEventCounterEvent> for MultiEventProjection {
    fn apply_projection(
        &mut self,
        aggregate_id: &Self::Id,
        event: &MultiEventCounterEvent,
        metadata: &Self::Metadata,
    ) {
        match event {
            MultiEventCounterEvent::ValueAdded(e) => {
                self.apply_projection(aggregate_id, e, metadata)
            }
            MultiEventCounterEvent::ValueReset(e) => {
                self.apply_projection(aggregate_id, e, metadata)
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

    // Use events() to load via the aggregate's event enum
    let projection: MultiEventProjection = repo
        .build_projection()
        .events::<MultiEventCounterEvent>()
        .load()
        .await
        .unwrap();

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

    // Use events_for() to load only events for c1
    let projection: MultiEventProjection = repo
        .build_projection()
        .events_for::<MultiEventCounter>(&"c1".to_string())
        .load()
        .await
        .unwrap();

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
#[projection(id = String, instance_id = String)]
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

    // Build projection with snapshot support - exercises Snapshots::load()
    let projection: SnapshotProjection = repo
        .build_projection()
        .event::<ValueAdded>()
        .with_snapshot()
        .load_for(&"proj-instance".to_string())
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
        .build_projection()
        .event::<ValueAdded>()
        .with_snapshot()
        .load_for(&instance_id)
        .await
        .unwrap();

    assert_eq!(projection.total, 5);

    // Add more events
    repo.execute_command::<Counter, AddValue>(&"c1".to_string(), &AddValue { amount: 7 }, &())
        .await
        .unwrap();

    // Load again - should now use the snapshot from before
    let projection: SnapshotProjection = repo
        .build_projection()
        .event::<ValueAdded>()
        .with_snapshot()
        .load_for(&instance_id)
        .await
        .unwrap();

    // Should have both events (5 + 7)
    assert_eq!(projection.total, 12);
    assert_eq!(projection.event_count, 2);
}
