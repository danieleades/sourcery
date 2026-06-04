//! Identifier projection for aggregate storage keys.

/// A domain identifier that can be stored as an event store's raw key type.
///
/// Implement this trait when an aggregate uses a domain-specific id newtype but
/// the backing store uses a shared raw id type such as [`String`].
///
/// The projection is intentionally one-way: repositories only need to route a
/// caller-supplied domain id to a storage key, and never reconstruct the domain
/// id from the stored key while loading aggregates.
pub trait StorageKey<Raw> {
    /// Project this domain id onto the store's raw key type.
    fn to_key(&self) -> Raw;
}

impl<T: Clone> StorageKey<T> for T {
    fn to_key(&self) -> T {
        self.clone()
    }
}
