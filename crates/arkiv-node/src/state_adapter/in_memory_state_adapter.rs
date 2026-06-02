//! HashMap-backed [`StateAdapter`] that mirrors the on-trie layout, for tests
//! that want to verify the trie encoding without booting a full reth
//! node.

use std::collections::HashMap;

use alloy_primitives::{Address, B256};
use arkiv_entitydb::{
    Bitmap, Entity, IndexTree, StateAdapter, index_address, pair_address,
};
use eyre::Result;

use super::trie_layout::{
    SYSTEM_ACCOUNT_ADDRESS, address_to_storage, entity_from_code, entity_to_code,
    slot_addr_to_id, slot_entity_count, slot_id_to_addr, slot_nonces, storage_to_address,
    storage_to_u32, storage_to_u64, u32_to_storage, u64_to_storage,
};

/// Per-account state: nonce, code, and the storage slot map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountState {
    pub nonce: u64,
    pub code: Vec<u8>,
    pub storage: HashMap<B256, B256>,
}

/// Toy state DB: account address → [`AccountState`]. Stand-in for
/// revm's State<DB> in tests that want to drive a [`StateAdapter`] without
/// booting reth.
#[derive(Debug, Clone, Default)]
pub struct InMemoryStateDb {
    accounts: HashMap<Address, AccountState>,
}

impl InMemoryStateDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn account(&self, addr: &Address) -> Option<&AccountState> {
        self.accounts.get(addr)
    }

    pub fn account_mut(&mut self, addr: &Address) -> &mut AccountState {
        self.accounts.entry(*addr).or_default()
    }
}

/// Thin [`StateAdapter`] over a borrowed [`InMemoryStateDb`]. Same trie
/// encoding as [`super::ReadWriteStateAdapter`] — values go through the slot
/// derivations and packed encodings in [`super::trie_layout`].
pub struct InMemoryStateAdapter<'a> {
    db: &'a mut InMemoryStateDb,
}

impl<'a> InMemoryStateAdapter<'a> {
    pub fn new(db: &'a mut InMemoryStateDb) -> Self {
        Self { db }
    }

    fn read_code(&self, addr: &Address) -> Vec<u8> {
        self.db
            .account(addr)
            .map(|a| a.code.clone())
            .unwrap_or_default()
    }

    fn write_code(&mut self, addr: &Address, code: Vec<u8>) {
        let acc = self.db.account_mut(addr);
        acc.code = code;
        if acc.nonce == 0 {
            acc.nonce = 1;
        }
    }

    fn read_slot(&self, addr: &Address, slot: B256) -> B256 {
        self.db
            .account(addr)
            .and_then(|a| a.storage.get(&slot).copied())
            .unwrap_or_default()
    }

    fn write_slot(&mut self, addr: &Address, slot: B256, value: B256) {
        let acc = self.db.account_mut(addr);
        acc.storage.insert(slot, value);
        if acc.nonce == 0 {
            acc.nonce = 1;
        }
    }
}

impl StateAdapter for InMemoryStateAdapter<'_> {
    // ── System-account slots ────────────────────────────────────────

    fn get_entity_count(&mut self) -> Result<u64> {
        Ok(storage_to_u64(self.read_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_entity_count(),
        )))
    }

    fn set_entity_count(&mut self, count: u64) -> Result<()> {
        self.write_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_entity_count(),
            u64_to_storage(count),
        );
        Ok(())
    }

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address> {
        Ok(storage_to_address(self.read_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_id_to_addr(id),
        )))
    }

    fn set_id_to_addr(&mut self, id: u64, addr: Address) -> Result<()> {
        self.write_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_id_to_addr(id),
            address_to_storage(addr),
        );
        Ok(())
    }

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64> {
        Ok(storage_to_u64(self.read_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_addr_to_id(*addr),
        )))
    }

    fn set_addr_to_id(&mut self, addr: &Address, id: u64) -> Result<()> {
        self.write_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_addr_to_id(*addr),
            u64_to_storage(id),
        );
        Ok(())
    }

    fn get_nonce(&mut self, caller: &Address) -> Result<u32> {
        Ok(storage_to_u32(self.read_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_nonces(*caller),
        )))
    }

    fn set_nonce(&mut self, caller: &Address, nonce: u32) -> Result<()> {
        self.write_slot(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_nonces(*caller),
            u32_to_storage(nonce),
        );
        Ok(())
    }

    // ── Entity accounts ─────────────────────────────────────────────

    fn get_entity(&mut self, addr: &Address) -> Result<Option<Entity>> {
        let code = self.read_code(addr);
        if code.is_empty() {
            Ok(None)
        } else {
            Ok(Some(entity_from_code(&code)?))
        }
    }

    fn set_entity(&mut self, addr: &Address, entity: Entity) -> Result<()> {
        self.write_code(addr, entity_to_code(&entity));
        Ok(())
    }

    fn tombstone_entity(&mut self, addr: &Address) -> Result<()> {
        self.write_code(addr, Vec::new());
        Ok(())
    }

    // ── Tier-1 pair-bitmap accounts ─────────────────────────────────

    fn get_pair_bitmap(&mut self, annot_key: &[u8], annot_val: &[u8]) -> Result<Bitmap> {
        let code = self.read_code(&pair_address(annot_key, annot_val));
        if code.is_empty() {
            Ok(Bitmap::new())
        } else {
            Bitmap::from_bytes(&code)
        }
    }

    fn set_pair_bitmap(
        &mut self,
        annot_key: &[u8],
        annot_val: &[u8],
        bitmap: Bitmap,
    ) -> Result<()> {
        self.write_code(&pair_address(annot_key, annot_val), bitmap.to_bytes());
        Ok(())
    }

    // ── Tier-2 ART index accounts ───────────────────────────────────

    fn get_index_tree(&mut self, attr_key: &[u8]) -> Result<IndexTree> {
        let code = self.read_code(&index_address(attr_key));
        if code.is_empty() {
            Ok(IndexTree::new())
        } else {
            IndexTree::from_bytes(&code)
        }
    }

    fn set_index_tree(&mut self, attr_key: &[u8], tree: IndexTree) -> Result<()> {
        self.write_code(&index_address(attr_key), tree.to_bytes());
        Ok(())
    }

    fn tombstone_index_tree(&mut self, attr_key: &[u8]) -> Result<()> {
        self.write_code(&index_address(attr_key), Vec::new());
        Ok(())
    }
}
