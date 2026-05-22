# Optimism sequencer feasibility for out-of-process DB CRUD via precompiles

A formal scrutiny of the hypothesis that the leak surface which makes
a side-effecting precompile unsafe in op-reth can be collapsed away by
restricting the deployment to an Optimism sequencer in a stripped-down
configuration. Evidence drawn from `repos/op-reth` (v1.1.5),
`repos/optimism/op-node` (`develop`), and the upstream reth tree.

This document is intended to stand alone. The technical foundations
that earlier analyses in this project established — what a precompile
is, why it is invoked multiple times per transaction, what the
alternative remedy looks like — are restated in §6 before the
formal deductions begin.

The first three sections layer the conclusion at three depths:
**§1 Abstract** is one paragraph for the executive reader; **§2
TL;DR** is a structured summary with the leaks named and the
recommendations branched; **§3 Setup** prepares the framing for the
formal sections that follow.

---

## 1. Abstract

This document tests whether out-of-process database CRUD via a
side-effecting precompile in **op-reth** can be made safe by
restricting the deployment to a pure Optimism sequencer that
exposes no public RPC, builds each block at most once, and runs
no validation pipeline.

**Scope.** The analysis is specifically about op-reth (v1.1.5)
and op-node (`develop`), not generic reth. The two diverge in
operationally significant ways — chiefly op-reth's
`InsertExecutedBlock` race wired into the launcher (a
sequencer-friendly optimisation absent from upstream) and
op-node's sequencer-driven engine-API roundtrips. But the
central Achilles heel persists across both: **the StateDB's
forkable, ephemeral, plural lifecycle is a property of reth's
architecture, inherited by every downstream consumer along with
all the leak paths it enables.** No downstream fork — op-reth,
base-reth, taiko-reth, or any future descendant — escapes this
without surgery on the parent.

**Precompiles and augmented-execution pathways are the same
object for this analysis.** A prior project attempt on op-geth
used an *augmented execution pathway* — a transaction
interceptor that diverts calls to a designated address out of
the EVM and into a sidecar process. This document treats that
earlier design and the present precompile design as
**functionally identical for the purposes of this analysis.**
Both are leaf-level interception points; both are called by
every site that constructs an EVM; both inherit the calling
context from whichever site dispatched them; both face exactly
the same plural-invocation, reorg, and engine-API-roundtrip
pathologies. Switching the execution client (reth → geth) or
the mechanism (precompile → sidecar) moves the implementation
but not the design problem. **The trouble is with the OP
protocol itself**, not with any specific execution-client or
interception-mechanism implementation. Every conclusion below
about "the precompile" applies, unchanged, to a sidecar
augmented-execution pathway in op-geth.

Two facts decide the hypothesis before any of its three
conditions is individually examined.

**The velocity mismatch.** The EVM's `StateDB` is engineered to
be created, forked, and discarded cheaply on every speculative
path — gas estimation, `try_build` retries, engine prewarming,
reorg replay, simulation. An external database that the
precompile mutates has none of those lifecycle properties: it is
single-master, durable on first write, and cannot keep pace with
the StateDB's branching. Every leak path documented in this
report is, fundamentally, a place where the StateDB forks but
EntityDB cannot follow.

**Reorg simulation has no off-switch.** `create_reorg_head` in
op-reth (inherited from upstream reth) constructs an EVM on
every chain reset. No CLI flag, no build option, no
rollup-config setting disables it. Even granting the
hypothesis's three conditions in full, this one path fires the
side-effecting precompile against branches the chain then
discards. The hypothesis fails on this point alone, before the
other leaks are tallied.

The other failures compound rather than rescue: `try_build`
retries are configurable but not contractually one-per-slot;
op-node mandates an `engine_newPayload` roundtrip back to the
same EL, mitigated by a tokio race that is not a contract; the
invalid-block hook is an adversarial trigger.

A deeper problem subsumes all of the above. The hypothesis
silently presumes the precompile is **Ornamental** (writes never
read back by the EVM), but **the project's actual requirements
force Accessible from day one** (reads see prior writes within
the same block). The DApp ecosystem this chain must host —
Uniswap-style AMMs reading reserves after a swap, Aave-style
lending reading collateral after a deposit, ERC-4626 vaults
reading share ratios after a mint, same-block MEV bundles — all
depend on same-block read-after-write of state. Any DB-backed
analogue of those dapps needs the same semantics. The Arkiv
precompile is Accessible by design, not Ornamental that
happens to need a read primitive added later.

For Accessible precompiles, neither sequencer-restriction nor
the previously-recommended **Pure-twin-remedy** applies. Of the
remaining options, three more are rejected on independent
grounds:

- **Cross-block commit-reveal** is rejected on
  DApp-compatibility grounds: any read latency breaks the
  same DApps the chain exists to host; one block of staleness
  is enough to make AMM pricing wrong on the second leg of a
  same-block trade.
- **Journaling the data into EVM state** is not a redesign of
  EntityDB but its dissolution. EntityDB exists precisely
  because raw EVM storage is the wrong substrate — no schema,
  no query model, no indexing. Putting the project's data
  into EVM storage is "build on Ethereum" with extra steps;
  the project's purpose ceases to exist.
- **Hard-forking op-reth and externally imposing per-retry DB
  snapshots** lacks theoretical backing. No one has designed
  how DB snapshots interact with the launcher's tokio race,
  the validator's `evm_with_env` path, or the reorg
  simulator. The proposal is a sketch, not a design.

That leaves **exactly one viable option: lifecycle-couple
EntityDB to StateDB, with EntityDB embedded in the reth
process.** EntityDB's lifecycle mirrors StateDB's — every EVM
construction gets a fresh EntityDB fork off canonical state;
forks are cheap to create (copy-on-write), dropped silently
when their StateDB drops, committed atomically when their
StateDB commits. The critical constraint is **in-process
embedding.** Out-of-process EntityDB cannot satisfy the
cheap-fork requirement (every fork would be an IPC
roundtrip); cannot guarantee atomic commit between StateDB
and EntityDB on canonical execution; cannot reliably drop
EntityDB forks alongside discarded StateDB forks. The
architectural consequence: **EntityDB becomes a Rust library
that reth links against, not a separate service.**
Independent consumers must query through reth's RPC or via a
read-only embedded instance. With that constraint accepted,
the velocity-conformance problem is solved cleanly at the DB
layer with one-time engineering and no op-reth fork. Without
it, full Accessible CRUD inside a single block is **not
safely achievable in op-reth**.

---

## 2. TL;DR

For readers who want the structured summary without the deductions:

- **Hypothesis tested.** Can out-of-process database CRUD via a
  side-effecting precompile **in op-reth** be made safe by
  restricting the deployment to a pure Optimism sequencer
  (no RPC, no speculative builds, no validation pipeline, only one
  block built per height and no more)?

- **Scope.** Specifically about op-reth (v1.1.5) and op-node
  (`develop`), not generic reth. But the central Achilles heel
  — the StateDB's forkable/ephemeral/plural lifecycle — is a
  property of reth's architecture, inherited by every
  downstream client (op-reth, base-reth, …) without escape.

- **Precompiles ≡ augmented-execution pathways.** The prior
  op-geth attempt (a transaction interceptor routing calls to
  a sidecar) and the present precompile are treated here as
  the same object. Both are leaves invoked by every site that
  constructs an EVM; both inherit calling context they cannot
  see; both face identical pathologies. The trouble is with
  the OP protocol itself — not with reth vs geth, not with
  precompile vs sidecar. Switching either dimension moves the
  implementation but not the problem.

- **Short answer: no.** Two arguments are sufficient on their
  own:

  1. **Velocity mismatch between StateDB and EntityDB.** The
     EVM's `StateDB` is engineered to fork cheaply on every
     speculative path — `try_build` retries, prewarming, reorg
     replay, simulation — and discard those forks silently when
     they lose or finish. An external EntityDB cannot keep pace
     with that lifecycle; it is single-master and durable on
     first write. Every leak documented later in this report is,
     at root, a place where the StateDB branches but EntityDB
     cannot follow.
  2. **Reorg simulation has no off-switch.** `create_reorg_head`
     in op-reth constructs an EVM on every chain reset, with no
     CLI flag, no build option, and no rollup-config setting
     disabling it. Even if every other condition of the
     hypothesis were satisfied, this one path fires the
     side-effecting precompile against branches the chain then
     discards. The hypothesis fails on this point alone.

- **Details that compound the same problem.** Three further
  leaks on the deployment side:
  1. Op-reth's payload builder retries `try_build` on a timer
     until the slot deadline. Each retry fires the precompile.
     Tunable to approximately-one-per-slot via
     `--builder.interval`, not contractually one.
  2. Op-node *requires* the engine API roundtrip: after sealing,
     the sequencer calls `engine_newPayload` back to its own EL.
     Op-reth has an internal optimisation (`InsertExecutedBlock`)
     that races to skip the re-execution and usually wins, but
     the optimisation is a tokio race rather than a contract.
  3. Invalid-block hooks fire the precompile against rejected
     blocks under adversarial input.

- **The deeper problem.** The hypothesis implicitly assumes the
  precompile is *Ornamental* (writes only; the EVM never reads
  back what was written). **Ornamental was never the project's
  target.** The chain must host DApps that read same-block
  state — Uniswap reserves after a swap, Aave collateral after
  a deposit, ERC-4626 share ratios after a mint, same-block MEV
  bundles — and any DB-backed analogue of those reads inside
  the same block forces the precompile to be *Accessible*
  (reads see prior writes) from day one. The Arkiv precompile
  is Accessible by design. The recommended **Pure-twin-remedy**
  covers Ornamental only; neither it nor sequencer-restriction
  works for Accessible in vanilla op-reth.

- **Pragmatic recommendation.** Of five superficially-available
  paths, four are rejected; only one survives:
  - ❌ ~~Keep the precompile Ornamental and apply the
    **Pure-twin-remedy**.~~ *Rejected: the DApp argument above
    forces Accessible from day one. The Ornamental path is
    structurally unavailable to this project.*
  - ❌ ~~Cross-block commit-reveal for Accessible operations.~~
    *Rejected: same-block reads of reserves, prices, vault
    shares are a hard DApp requirement (Uniswap, Aave,
    ERC-4626). Any read latency breaks the ecosystem this
    chain exists to host. One block of staleness is enough.*
  - ❌ ~~Journal the data into EVM state.~~ *Rejected on
    architectural grounds: this is not a redesign of EntityDB
    but its dissolution. EntityDB exists precisely because
    raw EVM storage is the wrong substrate (no schema, no
    query model, no indexing); replacing EntityDB with EVM
    storage is "build on Ethereum" vanilla. The project's
    purpose ceases to exist.*
  - ❌ ~~Hard-fork op-reth with externally-imposed per-retry
    DB snapshots.~~ *Rejected on theoretical grounds: no one
    has designed how DB snapshots interact with the
    launcher's tokio race, the validator path, or the reorg
    simulator. The proposal is a sketch, not a design — and
    accumulates substantial recurring maintenance cost on
    every reth/op-reth bump for a foundation that has not
    been laid.*
  - ✅ **Lifecycle-couple EntityDB to StateDB, with EntityDB
    embedded in the reth process.** Re-engineer EntityDB so
    that every EVM construction gets its own EntityDB fork
    off canonical state — cheap to create (copy-on-write),
    dropped silently when its StateDB drops, committed
    atomically when its StateDB commits. **Critical
    constraint: in-process embedding.** Out-of-process
    EntityDB cannot satisfy the cheap-fork requirement (every
    fork would be an IPC roundtrip); cannot guarantee atomic
    commit between StateDB and EntityDB on canonical
    execution; cannot reliably drop EntityDB forks alongside
    discarded StateDB forks. **EntityDB becomes a Rust
    library that reth links against, not a separate
    service.** Independent consumers must query through
    reth's RPC or via a read-only embedded instance. The
    architectural change is non-trivial but the work is
    one-time, confined to the DB layer, and independent of
    reth/op-reth release cadence.

- **What scrutiny revealed about the hypothesis.** Brittle from
  premise to conclusion. The framing intuition (restrict the
  call sites; don't ask the leaf to discriminate) is the right
  *shape* for a remedy, but the hypothesis fails on at least
  five independent counts:

  1. **Reorg simulation has no off-switch.** This is sufficient
     on its own. `create_reorg_head` is upstream-reth code with
     no CLI flag or build option to disable it, and reorgs are
     a structural fact of OP rollup operation.
  2. **The validation pipeline cannot be turned off.** The
     sequencer itself triggers `engine_newPayload` back to its
     own EL after sealing; the engine API is the only channel
     by which op-node informs the EL that a block is canonical.
     Removing the call would remove canonicalization itself.
  3. **Speculative builds cannot be suppressed structurally,
     only operationally.** `BasicPayloadJob`'s retry loop is
     the builder's fee-maximisation mechanism; reth offers no
     single-shot toggle. Tuning `--builder.interval` to
     approximate one-per-slot is the best available, and it
     is an operational approximation, not a contract.
  4. **The trouble is the OP protocol, not the execution
     client.** Switching to op-geth and an augmented-execution
     pathway preserves every failure mode above — the
     leaf-level interception design is isomorphic, and the
     engine-API roundtrip plus reorg semantics are protocol
     facts, not implementation choices. "Use a different EL"
     and "use a different interception mechanism" are not
     remedies.
  5. **The Ornamental/Accessible distinction is missed
     entirely.** Even if every leak were sealed, the
     hypothesis would still not cover Arkiv's actual
     requirement (Accessible CRUD for DApp compatibility).
     The Pure-twin-remedy and sequencer-restriction both fail
     on this dimension regardless of how successfully the
     leaks are addressed.

  The partly-correct details (mempool and RPC genuinely do go
  quiet on op-reth) narrow the leak surface but do not close
  it. The right framing of the hypothesis's status is not
  "a promising remedy with a few holes" but **"a hypothesis
  that could not have been correct in any system that runs
  the OP Engine API and supports chain reorgs."**

The rest of the document is the working-out.

---

## 3. Setup

A precompile that mutates an external database is a fragile thing.
The EVM, indifferent to consequence, will invoke it as many times
as its internal economy demands — once for gas estimation, once
for mempool admission, once for the speculative build, once for
the validating re-execution, once for the engine API's verifying
re-execution, once for historical replay. The side effect, blind
to which invocation is the canonical one, multiplies. The chain
forks, or — worse — drifts into a quiet desynchronization that
nobody notices until it is already irrecoverable.

One remedy, well documented in prior work, lives at the level of
the EVM factory: install the side-effecting variant only on the
build path, install a pure twin everywhere else, mint a per-trial
UUID, and reconcile at the moment the consensus client claims the
canonical payload. The hypothesis examined here is more austere:
not to discriminate at the factory, but to remove the impure call
sites from existence by running only a sequencer — no RPC, no
validators, no speculative candidate blocks.

This document tests whether op-reth and op-node, as they exist
today, will tolerate that configuration.

---

## 4. Velocity conformance: StateDB ⇔ EntityDB

This is the framing on which the entire analysis rests. The
hypothesis can be falsified without naming a single specific leak
once the mismatch described here is grasped — every concrete leak
in §8 onward is a corollary of it.

The EVM's `StateDB` is the canonical model of "world state that
needs to support speculative work":

- It is **created cheaply.** Every EVM construction calls
  `EvmFactory::create_evm`, which clones a state provider over the
  parent block's committed state.
- It is **forked aggressively.** Every `try_build` retry, every
  `eth_call`, every `debug_traceTransaction`, every engine prewarm
  fires a fresh fork off the same parent. None of these forks see
  each other; each is a self-contained universe.
- It is **discarded silently.** Losing `try_build` retries,
  finished simulations, completed traces all drop their state on
  the floor. Nothing in the EVM contract requires a fork to ever
  commit.

`StateDB` is built for this lifecycle. The bundle-state
machinery, the journaled storage, the `State<DB>` cursor over
`StateProvider` — all of it exists so that creating, branching,
advancing, and discarding state is a constant-cost operation in
the hot path.

An external database that the precompile mutates is not built for
any of this. EntityDB lives in real wall-clock time. It has a
single mutable cursor, no forks, no notion of "this write was
speculative and will be discarded if the current `try_build`
loses." Writes are durable the moment they are applied. Reads see
whatever the cursor currently shows.

> **The velocity-conformance problem.** A side-effecting precompile
> connects two worlds whose state-lifecycle properties do not
> match. The StateDB world is forkable, ephemeral, plural. The
> EntityDB world is linear, durable, single-master. The precompile
> is the chokepoint where the mismatch becomes a consensus hazard.

Every leak path documented later in this report (the `try_build`
retries, the `InsertExecutedBlock` race, the reorg simulation, the
adversarial-block hook) is a place where the StateDB is forked or
discarded but EntityDB cannot keep up.

> **The unkillable leak.** Among those leaks, one is decisive on
> its own. Op-reth's `create_reorg_head` (in
> `crates/engine/util/src/reorg.rs`) constructs an EVM on every
> chain reset and replays transactions on the discarded branch.
> *No CLI flag, no build feature, no rollup-config setting turns
> this off.* The function exists because reorgs are part of
> normal chain operation and op-reth needs to simulate the
> reorg'd branch to compute the diff. The function exists,
> therefore the precompile fires on branches the chain has
> abandoned, therefore EntityDB receives writes against
> non-canonical state, therefore velocity conformance is
> violated. The remedy is not a flag — it is a fork.

Every remedy the document considers is, at heart, an attempt to
restore velocity conformance:

- **Pure-twin-remedy** fakes conformance by banning reads on
  speculative forks. The pure twin returns the same bytes
  regardless of whether prior writes occurred, so the EntityDB's
  inability to fork is invisible to the EVM. Conformance is
  achieved by making the external state irrelevant to consensus —
  which only works for Ornamental precompiles.

- **Commit-reveal** would restore conformance by delaying reads
  by one block: by the time block N+1 needs to see what block N
  wrote, block N has been committed and EntityDB has caught up.
  EntityDB only has to keep pace with canonical progression, not
  with speculative branches. This is *technically* conformance-
  restoring but is **not viable for Arkiv** — same-block
  read-after-write is a hard DApp requirement (Uniswap reserves,
  Aave collateral, ERC-4626 share ratios, MEV bundles), and any
  read latency breaks the dapp surface the chain exists to host.

- **Journaling into EVM state** restores conformance by moving
  the external store inside the StateDB. The data being CRUD'd
  inherits all of StateDB's lifecycle properties. EntityDB
  stops being an external thing — *which means EntityDB stops
  being EntityDB.* For Arkiv, this is the dissolution of the
  project, not a redesign: raw EVM storage is the wrong
  substrate (no schema, no query model, no indexing), which is
  why the project chose to build EntityDB in the first place.

- **Lifecycle-coupled EntityDB (in-process)** restores
  conformance by re-engineering EntityDB to match StateDB's
  velocity. Each EVM construction gets its own EntityDB fork —
  cheap to create, copy-on-write under reads, dropped or
  committed alongside its StateDB. Conformance is achieved by
  making EntityDB forkable, not by making the EL non-forkable.
  Preserves the external-DB framing nominally — EntityDB still
  has its own schema, query model, and indexing — but requires
  EntityDB to be **embedded in the reth process.**
  Out-of-process operation cannot satisfy the cheap-fork
  requirement (every fork would be an IPC roundtrip), cannot
  guarantee atomic StateDB↔EntityDB commit on canonical
  execution, and cannot reliably drop EntityDB forks alongside
  discarded StateDB forks. **EntityDB becomes a Rust library
  reth links against, not a separate service.** This is the
  only conformance-restoring strategy that survives Arkiv's
  combined DApp-compatibility and external-DB-framing
  constraints.

- **Sequencer-restriction-suffices** attempts conformance by
  eliminating the speculative branches that EntityDB cannot
  follow — if there are no `eth_call`s, no `try_build` retries,
  no validation re-executions, then there is only one fork
  EntityDB needs to keep pace with. The body of this document
  tests whether that elimination is achievable in op-reth, and
  finds it is not.

Once the velocity-conformance lens is in place, the rest of the
document is the working-out of where op-reth's StateDB lifecycle
exceeds EntityDB's, and what kinds of conformance-restoring
remedies survive contact with the system.

---

## 5. Hypothesis

> **Sequencer-restriction-suffices.** Out-of-process database CRUD
> via a side-effecting precompile is safe if and only if the
> deployment is restricted to a pure Optimism sequencer that
> (a) exposes no public RPC, (b) builds each block at most once
> (no speculative `try_build` retries), and (c) runs no validation
> pipeline. Under these three conditions, the precompile fires
> exactly once per canonical block, and the external database
> state remains in lockstep with the chain.

The named conditions of this hypothesis are referred to throughout
as **(a) no-RPC**, **(b) one-build-per-block**, and
**(c) no-validation**.

---

## 6. Foundations

The deductions that follow depend on four prior findings. Each is
self-contained here so that the present document can be read
without the supporting analyses. Each foundation is named so that
later sections can refer back to it by intent.

### Precompile-anatomy — what a precompile is, mechanically

A precompile is a fixed-address handler the EVM consults *instead of*
executing bytecode at that address. It receives the call's calldata,
gas, caller, value, and a small bundle of contextual fields, and
returns either output bytes (with gas accounting) or a halt status.
In the contemporary `alloy-evm` / `revm-precompile` interface, the
trait is roughly:

```rust
pub trait Precompile {
    fn precompile_id(&self) -> &PrecompileId;
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult;
    fn supports_caching(&self) -> bool { true }
}

pub struct PrecompileInput<'a> {
    pub data: &'a [u8],          // calldata
    pub gas: u64,
    pub caller: Address,
    pub value: U256,
    pub target_address: Address,
    pub is_static: bool,
    pub bytecode_address: Address,
    pub internals: EvmInternals<'a>, // hooks into journaled EVM state
    // … a few more for EIP-8037
}
```

`PrecompileInput` is a fixed struct. It carries no caller-identity
field, no execution-context tag, no escape hatch. `EvmInternals` lets
the precompile read journaled EVM state (balances, code, storage)
but does not reveal anything about *who* invoked the EVM that is
running this call. **The precompile is structurally blind to its
calling context.**

A precompile installed in the canonical binary is part of the
state-transition function. Every node must agree on its presence and
its return value, or the chain forks at the first block that invokes
it. Side effects that are *not* part of the EVM-visible return —
HTTP calls, database writes, channel sends — are invisible to the
EVM and to consensus. They are unconstrained by the precompile's
contract, but also unprotected by it.

### Six-executions — the plural-invocation discovery

Empirical fact, observed against a local mock EntityDB: **a single
`cast send` of one user-level transaction fires the precompile six
times in the life of a normal reth node.** The mock recorded six
byte-identical `arkiv_precompileWrite` requests for one logical user
action, with only the JSON-RPC envelope `id` advancing between them.

The six paths, in roughly the order reth invokes them:

| # | Path | What runs the precompile |
|---|---|---|
| 1 | `eth_estimateGas` | A wallet (cast in this case) probes for a gas limit before signing. |
| 2 | Mempool admission | The pool partial-executes the tx to validate its declared gas. |
| 3 | Pending-block / state computation | `ConfigureEvm` builds pending state for queries that follow. |
| 4 | Block payload building | The payload builder runs the tx while assembling the next block. |
| 5 | Canonical block execution | The block executor produces the canonical state diff. |
| 6 | Engine API validation | The engine validator re-executes the block during forkchoice update. |

All six paths construct an EVM via `EvmFactory::create_evm` (or
`create_evm_with_inspector`). A custom factory that installs a
precompile in every EVM it produces is therefore dispatching the
precompile six times per logical user action.

This is **not a bug in reth.** Each path needs the precompile
installed — a precompile that no-ops in `eth_estimateGas` reports
the wrong gas; a precompile that no-ops in engine-API validation
accepts blocks the canonical executor would reject. The EVM has to
be free to invoke the precompile as many times as it wants, with no
awareness on the precompile's part of which invocation this one is.
And reth has no notion of "execution context" that a precompile can
introspect.

### EVM-construction-audit — every place an EVM is built

Reading reth's source comprehensively, every place an EVM is
constructed falls into one of these categories. Each entry names
the trait method through which the EVM is built, and whether a
side-effecting precompile installed via the standard factory would
fire on that path:

| Category | Trait method | Precompile fires? |
|---|---|---|
| **Block building** (the proposer's payload path) | `builder_for_next_block` (default body calls `evm_with_env`) | **Yes** |
| Block validation / import (engine API `newPayload`) | `evm_for_block` → `evm_with_env` | Yes |
| Engine prewarming (speculative cache-fill on incoming payloads) | `evm_with_env` | Yes |
| Engine reorg simulation | `evm_with_env` (or `evm_for_block`) | Yes |
| `eth_call`, `eth_callMany` | `evm_with_env` | Yes |
| `eth_estimateGas` | `evm_with_env` | Yes |
| `debug_traceCall`, `debug_traceTransaction` | `evm_with_env_and_inspector` | Yes |
| `eth_callBundle`, `mev_simBundle` | `evm_with_env` | Yes |
| RPC config bootstrapping (pending-block initialisation) | `evm_for_block` → `evm_with_env` | Yes |
| **Mempool / pool validation** | *No EVM is constructed.* Only `evm_config.evm_env(&tip)` is read for limits. | **No** |

The crucial reading: every non-building path that runs an EVM goes
through `evm_with_env` — *the same trait method that the building
path's default impl uses internally*. The only entry point exclusive
to the proposer's build is `ConfigureEvm::builder_for_next_block`;
every other EVM construction is shared with someone who has no
business firing the side-effecting precompile.

### Pure-twin-remedy — the remedy already on the books

The recommended remedy in prior analysis does not attempt to fix
the precompile's context blindness from inside its body. It moves
the discrimination one layer up, into the factory that constructs
the EVM. The shape:

- A custom `ConfigureEvm` overrides `builder_for_next_block` to
  produce an EVM whose `PrecompilesMap` installs the
  *side-effecting* precompile. It pointedly does **not** delegate to
  `self.evm_with_env`, since that method is shared with every leak
  path.
- Every other method on `ConfigureEvm` (`evm_with_env`,
  `evm_for_block`, `evm_with_env_and_inspector`) keeps an EVM whose
  `PrecompilesMap` installs a **pure twin**: a precompile at the
  same address that returns the same bytes for the same `(calldata,
  journaled state)` and performs no off-chain work.
- The proposer's `BasicPayloadJob` retries `try_build` repeatedly
  per slot to maximize fees; only one trial wins. A fresh
  `Uuid::new_v4()` is minted in a thread-local at the top of each
  `try_build` invocation, and the precompile tags each off-chain
  write with that UUID. Off-chain side buffers writes under the
  UUID and waits.
- When the consensus client calls `engine_getPayload`, the payload
  builder service emits `Events::BuiltPayload` for the resolved
  (winning) trial. A subscriber registered via
  `PayloadBuilderHandle::subscribe()` reads the event, looks up the
  trial UUID for the resolved payload, and signals the off-chain
  side: "commit UUID X." Losing trials' writes are GC'd by TTL.

The pure twin is mandatory for consensus: validators on the import
path run the pure twin and must compute the same state root as the
proposer. The UUID is mandatory for off-chain correctness: without
it, all `try_build` trials' effects would commit, and only one
trial's results are canonical. The UUID must **never** influence
the precompile's return bytes, gas charge, or any EVM-visible
state, or the chain forks.

The design also restricts the precompile's return-value class. Two
shapes hide in the phrase "side-effecting precompile", and the
distinction between them is load-bearing for everything that
follows. They are named here so that the rest of the document can
refer to them by intent rather than by alphabet letter.

- **Ornamental precompile.** Deterministic return, side effect
  invisible to the EVM. Same
  `(calldata, caller, value, journaled state)` always produces the
  same return bytes. The "impurity" is that the precompile *also*
  enqueues a write to some external store, but nothing the EVM
  observes ever depends on whether or what was written. Simulators
  run the pure twin; gas estimates are accurate; canonical
  re-execution sees the same return. Safe under the
  **Pure-twin-remedy**.

- **Accessible precompile.** Return depends on prior side effects.
  The precompile reads back what previous side-effecting calls
  wrote — the side effect, in other words, is *accessible* to
  subsequent invocations of the same precompile. Now simulation
  drift becomes a fork hazard: the simulator's pure twin doesn't
  perform the writes, so a later read in the same simulation sees
  state that diverges from the same read on the canonical path.

**The project's target is Accessible from day one.** The Arkiv
chain must host the DApp ecosystem (Uniswap-style AMMs reading
reserves after a swap, Aave-style lending reading collateral
after a deposit, ERC-4626 vaults reading share ratios after a
mint, same-block MEV bundles that depend on observing the first
tx's effects in the second), and any DB-backed equivalent of
those reads inside the same block needs the same same-block
read-after-write semantics. By the **Pure-twin-remedy**'s
typology, that is Accessible. There is no Ornamental phase the
project is passing through; the architectural target is
Accessible directly. The implications of operating in the
Accessible regime are formalised below in **CRUD-is-Accessible**
and **No-vanilla-Accessible-remedy**, and they materially
constrain the recommendation in §11.

An Accessible precompile, on the conventional menu of escape
hatches, has two redesign options: **commit-reveal across blocks**
(one block of read latency, with the EVM-visible return being a
commitment to be opened in the next block) or moving the
read/write into **journaled EVM state** (which collapses the
"external database" property the project is built on). The first
option is **rejected on DApp-compatibility grounds** for this
project — a chain that introduces one block of staleness on
reads cannot host Uniswap, Aave, or any of the same-block-MEV
patterns the L2 ecosystem depends on. Same-block read-after-write
is a non-negotiable property of the dapp surface this chain
exists to support. Both escape hatches are in any case non-trivial
design changes to the precompile contract, not configuration
knobs.

### Pure-twin-invariants — what the remedy depends on

The remedy is not "install two precompiles and walk away." It is
a contract between the side-effecting variant, the pure twin, the
EVM, the engine tree, and the off-chain reconciler. Each invariant
below is load-bearing: violation forks the chain or corrupts the
external database.

> **Return-equality.** For every input
> `(calldata, caller, value, journaled state)`, the side-effecting
> precompile and the pure twin must return byte-identical
> `PrecompileOutput.bytes`. Any divergence forks the chain at the
> first block that calls the precompile, because the proposer's
> state root will not match what validators reproduce.

> **Gas-equality.** Both variants must report identical
> `gas_used` and `gas_refunded`. The actual cost of the off-chain
> work cannot be billed *only* on the build path — validator gas
> accounting would diverge. The remedy is to charge a fixed
> cushion that bounds the worst-case external cost on *both*
> variants, even though the pure twin doesn't incur it.

> **Ornamental-return-only.** The return must be a pure function
> of `(calldata, caller, value, journaled state)`. A precompile
> whose return depends on what prior side-effecting calls wrote
> (i.e., an Accessible precompile) breaks the simulator: the
> pure twin doesn't perform the writes, so a later read in the
> same simulation sees state that diverges from the canonical
> path. Any move from Ornamental to Accessible dissolves the
> safety of the entire remedy.

> **Effect-unobservable-to-EVM.** Nothing the EVM reads after
> the call — account balances, storage slots, code, return bytes
> of subsequent calls — may depend on whether the off-chain
> write succeeded, failed, or was skipped. The side effect is
> ornamental: the EVM is sealed against its consequences. The
> moment a contract reads back the effect,
> **Ornamental-return-only** is violated.

> **Factory-completeness.** `EvmFactory::create_evm` and
> `EvmFactory::create_evm_with_inspector` must both install the
> appropriate variant. Forgetting one means tracing endpoints
> (`debug_traceTransaction`, inspector-driven paths) silently see
> a different precompile set than block execution, which
> desynchronises trace output from canonical state and can mask
> consensus bugs in pre-production review.

> **Cache-opt-out.** Both variants must register as stateful
> precompiles (`DynPrecompile::new_stateful`, i.e.
> `supports_caching = false`). The engine-tree precompile cache
> otherwise memoises return values across blocks; a stateful
> precompile served from cache may return bytes that no longer
> reflect current journaled state, and at minimum violates the
> spirit of **Ornamental-return-only**.

> **UUID-discipline.** The per-trial UUID must never influence
> return bytes, gas charge, or any EVM-visible state. It exists
> only between the precompile and the off-chain side. Any leak —
> even logging it in a way that reaches an on-chain event —
> forks the chain, because validators (re-running with no UUID,
> or a different one) reproduce different state roots.

> **Reorg-conformance.** The off-chain side must revert/replay in
> lockstep with EVM reorgs, not merely append. When the chain
> reorgs, off-chain writes from the discarded branch must be
> retractable; when a historical block is replayed, the off-chain
> state must reach the same content it had at original execution.
> The existing arkiv ExEx carries `arkiv_revert` / `arkiv_reorg`
> for exactly this; the precompile path must participate in the
> same model.

> **Stable-registration.** The pure twin and the side-effecting
> variant must share the precompile address, the
> `PrecompileId::custom("…")` string, and the hardfork-gating
> logic. EIP-7910 introspection and any external tooling that
> enumerates precompiles must see one entity, not two.

The invariants are independent: each can be violated without
violating any other, and each violation has a distinct failure
mode. **Return-equality** and **Ornamental-return-only** are the
hardest to defend over time, because the precompile author may be
tempted to "improve" the side-effecting variant in ways that
incidentally change its return. **Reorg-conformance** is the
hardest to verify, because it requires testing every reorg
scenario the chain might experience.

---

## 7. Axioms

Three load-bearing premises follow from §6 and condition the
deductions to come.

> **Context-blindness.** A precompile receives only its
> `PrecompileInput`. It cannot know whether the EVM that invoked
> it is building a block, validating one, replaying one,
> estimating gas, or serving an RPC simulation. The remedy must
> therefore live outside the precompile body.
> *(From **Precompile-anatomy**.)*

> **Plural-invocation.** A single user-level transaction is
> executed by the EVM many times across the node's lifetime — at
> least six times in nominal operation, more under historical
> replay. Side effects, ungoverned by the EVM, scale with
> invocations, not with logical transactions.
> *(From **Six-executions**.)*

> **Determinism.** The EVM-visible return of the precompile must
> be a pure function of journaled state and `PrecompileInput`.
> Any divergence between nodes — including between the proposer's
> own build and its own validating re-execution — forks the
> chain. Side effects outside that return are invisible to
> consensus but bind the database to a particular execution count
> that the precompile author cannot directly control.
> *(From **Precompile-anatomy**, **Pure-twin-remedy**.)*

---

## 8. Observations

What the source of op-reth and op-node reveals when read with
attention to the hypothesis. Each observation names the function or
entity one can grep for to follow the deduction back to its origin;
exact line numbers are tabulated in the appendix but are not
load-bearing for the argument.

> **Sequencer-reinserts.** In op-node, after the sequencer has
> built and sealed a block, the function `Sequencer.onBuildSealed`
> (in `op-node/rollup/sequencing/sequencer.go`) does not merely
> canonicalize the block locally. It emits a
> `PayloadProcessEvent` with the comment *"try to put it in our
> own canonical chain."* That event is handled by
> `EngineController.onPayloadProcess` (in
> `op-node/rollup/engine/payload_process.go`), which calls
> `engine.NewPayload` against the same execution layer that just
> built the block. The verifier path
> `EngineController.insertUnsafePayload` performs the same
> sequence: `NewPayload` followed by `ForkchoiceUpdate`. This is
> not optional: the engine API is the only channel by which
> op-node informs the EL that a block is canonical.

> **Builder-retry-loop.** The `BasicPayloadJob` (in
> `reth-basic-payload-builder`) polls a tokio interval and
> respawns `try_build` on each tick until the deadline expires.
> Op-reth wires this configuration at
> `OpPayloadBuilder::spawn_payload_service` (in
> `op-reth/crates/optimism/node/src/node.rs`), reading
> `--builder.interval` (default 1 second) and `--builder.deadline`
> (default 12 seconds). On a 2-second OP block, the effective
> deadline experienced by the builder is
> `BlockTime − sealing-duration ≈ 1.95s`, so the default cadence
> permits roughly one to two retries per slot. The retry loop is
> the builder's fee-maximization mechanism; reth offers no
> "single-shot" toggle.

> **Local-preinsertion.** The launcher loop in
> `crates/node/builder/src/launch/engine.rs` carries a
> `tokio::select!` that listens to two channels: built payloads
> from the builder service, and engine API events from op-node.
> The instant a build completes, the launcher emits
> `EngineApiRequest::InsertExecutedBlock` directly to the engine
> tree handler, bearing the comment *"prevent re-execution if
> that block is received as payload from the CL."* The receiving
> function `EngineApiTreeHandler.insert_block_inner` (in
> `crates/engine/tree/src/tree/mod.rs`) opens with a guard: if
> the block is already in the tree, return
> `InsertPayloadOk::AlreadySeen` and execute nothing. This is a
> race-win mechanism: when local insertion arrives before the
> remote `NewPayload`, the validation re-execution
> short-circuits.

> **Reorg-simulation-fires.** When the derivation pipeline
> produces a chain reset, the function `create_reorg_head` (in
> `crates/engine/util/src/reorg.rs`) constructs an EVM via
> `evm_for_block` and replays transactions on the discarded
> branch. No CLI flag turns this off. Sequencers reorg too:
> op-node emits `derive.NewResetError` on derivation
> inconsistencies.

> **Pool-EVM-free.** The `OpTransactionValidator` (in
> `op-reth/crates/optimism/node/src/txpool.rs`) holds an
> `evm_config` but never instantiates an executor; it reads only
> `evm_env` for gas-limit context. Mempool admission, despite
> appearing as item #2 in the **Six-executions** table, is not a
> leak path on op-reth. The "partial execution" in the
> upstream-Ethereum case is absent here.

> **Pending-off-by-default.** The `RollupArgs::compute_pending_block`
> field (in `crates/optimism/node/src/args.rs`) defaults to
> `false`. The pending-block computation that on Ethereum mainnet
> would construct an EVM via `evm_with_env` does not, on a default
> op-reth, run at all. The pending block is the latest block.

> **NoTxPool-orthogonal.** Op-node's `Sequencer.buildL2Block` sets
> `attrs.NoTxPool` per the conditions in
> `sequencing/sequencer.go` (drift past `MaxSequencerDrift`,
> hardfork activation blocks, recovery mode), but the flag affects
> only *which transactions* are pulled into the build, not whether
> the build or its retries occur. On a nominal sequencer slot
> `NoTxPool=false`, and the mempool is consulted; the precompile
> firing pattern is unchanged either way.

> **Invalid-block-hook-fires.** The function
> `pre_block_witness_recorder` (in
> `crates/engine/invalid-block-hooks/src/witness.rs`) constructs
> an EVM via `evm_for_block` when a block fails validation. This
> is dormant in nominal operation but live under attack — an
> adversary submitting deliberately invalid blocks would fire the
> side-effecting precompile against a branch that is rejected.

---

## 9. Statements

From the axioms and observations, the following deductions hold.
Each names the premises it rests on.

> **RPC-silenceable.** *Condition **(a) no-RPC** of the hypothesis
> is satisfiable.* Standard reth `RpcServerArgs` toggles
> (`--http=false --ws=false --ipcdisable`) silence the public
> surface. By **Pool-EVM-free** the mempool does not construct an
> EVM. By **Pending-off-by-default** the pending-block computation
> is already disabled by default on op-reth. The combination
> silences every entry in the **EVM-construction-audit** table
> below the building row.

> **Retries-approximable.** *Condition **(b) one-build-per-block**
> is approximable but not contractually guaranteeable.* From
> **Builder-retry-loop** alone. The knob `--builder.interval` can
> be raised to match or exceed the effective deadline, which
> collapses the retry loop to a single `try_build` in steady
> state. But the retry loop is structural to `BasicPayloadJob`,
> and the deadline is itself bounded by the unpredictable arrival
> time of op-node's `GetPayload`. A slow first attempt followed
> by a faster second attempt remains possible at the margins.
> There is no compile-time proof of singleness.

> **Validation-unavoidable.** *Condition **(c) no-validation** is
> contradicted by the system itself.* From **Sequencer-reinserts**.
> After sealing, op-node's sequencer emits `PayloadProcessEvent`,
> which is consumed by `onPayloadProcess`, which calls
> `engine.NewPayload` on the sequencer's own EL. The hypothesis
> presumes the validation pipeline can be absent; the engine API
> requires that `NewPayload` be the channel through which
> canonicalization is communicated. To remove the call would be
> to remove op-node's ability to tell the EL its head has moved.

> **Common-case-quiescence.** *Op-reth avoids re-execution in
> nominal operation — locally built blocks normally skip the
> validator.* From **Local-preinsertion**. The
> `InsertExecutedBlock` event races ahead of the round-trip
> `NewPayload`; if the local channel is drained first, the
> validation re-execution short-circuits at `AlreadySeen`. The
> system already implements an internal approximation of what the
> hypothesis prescribes externally.

> **Race-win, not contract.** *The
> `Local-preinsertion` optimization can fail.* From
> **Local-preinsertion** plus **Plural-invocation**. The
> launcher's `tokio::select!` processes channels in whatever
> order the runtime serves them; if the engine task is occupied
> with a slow operation when the local channel emits, the remote
> `NewPayload` may arrive first and `insert_block_inner` will
> execute the block. The precompile then fires twice. There is
> no signal from inside the precompile body — by
> **Context-blindness** — that allows it to detect or correct
> this.

> **Reorg-leak.** *Reorg simulation is an irreducible leak. Even
> with all other paths closed, reorgs fire the precompile against
> non-canonical branches.* From **Reorg-simulation-fires**. No
> CLI flag governs `create_reorg_head`. Sequencers can and do
> reorg.

> **Adversarial-block-leak.** *Invalid-block hooks present an
> adversarial corner — submission of invalid blocks fires the
> precompile against rejected branches.* From
> **Invalid-block-hook-fires**. Not a problem in nominal
> operation, but a separate trigger available to any adversary
> who can reach the engine API.

> **CRUD-is-Accessible.** *Full DB CRUD is an Accessible
> precompile.* A precompile that supports both reads and writes
> against the same external store, where a read return can
> depend on a prior write (whether earlier in the same
> transaction or in any prior transaction), is by definition
> Accessible in the **Pure-twin-remedy** typology. The Arkiv
> precompile is Accessible by design: the DApp ecosystem it
> exists to serve (Uniswap, Aave, ERC-4626 vaults, same-block
> MEV) requires same-block read-after-write of state, so any
> DB-backed analogue inherits the same semantics. The
> hypothesis **Sequencer-restriction-suffices**, which speaks
> explicitly of "out-of-process database CRUD", is therefore a
> hypothesis about an Accessible precompile.

> **Pure-twin-fails-Accessible.** *The **Pure-twin-remedy** does
> not apply to Accessible precompiles.* From **CRUD-is-Accessible**
> together with **Return-equality** and **Ornamental-return-only**.
> Under the remedy, validators run the pure twin and perform no
> external writes. An Accessible read on the pure twin therefore
> returns whatever the external store contains *without* those
> writes, while the same read on the side-effecting variant
> returns the post-write state. The two diverge by construction.
> **Return-equality** is violated mechanically;
> **Ornamental-return-only** is violated by definition. The
> design pattern is **not available** for the Arkiv project's
> stated ambition.

---

## 10. Lemmas

> **Precarious-quiescence.** *In nominal operation the validation
> pipeline appears absent, but the appearance is precarious.*
> From **Common-case-quiescence** and **Race-win, not contract**.
> The intuition that "no validation pipeline" can be arranged is
> half-right: the EL does, by design, not re-execute locally
> built blocks under normal conditions. But the guarantee is a
> tokio race rather than a property of the type system, and a
> reth bump that touches the launcher loop or the orchestrator
> queue invalidates the assumption.

> **Irreducible-leakage.** *No deployment configuration can
> reduce the precompile firing count to exactly one per canonical
> block without a fork.* From **Retries-approximable**,
> **Reorg-leak**, and **Precarious-quiescence**. The `try_build`
> retry is at best collapsible to one per slot, the validator is
> at best usually-skipped, and the reorg path fires regardless.
> Each gap is small in isolation; their union is structural.

> **No-vanilla-Accessible-remedy.** *For DB CRUD with
> read-after-write semantics, the **Pure-twin-remedy** is
> unavailable, sequencer-restriction is leaky, and the design
> space collapses to redesigns that change the precompile
> contract.* From **Pure-twin-fails-Accessible** and
> **Irreducible-leakage**. The first removes the pure-twin path
> for Accessible precompiles; the second keeps structural leaks
> (retries, race-loss, reorg) in the sequencer-restriction
> path. What remains is not a configuration choice but a
> redesign. Four candidates, only one of them viable for this
> project:
>
> - **Cross-block commit-reveal.** *Not viable for Arkiv:*
>   one block of read latency is enough to break Uniswap
>   reserves, Aave collateral computations, ERC-4626
>   share-price reads on the second leg of a same-block trade,
>   and same-block MEV bundles. Any DApp that depends on
>   observing in-block state writes — i.e., most of DeFi —
>   breaks. Same-block read-after-write is a non-negotiable
>   property of the dapp surface this chain exists to support.
> - **Journal the data into EVM state.** *Not viable for
>   Arkiv:* this is the dissolution of EntityDB, not its
>   redesign. EntityDB exists precisely because raw EVM
>   storage is the wrong substrate (no schema, no query
>   model, no indexing); using EVM storage as the project's
>   data store is "build on Ethereum" vanilla. The project's
>   purpose ceases to exist.
> - **Hard-fork extending from op-reth into EntityDB itself.**
>   *Not viable as currently sketched:* lacks theoretical
>   backing. No one has designed how externally-imposed
>   per-retry DB snapshots interact with the launcher's tokio
>   race, the validator's `evm_with_env` path, or the reorg
>   simulator. The proposal is a placeholder, not a remedy —
>   and accumulates substantial recurring maintenance cost on
>   every reth and op-reth bump for a foundation that has not
>   been laid.
> - **Lifecycle-couple EntityDB to StateDB, with EntityDB
>   embedded in the reth process.** Re-engineer EntityDB so
>   its lifecycle mirrors StateDB's: every EVM construction
>   gets its own EntityDB fork off canonical state; forks are
>   cheap to create (copy-on-write), dropped silently when
>   their StateDB drops, committed atomically when their
>   StateDB commits. Restores velocity conformance *from the
>   EntityDB side* by making EntityDB conform to StateDB's
>   velocity rather than asking the EL to slow down.
>   Preserves the external-DB framing — EntityDB is still its
>   own datastore, with its own schema and query model.
>   **Critical constraint: in-process embedding.**
>   Out-of-process EntityDB cannot satisfy the cheap-fork
>   requirement (every fork would be an IPC roundtrip),
>   cannot guarantee atomic commit between StateDB and
>   EntityDB on canonical execution, and cannot reliably drop
>   EntityDB forks alongside discarded StateDB forks.
>   EntityDB therefore becomes a Rust library that reth links
>   against, not a separate service. The engineering cost is
>   substantial (copy-on-write over a base snapshot,
>   in-process access, transactional commit/abort,
>   reorg-aware revert) but it is one-time work confined to
>   the DB layer; no op-reth fork is required, and the work
>   is independent of reth/op-reth release cadence.

---

## 11. Verdict

> **The hypothesis Sequencer-restriction-suffices is partially
> true, structurally false, and operationally fragile; and even
> where it does the most good, it does not cover the actual case
> the project is trying to solve.**
>
> By **RPC-silenceable**, the public surface can be made quiet.
> By **Common-case-quiescence** and the existence of
> `InsertExecutedBlock`, the validation re-execution can be made
> *usually*-quiet. By careful tuning of `--builder.interval`, the
> speculative builds can be made *mostly*-quiet
> (**Retries-approximable**). But by **Validation-unavoidable**,
> the validation channel cannot be closed, because op-node
> requires it. By **Race-win, not contract**, the race that
> currently closes it in practice can be lost. By **Reorg-leak**,
> reorg simulation cannot be closed at all. By
> **Adversarial-block-leak**, an adversary has a separate
> trigger.
>
> The hypothesis identifies the right shape of a remedy —
> restrict the call sites rather than ask the leaf to
> discriminate — but underestimates how many call sites the EL
> maintains internally. What the hypothesis wishes were a
> single-path system is, in practice, a system with one
> structural leak (reorg), one probabilistic leak (the race),
> one configurable-but-not-contractual leak (`try_build`
> retries), and one adversarial leak (the invalid-block hook).
>
> **The Arkiv-specific recommendation.** The project requires
> an Accessible precompile (per the DApp argument: Uniswap,
> Aave, ERC-4626 vaults, same-block MEV all read same-block
> state, and any DB-backed analogue of those reads forces
> Accessible). By **Pure-twin-fails-Accessible**, the pure-twin
> remedy does not apply. By **No-vanilla-Accessible-remedy**,
> the remaining design space is not a deployment configuration
> but a contract redesign. Four candidates; three are
> rejected; only one survives:
>
> 1. ❌ **Cross-block commit-reveal.** Returns a deterministic
>    commitment in block N; the underlying read is satisfiable
>    only in block N+1. One block of read latency.
>    **Rejected for Arkiv on DApp-compatibility grounds.** Same-
>    block AMM reserves, oracle prices, vault share ratios, and
>    MEV-bundle dependencies cannot tolerate read staleness.
>    Any DApp that observes in-block writes — i.e., most of
>    DeFi — breaks.
>
> 2. ❌ **Journal the data into EVM state.** Reads and writes
>    go through `EvmInternals`; same-block read-after-write
>    works trivially (it is just SLOAD on real EVM storage);
>    the precompile becomes pure. **Rejected on architectural
>    grounds.** This is the dissolution of EntityDB, not its
>    redesign. EntityDB exists precisely because raw EVM
>    storage is the wrong substrate (no schema, no query
>    model, no indexing); replacing EntityDB with EVM storage
>    is "build on Ethereum" vanilla. The project's purpose
>    ceases to exist.
>
> 3. ❌ **Hard-fork sequencer-restriction (with DB-side
>    snapshots).** §12's punch list, upgraded to address
>    Accessible precompiles specifically. **Rejected on
>    theoretical grounds.** Even with items 2 and 3 of §12
>    in place, an Accessible precompile inside a `try_build`
>    retry that loses to a better-fee retry has already
>    committed state changes that the next retry's reads
>    would observe in a way the winning retry did not. The
>    proposed remedy — per-retry DB snapshots externally
>    imposed on EntityDB, race-deterministic
>    `InsertExecutedBlock` ordering, reorg simulation patched
>    out — has not been designed end-to-end; no one has
>    worked out how the snapshots interact with the launcher
>    race, the validator's `evm_with_env` path, or the reorg
>    simulator. The proposal is a sketch. Even if the design
>    were worked out, the fork would extend across two
>    components (op-reth *and* EntityDB) with substantial
>    recurring maintenance on every reth/op-reth bump.
>
> 4. ✅ **Lifecycle-couple EntityDB to StateDB, with EntityDB
>    embedded in the reth process.** Re-engineer EntityDB so
>    its lifecycle mirrors StateDB's: every EVM construction
>    (gas estimation, `try_build` retry, prewarming, reorg
>    replay, simulation) gets its own EntityDB fork off
>    canonical state; forks are cheap to create
>    (copy-on-write); dropped silently when their StateDB
>    drops; committed atomically when their StateDB commits.
>    Restores velocity conformance *from the EntityDB side*
>    — instead of asking the EL to slow down to EntityDB's
>    velocity, EntityDB is re-engineered to match StateDB's.
>    Preserves the external-DB framing (EntityDB is still its
>    own datastore, with its own schema and query model).
>
>    **Critical constraint: in-process embedding.**
>    Out-of-process EntityDB cannot satisfy the cheap-fork
>    requirement (every fork would be an IPC roundtrip);
>    cannot guarantee atomic commit between StateDB and
>    EntityDB on canonical execution; cannot reliably drop
>    EntityDB forks alongside discarded StateDB forks. The
>    architectural consequence: **EntityDB becomes a Rust
>    library that reth links against, not a separate
>    service.** Independent consumers must query through
>    reth's RPC or a read-only embedded instance.
>
>    Requires substantial engineering to make EntityDB
>    "light and easily creatable" the way StateDB is —
>    copy-on-write over a base snapshot, in-process access,
>    transactional commit/abort semantics, reorg-aware
>    revert. **No op-reth fork required.** The work is
>    one-time, confined to the DB layer, and independent of
>    reth/op-reth release cadence. This is the only
>    conformance-restoring strategy that survives Arkiv's
>    combined DApp-compatibility and external-DB-framing
>    constraints.
>
> The pragmatic conclusion: there is one viable option, and
> it is option (4). The project's actual decision is no
> longer "which redesign" but "are we willing to make
> EntityDB an embedded library." If yes, the velocity-
> conformance problem is solved cleanly at the DB layer with
> one-time engineering and no op-reth fork. If no — if
> EntityDB must remain a standalone, out-of-process service
> — full Accessible CRUD inside a single block is **not
> safely achievable in op-reth**, and the project must
> change either its precompile (drop the read primitive,
> accept Ornamental) or its DApp commitments (live with
> commit-reveal staleness) or its design premise (accept
> EVM-state journaling). All three retreats abandon
> something the project currently considers load-bearing.
>
> ---
>
> **Aside, for readers of this document working on other
> projects.** If your precompile (or your sidecar
> augmented-execution pathway — they are the same object for
> this analysis) is genuinely Ornamental (effects whose
> resulting state the EVM never reads back — telemetry,
> logging, fire-and-forget event emission), the
> **Pure-twin-remedy** *is* available to you: override
> `ConfigureEvm::builder_for_next_block`, install a pure twin
> on the other EVM-construction methods, tag writes with a
> per-trial UUID, reconcile at `Events::BuiltPayload`. The
> design ships with one crate of override code, no fork, and
> consensus-level guarantees — subject to the
> **Pure-twin-invariants** (**Return-equality**, **Gas-equality**,
> **Ornamental-return-only**, **Effect-unobservable-to-EVM**,
> **Factory-completeness**, **Cache-opt-out**,
> **UUID-discipline**, **Reorg-conformance**,
> **Stable-registration**). The Arkiv project cannot use this
> path because the DApp constraint forces it into Accessible,
> but the path remains valid for genuinely Ornamental
> side-effecting precompiles.

---

## 12. What it would take to make Sequencer-restriction-suffices hold

The hypothesis is not unreachable, but the price is a permanent
fork that has to be carried indefinitely.

1. **Pin `--builder.interval` ≥ effective deadline.**
   Operational, no fork. Sacrifices fee maximization for
   predictability. Addresses **Retries-approximable**.

2. **Force `InsertExecutedBlock` to win deterministically.** Patch
   the engine handler to short-circuit `NewPayload` whenever the
   block originated from the local builder, regardless of arrival
   order. Fork of one reth crate. Addresses **Race-win, not contract**.

3. **Disable reorg simulation.** Gate `create_reorg_head` behind
   a flag, or remove the call entirely from the sequencer build.
   Fork of one reth crate. Addresses **Reorg-leak**. Operator
   visibility into invalid reorgs is lost as a side cost.

4. **Disable the invalid-block hook.** Remove or flag it.
   Trivial. Addresses **Adversarial-block-leak**.

5. **Optionally: fork op-node to skip `NewPayload` for self-built
   blocks.** Eliminates the race entirely by removing the racer.
   Heavy: diverges from the OP spec, breaks compatibility with
   non-reth ELs. Likely not worth the ongoing cost.

Even with items 1–4 in place, the design depends on a fragile
contract: that no future reth or op-reth bump introduces a new
EVM-instantiation site the design did not anticipate. Verification
becomes a recurring discipline — `rg "evm_with_env|evm_for_block|evm_factory.create_evm"`
on every workspace bump, with a written checklist of which results
are safe and which are leaks.

---

## 13. Cross-reference: the prior op-geth augmented-execution-pathway

As stated in §1 (Abstract), this document treats the precompile
and the augmented-execution-pathway / sidecar designs as
**functionally identical for analytical purposes**. This section
elaborates that claim — the mechanism by which they are
equivalent, and the corollary that the trouble is with the OP
protocol, not the execution client or the interception design.

The earlier project attempt — intercepting transactions to a
designated address *before* the EVM is dispatched and routing
them to a sidecar — suffered the same context-blindness
pathology as the present precompile design. The interceptor,
like the precompile, is a leaf called by every site that
constructs an EVM, and it inherits the calling context from
whichever site happened to dispatch it. Gas estimation that
routes to a state-mutating sidecar produces wrong gas;
engine-API validation that routes to the sidecar accepts blocks
the canonical builder would reject; reorg simulation that routes
to the sidecar fires against discarded branches. Moving the
mechanism — from precompile to sidecar, or from sidecar back to
precompile — preserves the design problem.

The op-geth attempt's failure mode is exactly the failure mode
this analysis predicts for the sequencer-restriction hypothesis:
leaks at sites the design did not anticipate, discovered only
when the chain has already drifted. Every observation,
statement, and lemma in §§7–10 of this document applies,
unchanged, to a sidecar implementation: substitute "sidecar
call" for "precompile invocation" and the entire argument
proceeds identically.

The same root cause underlies all three attempts (op-geth
sidecar, op-reth precompile, sequencer-restriction). The remedy
is not to make the leaf cleverer, not to switch the execution
client, and not to narrow the deployment — it is to push the
decision upstream into the component that constructs the EVM
(the **Pure-twin-remedy**'s trait-method override, for
Ornamental precompiles) or to remove the external state from
the consensus path entirely (journaling into EVM state, for
Accessible ones).

---

## 14. Reading list — entities worth tracing in the source

To follow the deductions to their concrete origins, the names below
are the load-bearing waypoints; each is searchable in its
respective tree.

In op-node (`repos/optimism/op-node`):

- `Sequencer.onBuildSealed` and `PayloadProcessEvent` —
  `rollup/sequencing/sequencer.go`. The decision to re-insert the
  block via the engine API.
- `EngineController.onPayloadProcess` —
  `rollup/engine/payload_process.go`. The call to
  `engine.NewPayload` after building.
- `EngineController.insertUnsafePayload` —
  `rollup/engine/engine_controller.go`. The verifier-path
  equivalent that pairs `NewPayload` with `ForkchoiceUpdate`.
- `SequencerSealingDurationFlag` and friends — `flags/flags.go`.
  Operator surface for the sealing window (default 50ms).
- `Sequencer.buildL2Block` (`NoTxPool` conditional logic) —
  `rollup/sequencing/sequencer.go`. The fork conditions that flip
  the mempool source on or off.

In op-reth (`repos/op-reth`):

- `OpPayloadBuilder` and `OpPayloadBuilder::spawn_payload_service` —
  `crates/optimism/payload/src/builder.rs`,
  `crates/optimism/node/src/node.rs`. Where the retry loop is
  configured.
- `BasicPayloadJob` / `BasicPayloadJobGeneratorConfig` —
  `crates/payload/basic/src/lib.rs`. The retry loop itself.
- `PayloadBuilderArgs` (`--builder.interval`,
  `--builder.deadline`) —
  `crates/node/core/src/args/payload_builder.rs`. The CLI knobs
  whose tuning addresses **Retries-approximable**.
- `RollupArgs` (`--rollup.compute-pending-block`,
  `--rollup.disable-tx-pool-gossip`) —
  `crates/optimism/node/src/args.rs`. The OP-specific CLI knobs.
- `OpTransactionValidator` —
  `crates/optimism/node/src/txpool.rs`. Evidence that the mempool
  is EVM-free on op-reth.
- The launcher `tokio::select!` with `InsertExecutedBlock`
  emission — `crates/node/builder/src/launch/engine.rs`. The
  race that closes **Race-win, not contract** when it wins.
- `EngineApiRequest::InsertExecutedBlock` handler in
  `EngineApiTreeHandler` — `crates/engine/tree/src/tree/mod.rs`.
  The receiving side.
- `EngineApiTreeHandler.insert_block_inner` (with the
  `AlreadySeen` guard) — `crates/engine/tree/src/tree/mod.rs`.
  The early return that bypasses execution when the block is
  already known.
- `create_reorg_head` — `crates/engine/util/src/reorg.rs`. The
  irreducible **Reorg-leak**.
- `pre_block_witness_recorder` —
  `crates/engine/invalid-block-hooks/src/witness.rs`. The
  **Adversarial-block-leak**.

For the **Pure-twin-remedy**:

- `ConfigureEvm::builder_for_next_block` — the build-exclusive
  entry point to override.
- `ConfigureEvm::evm_with_env`, `ConfigureEvm::evm_for_block`,
  `ConfigureEvm::evm_with_env_and_inspector` — the shared
  methods that get the pure twin.
- `EvmFactory::create_evm`, `EvmFactory::create_evm_with_inspector` —
  the two factory entry points every EVM passes through; a custom
  factory must wrap both, or `debug_traceTransaction` and other
  inspector-driven paths silently see a different precompile set
  than block execution.
- `PrecompileInput`, `PrecompilesMap`, `DynPrecompile` — the
  precompile API surface.
- `PayloadBuilderHandle::subscribe()` returning `Events::BuiltPayload` —
  the asynchronous "this trial won" signal that the off-chain
  reconciler subscribes to.

---

## Appendix: exact citations

For the reader who insists on line-level verification, the
following citations were spot-checked against the cloned trees on
2026-05-11. Line numbers are reproduced here for completeness and
are *not* the structural backbone of the argument — function names
and trait surfaces in the reading list above are the durable
references.

| Entity | Path | Line |
|---|---|---|
| `onPayloadProcess` `NewPayload` call | `repos/optimism/op-node/rollup/engine/payload_process.go` | 60 |
| `Sequencer.onBuildSealed` emitting `PayloadProcessEvent` | `repos/optimism/op-node/rollup/sequencing/sequencer.go` | 301 |
| `onBuildSeal` `GetPayload` call | `repos/optimism/op-node/rollup/engine/build_seal.go` | 63 |
| `insertUnsafePayload` `NewPayload` | `repos/optimism/op-node/rollup/engine/engine_controller.go` | 606 |
| `insertUnsafePayload` `ForkchoiceUpdate` | `repos/optimism/op-node/rollup/engine/engine_controller.go` | 665 |
| `SequencerSealingDurationFlag` (default 50ms) | `repos/optimism/op-node/flags/flags.go` | 291 |
| `NoTxPool` conditional | `repos/optimism/op-node/rollup/sequencing/sequencer.go` | 559 |
| `BasicPayloadJobGeneratorConfig` wiring | `repos/op-reth/crates/optimism/node/src/node.rs` | 518 |
| `--builder.interval`, `--builder.deadline` defaults | `repos/op-reth/crates/node/core/src/args/payload_builder.rs` | 28, 32 |
| `--rollup.compute-pending-block` default | `repos/op-reth/crates/optimism/node/src/args.rs` | 30, 45 |
| `InsertExecutedBlock` emission in launcher | `repos/op-reth/crates/node/builder/src/launch/engine.rs` | 339 |
| `InsertExecutedBlock` handler | `repos/op-reth/crates/engine/tree/src/tree/mod.rs` | 1332 |
| `insert_block_inner` `AlreadySeen` guard | `repos/op-reth/crates/engine/tree/src/tree/mod.rs` | 2307 |
| Reorg simulation `evm_for_block` | `repos/op-reth/crates/engine/util/src/reorg.rs` | 293 |
| Invalid-block witness `evm_for_block` | `repos/op-reth/crates/engine/invalid-block-hooks/src/witness.rs` | 78 |
