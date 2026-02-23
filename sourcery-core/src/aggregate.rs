//! Command-side domain primitives.
//!
//! This module defines the building blocks for aggregates: state reconstruction
//! (`Apply`) and command handling (`Handle` / `HandleCreate`). The
//! `#[derive(Aggregate)]` macro lives here to keep domain ergonomics in one
//! spot.

/// Command-side entities that produce domain events.
///
/// Aggregates rebuild their state from events (`Apply<E>`) and validate
/// commands via [`Handle<C>`] and [`HandleCreate<C>`]. The derive macro
/// generates the event enum and plumbing automatically, while keeping your
/// state struct focused on domain behaviour.
///
/// Aggregates are domain objects and do not require serialisation by default.
///
/// If you enable snapshots (via `Repository::with_snapshots`), the aggregate
/// state must be serialisable (`Serialize + DeserializeOwned`).
// ANCHOR: aggregate_trait
pub trait Aggregate {
    /// Aggregate type identifier used by the event store.
    ///
    /// This is combined with the aggregate ID to create stream identifiers.
    /// Use lowercase, kebab-case for consistency: `"product"`,
    /// `"user-account"`, etc.
    const KIND: &'static str;

    type Event;
    type Error;
    type Id;

    /// Construct aggregate state from its very first event.
    ///
    /// Called during event replay when no snapshot exists. The first event
    /// in the stream is passed to `create` rather than `apply`, allowing
    /// aggregates to initialise required fields without needing `Default`.
    ///
    /// When using `#[derive(Aggregate)]` with a `create(EventType)` attribute,
    /// this dispatches to your [`Create<E>`] implementation. Without the
    /// attribute it falls back to `Default::default()` + `apply`.
    fn create(event: &Self::Event) -> Self;

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

/// Construct an aggregate from a specific creation event.
///
/// `Create<E>` is the counterpart to [`Apply<E>`] for the first event in a
/// stream. It allows aggregates with required fields to be initialised
/// properly without resorting to `Option<T>` or sentinel values.
///
/// Use the `create(EventType)` attribute on `#[derive(Aggregate)]` to wire
/// this up automatically.
///
/// ```ignore
/// struct AccountOpened {
///     initial_balance: i64,
/// }
///
/// impl Create<AccountOpened> for Account {
///     fn create(event: &AccountOpened) -> Self {
///         Self { balance: event.initial_balance }
///     }
/// }
/// ```
// ANCHOR: create_trait
pub trait Create<E>: Sized {
    fn create(event: &E) -> Self;
}
// ANCHOR_END: create_trait

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
///     type HandleError = Self::Error;
///
///     fn handle(&self, command: &DepositFunds) -> Result<Vec<Self::Event>, Self::HandleError> {
///         if command.amount <= 0 {
///             return Err("amount must be positive".into());
///         }
///         Ok(vec![FundsDeposited { amount: command.amount }.into()])
///     }
/// }
/// ```
// ANCHOR: handle_trait
pub trait Handle<C>: Aggregate {
    type HandleError: Into<<Self as Aggregate>::Error>;

    /// Handle a command and produce events.
    ///
    /// # Errors
    ///
    /// Returns [`Self::HandleError`](Self::HandleError) if the command is
    /// invalid for the current aggregate state.
    fn handle(&self, command: &C) -> Result<Vec<Self::Event>, Self::HandleError>;
}
// ANCHOR_END: handle_trait

/// Entry point for creation command handling.
///
/// `HandleCreate<C>` is used when the aggregate stream does not yet exist.
/// It allows create commands to have their own validation logic and error
/// type, which is converted into the aggregate's top-level error type.
pub trait HandleCreate<C>: Aggregate {
    type HandleCreateError: Into<<Self as Aggregate>::Error>;

    /// Handle a creation command and produce events.
    ///
    /// # Errors
    ///
    /// Returns [`Self::HandleCreateError`](Self::HandleCreateError) if the
    /// command is invalid for a missing aggregate.
    fn handle_create(command: &C) -> Result<Vec<Self::Event>, Self::HandleCreateError>;
}
