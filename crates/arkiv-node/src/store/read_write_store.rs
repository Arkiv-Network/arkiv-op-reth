//! revm-backed [`Store`] used by the precompile during block execution.

use alloy_evm::EvmInternals;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::{
    Bitmap, Entity, IndexTree, Store, index_address, pair_address,
};
use eyre::Result;
use revm::state::Bytecode;

use super::trie_layout::{
    SYSTEM_ACCOUNT_ADDRESS, address_to_storage, entity_from_code, entity_to_code,
    slot_addr_to_id, slot_entity_count, slot_id_to_addr, slot_nonces, storage_to_address,
    storage_to_u32, storage_to_u64, u32_to_storage, u64_to_storage,
};

pub struct ReadWriteStore<'a, 'b> {
    internals: &'a mut EvmInternals<'b>,
}

impl<'a, 'b> ReadWriteStore<'a, 'b> {
    pub fn new(internals: &'a mut EvmInternals<'b>) -> Self {
        Self { internals }
    }

    // ── Byte-level primitives (private to this impl) ───────────────

    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        let load = self
            .internals
            .load_account_code(*addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?;
        Ok(load
            .data
            .code()
            .map(|c| c.original_byte_slice().to_vec())
            .unwrap_or_default())
    }

    pub(super) fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::from(code));
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code({addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    pub(super) fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::new());
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code (tombstone, {addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let load = self
            .internals
            .sload(*addr, key)
            .map_err(|e| eyre::eyre!("sload({addr}, {slot}): {e:?}"))?;
        Ok(B256::from(load.data.to_be_bytes()))
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
        let key = U256::from_be_bytes(slot.0);
        let val = U256::from_be_bytes(value.0);
        self.internals
            .sstore(*addr, key, val)
            .map_err(|e| eyre::eyre!("sstore({addr}, {slot}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    /// `set_code` / `set_storage` don't bump the nonce; new accounts
    /// would land with `nonce = 0` and EIP-161 would prune them.
    /// Force `nonce >= 1`.
    fn ensure_nonce_at_least_one(&mut self, addr: Address) -> Result<()> {
        let nonce = self
            .internals
            .load_account_code(addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?
            .data
            .nonce();
        if nonce == 0 {
            self.internals
                .bump_nonce(addr)
                .map_err(|e| eyre::eyre!("bump_nonce({addr}): {e:?}"))?;
        }
        Ok(())
    }
}

impl Store for ReadWriteStore<'_, '_> {
    // ── System-account slots ────────────────────────────────────────

    fn get_entity_count(&mut self) -> Result<u64> {
        Ok(storage_to_u64(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_entity_count())?,
        ))
    }

    fn set_entity_count(&mut self, count: u64) -> Result<()> {
        self.set_storage(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_entity_count(),
            u64_to_storage(count),
        )
    }

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address> {
        Ok(storage_to_address(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_id_to_addr(id))?,
        ))
    }

    fn set_id_to_addr(&mut self, id: u64, addr: Address) -> Result<()> {
        self.set_storage(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_id_to_addr(id),
            address_to_storage(addr),
        )
    }

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64> {
        Ok(storage_to_u64(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_addr_to_id(*addr))?,
        ))
    }

    fn set_addr_to_id(&mut self, addr: &Address, id: u64) -> Result<()> {
        self.set_storage(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_addr_to_id(*addr),
            u64_to_storage(id),
        )
    }

    fn get_nonce(&mut self, caller: &Address) -> Result<u32> {
        Ok(storage_to_u32(
            self.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_nonces(*caller))?,
        ))
    }

    fn set_nonce(&mut self, caller: &Address, nonce: u32) -> Result<()> {
        self.set_storage(
            &SYSTEM_ACCOUNT_ADDRESS,
            slot_nonces(*caller),
            u32_to_storage(nonce),
        )
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

    fn set_entity(&mut self, addr: &Address, entity: Entity) -> Result<()> {
        self.set_code(addr, entity_to_code(&entity))
    }

    fn tombstone_entity(&mut self, addr: &Address) -> Result<()> {
        self.tombstone_code(addr)
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
        annot_key: &[u8],
        annot_val: &[u8],
        bitmap: Bitmap,
    ) -> Result<()> {
        self.set_code(&pair_address(annot_key, annot_val), bitmap.to_bytes())
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

    fn set_index_tree(&mut self, attr_key: &[u8], tree: IndexTree) -> Result<()> {
        self.set_code(&index_address(attr_key), tree.to_bytes())
    }

    fn tombstone_index_tree(&mut self, attr_key: &[u8]) -> Result<()> {
        self.tombstone_code(&index_address(attr_key))
    }
}
