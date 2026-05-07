# Out-of-Band DB Coupling in Custom Precompiles

A challenge analysis for the Arkiv L3 node design, with attention to the
multi-execution model that every EVM client implements and what it implies
for any precompile that talks to a sibling process.

> **Status.** Analysis document. Frames the problem space; does not
> prescribe a single resolution. Sibling reading: the v2 design spec
> ([`reth-exex-entity-db-design-v2.md`](reth-exex-entity-db-design-v2.md))
> for the architecture this analyses, and the existing
> [`custom-precompile.md`](custom-precompile.md) §6.1 for the working POC's
> treatment of the same problem at lower depth.

---

## Contents

- [0. Purpose and scope](#0-purpose-and-scope)
- [1. Precompiles: foundations](#1-precompiles-foundations)
  - [1.1 What a precompile is](#11-what-a-precompile-is)
  - [1.2 What a precompile sees](#12-what-a-precompile-sees)
  - [1.3 What a precompile does *not* see](#13-what-a-precompile-does-not-see)
  - [1.4 The precompile contract: consensus-pure return](#14-the-precompile-contract-consensus-pure-return)
  - [1.5 The standard set is all pure of input bytes](#15-the-standard-set-is-all-pure-of-input-bytes)
  - [1.6 Stateful precompiles via `EvmInternals`](#16-stateful-precompiles-via-evminternals)
  - [1.7 An aside on registration: `PrecompilesMap`, `EvmFactory`, `create_evm`](#17-an-aside-on-registration-precompilesmap-evmfactory-create_evm)
- [2. The Arkiv L3 architecture in brief](#2-the-arkiv-l3-architecture-in-brief)
  - [2.1 Components](#21-components)
  - [2.2 The Arkiv precompile's role](#22-the-arkiv-precompiles-role)
  - [2.3 The two-level staging discipline](#23-the-two-level-staging-discipline)
  - [2.4 In-block state-root commitment](#24-in-block-state-root-commitment)
- [3. The core challenge: out-of-band coupling](#3-the-core-challenge-out-of-band-coupling)
  - [3.1 What "out-of-band" means in this context](#31-what-out-of-band-means-in-this-context)
  - [3.2 Reads vs writes — the load-bearing distinction](#32-reads-vs-writes--the-load-bearing-distinction)
  - [3.3 The EVM has no rollback authority over external state](#33-the-evm-has-no-rollback-authority-over-external-state)
  - [3.4 The implicit assumption that has to hold](#34-the-implicit-assumption-that-has-to-hold)
- [4. The multi-execution problem](#4-the-multi-execution-problem)
  - [4.1 Why the EVM re-executes the same transaction](#41-why-the-evm-re-executes-the-same-transaction)
  - [4.2 The execution paths in detail](#42-the-execution-paths-in-detail)
  - [4.3 Why the precompile fires on every path](#43-why-the-precompile-fires-on-every-path)
  - [4.4 Empirical evidence](#44-empirical-evidence)
- [5. How multi-execution interacts with the v2 staging discipline](#5-how-multi-execution-interacts-with-the-v2-staging-discipline)
  - [5.1 The hierarchy is sound for one linear execution stream](#51-the-hierarchy-is-sound-for-one-linear-execution-stream)
  - [5.2 It does not compose across the seven paths](#52-it-does-not-compose-across-the-seven-paths)
  - [5.3 Concrete failure modes](#53-concrete-failure-modes)
  - [5.4 The atomicity claim is path-restricted](#54-the-atomicity-claim-is-path-restricted)
- [6. Why this is hard to fix from inside the precompile](#6-why-this-is-hard-to-fix-from-inside-the-precompile)
  - [6.1 What the precompile can and cannot know](#61-what-the-precompile-can-and-cannot-know)
  - [6.2 The discrimination problem (briefly)](#62-the-discrimination-problem-briefly)
  - [6.3 Context-sensitivity violates the precompile contract](#63-context-sensitivity-violates-the-precompile-contract)
- [7. Resolution shapes and their trade-offs](#7-resolution-shapes-and-their-trade-offs)
  - [7.1 Session-id-aware DB](#71-session-id-aware-db)
  - [7.2 Idempotent staging keyed by consensus-stable identifiers](#72-idempotent-staging-keyed-by-consensus-stable-identifiers)
  - [7.3 `EvmFactory` routing per call site](#73-evmfactory-routing-per-call-site)
  - [7.4 Full DB idempotency with semantic restrictions](#74-full-db-idempotency-with-semantic-restrictions)
  - [7.5 Read-only precompile + ExEx-side writes](#75-read-only-precompile--exex-side-writes)
  - [7.6 Comparison](#76-comparison)
- [8. The deeper question: can a sibling DB be in the consensus envelope?](#8-the-deeper-question-can-a-sibling-db-be-in-the-consensus-envelope)
- [9. Open questions](#9-open-questions)
- [10. Glossary](#10-glossary)

---

## 0. Purpose and scope

This document explains, in depth, why a custom precompile that communicates
with a sibling process — Arkiv's `EntityDB` is the concrete case, but the
analysis applies to any external state machine — runs into structural
problems that are not present for any precompile in the canonical Ethereum
set, and that are not resolved by the per-tx / per-block staging discipline
proposed in the v2 design.

The intended reader is someone who knows broadly what an L2/L3 execution
client does but has not had to internalise the precompile contract or the
multi-execution model the EVM relies on. The document therefore starts at
foundations (§1, what a precompile actually is and what it sees), builds
the Arkiv architecture on top (§2), and only then turns to the challenge
itself (§3-§6) and the solution space (§7-§8).

What this document does **not** do:

- Pick a single resolution. §7 enumerates the shapes that survive analysis;
  the choice depends on operational and semantic preferences out of scope
  here.
- Re-derive the v2 design. It treats the spec as given.
- Cover the orthogonal "is the DB itself deterministic" question
  (floating-point, hash-map iteration, OS RNG, etc.). That's a separate
  audit, mostly DB-internal.

---

## 1. Precompiles: foundations

### 1.1 What a precompile is

A precompile is a fixed-address handler the Ethereum Virtual Machine
consults instead of running user-supplied bytecode. When the EVM
encounters a `CALL`, `STATICCALL`, or `DELEGATECALL` to one of a small set
of addresses, it does not enter the bytecode interpreter for that call.
It dispatches to a native (Rust, in reth's case) function that takes the
call's calldata, gas budget, caller, value, and a handle into the
journalled EVM state, and returns either output bytes plus a gas charge,
or a halt status.

The canonical Ethereum precompile set is:

| Address | Purpose |
|---|---|
| `0x01` | `ECRECOVER` — recover an address from an ECDSA signature |
| `0x02` | `SHA256` — SHA-2 digest |
| `0x03` | `RIPEMD160` |
| `0x04` | `IDENTITY` — copy input to output |
| `0x05` | `MODEXP` — modular exponentiation |
| `0x06`-`0x08` | `BN256` curve operations (add, mul, pairing) |
| `0x09` | `BLAKE2F` |
| `0x0a` | `POINT_EVALUATION` — KZG proof verification (EIP-4844) |
| `0x0b`-`0x11` | BLS12-381 family (EIP-2537) |

Every entry is a pure cryptographic primitive. Every entry's output is a
deterministic function of its calldata bytes. None of them read state,
block context, or anything outside their input.

The reason precompiles exist at all is performance. `MODEXP` in EVM
bytecode is possible but staggeringly expensive; `MODEXP` as a native
function written in C or Rust is fast. The precompile mechanism gives the
EVM a way to expose efficient native implementations of operations that
would otherwise be ruinous to do in-EVM, while keeping the cost predictable
(precompiles charge gas like any other call) and the semantics
deterministic (every node runs the same native function).

A custom precompile, in an L2/L3 context, is the same mechanism applied
to chain-specific functionality. Optimism's stack has had pre-deployed
system contracts since Bedrock; some of those have native precompile
acceleration. EigenLayer's precompiles, the BLS12-381 set in EIP-2537,
and various chains' SNARK-verifier precompiles all follow the same
pattern: a fixed address, a native function, gas accounted in calldata.

The Arkiv precompile in the v2 design is in the same family of mechanism
but does something none of the canonical precompiles do: it talks to a
sibling process over JSON-RPC. That difference is the entire subject of
this document.

### 1.2 What a precompile sees

A precompile in modern reth (revm-precompile 34, alloy-evm 0.33) is
called with the following input shape:

```rust
pub struct PrecompileInput<'a> {
    pub data: &'a [u8],            // calldata
    pub gas: u64,                  // gas limit available
    pub reservoir: u64,            // EIP-8037 state-gas reservoir
    pub caller: Address,           // msg.sender
    pub value: U256,               // call value
    pub target_address: Address,
    pub is_static: bool,           // STATICCALL?
    pub bytecode_address: Address,
    pub internals: EvmInternals<'a>,  // hooks back into journalled state
}
```

Plus the block environment, which is bound at `EvmFactory::create_evm`
time (so it is part of the EVM instance the precompile is registered on,
rather than per-call):

- block number, timestamp, base fee, blob base fee, prev randao,
  beneficiary (coinbase), gas limit;
- chain ID via the EVM config;
- `EvmInternals` provides reads (and, with care, writes) of journalled
  EVM state — account balances, code, storage slots.

That is the complete set of inputs available. Every value in the list is
**consensus-deterministic** — every honest node executing this transaction
on this block sees the same bytes.

### 1.3 What a precompile does *not* see

The list of things `PrecompileInput` does not expose is more interesting
than the list of things it does:

- **Which execution path called it.** A precompile cannot tell whether
  it is running inside the canonical block executor, the gas estimator,
  the mempool admission validator, the engine-API forkchoice validator,
  or `debug_traceBlock`. The EVM dispatches identically in all cases.
- **The transaction hash.** During payload building and engine-API
  validation, the tx hash *is* known and is consensus-stable. During gas
  estimation against pending state it is not. There is no field on
  `PrecompileInput` to surface this either way.
- **The position of the current call in the broader execution.** Block
  index, transaction index within the block, call-frame depth, intra-tx
  precompile invocation counter — none are surfaced.
- **Whether the surrounding transaction will end up canonical.** This is
  a future fact about consensus that no execution path can know at the
  time the precompile runs.
- **Whether the surrounding call frame will eventually revert.** The
  precompile produces its result and gas charge in isolation; the EVM
  decides revert behaviour later, by which point the precompile has
  long since returned.

The precompile API was designed under the assumption that the answer is
a function of input + journalled state, full stop. Anything not on that
list isn't on `PrecompileInput` because nothing the standard precompiles
do needs it.

### 1.4 The precompile contract: consensus-pure return

The property the EVM relies on is simple: **two honest nodes given the
same precompile inputs must compute the same outputs**. If they don't,
they produce different receipts, different state roots, different block
hashes, and the chain forks at that block.

The strongest version of this property — "output is a function of
calldata bytes alone" — is what every standard precompile satisfies. But
the EVM relies on a slightly weaker version that admits richer inputs as
long as those inputs are themselves consensus-deterministic:

> The precompile's return must be a deterministic function of
> consensus-visible state: calldata, caller, value, the call-site context
> (`is_static`, addresses), the journalled EVM state at the call site,
> and the block environment.

`SLOAD` reads journalled storage; that's allowed. `BLOBHASH` (an opcode,
not a precompile, but the same principle applies to opcodes that have
precompile-like signatures) reads `tx.blob_versioned_hashes[i]`; also
allowed. Block-environment opcodes read `block.timestamp`,
`block.coinbase`, etc. All of these are consensus-deterministic — every
node has them — even though none of them are functions of calldata alone.

A precompile may read any of these. It may read state via `EvmInternals`.
It may read the block env. None of that is a problem for consensus.

What it may **not** do, without extra work to bring the source into the
consensus envelope, is read or write anything outside this set: the wall
clock, an OS RNG, a sibling process, a TCP socket. None of those are
consensus-determined; different nodes can — and routinely do — disagree
about what they would return at any given moment.

### 1.5 The standard set is all pure of input bytes

It's worth spelling out: there is no "impure precompile" in the canonical
Ethereum spec, in the sense of "depends on state outside calldata". Every
single one is a stateless function from bytes to bytes:

```rust
fn ecrecover(data: &[u8]) -> Result<Bytes, _> { ... }
fn sha256(data: &[u8])    -> Result<Bytes, _> { ... }
fn modexp(data: &[u8])    -> Result<Bytes, _> { ... }
// ...
```

This is the strongest possible form of "consensus-pure". The precompile
mechanism *permits* richer inputs (via `EvmInternals` for journalled
state access), but no canonical precompile actually uses them. Custom
precompiles in L2 and L3 deployments that read storage do exist — for
example, precompiles that act as efficient lookup tables against system
contract storage — and they're consensus-safe. But "talks to a sibling
process" is not on the list of things any precompile in production does
on a public chain today.

### 1.6 Stateful precompiles via `EvmInternals`

`EvmInternals` is the API surface that lets a precompile read (and, with
care, write) journalled EVM state. The relevant operations include:

- Read an account's balance, nonce, code hash, code.
- Read a storage slot.
- Write a storage slot (rare; gas accounting and revert semantics are
  subtle).
- Spawn a sub-call into another contract or precompile.

Writes via `EvmInternals` go through the journal, which means they
participate in the EVM's normal revert / commit machinery. If the
surrounding call frame reverts, the journal undoes the writes. If the
transaction reverts, same. If the block ends up not canonical, the
entire state diff is discarded by consensus.

The journal is the EVM's tool for owning the consequences of every
mutation it permits. Nothing the EVM authorises is irreversible inside
the EVM's universe. This is what makes intra-tx revert safe, what makes
DELEGATECALL into untrusted code safe, and — critically for what follows
— what *isn't* available for mutations that go outside the EVM's
universe.

### 1.7 An aside on registration: `PrecompilesMap`, `EvmFactory`, `create_evm`

Because the resolution discussion in §7 turns on where in the reth stack
a precompile gets installed, it's worth a brief tour.

A precompile is attached to a specific EVM instance via `PrecompilesMap`,
a mutable container that the EVM consults on every call. `PrecompilesMap`
is populated at EVM construction time by `EvmFactory::create_evm` (or
`create_evm_with_inspector` for tracing), which is called once per fresh
EVM instance.

Reth creates fresh EVM instances *frequently*: once per gas estimation
RPC call, once per mempool admission, once per pending-state query, once
per block payload build, once per canonical block execution, once per
engine-API validation, once per `debug_traceBlock` request. Each call
to `create_evm` produces a brand-new EVM with its own `PrecompilesMap`.
The same `EvmFactory` is reused across all of them.

The Arkiv-side wiring is therefore:

- One `EvmFactory` (`ArkivOpEvmFactory`) wraps `OpEvmFactory<OpTx>` and
  installs the Arkiv precompile in every `EvmEnv` it produces.
- That factory holds an `Arc<EntityDbClient>` — the IPC handle to the DB.
- Every fresh EVM the factory produces has the precompile in its
  `PrecompilesMap`, sharing the same DB handle.

The factory is deliberately path-agnostic. It does not know — and has no
mechanism to know — *which* of the seven execution paths is asking for
an EVM at any given moment. This is the fact that §6 turns on.

---

## 2. The Arkiv L3 architecture in brief

### 2.1 Components

The v2 design splits the L3 node into four cooperating subsystems plus
one off-process state machine:

```
┌───────────────────────────────────────────────────────────┐
│  arkiv-op-reth                                            │
│                                                           │
│   EntityRegistry SC ──── (EVM call) ──→ Arkiv Precompile  │
│       (ownership +                          │             │
│        lifetime + fees)                     │ JSON-RPC    │
│                                             │             │
│   Custom BlockExecutor ─────────────────────┤             │
│       (finish: arkiv_commitBlock + system call)           │
│                                             │             │
│   Arkiv ExEx ───────────────────────────────┤             │
│       (ChainReverted, ChainReorged only)    │             │
│                                             ▼             │
└─────────────────────────────────────────────┬─────────────┘
                                              │
                                              ▼
                                   ┌─────────────────────────┐
                                   │  EntityDB (separate     │
                                   │  process; HTTP JSON-RPC)│
                                   └─────────────────────────┘
```

The EVM contract surface is intentionally narrow: `EntityRegistry` holds
only `(owner, expiresAt)` per entity plus a per-block `arkiv_stateRoot`
slot. Everything else — payload, attributes, query index, the entity
trie — lives in EntityDB.

### 2.2 The Arkiv precompile's role

The precompile is the per-transaction synchronous bridge from EVM
execution into EntityDB. Its responsibilities are:

1. Decode the operation batch from calldata.
2. Compute static gas from calldata (`Σ f(op_type, payload_bytes,
   attr_count, ...)`), with no DB-side lookups.
3. Charge that gas to the EVM up front.
4. Dispatch the batch to EntityDB synchronously via `arkiv_applyTx`.
5. Map the DB's response (`ok` / `revert` / `fatal`) onto a
   `PrecompileResult`.

It is stateless in the EVM sense — no journalled storage of its own — but
holds an `Arc<EntityDbClient>` for IPC. It is registered at a fixed
address (`0x...0900` in the v2 spec) via the custom `EvmFactory`
described above.

Crucially, **the precompile is the only path through which per-tx
operations reach EntityDB**. There is no parallel ExEx-driven path for
data; the v2 design eliminated that to make tx-level atomicity possible.

### 2.3 The two-level staging discipline

EntityDB maintains a three-tier write hierarchy:

```
arkiv_applyTx       →  per-tx staging   (in-memory; discardable)
  status == ok      ↓  promote
                     →  per-block staging (CacheStore; in-memory)
arkiv_commitBlock   ↓  flush
                     →  PebbleDB         (durable, single atomic batch)
```

Each tier is meant to be tied to a specific EVM event:

| Tier | Tied to | Reverts when |
|---|---|---|
| Per-tx staging | EVM tx execution | DB returns `revert`; or the EVM tx reverts before `commitBlock` (in principle) |
| Per-block staging (`CacheStore`) | The block being assembled | `BlockExecutor::finish` returns `Err`; block doesn't seal |
| Durable (PebbleDB) | Block sealing | Reorg, via `arkiv_revert` / `arkiv_reorg` from the ExEx |

The atomicity claim made by the spec is: **a tx's chain effects and DB
effects commit or revert together**. A DB-rejected payload reverts the
EVM tx atomically (via `Err(PrecompileError)` from the precompile), and
a successfully-processed tx's DB writes are durable iff the surrounding
block seals.

This is the central innovation of v2 over v1 (which had post-seal
asynchronous commit and could silently desynchronise chain and DB
state). It is also where the multi-execution problem bites, as §5 will
develop.

### 2.4 In-block state-root commitment

The custom `BlockExecutor` wraps `OpBlockExecutor` and overrides
`finish`. Inside `finish`, before reth computes the L3 state root for
block N, the wrapper:

1. Checks a precompile-set fatal flag; if set, returns `Err(...)` and
   the block does not seal.
2. Calls `arkiv_commitBlock(N, blockHash_N)` on the DB. The DB flushes
   per-block staging to PebbleDB atomically and returns `arkiv_stateRoot_N`.
3. Issues `evm.transact_system_call(SYSTEM_ADDRESS, ENTITY_REGISTRY_ADDRESS,
   abi.encode(N, arkiv_stateRoot_N))` to invoke
   `EntityRegistry.setArkivStateRoot(N, root)`.
4. Delegates to `inner.finish()`.

Because the system call's storage write completes before reth computes
the L3 state root, block N's state root *already* commits to
`arkiv_stateRoot_N`. There is no N+1 lag.

This pattern is exactly the EIP-4788 (parent beacon block root) shape:
a system call from inside `finish` writes a value into a known contract's
storage in the same block whose state root is about to be computed. The
v2 design is reusing a well-trodden reth pattern for a non-canonical
purpose.

---

## 3. The core challenge: out-of-band coupling

### 3.1 What "out-of-band" means in this context

"Out-of-band" here has a specific technical meaning. Every other
interaction the EVM has with state — `SLOAD`, `SSTORE`, `BALANCE`,
`CALL` into a contract, even reads of the block environment — happens
through channels the EVM owns and can reason about:

- The journalled state machine, with full revert / commit authority.
- The block environment, fixed at the start of the block.
- The transaction envelope, fixed at the start of the tx.

The Arkiv precompile's interaction with EntityDB is **not** through any
of these channels. It is over a TCP socket to a sibling process, via a
JSON-RPC protocol that lives outside the EVM's universe. The EVM has no
mechanism to:

- Roll the DB call back if the surrounding call frame reverts.
- Roll it back if the surrounding tx reverts after the call returns.
- Notice if the DB returned different bytes on a different node.
- Coordinate timing or ordering with the DB beyond the synchronous reply.

This is what "out-of-band" means: the channel is invisible to every
mechanism the EVM has for owning the consequences of mutations.

### 3.2 Reads vs writes — the load-bearing distinction

§1.4 noted that the EVM permits richer-than-calldata inputs as long as
they are consensus-deterministic. `SLOAD`, `BLOBHASH`, block-env opcodes
all qualify. None of them are pure functions of calldata, but all of
them are pure functions of consensus state.

The crucial thing every one of those constructs has in common: **the
act of consulting the value does not change anything observable**.
Calling `SLOAD` six times during the various execution paths reads the
same value six times and the world is identical to having called it
once. `BLOBHASH` is a read of the tx envelope; reading it is
inconsequential. `TIMESTAMP`, `COINBASE`, `BASEFEE` — all reads of
fixed block data; idempotent by nature of being reads.

The Arkiv precompile is doing something different. The natural reading
of the v2 spec is that `arkiv_applyTx` advances DB staging state — that
is, it has a side effect on something other than journalled EVM state.
Even within the carefully-engineered staging hierarchy, the precompile
is fundamentally a write operation against a state machine that lives
outside the EVM.

The asymmetry between reads and writes, in this context, is:

| Property | Read-style (e.g. `SLOAD`, `BLOBHASH`) | Write-style (Arkiv precompile) |
|---|---|---|
| Effect of repeated execution | Identical observation, no mutation | Each call mutates staging |
| Rollback authority | EVM journal (for `SLOAD`/`SSTORE`) | None inside the EVM; DB-internal only |
| Path independence | Same answer on every path | Each path leaves a trace |
| Failure semantics | Deterministic (read of consensus state) | Network failure is per-path, per-node |

Reads of consensus state are categorically safer than writes to external
state. This isn't a polemic point; it's the reason the canonical
precompile set has zero examples of out-of-band writes.

### 3.3 The EVM has no rollback authority over external state

The EVM's revert machinery is layered:

- **Call frame revert.** A `REVERT` opcode (or an exhausted gas budget,
  or a few other fault conditions) discards the current frame's journal
  entries.
- **Transaction revert.** If the top-level call ends in a revert, all
  state changes in the tx — across every nested call, every contract
  the tx touched — are unwound.
- **Block revert.** A failed block doesn't seal; nothing the block did
  is canonical.
- **Reorg.** A canonical block that gets replaced by a different chain
  has all of its state changes implicitly undone — the new canonical
  chain doesn't reference them.

Every one of these mechanisms operates on the journalled state. The
EVM has the journal entries, applies them on commit, throws them away
on revert. This is uncontroversial machinery; it is how the EVM has
worked since 2015.

A precompile that writes to an external system bypasses every layer:

- A `REVERT` opcode in the calling contract does **not** undo the DB
  call; the DB has already responded.
- A tx revert does **not** undo the DB call.
- A block that fails to seal does **not** undo any DB calls made during
  its execution.
- A reorg does **not** undo any DB calls made during the
  now-non-canonical chain.

The DB has to handle all of these revert events itself, on its own
schedule, with its own machinery. The v2 design's two-level staging is
an attempt to put DB-side machinery in place for the first three (with
the ExEx handling the fourth). This is the right kind of design move,
but the staging mechanism interacts with the multi-execution model in
ways the v2 spec doesn't yet address — which §5 develops.

### 3.4 The implicit assumption that has to hold

For the v2 design to be consensus-safe, an assumption has to hold:

> **EntityDB is part of the protocol.** Its state is a deterministic
> function of canonical chain history. Every honest node, given the
> same sequence of canonical blocks, reaches the same DB state. Any
> deviation is detected (because `arkiv_stateRoot` is committed
> on-chain).

This is a strong assumption. It implies:

- The DB has no non-determinism internally (no floating-point, no
  hash-map iteration order, no OS RNG, no wall-clock dependence).
- The DB is reorg-aware — its state can be unwound to a previous block
  and re-rolled forward differently, deterministically.
- The DB-side handling of multi-execution (§5) does not produce
  different state across nodes.
- The DB is available on every node (transport failure on one node and
  not another is a fork — the failing node returns `fatal`, the others
  return `ok`).

The v2 spec takes most of this as given (the DB-internal trajectory in
[`arkiv-storage-service/architecture.md`](../arkiv-storage-service/architecture.md)
covers determinism and reorg handling). What it doesn't yet address is
the multi-execution piece, which is the subject of the rest of this
document.

---

## 4. The multi-execution problem

### 4.1 Why the EVM re-executes the same transaction

It is tempting to think of "executing a transaction" as a singular event
— the user submits, the chain processes it once, end of story. The
reality is the opposite: from the moment a tx leaves the user's wallet
until it ends up in a fully-synced node's history, it is executed many
times, by many different code paths, for many different reasons. None
of these executions are spurious or removable; each serves a purpose
the chain cannot operate without.

This is true of every Ethereum-class client. It is not a reth-specific
implementation detail; it is intrinsic to how a permissionless,
gas-metered, mempool-driven blockchain works.

### 4.2 The execution paths in detail

The seven paths a single user-level transaction commonly traverses, in
roughly the order it encounters them:

#### Path 1: `eth_estimateGas`

Before the user signs the tx, their wallet (or `cast send`, or a dApp's
RPC client) typically calls `eth_estimateGas` to figure out an
appropriate gas limit. The node executes the tx against the current
pending state, observes how much gas it consumes, and returns that
number plus a safety margin.

Why it can't be skipped: gas is finite and pre-paid. A user-supplied
gas limit that's too low burns the user's gas without making progress.
Wallets that don't probe before signing are routinely hostile to users.

Where it touches the precompile: `EvmFactory::create_evm` is called to
build a fresh EVM for the estimation; that EVM has the Arkiv precompile
installed; the tx's call to `EntityRegistry.execute(...)` goes through
the precompile.

#### Path 2: Mempool admission

When the signed tx hits the node's mempool, the mempool validator
(`OpTransactionValidator` for OP-stack chains) partially executes it
to verify a number of properties: the sender has balance, the gas
limit is plausible against current state, no obvious pre-flight
faults. This is a real EVM execution against a snapshot of pending
state.

Why it can't be skipped: the alternative is admitting transactions
that are guaranteed to fail, which DoSes the mempool. Every Ethereum
client does some form of mempool-level pre-flight.

Where it touches the precompile: same — fresh EVM, precompile
installed, dispatch happens.

#### Path 3: Pending-block / `eth_call` against pending state

Tools and dApps that want to know "what would the result be if I
submitted X right now?" call `eth_call` against the pending block.
The node executes the call against pending state; if the call goes
through `EntityRegistry.execute(...)`, the precompile fires.

Why it can't be skipped: pending state is a useful query target; a
chain that doesn't expose it is much less ergonomic.

Where it touches the precompile: again, fresh EVM, precompile
installed.

#### Path 4: Block payload building

When the node is the proposer (or the block-builder, in a
proposer-builder split), it has to assemble candidate blocks. For each
candidate, every tx is executed against the candidate's state, in
order, with full receipts produced. This is the path that determines
which ordering of pending txs makes the best block.

Why it can't be skipped: this is *how* a block is assembled. There is
no shortcut.

Where it touches the precompile: every tx's execution during payload
construction goes through the precompile. The proposer builds at least
one candidate block per slot.

#### Path 5: Canonical block execution

Once a block is sealed and considered canonical (post-engine-API
acceptance), the block executor produces the canonical state diff for
that block. This is the "real" execution — the one whose state root
goes on chain.

Where it touches the precompile: canonical execution goes through the
precompile. This is the only path the Arkiv staging discipline is
explicitly designed for.

#### Path 6: Engine API validation

The engine API delivers blocks for validation: the consensus client
hands the execution client a payload, says "is this valid?", and
expects a yes/no plus the resulting state root. The execution client
re-executes the entire block to verify.

Why it can't be skipped: an execution client that doesn't validate
delivered payloads is trusting the consensus client implicitly, which
defeats the point of having two separately-implemented clients.

Where it touches the precompile: the validator constructs an EVM, the
EVM has the precompile, every tx in the payload runs through the
precompile.

#### Path 7: Reorg replay and `debug_traceBlock`

Reorgs cause blocks to be unwound and replaced; the new canonical chain
has to be executed forward from the common ancestor. Historical replay
(`debug_traceBlock`, snapshot regeneration, full sync from genesis)
re-executes blocks that have already been canonical.

Why it can't be skipped: reorgs are a normal, expected event on any
PoS chain (and OP-stack rollups). Historical replay is a debugging
necessity. Re-syncing nodes is operational reality.

Where it touches the precompile: every replayed block re-runs every tx
through the precompile.

### 4.3 Why the precompile fires on every path

All seven paths converge on `EvmFactory::create_evm`. There is no path
in reth that constructs an EVM by some other route. Therefore every
path's EVM has the same `PrecompilesMap`, including the Arkiv
precompile.

There is no signal in `PrecompileInput` or `EvmEnv` that distinguishes
"I am running inside the canonical block executor" from "I am running
inside a gas estimator". §6.2 returns to whether such discrimination
could be added; for the moment, the relevant fact is that **the
precompile cannot tell these paths apart**, and it must produce the
same return on all of them, or the chain forks.

### 4.4 Empirical evidence

The working POC ([`crates/arkiv-node/src/precompile.rs`](../crates/arkiv-node/src/precompile.rs))
is configured to log every JSON-RPC call from the precompile to a
mock-entitydb. A single `cast send` of `callPrecompile(0xdeadbeef)`
produces six byte-identical requests on the mock — same caller, same
calldata, same value, only the JSON-RPC envelope `id` advancing:

```json
{ "id": 77, "jsonrpc": "2.0", "method": "arkiv_precompileWrite",
  "params": [{ "caller": "0x9fe4…6e0", "data": "0xdeadbeef", "value": "0x0" }] }
{ "id": 78, …same params… }
{ "id": 79, …same params… }
…
{ "id": 82, …same params… }
```

This is not a bug in the POC. It is the structural property §4.2
described, manifesting in concrete telemetry. Each request corresponds
to one of the seven execution paths, in roughly the order reth invokes
them. See [`custom-precompile.md`](custom-precompile.md) §6.1 for the
full breakdown.

The working POC is intentionally tolerant of this duplication — the
mock returns a constant `0x000…0`, so all six calls produce identical
EVM-visible returns and no fork results. As §3.4 noted, this only works
because the mock is trivially deterministic. The moment EntityDB's
response or stored state depends on prior writes, the duplication
becomes consensus-affecting.

---

## 5. How multi-execution interacts with the v2 staging discipline

### 5.1 The hierarchy is sound for one linear execution stream

The two-level staging design (§2.3) is sound *if* the only execution
stream is canonical: payload build → `commitBlock` → durable. Trace it
through:

1. The proposer builds block N by executing each tx in order. For each
   tx: `arkiv_applyTx` writes to per-tx staging; on `ok`, promotes to
   per-block staging.
2. The proposer's `BlockExecutor::finish` calls `arkiv_commitBlock`. The
   DB flushes per-block staging to PebbleDB atomically.
3. The state root is system-called into `EntityRegistry`. Block N seals.

In this linear stream, every EVM event has a corresponding DB event:

| EVM event | DB event |
|---|---|
| `arkiv_applyTx` returns `ok` | per-tx staging exists |
| EVM tx commits | per-tx promotes to per-block |
| `arkiv_applyTx` returns `revert` | per-tx staging discarded |
| EVM tx reverts (after precompile returns ok) | ??? |
| `BlockExecutor::finish` runs | `arkiv_commitBlock` flushes per-block |
| Block doesn't seal | per-block staging is now in PebbleDB but no canonical block references its state root |

That penultimate row is already a question — it's not addressed by the
v2 spec — but in the canonical-only world it can probably be handled
by the staging machinery if the DB observes "the precompile returned
ok but no commitBlock was issued, so unwind" on some timeout or
session boundary.

In the canonical-only world, the design largely works.

### 5.2 It does not compose across the seven paths

The actual reth runtime does not have one execution stream. It has
seven (or more, depending on how you count `debug_traceBlock`,
`eth_call` against historical state, etc.). Each path constructs its
own EVM, dispatches through the same precompile, hits the same DB
endpoint.

The DB has *one* per-block staging area at a time. There is no
mechanism in the v2 spec for the DB to maintain seven separate
contexts and discard six of them at the right times. The staging
hierarchy was designed assuming a single-threaded canonical execution;
multi-execution is the case it doesn't model.

What actually happens:

1. Wallet calls `eth_estimateGas` for tx X. `arkiv_applyTx` writes to
   per-tx staging.
2. Wallet does not submit yet (or submits, no matter which).
3. The status returned to the precompile is `ok`; per-tx staging
   promotes to per-block staging.
4. ... and now per-block staging contains the effects of a tx that has
   not been included in any block.

Every subsequent path that hits `arkiv_applyTx` for the same tx (mempool
admission, pending-block queries, payload building, validation, eventual
canonical execution) tries to apply the same tx to a per-block staging
that already has its effects in it. What does the DB do?

- If it just appends to per-block staging, the same write happens N
  times and accumulates (problematic for any non-idempotent operation).
- If it deduplicates by some payload-derived key, it has to be careful
  not to dedupe legitimate identical user calls in different blocks
  (the problem from
  [`custom-precompile.md`](custom-precompile.md) §6.1 — distinguishing
  replays from legitimate replays).
- If it carries no awareness of canonical-vs-speculative, every
  speculative execution permanently dirties per-block staging.

None of these answers is in the v2 spec.

### 5.3 Concrete failure modes

Three concrete bad outcomes the v2 design as written admits:

#### Gas estimation pollutes per-block staging

A user calls `eth_estimateGas` for an `EntityRegistry.execute([create
0x...])`. The precompile dispatches `arkiv_applyTx`. The DB validates,
returns `ok`, the precompile returns to the EVM, the per-tx staging
promotes to per-block. The user looks at the gas estimate and decides
not to submit.

The next block's `arkiv_commitBlock` flushes per-block staging to
PebbleDB. The "create" the user never submitted is now durable. The
state root reflects an entity that no canonical transaction created.

Two nodes in the network may have wildly different gas-estimation
traffic patterns. They would compute different `arkiv_stateRoot`s.
They would fork.

#### Engine API validation produces a different state root

The proposer builds block N. Every tx's `arkiv_applyTx` writes to
per-block staging. The block is sealed and broadcast.

The validator on a different node receives the block. It re-executes
every tx, including their `arkiv_applyTx` calls. The validator's per-block
staging *also* has all those writes — but the validator started with the
canonical state at block N-1, not with the proposer's pre-build state.

If the proposer had had any prior pre-canonical traffic that polluted
its per-block staging (gas estimations, mempool admissions of txs not
in block N, etc.), the validator's `arkiv_stateRoot_N` will differ
from the proposer's. The block is rejected as invalid.

#### Reorg replay re-applies operations from staging that no longer exists

Block N is canonical, `arkiv_stateRoot_N` is committed, durable state
contains block N's writes. A reorg replaces block N with N'. The ExEx
calls `arkiv_revert` (or `arkiv_reorg`); the DB reverts to N-1's state
via repopulation from the trie.

The new chain's transactions need to be re-applied. The v2 spec says
the ExEx must "reconstruct the new chain's tx batches from logs (or
from the contract's own decoded calldata via receipts) because the
precompile's per-tx staging on the old chain has been discarded".

This is correct — but the same machinery that re-applies via `applyTx`
during reorg replay is the precompile path, which is also the path
that fires during gas estimation and mempool admission. The DB is now
fielding `applyTx` calls from two distinct contexts (reorg replay vs
ongoing pre-canonical traffic) with no way to tell them apart.

### 5.4 The atomicity claim is path-restricted

The v2 spec's atomicity claim — "DB effects commit or revert with the
EVM tx" — is true *along the canonical-execution path*. It is not true
across all paths. Specifically:

- A DB-side `revert` on the canonical path correctly rolls back the EVM
  tx. ✓
- A DB-side `revert` on the gas-estimation path causes the gas
  estimation to fail — but the tx was never going to be submitted, and
  no DB-side staging was supposed to land for it. The DB still saw the
  call though. ✗ (silent state pollution on `ok`)
- An EVM-side revert on the canonical path *after* the precompile
  returned `ok` does not roll back the per-tx staging promotion. ✗
  (already promoted to per-block)
- A block that fails to seal after `arkiv_commitBlock` succeeded leaves
  the DB durable state ahead of the chain. ✗ (chain↔DB drift)

The third item is particularly subtle: `arkiv_applyTx` returns `ok` and
the DB promotes per-tx → per-block staging. The EVM tx then reverts for
some other reason (e.g., a check in a separate contract called later
in the same tx). The DB has no way to know about that revert; it has
already promoted. When `arkiv_commitBlock` runs, the per-block staging
flushes to durable, and the chain has a tx that reverted but DB state
that reflects success. That is a silent atomicity violation.

The v2 spec's response to this would presumably be "the contract calls
the precompile *last*, after all its own checks pass, so post-precompile
EVM-side reverts are eliminated by construction". That works for the
specific shape of `EntityRegistry.execute(Op[])` if you can guarantee
no opcode in the tx after the precompile call can revert. In practice
this is a discipline, not a property the EVM enforces — and it does
not extend to txs that route to the precompile via other contracts.

---

## 6. Why this is hard to fix from inside the precompile

### 6.1 What the precompile can and cannot know

Recap §1.2 / §1.3 in the context of "what could we use to discriminate
paths":

The precompile knows: calldata, caller, value, target/bytecode address,
`is_static`, gas budget, journalled EVM state at the call site, block
environment (number, timestamp, base fee, ...).

The precompile does not know: which execution path called it, the
transaction hash (always — during pre-canonical paths the tx hash may
not be final), the position of the current call within the tx,
whether the tx will commit or revert, whether the block will seal.

There is no field on `PrecompileInput` that encodes "I am running
inside the canonical block executor". There is no API call the
precompile can make to ask the EVM "are you the executor?". There is
no journalled-state value reth populates differently per path.

### 6.2 The discrimination problem (briefly)

[`custom-precompile.md`](custom-precompile.md) §6.1.5 enumerates four
candidate mechanisms a precompile could use to discriminate paths and
why none of them is clean:

| Mechanism | Why it doesn't work cleanly |
|---|---|
| Static factory swap (different `EvmConfig` per path) | reth's node builder exposes one `ConfigureEvm` slot; routing requires upstream cooperation |
| `DB` type discrimination (downcast the database handle) | `Any` downcasts against reth-internal types; "canonical execution" isn't a single DB type |
| `BlockEnv` discrimination | gas estimation, payload building, canonical execution all target the same block number |
| Thread-local / atomic flag | re-execution paths set/clear it differently; concurrency breaks invariants |

The brief version: every mechanism that lets the precompile know
"which path am I in" couples consensus correctness to reth-internal
plumbing that changes across versions, or makes the precompile a
function of out-of-band state. Both are unacceptable.

### 6.3 Context-sensitivity violates the precompile contract

The deeper objection — beyond engineering brittleness — is the one §1.4
named: a precompile that varies its behaviour with hidden context is in
violation of the EVM's contract with it.

A precompile that writes only on canonical execution and is a no-op
elsewhere is computing different *behaviour* across paths, even if its
EVM-visible *return value* is identical on every path. The EVM contract
says "deterministic function of consensus state". Hidden context isn't
consensus state. It's not in the journal, not in the block env, not in
the tx envelope. Two nodes can disagree on it (one is doing gas
estimation right now, another is doing payload building) without
disagreeing on any consensus input.

The escape hatch is narrow but real: if the EVM-visible return is
*completely* independent of whether the side effect happened, then
varying the side effect across paths is consistent with the contract.
The catch is that the precompile then can't return anything that
depends on the DB write having taken effect — no row IDs, no "current
size", no "newly-allocated key". And if the EVM-visible return is
purely a function of input, the side effect is logically detached from
the EVM result, which is exactly the situation where moving the side
effect to the ExEx (where it already runs once per canonical block by
construction) would be cleaner.

This is the design pressure behind §7.5.

---

## 7. Resolution shapes and their trade-offs

There are no "clean" answers — only design moves that trade one set of
constraints against another. Five shapes are worth naming.

### 7.1 Session-id-aware DB

Make `arkiv_applyTx` carry a session ID; have the DB maintain N
parallel staging contexts, one per session, with discard semantics
attached to each.

The session ID has to come from somewhere. Candidates:

- The `EvmFactory` sets it when creating the EVM. But then *something
  upstream* — reth's node builder — has to route different sessions to
  different paths, and that upstream signal does not currently exist.
- The block executor sets it at start of canonical execution; gas
  estimation defaults to a "discard" session. But this requires
  threading session IDs through every reth path the precompile touches,
  including gas estimators that have no concept of sessions today.

**Trade-off:** clean conceptual model, mismatched against what reth
currently exposes. Most invasive of the options.

### 7.2 Idempotent staging keyed by consensus-stable identifiers

`arkiv_applyTx` is keyed by `(blockNumber, txHash, opIndex)` or similar.
The DB checks the key on every call: if a key has already been seen, the
call is a no-op (returns the same status as the first call). Multi-execution
becomes harmless because every replay collapses to the same staging
record.

The challenge: `txHash` is not always available at `applyTx` time. During
gas estimation, the tx hasn't been signed; it has no canonical hash. The
key would have to be something computable from the call context that
*happens* to be the same on every execution path that is going to apply
this op — which is essentially the same thing as "consensus-stable
identifier", and it is hard to construct one without information the
precompile doesn't currently get.

A weaker version: use `(blockNumber, sender, callerNonce, opPayloadHash)`.
This is consensus-stable for canonical-path executions but doesn't help
for gas estimation (where the nonce isn't yet committed) or pending-block
queries (same).

**Trade-off:** lighter-weight than 7.1, but the key construction is
fiddly and requires the precompile to receive context it doesn't get
today. Requires plumbing changes to reth (or to alloy-evm's
`PrecompileInput`).

### 7.3 `EvmFactory` routing per call site

Reth exposes hooks for different EVM configurations per call site: one
factory for canonical execution (with the real DB endpoint), another for
gas estimation / mempool / pending-block queries (with a no-op DB
endpoint, or no precompile at all).

The good news: this is conceptually clean. Canonical execution writes to
the DB; everything else doesn't touch it. The bad news: reth doesn't
have this routing today. The `ConfigureEvm` slot on the node builder is
single. Adding distinct slots is a non-trivial upstream change.

A workaround: the proposer/builder runs a separate node process from the
RPC node. The RPC node has no DB endpoint configured (precompile is a
no-op or returns a synthetic response). The proposer node has the real
DB. They co-locate but don't share the precompile path. This pushes the
problem out of code into deployment.

**Trade-off:** clean if upstream cooperates, deployment-fragile if it
doesn't. Doesn't address the canonical-execution / engine-validation
duplication on the same node — both paths are on the proposer.

### 7.4 Full DB idempotency with semantic restrictions

Make every `arkiv_applyTx` operation intrinsically idempotent: applying
it twice produces the same DB state as applying it once. Combined with
deterministic keying, multi-execution is harmless because every replay
converges on the same state.

This works for set-style operations (`create`, `update`, `transfer`,
`extend`, `delete`, `expire` — the v2 op set, which is set-style by
construction). It does not work for append-style operations (event
logging, counter increment, sequence allocation).

The reorg / speculative-execution problem (§3.3) is orthogonal to
idempotency — even idempotent writes leak if the surrounding tx isn't
canonical. To address that, the DB needs chain-awareness in addition to
idempotency.

**Trade-off:** lightest implementation cost, hard semantic restriction
on what the precompile can do. The v2 op set fits. Future ops that don't
fit (a per-entity event stream, say) would have to live elsewhere.

### 7.5 Read-only precompile + ExEx-side writes

The precompile becomes pure: it reads DB state at the parent block (or
journalled EVM state via `EvmInternals`), computes whatever the EVM
caller needs, and returns. It does not advance DB state. The actual
writes happen via the ExEx, post-canonical, exactly as v1's design
specified.

To preserve v2's in-block `arkiv_stateRoot` commitment, the precompile
must be able to compute "what the state root *would* be after this tx's
ops" deterministically from the prior root and the ops alone. This is
feasible for any DB whose state-root computation is itself a pure
function of (prior root, op batch) — which Merkle-root-style DBs all
are, given access to the relevant subtrees.

The price: the precompile needs read access to enough DB state to
compute the new root. Either the DB is embedded in-process (so reads
are cheap and synchronous), or the precompile is OK with making a
read-only IPC call to the DB per `applyTx`. The latter is the same
shape as the current write call but without the side effect.

**Trade-off:** sidesteps the entire multi-execution problem at the cost
of either embedding the DB in-process or restricting precompile work
to what's computable from a one-shot read. Maintains v1's clean
separation of "EVM does EVM things, ExEx does DB writes".

### 7.6 Comparison

| Shape | Multi-exec safe? | Reorg-safe? | Plumbing cost | Semantic restriction |
|---|---|---|---|---|
| 7.1 Session-aware | Yes | With chain-aware sessions | High (reth changes) | None |
| 7.2 Consensus-keyed idempotency | Yes | With chain-aware DB | Medium (txHash plumbing) | None |
| 7.3 `EvmFactory` routing | Yes (proposer-only) | Same as canonical-only design | High (reth changes) or operational | None |
| 7.4 Full idempotency + semantic | Yes (with caveats) | Needs chain-awareness | Low | Set-style ops only |
| 7.5 Read-only precompile + ExEx writes | Yes (by construction) | Same as v1 | Low (precompile rewrite) | Compute-from-state-only |

Combinations are possible. 7.4 + 7.5 ("the precompile is read-only
*and* the DB is fully idempotent") is the most defensive shape: even
read-only precompile traffic is harmless to the DB, even pre-canonical
reads are well-defined.

---

## 8. The deeper question: can a sibling DB be in the consensus envelope?

The whole problem reduces to one question: **is EntityDB part of the
protocol or not?**

If yes — every node runs an instance, its state is a deterministic
function of canonical chain history, transport between EVM and DB is
reliable, the DB is reorg-aware, idempotent, and embedded — then the
Arkiv precompile is *morally* like `BLOBHASH`: it reads (or writes
deterministically into) a richer state envelope than calldata, but
every node deterministically agrees on what that envelope contains.
The chain is safe.

If no — the DB is "an off-chain index" the node happens to talk to, as
in v1's framing — then the precompile shouldn't exist. The ExEx is the
appropriate channel, because the ExEx runs once per canonical commit
and is the only mechanism a sibling-process consumer can rely on
without inheriting the multi-execution problem.

The v2 design bets on the first answer. The bet is reasonable, but it
implies a *list* of properties the DB has to provide — determinism,
reorg-awareness, idempotency, in-process embedding (probably),
multi-execution tolerance — and the v2 spec, as currently written,
makes most of them but not the last. That last property is what this
document has been about.

The choice between resolution shapes in §7 is essentially a choice about
*how* to provide multi-execution tolerance. None of them eliminates the
need for the other DB properties; all of them assume those are in place.

---

## 9. Open questions

The following are concrete open questions §12 of the v2 spec should
absorb.

1. **Multi-execution tolerance.** Which of the §7 shapes does the design
   commit to? The current spec assumes single-stream canonical execution
   and does not address gas estimation, mempool admission, pending-block
   queries, engine-API validation, reorg replay, or `debug_traceBlock`.
   This is the gap.

2. **Atomicity-after-precompile-returns.** What happens if `arkiv_applyTx`
   returns `ok`, the precompile promotes per-tx → per-block staging, and
   the surrounding EVM tx then reverts for an unrelated reason
   (out-of-gas later in the tx, a separate `require` failure in another
   contract called after `EntityRegistry.execute`)? The current spec
   implicitly assumes this can't happen because `EntityRegistry.execute`
   is the last thing the tx does, but that is a discipline, not an
   enforced property.

3. **Transport failures and per-node forks.** Network failure between
   reth and EntityDB on one node and not another causes the failing
   node to return `fatal` while others return `ok`. This is a fork at
   the receipt level even with a perfectly deterministic DB. The v2
   spec acknowledges `fatal` halts the block but doesn't address the
   "different nodes' DBs are reachable at different rates" case
   directly.

4. **Bootstrap and pruning** (already in v2 §12.1, §12.3). Linked here
   because the resolution chosen for multi-execution tolerance affects
   what bootstrap has to reproduce — full per-tx staging history, just
   per-block, just durable, etc.

---

## 10. Glossary

- **Canonical execution.** The block executor's run of a sealed,
  validated, accepted block, producing the chain-of-record state diff.
  Distinct from speculative execution paths (gas estimation, mempool
  admission, payload building before sealing, validation).
- **Consensus envelope.** The set of state every honest node
  deterministically agrees on. Includes journalled EVM state, block
  header, tx envelope, chain config. Excludes wall clock, OS RNG,
  sibling-process state (unless the sibling is itself a deterministic
  function of consensus inputs, by construction).
- **EvmFactory / `create_evm`.** The reth/alloy-evm hook that produces
  a fresh EVM instance with its `PrecompilesMap` populated. Called
  once per execution-path entry.
- **`PrecompilesMap`.** The mutable container of precompiles attached
  to a specific EVM instance. The EVM consults it on every call.
- **`PrecompileInput`.** The struct passed to a precompile's `call`
  method. See §1.2 for fields.
- **`EvmInternals`.** The API surface inside `PrecompileInput` that
  lets the precompile read (and write) journalled EVM state.
- **Out-of-band state.** State that is not in the consensus envelope.
  Reads of it are per-node-different in general; writes to it are
  invisible to the EVM's revert machinery.
- **Per-tx / per-block / durable staging.** EntityDB's three-level
  write hierarchy. Per-tx is in-memory and discardable; per-block is
  in-memory accumulator (`CacheStore`); durable is PebbleDB.
- **`arkiv_applyTx` / `arkiv_commitBlock`.** The two JSON-RPC methods
  the precompile and the custom `BlockExecutor` use to talk to
  EntityDB. See v2 spec §6.
- **Multi-execution.** The property that a single user-level
  transaction is executed many times across reth's various paths. See
  §4.

---
