//! State adapter (the [`StateAdapter`] trait), op handlers, and the typed
//! primitives the handlers operate on.
//!
//! Submodules:
//!
//! - [`addresses`] — address derivations, built-in annotation keys,
//!   and value encoders.
//! - [`bitmap`] — roaring64 [`Bitmap`] of entity IDs (Tier-1 pair
//!   accounts).
//! - [`entity`] — on-trie [`Entity`] / [`Attribute`] representation.
//! - [`index_tree`] — Adaptive Radix Tree [`IndexTree`] (Tier-2 index
//!   accounts).
//! - [`handlers`] — `create` / `update` / `extend` / `transfer` /
//!   `delete` / `expire`, plus the read-side helpers used by the query
//!   interpreter (`read_pair_bitmap`, `all_entities`, `read_index_tree`,
//!   `resolve_id`).
//!
//! The trait itself lives here at the module root because it's what
//! "being a store" means; the submodules are the values it returns
//! and the operations that run against it.

use alloy_primitives::Address;
use eyre::Result;

pub mod addresses;
pub mod bitmap;
pub mod entity;
pub mod handlers;
pub mod index_tree;

#[cfg(feature = "test-utils")]
pub mod test_utils;

pub use addresses::{
    ANNOT_ALL, ANNOT_CONTENT_TYPE, ANNOT_CREATED_AT_BLOCK, ANNOT_CREATOR, ANNOT_EXPIRATION,
    ANNOT_KEY, ANNOT_OWNER, entity_address, index_address, pair_address,
};
pub use bitmap::Bitmap;
pub use entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute, Entity};
pub use handlers::{
    all_entities, create, delete, expire, extend, read_index_tree, read_pair_bitmap, resolve_id,
    transfer, update,
};
/// Adaptive Radix Tree ordered set of attribute values for a single
/// attribute key, stored in an **index account** at
/// [`index_address`]. Provides O(log n) insert/remove, prefix-compressed
/// deterministic serialisation, and ascending range/prefix iteration.
pub use index_tree::IndexTree;

/// Abstract state interface the op handlers run against.
///
/// In production, `arkiv_node::state_adapter` implements this over revm's
/// `EvmInternals`. For tests, [`crate::test_utils::MemStateAdapter`]
/// implements it over a plain `HashMap`-backed cache.
///
/// The trait is typed end-to-end — op handlers never see raw bytes or
/// storage slots. Four account categories, each with its own
/// accessors:
///
/// - **System-account slots** (`get_entity_count` / `set_entity_count`,
///   `get_id_to_addr` / `set_id_to_addr`, `get_addr_to_id` /
///   `set_addr_to_id`, `get_nonce` / `set_nonce`) — the global entity
///   counter, the ID ↔ address maps, and per-EOA minting nonces.
///   Slot derivation and value encoding are impl details.
/// - **Entity accounts** (`get_entity` / `set_entity` /
///   `tombstone_entity`) — code carries `0xFE || RLP(Entity)`.
/// - **Tier-1 pair-bitmap accounts** (`get_pair_bitmap` /
///   `set_pair_bitmap`) — code carries a roaring64 bitmap of entity
///   IDs, content-addressed in the trie. Addressed by
///   `(annot_key, annot_val)`; impls derive the account address via
///   [`pair_address`].
/// - **Tier-2 ART index accounts** (`get_index_tree` /
///   `set_index_tree` / `tombstone_index_tree`) — code carries a
///   serialised [`IndexTree`]. Addressed by `attr_key`; impls derive
///   the account address via [`index_address`].
///
/// Conventions on reads:
/// - `get_entity` returns `Ok(None)` for an absent or tombstoned
///   account.
/// - `get_pair_bitmap` returns an empty [`Bitmap`] for an absent /
///   empty pair account — not an error.
/// - `get_index_tree` returns an empty [`IndexTree`] for an absent /
///   tombstoned index account — not an error.
/// - System-account getters return the zero value (`0` / `Address::ZERO`)
///   for never-written slots; callers know the context (e.g. an entity
///   ID of 0 is valid because creation has been verified upstream).
///
/// Conventions on writes:
/// - Setters take owned values so a caching impl can park them
///   directly without cloning.
/// - System-slot setters and account-code setters materialise the
///   underlying account if needed (raise `nonce` to ≥ 1 so EIP-161
///   doesn't prune at end-of-tx).
/// - `tombstone_*` clears the code but preserves `nonce = 1`. Pair
///   accounts are never tombstoned — an empty bitmap is serialised
///   normally.
pub trait StateAdapter {
    // ── System-account slots ───────────────────────────────────────

    fn get_entity_count(&mut self) -> Result<u64>;
    fn set_entity_count(&mut self, count: u64) -> Result<()>;

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address>;
    fn set_id_to_addr(&mut self, id: u64, addr: Address) -> Result<()>;

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64>;
    fn set_addr_to_id(&mut self, addr: &Address, id: u64) -> Result<()>;

    fn get_nonce(&mut self, caller: &Address) -> Result<u32>;
    fn set_nonce(&mut self, caller: &Address, nonce: u32) -> Result<()>;

    // ── Entity accounts ────────────────────────────────────────────

    fn get_entity(&mut self, addr: &Address) -> Result<Option<Entity>>;
    fn set_entity(&mut self, addr: &Address, entity: Entity) -> Result<()>;
    fn tombstone_entity(&mut self, addr: &Address) -> Result<()>;

    // ── Tier-1 pair-bitmap accounts ────────────────────────────────

    fn get_pair_bitmap(&mut self, annot_key: &[u8], annot_val: &[u8]) -> Result<Bitmap>;
    fn set_pair_bitmap(
        &mut self,
        annot_key: &[u8],
        annot_val: &[u8],
        bitmap: Bitmap,
    ) -> Result<()>;

    // ── Tier-2 ART index accounts ──────────────────────────────────

    fn get_index_tree(&mut self, attr_key: &[u8]) -> Result<IndexTree>;
    fn set_index_tree(&mut self, attr_key: &[u8], tree: IndexTree) -> Result<()>;
    fn tombstone_index_tree(&mut self, attr_key: &[u8]) -> Result<()>;
}
