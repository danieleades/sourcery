//! Core traits and types for the Sourcery event-sourcing library.
//!
//! This crate provides the foundational abstractions for event sourcing:
//!
//! - [`aggregate`] - Command-side primitives (`Aggregate`, `Apply`, `Handle`)
//! - [`projection`] - Read-side primitives (`Projection`, `ApplyProjection`, `ProjectionBuilder`)
//! - [`repository`] - Command execution and aggregate lifecycle (`Repository`)
//! - [`store`] - Event persistence abstraction (`EventStore`)
//! - [`snapshot`] - Snapshot storage abstraction (`SnapshotStore`)
//! - [`event`] - Event marker traits (`DomainEvent`, `EventKind`, `ProjectionEvent`)
//! - [`concurrency`] - Concurrency strategy markers (`Optimistic`, `Unchecked`)
//!
//! # Example
//!
//! ```
//! use sourcery_core::{repository::Repository, store::inmemory};
//!
//! // Create an in-memory store and repository
//! let store: inmemory::Store<String, ()> = inmemory::Store::new();
//! let repo = Repository::new(store);
//! ```
//!
//! Most users should depend on the [`sourcery`](https://docs.rs/sourcery) crate,
//! which re-exports these types with a cleaner API surface.

pub mod aggregate;
pub mod concurrency;
pub mod event;
pub mod projection;
pub mod repository;
pub mod snapshot;
pub mod store;

// Test utilities module: public when feature enabled, internal for crate tests
#[cfg(feature = "test-util")]
pub mod test;

#[cfg(all(test, not(feature = "test-util")))]
pub(crate) mod test;
