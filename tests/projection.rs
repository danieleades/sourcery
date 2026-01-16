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
    repo.inject_event(Counter::KIND, &"c1".to_string(), &InvalidValueAdded, ())
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
