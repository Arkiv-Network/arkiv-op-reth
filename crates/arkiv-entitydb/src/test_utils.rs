//! In-memory test backend for the [`Store`] trait.
//!
//! [`MemStore`] is a pure typed cache: every category of state the
//! trait exposes (system counter, ID maps, nonces, entities, pair
//! bitmaps, ART indexes) is held directly as the typed value, in a
//! plain [`HashMap`]. There is no slot derivation, no byte packing,
//! no `SYSTEM_ACCOUNT_ADDRESS` — those are trie-encoding details and
//! they belong to the trie-backed [`Store`] impls in `arkiv-node`,
//! not to entitydb's tests.
//!
//! Op handlers go through the [`Store`] trait, so anything they do
//! against [`MemStore`] is logically identical to what they do against
//! the production adapters. Tests can both drive the op handlers and
//! inspect [`MemStore`]'s public fields directly to assert on the
//! resulting state.

use std::collections::HashMap;

use alloy_primitives::Address;
use eyre::Result;

use crate::{Bitmap, Entity, IndexTree, Store};

/// Test-only [`Store`] implementation. All state is held as typed
/// values; no serialisation or trie-layout concerns.
///
/// Fields are `pub` so tests can read/assert on internal state
/// directly without going through the trait. Tests that mutate state
/// should still go through the trait so that op handlers and the
/// store agree on semantics.
#[derive(Default, Clone)]
pub struct MemStore {
    pub entity_count: u64,
    pub id_to_addr: HashMap<u64, Address>,
    pub addr_to_id: HashMap<Address, u64>,
    pub nonces: HashMap<Address, u32>,
    /// Tombstoned / never-existed → absent from the map.
    pub entities: HashMap<Address, Entity>,
    pub pair_bitmaps: HashMap<(Vec<u8>, Vec<u8>), Bitmap>,
    /// Tombstoned / never-existed → absent from the map.
    pub index_trees: HashMap<Vec<u8>, IndexTree>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemStore {
    // ── System-account slots ────────────────────────────────────────

    fn get_entity_count(&mut self) -> Result<u64> {
        Ok(self.entity_count)
    }

    fn set_entity_count(&mut self, count: u64) -> Result<()> {
        self.entity_count = count;
        Ok(())
    }

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address> {
        Ok(self.id_to_addr.get(&id).copied().unwrap_or(Address::ZERO))
    }

    fn set_id_to_addr(&mut self, id: u64, addr: Address) -> Result<()> {
        // Mirror the trie semantics: a zero address is the "absent"
        // value, so we drop the entry rather than store an explicit
        // Address::ZERO.
        if addr == Address::ZERO {
            self.id_to_addr.remove(&id);
        } else {
            self.id_to_addr.insert(id, addr);
        }
        Ok(())
    }

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64> {
        Ok(self.addr_to_id.get(addr).copied().unwrap_or(0))
    }

    fn set_addr_to_id(&mut self, addr: &Address, id: u64) -> Result<()> {
        // Entity ID 0 is a valid ID (first create gets id=0), so we
        // cannot use 0 as an "absent" sentinel for inserts. But the
        // trie semantics for a clear-on-delete write *is* a 0-slot,
        // and callers that intend to clear pass id=0. Mirror that
        // exactly: store whatever the caller said, even 0. `get_*`
        // returns 0 for absent or explicitly-zero — caller knows the
        // context.
        self.addr_to_id.insert(*addr, id);
        Ok(())
    }

    fn get_nonce(&mut self, caller: &Address) -> Result<u32> {
        Ok(self.nonces.get(caller).copied().unwrap_or(0))
    }

    fn set_nonce(&mut self, caller: &Address, nonce: u32) -> Result<()> {
        self.nonces.insert(*caller, nonce);
        Ok(())
    }

    // ── Entity accounts ─────────────────────────────────────────────

    fn get_entity(&mut self, addr: &Address) -> Result<Option<Entity>> {
        Ok(self.entities.get(addr).cloned())
    }

    fn set_entity(&mut self, addr: &Address, entity: Entity) -> Result<()> {
        self.entities.insert(*addr, entity);
        Ok(())
    }

    fn tombstone_entity(&mut self, addr: &Address) -> Result<()> {
        self.entities.remove(addr);
        Ok(())
    }

    // ── Tier-1 pair-bitmap accounts ─────────────────────────────────

    fn get_pair_bitmap(&mut self, annot_key: &[u8], annot_val: &[u8]) -> Result<Bitmap> {
        Ok(self
            .pair_bitmaps
            .get(&(annot_key.to_vec(), annot_val.to_vec()))
            .cloned()
            .unwrap_or_default())
    }

    fn set_pair_bitmap(
        &mut self,
        annot_key: &[u8],
        annot_val: &[u8],
        bitmap: Bitmap,
    ) -> Result<()> {
        self.pair_bitmaps
            .insert((annot_key.to_vec(), annot_val.to_vec()), bitmap);
        Ok(())
    }

    // ── Tier-2 ART index accounts ───────────────────────────────────

    fn get_index_tree(&mut self, attr_key: &[u8]) -> Result<IndexTree> {
        Ok(self
            .index_trees
            .get(attr_key)
            .cloned()
            .unwrap_or_default())
    }

    fn set_index_tree(&mut self, attr_key: &[u8], tree: IndexTree) -> Result<()> {
        self.index_trees.insert(attr_key.to_vec(), tree);
        Ok(())
    }

    fn tombstone_index_tree(&mut self, attr_key: &[u8]) -> Result<()> {
        self.index_trees.remove(attr_key);
        Ok(())
    }
}
