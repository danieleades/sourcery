//! The `FulfillmentCoordinator`: a checkpointed reactor that turns three
//! isolated aggregates into one workflow by reacting to committed events with
//! commands.
//!
//! There is no multi-aggregate transaction: the workflow is eventually
//! consistent. The coordinator reacts to events and issues commands, and where
//! a step cannot complete it compensates (releases reservations, cancels the
//! order). Because delivery is at-least-once with resume, every reaction must
//! be idempotent — the coordinator achieves this by reading current state
//! before acting and by relying on idempotent commands.
//!
//! Flow:
//!
//! 1. `OrderPlaced` → reserve every line. If any reservation lacks stock,
//!    release the lines already reserved and cancel the order. Otherwise
//!    authorize a payment for the order total.
//! 2. `PaymentCaptured` → confirm the order and commit each reservation.
//! 3. `PaymentFailed` → release the reservations and cancel the order.
//! 4. `StockReserved` → observed only (the coordinator already reserves stock
//!    itself); subscribing keeps its checkpoint advancing across these events.

use std::{error::Error, future::Future};

use sourcery::{
    EventContext, React, ReactError, ReactFilters, Reactor, repository::CommandError,
    store::EventStore,
};

use crate::{
    app::AppRepo,
    domain::{
        ids::{OrderId, PaymentId},
        order::{CancelOrder, ConfirmOrder, Order, OrderLine, OrderPlaced, OrderStatus},
        payment::{Authorize, Payment, PaymentCaptured, PaymentFailed},
        product::{
            CommitReservation, Product, ProductError, ReleaseReservation, Reserve, StockReserved,
        },
    },
    metadata::RequestContext,
};

/// The actor name stamped on commands the coordinator issues.
const ACTOR: &str = "store.fulfillment-coordinator";

/// Retry budget for optimistic conflicts on hot products.
const MAX_RETRIES: usize = 8;

/// Failures the coordinator itself can originate (as opposed to relaying a
/// command error).
#[derive(Debug, thiserror::Error)]
enum CoordinatorError {
    #[error("could not parse aggregate key `{0}`")]
    UnparseableKey(String),
    #[error("product `{0}` referenced by an order does not exist")]
    MissingProduct(crate::domain::ids::ProductId),
}

/// Reduce a command result to a reaction outcome.
///
/// Success and *benign* rejections both resolve to `Ok`: the coordinator reads
/// current state before each command, so an aggregate or lifecycle rejection on
/// redelivery means the state already advanced — expected under at-least-once
/// delivery, not an error. Infrastructure failures are transient and asked to
/// be retried.
fn classify<T, A, Cc, Se, Sn>(
    result: Result<T, CommandError<A, Cc, Se, Sn>>,
) -> Result<(), ReactError>
where
    Cc: Error + Send + Sync + 'static,
    Se: Error + Send + Sync + 'static,
    Sn: Error + Send + Sync + 'static,
{
    match result {
        Ok(_) | Err(CommandError::Aggregate(_) | CommandError::Lifecycle(_)) => Ok(()),
        Err(CommandError::Concurrency(e)) => Err(ReactError::retry(e)),
        Err(CommandError::Projection(e)) => Err(ReactError::retry(e)),
        Err(CommandError::Store(e)) => Err(ReactError::retry(e)),
        Err(CommandError::Snapshot(e)) => Err(ReactError::retry(e)),
    }
}

/// The process manager. It captures a clone of the application repository so it
/// can issue commands while keeping the configured concurrency strategy and
/// snapshot setup.
pub struct FulfillmentCoordinator {
    repo: AppRepo,
}

impl FulfillmentCoordinator {
    /// Build a coordinator over a repository handle.
    #[must_use]
    pub const fn new(repo: AppRepo) -> Self {
        Self { repo }
    }

    /// Stamp causal lineage on a command issued in reaction to an event.
    fn causation(ctx: &EventContext<'_, String, RequestContext>) -> RequestContext {
        ctx.metadata
            .caused_by(ACTOR, format!("{}@{}", ctx.event_kind, ctx.aggregate_id))
    }

    async fn on_order_placed(
        &self,
        ctx: EventContext<'_, String, RequestContext>,
        event: &OrderPlaced,
    ) -> Result<(), ReactError> {
        let Some(order_id) = OrderId::from_storage_key(ctx.aggregate_id) else {
            return Err(ReactError::fatal(CoordinatorError::UnparseableKey(
                ctx.aggregate_id.clone(),
            )));
        };
        let md = Self::causation(&ctx);

        // Reserve every line. Reservation is idempotent per order, so redelivery
        // is safe; contention on hot products is expected, hence the retry loop.
        let mut reserved: Vec<&OrderLine> = Vec::new();
        for line in &event.lines {
            let command = Reserve {
                order_id,
                quantity: line.quantity,
            };
            match self
                .repo
                .update_with_retry::<Product, Reserve>(&line.product_id, &command, &md, MAX_RETRIES)
                .await
            {
                Ok(_) => reserved.push(line),
                Err(CommandError::Aggregate(ProductError::InsufficientStock { .. })) => {
                    // Compensate: release what we reserved, then cancel the order.
                    self.release_lines(order_id, &reserved, &md).await?;
                    self.cancel_order(order_id, "insufficient stock", &md)
                        .await?;
                    return Ok(());
                }
                Err(other) => classify(Err::<(), _>(other))?,
            }
        }

        // All lines reserved → authorize a payment for the order total. Skip if
        // a payment stream already exists (idempotent under redelivery).
        let payment_id = PaymentId::for_order(order_id);
        if self
            .repo
            .load::<Payment>(&payment_id)
            .await
            .map_err(ReactError::retry)?
            .is_none()
        {
            let amount_cents = self.price_order(event).await?;
            let command = Authorize {
                order_id,
                customer_id: event.customer_id.clone(),
                amount_cents,
                currency: "USD".to_string(),
            };
            classify(
                self.repo
                    .create::<Payment, Authorize>(&payment_id, &command, &md)
                    .await,
            )?;
        }
        Ok(())
    }

    async fn on_payment_captured(
        &self,
        ctx: EventContext<'_, String, RequestContext>,
        event: &PaymentCaptured,
    ) -> Result<(), ReactError> {
        let order_id = event.order_id;
        let md = Self::causation(&ctx);
        let Some(order) = self
            .repo
            .load::<Order>(&order_id)
            .await
            .map_err(ReactError::retry)?
        else {
            return Ok(());
        };

        if order.status() == OrderStatus::Placed {
            classify(
                self.repo
                    .update::<Order, ConfirmOrder>(&order_id, &ConfirmOrder, &md)
                    .await,
            )?;
        }

        for line in order.lines() {
            let command = CommitReservation {
                order_id,
                quantity: line.quantity,
            };
            classify(
                self.repo
                    .update_with_retry::<Product, CommitReservation>(
                        &line.product_id,
                        &command,
                        &md,
                        MAX_RETRIES,
                    )
                    .await,
            )?;
        }
        Ok(())
    }

    async fn on_payment_failed(
        &self,
        ctx: EventContext<'_, String, RequestContext>,
        event: &PaymentFailed,
    ) -> Result<(), ReactError> {
        let order_id = event.order_id;
        let md = Self::causation(&ctx);
        let Some(order) = self
            .repo
            .load::<Order>(&order_id)
            .await
            .map_err(ReactError::retry)?
        else {
            return Ok(());
        };

        if matches!(order.status(), OrderStatus::Placed | OrderStatus::Confirmed) {
            let lines: Vec<&OrderLine> = order.lines().iter().collect();
            self.release_lines(order_id, &lines, &md).await?;
            let reason = format!("payment failed: {}", event.reason);
            self.cancel_order(order_id, &reason, &md).await?;
        }
        Ok(())
    }

    /// Total order value in cents, priced from each product's catalogue price.
    async fn price_order(&self, event: &OrderPlaced) -> Result<u64, ReactError> {
        let mut total = 0;
        for line in &event.lines {
            let product = self
                .repo
                .load::<Product>(&line.product_id)
                .await
                .map_err(ReactError::retry)?
                .ok_or_else(|| {
                    ReactError::fatal(CoordinatorError::MissingProduct(line.product_id))
                })?;
            total += product.unit_price_cents() * u64::from(line.quantity);
        }
        Ok(total)
    }

    /// Release reservations for the given lines (idempotent compensation).
    async fn release_lines(
        &self,
        order_id: OrderId,
        lines: &[&OrderLine],
        md: &RequestContext,
    ) -> Result<(), ReactError> {
        for line in lines {
            let command = ReleaseReservation {
                order_id,
                quantity: line.quantity,
            };
            classify(
                self.repo
                    .update_with_retry::<Product, ReleaseReservation>(
                        &line.product_id,
                        &command,
                        md,
                        MAX_RETRIES,
                    )
                    .await,
            )?;
        }
        Ok(())
    }

    async fn cancel_order(
        &self,
        order_id: OrderId,
        reason: &str,
        md: &RequestContext,
    ) -> Result<(), ReactError> {
        classify(
            self.repo
                .update::<Order, CancelOrder>(
                    &order_id,
                    &CancelOrder {
                        reason: reason.to_string(),
                    },
                    md,
                )
                .await,
        )
    }
}

impl Reactor for FulfillmentCoordinator {
    type Id = String;
    type Metadata = RequestContext;

    const KIND: &'static str = "store.fulfillment-coordinator";

    fn filters<S>(&self) -> ReactFilters<S, Self>
    where
        S: EventStore<Id = Self::Id, Metadata = Self::Metadata>,
    {
        ReactFilters::new()
            .event::<OrderPlaced>()
            .event::<PaymentCaptured>()
            .event::<PaymentFailed>()
            .event::<StockReserved>()
    }
}

impl React<OrderPlaced> for FulfillmentCoordinator {
    fn react(
        &self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &OrderPlaced,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        self.on_order_placed(ctx, event)
    }
}

impl React<PaymentCaptured> for FulfillmentCoordinator {
    fn react(
        &self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentCaptured,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        self.on_payment_captured(ctx, event)
    }
}

impl React<PaymentFailed> for FulfillmentCoordinator {
    fn react(
        &self,
        ctx: EventContext<'_, Self::Id, Self::Metadata>,
        event: &PaymentFailed,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        self.on_payment_failed(ctx, event)
    }
}

impl React<StockReserved> for FulfillmentCoordinator {
    fn react(
        &self,
        _ctx: EventContext<'_, Self::Id, Self::Metadata>,
        _event: &StockReserved,
    ) -> impl Future<Output = Result<(), ReactError>> + Send {
        // The coordinator reserves stock itself inside `on_order_placed`, so it
        // takes no action here; it subscribes only so its checkpoint advances
        // across these events.
        std::future::ready(Ok(()))
    }
}
