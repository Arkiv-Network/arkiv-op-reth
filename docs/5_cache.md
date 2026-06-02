# Arkiv Account Cache

This document is the canonical spec for the per-block account cache
that sits between Arkiv's precompile and revm's `State<DB>`. Read
this if you're touching the precompile, the `BlockExecutor` wrapper,
or any `StateAdapter` impl.

For higher-level context see [`1_overview.md`](1_overview.md); for
the on-trie state layout the cache amortises see
[`2_state-model.md`](2_state-model.md); for crate-level engineering
details see [`4_engineering.md`](4_engineering.md).

## Contents

- [Abstract](#abstract)
- [1. Architecture](#1-architecture)
- [2. Tagging State<DB> with a session id](#2-tagging-statedb-with-a-session-id)
- [3. The CacheStore](#3-the-cachestore)
- [4. The BlockExecutor wrapper](#4-the-blockexecutor-wrapper)
- [5. CachedReadWriteStateAdapter](#5-cachedreadwritestateadapter)
- [6. What is and isn't cached](#6-what-is-and-isnt-cached)
- [7. Speculative lanes](#7-speculative-lanes)
- [8. Invariants preserved](#8-invariants-preserved)
- [9. Known limitations](#9-known-limitations)

---

## Abstract

Arkiv's write path repeatedly touches the same entity, pair, and
index accounts within a single block. Every op decodes the bytes out
of revm's `State<DB>`, mutates the typed value
(`Entity` / `Bitmap` / `IndexTree`), re-encodes, and writes back —
even when the next op is going to mutate the same account a moment
later. A batch that updates four annotations on one entity walks the
same pair bitmap four times; a tx that creates ten entities walks the
`$all` / `$creator` / `$owner` / ... bitmaps for each. The serde
round-trip dominates the cost.

**The fundamental building block is binding a typed in-memory cache
to each `State<DB>` instance via a transient session id minted at
block entry.** Each in-flight block-execution pass on a node —
payload build, canonical execution, validation, reorg replay —
carries its own `State<DB>` working copy, and the cache must be bound
to that copy: different passes for the same `block_number` must not
see each other's in-flight typed values. The `BlockExecutor` wrapper
that owns a given pass mints a random `SessionId` at
`apply_pre_execution_changes` and writes it to slot 0 of
`ARKIV_ADDRESS` through revm's journal. The slot write is bound to
this `State<DB>` instance; other passes' instances see zero (or their
own id). On every Arkiv precompile call, `derive_session(internals)`
SLOADs slot 0 and uses the result as a key into a per-node
`Arc<Mutex<HashMap<SessionId, CacheStore>>>`.

The session id is **transient and consensus-irrelevant**: it lives
only in process memory and on slot 0 of `ARKIV_ADDRESS`, where it
round-trips through `0 → sid → 0` within the block. Different nodes
mint different ids for the same canonical block and still arrive at
byte-identical merged bundles — the id never lands in `stateRoot`
(§2).

The cache itself is two-layered, mirroring Arkiv's per-tx atomicity
on top of revm's per-tx state machine. A **per-block layer** holds
the typed values for every account touched since the
`BlockExecutor` entered the block. A **per-tx layer** sits on top,
holding the in-flight precompile call's staged writes — it folds
into the block layer when the call returns success and is dropped
on revert.

Net effect on the hot path: one decode the first time a given
account is touched in a block, one encode at
`BlockExecutor::finish`. Everything in between is typed-value
mutation in memory. Bytes flushed at `finish` are byte-identical to
what the no-cache path would have written, so `stateRoot` is
unaffected.

---

## 1. Architecture

Four layers, top-down:

```
┌─────────────────────────────────────────────────────────────────────┐
│  ArkivOpEvmConfig (in arkiv-node)                                   │
│    owns Arc<Mutex<HashMap<SessionId, CacheStore>>>                  │
│    clones it into every BlockExecutor + every precompile closure    │
└──────────────────────────┬──────────────────────────────────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  ArkivOpBlockExecutor (one per State<DB> pass)                      │
│    apply_pre: mint SessionId + insert empty CacheStore + SSTORE     │
│               session id to slot 0 of ARKIV_ADDRESS                 │
│    finish:    SESSION_FLUSH (drain cache → State<DB>) +             │
│               SESSION_CLEAR (zero the slot)                          │
└──────────────────────────┬──────────────────────────────────────────┘
                           │  session id observable via SLOAD
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Arkiv precompile (one call per Arkiv tx)                           │
│    derive_session(internals) → SessionKind                          │
│    check CacheStore out of session map under brief lock             │
│    run ops through CachedReadWriteStateAdapter                       │
│    on Ok:  cache.commit_tx()                                         │
│    on Err: cache.rollback_tx()                                       │
│    check the CacheStore back in                                      │
└──────────────────────────┬──────────────────────────────────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  CacheStore (pure data structure)                                   │
│    per-block:  HashMap<Address, Cached> + dirty set                 │
│    per-tx:     shadow map + dirty delta                              │
└─────────────────────────────────────────────────────────────────────┘
```

**Layered responsibilities:**

- The `ArkivOpEvmConfig` owns the `SessionCacheMap` for the lifetime
  of the node. The map is `Arc`-cloned into every `BlockExecutor`
  wrapper the factory builds and into every precompile closure the
  EVM factory installs.
- The `ArkivOpBlockExecutor` wrapper drives the per-pass lifecycle.
  It owns the `SessionId` for its pass, inserts and removes the
  `CacheStore` from the map, and issues the slot writes via
  `evm.transact_system_call` so they go through revm's journal.
- The Arkiv precompile is invoked once per Arkiv tx via a normal
  `CALL` to `ARKIV_ADDRESS`. It reads the session beacon, checks the
  `CacheStore` out of the map, runs the batch with reads/writes
  routed through `CachedReadWriteStateAdapter`, and commits or rolls
  back the per-tx layer based on the outcome.
- The `CacheStore` is a pure data structure with no I/O — no
  `EvmInternals` handle, no revm dependency. It holds typed values
  and tracks dirty sets per layer.

`arkiv-entitydb` is cache-unaware throughout. Its op handlers take
`&mut impl StateAdapter` exactly as before; the cache machinery
lives entirely in `arkiv-node`.

---

## 2. Tagging State<DB> with a session id

The cache's correctness condition is simple: **a precompile call
running against `State<DB>` instance `X` must only see the cache
that was created for `X`.** Sharing across instances would expose
committed-to-`X` writes to a different pass that might never reach
`finish`, or hide them from a pass that's about to.

There is no API in revm that lets a precompile ask "which
`State<DB>` am I running against?". Each call sees only a
`PrecompileInput` that carries an `EvmInternals` view of *some*
`State<DB>` without identifying it. The bridge is a per-`State<DB>`
token written to a known storage slot through revm's journal:

1. The `BlockExecutor` wrapper mints `SessionId = B256::random()` at
   `apply_pre_execution_changes` and inserts an empty `CacheStore`
   into the shared session map under that id.
2. It invokes the Arkiv precompile via
   `evm.transact_system_call(ARKIV_SESSION_CALLER, ARKIV_ADDRESS,
   SESSION_SET || sid)`. The precompile sees
   `input.caller == ARKIV_SESSION_CALLER` (the standard EIP-4788
   system address, `0xff...fe`) and the reserved `SESSION_SET`
   selector, branches into its system-only dispatch, and SSTOREs slot
   0 of `ARKIV_ADDRESS` to the session id.
3. The SSTORE goes through revm's journal, so the value is bound to
   this `State<DB>` instance. Every subsequent SLOAD on the same
   instance sees the id; instances belonging to other passes of the
   same `block_number` see zero or their own id.
4. On every Arkiv precompile call,
   [`derive_session`](../crates/arkiv-node/src/evm/session.rs) SLOADs
   slot 0 and uses the value as the lookup key in the shared session
   map.
5. At `BlockExecutor::finish`, the wrapper invokes the precompile
   again with `SESSION_CLEAR` to SSTORE slot 0 back to zero. The
   merged bundle records no net transition on slot 0.

`ARKIV_ADDRESS` (`0x44…0044`) — the precompile registration target
— doubles as the beacon address. It has no genesis allocation and
no contract code; the precompile is registered there programmatically
by `ArkivOpEvmFactory`. Storing the beacon on its own slot 0 keeps
the mechanism within one well-known address.

**Why random is consensus-safe.** `B256::random()` inside a
block-executor hook is exactly the shape of code that should set off
alarms — anything non-deterministic that touches consensus-critical
state breaks consensus. The mechanism survives that scrutiny only
because **the session id is never committed to `stateRoot`**:

- The session id is a per-node, per-pass routing key into a local
  `HashMap<SessionId, CacheStore>` that lives in process memory. It
  is never transmitted, never logged into consensus state, never
  observable by other nodes. Two nodes executing the same block will
  mint different ids, and that is fine — each one only looks up its
  own cache.
- The only place the id touches the trie is slot 0 of `ARKIV_ADDRESS`.
  The wrapper SSTOREs the id at `apply_pre` and zeroes it at
  `finish`; both writes go through revm's journal, and the net
  transition the merged bundle records is `(0 → 0)` — no change.
  `ARKIV_ADDRESS` has no genesis allocation and carries no state at
  block boundaries, so the account itself contributes nothing to
  `stateRoot` either.
- The cache holds typed values that are eventually flushed as bytes
  through the *same* `set_code` / `tombstone_code` calls the
  no-cache path would have made (§4, §5). The bytes that land in
  `State<DB>` at `finish` are byte-identical to what the no-cache
  path would have produced.

Therefore two nodes' merged bundles for the same canonical block are
byte-identical regardless of which session ids they used during the
block. The cache is consensus-transparent: it speeds up the
in-block hot path without leaving any artefact in the canonical
state.

**Why a system call rather than a direct State<DB> write.** The
wrapper has `&mut State<DB>` at `apply_pre` and `finish` and could
inject the value directly through `state.insert_account_with_storage`,
but that path bypasses revm's journal: the write lands in revm's
account cache but doesn't appear in the per-tx transition log that
the bundle aggregator merges. Going through `transact_system_call`
puts both the SSTORE at `apply_pre` and the zero-write at `finish` on
the standard journal path, so the round-trip-to-no-op invariant
falls out for free.

**Why a reserved selector + caller gate.** The precompile dispatches
on the first four calldata bytes. Three reserved selectors are
recognised only when `input.caller == ARKIV_SESSION_CALLER`:

| Selector         | Bytes        | Purpose                                                                            |
| ---------------- | ------------ | ---------------------------------------------------------------------------------- |
| `SESSION_SET`    | `0xFFFE0001` | Body is a 32-byte session id; SSTORE'd to slot 0 of `ARKIV_ADDRESS`.                |
| `SESSION_CLEAR`  | `0xFFFE0002` | No body; SSTORE'd back to zero.                                                     |
| `SESSION_FLUSH`  | `0xFFFE0003` | No body; drains the cache for the current session and writes it through `set_code`. |

The selectors are chosen outside the keccak256 selector space so they
can't collide with any Solidity function signature. The caller gate
prevents EOAs or contracts from spoofing the system path —
`ARKIV_SESSION_CALLER` is not a real account that a user can call
from.

---

## 3. The CacheStore

The cache holds typed values keyed by account address. It lives at
[`crates/arkiv-node/src/state_adapter/cache_store.rs`](../crates/arkiv-node/src/state_adapter/cache_store.rs).

```rust
pub struct CacheStore {
    pub block: HashMap<Address, Cached>,
    pub block_dirty: HashSet<Address>,
    pub tx: HashMap<Address, Cached>,
    pub tx_dirty: HashSet<Address>,
}

pub enum Cached {
    Entity(Entity),
    Bitmap(Bitmap),
    Tree(IndexTree),
    Tombstone,
}
```

`Cached::Tombstone` is the cached form of "this account holds empty
code" — at flush time it maps to `tombstone_code(addr)`. It covers
both deleted entities and emptied index trees (both encode the same
way on the trie). Pair-bitmap accounts never tombstone — an empty
`Bitmap` serialises as normal bytes.

### Two layers

The cache is two-layered because Arkiv's write semantics require
atomic per-tx commit and rollback on top of revm's already-atomic
state machine. revm rolls back its journaled state changes when a
precompile call reverts; the cache must roll back its in-memory
mirror at the same point, even though those mirrored writes never
went through the journal in the first place.

| Layer       | What it holds                                                                 | When it commits / rolls back                                                                       |
| ----------- | ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| **Per-block** (`block`, `block_dirty`) | Typed values for every address the precompile has touched since the BlockExecutor entered the block. Both clean read-throughs and committed writes live here. `block_dirty` is the subset that diverges from `State<DB>` and needs flushing. | Drained at `BlockExecutor::finish` (§4). Never rolled back — reorgs drop the entire `State<DB>` and its cache along with it. |
| **Per-tx** (`tx`, `tx_dirty`)          | The in-flight precompile call's staged writes (and only writes — clean reads stay in the block layer). | `commit_tx()` folds `tx` into `block` and promotes `tx_dirty` into `block_dirty`. `rollback_tx()` drops `tx` and `tx_dirty` without touching the block layer. |

Lookups consult `tx` first, then fall through to `block`:

```rust
pub fn get(&self, addr: &Address) -> Option<&Cached> {
    self.tx.get(addr).or_else(|| self.block.get(addr))
}
```

Writes go into the tx layer (`stage`); read-throughs go into the
block layer (`insert_clean`), no-op if either layer already holds
the address (staged writes shadow read-throughs):

```rust
pub fn stage(&mut self, addr: Address, value: Cached) {
    self.tx.insert(addr, value);
    self.tx_dirty.insert(addr);
}

pub fn insert_clean(&mut self, addr: Address, value: Cached) {
    if self.tx.contains_key(&addr) || self.block.contains_key(&addr) {
        return;
    }
    self.block.insert(addr, value);
}
```

`commit_tx` drains `tx_dirty` and folds each entry into `block`,
promoting it into `block_dirty`. `rollback_tx` clears `tx` and
`tx_dirty`. `drain_block_dirty` returns the `(Address, Cached)` pairs
that need flushing.

---

## 4. The BlockExecutor wrapper

`ArkivOpBlockExecutorFactory` wraps the underlying
`OpBlockExecutorFactory` and produces `ArkivOpBlockExecutor`
instances. Both live at
[`crates/arkiv-node/src/evm/block_executor.rs`](../crates/arkiv-node/src/evm/block_executor.rs).
The wrapper is plugged in through `ConfigureEvm::block_executor_factory()`,
so canonical execution, engine validation, reorg replay, and payload
building all run through it. Speculative lanes bypass it — see §7.

**`apply_pre_execution_changes`:**

1. Call `inner.apply_pre_execution_changes()` first. That lets revm's
   system contracts (EIP-2935 block-hash history, EIP-4788 beacon
   root) initialise before we add our beacon.
2. Mint `sid = B256::random()`. Stash it on `self.session_id` for
   `finish`.
3. Insert an empty `CacheStore` into the session map under `sid`.
4. Issue
   `evm.transact_system_call(ARKIV_SESSION_CALLER, ARKIV_ADDRESS,
   SESSION_SET || sid)`. The precompile handles it as described in §2
   and journals the SSTORE.

Across precompile calls within the block, the wrapper does nothing.
Each precompile call independently SLOADs slot 0, finds its
`CacheStore`, mutates it, and puts it back. Per-tx commit/rollback
happens at the precompile boundary (§5).

**`finish`:**

1. `evm.transact_system_call(... SESSION_FLUSH)`. The precompile
   takes the `CacheStore` out of the session map and writes its
   `block_dirty` entries to `State<DB>` via byte-level `set_code` /
   `tombstone_code` calls (see §5).
2. `evm.transact_system_call(... SESSION_CLEAR)`. SSTORE slot 0 back
   to zero so the merged bundle records no net change on slot 0 of
   `ARKIV_ADDRESS`.
3. Defensively `sessions.lock().remove(&sid)` — a no-op on the
   canonical path (FLUSH already took it out) but a safety net if a
   future code path delivers `finish` without delivering FLUSH.
4. Delegate to `inner.finish()`.

**Why the flush goes through the precompile.** The cache holds typed
values keyed by address; to flush, each value needs to be serialised
to bytes and written via `set_code`. The byte-level writers
(`ReadWriteStateAdapter::set_code` / `tombstone_code`) require an
`EvmInternals` handle, which the precompile has but the wrapper does
not. Routing the flush through a single `transact_system_call` lets
the precompile use the same `EvmInternals` it uses on every regular
call, and keeps the flush's transition records on the same journal
path as the writes a no-cache run would have produced one at a time.

---

## 5. CachedReadWriteStateAdapter

The `StateAdapter` impl that routes reads and writes through the
cache. Lives at
[`crates/arkiv-node/src/state_adapter/cached_read_write_state_adapter.rs`](../crates/arkiv-node/src/state_adapter/cached_read_write_state_adapter.rs).

```rust
pub struct CachedReadWriteStateAdapter<'a, 'b, 'c> {
    inner: ReadWriteStateAdapter<'a, 'b>,
    cache: Option<&'c mut CacheStore>,
}
```

`inner` is a bare `ReadWriteStateAdapter` wrapping the same
`EvmInternals` — it provides the byte-level read / write path.
`cache` is the borrowed `&mut CacheStore` the precompile checked
out of the session map. With `cache: None` (speculative lanes,
§7) every method collapses to inner passthrough.

### Reads (`get_entity` / `get_pair_bitmap` / `get_index_tree`)

1. If the cache has the address, return the typed value:
   `Cached::Entity(e)` → `Some(e.clone())`; `Cached::Bitmap(b)` →
   `b.clone()`; `Cached::Tree(t)` → `t.clone()`. `Cached::Tombstone`
   returns `None` (entity) or `IndexTree::new()` (tree).
2. On a cold miss, call the matching `inner.get_*` (which does the
   byte-level read and decode), then
   `cache.insert_clean(addr, ...)` so the next read short-circuits.

For pair bitmaps and index trees the address is derived inside the
adapter from `pair_address(k, v)` / `index_address(k)`.

### Writes (`set_entity` / `set_pair_bitmap` / `set_index_tree`)

`cache.stage(addr, Cached::Entity(entity))` (or `Bitmap` / `Tree`).
The byte-level write is deferred to the flush at `finish`.

### Tombstones (`tombstone_entity` / `tombstone_index_tree`)

`cache.stage(addr, Cached::Tombstone)`.

### System-account slots

`get_entity_count` / `set_entity_count` / `get_id_to_addr` /
`set_id_to_addr` / `get_addr_to_id` / `set_addr_to_id` /
`get_nonce` / `set_nonce` pass through to `inner` unchanged. The
system account is small and slot-keyed; caching wouldn't pay off
(§6).

### Flush

```rust
impl CachedReadWriteStateAdapter<'_, '_, '_> {
    pub fn flush(&mut self) -> Result<()> {
        let Some(cache) = self.cache.as_deref_mut() else { return Ok(()); };
        for (addr, value) in cache.drain_block_dirty() {
            match value {
                Cached::Entity(e) => self.inner.set_code(&addr, entity_to_code(&e))?,
                Cached::Bitmap(b) => self.inner.set_code(&addr, b.to_bytes())?,
                Cached::Tree(t)   => self.inner.set_code(&addr, t.to_bytes())?,
                Cached::Tombstone => self.inner.tombstone_code(&addr)?,
            }
        }
        Ok(())
    }
}
```

Called from the precompile's `SESSION_FLUSH` handler. Drains the
per-block dirty set and emits the bytes through `inner.set_code` /
`inner.tombstone_code` — the same calls the no-cache write path
would have made, coalesced to once per `(addr, block)`.

### Per-tx commit / rollback at the precompile boundary

`dispatch_execute` is the only place that knows whether the batch
succeeded or reverted. After the ops loop:

```rust
if let Some(cache) = cache.as_mut() {
    match &batch_result {
        Ok(())  => cache.commit_tx(),
        Err(_)  => cache.rollback_tx(),
    }
}
```

revm rolls back its journaled state on precompile error / revert;
`rollback_tx` rolls back the cache's in-memory mirror at the same
point. The two stay in lockstep.

---

## 6. What is and isn't cached

**Cached** (typed values, deferred byte-level write):

- Entity accounts.
- Tier-1 pair-bitmap accounts.
- Tier-2 index-tree accounts.
- Tombstones for the above (deleted entities; emptied index trees).

**Not cached** (direct passthrough to `inner` / `EvmInternals`):

- The system account at `0x44…0046`. Slot-keyed, small, and the
  encoding is a single SSTORE per slot — there's no
  decode-mutate-encode round-trip to amortise. The
  `CachedReadWriteStateAdapter` system-slot methods are thin
  passthroughs.
- Slot 0 of `ARKIV_ADDRESS`. The session beacon itself; the
  precompile reads it via `derive_session` and the BlockExecutor
  wrapper writes it at `apply_pre` / `finish`. Routing it through
  the cache would be circular.

---

## 7. Speculative lanes

Any execution path that doesn't go through `ArkivOpBlockExecutor` —
gas estimation, pending-state `eth_call`, tracing-only simulation —
gets a fresh `State<DB>` whose slot 0 of `ARKIV_ADDRESS` reads zero.
`derive_session` returns `SessionKind::Speculative`, the precompile
constructs a `CachedReadWriteStateAdapter` with `cache: None`, and
every method collapses to inner passthrough. Behaviour is
byte-identical to a bare `ReadWriteStateAdapter`.

This is the right thing for speculative lanes: they live in
throwaway `State<DB>` instances that are dropped when the lane
returns, so caching would mean either paying for cache lifecycle on
every estimation call or keeping cache state alive for transitions
that'll never commit. Speculative lanes pay the per-op
encode/decode cost; they're rare on the hot path (single tx, not
batches).

---

## 8. Invariants preserved

| Invariant | Why it holds |
| --- | --- |
| **Consensus determinism**           | Bytes flushed at `finish` come from the same `entity_to_code` / `Bitmap::to_bytes` / `IndexTree::to_bytes` encoders the no-cache path uses. Round-trip stability of those encoders is already a load-bearing trie invariant (`codeHash` agreement across nodes). |
| **State root unaffected by beacon** | Slot 0 of `ARKIV_ADDRESS` round-trips through `0 → sid → 0` within a single block. The merged bundle records no net transition, so `stateRoot` is unaffected. |
| **Per-tx atomicity**                | `commit_tx` / `rollback_tx` mirror revm's per-tx state revert at the precompile boundary. If the precompile reverts, revm rolls back its journaled writes and the cache drops its tx layer in lockstep. |
| **Reorg safety**                    | The cache lives entirely within one `BlockExecutor` pass. Reorged-out passes simply drop their `CacheStore` along with their `State<DB>`; reth's standard machinery reverts the trie. No cache reconciliation code required. |
| **Historical reads untouched**      | The cache is on the write path only. `ReadOnlyStateAdapter` (the query path) reads directly from a `StateProvider` against committed trie state, untouched by any of this machinery. |

---

## 9. Known limitations

- **Outer-lock contention.** The session map is
  `Arc<Mutex<HashMap<SessionId, CacheStore>>>`. Per-call work runs
  uncontended on a locally-owned `CacheStore` (checked out under a
  brief lock, returned under a brief lock), so contention is bounded
  by `HashMap::insert` + `remove`. Heavy concurrent payload-build +
  validator load is the worst case; swap to `DashMap` if profiling
  shows it.
- **Cache size bounds.** A batch that touches `N` distinct pair
  accounts builds an `N`-entry cache. Real-world batches are
  gas-bounded (precompile gas per pair), so the cache should be
  naturally bounded — but no explicit eviction. A batch that fills
  the cache flushes it at `finish` and the next block starts fresh.
- **Flashblocks rebuild path.** op-reth's flashblocks worker can
  skip `apply_pre_execution_changes` on rebuilds, which would skip
  our session-write too. Suffix-tx precompile calls would then see
  slot 0 = zero and fall into the speculative branch even though
  they're being included in a canonical block — correct but missing
  the cache speedup. Out of scope until flashblocks is on the
  deployment path; fix is to hook the wrapper off whatever replaces
  `apply_pre` in the rebuild flow.
- **Multiple `finish` calls per pass.** If reth's pipeline ever
  exercises reorg replay heavily, two `finish` calls per
  `block_number` is steady state, not an edge case. The flush is
  idempotent (drained `block_dirty` is empty on the second call), so
  it's correct — but it's worth profiling that the redundant system
  calls don't double the flush cost.
- **Benchmarks pending.** End-to-end benchmarks comparing
  cache-on vs cache-off on batch-heavy and tx-heavy workloads
  haven't been measured yet. The `simulate` traffic mode in
  [`../crates/arkiv-cli`](../crates/arkiv-cli) is the closest
  available driver.
