# Verifiable Indexes in a Database Chain

A first-principles analysis of the design tension between indexed-query
verifiability, consensus over index state, and the latency budget that
bounds permissionless validators. Companion piece to
[`precompile-out-of-band-coupling.md`](precompile-out-of-band-coupling.md),
which examines the same tension from the implementation side.

> **Status.** Analysis document. Frames the problem space; does not
> prescribe a single resolution. Treats the v2 design spec
> ([`reth-exex-entity-db-design-v2.md`](reth-exex-entity-db-design-v2.md))
> as the concrete instance under examination, and Arkiv's first-principles
> document (`first-principles.md`) as the source of the constraints in
> tension.

---

## Contents

- [0. Purpose and scope](#0-purpose-and-scope)
- [1. Foundations](#1-foundations)
  - [1.1 What an index is, and what it costs](#11-what-an-index-is-and-what-it-costs)
  - [1.2 Synchronous vs asynchronous index maintenance](#12-synchronous-vs-asynchronous-index-maintenance)
  - [1.3 What "verifiability" means, in five strengths](#13-what-verifiability-means-in-five-strengths)
  - [1.4 CAP, PACELC, and what they actually say](#14-cap-pacelc-and-what-they-actually-say)
  - [1.5 Blockchain consensus, in one paragraph](#15-blockchain-consensus-in-one-paragraph)
  - [1.6 What "in the consensus envelope" means](#16-what-in-the-consensus-envelope-means)
- [2. The Arkiv goal: a queryable blockchain](#2-the-arkiv-goal-a-queryable-blockchain)
  - [2.1 What this requires that Bitcoin / Ethereum don't have](#21-what-this-requires-that-bitcoin--ethereum-dont-have)
  - [2.2 What "first-class data" means concretely](#22-what-first-class-data-means-concretely)
- [3. The core tension](#3-the-core-tension)
  - [3.1 Verifiable indexes require consensus on index state](#31-verifiable-indexes-require-consensus-on-index-state)
  - [3.2 Consensus on index state forces index work into the block-execution path](#32-consensus-on-index-state-forces-index-work-into-the-block-execution-path)
  - [3.3 Block time is bounded by liveness and validator capacity](#33-block-time-is-bounded-by-liveness-and-validator-capacity)
  - [3.4 Index work scales with state size](#34-index-work-scales-with-state-size)
  - [3.5 The triangle](#35-the-triangle)
- [4. The tension within Arkiv's principles framework](#4-the-tension-within-arkivs-principles-framework)
  - [4.1 Provable Execution](#41-provable-execution)
  - [4.2 Data Verifiability — the load-bearing invariant](#42-data-verifiability--the-load-bearing-invariant)
  - [4.3 Permissionless Operation](#43-permissionless-operation)
  - [4.4 Sustainability and Data First (the soft pull)](#44-sustainability-and-data-first-the-soft-pull)
  - [4.5 The §4.7 escape hatch and what it implies](#45-the-47-escape-hatch-and-what-it-implies)
- [5. Existing systems and where they sit](#5-existing-systems-and-where-they-sit)
  - [5.1 Bitcoin / Ethereum — no built-in indexing](#51-bitcoin--ethereum--no-built-in-indexing)
  - [5.2 The Graph — eventual-consistency indexer](#52-the-graph--eventual-consistency-indexer)
  - [5.3 Cosmos SDK chains with custom modules](#53-cosmos-sdk-chains-with-custom-modules)
  - [5.4 zk-indexed and SNARK-verified systems](#54-zk-indexed-and-snark-verified-systems)
  - [5.5 Where Arkiv's v2 design sits](#55-where-arkivs-v2-design-sits)
- [6. Three coherent positions in the design space](#6-three-coherent-positions-in-the-design-space)
  - [6.1 Strong: indexes in the state root](#61-strong-indexes-in-the-state-root)
  - [6.2 Hybrid: data items committed, indexes signed](#62-hybrid-data-items-committed-indexes-signed)
  - [6.3 Weak: signed responses with challenge games](#63-weak-signed-responses-with-challenge-games)
  - [6.4 Comparison](#64-comparison)
  - [6.5 Combinations and pivots](#65-combinations-and-pivots)
- [7. Implications for the architecture](#7-implications-for-the-architecture)
  - [7.1 The pivot decision the architecture has not yet named](#71-the-pivot-decision-the-architecture-has-not-yet-named)
  - [7.2 What an ADR for §4.7 would have to settle](#72-what-an-adr-for-47-would-have-to-settle)
  - [7.3 The downstream consequences](#73-the-downstream-consequences)
- [8. Open questions](#8-open-questions)
- [9. Glossary](#9-glossary)

---

## 0. Purpose and scope

This document does two things:

1. **Formalises** the architectural tension at the heart of the
   database-chain ambition — between making indexed query results
   cryptographically verifiable, requiring index state to be part of
   consensus, and the latency and capacity bounds that govern
   permissionless validators.
2. **Locates** that tension within Arkiv's existing principles and
   invariants, identifies where the v2 design has tightened a
   constraint beyond what the principles strictly require, and surfaces
   the design choices the architecture has not yet made explicit.

Intended reader: someone who has worked with blockchains and databases
independently but has not had to think hard about what it means to
combine them under blockchain trust guarantees. The first major section
(§1) is foundational; readers conversant with PACELC, eventual
consistency, and the cost of index maintenance can skim it and pick up
at §2.

What this document does **not** do:

- Recommend a specific architectural choice. §6 enumerates the three
  coherent positions; the choice depends on values out of scope here.
- Re-derive the v2 design or the principles document. Both are treated
  as given.
- Cover the orthogonal "is the DB itself deterministic" question
  (DB-internal concern) or the multi-execution problem (covered in the
  precompile companion doc).
- Argue that one position is universally better. Each of the three
  positions in §6 is in production somewhere; they suit different
  workloads and different threat models.

---

## 1. Foundations

### 1.1 What an index is, and what it costs

A database **index** is auxiliary data structured to accelerate queries
beyond the raw lookup-by-primary-key the storage engine provides natively.
The canonical example: a B-tree on a non-primary column of a Postgres
table. Without it, queries by that column do a full table scan, O(N);
with it, they do a logarithmic descent, O(log N). The index is a
parasitic data structure — it carries no information of its own,
existing only to make a particular access pattern fast.

Indexes are not free. Every insertion into the underlying table forces
an insertion into every index defined over it. The cost depends on the
index type:

- **B-tree / B+-tree** (sorted single-column or composite indexes):
  O(log N) hashes / pointer-chases per insert. The de-facto standard.
- **LSM-tree** (PebbleDB, RocksDB): amortised O(log N) per write, with
  background compaction producing periodic latency spikes (typically
  P99 / P999).
- **Inverted index** (full-text search): tokenise the input, write
  entries for each token. Insert cost roughly proportional to the
  number of unique tokens in the document.
- **Bitmap index** (low-cardinality categorical columns): one bitmap
  per category, every insert flips a bit. The bitmap itself can be
  large; rewrite cost grows with cardinality unless the implementation
  uses differential or chunked storage.
- **Spatial / multi-dimensional indexes** (R-trees, KD-trees): O(log N)
  but with worse constants and more disk I/O.
- **Composite indexes**: O(log N) per index, multiplying the constant
  factor by the number of indexes in play.
- **Merkle Patricia Trie** (Ethereum's account/state index): O(log_16
  N) keccaks per update. Dominated by hashing cost rather than disk
  I/O at moderate scale; both at billion-scale.

Two costs scale with state size beyond the asymptotic complexity:

- **Cache effects.** As state grows, the working set exceeds RAM, and
  every index level that misses cache becomes a disk seek. The
  asymptotic complexity is unchanged; the constant factor jumps 100×
  or more once paging dominates. This is what your colleague is
  probably gesturing at when they say "linear in state growth" — not
  literally O(N) per insert, but a regime change in the constant
  factor that *feels* like linearity at the scales engineers care
  about.
- **Compaction overhead** (LSM-style structures). Background work to
  consolidate write-amplified storage. Not on the per-write critical
  path in steady state, but produces P99 latency spikes that synchronous
  systems can't ignore.

The takeaway: even O(log N) indexes are expensive in absolute terms,
and they get more expensive at scale. Production database engineering
is largely the management of these costs. A naive design that worked
well at 10⁶ entities can collapse at 10⁹.

### 1.2 Synchronous vs asynchronous index maintenance

Two ways to keep indexes consistent with a base table:

**Synchronous.** Every write to the table also writes to every index,
within the same transaction. Reads of the index always see exactly the
post-write state — ACID semantics for query results.

- Cost: write latency includes index update time. Indexes constrain
  throughput.
- Benefit: query results are always up-to-date with respect to
  committed writes. No staleness window.

**Asynchronous.** Writes go to the table immediately. Index updates
happen in the background, possibly batched, possibly out-of-order,
possibly minutes behind.

- Cost: query results may be stale. Reads need to either tolerate the
  staleness or wait for the index to catch up.
- Benefit: write throughput decoupled from index maintenance cost.

Most production databases support both modes for different indexes,
and almost every system at meaningful scale uses async indexes for at
least some queries. Postgres `CREATE INDEX CONCURRENTLY` is async;
standard `CREATE INDEX` is sync. Elasticsearch is async by default
(refresh interval, typically 1s). Search engines are predominantly
async. Read replicas are eventually-consistent async copies of the
write master.

The choice between sync and async is a fundamental database design
dimension, not a low-level optimisation. It governs what consistency
guarantee the system can offer, what throughput it can achieve, and
how its costs scale.

For a database chain, this dimension takes on additional weight: the
consistency model of indexes interacts with the chain's consensus
model. A chain that wants synchronous indexes is choosing to put index
work on the consensus-critical path, with all the latency and capacity
implications that follow. A chain that accepts asynchronous indexes is
choosing eventual consistency for query results, with all the
verifiability implications that follow.

### 1.3 What "verifiability" means, in five strengths

Verifiability is the property that a data consumer can check the data's
correctness without having to trust the source. It comes in graded
strengths.

| Strength | Mechanism | Trust required |
|---|---|---|
| **None** | Trust the source's response | Full trust in the source |
| **Authenticated** | Source signs the response; consumer verifies the signature | Trust in the source's identity, not its honesty |
| **Inclusion proof** | Source returns data + Merkle proof against a known commitment | Trust in the commitment (typically anchored elsewhere) |
| **Computational proof** | Source returns data + zk-proof of a specific computation | Only the proof system itself |
| **Challenge / fraud proof** | Source's claim stands unless someone publishes contradicting evidence within a window | Trust that *some* honest party is watching, plus economic security from bonded participants |

Each strength has different infrastructure requirements:

- **Authenticated** requires PKI; no chain involvement.
- **Inclusion proofs** require the verifier to know the commitment
  (typically a chain state root) and to be able to verify hashes.
- **Computational proofs** require running a verifier algorithm
  against the proof; verification cost is independent of the
  computation's size.
- **Challenge games** require a watchful counterparty, an economic
  mechanism (bonds, slashing) that punishes provable fraud, and an
  on-chain dispute resolution path.

For a database query, the analogues are:

- **None.** Query a centralised index, trust the response. Web2 default.
- **Authenticated.** Query a signed-response index. The responder
  commits; you can prove malfeasance later, but at query time you
  trust them.
- **Inclusion proof.** Query an index that is itself committed to in
  chain state; the response includes a Merkle proof against the
  commitment. The verifier checks the proof against a recent block
  header.
- **Computational proof.** Query an index and receive a zk-proof that
  the response is the correct output of a known query function over
  committed state. The verifier checks the proof against the
  committed state root.
- **Challenge game.** Query a signed-response index; if the response
  is wrong, an honest party can challenge via an on-chain dispute that
  ends with the responder slashed.

The choice among these is the single largest architectural lever in the
design of a database chain. It determines what infrastructure has to be
deployed, what threat model is in play, what the user's verification
experience looks like, and — crucially for what follows — what the
node operator has to maintain.

### 1.4 CAP, PACELC, and what they actually say

Distributed systems literature has produced a number of frameworks for
reasoning about tradeoffs in shared-state systems. Two are commonly
cited; only one is the right tool for what's discussed here.

**CAP** (Brewer, 2000): in a distributed system, during a network
partition, you can preserve **Consistency** or **Availability**, not
both. **Partition tolerance** is taken as given because partitions are
unavoidable in any real-world system.

CAP is often invoked loosely as "you can have any two of C, A, P". This
framing is technically incorrect — P isn't optional, it's the
precondition under which you choose between C and A — but the
underlying intuition (that distributed systems force tradeoffs between
consistency and availability) is sound.

**PACELC** (Abadi, 2012): a strict refinement of CAP that adds the
"else" branch CAP doesn't address. *In case of Partition*, choose
Availability or Consistency; *Else* (i.e., when the network is
healthy), choose Latency or Consistency.

PACELC is the correct frame for blockchain-with-indexes. Even when
consensus is making progress and the network is healthy, you face a
continuous tradeoff between **block-time aggressiveness** (latency) and
**index-state synchronisation** (consistency). Strong consistency
between index state and chain state requires synchronisation;
synchronisation costs latency; latency budget is finite.

Most blockchains are CP/EC in PACELC terms: they prioritise
consistency over availability under partition (the chain halts), and
consistency over latency in the absence of partition (block time is
gated by consensus rounds, not by the speed of the fastest validator).

A queryable blockchain that wants synchronous indexes is also CP/EC.
An eventually-consistent indexer (The Graph, etc.) is CP/EL: the chain
itself is consistent, but query results trade consistency for lower
latency.

The colleague's CAP reference is in the right neighbourhood but
slightly misaimed; PACELC is the better tool. The point survives the
correction, though: there is a real, structural tradeoff, and it isn't
a tradeoff you can engineer your way out of by being clever — it's a
choice between two coherent design positions.

### 1.5 Blockchain consensus, in one paragraph

A blockchain is a state machine whose transitions are agreed upon by a
network of validators via consensus. State transitions are
deterministic functions of the current state and an ordered batch of
transactions; given the same starting state and the same transaction
batch, every honest validator computes the same resulting state. State
is committed to via a Merkle root (Ethereum's state trie); the Merkle
root is part of every block header; validators reach consensus on the
block header — including the state root — and from there everything
falls out. A transaction's effects are durable iff its containing
block is canonical; query results are verifiable iff the queried state
is committed under the canonical state root.

Two properties matter for what follows:

- **Determinism.** Every honest validator executing a block computes
  the same state. Non-negotiable. Non-determinism in block execution
  forks the chain.
- **Bounded execution time.** Every honest validator must finish
  executing a block within some window, or block production stalls.
  The window depends on the consensus protocol, the network topology,
  and the operational requirements of the validator set.

These two properties — determinism and bounded execution time — are
what every constraint in the rest of this document derives from.

### 1.6 What "in the consensus envelope" means

A piece of state is "in the consensus envelope" iff every validator
agrees on it via the consensus protocol. The mechanism is consensus on
the block header, which commits to the state root, which (via Merkle
proofs) commits to every individual piece of state.

State **inside** the envelope:

- EVM account balances, contract storage, code.
- Block timestamp, base fee, randomness (`prevrandao`), beneficiary.
- Transaction envelope contents (calldata, signature, blob hashes for
  EIP-4844).
- Anything reachable via `eth_getProof` against a canonical block
  header.

State **outside** the envelope:

- Wall-clock time on a specific validator.
- OS RNG output.
- The contents of a sibling process the validator happens to be
  running.
- A query response from an off-chain indexer.

Inside-envelope state is consensus-deterministic by construction;
outside-envelope state is whatever the validator's local environment
says it is. Verifiability via inline proof requires the queried state
to be inside the envelope. Verifiability via challenge game does not.

The whole question of "verifiable indexes" is the question of whether
index state can be brought inside the envelope — and at what cost.

---

## 2. The Arkiv goal: a queryable blockchain

Arkiv's stated goal, from the first-principles document:

> A modular database chain architecture that makes data a first-class
> citizen in web3. It combines the trust guarantees of blockchain
> (ownership, immutability, tamper-proof provenance) with the query
> capabilities of traditional databases (filters, indexes, SQL-like
> queries).

Decomposed:

- **Trust guarantees of blockchain**: cryptographic verifiability,
  deterministic execution, censorship resistance, decentralised
  operation. The full Web3 invariant set.
- **Query capabilities of traditional databases**: structured data,
  indexed access, expressive query language (filters, ranges,
  composites, sorting, paging). The full SQL-flavoured database surface.

Existing systems pick one side or the other. Bitcoin and Ethereum
provide trust guarantees but no built-in indexed queries; you query
state by exact key, and richer queries require external infrastructure
the chain itself does not endorse. Postgres, MongoDB, Elasticsearch
provide indexed queries but no chain-level trust. Hybrid systems
exist (The Graph, blockchain explorers, custom indexers) but always
trade off in one direction or the other — they bolt indexes onto an
unchanged chain, with verifiability mechanisms layered on top.

Arkiv's claim is that you can build a system where both are first-class
properties of the same artefact. The interesting question is what that
costs.

### 2.1 What this requires that Bitcoin / Ethereum don't have

A chain like Ethereum gives you, by way of indexed access:

- A state trie indexed by `(account, storage_slot)`. Lookups by key
  are O(log N) and verifiable via `eth_getProof`.
- That's it. Anything else — "find all accounts that hold token X",
  "list the most recent transactions on contract Y", "find blocks
  where event Z was emitted with parameter W" — requires an external
  indexer.

For Arkiv's ambition you also need:

- **Secondary indexes** keyed by data attributes, not just by storage
  slot. ("Find all entities where `attribute.priority = 5`.")
- **Range and prefix queries** over those attributes. ("Find entities
  with `priority >= 5`.")
- **Composite queries** combining multiple attributes. ("Find
  entities with `type = "note" AND priority >= 5`.")
- **Pagination** for any query that returns many results.
- **Sorting** by attribute or insertion order.
- **Verifiability** for each of the above, against on-chain state.

Each of these is straightforward in a traditional database. None of
them are present in Ethereum's built-in surface. Building any of them
in a way that satisfies blockchain trust guarantees is the technical
challenge.

The natural way to provide them is a sibling state machine
(EntityDB, in Arkiv's case) that maintains the indexes. The natural
question — the subject of this document — is: what does that sibling
state machine's relationship with chain consensus need to be?

### 2.2 What "first-class data" means concretely

The first-principles document describes the goal as data being
"owned, tamper-proof, queryable, composable":

- **Owned**: each data item has a clear on-chain owner. Modifications
  are authorised. (`Owner Authorization` invariant.)
- **Tamper-proof**: each data item is committed to under chain state.
  Modifications go through transactions. (`Transaction Provenance` +
  `Data Verifiability` invariants.)
- **Queryable**: secondary indexes provide rich query access — not
  just key-value lookup. (Implied by Data First principle; mechanism
  not yet specified at the principles level.)
- **Composable**: data items can be referenced by other data and
  other contracts. (Implied; needs a stable identifier scheme, which
  the entity-key derivation provides.)

The first three properties have natural mechanisms in the existing
chain primitive set. **Queryable** is the one that does not. It
requires either:

(a) An *index* that is itself part of chain state, queryable via the
standard verifiability mechanisms.
(b) An indexer outside the chain, with verifiability mechanisms
layered on top (signed responses, challenge games, computational
proofs).

(a) and (b) have different cost profiles, different verifiability
strengths, and different implications for what a permissionless node
operator has to do. The choice between them is the subject of §3
through §6.

---

## 3. The core tension

State as cleanly as possible:

> **Verifiable indexes require consensus on index state.**
> **Consensus on index state forces index work into the
> block-execution path.**
> **Block-execution work is bounded by validator capacity and
> block-time aggressiveness.**
> **Index work scales with state size.**
>
> Therefore: **index richness × state size × block-time aggressiveness
> is bounded by validator capacity, and the bound tightens
> monotonically as the chain grows.**

Each link merits unpacking.

### 3.1 Verifiable indexes require consensus on index state

The strongest form of verifiability — inline Merkle proofs against the
chain state root — requires the queried state to be inside the
consensus envelope. For an index, that means the index data
structure's own state has to be committed under the chain state root.

The mechanism: the index sits in some Merkle-shaped data structure
(a trie, a sparse Merkle tree, a verkle tree); the structure's root
is stored in chain state; when a query returns a result, it includes
a Merkle path from the result back to the index root, and the verifier
checks the path against the on-chain root.

This is exactly how `eth_getProof` works for storage slots, and how
the v2 design's `arkiv_stateRoot` works for entity records. The
structural property: the index has to be a Merkle data structure, and
its root has to be in chain state.

Weaker forms of verifiability — challenge games, signed responses —
do not require the index to be in chain state. They require the
*items the index points at* to be verifiable, but the index itself
can be eventually-consistent or even centralised, with the challenge
mechanism backing it.

This first link is therefore conditional: **strong verifiability**
requires consensus on index state. Weaker verifiability does not. The
choice between strengths is what the rest of this document hinges on.

### 3.2 Consensus on index state forces index work into the block-execution path

If the index is in chain state, then every state transition that
modifies indexed data must update the index *as part of the
transition*. The state root after the block must be the state root
that includes the post-update index. There is no place for the index
update to live except inside block execution.

Concretely: a transaction that creates a new entity has to perform
all the index updates that follow from the new entity's existence
(adding it to bitmaps for every attribute it has, updating sort
indexes, etc.) before the block in which the transaction is included
can be sealed. The state root has to commit to all those updates.

This is what "synchronous indexing" means in a blockchain context. It
is forced by the verifiability strength chosen in §3.1; it isn't a
choice the design can route around.

The alternative — async indexing with eventual consistency — gives up
the strength claim. It does not give up verifiability entirely, but it
moves verifiability to a different mechanism (challenge games or
similar).

### 3.3 Block time is bounded by liveness and validator capacity

Block time is not a free parameter that can be dialled up to
accommodate slow indexes. It is bounded by:

- **Network propagation.** Blocks have to be propagated to all
  validators within a fraction of the block time, or consensus
  forks. For a global validator set on commodity internet, this
  imposes a floor of tens to hundreds of milliseconds.
- **Consensus protocol overhead.** PoS protocols require multiple
  rounds of message-passing per slot. Each round is bounded below by
  network latency.
- **MEV and ordering concerns.** Faster blocks make MEV games more
  expensive to play; slower blocks give attackers more opportunity.
  Most chains pick a sweet spot.
- **User experience.** Slow blocks (>10s) make the chain feel
  unresponsive to users. Fast blocks (<1s) are an aspirational
  property, not a low-cost one.
- **Validator capacity.** Every block has to be executed within the
  block time on every validator's hardware. The slowest required
  hardware is the floor.

OP Stack mainnet currently runs 2-second blocks. Arkiv's L3 design
spec aspires to 1-second blocks, with subsecond as a stretch goal.

A 1-second block time means every validator has up to ~1 second to
execute the block. After subtracting consensus overhead, network
propagation slack, and a margin for safety, the actual on-validator
execution budget is more like 200-500ms. Within that budget the
validator must:

- Execute every transaction in the block (EVM work).
- Update every index every transaction touches.
- Compute the new state root.
- Persist the new state to durable storage.
- Sign and propagate the resulting block.

Index work is one line in that list, but it can grow without bound if
state grows without bound. Which leads to the next link.

### 3.4 Index work scales with state size

§1.1 made this case. To restate it in the chain context:

- Trie inserts are O(log N), but with constant factors that grow
  with disk I/O once cache is exceeded.
- Bitmap maintenance is O(K) where K is the cardinality of the
  bitmap (i.e., the number of entities sharing a particular attribute
  value), unless the implementation is clever about chunked or
  differential storage.
- LSM compaction overhead grows with state size and produces
  unpredictable P99 spikes.
- Merkle root computation requires hashing every node up the path; the
  path length is log N but the number of paths touched per block grows
  with the number of operations per block.

For a chain with 10⁶ active entities, none of this is a bottleneck
for any modern hardware. For 10⁹, every constant factor that was
ignorable becomes load-bearing. The chain that worked fine at launch
finds itself under increasing pressure as adoption grows — exactly the
opposite of the "scales gracefully with adoption" property a database
chain is trying to advertise.

### 3.5 The triangle

Three properties that all want to be simultaneously true:

```
                Strong query verifiability
                       (inline proofs)
                            /\
                           /  \
                          /    \
                         /      \
                        /        \
            Aggressive /          \ Permissionless
            block time/            \ validator set
                     /              \
                    /                \
                   /__________________\
                Index work fits in block budget
                  at any plausible state size
```

Pick any two; the third is constrained.

- **Strong verifiability + aggressive block time** ⇒ validators
  cannot be permissionless at scale; only well-resourced
  professional operators can keep up.
- **Strong verifiability + permissionless validators** ⇒ block time
  cannot be too aggressive; a generous budget is needed for
  validators on commodity hardware.
- **Aggressive block time + permissionless validators** ⇒
  verifiability has to weaken; indexes cannot be in the state root
  if they're going to grow.

The triangle is not a claim that the corners are unreachable. It's a
claim that you cannot get arbitrarily close to all three simultaneously.
At small state sizes all three are tractable. As state grows, the
constraints visible at large scale start to bite, and the design has
to commit to which corner it gives ground on.

This is the database-chain version of CAP/PACELC. It isn't CAP
literally — partition tolerance isn't the dimension under tension here
— but the *shape* is the same: three properties, only two simultaneously
achievable in the strongest forms, and the choice has to be made
consciously rather than discovered under load.

---

## 4. The tension within Arkiv's principles framework

The first-principles document defines nine architecture invariants. The
ones that participate directly in the tension above are listed below,
with what each pulls toward.

### 4.1 Provable Execution

> All state transitions must be provable in the settlement layer's
> dispute mechanism. ... Any modifications to the execution layer must
> have corresponding implementations for provability.

What it pulls toward: deterministic execution, bounded per-block work,
fault-proof completeness. Every operation in a block has to be
re-executable in a fault proof; if the operation is heavy, the fault
proof is heavy too.

This invariant is satisfied as long as execution is deterministic and
the operations are bounded. It doesn't directly mandate where indexes
live. It does mandate that *whatever* the design does, it must be
re-executable in the proof system, which has its own latency and
resource budget.

### 4.2 Data Verifiability — the load-bearing invariant

This is where the design decision actually lives. The invariant text
distinguishes two cases:

> **Data item verification**: Each data item has a deterministic
> hash computed from its content. This hash is part of the state
> Merkle tree. Clients can verify any data item by checking its hash
> and Merkle proof against the on-chain state root.

This is **strong inline-proof verifiability for individual data items**.
Each data item is in chain state via a hash commitment; the proof is a
Merkle path against the state root.

> **Query verification**: Query results are signed by the responding
> node. If a node returns an incorrect result set — omitting matching
> items or including non-matching ones — any party can challenge by
> providing data item content that contradicts the signed result.

This is **challenge-based verifiability for query results**. Signed
responses + bonded disputes + challenge resolution.

The two are very different in their requirements:

| Property | Inline proof verifiability | Challenge-based verifiability |
|---|---|---|
| What's in chain state | The data items themselves (Merkle commitment) | The data items themselves; the index need not be |
| Index location | Must be in state root | Can be off-chain, eventually consistent |
| Verification cost (client) | Hash + Merkle path check | Verify signature; in dispute, run challenge |
| Verification time (steady state) | One RPC round-trip | One RPC round-trip |
| Verification time (under fraud) | Detected immediately | Detected within challenge window |
| Trust required | None (cryptographic) | Bonded honesty (economic) |
| Permissionless | Yes (anyone with a verifier) | Yes (anyone with bond + watchful counterparty) |
| Fits async indexing | No | Yes |
| Per-block validator cost | High (sync index update + Merkle root) | Low (just write the data items) |

The crucial observation: the principles framework already accepts the
challenge-based model for queries. §4.7 even flags it as
work-in-progress, with an "ADR required" tag for the dispute mechanism.
The strong-verifiability claim is reserved for *data items*, where the
inline-proof model applies and indexes don't enter the picture.

### 4.3 Permissionless Operation

> Anyone can verify, operate nodes, and deploy database chains without
> permission.

What it pulls toward: low resource floor for validators, no
gatekeeper, public data availability.

The permissionless property bounds what a validator can be asked to
maintain. If running a validator requires a 1TB SSD with NVMe fsync
performance to keep up with index updates, the set of operators that
can validate shrinks. If the set shrinks too far, "permissionless" is
permissionless in name only.

There is no fixed line here — what counts as "anyone" is a values
question — but the direction of the pull is clear: the lower the
resource floor, the more meaningfully permissionless the chain.

### 4.4 Sustainability and Data First (the soft pull)

Two principles, not invariants, but they pull in opposing directions
on this question.

**Sustainability** wants value to exceed cost. Heavy in-consensus
indexing is expensive: per-validator hardware, per-block compute,
storage growth. The cost has to be passed somewhere — to users via
fees, to operators via subsidies, to investors via token issuance. The
heavier the indexing, the harder the economics.

**Data First** wants rich query capability. Range queries, full-text
search, composite filters, sort orders — the more capable the query
language, the more useful the chain is. Each capability typically
requires its own index. More indexes mean more per-block work.

These two pull against each other inside the tradeoff triangle.
Sustainability favours minimal indexing; Data First favours rich
indexing. Where the architecture lands is a values choice that
isn't determined by the invariants alone.

### 4.5 The §4.7 escape hatch and what it implies

Re-read the Data Verifiability invariant carefully. The query-side
mechanism it specifies is:

> Query results are signed by the responding node. If a node returns
> an incorrect result set — omitting matching items or including
> non-matching ones — any party can challenge by providing data item
> content that contradicts the signed result.

Followed by:

> **Architecture Decision Record Required**: Query verification is
> design work remaining. The mechanism described above is
> directionally correct, but details — challenge game design,
> economics/bonds, completeness proofs, on-chain vs off-chain
> resolution — require dedicated decision record.

This is a deliberate concession. The principles framework, as
currently written, does **not** require query results to come with
inline Merkle proofs. It accepts a challenge-game model as
directionally correct, and flags the details as ADR-pending.

This concession is significant. It implies that the principles
framework permits — and the ADR process is expected to consider —
designs in which:

- Indexes are not in chain state.
- Query results are signed responses with bonded honesty.
- Disputes are resolved on-chain via fraud proofs over data items.

A design that takes this concession seriously would land in column 3
of §6's design space. It would not require synchronous in-consensus
indexing. It would not be subject to most of the latency and capacity
pressure §3.5 describes.

The v2 design *over-strengthens* the verifiability commitment for
queries, by putting the index state root (`arkiv_stateRoot`) into
chain state via the system call from `BlockExecutor::finish`. This
provides inline-proof verifiability for queries — which is stronger
than the principles require — at the cost of forcing all index work
onto the synchronous block-execution path, with all the latency and
capacity implications that follow.

This is not a criticism of the v2 design. Stronger verifiability is
genuinely valuable, and there are use cases where the cost is
worthwhile. But it is an architectural choice that the framework
permits without mandating, and the cost of that choice is what every
prior section of this document has been describing.

The §4.7 ADR is therefore the pivot point. Resolving it explicitly —
choosing the strength of query verifiability the system commits to —
determines almost everything else about the design space.

---

## 5. Existing systems and where they sit

It helps to look at how existing systems have resolved this tension,
because every coherent point in the design space has at least one
example in production.

### 5.1 Bitcoin / Ethereum — no built-in indexing

Bitcoin and Ethereum take the simplest position: **no built-in indexed
queries**. The state is committed to under a Merkle root; you can do
key-based lookups (`getStorageAt`, `getBalance`, `getTransactionByHash`)
with inline proofs, but anything else requires an external indexer.

Where this lands in the framework:

- Verifiability strength: inline proof, but only for key-based lookups.
- Index location: outside the chain entirely, in clients (block
  explorers, wallets, dApp backends).
- Per-validator index cost: zero.
- Query richness: built-in is minimal; richness is provided by the
  ecosystem of off-chain indexers, with their own trust assumptions.

This is the column-zero position: the simplest possible answer to the
tension is "don't have indexes". Everyone using Ethereum for
non-trivial queries deals with this by running their own indexer or
trusting someone else's.

### 5.2 The Graph — eventual-consistency indexer

The Graph layers an indexed-query system on top of Ethereum (and other
chains) without modifying the chain itself. Indexers run subgraphs —
WASM programs that subscribe to chain events and maintain a Postgres
database keyed however they like. Queries hit the indexer's Postgres
and return results.

Verifiability: indexers stake GRT tokens. Wrong results can in
principle be challenged by a separate "fisherman" role that submits
fraud proofs. In practice the dispute mechanism has had limited use
and most users trust indexers.

Where this lands:

- Verifiability strength: challenge-based, weakly enforced in
  practice.
- Index location: outside the chain, in indexer Postgres instances.
- Per-validator index cost: zero (chain validators are unaffected;
  separate indexer set runs the indexes).
- Query richness: high (full SQL via Postgres or GraphQL).

This is the column-three position taken to its extreme: the chain
itself does no indexing work, the index is fully off-chain, and
verifiability is by challenge.

### 5.3 Cosmos SDK chains with custom modules

Cosmos SDK chains can implement custom modules that maintain in-chain
indexes. Example: the cosmos-sdk's `bank` module maintains a balance
index per address per denom, queryable via `QueryAllBalances`. The
index is part of chain state; it's updated synchronously with every
balance-changing transaction; queries return inline-proof-verifiable
results.

Where this lands:

- Verifiability strength: inline proof against chain state root.
- Index location: in chain state.
- Per-validator index cost: proportional to the index's complexity;
  for `bank` it's a per-address-per-denom map, manageable.
- Query richness: limited to whatever the module's specific
  `Query*` methods expose. No general-purpose query language.

This is the column-one position with a constraint: only specific,
designed-in queries are available. There is no general "find all X
where Y matches" — only the queries the module's authors anticipated.
For chains where the query set is known and limited, this works well.
For databases-as-product, it's too restrictive.

### 5.4 zk-indexed and SNARK-verified systems

A growing class of designs use zk-proofs to verify query results
against on-chain state. The querier runs the query, generates a
SNARK that attests the query is correctly evaluated against committed
state, and returns (result, proof). The verifier checks the proof
against the on-chain state root.

Examples: HyperOracle, Brevis, Axiom (in some configurations),
Lagrange's State Committee.

Where this lands:

- Verifiability strength: computational proof (cryptographically
  strong, no trust required).
- Index location: outside the chain (the prover maintains its own
  data structures); the chain only stores commitments.
- Per-validator index cost: zero (validators verify proofs, not
  rebuild indexes).
- Query richness: limited by what the proof system can express
  efficiently. Generic SQL is hard; specific common patterns
  (aggregations, filters over event logs, etc.) are tractable.

This is a column-two position that takes the form "data items are
committed, indexes are off-chain, verifiability is computational
rather than economic". As proof systems improve, this position becomes
more viable for general queries.

### 5.5 Where Arkiv's v2 design sits

The v2 design lands squarely in column one (strong / synchronous /
in-state-root), with one subtle modification: the index state root
(`arkiv_stateRoot`) is committed in-block via the system call from
`BlockExecutor::finish`, so query results have inline-proof
verifiability against chain state at the same block they describe.

This is more aggressive than the cosmos-sdk position because the
indexes Arkiv supports are richer (annotation bitmaps, prefix
indexes, etc.) and the v2 spec aspires to general query capability
rather than designed-in queries only.

It is more aggressive than the Postgres-on-blockchain or
zk-indexed positions because the index work is fully synchronous and
fully in chain state, with the per-validator cost that implies.

Within the design space outlined in §6, v2 is committing to the
hardest corner of the triangle. The cost — and the question this
document is asking — is whether that commitment is the one the
principles actually require, or one the design has chosen
over-strongly.

---

## 6. Three coherent positions in the design space

The space is parameterised by where indexes live and how query
results are verified. Three positions are coherent.

### 6.1 Strong: indexes in the state root

**Where indexes live**: in chain state, committed under the chain
state root via a Merkle-shaped data structure (entity trie + bitmap
roots, or similar).

**How query results are verified**: inline Merkle proof against the
on-chain state root.

**Per-block validator work**: full index update for every operation in
every transaction. Synchronous with block production; all of it must
fit in the block's execution budget.

**Threat model**: trustless. Wrong results are not just challengeable,
they're impossible — a node returning a wrong result simply can't
produce a Merkle proof for it.

**Examples in production**: Cosmos SDK custom modules (for restricted
query sets); Arkiv v2 (for general indexed queries).

**Cost profile**: heaviest. Block time bounded by index work; state
growth puts continuous pressure on the budget; the validator set is
constrained to operators who can keep up.

### 6.2 Hybrid: data items committed, indexes signed

**Where indexes live**: indexes themselves are off-chain (or
in-process but off the consensus path); the data items the indexes
point at are committed under the chain state root.

**How query results are verified**: the index responder signs the
result. If the result is wrong (the responder claims an item that
doesn't match the query, or omits one that does), any party can
provide the contradicting data item content (via inline Merkle proof
against the chain state root) and trigger a dispute that ends with
the responder slashed.

**Per-block validator work**: only the data-item commit. Indexes are
maintained on the responder's own schedule (typically asynchronously,
reading from chain state).

**Threat model**: economic. Honesty is bonded. Wrong results trigger
slashing within the challenge window; persistent malice is
unprofitable.

**Examples in production**: this is what Arkiv's first-principles §4.7
describes and tags ADR-required. Variants exist in the wild (data
availability committees, optimistic rollups for indexing).

**Cost profile**: medium. Validators do less work; indexers do more.
The economic mechanism (bonds, dispute resolution) has its own
infrastructure cost. Query richness is limited only by what the
indexer can compute, not by what fits in a block.

### 6.3 Weak: signed responses with challenge games

**Where indexes live**: fully off-chain. The chain has no index
state; only the per-data-item commitments live on-chain.

**How query results are verified**: signed responses, with challenge
games over the *index's input* (the data items it claims to be
indexing). A wrong result can be disputed, but the disputer has to
provide the corrected result themselves.

**Per-block validator work**: zero index work. Validators execute
state transitions over data items; indexes are someone else's
problem.

**Threat model**: economic, with weaker guarantees than column 2.
The index responder might be wrong in subtle ways that aren't easy
to challenge (omitting items rather than including wrong ones is
harder to detect). Watching is required.

**Examples in production**: The Graph; most blockchain explorers;
custom indexers maintained by dApp teams.

**Cost profile**: lightest for the chain; heaviest for users (who
have to either trust indexers or run their own). Query richness is
unbounded — full SQL or whatever the indexer supports.

### 6.4 Comparison

| Property | Strong (col 1) | Hybrid (col 2) | Weak (col 3) |
|---|---|---|---|
| Verifiability strength | Inline Merkle proof | Signed + challenge | Signed + challenge |
| Index in chain state | Yes | No | No |
| Per-block validator work | High | Low | Zero |
| Query richness ceiling | Bounded by block time | Bounded by indexer capacity | Bounded by indexer capacity |
| State-growth pressure | Continuous on validators | On indexers, not validators | None on validators |
| User trust required | None | Bond-backed honesty | Bond-backed honesty + watchful party |
| Permissionless validator | At small state size | Yes | Yes |
| Permissionless indexer | Same as validator | Yes (with bond) | Yes (with bond) |
| Challenge mechanism | Not needed | Required | Required |
| First-principles compliance | Stronger than required | Matches §4.7 framing | Matches §4.7 framing |

Column 1 is what Arkiv v2 implements. Columns 2 and 3 are what the
Arkiv principles permit and (per §4.7's ADR-required tag) explicitly
contemplate.

### 6.5 Combinations and pivots

The columns are not strictly mutually exclusive. Realistic designs
combine elements:

- **Item-level inline + query-level challenge.** Individual data items
  are inline-verifiable (their hashes are in chain state), but query
  result sets are signed-and-challengeable. This is what Arkiv's
  §4.7 invariant text actually describes — and it's a column-2
  position with stronger item-level guarantees than naive column 2.
- **Strong indexes on a subset of data, weak on the rest.** Some
  attributes are indexed in-state-root (high-value, low-cardinality);
  others are off-chain (long-tail). Validators only do the work for
  the strong subset.
- **Strong commitment, asynchronous build.** The chain commits to a
  "version number" of the index state (per-block, monotone); the
  actual index is built off-chain to match each version. Queries
  return results + index version + signed assertion. Disputes prove
  that the index at a given version doesn't match the chain at the
  corresponding block. This is a column-2 position with a chain-side
  hook.
- **Hybrid evolution.** Start in column 1 with a small state, plan
  to migrate to column 2 if state growth makes column 1 untenable.
  Requires the verification mechanism to be ADR-defined upfront so
  the transition doesn't break clients.

The point isn't that the design has to be one of three. The point is
that any coherent design lands in some combination of columns, and the
combination determines the cost profile, threat model, and growth
trajectory.

---

## 7. Implications for the architecture

### 7.1 The pivot decision the architecture has not yet named

The v2 spec commits to column 1 implicitly, by including
`arkiv_stateRoot` in the EVM state via the system call from
`BlockExecutor::finish`. This is a significant architectural commitment
— it forces synchronous indexing, it puts state-growth pressure on
validators, it constrains block time aggressiveness — and it is
made by a single sentence buried in the BlockExecutor wiring.

The first-principles document, in §4.7, gestures at column 2 (or 3)
and tags the dispute mechanism as ADR-required.

These two documents are in **silent tension**. The principles say
"queries are challenge-verifiable, ADR pending". The design spec
implements "queries are inline-proof-verifiable via in-state-root
indexes". An ADR for §4.7 would have to either:

- Confirm column 1 (and note that the v2 design's strong
  verifiability is the chosen position; the latency/capacity
  constraints are accepted as the cost).
- Choose column 2 or 3 (and note that the v2 design needs to be
  revisited to drop the in-block `arkiv_stateRoot` commitment in
  favour of off-chain indexes with a challenge mechanism).
- Choose a hybrid (and specify which queries get inline proofs and
  which get challenges).

This is the decision the architecture has not yet named, and it's the
biggest architectural lever in the design.

### 7.2 What an ADR for §4.7 would have to settle

Concretely, the ADR would have to answer:

1. **Verifiability strength for queries.** Inline proof, challenge
   game, or hybrid?
2. **If challenge game**: what is the dispute mechanism? On-chain or
   off-chain resolution? What bonds? What dispute window?
3. **If hybrid**: which queries get which treatment? On what basis
   does the system route a query to inline-proof or challenge?
4. **Completeness proofs.** Equality queries can be inline-proven
   (Merkle proof of the matching items). Range, prefix, glob queries
   cannot — there's no Merkle proof of "no match outside this set".
   How is completeness verified for those?
5. **Indexer set.** If indexes are off-chain (column 2 or 3), who
   runs the indexers? Validators? A separate set with its own
   incentives? Permissioned for a launch period?
6. **Reorg semantics for indexes.** Indexes have to track the chain.
   In-state indexes (column 1) reorg automatically with chain state.
   Off-chain indexes (column 2 or 3) need their own reorg-handling
   protocol. What is it?
7. **Migration path.** If the system starts in one column and needs
   to move to another (e.g., column 1 at launch, column 2 at scale),
   what is the migration protocol? Can clients keep working across
   the transition?

These are not cosmetic questions. They determine the deployment shape,
the operator economics, the user verification experience, and the
chain's growth trajectory.

### 7.3 The downstream consequences

The pivot decision in §7.1 has downstream consequences in three areas
the v2 spec already touches:

- **Block-time targets.** Column 1 with aspirational subsecond block
  time is the tightest version of the constraint triangle. Column 2
  or 3 give the block time back as an unconstrained design parameter.
- **Validator hardware floor.** Column 1 raises the floor as state
  grows. Column 2 or 3 keep it stable.
- **Out-of-band precompile coupling.** The whole content of
  [`precompile-out-of-band-coupling.md`](precompile-out-of-band-coupling.md)
  exists because the v2 design puts DB writes on the synchronous
  precompile path (column 1's premise). Column 2 or 3 designs put
  the DB writes on the ExEx path (post-canonical), and the
  multi-execution problem disappears.

In other words: the §4.7 ADR doesn't just settle a verifiability
question. It settles a cluster of questions that the design has been
treating as independent. The latency budget, the precompile shape,
the state-growth trajectory, and the verifier UX are all downstream
of the same root choice.

---

## 8. Open questions

These are concrete questions an ADR for §4.7 (and the surrounding
architecture work) should resolve.

1. **Verifiability strength for queries.** Inline proof, challenge,
   or hybrid? The v2 design assumes column 1; the principles permit
   any column. Pin this explicitly.

2. **Cost projections for column 1 at target scale.** What is the
   projected per-block index update cost at 10⁷ entities, 10⁸,
   10⁹? Against the target block-time budget, with the projected
   transaction-per-block load? If the projection breaks the budget
   at any of those scales, when?

3. **Cost projections for column 2 or 3.** What is the projected
   challenge-game cost — both the chain-side cost (dispute
   resolution) and the off-chain cost (indexer infrastructure,
   bond capital)? At what scale does the column-2/3 cost cross
   the column-1 cost?

4. **Completeness for non-equality queries.** Range, prefix, glob.
   Whatever column the design lands in, completeness is harder
   than equality. What's the answer?

5. **Migration path between columns.** If the launch design is
   column 1 and the long-term design is column 2, what does
   migration look like? Must the verifiability mechanism be
   forward-compatible from day one?

6. **Indexer permissionlessness.** If column 2 or 3, the indexer
   set has to be permissionless for the chain to satisfy the
   Permissionless Operation invariant. What does that look like
   operationally?

7. **Reorg-aware indexing.** Column 1 gets reorg handling for free
   from chain consensus. Columns 2 and 3 require an explicit reorg
   protocol for the index. What is it?

8. **Validator capacity floor.** What is the minimum hardware spec
   the chain commits to supporting at each scale? This is the
   concrete form of the Permissionless Operation invariant.

The first three are quantitative and require measurement. The rest
are design questions answerable by analysis.

---

## 9. Glossary

- **CAP / PACELC.** Distributed-systems frameworks for reasoning
  about availability vs consistency tradeoffs. CAP under partition;
  PACELC under both partition and steady-state operation. PACELC is
  the right tool for blockchain-with-indexes; CAP is too narrow.
- **Challenge game / fraud proof.** A verifiability mechanism in
  which claims are accepted by default but can be refuted by anyone
  publishing contradicting evidence within a time window. Cheap in
  the no-fraud case; requires economic security and watchful
  counterparties.
- **Computational proof.** A verifiability mechanism in which the
  responder produces a SNARK or STARK attesting that the response
  is the correct output of a known function over committed inputs.
  Cryptographically strong; verifier cost is independent of
  computation size.
- **Consensus envelope.** The set of state every honest validator
  agrees on via consensus. State inside the envelope is
  consensus-deterministic; state outside is per-validator-local.
- **Consistency (in PACELC sense).** All readers see the same data.
  Strong consistency is immediate; eventual consistency is
  bounded-by-time.
- **Database chain.** A chain whose primary value proposition is
  storing and querying structured data, as opposed to running smart
  contracts or settling assets. Arkiv is one. The Graph is not (it
  layers on top of other chains).
- **Eventual consistency.** A consistency model where reads
  eventually reflect all committed writes, but the exact lag is not
  bounded by the protocol — only by the implementation.
- **Inline proof verifiability.** A verifiability mechanism where
  the response includes a self-contained proof (typically Merkle)
  against an on-chain commitment. Cryptographically strong; trust
  required is only in the chain commitment.
- **In the state root.** Said of state that's committed under the
  chain's Merkle state root, accessible via `eth_getProof` against
  a canonical block header.
- **Permissionless.** Said of a role that any party can adopt
  without prior approval — validator, indexer, prover, querier.
  Bounded by the resource floor required to perform the role.
- **Synchronous indexing.** Index updates happen as part of the
  same write that updates the underlying data. Reads see
  immediately-consistent index state.
- **Asynchronous indexing.** Index updates happen lazily after the
  underlying write commits. Reads may see stale index state during
  the lag window.

---
