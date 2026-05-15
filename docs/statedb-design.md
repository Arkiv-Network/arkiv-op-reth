# Arkiv StateDB Design (op-reth)

## Contents

- [Abstract](#abstract)
- [1. Architecture](#1-architecture)
  - [Overview](#overview)
  - [Reth Integration](#reth-integration)
  - [EntityRegistry Smart Contract](#entityregistry-smart-contract)
  - [Arkiv Precompile](#arkiv-precompile)
  - [arkiv-entitydb crate](#arkiv-entitydb-crate)
- [2. State Model](#2-state-model)
  - [Entity Accounts](#entity-accounts)
  - [System Account](#system-account)
  - [Pair Accounts (Content-Addressed Bitmaps)](#pair-accounts-content-addressed-bitmaps)
  - [Why Numerical IDs](#why-numerical-ids)
- [3. Lifecycle](#3-lifecycle)
  - [Create](#create)
  - [Update](#update)
  - [Extend](#extend)
  - [Transfer](#transfer)
  - [Delete](#delete)
  - [Expire](#expire)
- [4. Query Execution](#4-query-execution)
  - [Equality, Inclusion, Boolean](#equality-inclusion-boolean)
  - [Historical Queries](#historical-queries)
  - [Range / Glob (not yet implemented)](#range--glob-not-yet-implemented)
  - [Query Completeness Proofs](#query-completeness-proofs)
- [5. Gas Model](#5-gas-model)
- [6. Reorg Handling](#6-reorg-handling)
- [7. Verification](#7-verification)
- [8. Summary](#8-summary)
- [9. Open Questions](#9-open-questions)

---

## Abstract

This document describes the Arkiv storage design for op-reth. **All
state** used to serve entity reads and annotation queries lives in
op-reth's world-state trie, committed in the L3 `stateRoot`. There is
no separate EntityDB process, no JSON-RPC bridge between subsystems,
no private `arkiv_stateRoot`, no anchoring submission transaction, no
ExEx, and no out-of-trie KV table.

Entities and the annotation index are expressed as Ethereum accounts.
Each entity is a dedicated account whose payload, content type, full
key, and annotation set are RLP-encoded and held as account `code`.
Each `(annotKey, annotVal)` pair is itself a dedicated account whose
code is the roaring64 bitmap of entity IDs matching that pair; the
account's `codeHash` is the keccak hash of those bytes by
construction, so **every bitmap is content-addressed in the trie**. A
singleton system account holds the global entity counter and the
trie-committed ID ↔ address map.

The contract entry point — `EntityRegistry` — holds per-entity
`(owner, expiresAt)` tuples and is responsible for ownership,
expiration, and `Ident32` charset validation. It mints entity keys,
enforces who can mutate what, dispatches to the precompile, and emits
per-op logs. The Arkiv precompile is the revm-side adapter that
decodes the calldata, charges gas, and dispatches into
`arkiv-entitydb`, which owns the indexing logic (system counter, ID
maps, bitmap deltas, RLP encode/decode).

Everything the precompile writes goes through revm's journaled state.
Op-reth's standard tables and standard reorg machinery handle every
mutation without any Arkiv-specific extension.

**What this design provides:**

- Verifiable entity payloads via standard `eth_getProof` against the
  L3 `stateRoot`. One-level proof; no anchor proof.
- Verifiable query results for the equality family. Every bitmap is
  content-addressed in the trie (`codeHash = keccak256(bitmap_bytes)`)
  and every ID → entity-address mapping is a trie-committed
  system-account slot. A client can verify each bitmap individually
  against its `codeHash`, re-execute the query logic locally (AND /
  OR / NOT), and check that the server's result matches.
- A natural "raw bitmap" RPC mode: clients can fetch matching bitmaps
  via `eth_getCode`, verify each against the proof, AND/OR them
  locally, resolve IDs through the system-account map, then fetch
  entity RLPs in a second step.
- Historical reads for every entity at every retained block, and
  historical queries against any retained `stateRoot`.
- Full Optimism fault-proof coverage of all consensus-critical Arkiv
  state.
- Single-process deployment with zero custom MDBX tables.

Range and glob queries are **not yet implemented** — they need an
ordered enumeration over `(annot_key, annot_val)` pairs that the
trie's hashed pair addresses preclude. §4 covers the limitation.

---

## 1. Architecture

### Overview

Three components inside `arkiv-op-reth`:

1. The **`EntityRegistry` smart contract** — user-facing entry point
   on the L3. Holds `(owner, expiresAt)` per entity; validates
   ownership, liveness, and `Ident32` charset; mints entity keys;
   collects fees; dispatches to the precompile; emits per-op logs.
2. The **Arkiv precompile** — invoked by `EntityRegistry` from inside
   EVM execution. A thin revm-side adapter: caller restriction,
   calldata decode, gas accounting, dispatch into `arkiv-entitydb` via
   a `StateAdapter` impl over `EvmInternals`.
3. The **`arkiv-entitydb` crate** — canonical home of the state
   model. Owns the entity / pair / system layout, RLP, roaring
   bitmap, the six op handlers, and the query language. No `revm`
   deps; runs against an abstract `StateAdapter` trait.

Every state-dependent mutation that affects consensus — entity
account writes, pair account writes (bitmaps), system account writes
— flows through revm's journaled state and is committed in the L3
`stateRoot`.

### Reth Integration

A single integration point on op-reth's standard extension surface:
an Arkiv precompile registered into `PrecompilesMap` via a custom
`EvmFactory` wrapping `OpEvmFactory<OpTx>`. The custom factory
inserts the precompile in both `create_evm` and
`create_evm_with_inspector` so simulation, tracing, payload-building,
validation, and canonical execution all see the same set.

No `BlockExecutor` wrapper, no system call, no ExEx, no
`arkiv_stateRoot` slot, no custom MDBX tables.

### EntityRegistry Smart Contract

`EntityRegistry` owns ownership, lifetime, and attribute-name
validation. The Solidity source lives in
[`contracts/src/EntityRegistry.sol`](../contracts/src/EntityRegistry.sol);
the runtime bytecode is built with `just contracts-build` and
committed to `contracts/artifacts/EntityRegistry.runtime.hex`
(consumed by `arkiv-genesis` via `include_str!`).

**SDK compatibility constraint.** The external surface — the
`execute(Operation[])` selector, the `EntityOperation` event
signature, the `nonces(address)` and `entityKey(address,uint32)`
views, the `Operation` / `Attribute` / `Mime128` / `Ident32` /
`BlockNumber32` struct and type layouts, and the op-type constants
(`CREATE=1 .. EXPIRE=6`) — is held identical to arkiv-contracts v1.
Internal storage and the contract↔precompile boundary are free to
evolve.

The contract stores only what it needs:

```solidity
struct EntityRecord {
    address       owner;
    BlockNumber32 expiresAt;     // packs with owner into one slot
}

mapping(address owner    => uint32)        public nonces;
mapping(bytes32 entityKey => EntityRecord) public entities;
```

Op set: `create | update | delete | extend | transfer | expire`. The
contract validates each op against the `entities` mapping in order,
applies its own state changes, emits the per-op `EntityOperation`
event, and accumulates a per-op record:

| Op | Contract validation | Contract state change |
|---|---|---|
| `create` | `btl > 0`; `validateIdent32` on every attribute name | mint `entityKey`; insert `(owner=sender, expiresAt)` |
| `update` | exists; `msg.sender == owner`; not expired; `validateIdent32` on every attribute name | none |
| `extend` | exists; `msg.sender == owner`; not expired; `btl > 0`; `newExpiresAt > stored` | update `expiresAt` |
| `transfer` | exists; `msg.sender == owner`; not expired; `newOwner ≠ 0`; `newOwner ≠ owner` | update `owner` |
| `delete` | exists; `msg.sender == owner`; not expired | remove entry |
| `expire` (anyone may call) | exists; `block.number > expiresAt` | remove entry |

`entityKey` is minted from a sender-scoped nonce:
```
entityKey = keccak256(chainId || registryAddress || msg.sender || nonces[msg.sender])
```
The derivation is exposed via the `entityKey(address,uint32)` view so
clients holding the sender's current `nonces` value can predict the
key before submitting the tx.

After validating and updating its own state, the contract dispatches
the whole batch to the precompile in a single `CALL`:

```solidity
struct OpRecord {                              // internal
    uint8                operationType;        // Entity.CREATE .. Entity.EXPIRE
    address              sender;               // msg.sender at validate time
    bytes32              entityKey;
    address              newOwner;             // CREATE / TRANSFER
    BlockNumber32        newExpiresAt;         // CREATE / EXTEND
    bytes                payload;              // CREATE / UPDATE
    Mime128              contentType;          // CREATE / UPDATE
    Entity.Attribute[]   attributes;           // CREATE / UPDATE
}

function _callPrecompile(OpRecord[] memory records) internal {
    (bool ok, bytes memory ret) = ARKIV_PRECOMPILE.call(abi.encode(records));
    if (!ok) revert PrecompileFailed(ret);
}
```

There are **no `old*` fields** — for ops that need the entity's
pre-op `owner` or `expiresAt` (to remove from a bitmap, or to
preserve in the re-encoded RLP), the precompile reads them from the
existing entity account's RLP, which carries `owner` and `expires_at`
(see [EntityRLP](#entityrlp)).

### Arkiv Precompile

The revm-side adapter. Per call:

- Caller restriction: refuses non-direct calls (STATICCALL,
  DELEGATECALL, value-bearing, or any caller other than
  `EntityRegistry`).
- Decode the `abi.encode(OpRecord[])` batch.
- Compute gas as a pure function of op shape (§5). Charge up-front;
  halt `OutOfGas` if the budget doesn't cover the batch.
- Wrap `EvmInternals` in a `RevmStateAdapter` that implements
  `arkiv_entitydb::StateAdapter` (`code` / `set_code` /
  `tombstone_code` / `storage` / `set_storage`).
- For each `OpRecord`, convert the ABI types into `arkiv-entitydb`
  types (`Ident32` → bytes, `Mime128` → bytes, `Attribute` →
  `StringAnnotation` / `NumericAnnotation` per `valueType`) and call
  the matching `arkiv_entitydb::{create,update,extend,transfer,delete,expire}`.

The precompile does **not** validate ownership, liveness, or
attribute names — the contract has already done that. It does no
content validation today either (e.g. payload size caps); the contract
is the validation surface.

### arkiv-entitydb crate

Canonical home of the state model. No `revm` deps, no DB deps. Runs
against an abstract trait:

```rust
pub trait StateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>>;
    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()>;
    fn tombstone_code(&mut self, addr: &Address) -> Result<()>;
    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256>;
    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()>;
}
```

The trait has two production implementations and one test
implementation:

- `arkiv_node::precompile::RevmStateAdapter` — write path. Wraps
  `&mut EvmInternals` and goes through the journal so reverts roll
  back cleanly on dispatch failure.
- `arkiv_node::rpc::RethStateAdapter` — read path. Wraps a
  `StateProviderBox` from reth; mutating methods bail (unreachable
  from the read path).
- `arkiv_entitydb::test_utils::InMemoryAdapter` — `cfg(test-utils)`.
  Drives the op handlers in unit tests without a revm context.

The op handlers (`create` / `update` / `extend` / `transfer` /
`delete` / `expire`) all take `&mut S: StateAdapter` and do the
indexing math.

---

## 2. State Model

All Arkiv state lives in three kinds of Ethereum accounts: entity
accounts (one per entity), pair accounts (one per `(annotKey,
annotVal)` ever seen — these hold the bitmaps), and the singleton
system account. The `EntityRegistry` contract holds its own per-entity
`(owner, expiresAt)` mapping plus the sender-scoped `nonces`. All in
the trie, all committed in `stateRoot`.

### Entity Accounts

#### Address Derivation

```
entityKey      = keccak256(chainId || registryAddress || msg.sender || nonces[msg.sender])
entity_address = entityKey[:20]
```

`nonces[msg.sender]` is held in `EntityRegistry`, incremented once per
`Create` op. The address is a pure identity anchor; content
commitment is via `codeHash`.

#### Account Structure

```
Entity Account  (address = entityKey[:20])
  nonce    = 1                               // prevents EIP-161 empty-account deletion on tombstoning
  balance  = 0
  codeHash = keccak256(0xFE || RLP(entity))  // commits to full entity content in the trie
  code     = 0xFE || RLP(entity)             // stored by op-reth in its Bytecodes table, keyed by codeHash

  storage slots: none
```

Entity accounts have **zero storage slots**. A single `SetCode` call
is the entirety of the entity's per-account trie footprint.

#### codeHash and RLP Storage

`codeHash` is set to `keccak256(0xFE || RLP(entity))`. Op-reth stores
the corresponding bytes in its `Bytecodes` table keyed by `codeHash`,
exactly as it does for contract bytecode. `eth_getCode(entity_address)`
retrieves the full RLP; `eth_getProof(entity_address)` includes
`codeHash` in the account node, verifiable against the L3 `stateRoot`.

The `0xFE` prefix ensures that any EVM `CALL` to an entity address
executes `INVALID` and reverts immediately. The RLP bytes are never
interpreted as bytecode.

#### EntityRLP

```rust
struct EntityRlp {
    payload:                Vec<u8>,
    creator:                Address,
    created_at_block:       u64,
    owner:                  Address,
    expires_at:             u64,
    content_type:           Vec<u8>,
    key:                    B256,                  // full 32-byte entityKey
    string_annotations:     Vec<StringAnnotation>,
    numeric_annotations:    Vec<NumericAnnotation>,
    last_modified_at_block: u64,
}
```

The RLP is **self-sufficient for query reads**: every field a client
needs to render an entity comes from a single
`eth_getCode(entity_address)`. No second lookup against
`EntityRegistry`'s storage required.

This intentionally duplicates `owner` and `expires_at` between the
entity RLP and the `EntityRegistry` contract's `entities` mapping.
The two are written together by the precompile (single revm tx, both
via journaled state) so they stay in lockstep across reorgs and
re-execution. The contract is the source of truth for **owner /
expiry validation** (cheap, no RLP decode in Solidity); the RLP is
the source of truth for **query reads** (single account read, no
stitching).

`creator` and `created_at_block` are immutable — set once at `Create`,
never updated. `owner` is rewritten on `Transfer`; `expires_at` on
`Extend`. `last_modified_at_block` is rewritten on every mutating op.
The corresponding built-in annotations (`$creator`, `$createdAtBlock`,
`$owner`, `$expiration`) provide the reverse direction (search) via
bitmaps.

The full 32-byte `key` is in the RLP so callers with only the 20-byte
address can recover the complete key.

### System Account

A singleton account at a fixed address. Pre-allocated in genesis with
`nonce = 1` (to defeat EIP-161) and empty storage.

```
System Account  (address = 0x4400000000000000000000000000000000000046)
  nonce    = 1
  storage slots:
    slot[keccak256("entity_count")]                  →  uint64       // next entity ID
    slot[keccak256("id_to_addr", uint64_id)]         →  address      // ID → entity_address
    slot[keccak256("addr_to_id", entity_address)]    →  uint64       // entity_address → ID
```

The three adjacent predeploys at `0x44…0044 / 0045 / 0046` are:

| Address | What |
|---|---|
| `0x4400…0044` | `EntityRegistry` Solidity contract |
| `0x4400…0045` | Arkiv precompile (native Rust, registered by the custom `EvmFactory`) |
| `0x4400…0046` | System account (no code; pre-allocated with `nonce=1` and empty storage) |

The `entity_count` slot is the canonical source for ID assignment.
Every node executing the same block sees the same value and assigns
IDs identically.

The `id_to_addr` and `addr_to_id` slots give both directions of the
ID ↔ address map, both trie-committed. Both are written at `Create`
and both are cleared at `Delete` / `Expire`. The address-to-ID
direction is needed during `Delete`/`Expire` to look up the entity's
ID without decoding the RLP; the ID-to-address direction is the
query-time resolver for bitmap hits.

### Pair Accounts (Content-Addressed Bitmaps)

One account per `(annotKey, annotVal)` pair ever seen. Created
lazily the first time the pair appears in an op. The bitmap of entity
IDs matching this pair is stored as the account's code; **the bitmap
is content-addressed in the trie because `codeHash = keccak256(bitmap_bytes)`
by construction**.

```
Pair Account  (address = keccak256("arkiv.pair" || key_bytes || 0x00 || val_bytes)[:20])
  nonce    = 1
  codeHash = keccak256(roaring64_bitmap_bytes)
  code     = roaring64_bitmap_bytes

  storage slots: none
```

On bitmap update, `SetCode` is called with the new bytes; `codeHash`
updates automatically to the keccak hash of the new content. Old
bitmap bytes remain in op-reth's `Bytecodes` table indefinitely,
keyed by their old hash — historical bitmap versions stay retrievable
via `eth_getCode(pair_address, blockN)` against any retained block.

The 0xFE-prefix trick used for entity accounts is **not** applied
here. A `CALL` to a pair account is not something the design needs to
defend against, and applying the prefix would defeat content-addressing.

The 20-byte pair address is derivable directly from
`(annotKey, annotVal)`, so equality queries can locate the bitmap
without consulting any index.

### Why Numerical IDs

Bitmaps are `roaring64` — compressed bitsets over 64-bit unsigned
integers. Ethereum addresses (20 bytes) cannot be stored directly in
a roaring bitmap; each entity is therefore assigned a compact
`uint64` ID at `Create` time. Both directions of the ID ↔ address
mapping live on the system account and are trie-committed.

---

## 3. Lifecycle

`EntityRegistry` validates ownership / liveness / charset from its
own storage + calldata and updates its storage before calling the
precompile. The precompile then dispatches to `arkiv-entitydb`. Every
write goes through revm's journaled state.

Whenever the op needs the entity's pre-op `owner` or `expires_at`
(for a bitmap removal, or to preserve in a re-encoded RLP), it reads
the existing entity account's RLP. The contract never forwards
`old*` fields.

### Create

**Contract:**
1. Read and increment `nonces[msg.sender]`; derive `entityKey`.
2. `validateIdent32` on every attribute name.
3. Insert `entities[entityKey] = (msg.sender, expiresAt)`.

**Op handler (`arkiv_entitydb::create`):**
1. Read and increment `entity_count` on the system account; the new
   value is `entity_id`.
2. Write the system-account ID maps:
   `slot[keccak256("id_to_addr", entity_id)] = entity_address`;
   `slot[keccak256("addr_to_id", entity_address)] = entity_id`.
3. For each annotation `(k, v)` — including built-ins `$all`,
   `$creator`, `$createdAtBlock`, `$owner`, `$key`, `$expiration`,
   `$contentType` (values derived from the record):
   - Derive `pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]`.
   - Read `pair_addr.code` (treat as empty bitmap if absent).
   - Deserialize, add `entity_id`, re-serialize. `SetCode(pair_addr, new_bytes)`.
4. Encode the entity RLP. `SetCode(entity_address, 0xFE || RLP)`.

### Update

**Contract:** validates ownership + liveness + `Ident32` charset on
every new attribute name. No storage change.

**Op handler:**
1. Read `entity_id` from `system.slot[keccak256("addr_to_id", entity_address)]`.
2. Decode the current entity RLP to recover `owner`, `expires_at`,
   `creator`, `created_at_block`, `key`, and the old annotation set.
3. Diff `(content_type + user annotations)` between old and new.
4. For each pair removed: `read_pair_bitmap`, remove `entity_id`,
   `SetCode` back.
5. For each pair added: same, add `entity_id`.
6. Re-encode the entity RLP using the new
   `payload`/`content_type`/`attributes` and the preserved
   `owner`/`expires_at`/`creator`/`created_at_block`/`key`. Set
   `last_modified_at_block = current_block`. `SetCode`.

Built-ins `$creator`, `$createdAtBlock`, `$key`, `$owner`,
`$expiration`, `$all` don't change on UPDATE, so the diff doesn't
touch them.

### Extend

**Contract:** validates ownership + liveness + `newExpiresAt >
stored.expiresAt`. Updates `entities[entityKey].expiresAt`.

**Op handler:**
1. Decode the current entity RLP. Read its `expires_at` (old value).
2. Remove `entity_id` from the `$expiration = old` pair account's
   bitmap; add it to the `$expiration = newExpiresAt` pair account's
   bitmap.
3. Re-encode the entity RLP with `expires_at = newExpiresAt`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Transfer

**Contract:** validates ownership + liveness + non-zero / different
`newOwner`. Updates `entities[entityKey].owner`.

**Op handler:**
1. Decode the current entity RLP. Read its `owner` (old value).
2. Remove `entity_id` from the `$owner = old` pair account's bitmap;
   add it to the `$owner = newOwner` pair account's bitmap.
3. Re-encode the entity RLP with `owner = newOwner`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Delete

**Contract:** validates ownership + liveness. Removes
`entities[entityKey]`.

**Op handler:**
1. Read `entity_id` from the system account's `addr_to_id` slot.
2. Decode the entity RLP to recover the full annotation set + built-ins.
3. For each pair (built-in + user): `read_pair_bitmap`, remove
   `entity_id`, `SetCode` back.
4. Clear both system-account ID slots.
5. `tombstone_code(entity_address)` — empty code, `nonce` stays at 1.

> **Why nonce stays at 1.** If nonce were zeroed, the account would
> become EIP-161-empty (nonce=0, balance=0, no code). Post-Cancun
> `handleDestruction` returns `"unexpected storage wiping"` when a
> prior non-empty storage root exists. Keeping nonce at 1 prevents
> EIP-161 from treating the account as empty; the account remains as
> a tombstone in the trie.

### Expire

Anyone may call `EntityRegistry.expire(entityKey)` once `block.number
> expiresAt`. The contract gates on the expiration check, removes the
entry, and dispatches to the precompile, which executes the same
state changes as `Delete`. There is no out-of-band housekeeping path;
expiration is contract-driven so it lives on the canonical execution
path along with every other state-mutating op.

---

## 4. Query Execution

All queries are evaluated by reading the trie. Every read is a
standard `eth_call` / `eth_getStorageAt` / `eth_getCode` against
op-reth's `StateProvider`.

The query grammar (lexer + parser in
`crates/arkiv-entitydb/src/query/`) and the tree-walking interpreter
live in `arkiv-entitydb`. The RPC layer (`crates/arkiv-node/src/rpc.rs`)
is a thin shell: take a `StateProvider` snapshot, wrap it in
`RethStateAdapter`, call `arkiv_entitydb::query::execute`, render
matching entities to wire-format `EntityData`, apply pagination.

### Equality, Inclusion, Boolean

```
Query: $contentType = "image/png" && tag = "approved"

1. Derive pair_addr_1 = keccak256("arkiv.pair" || "$contentType" || 0x00 || "image/png")[:20].
2. Derive pair_addr_2 = keccak256("arkiv.pair" || "tag"          || 0x00 || "approved")[:20].
3. Read pair_addr_1.code → bitmap_1; pair_addr_2.code → bitmap_2.
4. Deserialize both bitmaps; compute intersection in memory.
5. Apply cursor / page-size limit.
6. For each uint64_id in the result: read system.slot[keccak256("id_to_addr", id)] → entity_address.
7. eth_getCode(entity_address) → decode RLP, project per includeData.
```

Operators:

- `*` and `$all` — every live entity (reads the `$all` bitmap).
- `k = v`, `k != v` — point reads; `!=` subtracts from `$all`.
- `k IN (v1 v2 …)`, `k NOT IN (…)` — OR of per-value reads; `NOT IN`
  subtracts from `$all`.
- `&&` / `AND`, `||` / `OR` — intersect / union of sub-evaluations.
- `NOT (…)`, `!(…)` — `$all \ eval(inner)`.

Built-in keys (`$owner`, `$creator`, `$key`, `$expiration`,
`$contentType`, `$createdAtBlock`) and user-defined annotation keys
both follow the same path; the only difference is which pair-account
address gets derived for a given `(k, v)`.

### Historical Queries

The RPC handler takes an optional `atBlock` (hex number) and routes
to `provider.history_by_block_number(n)` instead of `provider.latest()`.
The resulting `StateProvider` is read by `RethStateAdapter` exactly
as for the head state. Op-reth's `Bytecodes` table retains old bitmap
bytes keyed by hash, so equality queries at any retained block
resolve cleanly.

The response's `block_number` field reports the block the query was
evaluated against (the explicit `atBlock`, or the head if absent).

### Range / Glob (not yet implemented)

Range queries (`k < n`, `k > n`, `k <= s`, `k >= s`) and glob queries
(`k ~ "prefix*"`, `k !~ "*pat*"`) would need an ordered enumeration
over the set of `(annot_key, annot_val)` pairs that exist for a given
key. The trie's pair-account addresses are
`keccak256("arkiv.pair" || k || 0x00 || v)[:20]` — the hash destroys
value ordering, so the trie itself doesn't support range scans over
values.

The standard solution (which arkiv-storage-service used) is an
**ordered sibling index**: an out-of-trie KV store keyed by
`annot_key || 0x00 || annot_val` whose entries point into the trie's
pair accounts. Numeric values stored as fixed-width big-endian, so
lex order matches numeric order. Iteration is bounded by the number
of distinct pairs matching the prefix, not by the number of entities.

That index is **not built yet** in `arkiv-op-reth`. Range and glob
queries are currently parse errors — the grammar doesn't include
those operators. When the ordered index lands, the lexer + parser
will gain `<`, `<=`, `>`, `>=`, `~`, `!~` and the evaluator will gain
matching iteration paths.

### Query Completeness Proofs

Every bitmap is content-addressed in the trie — a pair account's
`codeHash` **is** the keccak hash of its bitmap content. Every
ID-to-address mapping is a trie-committed system-account slot. From
these two primitives, a client can verify any equality-family query
result by re-running the query logic locally on cryptographically
verified bitmaps.

**Equality on `(k, v)` at block N.** Derive `pair_addr` locally.
Request `eth_getProof(pair_addr, [], blockN)` — the proof binds
`codeHash` to the L3 `stateRoot` at block N. Request
`eth_getCode(pair_addr, blockN)` for the bitmap bytes. Verify
`keccak256(bytes) == codeHash`. Decode the bitmap. For each ID,
request `eth_getProof(system_account, [slot[keccak256("id_to_addr", id)]], blockN)`
to recover and verify the corresponding entity address. The response
is complete iff it equals the decoded set.

**Multi-condition equality (`AND` / `OR` / `NOT` / `IN`).** Repeat
per term; combine bitmaps locally with the same logic the server
ran; one ID-resolution proof per surviving ID.

---

## 5. Gas Model

Gas is charged as a pure function of operation inputs, with no
dependency on any pre-existing state. The precompile computes per-op
cost from calldata only and charges it via standard revm precompile
gas accounting (`PrecompileOutput::new` for success,
`halt(OutOfGas)` for budget exhaustion).

| Op | Base | Per-byte | Per-annotation |
|---|---|---|---|
| `Create` | `G_CREATE = 80_000` | `G_BYTE = 16` × `(payload_bytes + annotation_bytes)` | `G_ANNOTATION = 5_000` |
| `Update` | `G_UPDATE = 30_000` | same | same |
| `Extend` | `G_EXTEND = 25_000` | — | — |
| `Transfer` | `G_TRANSFER = 25_000` | — | — |
| `Delete` | `G_DELETE = 50_000` | — | — |
| `Expire` | `G_EXPIRE = 50_000` | — | — |

`annotation_bytes` is `annotation_count × (32 + 128)` — the max
`Ident32` name plus the max `value128` payload per annotation. The
constant lives in `crates/arkiv-node/src/precompile.rs`.

Per-batch gas is computed before any state changes are applied. On
out-of-gas the entire call budget is consumed (matching EVM OOG
semantics natively via `PrecompileHalt::OutOfGas`).

Two nodes executing the same op batch always compute identical gas
regardless of their current state — the formulas reference only
calldata. This is the consensus-determinism property required for the
precompile to be part of the state-transition function.

---

## 6. Reorg Handling

Op-reth's standard reorg machinery handles every piece of Arkiv
state: entity accounts, pair accounts, the system account, and the
contract's `entities` mapping all revert via the trie. There is no
journal table, no Arkiv-side revert handler, no notification stream
the precompile subscribes to, no out-of-trie state to worry about.

The design is reorg-safe by construction: every consensus-critical
write goes through `EvmInternals` (`set_code` / `set_storage` /
`bump_nonce`) and lands in the journal, so reverts roll back cleanly.
No fix-up code required.

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

The L3 `stateRoot` is anchored to L2 and ultimately L1 by the OP
Stack fault-proof system. Each of the proofs above is a single-level
proof against that root. There is no separate `arkiv_stateRoot`, no
anchor proof, no second contract to consult.

---

## 8. Summary

### Storage Layout

```
Trie (committed in stateRoot):

  EntityRegistry contract  (0x4400…0044):
    storage:
      nonces[sender]                                        → uint32
      entities[entityKey]                                   → (owner, expiresAt)

  System account  (0x4400…0046):
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
  Standard op-reth tables only (Accounts, Storages, Bytecodes, ChangeSets, …).
  No custom Arkiv tables.
```

Zero custom MDBX tables. No journal table. No `arkiv_stateRoot`
slot. No content-addressed-bitmap side store (because bitmaps **are**
content-addressed natively — `codeHash` of a pair account is the
bitmap content hash by construction).

### Properties

| Property | This design |
|---|---|
| Entity payload committed in trie | Yes — `codeHash` in entity account |
| Bitmap content committed in trie | Yes — `codeHash` of pair account is bitmap content hash |
| Ownership / lifetime committed in trie | Yes — `entities` mapping in `EntityRegistry` |
| Custom MDBX tables required | None |
| Journal / out-of-trie consensus-critical state | None |
| Third-party proof of entity state | Yes — `eth_getProof` against any retained block |
| Third-party proof of equality query result | Yes — bitmap is content-addressed; ID map is trie-committed |
| Range / glob query support | Not yet — needs an ordered sibling index |
| Historical entity reads | Yes — trie versioning |
| Historical equality queries | Yes — pair `codeHash` retained at all blocks |
| Covered by Optimism fault proof system | Yes for all state |
| External process required | No |
| Reorg handling required | No — op-reth standard |
| Gas model deterministic | Yes — pure function of op shape |

### Compatibility with the Optimism Verification Pipeline

All state changes go through revm's journaled state: account
creation, `SetCode`, `SetNonce`, `SetState`. These are standard
Ethereum state transitions included in the `stateRoot`. Nothing the
precompile writes is out-of-trie.

Op-reth's standard fault-proof integration (`op-program`, `cannon`,
`op-challenger`) requires only that the Arkiv precompile be
registered in the fault-proof EVM build, so disputed L3 blocks
containing Arkiv ops can be replayed identically.

The precompile must be deterministic across nodes — gas formulas are
pure functions of op shape, and trie writes are pure functions of
`(op batch, prior trie state)`. The same `EvmFactory` is used by the
sequencer, validator, and fault-proof program, so all three paths
execute identically.

What the fault proof system covers:

- Entity payload integrity: `codeHash` of entity account.
- Ownership / lifetime: `entities` mapping in `EntityRegistry`.
- Entity metadata: system-account ID maps and entity counter.
- Annotation index integrity (per-pair): `codeHash` of each pair
  account (the bitmap content hash itself).

`eth_getProof` works against every Arkiv account exactly as for any
Ethereum account.

---

## 9. Open Questions

1. **Range / glob query support.** Needs an ordered sibling index
   keyed by `annot_key || 0x00 || annot_val`. Numeric values stored
   fixed-width big-endian for lex-order ≡ numeric-order. The natural
   place to register the table is an MDBX env opened alongside
   op-reth's, but reth's `Tables` enum is closed at the pinned rev —
   need to pick between patching reth, opening a sibling env at a
   separate path, or using an upstream extension surface that lands
   later.

2. **Op-reth `Bytecodes` retention.** Old bitmap-byte and
   entity-RLP-byte entries in op-reth's `Bytecodes` table are
   reachable only via historical state roots. Op-reth's retention
   policy (full archive, pruned, snapshot-only) determines how far
   back historical queries can reach. Document the resulting window
   per node profile.

3. **First-sight overhead.** Every distinct `(k, v)` ever seen
   creates a pair account. For chains with extreme annotation
   cardinality (e.g., timestamps used as annotation values), this
   produces a lot of pair accounts. Worth modelling against realistic
   workloads.

4. **Fees.** Native gas vs. an ERC-20 surcharge enforced by
   `EntityRegistry`. Independent decision, can be deferred. The
   precompile's gas model is unaffected either way.

5. **Per-op tx-position metadata.** `transaction_index_in_block` and
   `operation_index_in_transaction` are reported as 0 in
   `arkiv_query` responses today — revm's precompile context doesn't
   expose either. Plumbing them through would need a block-builder
   side annotation.

6. **Pair-account address collisions.** `keccak256("arkiv.pair" || …)[:20]`
   derivations could in principle collide with an existing
   externally-owned account on the L3. Genesis-time check + chain
   bring-up documentation is sufficient. (The system account is the
   fixed adjacent address `0x44…0046`, so collision risk there is
   gone.)
