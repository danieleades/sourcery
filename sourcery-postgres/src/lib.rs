//! Postgres-backed event sourcing implementations.
//!
//! This crate provides `PostgreSQL` implementations of the core Sourcery
//! traits:
//!
//! - [`Store`] - An implementation of [`sourcery_core::store::EventStore`]
//! - [`snapshot::Store`] - An implementation of
//!   [`sourcery_core::snapshot::SnapshotStore`]
//!
//! Both use the same database and can share a connection pool.

pub mod error;
pub mod snapshot;
pub mod store;

pub use error::Error;
pub use store::Store;
