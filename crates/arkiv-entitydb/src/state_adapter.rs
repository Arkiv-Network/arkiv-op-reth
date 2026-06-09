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
//! - [`btree_index`] — In-storage B+ tree for the Tier-2 index
//!   (values ≤ 32 bytes). Each tree is stored as a set of EVM accounts
//!   whose storage slots hold the node data.
//! - [`handlers`] — `create` / `update` / `extend` / `transfer` /
//!   `delete` / `expire`, plus the read-side helpers used by the query
//!   interpreter (`read_pair_bitmap`, `all_entities`, `resolve_id`).
//!
//! The trait itself lives here at the module root because it's what
//! "being a store" means; the submodules are the values it returns
//! and the operations that run against it.

use alloy_primitives::{Address, B256};
use eyre::Result;

pub mod addresses;
pub mod bitmap;
pub mod btree_index;
pub mod entity;
pub mod handlers;

#[cfg(feature = "test-utils")]
pub mod test_utils;

pub use addresses::{
    ANNOT_ALL, ANNOT_CONTENT_TYPE, ANNOT_CREATED_AT_BLOCK, ANNOT_CREATOR, ANNOT_EXPIRATION,
    ANNOT_KEY, ANNOT_OWNER, entity_address, pair_address,
};
pub use bitmap::Bitmap;
pub use btree_index::{
    BTREE_MAGIC, BTREE_ORDER, annot_val_to_slot, btree_header_address, btree_insert,
    btree_iter_from, btree_lazy_delete, slot_presence, slot_to_val_len,
};
pub use entity::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute, Entity};
pub use handlers::{
    all_entities, create, delete, expire, extend, read_pair_bitmap, resolve_id, transfer, update,
};

/// Abstract state interface the op handlers run against.
///
/// In production, `arkiv_node::state_adapter` implements this over revm's
/// `EvmInternals`. For tests, [`crate::test_utils::MemStateAdapter`]
/// implements it over a plain `HashMap`-backed cache.
///
/// The trait has four typed account categories plus raw storage access
/// for the Tier-2 B+ tree index:
///
/// - **System-account slots** (`get_entity_count` / `set_entity_count`,
///   `get_id_to_addr` / `set_id_to_addr`, `get_addr_to_id` /
///   `set_addr_to_id`, `get_nonce` / `set_nonce`) — the global entity
///   counter, the ID ↔ address maps, and per-EOA minting nonces.
/// - **Entity accounts** (`get_entity` / `set_entity` /
///   `tombstone_entity`) — code carries `0xFE || RLP(Entity)`.
/// - **Tier-1 pair-bitmap accounts** (`get_pair_bitmap` /
///   `set_pair_bitmap`) — code carries a roaring64 bitmap of entity
///   IDs. Addressed by `(annot_key, annot_val)` via [`pair_address`].
/// - **Raw storage** (`raw_storage` / `set_raw_storage` /
///   `ensure_raw_account`) — individual EVM storage-slot reads and
///   writes used by the Tier-2 B+ tree index. The B+ tree stores
///   each node as a separate EVM account; traversal and mutation go
///   entirely through point reads/writes, compatible with reth's
///   `MemoryOverlayStateProvider`.
///
/// Conventions on reads:
/// - `get_entity` returns `Ok(None)` for an absent or tombstoned account.
/// - `get_pair_bitmap` returns an empty [`Bitmap`] for an absent/empty
///   pair account — not an error.
/// - `raw_storage` returns `B256::ZERO` for never-written slots.
/// - System-account getters return the zero value (`0` / `Address::ZERO`)
///   for never-written slots.
///
/// Conventions on writes:
/// - Setters take owned values so a caching impl can park them directly
///   without cloning.
/// - Account-code setters materialise the underlying account if needed
///   (raise `nonce` to ≥ 1 so EIP-161 doesn't prune at end-of-tx).
/// - `ensure_raw_account` raises `nonce` to ≥ 1 for a storage-only
///   account. `set_raw_storage` may or may not do this implicitly;
///   callers should call `ensure_raw_account` before the first write.
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

    // ── Raw storage (Tier-2 B+ tree index nodes) ──────────────────

    fn raw_storage(&mut self, addr: &Address, slot: B256) -> Result<B256>;
    fn set_raw_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()>;
    fn ensure_raw_account(&mut self, addr: &Address) -> Result<()>;
}
