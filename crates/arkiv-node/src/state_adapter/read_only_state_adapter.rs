//! reth-backed [`StateAdapter`] used by the `arkiv_*` RPC namespace against a
//! committed-state snapshot.

use alloy_primitives::{Address, B256, U256};
use arkiv_entitydb::{
    Bitmap, Entity, IndexTree, StateAdapter, index_address, pair_address,
};
use eyre::Result;
use reth_storage_api::{StateProvider, StateProviderBox};

use super::trie_layout::{
    SYSTEM_ACCOUNT_ADDRESS, entity_from_code, slot_addr_to_id, slot_entity_count,
    slot_id_to_addr, slot_nonces, storage_to_address, storage_to_u32, storage_to_u64,
};

pub struct ReadOnlyStateAdapter {
    state: StateProviderBox,
}

impl ReadOnlyStateAdapter {
    pub fn new(state: StateProviderBox) -> Self {
        Self { state }
    }

    fn code(&self, addr: &Address) -> Result<Vec<u8>> {
        Ok(self
            .state
            .account_code(addr)
            .map_err(|e| eyre::eyre!("account_code({addr}): {e}"))?
            .map(|bc| bc.original_bytes().to_vec())
            .unwrap_or_default())
    }

    fn storage(&self, addr: &Address, slot: B256) -> Result<B256> {
        let v = self
            .state
            .storage(*addr, slot)
            .map_err(|e| eyre::eyre!("storage({addr}, {slot}): {e}"))?
            .unwrap_or(U256::ZERO);
        Ok(B256::from(v.to_be_bytes()))
    }
}

impl StateAdapter for ReadOnlyStateAdapter {
    // ── System-account slots (reads only) ──────────────────────────

    fn get_entity_count(&mut self) -> Result<u64> {
        Ok(storage_to_u64(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_entity_count())?,
        ))
    }

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address> {
        Ok(storage_to_address(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_id_to_addr(id))?,
        ))
    }

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64> {
        Ok(storage_to_u64(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_addr_to_id(*addr))?,
        ))
    }

    fn get_nonce(&mut self, caller: &Address) -> Result<u32> {
        Ok(storage_to_u32(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_nonces(*caller))?,
        ))
    }

    fn set_entity_count(&mut self, _count: u64) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_entity_count called from query path")
    }

    fn set_id_to_addr(&mut self, _id: u64, _addr: Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_id_to_addr called from query path")
    }

    fn set_addr_to_id(&mut self, _addr: &Address, _id: u64) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_addr_to_id called from query path")
    }

    fn set_nonce(&mut self, _caller: &Address, _nonce: u32) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_nonce called from query path")
    }

    // ── Entity accounts ─────────────────────────────────────────────

    fn get_entity(&mut self, addr: &Address) -> Result<Option<Entity>> {
        let code = self.code(addr)?;
        if code.is_empty() {
            Ok(None)
        } else {
            Ok(Some(entity_from_code(&code)?))
        }
    }

    fn set_entity(&mut self, _addr: &Address, _entity: Entity) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_entity called from query path")
    }

    fn tombstone_entity(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: tombstone_entity called from query path")
    }

    // ── Tier-1 pair-bitmap accounts ─────────────────────────────────

    fn get_pair_bitmap(&mut self, annot_key: &[u8], annot_val: &[u8]) -> Result<Bitmap> {
        let code = self.code(&pair_address(annot_key, annot_val))?;
        if code.is_empty() {
            Ok(Bitmap::new())
        } else {
            Bitmap::from_bytes(&code)
        }
    }

    fn set_pair_bitmap(
        &mut self,
        _annot_key: &[u8],
        _annot_val: &[u8],
        _bitmap: Bitmap,
    ) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_pair_bitmap called from query path")
    }

    // ── Tier-2 ART index accounts ───────────────────────────────────

    fn get_index_tree(&mut self, attr_key: &[u8]) -> Result<IndexTree> {
        let code = self.code(&index_address(attr_key))?;
        if code.is_empty() {
            Ok(IndexTree::new())
        } else {
            IndexTree::from_bytes(&code)
        }
    }

    fn set_index_tree(&mut self, _attr_key: &[u8], _tree: IndexTree) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_index_tree called from query path")
    }

    fn tombstone_index_tree(&mut self, _attr_key: &[u8]) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: tombstone_index_tree called from query path")
    }
}
