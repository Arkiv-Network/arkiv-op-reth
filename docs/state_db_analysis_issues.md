# Arkiv StateDB Design — Open Issues

Observations on [statedb-design.md](./statedb-design.md), ordered roughly
by impact. Recommendations at the end.

---

## 1. Fault proof stack is unstated (assumed: kona, not op-program)

The doc names `op-program` and `cannon` as the FP stack. Registering a Rust
precompile in op-reth's `EvmFactory` doesn't propagate to op-program (Go on
op-geth's EVM); the actual assumption is **kona** (Rust FP program) on
**asterisc** (RISC-V VM), where `arkiv-entitydb` compiles in directly.
This is a documentation correction rather than a design hole, but the
current wording can be misleading.

- Rewrite the FP paragraph in Section 8 and the FP references in Section 7
  to say kona + asterisc.
- Add a CI job that runs the same `arkiv_query` test vectors through both
  the Rust runtime and the kona FP build; fail on any divergence.

---

## 2. roaring64 byte format is consensus-critical and unpinned

`codeHash` is a per-account field in the Ethereum state trie holding
`keccak256(account.code)`. It is committed in every block's `stateRoot`,
so any byte-level change to an account's code changes the trie.

Content-addressed bitmaps require `codeHash = keccak256(bitmap_bytes)` to be
byte-identical across every node. The design doesn't currently pin which
serializer or which version. A patch bump in the `roaring` crate that
changes container ordering or run-length heuristics would change every
historical pair-account `codeHash`. Compounds with issue 1: kona and
op-reth need to use the same byte format.

- Pin `roaring` with `=x.y.z`, not `^x.y.z`.
- Document the canonical wire format independently of any library (header
  layout, endianness, container types).
- Commit a test-vector suite (`{ID set} → {bytes, keccak}`) and run it in
  CI on every dep update, in both the runtime and FP build.

---

## 3. EIP-170 bypass should be structural, not configurational

Entities and bitmaps are stored as account code and can exceed 24,576 bytes.
Two ways to make that work: raise `CfgEnv.limit_contract_code_size`, or
rely on the fact that `EvmInternals::set_code` writes directly to the
journal and never traverses the CREATE/CREATE2 return path where EIP-170
lives. The current design takes the second route, which is the better
choice here: the first one is a **global** EVM knob, and raising it to
accommodate large entities/bitmaps would simultaneously raise the cap on
every user-deployed dapp contract. That couples Arkiv's blob-size policy
to the chain's EVM-compatibility policy.

The structural bypass keeps user contracts at the standard 24KB while
letting precompile writes be unconstrained. Worth preserving that
decoupling and stating it explicitly in the design doc.

- Make this explicit in the design doc: cfg is left at the EVM default,
  the precompile relies on the structural bypass.
- Confirm at the pinned revm version that `set_code` does not call any
  code-size validator. Add a regression test: precompile-written > 24KB
  succeeds, user CREATE > 24KB still fails with `CreateContractSizeLimit`.
- Mirror that test to the kona FP build.

---

## 4. Cardinality explosion: griefing vector, gas-model conflict, categorical-only constraint

Likely the highest-leverage issue in this doc, and the one most worth
thinking through carefully. Every distinct `(attrKey, attrVal)` ever
observed creates a permanent pair account in the world state. The gas
formula in Section 5 charges per-attribute gas as a pure function of
calldata — **identical cost whether the pair already exists or is being
created**. Identical-cost first-touch opens a griefing path: an attacker
can submit `Create` ops where every attribute value is unique per
submission, pay normal L2 gas, and impose unbounded permanent state growth.

**Concrete attack.** Loop `Create` with one user attribute set to
`(k = "noise", v = <random 32B>)`. Each op costs the normal `G_CREATE +
G_ATTRIBUTE`; each op creates one new permanent pair account. Linear trie
growth at the cost of a single attribute per op.

**Gas-model conflict.** Charging first-touch differently means the
precompile needs to read prior trie state before pricing. That conflicts
with the "pure function of calldata" property Section 5 currently asserts.
The property is framed there as a consensus invariant, but it functions
more as a simplicity choice — determinism is preserved as long as both
nodes see the same prior state. The framing would need updating for
state-dependent pricing to land cleanly.

**Categorical-only data model.** The pair-account-per-`(k,v)` design
assumes attribute values are drawn from a small reusable set —
**categorical** data. With continuous-domain values (timestamps in seconds,
UUIDs, hashes, sizes, coordinates) the index degenerates into one pair
account per entity with a one-element bitmap, forever. At that point the
structure functions more as per-entity overhead than as a useful index.

The built-ins already approach this territory. `$key = entityKey` is
per-entity by construction (pure overhead). `$expiration` and
`$createdAtBlock` are continuous by nature; they only behave well when
SDKs cluster expiries at common block boundaries.

**Why the obvious fix is hard.** Grouping bitmaps under one "attribute-key"
indexed account would bound cardinality, but breaks the content-addressing
property — `codeHash` would no longer be the bitmap hash. So the
cardinality cost is structurally tied to the headline trick.

**The design would benefit from naming a stance.**

- *Categorical-only.* Contract validates that values belong to a known
  bounded set; continuous values rejected or routed to a separate (non-
  index) storage path. Simplest correct design; matches the index's
  actual capability.
- *Categorical + ordered sibling index for continuous values.* OQ1's range
  index becomes the home for continuous data; equality index stays
  categorical. Two indexes coexist.
- *Accept continuous values and price them.* First-touch gas surcharge,
  per-key cardinality caps, explicit state-growth budget.

The current design effectively lands on option 3 without pricing it in,
which is the gap to close.

Other mitigations regardless of stance:
- Drop `$key` — one pair account per entity for limited query benefit.
- Per-attribute-key cardinality cap at the contract layer, refuse `Create`
  once a key has more than N distinct values.
- Benchmark against an explicit attacker workload (1M Create ops with
  unique-per-op values) before launch.

---

## 5. MPT update cost per `Create`

A single `Create` with N user attributes touches **N + 13** logical state
locations across four accounts. At meaningful throughput, state-root
recomputation can dominate block production.

| Source                                       | Writes        | What                                                                                                                                                              |
| -------------------------------------------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `EntityRegistry` contract                    | 2             | `nonces[sender]` increment, `entities[entityKey]` insert                                                                                                          |
| System account                               | 3             | `entity_count`, `id_to_addr[id]`, `addr_to_id[addr]`                                                                                                              |
| Pair accounts                                | N + 7         | One `SetCode` per `(k, v)`: N user + 7 built-ins (`$all`, `$creator`, `$createdAtBlock`, `$owner`, `$key`, `$expiration`, `$contentType`)                          |
| Entity account                               | 1             | `SetCode(entity_address, 0xFE \|\| RLP)`                                                                                                                          |
| **Total**                                    | **N + 13**    |                                                                                                                                                                   |

Tracing to trie-node hashing: writes within one account share an
account-trie traversal, so the per-op count is `N + 10` account-trie
paths + `5` storage-trie paths. Each path re-hashes ~log_16(M) branch
nodes (~7 at 100M accounts). A 10-attribute Create lands ~175 branch-node
keccaks before leaf hashing — plausibly the dominant cost in block
production at high op rates.

Pair accounts are addressed at `keccak256("arkiv.pair" || k || 0x00 ||
v)[:20]`. The keccak scatters them uniformly, so the N + 7 traversals
share no common prefix and don't benefit from path-sharing.

- Benchmark a representative block: 50 Create ops × 10 user attributes.
  Compare state-root time vs. EVM execution vs. disk I/O.
- Consider an ops-per-block / attributes-per-op cap to bound worst-case
  block-production time.
- Dropping `$key` (issue 4) saves one trie traversal per Create forever.

---

## 6. No fee model; sequencer subsidizes state growth

OQ4 defers fees as "independent, can be deferred." Until then, the
sequencer pays L1 data costs + permanent storage cost while collecting
only L2 gas. Combined with issue 4, this is the economic shape of the
griefing path: an attacker can bloat permanent trie state at well below
the long-term cost of holding it.

- Decide between native gas surcharge baked into precompile formulas and
  an ERC-20 fee enforced by `EntityRegistry`.
- Model an attack budget against expected L2 gas pricing.

---

## 7. Historical queries require archive nodes

Section 4 promises historical queries at any retained block; Section 7's
verification flow assumes `eth_getProof(..., blockN)` and
`eth_getCode(..., blockN)` work cleanly. Both depend on op-reth's
`Bytecodes` retention. Most OP-stack validator/replica nodes run in pruned
mode, so the historical-query property quietly degrades on those endpoints.

- Document the archive-node requirement prominently.
- Decide deployment story: incentives for archive operators, a dedicated
  history-node role, or a third-party archival service.
- Confirm reth at the pinned version reliably serves historical
  `getProof` + `getCode` at the same block.

---

## 8. Predeploy address range is outside OP-stack convention

`0x4400…0044/45/46` sits outside the standard OP-stack predeploy range
(`0x4200…00` through `0x42…00FF`). OP-stack tooling (explorers, indexers,
superchain-registry) can miss these addresses without warning, and future
OP hardforks may reserve more of the `0x44xx` range upstream.

- Move to unused slots inside `0x4200…00`–`0x4200…FF`. See recommendations.

---

## 9. EOF and future EVM compatibility

The `0xFE` prefix on entity code depends on legacy code semantics
continuing to exist alongside EOF and any future code-format rules. EOF's
magic byte (`0xEF`) doesn't collide today, but pair-account code (raw
roaring bytes with no prefix) is more exposed if op-stack ever adopts
strict code-format validation.

- Add a one-liner to the design doc acknowledging the dependency on
  legacy-code support.
- Track EOF rollout on op-stack; have a contingency for moving entity /
  pair payloads from `code` into `storage` if the legacy path narrows.

---

## 10. Per-op tx-position metadata is zeroed

OQ5: `transaction_index_in_block` and `operation_index_in_transaction` are
reported as 0 because revm's precompile context doesn't expose them.
Without correct positional metadata, consumers can't deterministically
order ops within a block — which matters for ID assignment and any
event-sourced indexer.

- Plumb tx index / op index through a block-builder-side attribute
  surface before clients start depending on the zeros.

---

# Recommendations

- Explicitly describe that Arkiv database chains are **read-first** and
  expensive to write data in the system, meaning we accept that `Create`,
  `Update` etc. are exceedingly more complex than mere query of the data.

- Introduce attribute-system limitations:
  - Higher gas cost / penalty for first-touch of Entity account, indexes etc.
  - Upper cap on the attribute categories, potentially.

- Introduce **un-roaring-bitmapped attributes** that exist but are not
  queriable with equality, so a bucketing system can be introduced later
  (post-mainnet). For now, the mere ability to present an attribute that
  does not update roaring bitmaps is enough.

- Introduce a **zero-cost attribute reads** path for entities. Reserve
  the first `MAX_KEYS * (MAX_ATTR_KEY_LEN + MAX_ATTR_VAL_LEN)` of every
  entity account, similar to how topics work in Ethereum logs. This makes
  quick access to such attributes cheap.

- Introduce a **versioning byte** right after `0xFE`, as the second byte
  in code, to allow upgradability of entities later in the protocol.

- Stick to the predeploy range provided by the Optimism stack: the
  standard OP-stack predeploy range is
  `0x4200000000000000000000000000000000000000` through `0x42…00FF`.
