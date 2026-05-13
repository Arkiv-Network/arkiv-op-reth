# Arkiv StateDB Design (op-reth)

## Contents

- [Abstract](#abstract)
- [1. Architecture](#1-architecture)
  - [Overview](#overview)
  - [Reth Integration](#reth-integration)
  - [EntityRegistry Smart Contract](#entityregistry-smart-contract)
  - [Arkiv Precompile](#arkiv-precompile)
- [2. State Model](#2-state-model)
  - [Entity Accounts](#entity-accounts)
    - [Address Derivation](#address-derivation)
    - [Account Structure](#account-structure)
    - [codeHash and RLP Storage](#codehash-and-rlp-storage)
    - [EntityRLP](#entityrlp)
  - [System Account](#system-account)
  - [Pair Accounts (Content-Addressed Bitmaps)](#pair-accounts-content-addressed-bitmaps)
  - [ArkivPairs MDBX Table](#arkivpairs-mdbx-table)
  - [Why Numerical IDs](#why-numerical-ids)
- [3. Lifecycle](#3-lifecycle)
  - [Create](#create)
  - [Update](#update)
  - [Delete](#delete)
  - [Entity Expiration](#entity-expiration)
  - [Extend](#extend)
  - [Transfer](#transfer)
- [4. Query Execution](#4-query-execution)
  - [Equality and Inclusion Queries](#equality-and-inclusion-queries)
  - [Range Queries](#range-queries)
  - [Glob / Prefix Queries](#glob--prefix-queries)
  - [Historical Queries](#historical-queries)
  - [Query Completeness Proofs](#query-completeness-proofs)
- [5. Gas Model](#5-gas-model)
- [6. Reorg Handling](#6-reorg-handling)
- [7. Verification](#7-verification)
- [8. Summary](#8-summary)
  - [Storage Layout](#storage-layout)
  - [Properties](#properties)
  - [Compatibility with the Optimism Verification Pipeline](#compatibility-with-the-optimism-verification-pipeline)
- [9. Open Questions](#9-open-questions)

---

## Abstract

This document describes the Arkiv storage design for op-reth. Almost all of the state used to serve entity reads and SQL-like annotation queries lives in op-reth's world state trie, committed in the L3 `stateRoot`. There is no separate EntityDB process, no JSON-RPC bridge between subsystems, no private `arkiv_stateRoot`, no anchoring submission transaction, and no ExEx.

Entities and the annotation index are expressed as Ethereum accounts. Each entity is a dedicated account whose payload, content type, full key, and annotation set are RLP-encoded and held in `codeHash`. Each `(annotKey, annotVal)` pair is itself a dedicated account whose code is the roaring64 bitmap of entity IDs matching that pair; the account's `codeHash` is the keccak hash of those bytes by construction, so every bitmap is **content-addressed in the trie**. A singleton system account holds the global entity counter and the trie-committed ID ↔ address map.

The single piece of state that lives **outside** the trie is the `ArkivPairs` MDBX table — an append-only existence index of every `(annotKey, annotVal)` pair ever seen. It exists for one purpose: to make range and glob queries efficient via prefix scans. It is the explicit exception to the rule that the precompile never touches the underlying KV store directly.

The contract entry point — `EntityRegistry` — holds per-entity `(owner, expiresAt)` tuples and is responsible for **ownership and expiration validation**. It mints entity keys, enforces who can mutate what, collects fees, dispatches to the precompile, and emits per-op logs. The **Arkiv precompile**, called by the contract from inside EVM execution, is responsible for content validation (payload size, attribute caps, attribute formats) and for the state mutations themselves: writing entity-account code, writing pair-account code, bumping the system-account counters, and appending to the `ArkivPairs` table.

The precompile writes to revm's journaled state for everything except `ArkivPairs`. Op-reth's standard tables and standard reorg machinery handle everything that participates in the journal without any Arkiv-specific extension. The `ArkivPairs` exception is tolerated because the table is append-only and idempotent, the entries it holds are not consensus-critical, and the cost of speculative-execution pollution is bounded (at worst, an entry pointing to a bitmap that is empty at the current head).

**What this design provides:**
- Verifiable entity payloads via standard `eth_getProof` against the L3 `stateRoot`. One-level proof; no anchor proof.
- Verifiable query results of every shape — equality, range, glob. Every bitmap is content-addressed in the trie (`codeHash = keccak256(bitmap_bytes)`) and every ID → entity-address mapping is a trie-committed system-account slot. A client can verify each bitmap individually against its `codeHash`, re-execute the query logic locally (AND / OR / range / glob filter), and check that the server's result matches the locally-computed answer. The same shape as the equality proof, extended over multiple terms.
- A natural "raw bitmap" RPC mode: the server exposes bitmap reads + `eth_getProof` directly, and clients run the query entirely locally — fetch matching bitmaps, AND/OR them together, resolve IDs through the system-account map, then fetch entity RLPs in a second step.
- Historical reads for every entity at every retained block, and historical queries of every shape against any retained `stateRoot`.
- Full Optimism fault-proof coverage of all consensus-critical Arkiv state.
- Single-process deployment with one custom MDBX table.

`ArkivPairs` is a server-side speedup for range/glob enumeration; it is **not** part of the verification path. Clients verify bitmap *contents*, not the pair-set enumeration the server used to find them.

---

## 1. Architecture

### Overview

Two components inside `arkiv-op-reth` plus one out-of-trie MDBX table:

1. The **`EntityRegistry` smart contract** — the user-facing entry point on the L3. Holds `(owner, expiresAt)` per entity; validates ownership and liveness; mints entity keys; collects fees; dispatches to the precompile; emits per-op logs. The single source of truth for ownership and expiration.
2. The **Arkiv precompile** — invoked by `EntityRegistry` from inside EVM execution. Validates content (payload size, attribute count, attribute formats). Mutates state: entity account code, pair account code (bitmaps), system account counters and ID maps, and the `ArkivPairs` MDBX table on first sight of a new pair.
3. The **`ArkivPairs` MDBX table** — append-only existence index of `(annotKey, annotVal)` pairs, written by the precompile. The one piece of Arkiv state outside the trie. Exists to support prefix-scannable range and glob queries that cannot be served from the trie alone.

Every state-dependent mutation that affects consensus — entity account writes, pair account writes (bitmaps), system account writes — flows through revm's journaled state and is committed in the L3 `stateRoot`. `ArkivPairs` is the only direct MDBX touch from the precompile and is by design not consensus-critical.

### Reth Integration

A single integration point on op-reth's standard extension surface: an Arkiv precompile registered into `PrecompilesMap` via a custom `EvmFactory` wrapping `OpEvmFactory<OpTx>`. The custom factory inserts the Arkiv precompile in both `create_evm` and `create_evm_with_inspector` so simulation, tracing, payload building, validation, and canonical execution all see the same set of precompiles. The precompile also opens a write handle to the `ArkivPairs` MDBX table (defined in a small `arkiv-db` crate registered alongside op-reth's built-in tables).

No `BlockExecutor` wrapper. No system call. No ExEx. No `arkiv_stateRoot` slot.

### EntityRegistry Smart Contract

`EntityRegistry` owns ownership and lifetime — the validation gate that protects entity state from unauthorised mutation.

```solidity
struct EntityRecord {
    address owner;
    uint64  expiresAt;
}

mapping(address sender => uint64)            public createNonce;
mapping(bytes32 entityKey => EntityRecord)   public entities;
```

Op set: `create | update | delete | extend | transfer | expire`. The contract validates each op against the `entities` mapping before dispatching to the precompile:

| Op | Contract validation | Contract state change |
|---|---|---|
| `create` | (sender pays fee; no per-entity check) | mint `entityKey`; insert `(owner, expiresAt)` |
| `update` | `msg.sender == owner`; `block.number ≤ expiresAt` | none |
| `extend` | `msg.sender == owner`; `newExpiresAt > block.number` | update `expiresAt` |
| `transfer` | `msg.sender == owner`; `block.number ≤ expiresAt` | update `owner` |
| `delete` | `msg.sender == owner` | remove entry |
| `expire` (anyone may call) | `block.number > expiresAt` | remove entry |

`entityKey` is minted from a sender-scoped nonce:
```
entityKey = keccak256(chainId || registryAddress || msg.sender || createNonce[msg.sender])
```
The derivation is EVM-computable so clients holding the sender's current `createNonce` can predict the key before submitting the tx, and the contract can emit `entityKey` in its log.

After validating and updating its own state, the contract calls the precompile with the decoded op batch + `msg.sender` + the minted `entityKey`s + (for ops that depend on it) the new and/or old `owner` and `expiresAt` values. If the precompile reverts (content violation, OOG), the whole EVM tx reverts and the contract's storage changes roll back atomically.

The contract emits one `EntityOperation` event per op for indexers and SDKs. The log carries only op-identity fields (`entityKey`, sender, op type, block number) — content (payload, annotations) is not in the log because the source of truth for content is the entity account in the trie.

### Arkiv Precompile

The precompile is the per-transaction synchronous handler that performs content validation and state mutation. It owns:

- **Content validation.** Payload size cap, attribute count cap, attribute value formats, ban on `0x00` bytes in annotation keys and values. Pure functions of op shape; not state-dependent.
- **Gas accounting.** Pure functions of op shape (§5). Per-op cost is charged via standard revm precompile gas accounting (`PrecompileOutput::new` for success, `revert` for content violations, `halt(OutOfGas)` for budget exhaustion).
- **Trie state mutation.** Every consensus-critical write goes through revm's journaled state via `EvmInternals`: `CreateAccount`, `SetNonce`, `SetCode`, `SetState` on entity accounts, pair accounts, and the system account.
- **`ArkivPairs` MDBX write** on first sight of a new pair. This is the one direct MDBX touch and is not journaled by revm.

The precompile does **not** validate ownership or liveness — `EntityRegistry` has already done that before calling. The precompile trusts the inputs the contract passes (owner, expiresAt, prior owner/expiresAt where relevant, the minted entityKey on create).

The precompile is registered through op-reth's `EvmFactory` extension. It implements alloy-evm's `Precompile` trait, declares `supports_caching = false`, and refuses non-direct calls — STATICCALL, DELEGATECALL, value-bearing calls, and calls from any address other than `EntityRegistry` all return `PrecompileError::Fatal`.

---

## 2. State Model

All Arkiv state in the trie lives in three kinds of Ethereum accounts: entity accounts (one per entity), pair accounts (one per `(annotKey, annotVal)` ever seen — these hold the bitmaps), and the singleton system account. The `EntityRegistry` contract holds its own per-entity `(owner, expiresAt)` mapping plus the sender-scoped `createNonce`. Outside the trie, the `ArkivPairs` MDBX table holds the append-only pair-existence index.

### Entity Accounts

#### Address Derivation

```
entityKey      = keccak256(chainId || registryAddress || msg.sender || createNonce)
entity_address = entityKey[:20]
```

`createNonce` is held in `EntityRegistry`, incremented once per `Create` op. The payload is intentionally excluded from the derivation: the address is a pure identity anchor. Content commitment is handled by `codeHash`.

#### Account Structure

```
Entity Account  (address = entityKey[:20])
  nonce    = 1                               // prevents EIP-161 empty-account deletion on tombstoning
  balance  = 0
  codeHash = keccak256(0xFE || RLP(entity))  // commits to full entity content in the trie
  code     = 0xFE || RLP(entity)             // stored by op-reth in its Bytecodes table, keyed by codeHash

  storage slots: none
```

Entity accounts have **zero storage slots**. The `entity_id` mapping that was previously held on the entity account lives on the system account instead (see below). This makes entity accounts as cheap as possible to create and to delete: a single `SetCode` call is the entirety of the entity's per-account trie footprint.

#### codeHash and RLP Storage

`codeHash` is set to `keccak256(0xFE || RLP(entity))`. Op-reth stores the corresponding bytes in its `Bytecodes` table keyed by `codeHash`, exactly as it does for contract bytecode — no special handling is needed. `eth_getCode(entity_address)` retrieves the full RLP; `eth_getProof(entity_address)` includes `codeHash` in the account node, verifiable against the L3 `stateRoot`.

The `0xFE` prefix ensures that any EVM `CALL` to an entity address executes `INVALID` and reverts immediately. The RLP bytes are never interpreted as bytecode.

On every `Update`, the entity is re-encoded, the prefix is prepended, `keccak256` is recomputed, and `SetCode` is called with the new bytes. Old code bytes remain in op-reth's `Bytecodes` table indefinitely (subject to op-reth's pruning policy), keyed by their hash — historical entity payloads stay retrievable.

#### EntityRLP

```rust
struct EntityRlp {
    payload:             Vec<u8>,
    creator:             Address,
    created_at_block:    u64,
    content_type:        String,
    key:                 B256,                  // full 32-byte entityKey
    string_annotations:  Vec<StringAnnotRlp>,
    numeric_annotations: Vec<NumericAnnotRlp>,
}
```

The RLP holds everything immutable about the entity (creator, createdAtBlock, content_type, key, annotations) plus the payload. `owner` and `expires_at` are **not** in the RLP — they live in `EntityRegistry`'s storage so the contract can validate against them without decoding RLP from inside Solidity.

`creator` and `created_at_block` stay in the RLP because they are immutable — set once at `Create`, never updated — so there is no sync cost. They are present to support direct lookup of "who created entity X" and "when was entity X created" without consulting logs; the corresponding built-in annotations `$creator` and `$createdAtBlock` provide the reverse direction via bitmaps.

The full 32-byte `key` is in the RLP so that callers with only the 20-byte address can recover the complete key — the last 12 bytes are not stored anywhere else (the system account ID map gives address ↔ uint64 ID, not address → full key).

### System Account

A singleton account at a fixed address holding chain-global counters and the trie-committed ID ↔ address map:

```
System Account  (address = keccak256("arkiv.system")[:20])
  nonce    = 1                                                       // prevents EIP-161 pruning
  storage slots:
    slot[keccak256("entity_count")]                  →  uint64       // next entity ID
    slot[keccak256("id_to_addr", uint64_id)]         →  address      // ID → entity_address (live entities only)
    slot[keccak256("addr_to_id", entity_address)]    →  uint64       // entity_address → ID (live entities only)
```

The `entity_count` slot is the canonical source for ID assignment. Every node executing the same block sees the same value and assigns IDs identically.

The `id_to_addr` and `addr_to_id` slots give both directions of the ID ↔ address map, both trie-committed. Both are written at `Create` and both are cleared at `Delete` / `Expire`. The address-to-ID direction is needed during `Delete`/`Expire`/`Update` to look up the entity's ID without decoding the RLP; the ID-to-address direction is the query-time resolver for bitmap hits.

### Pair Accounts (Content-Addressed Bitmaps)

One account per `(annotKey, annotVal)` pair ever seen. Created lazily the first time the pair appears in an op. The bitmap of entity IDs matching this pair is stored as the account's code; **the bitmap is content-addressed in the trie because `codeHash = keccak256(bitmap_bytes)` by construction.**

```
Pair Account  (address = keccak256("arkiv.pair" || key_text || 0x00 || val_text)[:20])
  nonce    = 1
  codeHash = keccak256(roaring64_bitmap_bytes)                       // CONTENT HASH OF THE BITMAP
  code     = roaring64_bitmap_bytes                                  // the bitmap, content-addressed

  storage slots: none
```

On bitmap update, `SetCode` is called with the new bytes; `codeHash` updates automatically to the keccak hash of the new content. Old bitmap bytes remain in op-reth's `Bytecodes` table indefinitely, keyed by their old hash — historical bitmap versions stay retrievable via `eth_getCode(pair_address, blockN)` against any retained block.

The 0xFE-prefix trick used for entity accounts is **not** applied here. A `CALL` to a pair account is not something the design needs to defend against (the pair account is never the target of a contract call in this design), and applying the prefix would mean `codeHash` is not literally the bitmap content hash, which would defeat the content-addressing property.

The 20-byte pair address is derivable directly from `(annotKey, annotVal)`, so equality queries can locate the bitmap without consulting any index. The pair account holds no storage — the `(annotKey, annotVal)` pair text is recoverable from the `ArkivPairs` MDBX table for queries that need to iterate.

### ArkivPairs MDBX Table

A small custom MDBX table registered alongside op-reth's built-ins:

```
ArkivPairs   annot_key || 0x00 || annot_val   →   0x01      (append-only existence flag)
```

The `0x00` separator between `annot_key` and `annot_val` prevents prefix collisions in scans. Keys and values must not contain `0x00`; the precompile enforces this when validating an op.

The table is **append-only and idempotent**: the precompile writes an entry the first time it sees a `(k, v)` pair (during `Create` or `Update`); the value is always `0x01`; entries are never removed. Existing entries are never rewritten.

**This is the one exception** to the rule that the precompile only writes to revm's journaled state. The write is a direct MDBX put, not journaled by revm. Two consequences follow:

1. **Speculative-execution pollution.** Reth runs the precompile across multiple execution paths per transaction (gas estimation, payload building, validation, tracing, canonical execution). If a non-canonical path happens to write a `(k, v)` entry that the canonical execution never produces, the entry stays in `ArkivPairs` indefinitely. The cost is bounded — entries are 1 byte and idempotent — and harmless: the pair account that entry points to will have an empty bitmap at the canonical head, so range and glob queries return correct results.
2. **No reorg revert.** `ArkivPairs` entries do not roll back when blocks revert. After a reorg, the table may contain entries for pairs whose pair accounts in the trie now have empty bitmaps (because the trie reverted). Queries still return correct results because the bitmap content — which is consensus-critical — reverts with the trie.

The asymmetry is deliberate: `ArkivPairs` is an *index* for query efficiency, not a *commitment*. The actual bitmaps and the ID maps that range and glob queries resolve through are all in the trie and revert correctly with the chain.

### Why Numerical IDs

Bitmaps are `roaring64` — compressed bitsets over 64-bit unsigned integers. Ethereum addresses (20 bytes) cannot be stored directly in a roaring bitmap; each entity is therefore assigned a compact `uint64` ID at `Create` time. Both directions of the ID ↔ address mapping live on the system account and are trie-committed (see [System Account](#system-account)).

---

## 3. Lifecycle

`EntityRegistry` validates ownership / liveness from its own storage and updates that storage before calling the precompile. The precompile then performs content validation and state mutation. Every consensus-critical write goes through revm's journaled state; the one direct MDBX write (to `ArkivPairs`) is described where it appears.

### Create

**Contract:**
1. Read and increment `createNonce[msg.sender]`; derive `entityKey`.
2. Insert `entities[entityKey] = (msg.sender_or_specified_owner, expiresAt)`.

**Precompile:**
1. Read and increment `entity_count` on the system account; the new value is `entity_id`.
2. Write the system-account ID maps: `slot[keccak256("id_to_addr", entity_id)] = entity_address`; `slot[keccak256("addr_to_id", entity_address)] = entity_id`.
3. `CreateAccount(entity_address)`; `SetNonce(entity_address, 1)`.
4. For each annotation `(k, v)` — including built-ins `$all`, `$creator`, `$createdAtBlock`, `$owner`, `$key`, `$expiration`, `$contentType` (whose values are derived from the contract-passed arguments and the entity itself):
   - Derive `pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]`.
   - **If the pair account has no code (first sight of this pair):** `CreateAccount(pair_addr)`; `SetNonce(pair_addr, 1)`; write `ArkivPairs[k || 0x00 || v] = 0x01` (direct MDBX put — the exception).
   - Read `pair_addr.code` (treat as empty bitmap if absent). Deserialize, add `entity_id`, re-serialize. `SetCode(pair_addr, new_bytes)`.
5. Encode the entity RLP. `SetCode(entity_address, 0xFE || RLP)`.

### Update

**Contract:** validates `msg.sender == owner` and `block.number ≤ expiresAt`. No state change.

**Precompile:**
1. Read the entity's current `entity_id` from `system.slot[keccak256("addr_to_id", entity_address)]`.
2. Decode the current annotation set from `code(entity_address)`.
3. Diff old vs. new annotations.
4. For each annotation **removed**: derive `pair_addr`; read bitmap; remove `entity_id`; `SetCode(pair_addr, new_bytes)`. (The pair account stays in the trie; its bitmap may become empty.)
5. For each annotation **added**: derive `pair_addr`; if no code yet, create the pair account + write `ArkivPairs` (as in Create step 4); read bitmap, add `entity_id`, `SetCode`.
6. Unchanged annotations require no writes.
7. Re-encode the entity RLP. `SetCode(entity_address, new_bytes)`.

### Delete

**Contract:** validates `msg.sender == owner`. Removes `entities[entityKey]` from its mapping.

**Precompile:**
1. Read `entity_id` from `system.slot[keccak256("addr_to_id", entity_address)]`.
2. Decode the entity's annotations from `code(entity_address)`.
3. For each annotation (plus `$owner` and `$expiration`, whose values the contract passes in): derive `pair_addr`; read bitmap; remove `entity_id`; `SetCode` back.
4. Clear both system-account ID slots: `SetState(system, keccak256("id_to_addr", entity_id), 0)`; `SetState(system, keccak256("addr_to_id", entity_address), 0)`.
5. `SetCode(entity_address, nil)`. **Keep `nonce` at 1.**

> **Why nonce stays at 1.** If nonce were zeroed, the account would become EIP-161-empty (nonce=0, balance=0, no code). `StateDB.Finalise` would mark it for destruction and post-Cancun `handleDestruction` returns `"unexpected storage wiping"` when a prior non-empty storage root exists. Keeping nonce at 1 prevents EIP-161 from treating the account as empty; the account remains as a tombstone in the trie.

### Entity Expiration

Anyone may call `EntityRegistry.expire(entityKey)` once `block.number > expiresAt`. The contract gates on `expiresAt < block.number` against its own `entities` mapping, removes the entry, and dispatches to the precompile, which executes the same state changes as `Delete`. There is no out-of-band housekeeping path; expiration is contract-driven so it lives on the canonical execution path along with every other state-mutating op.

### Extend

**Contract:** validates `msg.sender == owner` and `newExpiresAt > block.number`. Updates `entities[entityKey].expiresAt = newExpiresAt`. Passes old and new `expiresAt` to the precompile.

**Precompile:** removes `entity_id` from the old `$expiration` pair account's bitmap; adds it to the new `$expiration` pair account's bitmap (creating the pair account + writing `ArkivPairs` if it's a first sight). No change to the entity RLP.

### Transfer

**Contract:** validates `msg.sender == owner` and `block.number ≤ expiresAt`. Updates `entities[entityKey].owner = newOwner`. Passes old and new owner to the precompile.

**Precompile:** removes `entity_id` from the old `$owner` pair account's bitmap; adds it to the new `$owner` pair account's bitmap (creating + writing `ArkivPairs` if it's a first sight). No change to the entity RLP.

---

## 4. Query Execution

All queries are evaluated by reading the trie, with prefix scans on `ArkivPairs` driving range and glob enumeration. Every read is a standard `eth_call` / `eth_getStorageAt` / `eth_getCode` against op-reth, plus (for range/glob) an MDBX iterator over `ArkivPairs`.

### Equality and Inclusion Queries

```
Query: contentType = "image/png" AND status = "approved"

1. Derive pair_addr_1 = keccak256("arkiv.pair" || "contentType" || 0x00 || "image/png")[:20].
2. Derive pair_addr_2 = keccak256("arkiv.pair" || "status"      || 0x00 || "approved")[:20].
3. Read pair_addr_1.code → bitmap_1; pair_addr_2.code → bitmap_2.
4. Deserialize both bitmaps; compute intersection in memory.
5. For each uint64_id in the result: read system.slot[keccak256("id_to_addr", id)] → entity_address.
6. Optionally fetch full entity: eth_getCode(entity_address).
```

Equality queries do not touch `ArkivPairs` — the pair address is derived directly from `(k, v)`. An inclusion query (`status IN ("approved", "pending")`) unions the bitmaps for each value before intersecting with other terms.

### Range Queries

Numeric annotation values are stored with fixed-width big-endian encoding so that lexicographic byte order matches numeric order. A range query prefix-scans `ArkivPairs`:

```
Query: score > 10

1. Open an MDBX iterator over ArkivPairs from "score" + 0x00 + encode(11)
                                          to "score" + 0x01.
2. For each key (k, v) in the iterator:
     pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]
     bm        = deserialize(pair_addr.code)
     accumulator |= bm
3. For each uint64_id in accumulator: resolve address via system.slot[keccak256("id_to_addr", id)].
```

Cost is proportional to the number of distinct pairs matching the prefix, not to the number of entities. The MDBX iterator is the cheap part; the per-pair trie read of `pair_addr.code` is the dominant cost.

### Glob / Prefix Queries

Same shape as range queries: prefix-scan `ArkivPairs` on `annot_key + 0x00 + match_prefix`; for each matching `(k, v)`, derive `pair_addr` and read the bitmap; union into the accumulator.

For queries that don't constrain the annotation key, the iterator scans `ArkivPairs` from the start; this is bounded by the total number of distinct pairs the chain has ever produced.

### Historical Queries

Equality queries at a past block work directly: substitute the historical state root, derive `pair_addr`, read `pair_addr.code` at the past block via op-reth's historical state cursor. Op-reth's `Bytecodes` table retains old bitmap bytes keyed by hash, so the lookup is always available within the retention window.

Range and glob queries at a past block use the **same** `ArkivPairs` iterator, then read each matched pair's bitmap from the past state. The iterator may yield pairs whose bitmap at the past block was empty (the pair came into existence after block N, or its bitmap was emptied later — `ArkivPairs` is append-only and doesn't track per-block existence). These pairs contribute empty bitmaps to the accumulator and have no effect on the result. The cost overhead is proportional to the number of pairs first seen between block N and the current head.

### Query Completeness Proofs

Every bitmap is content-addressed in the trie — a pair account's `codeHash` **is** the keccak hash of its bitmap content. Every ID-to-address mapping is a trie-committed system-account slot. From these two primitives, a client can verify any query result by re-running the query logic locally on cryptographically-verified bitmaps.

**Equality on (k, v) at block N.** Derive `pair_addr` locally. Request `eth_getProof(pair_addr, [], blockN)` — the proof binds `codeHash` to the L3 `stateRoot` at block N. Request `eth_getCode(pair_addr, blockN)` for the bitmap bytes. Verify `keccak256(bytes) == codeHash`. Decode the bitmap. For each ID, request `eth_getProof(system_account, [slot[keccak256("id_to_addr", id)]], blockN)` to recover and verify the corresponding entity address. The response is complete iff it equals the decoded set.

**Multi-condition equality.** Repeat for each term; intersect bitmaps locally; one ID-resolution proof per surviving ID.

**Range / glob.** Same shape as equality, applied to N terms. The server returns the result set `R` along with the bitmaps it consulted — one per matching `(k, v)`. For each disclosed pair:
- Derive `pair_addr` locally.
- `eth_getProof(pair_addr, [], blockN)` + `eth_getCode(pair_addr, blockN)` + `keccak256(bytes) == codeHash`.
- Confirm `v` satisfies the predicate (`v > 10`, prefix match, etc.).

Union (or intersect, depending on the query) the verified bitmaps locally to obtain `S_local`. Resolve each ID in `S_local` to an entity address via the system account's `id_to_addr` slot (with proof). Confirm the resulting address set equals `R`.

This binds the server to the exact contents of every bitmap it claims to have consulted, and to the correctness of the query logic applied to those contents. Equivalently, the server can expose a raw bitmap-read RPC and the client can run the entire query client-side: ask for the bitmaps matching the predicate, verify each, AND/OR them locally, look up addresses, optionally fetch entity RLPs in a second step. Both modes share the same proof primitives.

`ArkivPairs` is the server's prefix-scan utility for finding which `(k, v)` pairs to consult. It is outside the trie and **not** part of the proof. A client running the query entirely client-side (against a single trusted node, or cross-checking across multiple nodes) is consulting the same bitmaps the server would and will reach the same answer — the bitmaps themselves are the source of truth and they are trie-committed.

---

## 5. Gas Model

Gas is charged as a pure function of operation inputs, with no dependency on any pre-existing state. The precompile computes per-op cost from calldata only and charges it through standard revm precompile gas accounting (`PrecompileOutput::new` for success, `revert` for content violations, `halt(OutOfGas)` for budget exhaustion).

| Operation | Gas Formula |
|---|---|
| `Create` | `BASE_CREATE + GAS_PER_BYTE × len(payload) + GAS_PER_ANNOTATION × len(annotations)` |
| `Update` | `BASE_UPDATE + GAS_PER_BYTE × len(payload) + GAS_PER_ANNOTATION × len(annotations)` |
| `Delete` | `BASE_DELETE` (fixed) |
| `Extend` | `BASE_EXTEND` (fixed) |
| `Transfer` | `BASE_TRANSFER` (fixed) |
| `Expire` | `BASE_EXPIRE` (fixed) |

Per-op cost is checked against the remaining EVM gas before the op's state changes are applied. On out-of-gas the entire call budget is consumed (matching EVM OOG semantics natively via `PrecompileHalt::OutOfGas`). On content revert, only the cost of ops processed before the failure is billed; the unused remainder is refunded.

Two nodes executing the same op batch always compute identical gas regardless of their current state — the formulas reference only calldata. This is the consensus-determinism property required for the precompile to be part of the state-transition function.

`GAS_PER_ANNOTATION` covers the amortised cost of bitmap-account writes, including the bounded first-sight overhead (pair account creation + `ArkivPairs` insert) when a `(k, v)` pair is seen for the first time.

---

## 6. Reorg Handling

Op-reth's standard reorg machinery handles every consensus-critical piece of Arkiv state: entity accounts, pair accounts, the system account, and the contract's `entities` mapping all revert via the trie. There is no journal table, no Arkiv-side revert handler, no notification stream the precompile subscribes to.

The single piece of state that does **not** revert is the `ArkivPairs` MDBX table — entries are append-only and survive reorgs. The consequences are bounded and harmless:

- A reorg that removes a `Create` op leaves an `ArkivPairs` entry for the pair the create introduced. The corresponding pair account in the trie reverts (the account doesn't exist after the revert), so a range query that visits this entry reads empty/missing code and contributes nothing to the accumulator.
- A reorg that removes an `Update` op leaves any `ArkivPairs` entries the update introduced (for new annotation values). Same outcome: the pair accounts revert; the bitmaps at the reverted head don't contain the reverted entity's ID; the entries are harmless.

`ArkivPairs` is an index for efficiency, not a commitment. Its append-only nature is a feature: the table is monotonic, so reorg-safety follows for free as long as queries tolerate "pair entries that map to empty bitmaps at the current head" — which they do by construction.

---

## 7. Verification

For an **entity payload**:

```
eth_getProof(entity_address, [], blockN)  →  proves codeHash against stateRoot_N
eth_getCode (entity_address, blockN)       →  returns RLP bytes
verify keccak256(0xFE || rlp_bytes) == codeHash
```

For an **equality query result** (per-term):

```
eth_getProof(pair_address, [], blockN)                          →  proves bitmap codeHash
eth_getCode (pair_address, blockN)                              →  returns bitmap bytes
verify keccak256(bytes) == codeHash
decode bitmap; for each id:
  eth_getProof(system_account, [slot[keccak256("id_to_addr", id)]], blockN)  →  proves id → entity_address
```

For an **ownership / lifetime check**:

```
eth_getProof(EntityRegistry, [slot for entities[entityKey]], blockN)
  →  proves (owner, expiresAt) at blockN
```

The L3 `stateRoot` is anchored to L2 and ultimately L1 by the OP Stack fault-proof system. Each of the proofs above is a single-level proof against that root. There is no separate `arkiv_stateRoot`, no anchor proof, no second contract to consult.

**Range and glob query results** are verified the same way, per term: one bitmap proof per `(k, v)` pair the query touches, then a local re-execution of the union/intersect/filter logic against the verified bitmaps, plus one ID-to-address proof per surviving ID. A client that prefers to run the whole query locally can use a raw-bitmap RPC mode: fetch the bitmaps that match the predicate, verify each, AND/OR them client-side, resolve IDs through the system account, then fetch entity RLPs in a second step. The bitmaps are the source of truth and they are trie-committed; `ArkivPairs` is a server-side speedup and is not on the verification path.

---

## 8. Summary

### Storage Layout

```
Trie (committed in stateRoot):

  EntityRegistry contract:
    storage:
      createNonce[sender]                                   → uint64
      entities[entityKey]                                   → (owner: address, expiresAt: uint64)

  System account  (address = keccak256("arkiv.system")[:20]):
    nonce                                                   → 1
    storage:
      slot[keccak256("entity_count")]                       → uint64
      slot[keccak256("id_to_addr", uint64_id)]              → entity_address
      slot[keccak256("addr_to_id", entity_address)]         → uint64_id

  Entity account  (one per entity; address = entityKey[:20]):
    nonce                                                   → 1
    codeHash                                                → keccak256(0xFE || RLP(entity))
    code                                                    → 0xFE || RLP(entity)
    storage: (none)

  Pair account  (one per (k, v); address = keccak256("arkiv.pair" || k || 0x00 || v)[:20]):
    nonce                                                   → 1
    codeHash                                                → keccak256(bitmap_bytes)        (CONTENT HASH)
    code                                                    → roaring64 bitmap bytes
    storage: (none)

MDBX (op-reth's environment):
  Standard op-reth tables (Accounts, Storages, Bytecodes, ChangeSets, …) — house the trie state above.
  ArkivPairs   annot_key || 0x00 || annot_val   →   0x01                    (append-only existence flag)
```

One custom MDBX table. No journal table. No `arkiv_stateRoot` slot. No content-addressed-bitmap side store (because bitmaps **are** content-addressed natively — `codeHash` of a pair account is the bitmap content hash by construction).

### Properties

| Property | This design |
|---|---|
| Entity payload committed in trie | Yes — `codeHash` in entity account |
| Bitmap content committed in trie | Yes — `codeHash` of pair account is bitmap content hash |
| Ownership / lifetime committed in trie | Yes — `entities` mapping in `EntityRegistry` |
| Annotation-pair existence index | MDBX, append-only, outside trie |
| Custom MDBX tables required | One (`ArkivPairs`) |
| Journal / out-of-trie consensus-critical state | None |
| Third-party proof of entity state | Yes — `eth_getProof` against any retained block |
| Third-party proof of equality query result | Yes — bitmap is content-addressed; ID map is trie-committed |
| Third-party proof of range / glob query result | Yes — same per-bitmap proof, plus local re-execution of the query against verified bitmaps |
| Historical entity reads | Yes — trie versioning |
| Historical equality queries | Yes — pair `codeHash` retained at all blocks |
| Historical range / glob queries | Yes — same `ArkivPairs` iterator, with per-pair bitmap reads at the past state root |
| Covered by Optimism fault proof system | Yes for all trie state |
| External process required | No |
| Reorg handling required for trie state | No — op-reth standard |
| Reorg handling required for `ArkivPairs` | No — append-only, leaks harmless empty-bitmap entries |
| Gas model deterministic | Yes — pure function of op shape |

### Compatibility with the Optimism Verification Pipeline

All consensus-critical state changes go through revm's journaled state: account creation, `SetCode`, `SetNonce`, `SetState`. These are standard Ethereum state transitions included in the `stateRoot`. The `ArkivPairs` MDBX writes are **not** consensus-critical — they do not affect the `stateRoot` and play no role in fault proofs.

Op-reth's standard fault-proof integration (`op-program`, `cannon`, `op-challenger`) requires only that the Arkiv precompile be registered in the fault-proof EVM build, so disputed L3 blocks containing Arkiv ops can be replayed identically. The fault-proof build does **not** need the `ArkivPairs` table — replay reconstructs the trie from inputs alone, and `ArkivPairs` does not influence the trie.

The precompile must be deterministic across nodes — gas formulas are pure functions of op shape, and trie writes are pure functions of `(op batch, prior trie state)`. The same `EvmFactory` is used by the sequencer, validator, and fault-proof program, so all three paths execute identically.

What the fault proof system covers:

- Entity payload integrity: `codeHash` of entity account.
- Ownership / lifetime: `entities` mapping in `EntityRegistry`.
- Entity metadata: system-account ID maps and entity counter.
- Annotation index integrity (per-pair): `codeHash` of each pair account (the bitmap content hash itself).

`eth_getProof` works against every Arkiv account exactly as for any Ethereum account.

---

## 9. Open Questions

1. **First-sight overhead.** Every distinct `(k, v)` ever seen creates a pair account and an `ArkivPairs` entry. For chains with extreme annotation cardinality (e.g., timestamps used as annotation values), this produces a lot of pair accounts. Worth modelling against realistic workloads before fixing `BASE_CREATE` and `GAS_PER_ANNOTATION`.

2. **`ArkivPairs` pollution from speculative execution.** Reth runs the precompile across several non-canonical paths per transaction. Each can write entries to `ArkivPairs` that never become canonical. Entries are 1 byte and idempotent, so the pollution is bounded — but it grows over time and is never garbage-collected. A periodic compaction (drop entries whose pair account has empty/missing code at the canonical head) is worth scoping if cardinality becomes a problem.

3. **Op-reth Bytecodes retention.** Old bitmap-byte and entity-RLP-byte entries in op-reth's `Bytecodes` table are reachable only via historical state roots. Op-reth's retention policy (full archive, pruned, snapshot-only) determines how far back historical equality queries can reach. Choose a node profile per role and document the resulting window.

4. **Fees.** Native gas vs. an ERC-20 surcharge enforced by `EntityRegistry`. Independent decision, can be deferred. The precompile's gas model is unaffected either way.

5. **System-account address collision.** `keccak256("arkiv.system")[:20]` and the `keccak256("arkiv.pair" || …)[:20]` derivations could in principle collide with an existing externally-owned account on the L3. Genesis-time check + documentation is sufficient at chain bring-up.
