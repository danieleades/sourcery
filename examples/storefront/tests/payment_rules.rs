//! Unit tests for the `Payment` aggregate's command rules, including the
//! additive event-versioning behaviour.

use sourcery::test::TestFramework;
use storefront::domain::{
    ids::{CustomerId, OrderId},
    payment::{
        Authorize, Capture, FailPayment, Payment, PaymentAuthorized, PaymentCaptured, PaymentError,
        PaymentFailed, PaymentRefunded, Refund,
    },
};

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
    // An event stored before `currency` existed still decodes, defaulting the
    // new field.
    let order = OrderId::new();
    let v1 = format!(
        r#"{{"order_id":"{}","customer_id":"cust-legacy","amount_cents":2500}}"#,
        order.0
    );
    let decoded: PaymentAuthorized = serde_json::from_str(&v1).expect("v1 event decodes");
    assert_eq!(decoded.currency, "USD");
    assert_eq!(decoded.amount_cents, 2500);
}
