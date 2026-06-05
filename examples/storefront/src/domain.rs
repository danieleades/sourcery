//! The order-fulfillment domain: three aggregates, each its own consistency
//! boundary, that coordinate only through events.
//!
//! - [`product`] — stock on hand, reservations, replenishment.
//! - [`order`] — cart → placed → confirmed/cancelled → shipped.
//! - [`payment`] — authorize → capture → refund for one order.
//!
//! [`ids`] holds the strongly typed aggregate ids and their projection to the
//! shared `String` storage key.

pub mod ids;
pub mod order;
pub mod payment;
pub mod product;
