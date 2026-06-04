//! `storefront` — an end-to-end example application for the Sourcery
//! event-sourcing library.
//!
//! A small order-fulfillment system for an online store, built around three
//! aggregates that each own a consistency boundary and coordinate only through
//! events:
//!
//! - [`domain::product`] — stock on hand, reservations, replenishment.
//! - [`domain::order`] — cart → placed → confirmed/cancelled → shipped.
//! - [`domain::payment`] — authorize → capture → refund for one order.
//!
//! The two headline capabilities sit at the centre:
//!
//! - **Cross-aggregate read models** ([`projections`]) join events from
//!   different aggregates into query models.
//! - **A checkpointed reactor** ([`coordinator::FulfillmentCoordinator`])
//!   reacts to committed events by issuing commands, turning the three isolated
//!   aggregates into one eventually consistent workflow with compensation.
//!
//! The library code lives here so both the runnable demo (`src/main.rs`) and
//! the integration tests (`tests/`) share one definition of the domain.

pub mod app;
pub mod coordinator;
pub mod domain;
pub mod metadata;
pub mod projections;
