//! RocksDB-backed [`Storage`] backend for Arkiv entities.
//!
//! Persists the latest state of every entity plus secondary indexes
//! (owner, creator, string attributes, numeric attributes) so the
//! `arkiv_query` JSON-RPC method can resolve queries efficiently.
//!
//! # Column families
//!
//! * `entities` — `entity_key (32B) -> serde_json(EntityRecord)`
//!   The current state of every live entity. Deleted entities are removed.
//!
//! * `entity_history` — `block_be (8B) || entity_key (32B) || op_be (4B) ->
//!   serde_json(Option<EntityRecord>)`. The pre-mutation state of an
//!   entity, recorded for every mutation. Used to undo a block during
//!   `handle_revert` / `handle_reorg`. `Option::None` represents
//!   "the entity did not exist before this op" (so undoing means
//!   deleting it).
//!
//! * `block_meta` — `block_be (8B) -> serde_json(BlockMeta)`.
//!   Records the block hash and the count of history entries for the
//!   block; used to validate revert ordering and detect missing blocks.
//!
//! * `owner_index` — `owner (20B) || entity_key (32B) -> []`. Live owner index.
//!
//! * `creator_index` — `creator (20B) || entity_key (32B) -> []`. Live creator index.
//!
//! * `attr_string` — `name || 0x00 || value || 0x00 || entity_key -> []`.
//!   String attribute index (forward). Lex-ordered for range scans.
//!   Names and values must not contain `0x00`.
//!
//! * `attr_numeric` — `name || 0x00 || value_be_u64 (8B) || entity_key -> []`.
//!   Numeric attribute index (forward). Sorted lexicographically, which
//!   matches numeric ordering thanks to big-endian encoding.
//!
//! * `meta` — small key/value store for chain head etc.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::{Address, B256};
use eyre::{Result, bail, eyre};
use rocksdb::{
    ColumnFamilyDescriptor, DBWithThreadMode, Direction, IteratorMode, MultiThreaded, Options,
    ReadOptions, WriteBatch,
};
use serde::{Deserialize, Serialize};

use crate::storage::entity::EntityRecord;
use crate::storage::{
    ArkivBlock, ArkivBlockRef, ArkivOperation, Storage,
};

type Db = DBWithThreadMode<MultiThreaded>;

const CF_ENTITIES: &str = "entities";
const CF_ENTITY_HISTORY: &str = "entity_history";
const CF_BLOCK_META: &str = "block_meta";
const CF_OWNER_INDEX: &str = "owner_index";
const CF_CREATOR_INDEX: &str = "creator_index";
const CF_ATTR_STRING: &str = "attr_string";
const CF_ATTR_NUMERIC: &str = "attr_numeric";
const CF_META: &str = "meta";

const META_KEY_HEAD_NUMBER: &[u8] = b"head_number";
const META_KEY_HEAD_HASH: &[u8] = b"head_hash";

#[derive(Debug, Serialize, Deserialize)]
struct BlockMeta {
    hash: B256,
    /// Number of `entity_history` entries written for this block.
    history_entries: u64,
}

/// RocksDB storage backend.
///
/// Use [`Self::open`] to open or create the database at a given path.
#[derive(Clone)]
pub struct RocksDbStore {
    db: Arc<Db>,
}

impl RocksDbStore {
    /// Open (or create) the RocksDB database at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let cf_opts = Options::default();
        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_ENTITIES, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_ENTITY_HISTORY, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_BLOCK_META, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_OWNER_INDEX, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_CREATOR_INDEX, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_ATTR_STRING, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_ATTR_NUMERIC, cf_opts.clone()),
            ColumnFamilyDescriptor::new(CF_META, cf_opts),
        ];

        let db = Db::open_cf_descriptors(&opts, path, cfs)
            .map_err(|e| eyre!("failed to open rocksdb: {}", e))?;
        Ok(Self { db: Arc::new(db) })
    }

    fn cf(&self, name: &str) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| eyre!("missing column family: {}", name))
    }

    // ---- Public read APIs (used by the JSON-RPC layer) ----------------------

    /// Look up a single entity by key.
    pub fn get_entity(&self, key: &B256) -> Result<Option<EntityRecord>> {
        let cf = self.cf(CF_ENTITIES)?;
        match self.db.get_cf(&cf, key.as_slice())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Total number of live entities.
    pub fn entity_count(&self) -> Result<u64> {
        let cf = self.cf(CF_ENTITIES)?;
        // RocksDB's `rocksdb.estimate-num-keys` is approximate; iterate for an
        // exact count. The DB is small enough in practice for `arkiv_getEntityCount`.
        let mut count: u64 = 0;
        let iter = self.db.iterator_cf(&cf, IteratorMode::Start);
        for item in iter {
            item?;
            count += 1;
        }
        Ok(count)
    }

    /// Iterate all live entities. Returns owned [`EntityRecord`]s in
    /// `entity_key` order.
    pub fn iter_entities(&self) -> Result<Vec<EntityRecord>> {
        let cf = self.cf(CF_ENTITIES)?;
        let mut out = Vec::new();
        for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
            let (_, v) = item?;
            out.push(serde_json::from_slice(&v)?);
        }
        Ok(out)
    }

    /// Iterate entities owned by `owner`, in `entity_key` order.
    pub fn iter_entities_by_owner(&self, owner: &Address) -> Result<Vec<EntityRecord>> {
        self.iter_entities_via_addr_index(CF_OWNER_INDEX, owner)
    }

    /// Iterate entities created by `creator`, in `entity_key` order.
    pub fn iter_entities_by_creator(&self, creator: &Address) -> Result<Vec<EntityRecord>> {
        self.iter_entities_via_addr_index(CF_CREATOR_INDEX, creator)
    }

    fn iter_entities_via_addr_index(
        &self,
        cf_name: &str,
        addr: &Address,
    ) -> Result<Vec<EntityRecord>> {
        let cf = self.cf(cf_name)?;
        let prefix = addr.as_slice().to_vec();
        let mut out = Vec::new();
        let mut read_opts = ReadOptions::default();
        read_opts.set_prefix_same_as_start(true);
        let iter = self
            .db
            .iterator_cf_opt(&cf, read_opts, IteratorMode::From(&prefix, Direction::Forward));
        for item in iter {
            let (k, _) = item?;
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() < 20 + 32 {
                continue;
            }
            let entity_key = B256::from_slice(&k[20..52]);
            if let Some(e) = self.get_entity(&entity_key)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Current chain head number persisted in `meta`. `None` before any commit.
    pub fn head_block_number(&self) -> Result<Option<u64>> {
        let cf = self.cf(CF_META)?;
        match self.db.get_cf(&cf, META_KEY_HEAD_NUMBER)? {
            Some(b) if b.len() == 8 => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&b);
                Ok(Some(u64::from_be_bytes(buf)))
            }
            _ => Ok(None),
        }
    }

    // ---- Internal write helpers -------------------------------------------

    fn history_key(block: u64, entity_key: &B256, op_index: u32) -> Vec<u8> {
        let mut k = Vec::with_capacity(8 + 32 + 4);
        k.extend_from_slice(&block.to_be_bytes());
        k.extend_from_slice(entity_key.as_slice());
        k.extend_from_slice(&op_index.to_be_bytes());
        k
    }

    fn owner_key(owner: &Address, entity_key: &B256) -> Vec<u8> {
        let mut k = Vec::with_capacity(20 + 32);
        k.extend_from_slice(owner.as_slice());
        k.extend_from_slice(entity_key.as_slice());
        k
    }

    fn creator_key(creator: &Address, entity_key: &B256) -> Vec<u8> {
        let mut k = Vec::with_capacity(20 + 32);
        k.extend_from_slice(creator.as_slice());
        k.extend_from_slice(entity_key.as_slice());
        k
    }

    fn attr_string_key(name: &str, value: &str, entity_key: &B256) -> Vec<u8> {
        let mut k = Vec::with_capacity(name.len() + value.len() + 2 + 32);
        k.extend_from_slice(name.as_bytes());
        k.push(0x00);
        k.extend_from_slice(value.as_bytes());
        k.push(0x00);
        k.extend_from_slice(entity_key.as_slice());
        k
    }

    fn attr_numeric_key(name: &str, value: u64, entity_key: &B256) -> Vec<u8> {
        let mut k = Vec::with_capacity(name.len() + 1 + 8 + 32);
        k.extend_from_slice(name.as_bytes());
        k.push(0x00);
        k.extend_from_slice(&value.to_be_bytes());
        k.extend_from_slice(entity_key.as_slice());
        k
    }

    /// Add a new entity's index entries to a write batch.
    fn add_indexes(&self, batch: &mut WriteBatch, entity: &EntityRecord) -> Result<()> {
        let owner_cf = self.cf(CF_OWNER_INDEX)?;
        let creator_cf = self.cf(CF_CREATOR_INDEX)?;
        let attr_s_cf = self.cf(CF_ATTR_STRING)?;
        let attr_n_cf = self.cf(CF_ATTR_NUMERIC)?;
        batch.put_cf(&owner_cf, Self::owner_key(&entity.owner, &entity.key), []);
        batch.put_cf(&creator_cf, Self::creator_key(&entity.creator, &entity.key), []);
        for (name, value) in &entity.string_attributes {
            if name.contains('\0') || value.contains('\0') {
                bail!("attribute name/value must not contain NUL byte");
            }
            batch.put_cf(&attr_s_cf, Self::attr_string_key(name, value, &entity.key), []);
        }
        for (name, value) in &entity.numeric_attributes {
            if name.contains('\0') {
                bail!("attribute name must not contain NUL byte");
            }
            batch.put_cf(&attr_n_cf, Self::attr_numeric_key(name, *value, &entity.key), []);
        }
        Ok(())
    }

    /// Remove an entity's index entries from a write batch.
    fn remove_indexes(&self, batch: &mut WriteBatch, entity: &EntityRecord) -> Result<()> {
        let owner_cf = self.cf(CF_OWNER_INDEX)?;
        let creator_cf = self.cf(CF_CREATOR_INDEX)?;
        let attr_s_cf = self.cf(CF_ATTR_STRING)?;
        let attr_n_cf = self.cf(CF_ATTR_NUMERIC)?;
        batch.delete_cf(&owner_cf, Self::owner_key(&entity.owner, &entity.key));
        batch.delete_cf(&creator_cf, Self::creator_key(&entity.creator, &entity.key));
        for (name, value) in &entity.string_attributes {
            batch.delete_cf(&attr_s_cf, Self::attr_string_key(name, value, &entity.key));
        }
        for (name, value) in &entity.numeric_attributes {
            batch.delete_cf(&attr_n_cf, Self::attr_numeric_key(name, *value, &entity.key));
        }
        Ok(())
    }

    /// Apply a single decoded operation to the write batch, returning
    /// the number of `entity_history` entries written (0 or 1).
    ///
    /// `pending` holds the effective per-entity state built up by earlier
    /// operations in the same block, so that within a block subsequent
    /// ops observe the cumulative state instead of stale on-disk state.
    /// `history_recorded` tracks which entities have already had their
    /// pre-block state snapshotted into the history CF, so we never
    /// snapshot an intermediate (in-block) state.
    fn apply_operation(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        tx_index: u32,
        op_index: u32,
        op: &ArkivOperation,
        pending: &mut HashMap<B256, Option<EntityRecord>>,
        history_recorded: &mut std::collections::HashSet<B256>,
    ) -> Result<u64> {
        let entities_cf = self.cf(CF_ENTITIES)?;
        let history_cf = self.cf(CF_ENTITY_HISTORY)?;

        let entity_key = match op {
            ArkivOperation::Create(o) => o.entity_key,
            ArkivOperation::Update(o) => o.entity_key,
            ArkivOperation::Extend(o) => o.entity_key,
            ArkivOperation::ChangeOwner(o) => o.entity_key,
            ArkivOperation::Delete(o) => o.entity_key,
            ArkivOperation::Expire(o) => o.entity_key,
        };

        // Effective prior: first check the in-flight pending state, then
        // fall back to the on-disk entity (which is the true pre-block state
        // for the first op touching this entity in this block).
        let prior: Option<EntityRecord> = match pending.get(&entity_key) {
            Some(p) => p.clone(),
            None => self.get_entity(&entity_key)?,
        };

        // Compute the new state (None => entity is being deleted).
        let new_state: Option<EntityRecord> = match op {
            ArkivOperation::Create(o) => Some(EntityRecord::from_parts(
                entity_key,
                o.owner,
                o.owner, // creator = owner at create time
                o.expires_at,
                block_number,
                block_number,
                tx_index,
                op_index,
                o.content_type.clone(),
                o.payload.clone(),
                &o.annotations,
            )),
            ArkivOperation::Update(o) => {
                let creator =
                    prior.as_ref().map(|p| p.creator).unwrap_or(o.owner);
                let created_at =
                    prior.as_ref().map(|p| p.created_at_block).unwrap_or(block_number);
                let expires_at = prior.as_ref().map(|p| p.expires_at).unwrap_or(0);
                Some(EntityRecord::from_parts(
                    entity_key,
                    o.owner,
                    creator,
                    expires_at,
                    created_at,
                    block_number,
                    tx_index,
                    op_index,
                    o.content_type.clone(),
                    o.payload.clone(),
                    &o.annotations,
                ))
            }
            ArkivOperation::Extend(o) => {
                let prior_ref = match &prior {
                    Some(p) => p,
                    None => {
                        // Extending a non-existent entity is a no-op for storage.
                        return Ok(0);
                    }
                };
                let mut next = prior_ref.clone();
                next.owner = o.owner;
                next.expires_at = o.expires_at;
                next.last_modified_at_block = block_number;
                next.transaction_index_in_block = tx_index;
                next.operation_index_in_transaction = op_index;
                Some(next)
            }
            ArkivOperation::ChangeOwner(o) => {
                let prior_ref = match &prior {
                    Some(p) => p,
                    None => return Ok(0),
                };
                let mut next = prior_ref.clone();
                next.owner = o.owner;
                next.last_modified_at_block = block_number;
                next.transaction_index_in_block = tx_index;
                next.operation_index_in_transaction = op_index;
                Some(next)
            }
            ArkivOperation::Delete(_) | ArkivOperation::Expire(_) => None,
        };

        // No-op (deleting a non-existent entity).
        if prior.is_none() && new_state.is_none() {
            return Ok(0);
        }

        // Record the *pre-block* state as a history entry, but only once per
        // entity per block. The first op for an entity has `prior == DB state`,
        // so its `prior` is exactly the pre-block state.
        let mut history_written = 0;
        if !history_recorded.contains(&entity_key) {
            let history_key = Self::history_key(block_number, &entity_key, op_index);
            let prior_bytes = serde_json::to_vec(&prior)?;
            batch.put_cf(&history_cf, &history_key, &prior_bytes);
            history_recorded.insert(entity_key);
            history_written = 1;
        }

        // Remove prior (effective) index entries, write the new state and its indexes.
        // RocksDB applies puts/deletes in batch order, so even if a later op
        // overwrites these, the final state is consistent.
        if let Some(p) = &prior {
            self.remove_indexes(batch, p)?;
        }
        match &new_state {
            Some(e) => {
                let bytes = serde_json::to_vec(e)?;
                batch.put_cf(&entities_cf, entity_key.as_slice(), &bytes);
                self.add_indexes(batch, e)?;
            }
            None => {
                batch.delete_cf(&entities_cf, entity_key.as_slice());
            }
        }

        // Update the in-block pending state for subsequent ops.
        pending.insert(entity_key, new_state);

        Ok(history_written)
    }

    /// Apply one block's operations to a write batch.
    fn apply_block(&self, batch: &mut WriteBatch, block: &ArkivBlock) -> Result<()> {
        let mut entries: u64 = 0;
        let mut pending: HashMap<B256, Option<EntityRecord>> = HashMap::new();
        let mut history_recorded: std::collections::HashSet<B256> =
            std::collections::HashSet::new();
        for tx in &block.transactions {
            for op in &tx.operations {
                let op_index = match op {
                    ArkivOperation::Create(o) => o.op_index,
                    ArkivOperation::Update(o) => o.op_index,
                    ArkivOperation::Extend(o) => o.op_index,
                    ArkivOperation::ChangeOwner(o) => o.op_index,
                    ArkivOperation::Delete(o) => o.op_index,
                    ArkivOperation::Expire(o) => o.op_index,
                };
                entries += self.apply_operation(
                    batch,
                    block.header.number,
                    tx.index,
                    op_index,
                    op,
                    &mut pending,
                    &mut history_recorded,
                )?;
            }
        }
        let meta_cf = self.cf(CF_BLOCK_META)?;
        let block_meta = BlockMeta { hash: block.header.hash, history_entries: entries };
        batch.put_cf(
            &meta_cf,
            block.header.number.to_be_bytes(),
            serde_json::to_vec(&block_meta)?,
        );
        // Update head pointer.
        let chain_meta_cf = self.cf(CF_META)?;
        batch.put_cf(&chain_meta_cf, META_KEY_HEAD_NUMBER, block.header.number.to_be_bytes());
        batch.put_cf(&chain_meta_cf, META_KEY_HEAD_HASH, block.header.hash.as_slice());
        Ok(())
    }

    /// Revert a single block by replaying its history entries in reverse.
    fn revert_block(&self, batch: &mut WriteBatch, block_number: u64) -> Result<()> {
        let history_cf = self.cf(CF_ENTITY_HISTORY)?;
        let entities_cf = self.cf(CF_ENTITIES)?;
        let block_meta_cf = self.cf(CF_BLOCK_META)?;

        // Collect all history entries for this block (sorted by key — i.e. by
        // entity_key then op_index). We need to undo in reverse insertion order:
        // the *last* mutation for a given entity in this block has the highest
        // (op_index) suffix and the prior state captured for it is the one
        // immediately before that op. To restore the entity to its
        // pre-block state, we undo each history entry, walking from the
        // *highest* op_index downward per entity. Equivalently: for each
        // entity, the earliest history entry holds the pre-block state
        // (because each op records prior state at the time it ran).
        //
        // Since the per-entity earliest entry is the pre-block snapshot,
        // we group history entries by entity_key, take the first (lowest
        // op_index) per entity, and use that as the restore state.
        let prefix = block_number.to_be_bytes().to_vec();
        let iter = self.db.iterator_cf_opt(
            &history_cf,
            ReadOptions::default(),
            IteratorMode::From(&prefix, Direction::Forward),
        );

        let mut earliest: HashMap<B256, (Vec<u8>, Option<EntityRecord>)> = HashMap::new();
        let mut history_keys_to_delete: Vec<Vec<u8>> = Vec::new();
        for item in iter {
            let (k, v) = item?;
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() != 8 + 32 + 4 {
                continue;
            }
            let entity_key = B256::from_slice(&k[8..40]);
            let history_key_vec = k.to_vec();
            history_keys_to_delete.push(history_key_vec.clone());
            let prior: Option<EntityRecord> = serde_json::from_slice(&v)?;
            // Only insert if we don't already have an earlier op_index for this entity.
            earliest.entry(entity_key).or_insert((history_key_vec, prior));
        }

        // Restore each entity to its pre-block state.
        for (entity_key, (_, prior)) in earliest {
            // First: tear down the *current* live state's index entries.
            if let Some(current) = self.get_entity(&entity_key)? {
                self.remove_indexes(batch, &current)?;
            }
            match prior {
                Some(prev) => {
                    let bytes = serde_json::to_vec(&prev)?;
                    batch.put_cf(&entities_cf, entity_key.as_slice(), &bytes);
                    self.add_indexes(batch, &prev)?;
                }
                None => {
                    batch.delete_cf(&entities_cf, entity_key.as_slice());
                }
            }
        }

        // Drop history + block meta.
        for k in history_keys_to_delete {
            batch.delete_cf(&history_cf, &k);
        }
        batch.delete_cf(&block_meta_cf, block_number.to_be_bytes());

        Ok(())
    }
}

impl Storage for RocksDbStore {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>> {
        let mut batch = WriteBatch::default();
        for block in blocks {
            self.apply_block(&mut batch, block)?;
        }
        self.db.write(batch)?;
        // We don't compute a state-trie root; return None so the caller knows
        // not to submit an on-chain commitment from this backend.
        Ok(None)
    }

    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>> {
        // `blocks` are newest-first; iterate in given order.
        let mut batch = WriteBatch::default();
        for block in blocks {
            self.revert_block(&mut batch, block.number)?;
        }
        // Update head pointer to the parent of the lowest reverted block.
        if let Some(lowest) = blocks.iter().map(|b| b.number).min() {
            let chain_meta_cf = self.cf(CF_META)?;
            if lowest > 0 {
                batch.put_cf(&chain_meta_cf, META_KEY_HEAD_NUMBER, (lowest - 1).to_be_bytes());
            } else {
                batch.delete_cf(&chain_meta_cf, META_KEY_HEAD_NUMBER);
                batch.delete_cf(&chain_meta_cf, META_KEY_HEAD_HASH);
            }
        }
        self.db.write(batch)?;
        Ok(None)
    }

    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>> {
        let mut batch = WriteBatch::default();
        for block in reverted {
            self.revert_block(&mut batch, block.number)?;
        }
        for block in new_blocks {
            self.apply_block(&mut batch, block)?;
        }
        self.db.write(batch)?;
        Ok(None)
    }
}

// ---- Helper conversions to RPC wire types -----------------------------------

/// Lightweight builder for `Annotation` lists (used in tests).
#[cfg(test)]
pub fn ann_string(k: &str, v: &str) -> crate::storage::Annotation {
    crate::storage::Annotation::String { key: k.into(), string_value: v.into() }
}

#[cfg(test)]
pub fn ann_numeric(k: &str, v: u64) -> crate::storage::Annotation {
    crate::storage::Annotation::Numeric { key: k.into(), numeric_value: v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use crate::storage::{
        ArkivBlockHeader, ArkivTransaction, ChangeOwnerOp, CreateOp, DeleteOp, ExtendOp, UpdateOp,
    };

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("rocksdb_store_test")
            .tempdir()
            .unwrap()
    }

    fn mk_block(number: u64, txs: Vec<ArkivTransaction>) -> ArkivBlock {
        ArkivBlock {
            header: ArkivBlockHeader {
                number,
                hash: B256::from_slice(&[number as u8; 32]),
                parent_hash: B256::ZERO,
                changeset_hash: None,
            },
            transactions: txs,
        }
    }

    fn create_op(key_byte: u8, owner_byte: u8) -> ArkivOperation {
        ArkivOperation::Create(CreateOp {
            op_index: 0,
            entity_key: B256::from_slice(&[key_byte; 32]),
            owner: Address::from_slice(&[owner_byte; 20]),
            expires_at: 1000,
            entity_hash: B256::ZERO,
            changeset_hash: B256::ZERO,
            payload: Bytes::from_static(b"hello"),
            content_type: "text/plain".into(),
            annotations: vec![ann_string("category", "docs"), ann_numeric("priority", 5)],
        })
    }

    #[test]
    fn commit_then_query() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();

        let tx = ArkivTransaction {
            hash: B256::ZERO,
            index: 0,
            sender: Address::ZERO,
            operations: vec![create_op(0xaa, 0x11), create_op(0xbb, 0x22)],
        };
        let block = mk_block(1, vec![tx]);

        store.handle_commit(&[block]).unwrap();
        assert_eq!(store.entity_count().unwrap(), 2);
        assert_eq!(store.head_block_number().unwrap(), Some(1));

        let e = store
            .get_entity(&B256::from_slice(&[0xaa; 32]))
            .unwrap()
            .unwrap();
        assert_eq!(e.payload.as_ref(), b"hello");
        assert_eq!(e.get_string("category"), Some("docs"));
        assert_eq!(e.get_numeric("priority"), Some(5));

        // Owner index works.
        let owned =
            store.iter_entities_by_owner(&Address::from_slice(&[0x11; 20])).unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].key, B256::from_slice(&[0xaa; 32]));
    }

    #[test]
    fn update_replaces_attributes_and_indexes() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();

        let key = B256::from_slice(&[0x33; 32]);
        let owner1 = Address::from_slice(&[0x44; 20]);
        let owner2 = Address::from_slice(&[0x55; 20]);

        let create = ArkivOperation::Create(CreateOp {
            op_index: 0,
            entity_key: key,
            owner: owner1,
            expires_at: 100,
            entity_hash: B256::ZERO,
            changeset_hash: B256::ZERO,
            payload: Bytes::from_static(b"v1"),
            content_type: "text/plain".into(),
            annotations: vec![ann_string("tag", "old"), ann_numeric("score", 1)],
        });
        let update = ArkivOperation::Update(UpdateOp {
            op_index: 1,
            entity_key: key,
            owner: owner2,
            entity_hash: B256::ZERO,
            changeset_hash: B256::ZERO,
            payload: Bytes::from_static(b"v2"),
            content_type: "text/markdown".into(),
            annotations: vec![ann_string("tag", "new"), ann_numeric("score", 2)],
        });

        let tx = ArkivTransaction {
            hash: B256::ZERO,
            index: 0,
            sender: Address::ZERO,
            operations: vec![create, update],
        };
        store.handle_commit(&[mk_block(1, vec![tx])]).unwrap();

        let e = store.get_entity(&key).unwrap().unwrap();
        assert_eq!(e.owner, owner2);
        assert_eq!(e.creator, owner1, "creator stays from create op");
        assert_eq!(e.payload.as_ref(), b"v2");
        assert_eq!(e.get_string("tag"), Some("new"));
        assert_eq!(e.get_numeric("score"), Some(2));

        // Old owner index entry must be gone.
        assert!(store.iter_entities_by_owner(&owner1).unwrap().is_empty());
        assert_eq!(store.iter_entities_by_owner(&owner2).unwrap().len(), 1);
    }

    #[test]
    fn revert_restores_pre_block_state() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();

        let key = B256::from_slice(&[0x77; 32]);

        // Block 1: create
        let tx1 = ArkivTransaction {
            hash: B256::ZERO,
            index: 0,
            sender: Address::ZERO,
            operations: vec![ArkivOperation::Create(CreateOp {
                op_index: 0,
                entity_key: key,
                owner: Address::from_slice(&[0x10; 20]),
                expires_at: 100,
                entity_hash: B256::ZERO,
                changeset_hash: B256::ZERO,
                payload: Bytes::from_static(b"v1"),
                content_type: "text/plain".into(),
                annotations: vec![ann_string("tag", "a")],
            })],
        };
        store.handle_commit(&[mk_block(1, vec![tx1])]).unwrap();

        // Block 2: update + extend
        let tx2 = ArkivTransaction {
            hash: B256::ZERO,
            index: 0,
            sender: Address::ZERO,
            operations: vec![
                ArkivOperation::Update(UpdateOp {
                    op_index: 0,
                    entity_key: key,
                    owner: Address::from_slice(&[0x10; 20]),
                    entity_hash: B256::ZERO,
                    changeset_hash: B256::ZERO,
                    payload: Bytes::from_static(b"v2"),
                    content_type: "text/plain".into(),
                    annotations: vec![ann_string("tag", "b")],
                }),
                ArkivOperation::Extend(ExtendOp {
                    op_index: 1,
                    entity_key: key,
                    owner: Address::from_slice(&[0x10; 20]),
                    expires_at: 200,
                    entity_hash: B256::ZERO,
                    changeset_hash: B256::ZERO,
                }),
            ],
        };
        store.handle_commit(&[mk_block(2, vec![tx2])]).unwrap();

        // Sanity: state after block 2.
        let e = store.get_entity(&key).unwrap().unwrap();
        assert_eq!(e.payload.as_ref(), b"v2");
        assert_eq!(e.get_string("tag"), Some("b"));
        assert_eq!(e.expires_at, 200);

        // Revert block 2.
        store
            .handle_revert(&[ArkivBlockRef {
                number: 2,
                hash: B256::from_slice(&[2u8; 32]),
            }])
            .unwrap();

        let e = store.get_entity(&key).unwrap().unwrap();
        assert_eq!(e.payload.as_ref(), b"v1", "block 2 ops are undone");
        assert_eq!(e.get_string("tag"), Some("a"));
        assert_eq!(e.expires_at, 100);
        assert_eq!(store.head_block_number().unwrap(), Some(1));
    }

    #[test]
    fn delete_then_revert_resurrects() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();
        let key = B256::from_slice(&[0x99; 32]);
        let owner = Address::from_slice(&[0x12; 20]);

        store
            .handle_commit(&[mk_block(
                1,
                vec![ArkivTransaction {
                    hash: B256::ZERO,
                    index: 0,
                    sender: Address::ZERO,
                    operations: vec![ArkivOperation::Create(CreateOp {
                        op_index: 0,
                        entity_key: key,
                        owner,
                        expires_at: 100,
                        entity_hash: B256::ZERO,
                        changeset_hash: B256::ZERO,
                        payload: Bytes::from_static(b"x"),
                        content_type: "text/plain".into(),
                        annotations: vec![],
                    })],
                }],
            )])
            .unwrap();

        store
            .handle_commit(&[mk_block(
                2,
                vec![ArkivTransaction {
                    hash: B256::ZERO,
                    index: 0,
                    sender: Address::ZERO,
                    operations: vec![ArkivOperation::Delete(DeleteOp {
                        op_index: 0,
                        entity_key: key,
                        owner,
                        entity_hash: B256::ZERO,
                        changeset_hash: B256::ZERO,
                    })],
                }],
            )])
            .unwrap();

        assert!(store.get_entity(&key).unwrap().is_none());

        store
            .handle_revert(&[ArkivBlockRef {
                number: 2,
                hash: B256::from_slice(&[2u8; 32]),
            }])
            .unwrap();

        assert!(store.get_entity(&key).unwrap().is_some(), "delete is undone");
    }

    #[test]
    fn change_owner_updates_owner_index() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();
        let key = B256::from_slice(&[0xab; 32]);
        let o1 = Address::from_slice(&[0x01; 20]);
        let o2 = Address::from_slice(&[0x02; 20]);

        store
            .handle_commit(&[mk_block(
                1,
                vec![ArkivTransaction {
                    hash: B256::ZERO,
                    index: 0,
                    sender: Address::ZERO,
                    operations: vec![
                        ArkivOperation::Create(CreateOp {
                            op_index: 0,
                            entity_key: key,
                            owner: o1,
                            expires_at: 100,
                            entity_hash: B256::ZERO,
                            changeset_hash: B256::ZERO,
                            payload: Bytes::new(),
                            content_type: "x".into(),
                            annotations: vec![],
                        }),
                        ArkivOperation::ChangeOwner(ChangeOwnerOp {
                            op_index: 1,
                            entity_key: key,
                            owner: o2,
                            entity_hash: B256::ZERO,
                            changeset_hash: B256::ZERO,
                        }),
                    ],
                }],
            )])
            .unwrap();

        assert!(store.iter_entities_by_owner(&o1).unwrap().is_empty());
        assert_eq!(store.iter_entities_by_owner(&o2).unwrap().len(), 1);
    }

    #[test]
    fn query_via_parser_filters_attribute_predicate() {
        use crate::query::parser;

        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();

        let mk_create = |k: u8, owner: u8, cat: &str, prio: u64| ArkivOperation::Create(CreateOp {
            op_index: k as u32,
            entity_key: B256::from_slice(&[k; 32]),
            owner: Address::from_slice(&[owner; 20]),
            expires_at: 1000,
            entity_hash: B256::ZERO,
            changeset_hash: B256::ZERO,
            payload: Bytes::from_static(b"data"),
            content_type: "text/plain".into(),
            annotations: vec![ann_string("category", cat), ann_numeric("priority", prio)],
        });
        let tx = ArkivTransaction {
            hash: B256::ZERO,
            index: 0,
            sender: Address::ZERO,
            operations: vec![
                mk_create(1, 0xaa, "docs", 1),
                mk_create(2, 0xaa, "docs", 5),
                mk_create(3, 0xbb, "code", 5),
            ],
        };
        store.handle_commit(&[mk_block(1, vec![tx])]).unwrap();

        // category = "docs" && priority >= 5
        let expr = parser::parse(r#"category = "docs" && priority >= 5"#).unwrap();
        let matches: Vec<_> = store
            .iter_entities()
            .unwrap()
            .into_iter()
            .filter(|e| expr.matches(e))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].key, B256::from_slice(&[2; 32]));

        // OR predicate
        let expr = parser::parse(r#"category = "code" || priority = 1"#).unwrap();
        let matches: Vec<_> = store
            .iter_entities()
            .unwrap()
            .into_iter()
            .filter(|e| expr.matches(e))
            .collect();
        assert_eq!(matches.len(), 2);

        // $owner selector: owner=0xbb...
        let expr =
            parser::parse("$owner = 0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let by_owner = store
            .iter_entities_by_owner(&Address::from_slice(&[0xbb; 20]))
            .unwrap();
        assert_eq!(by_owner.len(), 1);
        assert!(expr.matches(&by_owner[0]));
    }

    #[test]
    fn reorg_reverts_then_applies() {
        let dir = temp_dir();
        let store = RocksDbStore::open(dir.path()).unwrap();

        let key = B256::from_slice(&[0x42; 32]);
        let owner_a = Address::from_slice(&[0xaa; 20]);
        let owner_b = Address::from_slice(&[0xbb; 20]);

        // Block 1 on the abandoned chain.
        store
            .handle_commit(&[mk_block(
                1,
                vec![ArkivTransaction {
                    hash: B256::ZERO,
                    index: 0,
                    sender: Address::ZERO,
                    operations: vec![ArkivOperation::Create(CreateOp {
                        op_index: 0,
                        entity_key: key,
                        owner: owner_a,
                        expires_at: 100,
                        entity_hash: B256::ZERO,
                        changeset_hash: B256::ZERO,
                        payload: Bytes::from_static(b"old"),
                        content_type: "text/plain".into(),
                        annotations: vec![ann_string("tag", "abandoned")],
                    })],
                }],
            )])
            .unwrap();

        // Reorg: revert block 1, apply a new block 1 + block 2 on the canonical chain.
        let new_b1 = mk_block(
            1,
            vec![ArkivTransaction {
                hash: B256::ZERO,
                index: 0,
                sender: Address::ZERO,
                operations: vec![ArkivOperation::Create(CreateOp {
                    op_index: 0,
                    entity_key: key,
                    owner: owner_b,
                    expires_at: 200,
                    entity_hash: B256::ZERO,
                    changeset_hash: B256::ZERO,
                    payload: Bytes::from_static(b"new"),
                    content_type: "text/plain".into(),
                    annotations: vec![ann_string("tag", "canonical")],
                })],
            }],
        );

        store
            .handle_reorg(
                &[ArkivBlockRef { number: 1, hash: B256::from_slice(&[1u8; 32]) }],
                &[new_b1],
            )
            .unwrap();

        let e = store.get_entity(&key).unwrap().unwrap();
        assert_eq!(e.owner, owner_b);
        assert_eq!(e.payload.as_ref(), b"new");
        assert_eq!(e.get_string("tag"), Some("canonical"));
        // Old owner_a index entry must be gone after reorg.
        assert!(store.iter_entities_by_owner(&owner_a).unwrap().is_empty());
        assert_eq!(store.iter_entities_by_owner(&owner_b).unwrap().len(), 1);
    }

    #[test]
    fn reopen_persists_state() {
        let dir = temp_dir();
        let key = B256::from_slice(&[0x55; 32]);
        let owner = Address::from_slice(&[0x66; 20]);

        {
            let store = RocksDbStore::open(dir.path()).unwrap();
            store
                .handle_commit(&[mk_block(
                    1,
                    vec![ArkivTransaction {
                        hash: B256::ZERO,
                        index: 0,
                        sender: Address::ZERO,
                        operations: vec![ArkivOperation::Create(CreateOp {
                            op_index: 0,
                            entity_key: key,
                            owner,
                            expires_at: 100,
                            entity_hash: B256::ZERO,
                            changeset_hash: B256::ZERO,
                            payload: Bytes::from_static(b"persist"),
                            content_type: "text/plain".into(),
                            annotations: vec![ann_string("k", "v")],
                        })],
                    }],
                )])
                .unwrap();
        }

        // Reopen and verify state survived.
        let store = RocksDbStore::open(dir.path()).unwrap();
        let e = store.get_entity(&key).unwrap().unwrap();
        assert_eq!(e.payload.as_ref(), b"persist");
        assert_eq!(e.get_string("k"), Some("v"));
        assert_eq!(store.head_block_number().unwrap(), Some(1));
    }
}
