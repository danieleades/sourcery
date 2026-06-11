//! The `Payment` aggregate: authorize → capture → refund for one order.
//!
//! A payment belongs to exactly one order, so `PaymentId` is *derived* from the
//! order's uuid (see
//! [`PaymentId::for_order`](super::ids::PaymentId::for_order)), encoding the
//! 1:1 link in the stream key. `order_id`/`customer_id` are *also* carried in
//! the payload because they are genuine domain facts the payment asserts and
//! the join keys `CustomerAccountView` reads.
//!
//! `PaymentAuthorized` also shows additive event versioning: the struct adds
//! `currency` with a serde default, so older events written before the field
//! existed still deserialise.

use serde::{Deserialize, Serialize};
use sourcery::{Aggregate, Apply, Create, DomainEvent, Handle, HandleCreate};

use super::ids::{CustomerId, OrderId, PaymentId};

/// Default currency for events written before the `currency` field existed.
fn default_currency() -> String {
    "USD".to_string()
}

// ---------------------------------------------------------------------------
// Events (`store.payment.*`)
// ---------------------------------------------------------------------------

/// Payment was authorised against the customer's method.
///
/// `currency` carries `#[serde(default)]`, so older events stored as
/// `{ order_id, customer_id, amount_cents }` still deserialise (currency =
/// `"USD"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAuthorized {
    pub order_id: OrderId,
    pub customer_id: CustomerId,
    pub amount_cents: u64,
    #[serde(default = "default_currency")]
    pub currency: String,
}

impl DomainEvent for PaymentAuthorized {
    const KIND: &'static str = "store.payment.authorized";
}

/// Funds were captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentCaptured {
    pub order_id: OrderId,
    pub customer_id: CustomerId,
    pub amount_cents: u64,
}

impl DomainEvent for PaymentCaptured {
    const KIND: &'static str = "store.payment.captured";
}

/// Captured funds were refunded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRefunded {
    pub order_id: OrderId,
    pub customer_id: CustomerId,
    pub amount_cents: u64,
    pub reason: String,
}

impl DomainEvent for PaymentRefunded {
    const KIND: &'static str = "store.payment.refunded";
}

/// The payment failed (declined, expired authorisation, etc.).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentFailed {
    pub order_id: OrderId,
    pub reason: String,
}

impl DomainEvent for PaymentFailed {
    const KIND: &'static str = "store.payment.failed";
}

// ---------------------------------------------------------------------------
// Error & status
// ---------------------------------------------------------------------------

/// Reasons a payment command can be rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PaymentError {
    /// Authorise was issued with a zero amount.
    #[error("payment amount must be positive")]
    NonPositiveAmount,
    /// A command was issued from an incompatible state.
    #[error("invalid transition: cannot {action} a payment that is {state}")]
    InvalidTransition {
        action: &'static str,
        state: &'static str,
    },
}

/// Lifecycle state of a payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentStatus {
    /// Authorised, awaiting capture.
    Authorized,
    Captured,
    Refunded,
    Failed,
}

impl PaymentStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
            Self::Captured => "captured",
            Self::Refunded => "refunded",
            Self::Failed => "failed",
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// Payment aggregate for a single order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Aggregate)]
#[aggregate(
    id = PaymentId,
    error = PaymentError,
    kind = "store.payment",
    events(PaymentAuthorized, PaymentCaptured, PaymentRefunded, PaymentFailed),
    create(PaymentAuthorized),
    derives(Debug, PartialEq, Eq)
)]
pub struct Payment {
    status: PaymentStatus,
    order_id: Option<OrderId>,
    customer_id: Option<CustomerId>,
    amount_cents: u64,
}

impl Payment {
    /// Current lifecycle status.
    #[must_use]
    pub const fn status(&self) -> PaymentStatus {
        self.status
    }

    /// Authorised/captured amount in cents.
    #[must_use]
    pub const fn amount_cents(&self) -> u64 {
        self.amount_cents
    }
}

impl Create<PaymentAuthorized> for Payment {
    fn create(event: &PaymentAuthorized) -> Self {
        Self {
            status: PaymentStatus::Authorized,
            order_id: Some(event.order_id),
            customer_id: Some(event.customer_id.clone()),
            amount_cents: event.amount_cents,
        }
    }
}

impl Apply<PaymentAuthorized> for Payment {
    fn apply(&mut self, event: &PaymentAuthorized) {
        self.status = PaymentStatus::Authorized;
        self.order_id = Some(event.order_id);
        self.customer_id = Some(event.customer_id.clone());
        self.amount_cents = event.amount_cents;
    }
}

impl Apply<PaymentCaptured> for Payment {
    fn apply(&mut self, _event: &PaymentCaptured) {
        self.status = PaymentStatus::Captured;
    }
}

impl Apply<PaymentRefunded> for Payment {
    fn apply(&mut self, _event: &PaymentRefunded) {
        self.status = PaymentStatus::Refunded;
    }
}

impl Apply<PaymentFailed> for Payment {
    fn apply(&mut self, _event: &PaymentFailed) {
        self.status = PaymentStatus::Failed;
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Authorise a payment (creation command).
#[derive(Clone, Debug)]
pub struct Authorize {
    pub order_id: OrderId,
    pub customer_id: CustomerId,
    pub amount_cents: u64,
    pub currency: String,
}

/// Capture an authorised payment.
#[derive(Clone, Debug)]
pub struct Capture;

/// Refund a captured payment.
#[derive(Clone, Debug)]
pub struct Refund {
    pub reason: String,
}

/// Mark an authorised payment as failed.
#[derive(Clone, Debug)]
pub struct FailPayment {
    pub reason: String,
}

impl HandleCreate<Authorize> for Payment {
    type HandleCreateError = PaymentError;

    fn handle_create(command: &Authorize) -> Result<Vec<Self::Event>, Self::HandleCreateError> {
        if command.amount_cents == 0 {
            return Err(PaymentError::NonPositiveAmount);
        }
        Ok(vec![
            PaymentAuthorized {
                order_id: command.order_id,
                customer_id: command.customer_id.clone(),
                amount_cents: command.amount_cents,
                currency: command.currency.clone(),
            }
            .into(),
        ])
    }
}

impl Handle<Capture> for Payment {
    type HandleError = PaymentError;

    fn handle(&self, _command: &Capture) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            PaymentStatus::Authorized => Ok(vec![
                PaymentCaptured {
                    order_id: self.order_id.expect("authorized payment has an order"),
                    customer_id: self
                        .customer_id
                        .clone()
                        .expect("authorized payment has a customer"),
                    amount_cents: self.amount_cents,
                }
                .into(),
            ]),
            other => Err(PaymentError::InvalidTransition {
                action: "capture",
                state: other.label(),
            }),
        }
    }
}

impl Handle<Refund> for Payment {
    type HandleError = PaymentError;

    fn handle(&self, command: &Refund) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            PaymentStatus::Captured => Ok(vec![
                PaymentRefunded {
                    order_id: self.order_id.expect("captured payment has an order"),
                    customer_id: self
                        .customer_id
                        .clone()
                        .expect("captured payment has a customer"),
                    amount_cents: self.amount_cents,
                    reason: command.reason.clone(),
                }
                .into(),
            ]),
            other => Err(PaymentError::InvalidTransition {
                action: "refund",
                state: other.label(),
            }),
        }
    }
}

impl Handle<FailPayment> for Payment {
    type HandleError = PaymentError;

    fn handle(&self, command: &FailPayment) -> Result<Vec<Self::Event>, Self::HandleError> {
        match self.status {
            PaymentStatus::Authorized => Ok(vec![
                PaymentFailed {
                    order_id: self.order_id.expect("authorized payment has an order"),
                    reason: command.reason.clone(),
                }
                .into(),
            ]),
            other => Err(PaymentError::InvalidTransition {
                action: "fail",
                state: other.label(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use sourcery::test::TestFramework;

    use super::*;

    type PaymentTest = TestFramework<Payment>;

    fn authorized() -> PaymentAuthorized {
        PaymentAuthorized {
            order_id: OrderId::new(),
            customer_id: CustomerId::new("cust-1"),
            amount_cents: 1_999,
            currency: "USD".to_string(),
        }
    }

    #[test]
    fn authorizing_emits_payment_authorized() {
        let event = authorized();
        PaymentTest::new()
            .when_create(&Authorize {
                order_id: event.order_id,
                customer_id: event.customer_id.clone(),
                amount_cents: event.amount_cents,
                currency: event.currency.clone(),
            })
            .then_expect_events(&[event.into()]);
    }

    #[test]
    fn authorizing_zero_is_rejected() {
        PaymentTest::new()
            .when_create(&Authorize {
                order_id: OrderId::new(),
                customer_id: CustomerId::new("cust-1"),
                amount_cents: 0,
                currency: "USD".to_string(),
            })
            .then_expect_error_eq(&PaymentError::NonPositiveAmount);
    }

    #[test]
    fn capturing_an_authorized_payment_emits_captured() {
        let auth = authorized();
        PaymentTest::given(&[auth.clone().into()])
            .when(&Capture)
            .then_expect_events(&[PaymentCaptured {
                order_id: auth.order_id,
                customer_id: auth.customer_id,
                amount_cents: auth.amount_cents,
            }
            .into()]);
    }

    #[test]
    fn capturing_twice_is_rejected() {
        let auth = authorized();
        PaymentTest::given(&[
            auth.clone().into(),
            PaymentCaptured {
                order_id: auth.order_id,
                customer_id: auth.customer_id.clone(),
                amount_cents: auth.amount_cents,
            }
            .into(),
        ])
        .when(&Capture)
        .then_expect_error_eq(&PaymentError::InvalidTransition {
            action: "capture",
            state: "captured",
        });
    }

    #[test]
    fn refunding_a_captured_payment_emits_refunded() {
        let auth = authorized();
        PaymentTest::given(&[
            auth.clone().into(),
            PaymentCaptured {
                order_id: auth.order_id,
                customer_id: auth.customer_id.clone(),
                amount_cents: auth.amount_cents,
            }
            .into(),
        ])
        .when(&Refund {
            reason: "returned".to_string(),
        })
        .then_expect_events(&[PaymentRefunded {
            order_id: auth.order_id,
            customer_id: auth.customer_id,
            amount_cents: auth.amount_cents,
            reason: "returned".to_string(),
        }
        .into()]);
    }

    #[test]
    fn refunding_before_capture_is_rejected() {
        PaymentTest::given(&[authorized().into()])
            .when(&Refund {
                reason: "too soon".to_string(),
            })
            .then_expect_error_eq(&PaymentError::InvalidTransition {
                action: "refund",
                state: "authorized",
            });
    }

    #[test]
    fn failing_an_authorized_payment_emits_failed() {
        let auth = authorized();
        PaymentTest::given(&[auth.clone().into()])
            .when(&FailPayment {
                reason: "declined".to_string(),
            })
            .then_expect_events(&[PaymentFailed {
                order_id: auth.order_id,
                reason: "declined".to_string(),
            }
            .into()]);
    }

    #[test]
    fn legacy_payment_authorized_without_currency_defaults_to_usd() {
        let order = OrderId::new();
        let v1 = format!(
            r#"{{"order_id":"{}","customer_id":"cust-legacy","amount_cents":2500}}"#,
            order.0
        );
        let decoded: PaymentAuthorized = serde_json::from_str(&v1).expect("v1 event decodes");
        assert_eq!(decoded.currency, "USD");
        assert_eq!(decoded.amount_cents, 2500);
    }
}
