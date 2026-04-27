# Architecture

This document describes the design of the `arkiv-op-reth` workspace: what
each crate does, how they fit together, what flows through the system at
runtime, and why the boundaries are drawn the way they are.

For the JSON-RPC wire format the ExEx posts to EntityDB, see
[`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md). For the
contract itself see the [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts)
repo.

---

## 1. What this software does

### 1.1 The Arkiv chain

Arkiv is an OP-stack L2 with one additional standard predeploy:
`EntityRegistry`. The contract sits at the canonical address
`0x4200000000000000000000000000000000000042` and exposes six operations
on opaque entities — `CREATE`, `UPDATE`, `EXTEND`, `TRANSFER`, `DELETE`,
`EXPIRE` — submitted in batches via `execute(Operation[])`.

The contract stores a minimal commitment per entity (creator, owner,
expiry, content hash) and emits per-operation events. Full entity payloads
and attributes live in calldata, not on-chain storage. A rolling
`changeSetHash` — produced by chaining each operation's hash into the
previous head — is exposed via view functions and emitted with every
operation, giving downstream consumers a verifiable single-value witness
of the chain's entity state.

### 1.2 The off-chain indexer

EntityDB is a Go service that consumes the calldata-only entity stream
and serves indexed queries (by entity key, by owner, by attribute, …).
It's the canonical reader; clients of the Arkiv chain talk to EntityDB,
not directly to the contract.

### 1.3 The role of this repository

`arkiv-op-reth` is the bridge: an op-reth-derived execution node that
decodes EntityRegistry calldata + events from each canonical block and
forwards a structured representation to EntityDB. The forwarding lives
inside an Execution Extension (ExEx) — reth's mechanism for
post-execution side effects.

---

## 2. System overview

```
   ┌─────────────────────────────────────────────────────────────┐
   │ arkiv-node binary                                           │
   │                                                             │
   │  ┌───────────────────┐                                      │
   │  │   reth engine     │  ChainCommitted / ChainReorged       │
   │  │   (op-reth)       │  / ChainReverted notifications       │
   │  │                   │ ───────────────────────┐             │
   │  └─────────┬─────────┘                        │             │
   │            │ executes blocks                  ▼             │
   │            │                          ┌──────────────┐      │
   │            ▼                          │  Arkiv ExEx  │      │
   │     ┌─────────────┐                   │              │      │
   │     │  state DB   │ ◄── slot reads ── │  decode +    │      │
   │     │  (MDBX)     │   for rolling     │  forward     │      │
   │     └─────────────┘   changeset hash  └──────┬───────┘      │
   │                                              │              │
   └──────────────────────────────────────────────┼──────────────┘
                                                  │ Storage trait
                                  ┌───────────────┴───────────────┐
                                  │                               │
                                  ▼                               ▼
                          ┌──────────────┐               ┌────────────────┐
                          │ LoggingStore │               │ JsonRpcStore   │
                          │ (tracing)    │               │ (HTTP → Go DB) │
                          └──────────────┘               └────────────────┘
```

The binary's job is to turn reth's chain notifications into well-typed,
serialisable Arkiv operations and hand them off to whichever Storage
implementation is configured. Everything else (block production, p2p,
state storage, RPC) is unchanged op-reth.

---

## 3. Workspace crates

### 3.1 `arkiv-genesis`

A small library shared by both binaries. It owns:

- The canonical predeploy address constant (`ENTITY_REGISTRY_ADDRESS =
  0x42…0042`).
- The dev account constant (`DEV_ADDRESS`, the first hardhat-mnemonic
  account).
- `deploy_creation_code(chain_id) -> Result<Bytes>` — runs
  `arkiv_bindings::ENTITY_REGISTRY_CREATION_CODE` through revm at the
  given chain ID and returns the resulting runtime bytecode. The chain ID
  is forwarded to revm's `block.chainid` so the EIP-712 domain separator
  baked into immutables matches the target chain.
- `predeploy_account(chain_id) -> Result<GenesisAccount>` — convenience
  for splicing into a `Genesis.alloc`.
- `genesis_alloc(chain_id) -> Result<BTreeMap<Address, GenesisAccount>>`
  — predeploy + dev account, suitable for self-contained dev chains.
- Re-exports `alloy_genesis::{Genesis, GenesisAccount}` so consumers
  don't need a direct alloy-genesis dep.

This crate is the single place that knows how to materialise the
EntityRegistry runtime bytecode for a given chain. Both `arkiv-node`
(currently unused, but available for future built-in chainspec support)
and `arkiv-cli inject-predeploy` consume it. The bytecode generation is
deterministic — same chain ID + same `arkiv-bindings` rev produces the
same bytes — which is what lets `init` and `node` agree on the genesis
hash even when the chainspec was assembled at recipe-time.

### 3.2 `arkiv-node`

The execution-client binary. It's a thin wrapper around `reth_optimism_cli::Cli`:

```rust
fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|builder, rollup_args| async move {
        let arkiv_active = has_arkiv_predeploy(&builder.config().chain);

        let mut node = builder.node(OpNode::new(rollup_args));
        if arkiv_active {
            let store = build_store().await?;
            node = node.install_exex("arkiv", move |ctx| async move {
                Ok(arkiv_exex(ctx, store))
            });
        }

        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
```

There is **no chainspec mutation**. The chainspec is whatever `--chain`
loaded; we never reach into `config_mut()` to inject anything at startup.
This is what lets the binary serve as a true drop-in op-reth.

#### 3.2.1 ExEx auto-activation

Whether to install the Arkiv ExEx is decided once, at startup, by
`has_arkiv_predeploy(&chain)`:

```rust
fn has_arkiv_predeploy(chain: &OpChainSpec) -> bool {
    let chain_id = chain.inner.chain.id();
    let Some(account) = chain.inner.genesis.alloc.get(&ENTITY_REGISTRY_ADDRESS)
    else { return false; };
    let Some(code) = &account.code else { return false; };
    let Ok(expected) = arkiv_genesis::deploy_creation_code(chain_id)
    else { return false; };
    keccak256(code) == keccak256(&expected)
}
```

It checks the loaded genesis alloc for our predeploy address, computes
the expected runtime bytecode for *this* chain's chain ID, and compares
hashes. Address-presence alone isn't sufficient: a hostile chainspec
could squat at `0x42…0042` with unrelated code. The hash equality check
makes activation a property of the chainspec content, not of either the
chain ID or the binary's identity.

Consequences:

| `--chain` | Behavior |
|---|---|
| `optimism`, `base`, `op_sepolia`, … (vanilla OP) | No predeploy → ExEx skipped → vanilla op-reth |
| Path-A JSON with predeploy spliced in | ExEx active → Arkiv mode |
| Path-A JSON squatting at the address with wrong bytecode | Hash mismatch → ExEx skipped, INFO log |

#### 3.2.2 Storage backend

`build_store()` selects between two implementations based on the
`ARKIV_ENTITYDB_URL` env var:

- **Set** → `JsonRpcStore` posts to that URL. Performs a startup health
  check; if the URL is unreachable the binary exits cleanly rather than
  crashing the ExEx mid-stream.
- **Unset** → `LoggingStore` emits structured tracing events for every
  operation. Useful for `--dev` workflows and integration tests.

Both implement `trait Storage`:

```rust
pub trait Storage: Send + Sync + 'static {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>>;
    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>>;
    fn handle_reorg(&self, reverted: &[ArkivBlockRef], new_blocks: &[ArkivBlock]) -> Result<Option<B256>>;
}
```

The optional `B256` return is a state-root hash for backends that
maintain a Merkle trie (currently only the JSON-RPC backend forwards what
EntityDB returns). The trait is sync; `JsonRpcStore` bridges to async via
`tokio::task::block_in_place` since the ExEx loop runs on a multi-threaded
runtime.

### 3.3 `arkiv-cli`

The operator command-line tool. Two distinct surfaces:

#### 3.3.1 Entity operations (require an RPC endpoint + signer)

| Subcommand | What it does |
|---|---|
| `create` | Mint an entity with a random payload + content type |
| `update` | Replace an existing entity's payload |
| `extend` | Push out an entity's expiry |
| `transfer` | Hand ownership to another address |
| `delete` | Owner-initiated removal |
| `expire` | Anyone-callable removal of a past-expiry entity |
| `query` | Read a single entity's commitment |
| `hash` | Print the contract's current rolling `changeSetHash` |
| `history` | Walk the per-op hash chain from head to genesis |
| `balance` | ETH balance for the signer (or `--address`) |
| `spam` | Fire many CREATE txs with backpressure on the txpool |
| `batch <FILE>` | Submit an arbitrary sequence of ops in one tx (see below) |

Defaults are tuned for local dev (`http://localhost:8545`, the hardhat
mnemonic #0 key, `--chain arkiv` predeploy address). All overridable via
flags.

#### 3.3.2 Genesis post-processing (no network required)

```
arkiv-cli inject-predeploy <input.json> [--out output.json]
```

This subcommand short-circuits before any signer/provider setup. It:

1. Parses the input as a geth-format `Genesis`.
2. Reads `config.chainId`.
3. Calls `arkiv_genesis::deploy_creation_code(chain_id)` to mint the
   matching runtime bytecode.
4. Inserts a `GenesisAccount { code, .. }` at `ENTITY_REGISTRY_ADDRESS`,
   warning if there's already an entry there.
5. Writes the augmented JSON back (overwriting the input by default).

It works against *any* geth-format chainspec: a hand-crafted dev base, an
op-deployer-produced production genesis, a snapshot dump from another
node. The chain ID it reads from the input is what binds the predeploy's
EIP-712 immutables, so the resulting bytecode is correct for that chain.

### 3.4 The batch format

`arkiv-cli batch <FILE>` takes a JSON array of operations and submits them
as a single `execute()` call. The schema is loosely OP-flavored:

```json
[
  {
    "type": "create",
    "contentType": "application/json",
    "payload": "{\"v\":1}",
    "expiresIn": "1h",
    "attributes": [
      { "name": "title", "string": "the answer" },
      { "name": "priority", "uint": 42 },
      { "name": "linked.to", "entityKey": "$0" }
    ]
  },
  {
    "type": "update",
    "entityKey": "$0",
    "payload": "{\"v\":2}"
  }
]
```

Notable mechanics:

- **Cross-references**: `"$N"` in an `entityKey` field refers to the Nth
  op in the batch (which must be a CREATE). The CLI calls
  `registry.entityKey(signer, signer_nonce + create_index)` upfront —
  before submission, so the predicted address is stable — and substitutes.
  Literal `"0x…"` hex is accepted in the same fields for pre-existing
  entities.
- **Payload**: optional UTF-8 string (or `0x`-prefixed hex). Mutually
  exclusive with `size` (random bytes). Both omitted → empty payload.
- **Attributes**: validated client-side. Names go through
  `Ident32::encode` (lowercase ASCII, max 32 bytes); strings are capped
  at 128 bytes; values are packed per the `value128-encoding.md` rules.
  Auto-sorted ascending by name (the contract requires strict order).
- **Durations**: `expiresIn` accepts humantime strings (`"1h"`, `"7d"`).
  Resolved to a block number via the configured block-time.

See `scripts/fixtures/` for end-to-end examples.

---

## 4. Genesis construction

Genesis is the thorniest part of integrating with the OP stack, and the
design here has been iterated several times. The current rules:

### 4.1 No runtime mutation

Earlier versions of `arkiv-node` mutated the loaded chainspec in `main.rs`
— forcing chain ID 1337, injecting the predeploy, zeroing OP hardfork
timestamps, etc. This **broke `op-reth init`**: `init` reads the
chainspec untouched and writes its genesis hash to disk, while `node`
applied the mutation and computed a different genesis hash, producing the
classic
`genesis hash in the storage does not match the specified chainspec`
error.

The fix was to remove the mutation entirely. The chainspec is treated as
read-only data flowing in from `--chain`. Whatever needs to be in there
must already be in there before `--chain` is parsed.

### 4.2 Path A vs Path B

OP-reth supports two ways to build an `OpChainSpec`:

| | **Path A** (pure JSON) | **Path B** (programmatic forks) |
|---|---|---|
| Hardfork source | `config.{bedrockBlock, regolithTime, …}` + `config.optimism.{eip1559…}` parsed via `OpChainSpec::from(Genesis)` | JSON has only allocs/chainId; forks attached in code via `LazyLock` |
| Examples | `optimism`, `base`, `op_sepolia`, all superchain registry entries | `dev` (`OP_DEV`), the static `OP_MAINNET`/`BASE_MAINNET` definitions |
| `--chain ./file.json` | **Yes** — works end-to-end | **No** — forks won't be activated |
| `dump-genesis` roundtrips? | **Yes** | **No** — output is missing the hardfork data |

For an `--chain ./our.json` flow to work for both `init` and `node`,
the chainspec **must** be Path A. Otherwise reth loads it with no
hardforks active, the engine produces post-hardfork blocks anyway, and
validation explodes.

`chainspec/dev.base.json` is therefore Path A:

```json
{
  "config": {
    "chainId": 1337,
    "bedrockBlock": 0, "regolithTime": 0, "canyonTime": 0,
    "ecotoneTime": 0, "fjordTime": 0, "graniteTime": 0,
    "holoceneTime": 0, "isthmusTime": 0,
    "shanghaiTime": 0, "cancunTime": 0,
    "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 },
    ...
  },
  "extraData": "0x000000000000000000",
  "alloc": { "0xf39F…": { "balance": "0x21e19e0c9bab2400000" } }
}
```

The `extraData` value is the second subtle point.

### 4.3 The Holocene `extraData` requirement

After Holocene activates, EIP-1559 base-fee parameters are encoded in
the previous block's `extraData` (9 bytes:
`[version=0x00][denominator: u32 BE][elasticity: u32 BE]`). When block 1
is validated against genesis, the consensus path calls
`decode_holocene_extra_data(genesis.extra_data)` and bails if the length
isn't exactly 9 bytes.

Empty `extraData` ⇒ `InvalidExtraDataLength` ⇒ `BaseFeeMissing` warning
on every block. The local miner retries until something sticks, so the
chain progresses, but the log is noise.

The decoder has a documented fallback: if the encoded denominator and
elasticity are both zero, it falls back to the chainspec's
`base_fee_params_at_timestamp`. So `0x000000000000000000` (9 zero bytes)
is a valid value that says "use chainspec params at block 0".

This is how `chainspec/dev.base.json` ships. (`OP_DEV` itself has empty
`extraData` and produces the same warnings — they're recoverable in
practice. We do better than `--dev` here.)

### 4.4 The injection step

`chainspec/dev.base.json` does **not** contain the EntityRegistry
predeploy. The predeploy is added via `arkiv-cli inject-predeploy` at
recipe time, producing a complete chainspec on the fly:

```bash
cp chainspec/dev.base.json $TMPDIR/genesis.json
arkiv-cli inject-predeploy $TMPDIR/genesis.json
op-reth init --chain $TMPDIR/genesis.json --datadir $TMPDIR
op-reth node --chain $TMPDIR/genesis.json --datadir $TMPDIR …
```

Why not bake the predeploy bytecode directly into `dev.base.json`?

- **Drift.** Bytecode regenerates whenever the bindings rev bumps. A
  build-time injection means every build is consistent with the bindings
  it actually depends on; checking in pre-baked bytecode would require
  a CI drift guard.
- **Reusability.** The same `inject-predeploy` tool composes with
  op-deployer output for production deployments — it's not just a dev
  helper. A pre-baked dev file wouldn't generalise.

### 4.5 The production flow

```
op-deployer apply --intent intent.toml --workdir ./ops
   └─► ops/genesis.json   (standard OP chainspec, all OP system contracts in alloc)
arkiv-cli inject-predeploy ops/genesis.json
   └─► ops/genesis.json   (now also has EntityRegistry at 0x42…0042)
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`inject-predeploy` is intentionally generic: it doesn't care whether the
input is a hand-crafted dev base or an op-deployer-produced production
genesis. Its only job is to read `chainId`, mint matching bytecode, and
splice it in.

The longer-term option — contributing EntityRegistry to op-deployer's
`L2Genesis.s.sol` so the standard tool produces it directly — is sketched
in this repo's previous design notes but isn't currently being pursued.
The post-process approach trades upstream contribution for a tiny
maintenance surface that we own.

---

## 5. ExEx mechanics

The ExEx is implemented in `crates/arkiv-node/src/exex.rs`. Top-level
shape:

```rust
pub async fn arkiv_exex<Node>(mut ctx: ExExContext<Node>, store: Arc<dyn Storage>) -> Result<()> {
    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let prior_rolling = prior_rolling_hash(&ctx, new)?;
                let blocks = extract_blocks(new, prior_rolling);
                store.handle_commit(&blocks)?;
                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => { /* revert + commit */ }
            ExExNotification::ChainReverted { old }      => { /* revert only */ }
        }
    }
    Ok(())
}
```

### 5.1 Per-block extraction

For each canonical block we forward — *including blocks with no
EntityRegistry transactions* — `extract_blocks` produces an `ArkivBlock`:

```
ArkivBlock {
    header:   { number, hash, parent_hash, changeset_hash },
    transactions: [ArkivTransaction { hash, index, sender, operations: [...] }, ...]
}
```

For each transaction whose `to` matches the EntityRegistry address, we
call `arkiv_bindings::decode::decode_registry_transaction(...)`. That
helper returns a `Vec<DecodedOperation>` correlated positionally with the
contract's emitted `EntityOperation` and `ChangeSetHashUpdate` events. We
map each `DecodedOperation` into the wire-format `ArkivOperation` enum
and stamp on the per-op rolling changeset hash directly from the
decoder's output.

### 5.2 Rolling changeset hash

`ArkivBlockHeader.changeset_hash` is the *rolling* hash as of the end of
the block — i.e., empty blocks inherit the previous mutated block's hash.
This intentionally diverges from the contract's `changeSetHashAtBlock(N)`
view, which returns `bytes32(0)` for empty blocks; the ExEx form is what
downstream consumers actually want.

The starting value for a notification (the rolling hash *before* the
first new block) is read directly from the contract's storage at the
parent block's state, using the slot helpers exposed by
`arkiv_bindings::storage_layout`:

```rust
let head     = decode_head_block(state.storage(reg, head_block_slot())?);
let node     = decode_block_node(state.storage(reg, block_node_slot(head))?);
let last_tx  = node.txCount - 1;
let op_count = decode_tx_op_count(state.storage(reg, tx_op_count_slot(head, last_tx))?);
let last_op  = op_count - 1;
let hash     = decode_hash_at(state.storage(reg, hash_at_slot(head, last_tx, last_op))?);
```

That's four `state.storage()` calls plus one `history_by_block_hash()` to
open the historical state — once per chain notification, regardless of
how many blocks the notification spans. The cost is dwarfed by the
HTTP/serialisation work the JSON-RPC store does anyway. Within the
notification, blocks carry the rolling value forward in memory.

Direct slot reads are used (rather than `eth_call` to
`registry.changeSetHash()`) because we control both the contracts repo
and this one — the storage layout coupling is acceptable, and a Foundry
artifact-based drift guard lives in `arkiv-bindings::storage_layout`'s
test suite.

### 5.3 Reorgs and reverts

`ChainReorged { old, new }` and `ChainReverted { old }` map onto
`Storage::handle_reorg` / `handle_revert`. Reverted blocks are sent as
minimal `ArkivBlockRef { number, hash }` references — the EntityDB
already has the full data; it just needs to know which heights to roll
back. The newest-first ordering is preserved (block N revert before
block N-1).

For reorgs, the new chain's `prior_rolling_hash` is computed at its
parent — which is also the parent of the reverted blocks — so the
EntityDB sees a coherent revert+commit pair.

### 5.4 Wire format

The full JSON-RPC payload shape is documented in
[`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md). The
short version:

| Method | Params |
|---|---|
| `arkiv_commitChain` | `{ "blocks": [ArkivBlock] }` |
| `arkiv_revert` | `{ "blocks": [ArkivBlockRef] }` |
| `arkiv_reorg` | `{ "revertedBlocks": [ArkivBlockRef], "newBlocks": [ArkivBlock] }` |

The server returns `{ "stateRoot": "0x..." }` on success.

---

## 6. Testing surface

| Layer | How to test |
|---|---|
| `arkiv-genesis` | Pure functions; reuse the bindings' Foundry-backed drift tests |
| `arkiv-cli inject-predeploy` | Smoke: feed a known JSON, diff the alloc |
| ExEx decoding | Run `just node-dev-jsonrpc` + `just batch <fixture>`, observe mock-entitydb output |
| ExEx rolling hash | Submit two ops on the same entity in one tx; observe distinct `changesetHash` per op |
| Activation guard | Start with `--chain optimism` → ExEx should stay inactive |

The `scripts/fixtures/` directory is a growing collection of batch JSON
files that exercise specific cases:

- `double-op-same-entity.json` — CREATE then UPDATE of the same entity
  in one tx; tests per-op changeset hash correctness.
- `attributes-all-types.json` — tests decoding for all three attribute
  value types (UINT/STRING/ENTITY_KEY), including auto-sorting of names
  before submission.

`scripts/mock-entitydb.js` is a tiny Node script that accepts any of the
three RPC methods and pretty-prints the payload. Enough to inspect the
wire format end-to-end without involving the real Go EntityDB.

---

## 7. Key design decisions, recapped

| Decision | Why |
|---|---|
| Predeploy at `0x42…0042` | Matches OP convention for system contracts; the address is a property of the chain, not the binary |
| ExEx auto-activates via bytecode hash check | Makes the binary a true drop-in op-reth; behavior is determined by chainspec content, not by which binary was launched |
| Direct storage-slot reads for rolling hash | Cheaper than `eth_call` (no EVM spin-up); coupling acceptable since we control both sides |
| Path-A chainspec (full hardfork data in JSON) | Required for `op-reth init` and `op-reth node` to agree on genesis hash when reading the same JSON |
| `inject-predeploy` as a separate post-process | Composes with op-deployer output rather than forking it; same tool serves dev and prod |
| `arkiv-genesis` as its own crate | Both binaries need the same predeploy-bytecode generator; lifting it out avoids cross-bin deps |
| `Storage` as a trait, not a concrete type | Local logging vs JSON-RPC forwarding are equally valid; future backends (e.g. a local sqlite indexer) drop in |
| No runtime chainspec mutation | Removed the `init`/`node` divergence bug structurally, not just patched the symptom |

---

## 8. Things this design does *not* do

Honest scope notes:

- **Built-in `--chain arkiv` name.** A custom `ChainSpecParser` could
  register `arkiv` as a name resolved to an embedded chainspec, removing
  the `inject-predeploy` step for dev. Not done; the file-based flow
  works, and it composes uniformly with prod.
- **Mainnet predeploy registration.** The cleanest long-term home for
  EntityRegistry is op-deployer's `L2Genesis.s.sol`. Not pursued yet.
- **L1 / op-node / op-batcher / op-proposer.** Out of scope. This repo
  is the L2 execution client only.
- **Pre-Bedrock state import.** Standard op-reth concern; the canonical
  Optimism docs cover it.
- **Custom RPC methods.** Arkiv-specific RPCs (e.g.
  `arkiv_changeSetHash`) could be added via `add_rpc_endpoints` on the
  node builder. Not implemented; consumers read from the contract or
  EntityDB directly.

---

## 9. Where to read next

- **Contract internals.** `arkiv-contracts/contracts/EntityRegistry.sol`
  and `Entity.sol`. The hashing scheme (EIP-712 typehashes for `CoreHash`
  and `EntityHash`, the per-op rolling extension) is the single most
  important thing to understand if you're touching the ExEx decoder.
- **Wire format.** [`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md).
- **Attribute encoding.**
  `arkiv-contracts/docs/value128-encoding.md` — explains the
  `bytes32[4]` packing for UINT/STRING/ENTITY_KEY values and which
  byte-level orderings are guaranteed.
- **Storage layout.** `arkiv-contracts/src/storage_layout.rs` — slot
  indices, key-packing functions, decoders. The Foundry-artifact drift
  guard there is the safety net for the ExEx's direct slot reads.
- **reth ExEx framework.** <https://reth.rs/exex.html>.
- **op-reth.** <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>.
