//! Abstract Arkiv state model.
//!
//! Op handlers (`create` / `update` / `extend` / `transfer` / `delete`
//! / `expire`), the [`StateAdapter`] trait they run against, the typed
//! primitives ([`Entity`], [`Bitmap`], [`IndexTree`]), the
//! address-derivation functions used by the query interpreter and the
//! SDK ([`entity_address`], [`pair_address`], [`index_address`]), and
//! the query language itself.
//!
//! This crate is deliberately abstract: it knows nothing about the
//! trie, about EVM accounts, or about how a [`StateAdapter`] impl backs its
//! storage. The trie encoding (system-account slot derivations, code
//! packing, the `0xFE` entity-code prefix) lives in `arkiv-node`'s
//! `trie_layout` and adapter modules.

pub mod query;
pub mod state_adapter;

pub use state_adapter::*;
#[cfg(feature = "test-utils")]
pub use state_adapter::test_utils;
