# Architecture

This document describes the design of the `arkiv-op-reth` workspace: what
the binary is, how the workspace is laid out, where each concern lives,
and the design decisions that fall out.

The canonical state-model design — entity accounts, pair accounts, the
system account, gas, query verification — lives in
[`statedb-design.md`](statedb-design.md). This doc summarises and
cross-references; it does not duplicate.

The phased migration from the v1 architecture (off-process EntityDB +
ExEx + JSON-RPC bridge) to v2 (in-process precompile + custom
`EvmFactory` + one MDBX side table) is tracked in
`arkiv-op-reth-v2-migration-plan.md` at the workspace root. Current
implementation status is in §10 below.

---

## 1. What this software does

`arkiv-op-reth` is an [op-reth](https://github.com/ethereum-optimism/optimism)
fork that turns an OP-stack L2/L3 node into an **Arkiv** node by adding
three things:

1. A single predeploy at `0x4400000000000000000000000000000000000044` —
   the `EntityRegistry` contract.
2. A custom op-reth `EvmFactory` that registers an **Arkiv precompile**
   into `PrecompilesMap` for every revm context (canonical execution,
   payload-building, simulation, validation, tracing).
3. A custom MDBX table — `ArkivPairs` — registered alongside op-reth's
   built-in tables. The only piece of Arkiv state that lives outside
   the L3 state trie.

Together these store every entity, every annotation index, and every
counter inside op-reth's standard world-state trie, committed in the L3
`stateRoot`. Reads — single-entity lookups and SQL-like annotation
queries — are served by an `arkiv_*` JSON-RPC namespace backed entirely
by local state. No external indexer process, no JSON-RPC bridge, no
ExEx.

The binary is a **drop-in op-reth**: against a chainspec without the
predeploy it refuses to start (until that gating is relaxed in a future
phase). Against a chainspec containing the predeploy it installs the
custom `EvmFactory` + the `arkiv_*` RPC and serves the full Arkiv
surface.

---

## 2. System overview

```
                  ┌──────────────────────────────────────────────────┐
                  │ arkiv-node binary                                │
                  │                                                  │
                  │   ┌──────────────────────────────────────────┐   │
   user tx ──────►│   │ revm  ─── ArkivEvmFactory inserts ───────┼─► trie state (entity/pair/system accounts)
                  │   │       │   ArkivPrecompile               │   │   committed in stateRoot
                  │   │       │   into PrecompilesMap            │   │
                  │   │       └──► ArkivPrecompile ──────────────┼─► ArkivPairs MDBX table (append-only)
                  │   └──────────────────────────────────────────┘   │
                  │                                                  │
                  │   ┌──────────────────────────────────────────┐   │
   user query ───►│   │ arkiv_* RPC                              │   │
                  │   │   reads entity accounts, pair-account     │   │
                  │   │   bitmaps, ArkivPairs iterator           │   │
                  │   └──────────────────────────────────────────┘   │
                  └──────────────────────────────────────────────────┘
```

Everything consensus-critical flows through revm's journaled state and
ends up in the L3 `stateRoot`. The single non-journaled write — the
`ArkivPairs` insert on first-sight of a `(annot_key, annot_val)` pair —
is append-only and idempotent; it is not part of the verification path
(see [`statedb-design.md`](statedb-design.md) §2.4).

---

## 3. Workspace crates

```
crates/
  arkiv-node/       # binary + custom EvmFactory + precompile + arkiv_* RPC
  arkiv-cli/        # operator CLI: entity ops, batches, simulate, inject-predeploy
  arkiv-genesis/    # shared lib: predeploy address, runtime bytecode, alloc helpers
chainspec/
  dev.base.json     # geth-format dev chainspec (no predeploy; injected at recipe time)
docs/
  architecture.md   # this file
  statedb-design.md # canonical state model
```

### 3.1 `arkiv-genesis`

Pure library shared by both binaries. Owns:

- `ENTITY_REGISTRY_ADDRESS` — the canonical predeploy address constant.
- `ARKIV_DEV_MNEMONIC`, `DEV_ADDRESS`, `ARKIV_DEV_ACCOUNT_COUNT` — the
  hardhat-compatible dev mnemonic and the 100 pre-funded dev accounts
  derived from it.
- `deploy_creation_code(chain_id) -> Bytes` — runs
  `arkiv_bindings::ENTITY_REGISTRY_CREATION_CODE` through revm at the
  given chain ID and returns the resulting runtime bytecode. The
  chain ID is forwarded to revm's `block.chainid` so the EIP-712 domain
  separator baked into immutables matches the target chain.
- `predeploy_account(chain_id)`, `genesis_alloc(chain_id)` — assemble
  the predeploy + dev-funding entries for splicing into a
  `Genesis.alloc`.

The runtime bytecode comes from the in-tree Solidity source
(`contracts/src/EntityRegistry.sol`) via the committed artifact at
`contracts/artifacts/EntityRegistry.runtime.hex`. The contract has no
constructor immutables — it reads `block.chainid` at runtime — so the
same runtime code works on every chain. `just contracts-build`
refreshes the artifact after editing the source.

`genesis_alloc()` produces three predeploys + 100 dev-funded accounts:

| Address | What |
|---|---|
| `0x44…0044` | `EntityRegistry` runtime code |
| `0x44…0046` | System account (`nonce=1`, no code, no storage) |
| `…funding addresses…` | First 100 hardhat-mnemonic accounts, 10k ETH each |

(No genesis entry for `0x44…0045` — that's where `arkiv-op-reth`'s
custom `EvmFactory` registers the Arkiv precompile; it's a native
component, not an Ethereum account.)

### 3.2 `arkiv-node`

The execution-client binary. A thin wrapper around
`reth_optimism_cli::Cli`. Layout (target v2 state — current state is
the post-demolition scaffold, see §10):

- `evm.rs` — `ArkivEvmFactory` wrapping `OpEvmFactory<OpTx>`. Registers
  `ArkivPrecompile` into `PrecompilesMap` in both `create_evm` and
  `create_evm_with_inspector` so simulation, payload-building,
  validation, tracing, and canonical execution see the same set.
- `precompile/` — the precompile implementation: caller restriction,
  content validation, gas accounting, op dispatch, entity/pair/system
  account writes via `EvmInternals`, `ArkivPairs` first-sight write,
  roaring64 bitmap ser/deser, `EntityRlp` codec.
- `rpc/` — the `arkiv_*` JSON-RPC namespace. Local-only handlers
  (`arkiv_getEntity`, `arkiv_getEntityByKey`, `arkiv_getEntityCount`,
  `arkiv_query`, `arkiv_getBitmap`, `arkiv_getBlockTiming`).
- `install.rs` — wires the EvmFactory + RPC + `ArkivPairs` handle onto
  an `OpNode` builder.
- `cli.rs` — `ArkivExt` clap args (currently an empty wrapper over
  `RollupArgs`; v2 flags land here as they appear).
- `genesis.rs` — `has_arkiv_predeploy(chain)` bytecode-equality check.

There is **no chainspec mutation**. The chainspec is whatever `--chain`
loaded; we never reach into `config_mut()` to inject anything at
startup. The predeploy must already be in the loaded chainspec's
`alloc` (see §6).

### 3.3 `arkiv-cli`

The operator command-line tool. Two distinct surfaces:

**Entity operations** (require an RPC endpoint + signer):
`create`, `update`, `extend`, `transfer`, `delete`, `expire`, `query`,
`balance`, `spam`, `batch`, `simulate`. All ops go through
`EntityRegistry.execute(Operation[])` — the contract validates
ownership / liveness and dispatches to the Arkiv precompile during EVM
execution. The CLI itself is unaware of the precompile; it speaks the
same Solidity ABI as any other contract caller.

**Genesis post-processing** (no network required):
`arkiv-cli inject-predeploy <input.json>` reads `chainId` from a
geth-format genesis, computes the matching predeploy runtime bytecode,
and splices it into `alloc` at the canonical predeploy address.
Composes with op-deployer output for production deployments. See §6.

The traffic simulator (`simulate`) is described in detail in the
migration plan; the short version is that it rotates through
mnemonic-derived signers, maintains an in-memory pool of alive
entities, and submits a weighted random mix of CRUD ops. State updates
come from decoding `EntityOperation` logs and from polling the
contract's `entities` mapping.

---

## 4. State model

The canonical state-model spec is in
[`statedb-design.md`](statedb-design.md). A one-paragraph summary:

Three kinds of Ethereum account hold Arkiv state in the trie. **Entity
accounts** (one per entity, address = `entityKey[:20]`) hold the
RLP-encoded entity payload + annotation set in `codeHash`, prefixed
with `0xFE` so a `CALL` reverts. **Pair accounts** (one per
`(annot_key, annot_val)` ever seen) hold a roaring64 bitmap of matching
entity IDs as the account's code; `codeHash` is the keccak hash of the
bitmap bytes, so **every bitmap is content-addressed in the trie**. A
singleton **system account** at `0x4400000000000000000000000000000000000046` (adjacent to the registry) holds
the global entity counter (`entity_count`) and both directions of the
ID ↔ address map. The `EntityRegistry` contract holds per-entity
`(owner, expiresAt)` and a sender-scoped `createNonce`.

Outside the trie, the `ArkivPairs` MDBX table is an append-only
existence index of every `(annot_key, annot_val)` pair ever seen — it
exists to make range and glob queries efficient via prefix scans. It
is **not** part of the verification path. Range/glob query *results*
are verified against the per-pair bitmap proofs (each bitmap is a
single-level `eth_getProof` against `stateRoot`).

---

## 5. Precompile integration

The `EntityRegistry` contract validates ownership and liveness from its
own storage, updates that storage, then calls the Arkiv precompile via
a fixed address. The precompile:

- Refuses non-direct calls (STATICCALL, DELEGATECALL, value-bearing, or
  any caller other than `EntityRegistry`).
- Validates content (payload size, attribute count, attribute formats,
  ban on `0x00` in annotation keys/values).
- Computes per-op gas as a **pure function of calldata** (no
  state-dependent gas). Charged via standard revm precompile
  accounting.
- Performs every consensus-critical write through revm's journaled
  state via `EvmInternals`: entity-account create + `SetCode`,
  pair-account create + bitmap `SetCode`, system-account `SetState` for
  `entity_count` and the ID maps.
- Performs one **direct MDBX write** on first sight of an annotation
  pair: `ArkivPairs[k || 0x00 || v] = 0x01`. Not journaled, not
  reverted on reorg, idempotent.

Determinism is by construction: gas is pure-function-of-calldata, trie
writes are pure-function-of-(op-batch, prior-trie-state), and the same
`EvmFactory` is used by sequencer, validator, and the fault-proof
program — so all three execute identically. See
[`statedb-design.md`](statedb-design.md) §1.4 and §5.

---

## 6. Custom MDBX table

`ArkivPairs` lives alongside op-reth's built-in tables. Schema:

```
ArkivPairs   annot_key || 0x00 || annot_val   →   0x01
```

Append-only and idempotent. Two consequences fall out:

- **Reorg-safe by virtue of being append-only.** The trie (which holds
  the bitmap contents) reverts via op-reth's standard machinery; an
  `ArkivPairs` entry pointing at a reverted pair account just resolves
  to an empty bitmap at the new head and contributes nothing to range
  queries.
- **Speculative-execution pollution is bounded.** Reth runs the
  precompile across multiple paths per transaction; non-canonical
  paths can write `ArkivPairs` entries that never become canonical.
  Entries are 1 byte and idempotent. No GC today; a future periodic
  compaction is tracked as an open item.

This is the **only** direct MDBX touch from the precompile. Everything
else flows through revm's journaled state. The asymmetry is
deliberate — `ArkivPairs` is a server-side index for prefix scans, not
a commitment. See [`statedb-design.md`](statedb-design.md) §2.4 / §6.

**Open implementation question:** at the pinned reth rev
(`27bfddeada3953edc22759080a3659ccea62ca1f`), the reth `Tables` enum is
historically closed. Three options to register `ArkivPairs`: patch
reth, open a sibling MDBX env at a separate path, or use whatever
post-`Tables`-enum extension surface exists at this rev. Decision is
the first deliverable of Phase 2; see the migration plan.

---

## 7. `arkiv_*` RPC namespace

Local-only — no proxy, no external indexer. Handlers (target set):

| Method | What it returns |
|---|---|
| `arkiv_getEntity(addr, [blockTag])` | Decoded RLP from the entity account's code at that block |
| `arkiv_getEntityByKey(key, [blockTag])` | Same, with address derivation from the 32-byte key done server-side |
| `arkiv_getEntityCount([blockTag])` | `system.slot[keccak256("entity_count")]` |
| `arkiv_query(predicate, [options])` | Query execution: derive pair addresses for equality terms, iterate `ArkivPairs` for range/glob, combine bitmaps, resolve IDs through the system account, return entities or addresses |
| `arkiv_getBitmap(pair_addr, [blockTag])` | Raw bitmap bytes + codeHash; supports client-side query verification |
| `arkiv_getBlockTiming()` | Current block number, timestamp, seconds since previous block |

All historical reads work via op-reth's standard historical state
cursor — bitmap bytes are retained in the `Bytecodes` table keyed by
hash, so equality queries at any retained block resolve cleanly.

`arkiv_query` replaces the v1 EntityDB-proxy method of the same name.
The wire shape **changes**: v1 took an opaque `expr: String` + options
object proxied verbatim to the Go EntityDB. The v2 grammar is a
structured-predicate JSON; the exact shape is tracked in the migration
plan as an open question to lock during Phase 4.

The namespace is registered on every transport the operator has
enabled (`--http`, `--ws`, `--ipc`).

---

## 8. Genesis construction

Genesis is the thorniest part of integrating with the OP stack, and the
design here has been iterated several times. The current rules:

### 8.1 No runtime mutation

The chainspec is treated as read-only data flowing in from `--chain`.
Whatever needs to be in there must already be in there before `--chain`
is parsed. This is what lets the binary be a true drop-in op-reth, and
what keeps `op-reth init` and `op-reth node` in agreement on the
genesis hash (mutating the chainspec at startup historically caused
`init` and `node` to disagree).

### 8.2 Path-A chainspec

OP-reth supports two paths to build an `OpChainSpec`: a **pure-JSON**
path (hardforks in `config.{bedrockBlock, regolithTime, …}`,
EIP-1559 params in `config.optimism`) and a **programmatic** path
(forks attached in code via `LazyLock`, e.g. `OP_DEV`).

For an `--chain ./file.json` flow to work for both `init` and `node`,
the chainspec **must** be the pure-JSON form. The programmatic form
loads the JSON with no hardforks active, the engine produces
post-hardfork blocks anyway, and validation explodes.

`chainspec/dev.base.json` is therefore pure-JSON, with all OP
hardforks activated at time 0.

### 8.3 The Holocene `extraData` requirement

After Holocene, EIP-1559 base-fee parameters are encoded in the
previous block's `extraData` (9 bytes:
`[version=0x00][denominator: u32 BE][elasticity: u32 BE]`). When block
1 is validated against genesis, the consensus path bails if
`genesis.extra_data` isn't exactly 9 bytes.

The decoder has a documented fallback: if both encoded values are
zero, it falls back to the chainspec's `base_fee_params_at_timestamp`.
So `extraData = 0x000000000000000000` (9 zero bytes) is the canonical
"use chainspec params at block 0" value. `chainspec/dev.base.json`
ships with this.

### 8.4 The injection step

`chainspec/dev.base.json` ships with an empty `alloc` — no predeploy,
no funded accounts. Both are added via `arkiv-cli inject-predeploy` at
recipe time:

```bash
cp chainspec/dev.base.json $TMPDIR/genesis.json
arkiv-cli inject-predeploy $TMPDIR/genesis.json
op-reth init --chain $TMPDIR/genesis.json --datadir $TMPDIR
op-reth node --chain $TMPDIR/genesis.json --datadir $TMPDIR …
```

This composes with op-deployer output for production:

```bash
op-deployer apply --intent intent.toml --workdir ./ops
arkiv-cli inject-predeploy ops/genesis.json
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

Why not bake the bytecode into the JSON? Drift — bytecode regenerates
when the Solidity source changes; build-time injection (from the
in-tree `contracts/` artifact) means every build is consistent with
the source it depends on.

### 8.5 System account pre-allocation

`genesis_alloc()` pre-allocates the system account at
`0x4400000000000000000000000000000000000046` with `nonce=1` (empty
code, empty storage). Pre-allocation avoids a per-`Create`
"does the system account exist?" check, and the adjacent address means
no derivation / collision check is needed at chain bring-up.

---

## 9. Testing surface

Targets, per the migration plan:

| Layer | Test type |
|---|---|
| `arkiv-genesis` | Pure-function unit tests; bindings drift tests run upstream |
| `arkiv-cli inject-predeploy` | Smoke: feed a known JSON, diff the alloc |
| Precompile per-op | Unit tests on a revm harness: pre-state → call → assert journaled writes match expected codeHashes |
| Precompile caller restriction | STATICCALL / DELEGATECALL / non-EntityRegistry caller all fatal-error |
| `ArkivPairs` first-sight | Same `(k, v)` twice → one entry |
| Gas determinism | Two revm contexts, different pre-state, same op batch → identical gas |
| End-to-end | Dev chain + `arkiv-cli batch` + `arkiv_getEntity` round-trip |
| Range / glob query | Multi-pair fixture; verify result + bitmap proofs |
| Reorg posture | Force a reorg; confirm trie state reverts and `ArkivPairs` entries remain harmless |

The matrix that v1 used (manual `just node-dev-jsonrpc` + `just batch`
+ mock-entitydb output) is gone with the JSON-RPC bridge.

---

## 10. Implementation status

Phase 1 (demolition of the v1 ExEx + EntityDB bridge) is complete. The
binary currently compiles as plain op-reth with predeploy detection:
no precompile, no `arkiv_*` RPC, no `ArkivPairs` table. Phases 2–6
fill it back in.

| Phase | What | Status |
|---|---|---|
| 0 | Doc rewrite (this doc + `statedb-design.md`) | done |
| 1 | Demolition of v1 ExEx, storage, RPC proxy, storaged | done |
| 2 | `arkiv-db` crate + empty `ArkivEvmFactory` scaffolding | pending |
| 3 | Precompile + per-op handlers + gas + tests | pending |
| 4 | `arkiv_*` RPC namespace | pending |
| 5 | Integration glue + reorg/spec-exec tests | pending |
| 6 | `arkiv-cli` Query/Hash/History cleanup + simulator audit | pending |

Phase details, exit criteria, and open questions are in
`arkiv-op-reth-v2-migration-plan.md` at the workspace root.

The v2 design depends on coordinated changes to the `EntityRegistry`
contract (move ownership / lifetime validation into the contract; drop
the rolling-changeset-hash machinery; call the precompile with
`msg.sender` + minted keys + owner + expiresAt). Contract work lives
in `arkiv-contracts`; a rev bump of `arkiv-bindings` picks it up here.

---

## 11. Key design decisions, recapped

| Decision | Why |
|---|---|
| Predeploy at `0x44…0044` | Matches OP convention for system contracts; the address is a property of the chain, not the binary |
| Custom `EvmFactory` (not ExEx) | State mutation happens inside EVM execution, not after; the result lands in `stateRoot` and inherits op-reth's standard reorg machinery for free |
| Bitmap as account code (`codeHash` = `keccak256(bitmap)`) | Content-addressing in the trie comes for free; query verification is one `eth_getProof` per bitmap |
| `0xFE` prefix on entity-account code | Defends against accidental `CALL` to an entity address; `INVALID` opcode reverts immediately |
| Entity tombstone keeps `nonce=1` | Prevents EIP-161 pruning of deleted entities |
| `ArkivPairs` outside the trie, append-only | Index, not commitment; range/glob enumeration via prefix scan without contaminating consensus state |
| Gas is pure function of calldata | Consensus determinism by construction — same op batch from any pre-state charges the same gas |
| Same `EvmFactory` across sequencer/validator/fault-proof | Disputed Arkiv blocks replay identically in `op-program` / `cannon` / `op-challenger` |
| Path-A chainspec | `op-reth init` and `op-reth node` need to agree on genesis hash when reading the same JSON file |
| `inject-predeploy` as a separate post-process | Composes with op-deployer output rather than forking it; same tool serves dev and prod |
| `arkiv-genesis` as its own crate | Both binaries need the same predeploy-bytecode generator; lifting it out avoids cross-bin deps |
| No runtime chainspec mutation | Removed the `init`/`node` genesis-hash divergence bug structurally |

---

## 12. Things this design does *not* do

- **Built-in `--chain arkiv` name.** Could be done via a custom
  `ChainSpecParser`. Not pursued; the file-based flow works and
  composes uniformly with prod.
- **Mainnet predeploy registration in `L2Genesis.s.sol`.** The cleanest
  long-term home for `EntityRegistry`. Not pursued yet; the
  post-process approach has a tinier surface we own.
- **L1 / op-node / op-batcher / op-proposer.** Out of scope. This repo
  is the L3 execution client only.
- **Pre-Bedrock state import.** Standard op-reth concern.
- **Fault-proof EVM integration verification.** The design relies on
  `op-program` / `cannon` / `op-challenger` picking up our custom
  `EvmFactory` through whatever shared crate dependency they use; if
  they bypass `OpNode` and instantiate `OpEvmFactory<OpTx>` directly
  they'll miss the precompile. Tracked as an investigation item.

---

## 13. Where to read next

- **Canonical state model:** [`statedb-design.md`](statedb-design.md) —
  read this if you're touching the precompile, the RPC handlers, or
  the gas model.
- **Migration plan:** `arkiv-op-reth-v2-migration-plan.md` (workspace
  root) — phase scope, exit criteria, open questions.
- **EntityRegistry contract:**
  <https://github.com/Arkiv-Network/arkiv-contracts> — `EntityRegistry.sol`,
  `Entity.sol`, and `docs/value128-encoding.md` for attribute-value
  packing.
- **`arkiv-bindings`:** the contracts repo also publishes Rust bindings
  consumed here as `arkiv-bindings`. Validated types (`Ident32`,
  `Mime128`), `Operation` calldata encoders, and storage-layout helpers
  live there.
- **op-reth:** <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>
- **alloy-evm `Precompile` trait:** the host trait the Arkiv precompile
  implements.
