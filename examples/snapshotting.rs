//! Snapshotting Example
//!
//! Demonstrates how to use snapshots to optimize aggregate loading for
//! long-lived aggregates with many events.
//!
//! This example shows:
//! - **Snapshot configuration**: Using `InMemorySnapshotStore` with different
//!   policies
//! - **Automatic snapshotting**: Snapshots created after N events
//! - **Snapshot-based loading**: Aggregate restored from snapshot + delta
//!   events
//! - **Policy comparison**: Comparing always, every-N, and never policies
//!
//! Run with: `cargo run --example snapshotting`

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sourcery::{
    Aggregate, Apply, ApplyProjection, DomainEvent, Handle, Repository,
    snapshot::InMemorySnapshotStore,
    store::{JsonCodec, inmemory},
};

// =============================================================================
// Domain Events
// =============================================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PointsEarned {
    pub amount: u64,
    pub reason: String,
}

impl DomainEvent for PointsEarned {
    const KIND: &'static str = "loyalty.points.earned";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PointsRedeemed {
    pub amount: u64,
    pub reward: String,
}

impl DomainEvent for PointsRedeemed {
    const KIND: &'static str = "loyalty.points.redeemed";
}

// =============================================================================
// Loyalty Account Aggregate
// =============================================================================

/// A loyalty account that accumulates points over time.
///
/// This is a good candidate for snapshotting because:
/// - Long-lived (customers stay for years)
/// - Many events (every purchase earns points)
/// - Simple state (just a balance)
#[derive(Debug, Default, Serialize, Deserialize, Aggregate)]
#[aggregate(id = String, error = String, events(PointsEarned, PointsRedeemed))]
pub struct LoyaltyAccount {
    points: u64,
    lifetime_earned: u64,
    lifetime_redeemed: u64,
}

impl LoyaltyAccount {
    #[must_use]
    pub const fn points(&self) -> u64 {
        self.points
    }

    #[must_use]
    pub const fn lifetime_earned(&self) -> u64 {
        self.lifetime_earned
    }

    #[must_use]
    pub const fn lifetime_redeemed(&self) -> u64 {
        self.lifetime_redeemed
    }
}

impl Apply<PointsEarned> for LoyaltyAccount {
    fn apply(&mut self, event: &PointsEarned) {
        self.points += event.amount;
        self.lifetime_earned += event.amount;
    }
}

impl Apply<PointsRedeemed> for LoyaltyAccount {
    fn apply(&mut self, event: &PointsRedeemed) {
        self.points = self.points.saturating_sub(event.amount);
        self.lifetime_redeemed += event.amount;
    }
}

// =============================================================================
// Commands
// =============================================================================

pub struct EarnPoints {
    pub amount: u64,
    pub reason: String,
}

pub struct RedeemPoints {
    pub amount: u64,
    pub reward: String,
}

impl Handle<EarnPoints> for LoyaltyAccount {
    fn handle(&self, command: &EarnPoints) -> Result<Vec<Self::Event>, Self::Error> {
        if command.amount == 0 {
            return Err("Cannot earn zero points".to_string());
        }
        Ok(vec![
            PointsEarned {
                amount: command.amount,
                reason: command.reason.clone(),
            }
            .into(),
        ])
    }
}

impl Handle<RedeemPoints> for LoyaltyAccount {
    fn handle(&self, command: &RedeemPoints) -> Result<Vec<Self::Event>, Self::Error> {
        if command.amount > self.points {
            return Err(format!(
                "Insufficient points: have {}, need {}",
                self.points, command.amount
            ));
        }
        Ok(vec![
            PointsRedeemed {
                amount: command.amount,
                reward: command.reward.clone(),
            }
            .into(),
        ])
    }
}

// =============================================================================
// Projection (Snapshotting)
// =============================================================================

#[derive(Debug, Default, Serialize, Deserialize, sourcery::Projection)]
#[projection(id = String, kind = "loyalty.summary")]
pub struct LoyaltySummary {
    total_earned: u64,
    total_redeemed: u64,
    customer_points: HashMap<String, u64>,
}

impl ApplyProjection<PointsEarned> for LoyaltySummary {
    fn apply_projection(&mut self, aggregate_id: &Self::Id, event: &PointsEarned, &(): &()) {
        self.total_earned += event.amount;
        *self
            .customer_points
            .entry(aggregate_id.clone())
            .or_default() += event.amount;
    }
}

impl ApplyProjection<PointsRedeemed> for LoyaltySummary {
    fn apply_projection(&mut self, aggregate_id: &Self::Id, event: &PointsRedeemed, &(): &()) {
        self.total_redeemed += event.amount;
        let entry = self
            .customer_points
            .entry(aggregate_id.clone())
            .or_default();
        *entry = entry.saturating_sub(event.amount);
    }
}

// =============================================================================
// Example
// =============================================================================

type ExampleResult = Result<(), Box<dyn std::error::Error>>;

async fn run_repository_without_snapshots() -> ExampleResult {
    println!("1. Repository without snapshots (default behavior)");

    let event_store = inmemory::Store::new(JsonCodec);
    let repo = Repository::new(event_store);
    let customer_id = "CUST-001".to_string();

    for i in 1..=10 {
        repo.execute_command::<LoyaltyAccount, EarnPoints>(
            &customer_id,
            &EarnPoints {
                amount: 100,
                reason: format!("Purchase #{i}"),
            },
            &(),
        )
        .await?;
    }

    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!(
        "   Points balance: {} (replayed 10 events)",
        account.points()
    );
    println!();

    Ok(())
}

async fn run_always_snapshot_policy() -> ExampleResult {
    println!("2. Repository with always-snapshot policy");

    let event_store = inmemory::Store::new(JsonCodec);
    let snapshot_store = InMemorySnapshotStore::always();
    let repo = Repository::new(event_store).with_snapshots(snapshot_store);
    let customer_id = "CUST-002".to_string();

    for i in 1..=5 {
        repo.execute_command::<LoyaltyAccount, EarnPoints>(
            &customer_id,
            &EarnPoints {
                amount: 200,
                reason: format!("Purchase #{i}"),
            },
            &(),
        )
        .await?;
        println!("   After purchase #{i}: snapshot saved");
    }

    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!(
        "   Final points: {} (loaded from snapshot + 0 events)",
        account.points()
    );
    println!();

    Ok(())
}

async fn run_every_n_snapshot_policy() -> ExampleResult {
    println!("3. Repository with every-5-events policy");

    let event_store = inmemory::Store::new(JsonCodec);
    let snapshot_store = InMemorySnapshotStore::every(5);
    let repo = Repository::new(event_store).with_snapshots(snapshot_store);
    let customer_id = "CUST-003".to_string();

    for i in 1..=12 {
        repo.execute_command::<LoyaltyAccount, EarnPoints>(
            &customer_id,
            &EarnPoints {
                amount: 50,
                reason: format!("Purchase #{i}"),
            },
            &(),
        )
        .await?;
    }

    // With 12 events and snapshots every 5:
    // - Snapshot at position ~5 or ~10 (depending on when threshold triggered)
    // - Remaining events replayed
    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!(
        "   Points balance: {} (12 events, snapshots every 5)",
        account.points()
    );
    println!("   Lifetime earned: {}", account.lifetime_earned());
    println!();

    Ok(())
}

async fn run_snapshot_restoration() -> ExampleResult {
    println!("4. Snapshot restoration after more activity");

    let event_store = inmemory::Store::new(JsonCodec);
    let snapshot_store = InMemorySnapshotStore::every(3);
    let repo = Repository::new(event_store).with_snapshots(snapshot_store);
    let customer_id = "CUST-004".to_string();

    for i in 1..=5 {
        repo.execute_command::<LoyaltyAccount, EarnPoints>(
            &customer_id,
            &EarnPoints {
                amount: 100,
                reason: format!("Earning #{i}"),
            },
            &(),
        )
        .await?;
    }

    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!("   After 5 earnings: {} points", account.points());

    repo.execute_command::<LoyaltyAccount, RedeemPoints>(
        &customer_id,
        &RedeemPoints {
            amount: 200,
            reward: "Free coffee".to_string(),
        },
        &(),
    )
    .await?;

    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!("   After redemption: {} points", account.points());
    println!("   Lifetime earned: {}", account.lifetime_earned());
    println!("   Lifetime redeemed: {}", account.lifetime_redeemed());
    println!();

    Ok(())
}

async fn run_never_snapshot_policy() -> ExampleResult {
    println!("5. Never-snapshot policy (load-only mode)");

    let event_store = inmemory::Store::new(JsonCodec);
    let snapshot_store = InMemorySnapshotStore::never();
    let repo = Repository::new(event_store).with_snapshots(snapshot_store);
    let customer_id = "CUST-005".to_string();

    for i in 1..=3 {
        repo.execute_command::<LoyaltyAccount, EarnPoints>(
            &customer_id,
            &EarnPoints {
                amount: 100,
                reason: format!("Purchase #{i}"),
            },
            &(),
        )
        .await?;
    }

    let account: LoyaltyAccount = repo.aggregate_builder().load(&customer_id).await?;
    println!(
        "   Points: {} (full replay, no snapshots saved)",
        account.points()
    );
    println!();

    Ok(())
}

async fn run_projection_snapshotting() -> ExampleResult {
    println!("6. Projection snapshotting (global ordering required)");

    let event_store = inmemory::Store::new(JsonCodec);
    let snapshot_store = InMemorySnapshotStore::every(2);
    let repo = Repository::new(event_store).with_snapshots(snapshot_store);

    let customer_a = "CUST-P1".to_string();
    let customer_b = "CUST-P2".to_string();

    repo.execute_command::<LoyaltyAccount, EarnPoints>(
        &customer_a,
        &EarnPoints {
            amount: 120,
            reason: "Signup bonus".to_string(),
        },
        &(),
    )
    .await?;
    repo.execute_command::<LoyaltyAccount, EarnPoints>(
        &customer_b,
        &EarnPoints {
            amount: 75,
            reason: "First purchase".to_string(),
        },
        &(),
    )
    .await?;
    repo.execute_command::<LoyaltyAccount, RedeemPoints>(
        &customer_a,
        &RedeemPoints {
            amount: 50,
            reward: "Discount code".to_string(),
        },
        &(),
    )
    .await?;

    let projection: LoyaltySummary = repo
        .build_projection::<LoyaltySummary>()
        .event::<PointsEarned>()
        .event::<PointsRedeemed>()
        .with_snapshot()
        .load()
        .await?;

    println!(
        "   Totals: earned {}, redeemed {}",
        projection.total_earned, projection.total_redeemed
    );
    println!(
        "   Points: {} -> {}, {} -> {}",
        customer_a,
        projection.customer_points.get(&customer_a).unwrap_or(&0u64),
        customer_b,
        projection.customer_points.get(&customer_b).unwrap_or(&0u64)
    );
    println!();

    Ok(())
}

#[tokio::main]
async fn main() -> ExampleResult {
    println!("=== Snapshotting Example ===\n");

    run_repository_without_snapshots().await?;
    run_always_snapshot_policy().await?;
    run_every_n_snapshot_policy().await?;
    run_snapshot_restoration().await?;
    run_never_snapshot_policy().await?;
    run_projection_snapshotting().await?;

    println!("=== Summary ===\n");
    println!("✓ Default repository: No snapshotting, full event replay");
    println!("✓ Always policy: Snapshot after every command (fastest loads, most storage)");
    println!("✓ Every-N policy: Balance between storage and replay cost");
    println!("✓ Never policy: Useful for read replicas consuming external snapshots");
    println!("✓ Projection snapshots: Read models can snapshot global streams");
    println!();
    println!("Snapshots are transparent - aggregate behavior is identical regardless of policy.");
    println!("The only difference is loading performance for aggregates with many events.");
    println!();

    println!("=== Example completed successfully! ===");
    Ok(())
}
