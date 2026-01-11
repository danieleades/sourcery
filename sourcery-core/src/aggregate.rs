//! Command-side domain primitives.
//!
//! This module defines the building blocks for aggregates: state reconstruction
//! (`Apply`) and command handling (`Handle`). The `#[derive(Aggregate)]` macro
//! lives here to keep domain ergonomics in one spot.


/// Command-side entities that produce domain events.
///
/// Aggregates rebuild their state from events (`Apply<E>`) and validate
/// commands via [`Handle<C>`]. The derive macro generates the event enum and
/// plumbing automatically, while keeping your state struct focused on domain
/// behaviour.
///
/// Aggregates are domain objects and do not require serialization by default.
///
/// If you enable snapshots (via `Repository::with_snapshots`), the aggregate
/// state must be serializable (`Serialize + DeserializeOwned`).
// ANCHOR: aggregate_trait
pub trait Aggregate: Default {
    /// Aggregate type identifier used by the event store.
    ///
    /// This is combined with the aggregate ID to create stream identifiers.
    /// Use lowercase, kebab-case for consistency: `"product"`,
    /// `"user-account"`, etc.
    const KIND: &'static str;

    type Event;
    type Error;
    type Id;

    /// Apply an event to update aggregate state.
    ///
    /// This is called during event replay to rebuild aggregate state from
    /// history.
    ///
    /// When using `#[derive(Aggregate)]`, this dispatches to your `Apply<E>`
    /// implementations. For hand-written aggregates, implement this
    /// directly with a match expression.
    fn apply(&mut self, event: &Self::Event);
}
// ANCHOR_END: aggregate_trait

/// Mutate an aggregate with a domain event.
///
/// `Apply<E>` is called while the repository rebuilds aggregate state, keeping
/// the domain logic focused on pure events rather than persistence concerns.
///
/// ```ignore
/// #[derive(Default)]
/// struct Account {
///     balance: i64,
/// }
///
/// impl Apply<FundsDeposited> for Account {
///     fn apply(&mut self, event: &FundsDeposited) {
///         self.balance += event.amount;
///     }
/// }
/// ```
// ANCHOR: apply_trait
pub trait Apply<E> {
    fn apply(&mut self, event: &E);
}
// ANCHOR_END: apply_trait

/// Entry point for command handling.
///
/// Each command type gets its own implementation, letting the aggregate express
/// validation logic in a strongly typed way.
///
/// ```ignore
/// impl Handle<DepositFunds> for Account {
///     fn handle(&self, command: &DepositFunds) -> Result<Vec<Self::Event>, Self::Error> {
///         if command.amount <= 0 {
///             return Err("amount must be positive".into());
///         }
///         Ok(vec![FundsDeposited { amount: command.amount }.into()])
///     }
/// }
/// ```
// ANCHOR: handle_trait
pub trait Handle<C>: Aggregate {
    /// Handle a command and produce events.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the command is invalid for the current
    /// aggregate state.
    fn handle(&self, command: &C) -> Result<Vec<Self::Event>, Self::Error>;
}
// ANCHOR_END: handle_trait
