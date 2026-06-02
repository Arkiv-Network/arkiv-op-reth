//! Cache-aware revm-backed [`Store`] used during canonical execution.
//!
//! Sibling to [`super::ReadWriteStore`]. Wraps a `ReadWriteStore`
//! together with an optional `&mut CacheStore`. With the cache
//! present, typed reads and writes go through the cache; the
//! underlying byte-level writes are deferred to
//! [`Self::flush`] at `BlockExecutor::finish`. With the cache absent
//! (`None`, e.g. speculative lanes), behavior collapses to the same
//! passthrough that `ReadWriteStore` performs today.
//!
//! See `account-cache-design.md` §6 for the layered semantics.

use alloy_evm::EvmInternals;
use alloy_primitives::Address;
use arkiv_entitydb::{
    Bitmap, Entity, IndexTree, Store, index_address, pair_address,
};
use eyre::Result;

use super::cache_store::{CacheStore, Cached};
use super::read_write_store::ReadWriteStore;
use super::trie_layout::entity_to_code;

pub struct CachedReadWriteStore<'a, 'b, 'c> {
    inner: ReadWriteStore<'a, 'b>,
    cache: Option<&'c mut CacheStore>,
}

impl<'a, 'b, 'c> CachedReadWriteStore<'a, 'b, 'c> {
    pub fn new(
        internals: &'a mut EvmInternals<'b>,
        cache: Option<&'c mut CacheStore>,
    ) -> Self {
        Self {
            inner: ReadWriteStore::new(internals),
            cache,
        }
    }

    /// Flush the cache's per-block dirty entries to revm's
    /// `State<DB>` as byte-level writes. Called at
    /// `BlockExecutor::finish` after the cache has been taken back
    /// from the session-cache map.
    ///
    /// No-op when there is no cache attached (speculative lanes).
    pub fn flush(&mut self) -> Result<()> {
        let Some(cache) = self.cache.as_deref_mut() else {
            return Ok(());
        };
        let entries = cache.drain_block_dirty();
        for (addr, value) in entries {
            match value {
                Cached::Entity(e) => self.inner.set_code(&addr, entity_to_code(&e))?,
                Cached::Bitmap(b) => self.inner.set_code(&addr, b.to_bytes())?,
                Cached::Tree(t) => self.inner.set_code(&addr, t.to_bytes())?,
                Cached::Tombstone => self.inner.tombstone_code(&addr)?,
            }
        }
        Ok(())
    }
}

impl Store for CachedReadWriteStore<'_, '_, '_> {
    // ── System-account slots — passthrough, not cached ────────────────

    fn get_entity_count(&mut self) -> Result<u64> {
        self.inner.get_entity_count()
    }

    fn set_entity_count(&mut self, count: u64) -> Result<()> {
        self.inner.set_entity_count(count)
    }

    fn get_id_to_addr(&mut self, id: u64) -> Result<Address> {
        self.inner.get_id_to_addr(id)
    }

    fn set_id_to_addr(&mut self, id: u64, addr: Address) -> Result<()> {
        self.inner.set_id_to_addr(id, addr)
    }

    fn get_addr_to_id(&mut self, addr: &Address) -> Result<u64> {
        self.inner.get_addr_to_id(addr)
    }

    fn set_addr_to_id(&mut self, addr: &Address, id: u64) -> Result<()> {
        self.inner.set_addr_to_id(addr, id)
    }

    fn get_nonce(&mut self, caller: &Address) -> Result<u32> {
        self.inner.get_nonce(caller)
    }

    fn set_nonce(&mut self, caller: &Address, nonce: u32) -> Result<()> {
        self.inner.set_nonce(caller, nonce)
    }

    // ── Entity accounts ───────────────────────────────────────────────

    fn get_entity(&mut self, addr: &Address) -> Result<Option<Entity>> {
        if let Some(cache) = self.cache.as_deref_mut() {
            if let Some(cached) = cache.get(addr) {
                return match cached {
                    Cached::Entity(e) => Ok(Some(e.clone())),
                    Cached::Tombstone => Ok(None),
                    other => Err(eyre::eyre!(
                        "cache type mismatch at {addr}: expected Entity, got {other:?}"
                    )),
                };
            }
            let entity = self.inner.get_entity(addr)?;
            let cached_val = entity
                .as_ref()
                .map_or(Cached::Tombstone, |e| Cached::Entity(e.clone()));
            cache.insert_clean(*addr, cached_val);
            Ok(entity)
        } else {
            self.inner.get_entity(addr)
        }
    }

    fn set_entity(&mut self, addr: &Address, entity: Entity) -> Result<()> {
        if let Some(cache) = self.cache.as_deref_mut() {
            cache.stage(*addr, Cached::Entity(entity));
            Ok(())
        } else {
            self.inner.set_entity(addr, entity)
        }
    }

    fn tombstone_entity(&mut self, addr: &Address) -> Result<()> {
        if let Some(cache) = self.cache.as_deref_mut() {
            cache.stage(*addr, Cached::Tombstone);
            Ok(())
        } else {
            self.inner.tombstone_entity(addr)
        }
    }

    // ── Tier-1 pair-bitmap accounts ───────────────────────────────────

    fn get_pair_bitmap(&mut self, annot_key: &[u8], annot_val: &[u8]) -> Result<Bitmap> {
        let addr = pair_address(annot_key, annot_val);
        if let Some(cache) = self.cache.as_deref_mut() {
            if let Some(cached) = cache.get(&addr) {
                return match cached {
                    Cached::Bitmap(b) => Ok(b.clone()),
                    other => Err(eyre::eyre!(
                        "cache type mismatch at {addr}: expected Bitmap, got {other:?}"
                    )),
                };
            }
            let bitmap = self.inner.get_pair_bitmap(annot_key, annot_val)?;
            cache.insert_clean(addr, Cached::Bitmap(bitmap.clone()));
            Ok(bitmap)
        } else {
            self.inner.get_pair_bitmap(annot_key, annot_val)
        }
    }

    fn set_pair_bitmap(
        &mut self,
        annot_key: &[u8],
        annot_val: &[u8],
        bitmap: Bitmap,
    ) -> Result<()> {
        if let Some(cache) = self.cache.as_deref_mut() {
            let addr = pair_address(annot_key, annot_val);
            cache.stage(addr, Cached::Bitmap(bitmap));
            Ok(())
        } else {
            self.inner.set_pair_bitmap(annot_key, annot_val, bitmap)
        }
    }

    // ── Tier-2 ART index accounts ─────────────────────────────────────

    fn get_index_tree(&mut self, attr_key: &[u8]) -> Result<IndexTree> {
        let addr = index_address(attr_key);
        if let Some(cache) = self.cache.as_deref_mut() {
            if let Some(cached) = cache.get(&addr) {
                return match cached {
                    Cached::Tree(t) => Ok(t.clone()),
                    Cached::Tombstone => Ok(IndexTree::new()),
                    other => Err(eyre::eyre!(
                        "cache type mismatch at {addr}: expected Tree, got {other:?}"
                    )),
                };
            }
            let tree = self.inner.get_index_tree(attr_key)?;
            cache.insert_clean(addr, Cached::Tree(tree.clone()));
            Ok(tree)
        } else {
            self.inner.get_index_tree(attr_key)
        }
    }

    fn set_index_tree(&mut self, attr_key: &[u8], tree: IndexTree) -> Result<()> {
        if let Some(cache) = self.cache.as_deref_mut() {
            let addr = index_address(attr_key);
            cache.stage(addr, Cached::Tree(tree));
            Ok(())
        } else {
            self.inner.set_index_tree(attr_key, tree)
        }
    }

    fn tombstone_index_tree(&mut self, attr_key: &[u8]) -> Result<()> {
        if let Some(cache) = self.cache.as_deref_mut() {
            let addr = index_address(attr_key);
            cache.stage(addr, Cached::Tombstone);
            Ok(())
        } else {
            self.inner.tombstone_index_tree(attr_key)
        }
    }
}
