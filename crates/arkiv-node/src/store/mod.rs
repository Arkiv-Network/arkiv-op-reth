//! Trie-backed [`Store`](arkiv_entitydb::Store) implementations for
//! `arkiv-entitydb`.
//!
//! Three flavours, all sharing the on-trie encoding defined in
//! [`trie_layout`]:
//!
//! - [`ReadWriteStore`] — wraps revm's `&mut EvmInternals`. Used by
//!   the precompile during block execution. All writes go through
//!   revm's journal so reverts roll back cleanly.
//! - [`ReadOnlyStore`] — wraps a reth `StateProviderBox` snapshot.
//!   Used by the `arkiv_*` RPC handler to drive
//!   `arkiv_entitydb::query::execute` against committed state.
//!   Mutating methods bail — they shouldn't be reached from the
//!   read-only query path.
//! - [`InMemoryStore`] — wraps an in-process [`InMemoryStateDb`] that
//!   mirrors the on-trie layout (account code + storage slots).
//!   Useful for tests that want to verify the trie encoding without
//!   booting a full reth node.
//!
//! `arkiv-entitydb`'s own unit tests use a different, pure-typed
//! cache (`arkiv_entitydb::test_utils::MemStore`) that has nothing
//! to do with the trie layout — see that module for the rationale.

mod cache_store;
mod cached_read_write_store;
mod in_memory_store;
mod read_only_store;
mod read_write_store;
mod trie_layout;

pub use cache_store::{CacheStore, Cached};
pub use cached_read_write_store::CachedReadWriteStore;
pub use in_memory_store::{AccountState, InMemoryStateDb, InMemoryStore};
pub use read_only_store::ReadOnlyStore;
pub use read_write_store::ReadWriteStore;
