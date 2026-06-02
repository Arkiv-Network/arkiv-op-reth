//! Trie-backed [`Store`] implementations for `arkiv-entitydb`.
//!
//! Three flavours, all sharing the on-trie encoding defined in
//! [`crate::trie_layout`]:
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

use std::collections::HashMap;

use alloy_evm::EvmInternals;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::{
    Bitmap, Entity, IndexTree, Store, index_address, pair_address,
};
use eyre::Result;
use reth_storage_api::{StateProvider, StateProviderBox};
use revm::state::Bytecode;

use crate::trie_layout::{
    SYSTEM_ACCOUNT_ADDRESS, address_to_storage, entity_from_code, entity_to_code,
    slot_addr_to_id, slot_entity_count, slot_id_to_addr, slot_nonces, storage_to_address,
    storage_to_u32, storage_to_u64, u32_to_storage, u64_to_storage,
};

// ─── ReadWriteStore — revm-backed (write path) ───────────────────────

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

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::from(code));
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code({addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
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

// ─── ReadOnlyStore — reth-backed (read path) ─────────────────────────

pub struct ReadOnlyStore {
    state: StateProviderBox,
}

impl ReadOnlyStore {
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

impl Store for ReadOnlyStore {
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
        eyre::bail!("ReadOnlyStore: set_entity_count called from query path")
    }

    fn set_id_to_addr(&mut self, _id: u64, _addr: Address) -> Result<()> {
        eyre::bail!("ReadOnlyStore: set_id_to_addr called from query path")
    }

    fn set_addr_to_id(&mut self, _addr: &Address, _id: u64) -> Result<()> {
        eyre::bail!("ReadOnlyStore: set_addr_to_id called from query path")
    }

    fn set_nonce(&mut self, _caller: &Address, _nonce: u32) -> Result<()> {
        eyre::bail!("ReadOnlyStore: set_nonce called from query path")
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
        eyre::bail!("ReadOnlyStore: set_entity called from query path")
    }

    fn tombstone_entity(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStore: tombstone_entity called from query path")
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
        eyre::bail!("ReadOnlyStore: set_pair_bitmap called from query path")
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
        eyre::bail!("ReadOnlyStore: set_index_tree called from query path")
    }

    fn tombstone_index_tree(&mut self, _attr_key: &[u8]) -> Result<()> {
        eyre::bail!("ReadOnlyStore: tombstone_index_tree called from query path")
    }
}

// ─── InMemoryStore — HashMap-backed, mirrors the on-trie layout ──────
//
// Holds a [`InMemoryStateDb`]: account address → `(nonce, code,
// storage)`. Encodes/decodes the system-account slots and the entity
// / pair / index accounts the same way the production adapters do, so
// any test exercising this store also exercises the canonical
// encoding.

/// Per-account state: nonce, code, and the storage slot map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountState {
    pub nonce: u64,
    pub code: Vec<u8>,
    pub storage: HashMap<B256, B256>,
}

/// Toy state DB: account address → [`AccountState`]. Stand-in for
/// revm's State<DB> in tests that want to drive a [`Store`] without
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

/// Thin [`Store`] over a borrowed [`InMemoryStateDb`]. Same trie
/// encoding as [`ReadWriteStore`] — values go through the slot
/// derivations and packed encodings in [`crate::trie_layout`].
pub struct InMemoryStore<'a> {
    db: &'a mut InMemoryStateDb,
}

impl<'a> InMemoryStore<'a> {
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

impl Store for InMemoryStore<'_> {
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
