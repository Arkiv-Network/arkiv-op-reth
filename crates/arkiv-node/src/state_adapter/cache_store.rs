//! Per-block typed account cache with a per-tx shadow layer.
//!
//! Sits between an op-handler call and revm's `State<DB>` for the
//! cacheable account categories (entity, Tier-1 pair-bitmap, Tier-2
//! index-tree). System-account slots and the scratch account are
//! deliberately not cached — see `account-cache-design.md` §7.
//!
//! Pure data structure: no I/O, no `EvmInternals` handle. The reads
//! and the deferred flushes happen in
//! [`super::cached_read_write_state_adapter::CachedReadWriteStateAdapter`], which
//! borrows a `&mut CacheStore` for the duration of a precompile call.
//!
//! See `account-cache-design.md` §3 for the layering rationale.

use std::collections::{HashMap, HashSet};

use alloy_primitives::Address;
use arkiv_entitydb::{Bitmap, Entity, IndexTree};

/// Typed value cached at an account address.
///
/// `Tombstone` is the cached form of "this account holds empty code"
/// — at flush time it maps to `tombstone_code(addr)`. It covers both
/// deleted entities and emptied index trees (both encoded the same
/// way on the trie). Pair-bitmap accounts never tombstone — an empty
/// `Bitmap` serialises as normal bytes.
#[derive(Clone)]
pub enum Cached {
    Entity(Entity),
    Bitmap(Bitmap),
    Tree(IndexTree),
    Tombstone,
}

impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cached::Entity(_) => f.write_str("Cached::Entity(..)"),
            Cached::Bitmap(_) => f.write_str("Cached::Bitmap(..)"),
            Cached::Tree(_) => f.write_str("Cached::Tree(..)"),
            Cached::Tombstone => f.write_str("Cached::Tombstone"),
        }
    }
}

/// Two-layer typed cache.
///
/// - `block` / `block_dirty`: per-block layer. Holds every address
///   touched since the block-executor entered. `block_dirty` is the
///   subset that diverges from the underlying `State<DB>` and
///   therefore needs flushing at `BlockExecutor::finish`.
/// - `tx` / `tx_dirty`: per-tx shadow layer. Holds the in-flight
///   precompile call's staged writes. Folded into the block layer on
///   [`Self::commit_tx`]; dropped on [`Self::rollback_tx`].
#[derive(Debug, Default)]
pub struct CacheStore {
    pub block: HashMap<Address, Cached>,
    pub block_dirty: HashSet<Address>,
    pub tx: HashMap<Address, Cached>,
    pub tx_dirty: HashSet<Address>,
}

impl CacheStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up through both layers. The tx layer shadows the block
    /// layer.
    pub fn get(&self, addr: &Address) -> Option<&Cached> {
        self.tx.get(addr).or_else(|| self.block.get(addr))
    }

    /// Stage a write in the tx layer; mark it dirty so [`Self::commit_tx`]
    /// promotes it into `block_dirty`.
    pub fn stage(&mut self, addr: Address, value: Cached) {
        self.tx.insert(addr, value);
        self.tx_dirty.insert(addr);
    }

    /// Record a clean read-through in the block layer. No-op if either
    /// layer already holds the address — staged writes (and earlier
    /// clean reads) take precedence.
    pub fn insert_clean(&mut self, addr: Address, value: Cached) {
        if self.tx.contains_key(&addr) || self.block.contains_key(&addr) {
            return;
        }
        self.block.insert(addr, value);
    }

    /// Per-tx commit: fold the tx layer into the block layer, promote
    /// `tx_dirty` keys into `block_dirty`.
    pub fn commit_tx(&mut self) {
        for addr in self.tx_dirty.drain() {
            if let Some(value) = self.tx.remove(&addr) {
                self.block.insert(addr, value);
                self.block_dirty.insert(addr);
            }
        }
        self.tx.clear();
    }

    /// Per-tx rollback: drop the tx layer. The block layer is
    /// untouched.
    pub fn rollback_tx(&mut self) {
        self.tx.clear();
        self.tx_dirty.clear();
    }

    /// Drain the per-block dirty entries as owned `(Address, Cached)`
    /// pairs. Called at flush time. After this call `block_dirty` is
    /// empty and the corresponding `block` entries are gone — clean
    /// read-throughs remain in `block` untouched.
    pub fn drain_block_dirty(&mut self) -> Vec<(Address, Cached)> {
        let mut out = Vec::with_capacity(self.block_dirty.len());
        for addr in self.block_dirty.drain() {
            let value = self
                .block
                .remove(&addr)
                .expect("block_dirty entry without block value");
            out.push((addr, value));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn entity_with_payload(payload: &[u8]) -> Cached {
        Cached::Entity(Entity {
            payload: payload.to_vec(),
            creator: Address::ZERO,
            created_at_block: 0,
            owner: Address::ZERO,
            expires_at: 0,
            content_type: Vec::new(),
            key: alloy_primitives::B256::ZERO,
            attributes: Vec::new(),
            last_modified_at_block: 0,
        })
    }

    // ─── Lookups ──────────────────────────────────────────────────────

    #[test]
    fn get_returns_none_when_empty() {
        let cache = CacheStore::new();
        assert!(cache.get(&addr(1)).is_none());
    }

    #[test]
    fn stage_then_get_sees_tx_value() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), Cached::Tombstone);
        assert!(matches!(cache.get(&addr(1)), Some(Cached::Tombstone)));
    }

    #[test]
    fn tx_layer_shadows_block_layer() {
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), entity_with_payload(b"old"));
        cache.stage(addr(1), entity_with_payload(b"new"));
        match cache.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"new"),
            other => panic!("expected Entity('new'), got {other:?}"),
        }
    }

    #[test]
    fn insert_clean_is_noop_when_block_already_has_key() {
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), entity_with_payload(b"first"));
        cache.insert_clean(addr(1), entity_with_payload(b"second"));
        match cache.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"first"),
            other => panic!("expected Entity('first'), got {other:?}"),
        }
    }

    #[test]
    fn insert_clean_is_noop_when_tx_already_has_key() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), entity_with_payload(b"staged"));
        cache.insert_clean(addr(1), entity_with_payload(b"loaded"));
        match cache.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"staged"),
            other => panic!("expected Entity('staged'), got {other:?}"),
        }
    }

    // ─── Commit / rollback ───────────────────────────────────────────

    #[test]
    fn commit_tx_folds_tx_into_block_and_marks_block_dirty() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), Cached::Tombstone);
        cache.commit_tx();

        assert!(cache.tx.is_empty());
        assert!(cache.tx_dirty.is_empty());
        assert!(matches!(cache.block.get(&addr(1)), Some(Cached::Tombstone)));
        assert!(cache.block_dirty.contains(&addr(1)));
    }

    #[test]
    fn rollback_tx_drops_tx_layer_without_touching_block() {
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), entity_with_payload(b"clean"));
        cache.stage(addr(1), entity_with_payload(b"staged"));
        cache.rollback_tx();

        // Block clean read survives; staged write is gone.
        match cache.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"clean"),
            other => panic!("expected Entity('clean'), got {other:?}"),
        }
        assert!(cache.tx_dirty.is_empty());
    }

    #[test]
    fn second_stage_to_same_addr_overwrites() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), entity_with_payload(b"first"));
        cache.stage(addr(1), entity_with_payload(b"second"));

        match cache.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"second"),
            other => panic!("expected Entity('second'), got {other:?}"),
        }
        cache.commit_tx();
        match cache.block.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"second"),
            other => panic!("expected Entity('second'), got {other:?}"),
        }
    }

    #[test]
    fn rollback_after_commit_only_drops_uncommitted_writes() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), Cached::Tombstone);
        cache.commit_tx();
        cache.stage(addr(2), Cached::Tombstone); // staged, not committed
        cache.rollback_tx();

        assert!(matches!(cache.get(&addr(1)), Some(Cached::Tombstone)));
        assert!(cache.get(&addr(2)).is_none());
    }

    #[test]
    fn multiple_commits_accumulate_block_dirty() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), Cached::Tombstone);
        cache.commit_tx();
        cache.stage(addr(2), Cached::Tombstone);
        cache.commit_tx();

        assert!(cache.block_dirty.contains(&addr(1)));
        assert!(cache.block_dirty.contains(&addr(2)));
        assert_eq!(cache.block_dirty.len(), 2);
    }

    // ─── Flush drain ─────────────────────────────────────────────────

    #[test]
    fn drain_block_dirty_returns_only_dirty_entries() {
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), entity_with_payload(b"clean")); // not dirty
        cache.stage(addr(2), Cached::Tombstone); // dirty after commit
        cache.commit_tx();

        let drained = cache.drain_block_dirty();
        let drained_addrs: Vec<Address> = drained.iter().map(|(a, _)| *a).collect();
        assert_eq!(drained_addrs, vec![addr(2)]);

        // Block dirty is empty; dirty value is gone from block; clean
        // read-through still in block.
        assert!(cache.block_dirty.is_empty());
        assert!(cache.block.get(&addr(2)).is_none());
        assert!(cache.block.contains_key(&addr(1)));
    }

    #[test]
    fn drain_block_dirty_is_empty_for_clean_only_block() {
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), entity_with_payload(b"read"));
        assert!(cache.drain_block_dirty().is_empty());
        // The clean entry survives — only dirty entries are drained.
        assert!(cache.block.contains_key(&addr(1)));
    }

    // ─── Tombstone semantics ─────────────────────────────────────────

    #[test]
    fn tombstone_overwrites_present_value_within_tx() {
        let mut cache = CacheStore::new();
        cache.stage(addr(1), entity_with_payload(b"about to delete"));
        cache.stage(addr(1), Cached::Tombstone);
        assert!(matches!(cache.get(&addr(1)), Some(Cached::Tombstone)));
        cache.commit_tx();
        assert!(matches!(cache.block.get(&addr(1)), Some(Cached::Tombstone)));
        assert!(cache.block_dirty.contains(&addr(1)));
    }

    #[test]
    fn read_then_write_within_tx_promotes_via_tx_layer() {
        // Read-through caches Tombstone in block; same tx stages a
        // Present; commit promotes Present + marks block_dirty.
        let mut cache = CacheStore::new();
        cache.insert_clean(addr(1), Cached::Tombstone);
        cache.stage(addr(1), entity_with_payload(b"created"));
        cache.commit_tx();

        match cache.block.get(&addr(1)) {
            Some(Cached::Entity(e)) => assert_eq!(e.payload, b"created"),
            other => panic!("expected Entity('created'), got {other:?}"),
        }
        assert!(cache.block_dirty.contains(&addr(1)));
    }
}
