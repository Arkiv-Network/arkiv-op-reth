//! Block-level bitmap read/write cache for pair-account bitmaps.
//!
//! # Two-layer design
//!
//! ```text
//! BlockBitmapCache  (Arc<Mutex<...>>, lives for one block)
//!   committed:           HashMap<Address, Bitmap>   ← read cache + accumulated writes
//!   dirty:               HashSet<Address>            ← needs flush into ResultAndState
//!   initial_code_hashes: HashMap<Address, B256>     ← code_hash before first write this block
//!
//! TxBitmapOverlay  (per precompile call, discarded on tx failure)
//!   writes:   HashMap<Address, Bitmap>  ← tentative mutations
//!   snapshot: HashMap<Address, Option<Bitmap>>  ← pre-tx values for rollback
//! ```
//!
//! # Read path
//! overlay.writes → committed → inner.code() (DB load, cached once per block)
//!
//! # Write path
//! Write to overlay only — no set_code during the tx.
//!
//! # On tx success
//! Merge overlay.writes → committed; mark dirty; clear overlay.
//!
//! # On tx failure
//! Restore committed from overlay.snapshot; clear overlay.
//! No journal entries exist for bitmaps, so revm has nothing to revert.
//!
//! # At end of `transact_raw`
//! For each dirty address inject a synthetic `Account` (carrying the new code blob)
//! into `ResultAndState.state`. The executor's `db.commit(state)` then persists the
//! code change — no `DatabaseCommit` bound needed on `ArkivOpEvm`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use arkiv_entitydb::Bitmap;
use revm::bytecode::JumpTable;
use revm::primitives::KECCAK_EMPTY;
use revm::state::{Account, AccountInfo, Bytecode, EvmState};

// ─── BlockBitmapCache ─────────────────────────────────────────────────

/// Per-block in-memory bitmap cache.
pub struct BlockBitmapCache {
    /// Authoritative in-memory bitmap state for this block.
    pub committed: HashMap<Address, Bitmap>,
    /// Pair addresses whose bitmaps were mutated since last flush.
    pub dirty: HashSet<Address>,
    /// Code hash of each pair account before its first write this block.
    /// Used as `original_info.code_hash` when synthesising Account entries
    /// for accounts that were never loaded into the revm journal during the
    /// flushing tx (warm cache hits).
    pub initial_code_hashes: HashMap<Address, B256>,
}

impl BlockBitmapCache {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            committed: HashMap::new(),
            dirty: HashSet::new(),
            initial_code_hashes: HashMap::new(),
        }))
    }

    /// Merge a successfully-committed tx overlay into the block cache.
    pub fn apply_tx(&mut self, writes: HashMap<Address, Bitmap>) {
        for (addr, bitmap) in writes {
            self.committed.insert(addr, bitmap);
            self.dirty.insert(addr);
        }
    }

    /// Restore cache entries from a pre-tx snapshot (on tx failure).
    pub fn restore(&mut self, snapshot: HashMap<Address, Option<Bitmap>>) {
        for (addr, old) in snapshot {
            match old {
                Some(bitmap) => { self.committed.insert(addr, bitmap); }
                None => { self.committed.remove(&addr); }
            }
        }
    }
}

// ─── TxBitmapOverlay ──────────────────────────────────────────────────

/// Per-precompile-call overlay. Tentative writes live here until the
/// precompile returns success, then merged into `BlockBitmapCache`.
#[derive(Default)]
pub struct TxBitmapOverlay {
    pub writes: HashMap<Address, Bitmap>,
    pub snapshot: HashMap<Address, Option<Bitmap>>,
}

// ─── Flush ────────────────────────────────────────────────────────────

fn make_bytecode(bytes: Bytes) -> (Bytecode, B256) {
    if bytes.is_empty() {
        return (Bytecode::new(), KECCAK_EMPTY);
    }
    let hash = keccak256(&bytes);
    let n = bytes.len();
    let table = JumpTable::from_slice(&vec![0u8; n.div_ceil(8)], n);
    (Bytecode::new_analyzed(bytes, n, table), hash)
}

/// Encode dirty bitmaps as synthetic `Account` entries and merge them into
/// `evm_state` (`ResultAndState.state`). The executor's `db.commit(state)`
/// then persists the code changes.
///
/// After this call `cache.dirty` is cleared.
pub fn flush_dirty_into_state(cache: &Arc<Mutex<BlockBitmapCache>>, evm_state: &mut EvmState) {
    let mut locked = cache.lock().unwrap();
    if locked.dirty.is_empty() {
        return;
    }

    let dirty: Vec<Address> = locked.dirty.drain().collect();
    for addr in dirty {
        let bitmap = match locked.committed.get(&addr) {
            Some(b) => b,
            None => continue,
        };

        let bytes = Bytes::from(bitmap.to_bytes());
        let (bytecode, new_hash) = make_bytecode(bytes);

        // original_info carries the pre-block code hash so State<DB> records
        // the correct revert anchor for this account.
        let original_hash = locked
            .initial_code_hashes
            .get(&addr)
            .copied()
            .unwrap_or(KECCAK_EMPTY);
        let original_info = AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: original_hash,
            code: None,
            account_id: None,
        };

        // If the account was already loaded during this tx (cold miss in
        // get_pair_bitmap), use that existing entry so its original_info is
        // preserved. Otherwise create a fresh entry from original_info.
        let entry = evm_state
            .entry(addr)
            .or_insert_with(|| Account::from(original_info));
        entry.info.code = Some(bytecode);
        entry.info.code_hash = new_hash;
        entry.mark_touch();
    }
}
